//! Matching jobs (D-011 to D-014, D-032, D-033, D-070): a client's deterministic matching
//! run as a durable row — claimed under a lease, fenced by its `lease_token`, its snapshot
//! of open lines and document open amounts pinned at start, committing the engine's
//! outcomes in chunks of [`CHUNK_LINES`] lines with the cursor advancing in the same
//! transaction. A crash resumes from the cursor and, because the snapshot is pinned and
//! decisions are unique per (job, subject), produces exactly the uninterrupted run.
//!
//! The engine itself is pure (`recon-match`); this module owns only its durable side. The
//! caller translates each engine `Outcome` into a [`DecidedLine`].

use std::time::Duration;

use recon_domain::line::{ActorKind, ExceptionKind};
use recon_domain::matching::{BalancedMatch, ScoreBreakdown, Tier};
use recon_domain::{ClientId, Currency, Date, DocumentId, FirmId, LineId, Money};
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::DbError;
use crate::decisions;

/// The lines of one chunk commit (D-033).
pub const CHUNK_LINES: usize = 500;
/// The tier, as stored on `matches.tier`.
pub const MANUAL_TIER: &str = "manual";

/// A claimed job's fencing handle: like a staging import's, the `token` changes on every
/// (re)claim, so a stale worker's next chunk update matches nothing (`LeaseLost`).
#[derive(Debug, Clone, Copy)]
pub struct JobLease {
    /// The firm.
    pub firm: FirmId,
    /// The job.
    pub id: Uuid,
    /// The client it reconciles.
    pub client: ClientId,
    token: Uuid,
}

/// What `start` answered.
#[derive(Debug, Clone, Copy)]
pub enum Start {
    /// A new job was created and claimed by the caller.
    Created(JobLease),
    /// A stale job (lease expired) was re-claimed; it resumes from its cursor.
    Resumed(JobLease),
    /// A live job already runs for the client.
    Busy {
        /// The running job.
        job: Uuid,
    },
}

/// The job's matching settings, pinned from the firm at claim (D-035).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobSettings {
    /// T1 may commit without a person (the firm may turn it off, D-011).
    pub auto_commit_t1: bool,
    /// Per-currency residual tolerances (D-014): `{ISO: {bank_fee, tax_withheld, rounding}}`.
    pub tolerances: serde_json::Value,
}

/// The job as `GET` answers it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobRecord {
    /// The job.
    pub id: Uuid,
    /// The client.
    pub client_id: Uuid,
    /// `running`, `complete` or `failed`.
    pub status: String,
    /// Lines decided so far (position in the snapshot).
    pub cursor: i64,
    /// The snapshot's size.
    pub line_count: Option<i64>,
    /// Auto-committed matches.
    pub matched: i64,
    /// Proposals awaiting a person.
    pub proposed: i64,
    /// Typed exceptions.
    pub exceptions: i64,
    /// Outcomes skipped because the line left `unreviewed` meanwhile.
    pub skipped: i64,
    /// The engine run's digest (hex) once complete.
    pub run_digest: Option<String>,
    /// The engine version that ran.
    pub engine_version: String,
    /// The failure code when `failed`.
    pub error_code: Option<String>,
    /// Timestamps.
    pub created_at: String,
    /// Finished, when final.
    pub finished_at: Option<String>,
}

/// One line's engine outcome, translated for storage.
#[derive(Debug)]
pub enum Decided {
    /// A balanced match (`commit` only for T1 with the firm's consent, D-011).
    Matched {
        /// The tier that found it.
        tier: Tier,
        /// The match (its currency is the match's).
        matched: BalancedMatch,
        /// The score's breakdown (D-012).
        score: ScoreBreakdown,
        /// Insert `committed` (T1 only) or `proposed`.
        commit: bool,
    },
    /// A typed exception with its candidates (at most 10).
    Exception {
        /// Why.
        kind: ExceptionKind,
        /// The candidates behind it, nearest first.
        candidates: Vec<DocumentId>,
    },
}

/// A line and its outcome at snapshot position `seq`.
#[derive(Debug)]
pub struct DecidedLine {
    /// The `match_job_lines.seq`.
    pub seq: i64,
    /// The line.
    pub line: LineId,
    /// What the engine decided.
    pub outcome: Decided,
}

/// A chunk's commit summary.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChunkReport {
    /// Committed matches.
    pub matched: u32,
    /// Proposals written.
    pub proposed: u32,
    /// Exceptions written.
    pub exceptions: u32,
    /// Outcomes skipped (the line left `unreviewed` before this chunk).
    pub skipped: u32,
}

/// One job line's stored input (the engine's `LineInput`, DB-shaped).
#[derive(Debug)]
pub struct JobLine {
    /// The snapshot position.
    pub seq: i64,
    /// The line.
    pub id: LineId,
    /// The booking date.
    pub booking_date: Date,
    /// The signed amount.
    pub amount: Money,
    /// Normalized narration.
    pub narration_norm: String,
    /// The bank's reference.
    pub reference: Option<String>,
    /// Normalized counterparty.
    pub counterparty_norm: Option<String>,
}

/// One candidate's stored input (the engine's `DocumentInput`, DB-shaped).
#[derive(Debug)]
pub struct JobDocument {
    /// The document.
    pub id: DocumentId,
    /// Its kind.
    pub kind: recon_domain::document::DocumentKind,
    /// Normalized number.
    pub number_norm: Option<String>,
    /// Issue date.
    pub issue_date: Date,
    /// Due date.
    pub due_date: Option<Date>,
    /// The open amount pinned at snapshot.
    pub open_amount: Money,
    /// Normalized counterparty.
    pub counterparty_norm: Option<String>,
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn domain_date(d: time::Date) -> Result<Date, DbError> {
    Date::new(
        u16::try_from(d.year()).map_err(|_| DbError::Inconsistent("year".into()))?,
        u8::from(d.month()),
        d.day(),
    )
    .map_err(|_| DbError::Inconsistent("invalid stored date".into()))
}

fn money(minor: i64, currency: &str) -> Result<Money, DbError> {
    let currency = Currency::from_code(currency)
        .map_err(|_| DbError::Inconsistent(format!("unknown currency {currency}")))?;
    Money::new(minor, currency).map_err(|e| DbError::Inconsistent(format!("money: {e}")))
}

fn evidence(value: &impl Serialize) -> Result<Vec<u8>, DbError> {
    serde_json::to_vec(value).map_err(|e| DbError::Inconsistent(format!("evidence: {e}")))
}

fn to_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

async fn begin_job_tx(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, DbError> {
    let tx = pool.begin().await?;
    Ok(tx)
}

/// The firm's matching settings (what a new job pins).
pub async fn settings(pool: &PgPool, firm: FirmId) -> Result<JobSettings, DbError> {
    let row =
        sqlx::query("SELECT auto_commit_t1, residual_tolerances FROM recon.firms WHERE id = $1")
            .bind(firm.as_uuid())
            .fetch_optional(pool)
            .await?
            .ok_or(DbError::NotFound)?;
    Ok(JobSettings {
        auto_commit_t1: row.try_get("auto_commit_t1")?,
        tolerances: row.try_get("residual_tolerances")?,
    })
}

/// Writes the firm's matching settings (what the next job pins; running jobs keep theirs).
pub async fn write_settings(
    pool: &PgPool,
    firm: FirmId,
    settings: &JobSettings,
) -> Result<(), DbError> {
    let written = sqlx::query(
        "UPDATE recon.firms SET auto_commit_t1 = $2, residual_tolerances = $3 WHERE id = $1",
    )
    .bind(firm.as_uuid())
    .bind(settings.auto_commit_t1)
    .bind(&settings.tolerances)
    .execute(pool)
    .await?
    .rows_affected();
    if written == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Starts a matching job for the client: inserts and claims a fresh one, re-claims a stale
/// one (`Resumed`) or reports the live one (`Busy`). The partial unique index
/// `match_jobs_one_running_per_client` makes a second `running` row impossible even for
/// raw SQL; the `lease_token` is the fencing token for everything after.
pub async fn start(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    lease: Duration,
    engine_version: &str,
) -> Result<Start, DbError> {
    let settings = settings(pool, firm).await?;
    let mut tx = begin_job_tx(pool).await?;
    let existing = sqlx::query(
        "SELECT id, lease_token, lease_until > pg_catalog.clock_timestamp() AS live
           FROM recon.match_jobs
          WHERE firm_id = $1 AND client_id = $2 AND status = 'running'
          FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    match existing {
        Some(row) if row.try_get::<bool, _>("live")? => {
            let job: Uuid = row.try_get("id")?;
            tx.rollback().await?;
            return Ok(Start::Busy { job });
        }
        Some(row) => {
            // Stale lease: re-claim under a fresh token and resume from the cursor.
            let id: Uuid = row.try_get("id")?;
            let old_token: Uuid = row.try_get("lease_token")?;
            let token = Uuid::now_v7();
            let claimed = sqlx::query(
                "UPDATE recon.match_jobs
                    SET lease_token = $4,
                        lease_until = pg_catalog.clock_timestamp() + ($5::bigint * interval '1 millisecond')
                  WHERE firm_id = $1 AND id = $2 AND lease_token = $3",
            )
            .bind(firm.as_uuid())
            .bind(id)
            .bind(old_token)
            .bind(token)
            .bind(millis(lease))
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            if claimed == 0 {
                return Err(DbError::LeaseLost);
            }
            return Ok(Start::Resumed(JobLease {
                firm,
                id,
                client,
                token,
            }));
        }
        None => {}
    }
    let id = Uuid::now_v7();
    let token = Uuid::now_v7();
    let inserted = sqlx::query(
        "INSERT INTO recon.match_jobs
             (firm_id, id, client_id, lease_token, lease_until, auto_commit_t1,
              residual_tolerances, engine_version)
         VALUES ($1, $2, $3, $4,
                 pg_catalog.clock_timestamp() + ($5::bigint * interval '1 millisecond'),
                 $6, $7, $8)
         ON CONFLICT (firm_id, client_id) WHERE status = 'running' DO NOTHING",
    );
    let inserted = inserted
        .bind(firm.as_uuid())
        .bind(id)
        .bind(client.as_uuid())
        .bind(token)
        .bind(millis(lease))
        .bind(settings.auto_commit_t1)
        .bind(&settings.tolerances)
        .bind(engine_version)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    if inserted == 0 {
        // Lost the race to another starter after the check above.
        return Err(DbError::ClientBusy);
    }
    Ok(Start::Created(JobLease {
        firm,
        id,
        client,
        token,
    }))
}

/// Re-claims every expired running job for the sweeper's resume pass. A claimed job
/// resumes from its cursor; a race with another claimer loses to its token CAS.
pub async fn claim_expired(pool: &PgPool, lease: Duration) -> Result<Vec<JobLease>, DbError> {
    let stale = sqlx::query(
        "SELECT firm_id, id, client_id, lease_token FROM recon.match_jobs
          WHERE status = 'running' AND lease_until <= pg_catalog.clock_timestamp()
          ORDER BY id FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(pool)
    .await?;
    let mut claimed = Vec::new();
    for row in stale {
        let firm: Uuid = row.try_get("firm_id")?;
        let id: Uuid = row.try_get("id")?;
        let client: Uuid = row.try_get("client_id")?;
        let old_token: Uuid = row.try_get("lease_token")?;
        let token = Uuid::now_v7();
        let ok = sqlx::query(
            "UPDATE recon.match_jobs
                SET lease_token = $4,
                    lease_until = pg_catalog.clock_timestamp() + ($5::bigint * interval '1 millisecond')
              WHERE firm_id = $1 AND id = $2 AND lease_token = $3 AND status = 'running'",
        )
        .bind(firm)
        .bind(id)
        .bind(old_token)
        .bind(token)
        .bind(millis(lease))
        .execute(pool)
        .await?
        .rows_affected();
        if ok == 1 {
            claimed.push(JobLease {
                firm: FirmId::from_uuid(firm),
                id,
                client: ClientId::from_uuid(client),
                token,
            });
        }
    }
    Ok(claimed)
}

/// The job's row for `GET`; `NotFound` for another firm's.
pub async fn get(pool: &PgPool, firm: FirmId, id: Uuid) -> Result<JobRecord, DbError> {
    let row = sqlx::query(
        "SELECT client_id, status, cursor, line_count, matched, proposed, exceptions, skipped,
                run_digest, engine_version, error_code,
                pg_catalog.to_char(created_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                pg_catalog.to_char(finished_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS finished_at
           FROM recon.match_jobs WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    Ok(JobRecord {
        id,
        client_id: row.try_get("client_id")?,
        status: row.try_get("status")?,
        cursor: row.try_get("cursor")?,
        line_count: row.try_get("line_count")?,
        matched: row.try_get("matched")?,
        proposed: row.try_get("proposed")?,
        exceptions: row.try_get("exceptions")?,
        skipped: row.try_get("skipped")?,
        run_digest: row
            .try_get::<Option<Vec<u8>>, _>("run_digest")?
            .map(|b| to_hex(&b)),
        engine_version: row.try_get("engine_version")?,
        error_code: row.try_get("error_code")?,
        created_at: row.try_get("created_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

/// Extends the claimed job's lease under the token CAS — the worker's heartbeat during
/// the snapshot-and-engine stretch, where no chunk commit renews it. `LeaseLost` when
/// another claim holds the job.
pub async fn renew(pool: &PgPool, lease: &JobLease, lease_for: Duration) -> Result<(), DbError> {
    let ok = sqlx::query(
        "UPDATE recon.match_jobs
            SET lease_until = pg_catalog.clock_timestamp() + ($4::bigint * interval '1 millisecond')
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3 AND status = 'running'",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.token)
    .bind(millis(lease_for))
    .execute(pool)
    .await?
    .rows_affected();
    if ok == 0 {
        return Err(DbError::LeaseLost);
    }
    Ok(())
}

/// The job's pinned settings and token check — `LeaseLost` for a stale worker.
pub async fn job_settings(pool: &PgPool, lease: &JobLease) -> Result<JobSettings, DbError> {
    let row = sqlx::query(
        "SELECT auto_commit_t1, residual_tolerances FROM recon.match_jobs
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3 AND status = 'running'",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.token)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::LeaseLost)?;
    Ok(JobSettings {
        auto_commit_t1: row.try_get("auto_commit_t1")?,
        tolerances: row.try_get("residual_tolerances")?,
    })
}

/// Pins the job's snapshot once: every `unreviewed` line of the client in id order and
/// every document's open amount (amount − allocated) — both in one transaction, so a
/// resumed run decides exactly the set its earlier chunks did. A second snapshot attempt
/// (`line_count` already set) is a no-op answered with the stored count; a stale worker's
/// is refused.
pub async fn snapshot(pool: &PgPool, lease: &JobLease) -> Result<i64, DbError> {
    let mut tx = begin_job_tx(pool).await?;
    let row = sqlx::query(
        "SELECT line_count FROM recon.match_jobs
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3 AND status = 'running'
          FOR UPDATE",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.token)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::LeaseLost)?;
    if let Some(count) = row.try_get::<Option<i64>, _>("line_count")? {
        tx.rollback().await?;
        return Ok(count);
    }
    sqlx::query(
        "INSERT INTO recon.match_job_lines (firm_id, job_id, seq, line_id)
         SELECT firm_id, $2, pg_catalog.row_number() OVER (ORDER BY id), id
           FROM recon.statement_lines
          WHERE firm_id = $1 AND client_id = $3 AND status = 'unreviewed'
          ORDER BY id",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.client.as_uuid())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO recon.match_job_documents (firm_id, job_id, document_id, open_amount_minor)
         SELECT firm_id, $2, id, amount_minor - allocated_minor
           FROM recon.documents
          WHERE firm_id = $1 AND client_id = $3 AND allocated_minor <> amount_minor",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.client.as_uuid())
    .execute(&mut *tx)
    .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.match_job_lines WHERE firm_id = $1 AND job_id = $2",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE recon.match_jobs SET line_count = $3
          WHERE firm_id = $1 AND id = $2 AND lease_token = $4 AND status = 'running'",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(count)
    .bind(lease.token)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(count)
}

/// The job's engine inputs: the pinned lines' (immutable) fields joined to their sequence
/// numbers, and the pinned open amounts joined to their (immutable) document fields.
pub async fn inputs(
    pool: &PgPool,
    lease: &JobLease,
) -> Result<(Vec<JobLine>, Vec<JobDocument>), DbError> {
    // The token is not checked here: the inputs are read-only; only commits fence.
    let lines = sqlx::query(
        "SELECT jl.seq, l.id, l.booking_date, l.amount_minor, l.currency, l.narration_norm,
                l.reference, l.counterparty_norm
           FROM recon.match_job_lines jl
           JOIN recon.statement_lines l ON l.firm_id = jl.firm_id AND l.id = jl.line_id
          WHERE jl.firm_id = $1 AND jl.job_id = $2 ORDER BY jl.seq",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .fetch_all(pool)
    .await?;
    let mut job_lines = Vec::with_capacity(lines.len());
    for row in &lines {
        job_lines.push(JobLine {
            seq: row.try_get("seq")?,
            id: LineId::from_uuid(row.try_get("id")?),
            booking_date: domain_date(row.try_get("booking_date")?)?,
            amount: money(row.try_get("amount_minor")?, row.try_get("currency")?)?,
            narration_norm: row.try_get("narration_norm")?,
            reference: row.try_get("reference")?,
            counterparty_norm: row.try_get("counterparty_norm")?,
        });
    }
    let docs = sqlx::query(
        "SELECT d.id, d.kind, d.number_norm, d.issue_date, d.due_date, d.currency,
                jd.open_amount_minor, d.counterparty_norm
           FROM recon.match_job_documents jd
           JOIN recon.documents d ON d.firm_id = jd.firm_id AND d.id = jd.document_id
          WHERE jd.firm_id = $1 AND jd.job_id = $2 ORDER BY d.id",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .fetch_all(pool)
    .await?;
    let mut job_docs = Vec::with_capacity(docs.len());
    for row in &docs {
        job_docs.push(JobDocument {
            id: DocumentId::from_uuid(row.try_get("id")?),
            kind: recon_domain::document::DocumentKind::from_code(row.try_get("kind")?)
                .ok_or_else(|| DbError::Inconsistent("unknown document kind".into()))?,
            number_norm: row.try_get("number_norm")?,
            issue_date: domain_date(row.try_get("issue_date")?)?,
            due_date: row
                .try_get::<Option<time::Date>, _>("due_date")?
                .map(domain_date)
                .transpose()?,
            open_amount: money(row.try_get("open_amount_minor")?, row.try_get("currency")?)?,
            counterparty_norm: row.try_get("counterparty_norm")?,
        });
    }
    Ok((job_lines, job_docs))
}

/// The line, match and exception evidence a decision carries (bounded: no narrations).
#[derive(Serialize)]
struct LineEvidence<'a> {
    line: Uuid,
    job: Uuid,
    engine: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidates: Option<&'a [Uuid]>,
}

#[derive(Serialize)]
struct MatchEvidence<'a> {
    matched: Uuid,
    job: Uuid,
    engine: &'a str,
    tier: &'a str,
    four_eyes: bool,
    score: u8,
    lines: &'a [AllocEvidence],
    documents: &'a [AllocEvidence],
    residuals: &'a [ResidualEvidence<'a>],
}

#[derive(Serialize)]
struct AllocEvidence {
    id: Uuid,
    amount: i64,
}

#[derive(Serialize)]
struct ResidualEvidence<'a> {
    code: &'a str,
    amount: i64,
}

struct PendingDecision {
    subject_kind: recon_domain::decision::SubjectKind,
    subject_id: Uuid,
    from_state: Option<&'static str>,
    to_state: &'static str,
    actor_kind: ActorKind,
    actor_ref: String,
    reason_code: String,
    evidence: Vec<u8>,
}

/// A match to insert: its generated id and the line that proposed it.
struct MatchPlan<'a> {
    id: Uuid,
    line: LineId,
    tier: Tier,
    matched: &'a BalancedMatch,
    score: ScoreBreakdown,
    commit: bool,
}

fn tier_code(tier: Tier) -> &'static str {
    match tier {
        Tier::T1 => "t1",
        Tier::T2 => "t2",
        Tier::T3 => "t3",
        Tier::T4 => "t4",
    }
}

/// One chunk of outcomes, committed atomically: matches and their allocations (each
/// bounded by the documents' `CHECK` and the allocation trigger's explicit over-allocation
/// refusal even for raw SQL), the lines' status changes each with their decision in this
/// transaction (the deferred `RCL04`/`RCM02` checks), the job's cursor and counters, and a
/// lease renewal — all fenced by `lease_token`, so a stale worker commits nothing.
#[allow(
    clippy::too_many_lines,
    reason = "one atomic commit across all match tables"
)]
pub async fn commit_chunk(
    pool: &PgPool,
    lease: &JobLease,
    decided: &[DecidedLine],
    lease_for: Duration,
    statement_timeout: Duration,
) -> Result<ChunkReport, DbError> {
    if decided.is_empty() {
        return Ok(ChunkReport::default());
    }
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = '{}ms'",
        millis(statement_timeout)
    )))
    .execute(&mut *tx)
    .await?;
    // Fencing: the token CAS is also the lease renewal. A stale worker matches no row;
    // the pinned engine version comes back for the decisions and proposals to name.
    let engine: String = sqlx::query_scalar(
        "UPDATE recon.match_jobs
            SET lease_until = pg_catalog.clock_timestamp() + ($4::bigint * interval '1 millisecond')
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3 AND status = 'running'
         RETURNING engine_version",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.token)
    .bind(millis(lease_for))
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::LeaseLost)?;

    // Lock the chunk's lines; any no longer `unreviewed` (an accepted revision superseded
    // it) is skipped — it already has exactly one state.
    let line_ids: Vec<Uuid> = decided.iter().map(|d| d.line.as_uuid()).collect();
    let live_rows = sqlx::query(
        "SELECT id FROM recon.statement_lines
          WHERE firm_id = $1 AND id = ANY($2) AND status = 'unreviewed' FOR UPDATE",
    )
    .bind(lease.firm.as_uuid())
    .bind(&line_ids)
    .fetch_all(&mut *tx)
    .await?;
    let live: std::collections::HashSet<Uuid> = live_rows
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<_, sqlx::Error>>()?;

    // Classify: one match plan per live `Matched` outcome, one queue row per live
    // `Exception` outcome; the rest are skipped.
    let mut report = ChunkReport::default();
    let mut plans: Vec<MatchPlan<'_>> = Vec::new();
    let mut exceptions: Vec<&DecidedLine> = Vec::new();
    let mut line_updates: Vec<(Uuid, &'static str)> = Vec::new();
    for d in decided {
        if !live.contains(&d.line.as_uuid()) {
            report.skipped = report.skipped.saturating_add(1);
            continue;
        }
        match &d.outcome {
            Decided::Matched {
                tier,
                matched,
                score,
                commit,
            } => {
                if *commit {
                    report.matched = report.matched.saturating_add(1);
                } else {
                    report.proposed = report.proposed.saturating_add(1);
                }
                line_updates.push((
                    d.line.as_uuid(),
                    if *commit { "matched" } else { "proposed" },
                ));
                plans.push(MatchPlan {
                    id: Uuid::now_v7(),
                    line: d.line,
                    tier: *tier,
                    matched,
                    score: *score,
                    commit: *commit,
                });
            }
            Decided::Exception { .. } => {
                report.exceptions = report.exceptions.saturating_add(1);
                line_updates.push((d.line.as_uuid(), "exception"));
                exceptions.push(d);
            }
        }
    }

    // (a) The matches, while their lines are still `unreviewed` (the allocation guards
    // require it at insert).
    if !plans.is_empty() {
        sqlx::query(
            "INSERT INTO recon.matches
                 (firm_id, id, client_id, job_id, tier, status, currency, score,
                  score_version, score_exact_amount, score_exact_reference,
                  score_counterparty, score_date_proximity, score_uniqueness, four_eyes)
             SELECT $1, u.id, $2, $3, u.tier, u.status, u.currency, u.score,
                    u.sversion, u.sea, u.ser, u.scp, u.sdp, u.suq, u.fe
               FROM ROWS FROM (
                    pg_catalog.unnest($4::uuid[]), pg_catalog.unnest($5::text[]),
                    pg_catalog.unnest($6::text[]), pg_catalog.unnest($7::text[]),
                    pg_catalog.unnest($8::int2[]), pg_catalog.unnest($9::text[]),
                    pg_catalog.unnest($10::int2[]), pg_catalog.unnest($11::int2[]),
                    pg_catalog.unnest($12::int2[]), pg_catalog.unnest($13::int2[]),
                    pg_catalog.unnest($14::int2[]), pg_catalog.unnest($15::bool[]))
                    AS u(id, tier, status, currency, score, sversion, sea, ser, scp, sdp, suq, fe)",
        )
        .bind(lease.firm.as_uuid())
        .bind(lease.client.as_uuid())
        .bind(lease.id)
        .bind(plans.iter().map(|p| p.id).collect::<Vec<_>>())
        .bind(plans.iter().map(|p| tier_code(p.tier)).collect::<Vec<_>>())
        .bind(
            plans
                .iter()
                .map(|p| if p.commit { "committed" } else { "proposed" })
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| p.matched.currency().code())
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| i16::from(p.score.total()))
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|_| ScoreBreakdown::VERSION)
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| i16::from(p.score.exact_amount))
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| i16::from(p.score.exact_reference))
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| i16::from(p.score.counterparty))
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| i16::from(p.score.date_proximity))
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| i16::from(p.score.uniqueness))
                .collect::<Vec<_>>(),
        )
        .bind(
            plans
                .iter()
                .map(|p| p.matched.needs_four_eyes())
                .collect::<Vec<_>>(),
        )
        .execute(&mut *tx)
        .await?;

        // (b) The allocations. The per-row triggers check the line's state and the
        // document's sign and bounds, then move `allocated_minor` — even raw SQL cannot
        // over-allocate past the documents' CHECK.
        let mut ml: Vec<(Uuid, Uuid, i64)> = Vec::new();
        let mut md: Vec<(Uuid, Uuid, i64)> = Vec::new();
        let mut mr: Vec<(Uuid, &'static str, i64)> = Vec::new();
        for p in &plans {
            for a in p.matched.lines() {
                ml.push((p.id, a.line.as_uuid(), a.amount.minor()));
            }
            for a in p.matched.documents() {
                md.push((p.id, a.document.as_uuid(), a.amount.minor()));
            }
            for r in p.matched.residuals() {
                mr.push((p.id, r.code.code(), r.amount.minor()));
            }
        }
        if !ml.is_empty() {
            sqlx::query(
                "INSERT INTO recon.match_lines (firm_id, client_id, match_id, line_id, amount_minor)
                 SELECT $1, $2, u.m, u.l, u.a
                   FROM ROWS FROM (pg_catalog.unnest($3::uuid[]), pg_catalog.unnest($4::uuid[]),
                         pg_catalog.unnest($5::bigint[])) AS u(m, l, a)",
            )
            .bind(lease.firm.as_uuid())
            .bind(lease.client.as_uuid())
            .bind(ml.iter().map(|r| r.0).collect::<Vec<_>>())
            .bind(ml.iter().map(|r| r.1).collect::<Vec<_>>())
            .bind(ml.iter().map(|r| r.2).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }
        if !md.is_empty() {
            sqlx::query(
                "INSERT INTO recon.match_documents (firm_id, client_id, match_id, document_id, amount_minor)
                 SELECT $1, $2, u.m, u.d, u.a
                   FROM ROWS FROM (pg_catalog.unnest($3::uuid[]), pg_catalog.unnest($4::uuid[]),
                         pg_catalog.unnest($5::bigint[])) AS u(m, d, a)",
            )
            .bind(lease.firm.as_uuid())
            .bind(lease.client.as_uuid())
            .bind(md.iter().map(|r| r.0).collect::<Vec<_>>())
            .bind(md.iter().map(|r| r.1).collect::<Vec<_>>())
            .bind(md.iter().map(|r| r.2).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }
        if !mr.is_empty() {
            sqlx::query(
                "INSERT INTO recon.match_residuals (firm_id, client_id, match_id, code, amount_minor)
                 SELECT $1, $2, u.m, u.c, u.a
                   FROM ROWS FROM (pg_catalog.unnest($3::uuid[]), pg_catalog.unnest($4::text[]),
                         pg_catalog.unnest($5::bigint[])) AS u(m, c, a)",
            )
            .bind(lease.firm.as_uuid())
            .bind(lease.client.as_uuid())
            .bind(mr.iter().map(|r| r.0).collect::<Vec<_>>())
            .bind(mr.iter().map(|r| r.1).collect::<Vec<_>>())
            .bind(mr.iter().map(|r| r.2).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }

        // (c) A proposal row for every match a person still has to decide.
        let open: Vec<&MatchPlan<'_>> = plans.iter().filter(|p| !p.commit).collect();
        if !open.is_empty() {
            sqlx::query(
                "INSERT INTO recon.proposals
                     (firm_id, id, client_id, line_id, match_id, job_id, actor,
                      engine_version, candidates)
                 SELECT $1, u.id, $2, u.line, u.m, $3, 'engine', $4,
                        coalesce((SELECT pg_catalog.array_agg(v::uuid)
                           FROM pg_catalog.jsonb_array_elements_text(u.c) AS v), '{}'::uuid[])
                   FROM ROWS FROM (pg_catalog.unnest($5::uuid[]), pg_catalog.unnest($6::uuid[]),
                         pg_catalog.unnest($7::uuid[]), pg_catalog.unnest($8::jsonb[]))
                        AS u(id, line, m, c)",
            )
            .bind(lease.firm.as_uuid())
            .bind(lease.client.as_uuid())
            .bind(lease.id)
            .bind(&engine)
            .bind(open.iter().map(|_| Uuid::now_v7()).collect::<Vec<_>>())
            .bind(open.iter().map(|p| p.line.as_uuid()).collect::<Vec<_>>())
            .bind(open.iter().map(|p| p.id).collect::<Vec<_>>())
            .bind(
                open.iter()
                    .map(|p| {
                        serde_json::to_value(
                            p.matched
                                .documents()
                                .iter()
                                .map(|a| a.document.as_uuid())
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| DbError::Inconsistent(format!("candidates: {e}")))?,
            )
            .execute(&mut *tx)
            .await?;
        }
    }

    // (d) The lines' states; every one of them was locked `unreviewed` above.
    if !line_updates.is_empty() {
        let updated = sqlx::query(
            "UPDATE recon.statement_lines l
                SET status = u.to_state, row_version = l.row_version + 1
               FROM ROWS FROM (pg_catalog.unnest($2::uuid[]), pg_catalog.unnest($3::text[]))
                    AS u(id, to_state)
              WHERE l.firm_id = $1 AND l.id = u.id AND l.status = 'unreviewed'",
        )
        .bind(lease.firm.as_uuid())
        .bind(line_updates.iter().map(|u| u.0).collect::<Vec<_>>())
        .bind(line_updates.iter().map(|u| u.1).collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != line_updates.len() as u64 {
            return Err(DbError::Inconsistent(format!(
                "line updates: {updated} of {} applied",
                line_updates.len()
            )));
        }
    }

    // (e) The exception queue rows — now that their lines are `exception`.
    if !exceptions.is_empty() {
        let mut kinds = Vec::with_capacity(exceptions.len());
        let mut cands = Vec::with_capacity(exceptions.len());
        for d in &exceptions {
            let Decided::Exception { kind, candidates } = &d.outcome else {
                return Err(DbError::Inconsistent("exception outcome changed".into()));
            };
            kinds.push(kind.code());
            cands.push(
                serde_json::to_value(
                    candidates
                        .iter()
                        .map(|c| c.as_uuid())
                        .collect::<Vec<Uuid>>(),
                )
                .map_err(|e| DbError::Inconsistent(format!("candidates: {e}")))?,
            );
        }
        sqlx::query(
            "INSERT INTO recon.line_exceptions (firm_id, client_id, line_id, kind, candidates, job_id)
             SELECT $1, $2, u.line, u.kind,
                    coalesce((SELECT pg_catalog.array_agg(v::uuid)
                       FROM pg_catalog.jsonb_array_elements_text(u.c) AS v), '{}'::uuid[]), $3
               FROM ROWS FROM (pg_catalog.unnest($4::uuid[]), pg_catalog.unnest($5::text[]),
                     pg_catalog.unnest($6::jsonb[])) AS u(line, kind, c)",
        )
        .bind(lease.firm.as_uuid())
        .bind(lease.client.as_uuid())
        .bind(lease.id)
        .bind(exceptions.iter().map(|d| d.line.as_uuid()).collect::<Vec<_>>())
        .bind(kinds)
        .bind(cands)
        .execute(&mut *tx)
        .await?;
    }

    // (f) The decisions: one per match and one per line, all in this transaction (the
    // deferred RCL04/RCM02 checks fire at commit).
    let mut pending: Vec<PendingDecision> = Vec::with_capacity(decided.len().saturating_mul(2));
    for p in &plans {
        let line_allocs: Vec<AllocEvidence> = p
            .matched
            .lines()
            .iter()
            .map(|a| AllocEvidence {
                id: a.line.as_uuid(),
                amount: a.amount.minor(),
            })
            .collect();
        let doc_allocs: Vec<AllocEvidence> = p
            .matched
            .documents()
            .iter()
            .map(|a| AllocEvidence {
                id: a.document.as_uuid(),
                amount: a.amount.minor(),
            })
            .collect();
        let residuals: Vec<ResidualEvidence<'_>> = p
            .matched
            .residuals()
            .iter()
            .map(|r| ResidualEvidence {
                code: r.code.code(),
                amount: r.amount.minor(),
            })
            .collect();
        let (match_state, line_state, actor, reason) = if p.commit {
            ("committed", "matched", ActorKind::Rule, "auto_commit")
        } else {
            ("proposed", "proposed", ActorKind::Engine, "engine_proposal")
        };
        pending.push(PendingDecision {
            subject_kind: recon_domain::decision::SubjectKind::Match,
            subject_id: p.id,
            from_state: None,
            to_state: match_state,
            actor_kind: actor,
            actor_ref: engine.clone(),
            reason_code: tier_code(p.tier).to_owned(),
            evidence: evidence(&MatchEvidence {
                matched: p.id,
                job: lease.id,
                engine: engine.as_str(),
                tier: tier_code(p.tier),
                four_eyes: p.matched.needs_four_eyes(),
                score: p.score.total(),
                lines: &line_allocs,
                documents: &doc_allocs,
                residuals: &residuals,
            })?,
        });
        pending.push(PendingDecision {
            subject_kind: recon_domain::decision::SubjectKind::Line,
            subject_id: p.line.as_uuid(),
            from_state: Some("unreviewed"),
            to_state: line_state,
            actor_kind: actor,
            actor_ref: engine.clone(),
            reason_code: reason.to_owned(),
            evidence: evidence(&LineEvidence {
                line: p.line.as_uuid(),
                job: lease.id,
                engine: engine.as_str(),
                matched: Some(p.id),
                tier: Some(tier_code(p.tier)),
                score: Some(p.score.total()),
                kind: None,
                candidates: None,
            })?,
        });
    }
    for d in &exceptions {
        let Decided::Exception { kind, candidates } = &d.outcome else {
            return Err(DbError::Inconsistent("exception outcome changed".into()));
        };
        let cands: Vec<Uuid> = candidates.iter().map(|c| c.as_uuid()).collect();
        pending.push(PendingDecision {
            subject_kind: recon_domain::decision::SubjectKind::Line,
            subject_id: d.line.as_uuid(),
            from_state: Some("unreviewed"),
            to_state: "exception",
            actor_kind: ActorKind::Engine,
            actor_ref: engine.clone(),
            reason_code: kind.code().to_owned(),
            evidence: evidence(&LineEvidence {
                line: d.line.as_uuid(),
                job: lease.id,
                engine: engine.as_str(),
                matched: None,
                tier: None,
                score: None,
                kind: Some(kind.code()),
                candidates: Some(&cands),
            })?,
        });
    }
    insert_decisions(&mut tx, lease, &pending).await?;

    // (g) The cursor and counters advance in this same transaction (D-033).
    let last_seq = decided.last().map_or(0, |d| d.seq);
    let advanced = sqlx::query(
        "UPDATE recon.match_jobs
            SET cursor = $3, matched = matched + $4, proposed = proposed + $5,
                exceptions = exceptions + $6, skipped = skipped + $7,
                lease_until = pg_catalog.clock_timestamp() + ($8::bigint * interval '1 millisecond')
          WHERE firm_id = $1 AND id = $2 AND lease_token = $9 AND status = 'running'",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(last_seq)
    .bind(i64::from(report.matched))
    .bind(i64::from(report.proposed))
    .bind(i64::from(report.exceptions))
    .bind(i64::from(report.skipped))
    .bind(millis(lease_for))
    .bind(lease.token)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if advanced == 0 {
        return Err(DbError::LeaseLost);
    }
    tx.commit().await?;
    Ok(report)
}

/// All of a chunk's decisions in one statement; the chain trigger orders them.
async fn insert_decisions(
    tx: &mut PgConnection,
    lease: &JobLease,
    pending: &[PendingDecision],
) -> Result<(), DbError> {
    if pending.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO recon.decision_log
             (firm_id, client_id, id, subject_kind, subject_id, from_state, to_state,
              actor_kind, actor_ref, reason_code, evidence, engine_version, job_id)
         SELECT $1, $2, u.id, u.sk, u.sid, u.fs, u.ts, u.ak, u.ar, u.rc, u.ev, $3, $4
           FROM ROWS FROM (
                pg_catalog.unnest($5::uuid[]), pg_catalog.unnest($6::text[]),
                pg_catalog.unnest($7::uuid[]), pg_catalog.unnest($8::text[]),
                pg_catalog.unnest($9::text[]), pg_catalog.unnest($10::text[]),
                pg_catalog.unnest($11::text[]), pg_catalog.unnest($12::text[]),
                pg_catalog.unnest($13::bytea[]))
                AS u(id, sk, sid, fs, ts, ak, ar, rc, ev)",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.client.as_uuid())
    .bind(decisions::engine_version())
    .bind(lease.id)
    .bind(pending.iter().map(|_| Uuid::now_v7()).collect::<Vec<_>>())
    .bind(
        pending
            .iter()
            .map(|d| d.subject_kind.code())
            .collect::<Vec<_>>(),
    )
    .bind(pending.iter().map(|d| d.subject_id).collect::<Vec<_>>())
    .bind(pending.iter().map(|d| d.from_state).collect::<Vec<_>>())
    .bind(pending.iter().map(|d| d.to_state).collect::<Vec<_>>())
    .bind(
        pending
            .iter()
            .map(|d| d.actor_kind.code())
            .collect::<Vec<_>>(),
    )
    .bind(
        pending
            .iter()
            .map(|d| d.actor_ref.as_str())
            .collect::<Vec<_>>(),
    )
    .bind(
        pending
            .iter()
            .map(|d| d.reason_code.as_str())
            .collect::<Vec<_>>(),
    )
    .bind(
        pending
            .iter()
            .map(|d| d.evidence.as_slice())
            .collect::<Vec<_>>(),
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Completes the job: the whole snapshot decided and the engine run's digest recorded
/// (the schema's `complete` guard requires both). `LeaseLost` for a stale worker.
pub async fn finish(pool: &PgPool, lease: &JobLease, digest: [u8; 32]) -> Result<(), DbError> {
    let done = sqlx::query(
        "UPDATE recon.match_jobs SET status = 'complete', run_digest = $4
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3
            AND status = 'running' AND cursor = line_count",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.token)
    .bind(digest.as_slice())
    .execute(pool)
    .await?
    .rows_affected();
    if done == 0 {
        return Err(DbError::LeaseLost);
    }
    Ok(())
}

/// Fails the job with a code — a real error, not a lost lease (a stale worker exits
/// quietly; the job stays claimable).
pub async fn fail(pool: &PgPool, lease: &JobLease, code: &'static str) -> Result<(), DbError> {
    let done = sqlx::query(
        "UPDATE recon.match_jobs SET status = 'failed', error_code = $4
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3 AND status = 'running'",
    )
    .bind(lease.firm.as_uuid())
    .bind(lease.id)
    .bind(lease.token)
    .bind(code)
    .execute(pool)
    .await?
    .rows_affected();
    if done == 0 {
        return Err(DbError::LeaseLost);
    }
    Ok(())
}
