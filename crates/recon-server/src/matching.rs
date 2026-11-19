//! The matching worker (R2): a claimed job runs the pure engine over its pinned snapshot
//! and commits the outcomes in chunks, each a fenced transaction in `recon-db`. The
//! worker owns nothing — the lease token decides — so a stale worker simply stops writing
//! and the sweeper's next claim resumes the job where the cursor stands (D-033).

use std::collections::BTreeMap;
use std::time::Duration;

use recon_db::matching::{self, ChunkReport, Decided, DecidedLine, JobLease, Start};
use recon_domain::{ClientId, Currency, FirmId, LineId};
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

/// The engine version every new job pins (D-035).
pub const ENGINE_VERSION: &str = recon_match::MATCH_ENGINE_VERSION;

/// Starts a client's matching job and, when the claim is ours, drives it to completion
/// on a spawned worker. `Busy` is left to the caller to report.
pub async fn start(
    state: &AppState,
    firm: FirmId,
    client: ClientId,
) -> Result<Start, recon_db::DbError> {
    // A worker permit drives the job at once. With none free the job is still written —
    // as a durable row whose lease is already expired: it is the queue, claimed in
    // creation order by the sweeper as workers free (one running job per client still
    // bounds it). Only the drive is concurrency-limited, never the submission.
    if let Ok(permit) = state.match_permits.clone().try_acquire_owned() {
        let started = matching::start(
            &state.pool,
            firm,
            client,
            state.config.matching_lease,
            ENGINE_VERSION,
        )
        .await?;
        if let Start::Created(lease) | Start::Resumed(lease) = started {
            let state = state.clone();
            let shutdown = state.shutdown.clone();
            tokio::spawn(async move {
                let _permit = permit;
                run(state, lease, shutdown).await;
            });
        }
        return Ok(started);
    }
    matching::start(&state.pool, firm, client, Duration::ZERO, ENGINE_VERSION).await
}

/// Claims up to `available_permits` expired jobs (the sweeper's call each round) and
/// drives each under a worker permit — the queued rows leave the queue in creation
/// order. Jobs without a permit stay claimable next round.
pub async fn resume_expired(state: &AppState, shutdown: CancellationToken) {
    let claimed = matching::claim_expired(
        &state.pool,
        state.config.matching_lease,
        state.match_permits.available_permits(),
    )
    .await;
    match claimed {
        Ok(leases) => {
            for lease in leases {
                let Ok(permit) = state.match_permits.clone().try_acquire_owned() else {
                    tracing::warn!("no matching worker permit; the job stays claimable");
                    break;
                };
                let state = state.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    run(state, lease, shutdown).await;
                });
            }
        }
        Err(e) => tracing::warn!(error = %e, "claiming expired matching jobs failed"),
    }
}

/// The worker's loop: pin the snapshot, load it, run the engine once off the async
/// threads, then commit the outcomes 500 lines at a time — every chunk a fenced
/// transaction that renews the lease. A lost lease means stop quietly; a real failure
/// marks the job `failed` with a code.
async fn run(state: AppState, lease: JobLease, shutdown: CancellationToken) {
    // The lease renews on every chunk commit; the snapshot-and-engine stretch in
    // between can run far past one lease window, so a heartbeat keeps it alive.
    // Its cadence is a third of the lease: two missed renewals may pass before the
    // sweeper could claim the job. Only a lost lease stops the heart — transient
    // failures simply retry on the next beat — and losing it cancels the drive at
    // once, at `drive_cancel`, rather than letting it compute until its next fenced
    // write. The drive's own deadline (floor + per pinned line, capped) bounds the
    // whole stretch anyway: a hung engine ends `deadline_exceeded`, never a lease
    // held forever by a worker that cannot finish.
    let drive_cancel = shutdown.child_token();
    let heartbeat = CancellationToken::new();
    let beat = {
        let pool = state.pool.clone();
        let (stop, cancel) = (heartbeat.clone(), drive_cancel.clone());
        let lease_for = state.config.matching_lease;
        tokio::spawn(async move {
            let every = (lease_for / 3).max(Duration::from_millis(50));
            // The renew call is bounded well inside the beat.
            let renew_for = (every / 2).max(Duration::from_millis(10));
            loop {
                tokio::select! {
                    () = tokio::time::sleep(every) => {
                        match tokio::time::timeout(
                            renew_for,
                            matching::renew(&pool, &lease, lease_for),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(recon_db::DbError::LeaseLost)) => {
                                cancel.cancel();
                                return;
                            }
                            Ok(Err(e)) => {
                                tracing::debug!(error = %e, "lease renewal failed; retrying next beat");
                            }
                            Err(_) => {
                                tracing::debug!("lease renewal timed out; retrying next beat");
                            }
                        }
                    }
                    () = stop.cancelled() => return,
                }
            }
        })
    };
    let driven = drive(&state, &lease, &drive_cancel).await;
    heartbeat.cancel();
    if let Err(e) = beat.await {
        tracing::error!(error = %e, "the matching heartbeat task died");
    }
    match driven {
        Ok(()) => {}
        Err(Step::LeaseLost) => tracing::info!("matching job lost its lease; stopping"),
        Err(Step::Failed(code)) => {
            if let Err(e) = matching::fail(&state.pool, &lease, code).await {
                tracing::warn!(error = %e, "marking the matching job failed failed");
            }
        }
    }
}

/// Why `drive` ended.
enum Step {
    /// The lease token stopped matching: another worker owns the job.
    LeaseLost,
    /// A real failure; the job is marked with this code.
    Failed(&'static str),
}

impl Step {
    fn db(error: &recon_db::DbError) -> Self {
        match error {
            recon_db::DbError::LeaseLost => Self::LeaseLost,
            recon_db::DbError::Unavailable(_) | recon_db::DbError::Timeout(_) => {
                // The job keeps its lease; another claim or the sweeper resumes it.
                Self::LeaseLost
            }
            _ => Self::Failed("storage_error"),
        }
    }
}

async fn drive(
    state: &AppState,
    lease: &JobLease,
    drive_cancel: &CancellationToken,
) -> Result<(), Step> {
    let pool = &state.pool;
    let lease_for = state.config.matching_lease;
    let statement_timeout = state.config.pool.statement_timeout;
    let setup_started = std::time::Instant::now();
    let line_count = matching::snapshot(pool, lease)
        .await
        .map_err(|e| Step::db(&e))?;
    state
        .faults
        .reached(crate::faults::FaultPoint::MatchSnapshot)
        .await;
    let settings = matching::job_settings(pool, lease)
        .await
        .map_err(|e| Step::db(&e))?;
    let settings = engine_settings(&settings).map_err(|_| Step::Failed("invalid_settings"))?;
    let (lines, documents) = matching::inputs(pool, lease)
        .await
        .map_err(|e| Step::db(&e))?;
    if line_count != i64::try_from(lines.len()).map_err(|_| Step::Failed("storage_error"))? {
        return Err(Step::Failed("snapshot_inconsistent"));
    }
    let (seqs, engine_lines, engine_docs) = engine_inputs(lines, documents);

    // The engine-and-commit stretch runs under the job's deadline — floor plus
    // per pinned line, capped — computed off the snapshot count. The DB calls
    // before it carry the pool's own acquire and statement deadlines.
    let deadline = state.config.matching_deadline(line_count);
    tokio::select! {
        () = tokio::time::sleep(deadline) => {
            drive_cancel.cancel();
            Err(Step::Failed("deadline_exceeded"))
        }
        result = drive_engine_and_commit(
            state, lease, drive_cancel, lease_for, statement_timeout,
            setup_started.elapsed(), settings, seqs, engine_lines, engine_docs,
        ) => result,
    }
}

/// The deadline-bounded stretch: the engine (off the async threads on
/// `tokio::task::spawn_blocking`) then the 500-line fenced chunk commits.
/// `drive_cancel` is checked before the engine, before its results are used and
/// between chunks — a lost lease or a shutdown stops it at once.
#[expect(
    clippy::too_many_arguments,
    reason = "one deadline-bounded stretch; the parts share no natural owner"
)]
async fn drive_engine_and_commit(
    state: &AppState,
    lease: &JobLease,
    drive_cancel: &CancellationToken,
    lease_for: Duration,
    statement_timeout: Duration,
    setup_ms: Duration,
    settings: recon_match::Settings,
    seqs: BTreeMap<LineId, i64>,
    engine_lines: Vec<recon_match::LineInput>,
    engine_docs: Vec<recon_match::DocumentInput>,
) -> Result<(), Step> {
    if drive_cancel.is_cancelled() {
        return Err(Step::LeaseLost);
    }
    let pool = &state.pool;
    let faults = state.faults.clone();
    let engine_started = std::time::Instant::now();
    let run = tokio::task::spawn_blocking(move || {
        faults.reached_blocking(crate::faults::FaultPoint::MatchEngine);
        recon_match::run(&engine_lines, &engine_docs, &settings)
    })
    .await
    .map_err(|_| Step::Failed("engine_panicked"))?;
    let engine_ms = engine_started.elapsed();
    if drive_cancel.is_cancelled() {
        return Err(Step::LeaseLost);
    }

    // Resume starts after the cursor; each chunk commits the outcomes it holds.
    let cursor = matching::get(pool, lease.firm, lease.id)
        .await
        .map_err(|e| Step::db(&e))?
        .cursor;
    let mut decided: Vec<DecidedLine> = Vec::with_capacity(matching::CHUNK_LINES);
    let mut reports = ChunkReport::default();
    let mut chunks = 0_u32;
    // Phase timings on the completion log line (D-075): which phase takes a drive's
    // time, and whether the chunks carry a growing tail.
    let mut commit_ms = Duration::ZERO;
    let mut slowest_chunk_ms = Duration::ZERO;
    for (line_id, seq) in seqs.iter().filter(|(_, seq)| **seq > cursor) {
        let outcome = run
            .outcomes
            .get(line_id)
            .ok_or(Step::Failed("engine_gap"))?;
        decided.push(DecidedLine {
            seq: *seq,
            line: *line_id,
            outcome: translate(outcome),
        });
        if decided.len() == matching::CHUNK_LINES {
            let chunk_started = std::time::Instant::now();
            let report =
                matching::commit_chunk(pool, lease, &decided, lease_for, statement_timeout)
                    .await
                    .map_err(|e| Step::db(&e))?;
            let ms = chunk_started.elapsed();
            fold(&mut reports, report);
            commit_ms += ms;
            slowest_chunk_ms = slowest_chunk_ms.max(ms);
            decided.clear();
            chunks = chunks.saturating_add(1);
            state
                .faults
                .reached(crate::faults::FaultPoint::MatchChunk(chunks))
                .await;
            if drive_cancel.is_cancelled() {
                return Err(Step::LeaseLost);
            }
        }
    }
    if !decided.is_empty() {
        let chunk_started = std::time::Instant::now();
        let report = matching::commit_chunk(pool, lease, &decided, lease_for, statement_timeout)
            .await
            .map_err(|e| Step::db(&e))?;
        let ms = chunk_started.elapsed();
        fold(&mut reports, report);
        commit_ms += ms;
        slowest_chunk_ms = slowest_chunk_ms.max(ms);
        chunks = chunks.saturating_add(1);
        state
            .faults
            .reached(crate::faults::FaultPoint::MatchChunk(chunks))
            .await;
    }
    state
        .faults
        .reached(crate::faults::FaultPoint::MatchBeforeFinish)
        .await;
    let finish_started = std::time::Instant::now();
    matching::finish(pool, lease, run.digest)
        .await
        .map_err(|e| Step::db(&e))?;
    let finish_ms = finish_started.elapsed();
    state
        .faults
        .reached(crate::faults::FaultPoint::MatchFinished)
        .await;
    tracing::info!(
        matched = reports.matched,
        proposed = reports.proposed,
        exceptions = reports.exceptions,
        skipped = reports.skipped,
        setup_ms = u64::try_from(setup_ms.as_millis()).unwrap_or(u64::MAX),
        engine_ms = u64::try_from(engine_ms.as_millis()).unwrap_or(u64::MAX),
        commit_ms = u64::try_from(commit_ms.as_millis()).unwrap_or(u64::MAX),
        slowest_chunk_ms = u64::try_from(slowest_chunk_ms.as_millis()).unwrap_or(u64::MAX),
        finish_ms = u64::try_from(finish_ms.as_millis()).unwrap_or(u64::MAX),
        "matching job complete"
    );
    Ok(())
}

/// The stored snapshot rows as the engine's inputs plus each line's sequence for the
/// commit order (`match_job_lines.seq` follows the id order `BTreeMap` iterates).
fn engine_inputs(
    lines: Vec<matching::JobLine>,
    documents: Vec<matching::JobDocument>,
) -> (
    BTreeMap<LineId, i64>,
    Vec<recon_match::LineInput>,
    Vec<recon_match::DocumentInput>,
) {
    let seqs: BTreeMap<LineId, i64> = lines.iter().map(|l| (l.id, l.seq)).collect();
    let lines = lines
        .into_iter()
        .map(|l| recon_match::LineInput {
            id: l.id,
            booking_date: l.booking_date,
            amount: l.amount,
            narration_norm: l.narration_norm,
            reference: l.reference,
            counterparty_norm: l.counterparty_norm,
        })
        .collect();
    let docs = documents
        .into_iter()
        .map(|d| recon_match::DocumentInput {
            id: d.id,
            kind: d.kind,
            number_norm: d.number_norm,
            issue_date: d.issue_date,
            due_date: d.due_date,
            open_amount: d.open_amount,
            counterparty_norm: d.counterparty_norm,
        })
        .collect();
    (seqs, lines, docs)
}

/// The engine's `Outcome` as `recon-db`'s `Decided` (storage has no engine types).
fn translate(outcome: &recon_match::Outcome) -> Decided {
    match outcome {
        recon_match::Outcome::Matched {
            tier,
            matched,
            score,
            commit,
        } => Decided::Matched {
            tier: *tier,
            matched: matched.clone(),
            score: *score,
            commit: *commit,
        },
        recon_match::Outcome::Exception { kind, candidates } => Decided::Exception {
            kind: *kind,
            candidates: candidates.clone(),
        },
    }
}

fn fold(into: &mut ChunkReport, report: ChunkReport) {
    into.matched = into.matched.saturating_add(report.matched);
    into.proposed = into.proposed.saturating_add(report.proposed);
    into.exceptions = into.exceptions.saturating_add(report.exceptions);
    into.skipped = into.skipped.saturating_add(report.skipped);
}

/// The pinned `{ISO: {bank_fee, tax_withheld, rounding}}` tolerances as the engine reads
/// them (D-014). A malformed or unbounded value fails the job, never silently widens.
fn engine_settings(pinned: &matching::JobSettings) -> Result<recon_match::Settings, &'static str> {
    let mut tolerances = BTreeMap::new();
    if let serde_json::Value::Object(per_currency) = &pinned.tolerances {
        for (code, value) in per_currency {
            let currency = Currency::from_code(code).map_err(|_| "currency")?;
            let tolerance = |field: &str| -> Result<u64, &'static str> {
                value
                    .get(field)
                    .and_then(serde_json::Value::as_u64)
                    .filter(|v| i64::try_from(*v).is_ok())
                    .ok_or("tolerance")
            };
            tolerances.insert(
                currency,
                recon_match::Tolerances {
                    bank_fee: tolerance("bank_fee")?,
                    tax_withheld: tolerance("tax_withheld")?,
                    rounding: tolerance("rounding")?,
                },
            );
        }
    } else {
        return Err("tolerances");
    }
    Ok(recon_match::Settings {
        auto_commit_t1: pinned.auto_commit_t1,
        tolerances,
        ..recon_match::Settings::default()
    })
}
