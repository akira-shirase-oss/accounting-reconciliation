//! `recon explain line` — everything the database holds about one line, so every one of
//! its decisions can be explained (the ship gate): the line's facts and state, the
//! decisions that put it there in chain order, the matches covering it, and its
//! exception. Person actors resolve to their email; evidence comes back raw (canonical
//! JSON) exactly as the chain hashed it.

use std::collections::BTreeMap;

use recon_domain::{AccountId, ClientId, FirmId, Money};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::DbError;

/// One line and everything that explains how it got its state; [`None`] when the id is
/// not a line.
#[derive(Debug)]
pub struct LineExplanation {
    /// The line.
    pub id: Uuid,
    /// The firm.
    pub firm: FirmId,
    /// The client.
    pub client: ClientId,
    /// The client's name.
    pub client_name: String,
    /// The account.
    pub account: AccountId,
    /// The account's name.
    pub account_name: String,
    /// The import the line came in on.
    pub import: Uuid,
    /// Its row in the file.
    pub row_no: i32,
    /// `YYYY-MM-DD`.
    pub booking_date: String,
    /// The value date, when the file gave one.
    pub value_date: Option<String>,
    /// The amount.
    pub amount: Money,
    /// The narration as the file gave it.
    pub narration_raw: String,
    /// The reference, when the file gave one.
    pub reference: Option<String>,
    /// The counterparty as the file gave it.
    pub counterparty_raw: Option<String>,
    /// `unreviewed`, `proposed`, `matched`, `excluded`, `exception` or `superseded`.
    pub status: String,
    /// The row version review decisions are checked against.
    pub row_version: i64,
    /// Matches covering the line, newest first.
    pub matches: Vec<ExplainedMatch>,
    /// The exception queue row, when the line is one.
    pub exception: Option<ExplainedException>,
    /// Every decision about the line, its matches or its import, in chain order.
    pub decisions: Vec<ExplainedDecision>,
    /// User id → email for the actors the trail names (`person`, and an
    /// acknowledgement's signer).
    pub people: BTreeMap<Uuid, String>,
}

/// A match covering the explained line.
#[derive(Debug)]
pub struct ExplainedMatch {
    /// The match.
    pub id: Uuid,
    /// `t1`–`t4` or `manual`.
    pub tier: String,
    /// `proposed`, `committed`, `suspended`, `rejected` or `reversed`.
    pub status: String,
    /// The score, when the engine scored it.
    pub score: Option<i16>,
    /// The part of the line's amount this match allocates (minor units).
    pub allocated_minor: i64,
}

/// The line's exception queue row.
#[derive(Debug)]
pub struct ExplainedException {
    /// Why (`no_candidate`, …).
    pub kind: String,
    /// The candidates behind it.
    pub candidates: Vec<Uuid>,
    /// Who acknowledged it and when (both absent while it waits).
    pub acknowledged_by: Option<Uuid>,
    /// `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
    pub acknowledged_at: Option<String>,
}

/// A decision about the line's subjects, as the log holds it.
#[derive(Debug)]
pub struct ExplainedDecision {
    /// Its place in the client's chain.
    pub seq: i64,
    /// `import`, `line` or `match`.
    pub subject_kind: String,
    /// The subject.
    pub subject_id: Uuid,
    /// The state before (`None` at creation).
    pub from_state: Option<String>,
    /// The state after.
    pub to_state: String,
    /// `operator`, `person`, `rule`, `approval`, `engine`, `model` or `system`.
    pub actor_kind: String,
    /// Who exactly: a user id, `rule:{id}@v{n}`, an import, `operator`.
    pub actor_ref: String,
    /// Why, as a code.
    pub reason_code: String,
    /// Canonical JSON, exactly as hashed.
    pub evidence: Vec<u8>,
    /// The engine version in force (D-035).
    pub engine_version: String,
    /// `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
    pub decided_at: String,
}

const AT: &str = "YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"";

/// Explains `line` — its facts, every decision about it (and the matches covering it and
/// the import that brought it), its covering matches, and its exception. [`None`] when
/// the id is not a line.
pub async fn line(pool: &PgPool, line: Uuid) -> Result<Option<LineExplanation>, DbError> {
    let Some(row) = sqlx::query(
        "SELECT l.firm_id, l.client_id, c.name AS client_name,
                l.account_id, a.name AS account_name, l.import_id, l.row_no,
                to_char(l.booking_date, 'YYYY-MM-DD') AS booking_date,
                to_char(l.value_date, 'YYYY-MM-DD') AS value_date,
                l.amount_minor, l.currency, l.narration_raw, l.reference,
                l.counterparty_raw, l.status, l.row_version
           FROM recon.statement_lines l
           JOIN recon.clients c ON c.firm_id = l.firm_id AND c.id = l.client_id
           JOIN recon.accounts a ON a.firm_id = l.firm_id AND a.client_id = l.client_id
                                AND a.id = l.account_id
          WHERE l.id = $1",
    )
    .bind(line)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let firm = FirmId::from_uuid(row.try_get("firm_id")?);
    let client = ClientId::from_uuid(row.try_get("client_id")?);
    let import: Uuid = row.try_get("import_id")?;
    let currency = recon_domain::Currency::from_code(row.try_get("currency")?)
        .map_err(|e| DbError::Inconsistent(format!("line {line}: {e}")))?;
    let amount = Money::new(row.try_get("amount_minor")?, currency)
        .map_err(|e| DbError::Inconsistent(format!("line {line}: {e}")))?;

    let matches = covering_matches(pool, firm, line).await?;
    let match_ids: Vec<Uuid> = matches.iter().map(|m| m.id).collect();

    let exception = sqlx::query(
        "SELECT kind, candidates, acknowledged_by,
                to_char(acknowledged_at AT TIME ZONE 'UTC', $2) AS acknowledged_at
           FROM recon.line_exceptions
          WHERE firm_id = $1 AND line_id = $3",
    )
    .bind(firm.as_uuid())
    .bind(AT)
    .bind(line)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(exception_row)
    .transpose()?;

    let decisions = sqlx::query(
        "SELECT seq, subject_kind, subject_id, from_state, to_state, actor_kind,
                actor_ref, reason_code, evidence, engine_version,
                to_char(decided_at AT TIME ZONE 'UTC', $4) AS decided_at
           FROM recon.decision_log
          WHERE firm_id = $1 AND client_id = $2
            AND ((subject_kind = 'line' AND subject_id = $3)
              OR (subject_kind = 'match' AND subject_id = ANY($5))
              OR (subject_kind = 'import' AND subject_id = $6))
          ORDER BY seq",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(line)
    .bind(AT)
    .bind(&match_ids)
    .bind(import)
    .fetch_all(pool)
    .await?
    .iter()
    .map(decision_row)
    .collect::<Result<Vec<_>, sqlx::Error>>()?;

    let people = people_named(pool, firm, &decisions, exception.as_ref()).await?;

    Ok(Some(LineExplanation {
        id: line,
        firm,
        client,
        client_name: row.try_get("client_name")?,
        account: AccountId::from_uuid(row.try_get("account_id")?),
        account_name: row.try_get("account_name")?,
        import,
        row_no: row.try_get("row_no")?,
        booking_date: row.try_get("booking_date")?,
        value_date: row.try_get("value_date")?,
        amount,
        narration_raw: row.try_get("narration_raw")?,
        reference: row.try_get("reference")?,
        counterparty_raw: row.try_get("counterparty_raw")?,
        status: row.try_get("status")?,
        row_version: row.try_get("row_version")?,
        matches,
        exception,
        decisions,
        people,
    }))
}

/// The matches covering `line`, newest first.
async fn covering_matches(
    pool: &PgPool,
    firm: FirmId,
    line: Uuid,
) -> Result<Vec<ExplainedMatch>, DbError> {
    let matches = sqlx::query(
        "SELECT m.id, m.tier, m.status, m.score, ml.amount_minor AS allocated_minor
           FROM recon.match_lines ml
           JOIN recon.matches m ON m.firm_id = ml.firm_id AND m.client_id = ml.client_id
                               AND m.id = ml.match_id
          WHERE ml.firm_id = $1 AND ml.line_id = $2
          ORDER BY m.created_at DESC",
    )
    .bind(firm.as_uuid())
    .bind(line)
    .fetch_all(pool)
    .await?
    .iter()
    .map(match_row)
    .collect::<Result<Vec<_>, sqlx::Error>>()?;
    Ok(matches)
}

/// The user ids the trail names (`person`/`approval` actors) plus an acknowledgement's
/// signer, resolved to emails.
async fn people_named(
    pool: &PgPool,
    firm: FirmId,
    decisions: &[ExplainedDecision],
    exception: Option<&ExplainedException>,
) -> Result<BTreeMap<Uuid, String>, DbError> {
    let mut ids: Vec<Uuid> = decisions
        .iter()
        .filter(|d| matches!(d.actor_kind.as_str(), "person" | "approval"))
        .filter_map(|d| d.actor_ref.parse().ok())
        .collect();
    if let Some(by) = exception.and_then(|e| e.acknowledged_by) {
        ids.push(by);
    }
    ids.sort_unstable();
    ids.dedup();
    let people =
        sqlx::query("SELECT id, email FROM recon.users WHERE firm_id = $1 AND id = ANY($2)")
            .bind(firm.as_uuid())
            .bind(&ids)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|u| Ok::<_, sqlx::Error>((u.try_get("id")?, u.try_get("email")?)))
            .collect::<Result<_, sqlx::Error>>()?;
    Ok(people)
}

fn match_row(m: &sqlx::postgres::PgRow) -> Result<ExplainedMatch, sqlx::Error> {
    Ok(ExplainedMatch {
        id: m.try_get("id")?,
        tier: m.try_get("tier")?,
        status: m.try_get("status")?,
        score: m.try_get("score")?,
        allocated_minor: m.try_get("allocated_minor")?,
    })
}

fn exception_row(e: &sqlx::postgres::PgRow) -> Result<ExplainedException, sqlx::Error> {
    Ok(ExplainedException {
        kind: e.try_get("kind")?,
        candidates: e.try_get("candidates")?,
        acknowledged_by: e.try_get("acknowledged_by")?,
        acknowledged_at: e.try_get("acknowledged_at")?,
    })
}

fn decision_row(d: &sqlx::postgres::PgRow) -> Result<ExplainedDecision, sqlx::Error> {
    Ok(ExplainedDecision {
        seq: d.try_get("seq")?,
        subject_kind: d.try_get("subject_kind")?,
        subject_id: d.try_get("subject_id")?,
        from_state: d.try_get("from_state")?,
        to_state: d.try_get("to_state")?,
        actor_kind: d.try_get("actor_kind")?,
        actor_ref: d.try_get("actor_ref")?,
        reason_code: d.try_get("reason_code")?,
        evidence: d.try_get("evidence")?,
        engine_version: d.try_get("engine_version")?,
        decided_at: d.try_get("decided_at")?,
    })
}
