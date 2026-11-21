//! Periods (D-006, D-015, failure case 5): client × account × month, `open` →
//! `closed` → `reopened`. Close refuses while a line is undecided or an exception is
//! unacknowledged, records the period's decision digest — sha256 over the chain order
//! of the month's line decisions — and the client's chain head, and locks the month:
//! no insert, no status change, no re-evaluation until an owner reopens it with a
//! reason. Closing and reopening are decisions on the period.

use recon_domain::decision::SubjectKind;
use recon_domain::line::ActorKind;
use recon_domain::rule::{self, CompiledPredicate, RuleAction};
use recon_domain::{AccountId, ClientId, FirmId, UserId};
use serde::Serialize;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};
use time::Date;
use uuid::Uuid;

use crate::DbError;
use crate::decisions::{self, NewDecision};
use crate::rules::active_rules;

/// The most lines a re-evaluation reads (bounded everything).
const MAX_EVALUATED_LINES: i64 = 10_000;
/// The most proposals a re-evaluation returns.
const MAX_PROPOSALS: usize = 1_000;
/// The longest reopen reason.
const MAX_REASON_BYTES: usize = 500;

/// A period row.
#[derive(Debug)]
pub struct Period {
    /// The period's id (decisions about it name this).
    pub id: Uuid,
    /// `open`, `closed` or `reopened`.
    pub status: String,
    /// The month's first day.
    pub month: Date,
    /// The digest of the month's line decisions recorded at close (kept as the
    /// record of the last close when reopened).
    pub digest: Option<Vec<u8>>,
    /// The client's chain seq at close.
    pub chain_seq: Option<i64>,
    /// The client's chain head hash at close.
    pub chain_head: Option<Vec<u8>>,
    /// The optimistic row version.
    pub row_version: i64,
}

/// One proposal a re-evaluation yields — never committed (D-015).
#[derive(Debug)]
pub struct RuleProposal {
    /// The undecided line it would act on.
    pub line: Uuid,
    /// The rule and the immutable version behind it.
    pub rule: Uuid,
    /// The version.
    pub version: i32,
    /// What the rule would do.
    pub action: RuleAction,
}

/// A re-evaluation's diff.
#[derive(Debug)]
pub struct ReEvaluation {
    /// The period evaluated.
    pub period: Uuid,
    /// Its status (`open` or `reopened` — never `closed`).
    pub status: String,
    /// The undecided lines read.
    pub evaluated: i64,
    /// The proposals, capped at [`MAX_PROPOSALS`].
    pub proposals: Vec<RuleProposal>,
    /// More proposals exist than the cap carried.
    pub truncated: bool,
}

/// A refused period operation.
#[derive(Debug)]
pub enum PeriodError {
    /// The account or period does not exist in this tenant.
    NotFound,
    /// A state conflict (already closed, not closed, stale version).
    Conflict(&'static str),
    /// The period is closed.
    Closed,
    /// Lines in the month are still `unreviewed` or `proposed` (their count).
    UndecidedLines(i64),
    /// Live exceptions in the month lack a reviewer's acknowledgement (their count).
    Unacknowledged(i64),
    /// Reopening takes a non-empty reason.
    ReasonRequired,
    /// The account's flip holds its advisory lock — the close would race the
    /// lines it is publishing; refuse, let the caller retry (S1-1, D-076).
    AccountBusy,
    /// The re-evaluation would scan more work than the bound allows (S1-2).
    EvaluationTooLarge,
    /// Another re-evaluation of this firm is already running — the per-firm cap
    /// bounds the blocking CPU one caller can burn (N2).
    ReEvaluationBusy,
    /// Storage.
    Db(DbError),
}

impl From<sqlx::Error> for PeriodError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(DbError::from(e))
    }
}

impl From<DbError> for PeriodError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

fn evidence(v: &impl Serialize) -> Result<Vec<u8>, DbError> {
    serde_json::to_vec(v).map_err(|e| DbError::Inconsistent(format!("evidence: {e}")))
}

fn period(row: &PgRow) -> Result<Period, DbError> {
    Ok(Period {
        id: row.try_get("id")?,
        status: row.try_get("status")?,
        month: row.try_get("month")?,
        digest: row.try_get("digest")?,
        chain_seq: row.try_get("chain_seq")?,
        chain_head: row.try_get("chain_head")?,
        row_version: row.try_get("row_version")?,
    })
}

async fn account_exists(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM recon.accounts
         WHERE firm_id = $1 AND client_id = $2 AND id = $3)",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .fetch_one(&mut *conn)
    .await?)
}

/// The period row for (client, account, month), created `open` when absent — a month
/// is open by default; a row appears the first time somebody touches it.
async fn ensure(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
    month: Date,
) -> Result<Period, PeriodError> {
    sqlx::query(
        "INSERT INTO recon.periods (firm_id, client_id, account_id, month)
         VALUES ($1, $2, $3, $4) ON CONFLICT (firm_id, client_id, account_id, month) DO NOTHING",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(
        "SELECT id, status, digest, chain_seq, chain_head, row_version, month FROM recon.periods
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4
          FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .fetch_one(&mut *conn)
    .await?;
    period(&row).map_err(PeriodError::Db)
}

/// `ensure` without the row lock: the period is created when missing, but the read
/// takes no `FOR UPDATE` so a caller may hold the row across a long evaluation
async fn lock_chain_head(
    tx: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
) -> Result<(), PeriodError> {
    // The chain trigger locks chain_heads first on every decision insert, so
    // every path that will write decisions takes the same lock first — before
    // the period row — to keep one lock order and no cycle (D-077). The table
    // is not granted to the app role; the definer function carries the lock.
    sqlx::query("SELECT recon.chain_heads_lock($1, $2)")
        .bind(firm.as_uuid())
        .bind(client.as_uuid())
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// (N2 — `re_evaluate` locks only to write its decision).
async fn ensure_unlocked(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
    month: Date,
) -> Result<Period, PeriodError> {
    sqlx::query(
        "INSERT INTO recon.periods (firm_id, client_id, account_id, month)
         VALUES ($1, $2, $3, $4) ON CONFLICT (firm_id, client_id, account_id, month) DO NOTHING",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(
        "SELECT id, status, digest, chain_seq, chain_head, row_version, month FROM recon.periods
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .fetch_one(&mut *conn)
    .await?;
    period(&row).map_err(PeriodError::Db)
}

/// sha256 over the chain order of the month's line decisions — the digest stored at
/// close and recomputed to prove nothing moved (failure case 5).
async fn period_digest(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
    month: Date,
) -> Result<Vec<u8>, DbError> {
    Ok(sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT pg_catalog.sha256(coalesce(
             (SELECT pg_catalog.string_agg(d.record_hash, ''::bytea ORDER BY d.seq)
                FROM recon.decision_log d
               WHERE d.firm_id = $1 AND d.client_id = $2 AND d.subject_kind = 'line'
                 AND d.subject_id IN (
                     SELECT l.id FROM recon.statement_lines l
                      WHERE l.firm_id = $1 AND l.client_id = $2 AND l.account_id = $3
                        AND date_trunc('month', l.booking_date)::date = $4)),
             ''::bytea))",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .fetch_one(&mut *conn)
    .await?)
}

/// The digest as it stands now (reports and the byte-identical check).
pub async fn digest(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
    month: Date,
) -> Result<Vec<u8>, DbError> {
    let mut conn = pool.acquire().await?;
    period_digest(&mut conn, firm, client, account, month).await
}

/// Every period of the account, month order.
pub async fn list(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
) -> Result<Vec<Period>, DbError> {
    let rows = sqlx::query(
        "SELECT id, status, digest, chain_seq, chain_head, row_version, month
           FROM recon.periods
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 ORDER BY month",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .fetch_all(pool)
    .await?;
    rows.iter().map(period).collect()
}

#[expect(
    clippy::too_many_arguments,
    reason = "a decision row's own columns; bundling them hides the audit's shape"
)]
async fn decide(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    period: Uuid,
    from: &str,
    to: &str,
    user: UserId,
    reason: &str,
    evidence: &[u8],
) -> Result<(), DbError> {
    decisions::insert(
        conn,
        firm,
        &NewDecision {
            client,
            subject_kind: SubjectKind::Period,
            subject_id: period,
            from_state: Some(from),
            to_state: to,
            actor_kind: ActorKind::Person,
            actor_ref: &user.as_uuid().to_string(),
            reason_code: reason,
            evidence,
        },
    )
    .await?;
    Ok(())
}

/// Closes the period (D-006): refuses while a line in the month is `unreviewed` or
/// `proposed` or an exception lacks a reviewer's acknowledgement; otherwise records
/// the decision digest and the chain head, locks the month and writes the decision.
#[expect(
    clippy::too_many_lines,
    reason = "the lock's preconditions, digest, decision and state write are one commit"
)]
pub async fn close(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
    month: Date,
    user: UserId,
) -> Result<Period, PeriodError> {
    let mut tx = pool.begin().await?;
    if !account_exists(&mut tx, firm, client, account).await? {
        return Err(PeriodError::NotFound);
    }
    // Serialize with an in-flight flip before touching the period row — the same
    // order the flip takes (advisory lock, then the period), so neither can wait
    // on the other forever and neither commits past the other (S1-1, D-076).
    if !crate::imports::try_lock(&mut tx, account.as_uuid(), crate::imports::ACCOUNT_LOCK).await? {
        return Err(PeriodError::AccountBusy);
    }
    lock_chain_head(&mut tx, firm, client).await?;
    let p = ensure(&mut tx, firm, client, account, month).await?;
    let from = p.status.clone();
    if from == "closed" {
        return Err(PeriodError::Conflict("the period is already closed"));
    }
    let undecided: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.statement_lines l
          WHERE l.firm_id = $1 AND l.client_id = $2 AND l.account_id = $3
            AND date_trunc('month', l.booking_date)::date = $4
            AND l.status IN ('unreviewed', 'proposed')",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .fetch_one(&mut *tx)
    .await?;
    if undecided > 0 {
        return Err(PeriodError::UndecidedLines(undecided));
    }
    let unacknowledged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.line_exceptions e
           JOIN recon.statement_lines l
             ON l.firm_id = e.firm_id AND l.id = e.line_id
          WHERE e.firm_id = $1 AND l.client_id = $2 AND l.account_id = $3
            AND date_trunc('month', l.booking_date)::date = $4
            AND e.acknowledged_at IS NULL",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .fetch_one(&mut *tx)
    .await?;
    if unacknowledged > 0 {
        return Err(PeriodError::Unacknowledged(unacknowledged));
    }
    let digest = period_digest(&mut tx, firm, client, account, month).await?;
    let head = sqlx::query(
        "SELECT seq, head_hash FROM recon.chain_heads
          WHERE firm_id = $1 AND client_id = $2",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let (chain_seq, chain_head) = match &head {
        Some(row) => (
            row.try_get::<i64, _>("seq")?,
            row.try_get::<Vec<u8>, _>("head_hash")?,
        ),
        None => (
            0,
            recon_domain::decision::genesis_hash(firm, client).to_vec(),
        ),
    };
    sqlx::query(
        "UPDATE recon.periods
            SET status = 'closed', digest = $5, chain_seq = $6, chain_head = $7,
                closed_by = $8, closed_at = now(), row_version = row_version + 1
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .bind(&digest)
    .bind(chain_seq)
    .bind(&chain_head)
    .bind(user.as_uuid())
    .execute(&mut *tx)
    .await?;
    let body = evidence(&serde_json::json!({
        "digest": hex::encode(&digest), "chain_seq": chain_seq, "month": month.to_string(),
    }))?;
    decide(
        &mut tx,
        firm,
        client,
        p.id,
        &from,
        "closed",
        user,
        "period_closed",
        &body,
    )
    .await?;
    let row = sqlx::query(
        "SELECT id, status, digest, chain_seq, chain_head, row_version, month FROM recon.periods
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .fetch_one(&mut *tx)
    .await?;
    let closed = period(&row).map_err(PeriodError::Db)?;
    tx.commit().await?;
    Ok(closed)
}

/// Reopens a closed period: owner-only (the route checks the role), always with a
/// reason, and itself a decision. The stored digest stays as the record of the close.
pub async fn reopen(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
    month: Date,
    user: UserId,
    reason: &str,
) -> Result<Period, PeriodError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(PeriodError::ReasonRequired);
    }
    if reason.len() > MAX_REASON_BYTES {
        return Err(PeriodError::ReasonRequired);
    }
    let mut tx = pool.begin().await?;
    if !account_exists(&mut tx, firm, client, account).await? {
        return Err(PeriodError::NotFound);
    }
    lock_chain_head(&mut tx, firm, client).await?;
    let p = ensure(&mut tx, firm, client, account, month).await?;
    if p.status != "closed" {
        return Err(PeriodError::Conflict("the period is not closed"));
    }
    sqlx::query(
        "UPDATE recon.periods
            SET status = 'reopened', reopened_by = $5, reopened_at = now(),
                row_version = row_version + 1
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .bind(user.as_uuid())
    .execute(&mut *tx)
    .await?;
    let body = evidence(&serde_json::json!({
        "reason": reason, "month": month.to_string(),
    }))?;
    decide(
        &mut tx,
        firm,
        client,
        p.id,
        "closed",
        "reopened",
        user,
        "period_reopened",
        &body,
    )
    .await?;
    let row = sqlx::query(
        "SELECT id, status, digest, chain_seq, chain_head, row_version, month FROM recon.periods
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(account.as_uuid())
    .bind(month)
    .fetch_one(&mut *tx)
    .await?;
    let reopened = period(&row).map_err(PeriodError::Db)?;
    tx.commit().await?;
    Ok(reopened)
}

/// Re-evaluates the period's undecided lines (`unreviewed` or `exception`) against the
/// rules now in force — a diff of proposals, never a commit (D-015). A closed period
/// refuses; the act is itself a decision on the period.
pub async fn re_evaluate(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    account: AccountId,
    month: Date,
    user: UserId,
) -> Result<ReEvaluation, PeriodError> {
    let _permit = re_evaluation_permit(firm)?;
    // Phase 1 — short read transaction, no row lock: the month's rules and lines
    // are copied out so the evaluation runs against a snapshot.
    let (compiled, lines) = {
        let mut tx = pool.begin().await?;
        if !account_exists(&mut tx, firm, client, account).await? {
            return Err(PeriodError::NotFound);
        }
        let p = ensure_unlocked(&mut tx, firm, client, account, month).await?;
        if p.status == "closed" {
            return Err(PeriodError::Closed);
        }
        let rules = active_rules(&mut tx, firm, client, false).await?;
        let mut compiled = Vec::with_capacity(rules.len());
        for (rule_id, version, predicate, action) in &rules {
            compiled.push((
                *rule_id,
                *version,
                CompiledPredicate::compile(predicate, action)
                    .map_err(|e| DbError::Inconsistent(format!("stored rule: {e}")))?,
                action.clone(),
            ));
        }
        let lines = sqlx::query(
            "SELECT id, narration_norm, counterparty_norm, amount_minor, account_id
               FROM recon.statement_lines l
              WHERE l.firm_id = $1 AND l.client_id = $2 AND l.account_id = $3
                AND date_trunc('month', l.booking_date)::date = $4
                AND l.status IN ('unreviewed', 'exception')
              ORDER BY l.booking_date, l.row_no
              LIMIT $5",
        )
        .bind(firm.as_uuid())
        .bind(client.as_uuid())
        .bind(account.as_uuid())
        .bind(month)
        .bind(MAX_EVALUATED_LINES)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        (compiled, lines)
    };
    // Phase 2 — evaluate outside any transaction, deadline-bound; the row lock is
    // not held while the blocking worker runs (N2).
    let (compiled, proposals, truncated) =
        tokio::time::timeout(RE_EVALUATION_DEADLINE, run_evaluation(compiled, &lines))
            .await
            .map_err(|_| PeriodError::EvaluationTooLarge)??;
    let rule_versions: Vec<serde_json::Value> = compiled
        .iter()
        .map(|(id, v, _, _)| serde_json::json!({"rule": id, "version": v}))
        .collect();
    let body = evidence(&serde_json::json!({
        "month": month.to_string(),
        "rules": rule_versions,
        "evaluated": lines.len(),
        "proposals": proposals.len(),
        "truncated": truncated,
    }))?;
    // Phase 3 — lock only to write the decision; a close may have landed while the
    // evaluation ran, so the status is checked again under the lock.
    let mut tx = pool.begin().await?;
    lock_chain_head(&mut tx, firm, client).await?;
    let p = ensure(&mut tx, firm, client, account, month).await?;
    if p.status == "closed" {
        return Err(PeriodError::Closed);
    }
    decide(
        &mut tx,
        firm,
        client,
        p.id,
        &p.status,
        &p.status,
        user,
        "re_evaluated",
        &body,
    )
    .await?;
    tx.commit().await?;
    Ok(ReEvaluation {
        period: p.id,
        status: p.status,
        evaluated: i64::try_from(lines.len()).unwrap_or(i64::MAX),
        proposals,
        truncated,
    })
}

/// Most re-evaluations one firm may run at once — the work runs on a blocking
/// worker, so the cap bounds the CPU any one tenant can burn (N2).
const MAX_RE_EVALUATIONS_PER_FIRM: usize = 1;
/// The evaluation phase's wall-clock bound — a deadline alongside the searched-
/// bytes budget, since regex cost is not linear in bytes alone (N2).
const RE_EVALUATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
/// Per-firm re-evaluation semaphores (created on demand, one per firm).
static RE_EVALUATION_PERMITS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<Uuid, std::sync::Arc<tokio::sync::Semaphore>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn re_evaluation_permit(firm: FirmId) -> Result<tokio::sync::OwnedSemaphorePermit, PeriodError> {
    let semaphore = {
        let mut map = RE_EVALUATION_PERMITS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(firm.as_uuid())
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_RE_EVALUATIONS_PER_FIRM))
            })
            .clone()
    };
    semaphore
        .try_acquire_owned()
        .map_err(|_| PeriodError::ReEvaluationBusy)
}

/// The compiled form of one active rule for [`run_evaluation`].
type CompiledRule = (Uuid, i32, CompiledPredicate, RuleAction);

/// The lines × rules × bytes bound (S1-2): `PikeVM` cost is linear in the
/// searched narration bytes, so the bound counts the real work and the refusal
/// names it. 10k lines of 8 KiB narrations under a handful of rules would
/// otherwise wedge a Tokio worker while the row lock is held — the search runs
/// in a blocking worker, bounded.
async fn run_evaluation(
    compiled: Vec<CompiledRule>,
    lines: &[sqlx::postgres::PgRow],
) -> Result<(Vec<CompiledRule>, Vec<RuleProposal>, bool), PeriodError> {
    let mut owned: Vec<(Uuid, String, Option<String>, i64, Uuid)> = Vec::with_capacity(lines.len());
    let mut searched_bytes: u64 = 0;
    for line in lines {
        let narration: String = line.try_get("narration_norm")?;
        searched_bytes = searched_bytes.saturating_add(narration.len() as u64);
        owned.push((
            line.try_get("id")?,
            narration,
            line.try_get("counterparty_norm")?,
            line.try_get("amount_minor")?,
            line.try_get("account_id")?,
        ));
    }
    if rule::evaluation_too_big(searched_bytes, compiled.len()) {
        return Err(PeriodError::EvaluationTooLarge);
    }
    tokio::task::spawn_blocking(move || {
        let mut compiled = compiled;
        let mut proposals = Vec::new();
        let mut truncated = false;
        'lines: for (id, narration, counterparty, amount_minor, account) in &owned {
            let facts = rule::LineFacts {
                narration,
                counterparty: counterparty.as_deref(),
                amount_minor: *amount_minor,
                account: *account,
            };
            for (rule_id, version, predicate, action) in &mut compiled {
                if predicate.matches(&facts) {
                    if proposals.len() >= MAX_PROPOSALS {
                        truncated = true;
                        break 'lines;
                    }
                    proposals.push(RuleProposal {
                        line: *id,
                        rule: *rule_id,
                        version: *version,
                        action: action.clone(),
                    });
                    break; // first matching rule wins
                }
            }
        }
        Ok((compiled, proposals, truncated))
    })
    .await
    .map_err(|e| DbError::Inconsistent(format!("re-evaluation worker: {e}")))?
}
