//! Review writes (R3): approve, reject, exclude and the manual match — every state
//! change and its decision in one transaction (D-018's four-eyes and D-032's
//! optimistic row versions).

use recon_domain::decision::SubjectKind;
use recon_domain::line::{ActorKind, ExcludeReason};
use recon_domain::matching::{
    BalancedMatch, DocumentAllocation, LineAllocation, Residual, ResidualCode,
};
use recon_domain::money::{Currency, Money};
use recon_domain::{ClientId, DocumentId, FirmId, LineId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Row as _, Transaction};
use uuid::Uuid;

use crate::decisions::{self, NewDecision};
use crate::error::DbError;

/// The decision a conflicting review names as the winner — failure case 7's
/// requirement: a loser learns not only that the state moved but which recorded
/// decision moved it.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Winner {
    /// The winning decision's id and its chain position.
    pub decision: Uuid,
    pub seq: i64,
    /// Who recorded it, the state it moved the subject to and its reason code.
    pub actor_kind: String,
    pub actor_ref: String,
    pub to_state: String,
    pub reason_code: String,
}

/// What a review call found wrong.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The subject does not exist in this firm and client.
    #[error("not found")]
    NotFound,
    /// The state moved since the caller read it — the optimistic-conflict answer
    /// (409), naming the winning decision when the subject has one.
    #[error("{0}")]
    Conflict(&'static str, Option<Box<Winner>>),
    /// A semantic refusal the caller can fix (422).
    #[error("{0}")]
    Refused(&'static str),
    /// The subject's period is closed — an owner must reopen it first (D-015).
    #[error("the line's period is closed")]
    PeriodClosed,
    /// The database itself.
    #[error(transparent)]
    Db(#[from] DbError),
}

/// `true` when any of `lines` falls in a closed period (the schema's own lock is the
/// backstop; this names the refusal for the caller).
async fn any_period_closed(
    tx: &mut Transaction<'_, Postgres>,
    firm: FirmId,
    lines: &[Uuid],
) -> Result<bool, ReviewError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM recon.statement_lines l
               JOIN recon.periods p
                 ON p.firm_id = l.firm_id AND p.client_id = l.client_id
                AND p.account_id = l.account_id
                AND p.month = date_trunc('month', l.booking_date)::date
                AND p.status = 'closed'
              WHERE l.firm_id = $1 AND l.id = ANY($2))",
    )
    .bind(firm.as_uuid())
    .bind(lines)
    .fetch_one(&mut **tx)
    .await?)
}

/// The subject's latest recorded decision — the winner a conflicting call names.
/// `None` when the subject has no decision yet (a stale read before any write).
async fn winner_of(
    tx: &mut Transaction<'_, Postgres>,
    firm: FirmId,
    client: ClientId,
    kind: SubjectKind,
    subject: Uuid,
) -> Result<Option<Box<Winner>>, ReviewError> {
    Ok(sqlx::query_as::<_, Winner>(
        "SELECT id AS decision, seq, actor_kind, actor_ref, to_state, reason_code
           FROM recon.decision_log
          WHERE firm_id = $1 AND client_id = $2 AND subject_kind = $3 AND subject_id = $4
          ORDER BY seq DESC LIMIT 1",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(kind.code())
    .bind(subject)
    .fetch_optional(&mut **tx)
    .await?
    .map(Box::new))
}

/// The line ids a match covers.
async fn match_lines(
    tx: &mut Transaction<'_, Postgres>,
    firm: FirmId,
    match_id: Uuid,
) -> Result<Vec<Uuid>, ReviewError> {
    Ok(sqlx::query_scalar(
        "SELECT line_id FROM recon.match_lines WHERE firm_id = $1 AND match_id = $2",
    )
    .bind(firm.as_uuid())
    .bind(match_id)
    .fetch_all(&mut **tx)
    .await?)
}

impl From<sqlx::Error> for ReviewError {
    fn from(error: sqlx::Error) -> Self {
        DbError::from(error).into()
    }
}

/// What an approval did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approved {
    /// The match committed; every covered line is `matched`.
    Committed,
    /// The first eye recorded; a different person must finish it (D-018).
    AwaitingSecondEye,
}

/// A created manual match (always `proposed`, always four-eyes — D-018).
#[derive(Debug, Clone, Copy)]
pub struct ManualMatch {
    /// The new match's id.
    pub match_id: Uuid,
    /// Its line's new row version.
    pub line_version: i64,
}

/// What the review queue lists.
#[derive(Debug)]
pub struct QueueItem {
    /// `proposal` (engine/model) or `manual_match` or `exception`.
    pub kind: &'static str,
    /// The reviewable id: the match's for matches, the line's for exceptions.
    pub id: Uuid,
    /// The line it covers (for a match, its first line).
    pub line: Uuid,
    /// Match: `proposed`; exception: its kind code.
    pub detail: String,
    /// The subject's row version for optimistic review (the match's, or the
    /// exception's line's).
    pub row_version: i64,
    /// Whether a second, distinct approver is required.
    pub four_eyes: bool,
    /// The first approver, once taken.
    pub first_approver: Option<Uuid>,
}

/// A match read for review: its flag plus the firm's threshold decide the eyes.
#[derive(Debug, Clone)]
pub struct Reviewable {
    /// `proposed`, `committed`, `rejected`, `suspended`, `reversed`.
    pub status: String,
    /// The optimistic-concurrency version.
    pub row_version: i64,
    /// Residual/manual flag from the engine or creator.
    pub flagged_four_eyes: bool,
    /// The first approver recorded so far.
    pub first_approver: Option<Uuid>,
    /// `flagged_four_eyes || amount > firm's per-currency threshold`.
    pub needs_four_eyes: bool,
}

/// The match, its line count and covered amount, for the review endpoints' checks.
pub async fn match_for_review(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    match_id: Uuid,
) -> Result<Option<Reviewable>, DbError> {
    let row = sqlx::query(
        "SELECT m.status, m.row_version, m.four_eyes, m.first_approver,
                coalesce((SELECT sum(abs(ml.amount_minor))::bigint FROM recon.match_lines ml
                           WHERE ml.firm_id = m.firm_id AND ml.match_id = m.id), 0) AS amount,
                (SELECT (f.four_eyes_thresholds ->> m.currency)::bigint
                   FROM recon.firms f WHERE f.id = m.firm_id) AS threshold
           FROM recon.matches m
          WHERE m.firm_id = $1 AND m.client_id = $2 AND m.id = $3",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(match_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let flagged: bool = row.try_get("four_eyes")?;
    let amount: i64 = row.try_get("amount")?;
    let threshold: Option<i64> = row.try_get("threshold")?;
    Ok(Some(Reviewable {
        status: row.try_get("status")?,
        row_version: row.try_get("row_version")?,
        flagged_four_eyes: flagged,
        first_approver: row.try_get("first_approver")?,
        needs_four_eyes: flagged || threshold.is_some_and(|t| amount > t),
    }))
}

/// The queue: proposed matches (engine proposals and manual ones) then exception lines.
pub async fn queue(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    limit: i64,
) -> Result<Vec<QueueItem>, DbError> {
    let rows = sqlx::query(
        "SELECT kind, id, line, detail, row_version, four_eyes, first_approver FROM (
             SELECT CASE WHEN p.id IS NULL THEN 'manual_match' ELSE 'proposal' END AS kind,
                    m.id, (SELECT ml.line_id FROM recon.match_lines ml
                            WHERE ml.firm_id = m.firm_id AND ml.match_id = m.id
                            ORDER BY ml.line_id LIMIT 1) AS line,
                    m.tier AS detail, m.row_version, m.four_eyes, m.first_approver,
                    m.created_at
               FROM recon.matches m
               LEFT JOIN recon.proposals p
                 ON p.firm_id = m.firm_id AND p.match_id = m.id AND p.status = 'open'
              WHERE m.firm_id = $1 AND m.client_id = $2 AND m.status = 'proposed'
             UNION ALL
             SELECT 'exception', le.line_id, le.line_id, le.kind, sl.row_version,
                    false, NULL, le.created_at
               FROM recon.line_exceptions le
               JOIN recon.statement_lines sl
                 ON sl.firm_id = le.firm_id AND sl.id = le.line_id
              WHERE le.firm_id = $1 AND le.client_id = $2
         ) q ORDER BY created_at LIMIT $3",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(QueueItem {
                kind: match r.try_get::<&str, _>("kind")? {
                    "manual_match" => "manual_match",
                    "exception" => "exception",
                    _ => "proposal",
                },
                id: r.try_get("id")?,
                line: r.try_get("line")?,
                detail: r
                    .try_get::<Option<String>, _>("detail")?
                    .unwrap_or_default(),
                row_version: r.try_get("row_version")?,
                four_eyes: r.try_get("four_eyes")?,
                first_approver: r.try_get("first_approver")?,
            })
        })
        .collect()
}

#[derive(Serialize)]
struct ReviewEvidence<'a> {
    subject: Uuid,
    by: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

fn evidence(value: &impl Serialize) -> Result<Vec<u8>, DbError> {
    serde_json::to_vec(value).map_err(|e| DbError::Inconsistent(format!("evidence: {e}")))
}

#[expect(
    clippy::too_many_arguments,
    reason = "a decision row's own columns; bundling them hides the audit's shape"
)]
async fn decide(
    tx: &mut Transaction<'_, Postgres>,
    firm: FirmId,
    client: ClientId,
    kind: SubjectKind,
    subject: Uuid,
    from: Option<&'static str>,
    to: &'static str,
    user: Uuid,
    reason: &'static str,
    evidence_body: &[u8],
) -> Result<(), DbError> {
    let user_ref = user.to_string();
    decisions::insert(
        tx.as_mut(),
        firm,
        &NewDecision {
            client,
            subject_kind: kind,
            subject_id: subject,
            from_state: from,
            to_state: to,
            actor_kind: ActorKind::Person,
            actor_ref: &user_ref,
            reason_code: reason,
            evidence: evidence_body,
        },
    )
    .await?;
    Ok(())
}

/// One row of the match's own state, locked inside the transaction.
struct LockedMatch {
    status: String,
    row_version: i64,
    first_approver: Option<Uuid>,
}

async fn lock_match(
    tx: &mut Transaction<'_, Postgres>,
    firm: FirmId,
    client: ClientId,
    match_id: Uuid,
) -> Result<Option<LockedMatch>, DbError> {
    sqlx::query(
        "SELECT status, row_version, first_approver FROM recon.matches
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(match_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(|r| {
        Ok(LockedMatch {
            status: r.try_get("status")?,
            row_version: r.try_get("row_version")?,
            first_approver: r.try_get("first_approver")?,
        })
    })
    .transpose()
}

async fn still_proposed(
    tx: &mut Transaction<'_, Postgres>,
    firm: FirmId,
    client: ClientId,
    m: &LockedMatch,
    match_id: Uuid,
    expected_version: i64,
) -> Result<(), ReviewError> {
    if m.status != "proposed" {
        return Err(ReviewError::Conflict(
            "the match was already decided",
            winner_of(tx, firm, client, SubjectKind::Match, match_id).await?,
        ));
    }
    if m.row_version != expected_version {
        return Err(ReviewError::Conflict(
            "the match changed; reload it",
            winner_of(tx, firm, client, SubjectKind::Match, match_id).await?,
        ));
    }
    Ok(())
}

/// Approve `match_id` at `expected_version`. A four-eyes match's first approval records
/// `first_approver` and waits; the second — a different person — commits (D-018).
#[expect(
    clippy::too_many_lines,
    reason = "one transaction from lock to commit; splitting it splits the atomicity story"
)]
pub async fn approve(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    match_id: Uuid,
    user: Uuid,
    expected_version: i64,
    needs_four_eyes: bool,
) -> Result<Approved, ReviewError> {
    let mut tx = pool.begin().await?;
    let m = lock_match(&mut tx, firm, client, match_id)
        .await?
        .ok_or(ReviewError::NotFound)?;
    let covered = match_lines(&mut tx, firm, match_id).await?;
    if any_period_closed(&mut tx, firm, &covered).await? {
        return Err(ReviewError::PeriodClosed);
    }
    still_proposed(&mut tx, firm, client, &m, match_id, expected_version).await?;

    let evidence_body = evidence(&ReviewEvidence {
        subject: match_id,
        by: user,
        matched: Some(match_id),
        reason: None,
    })?;

    if needs_four_eyes {
        match m.first_approver {
            None => {
                sqlx::query(
                    "UPDATE recon.matches SET first_approver = $4
                      WHERE firm_id = $1 AND client_id = $2 AND id = $3
                        AND status = 'proposed' AND first_approver IS NULL",
                )
                .bind(firm.as_uuid())
                .bind(client.as_uuid())
                .bind(match_id)
                .bind(user)
                .execute(&mut *tx)
                .await?;
                decide(
                    &mut tx,
                    firm,
                    client,
                    SubjectKind::Match,
                    match_id,
                    Some("proposed"),
                    "proposed",
                    user,
                    "first_eyes",
                    &evidence_body,
                )
                .await?;
                tx.commit().await?;
                return Ok(Approved::AwaitingSecondEye);
            }
            Some(first) if first == user => {
                return Err(ReviewError::Refused("the second eye is a different person"));
            }
            Some(_) => {}
        }
    }

    // The commit: status CAS under the row lock, the covered lines, the queue row and
    // the decisions — one transaction or nothing.
    let moved = sqlx::query(
        "UPDATE recon.matches SET status = 'committed', row_version = row_version + 1
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 AND status = 'proposed'
            AND row_version = $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(match_id)
    .bind(expected_version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved != 1 {
        return Err(ReviewError::Conflict(
            "the match changed; reload it",
            winner_of(&mut tx, firm, client, SubjectKind::Match, match_id).await?,
        ));
    }
    let lines = sqlx::query(
        "UPDATE recon.statement_lines l SET status = 'matched', row_version = l.row_version + 1
          WHERE l.firm_id = $1 AND l.client_id = $2 AND l.status = 'proposed' AND l.id IN
                (SELECT ml.line_id FROM recon.match_lines ml
                  WHERE ml.firm_id = $1 AND ml.match_id = $3)
         RETURNING l.id",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(match_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE recon.proposals SET status = 'approved'
          WHERE firm_id = $1 AND match_id = $2 AND status = 'open'",
    )
    .bind(firm.as_uuid())
    .bind(match_id)
    .execute(&mut *tx)
    .await?;
    decide(
        &mut tx,
        firm,
        client,
        SubjectKind::Match,
        match_id,
        Some("proposed"),
        "committed",
        user,
        "approval",
        &evidence_body,
    )
    .await?;
    for line in &lines {
        let line_id: Uuid = line.try_get("id")?;
        let body = evidence(&ReviewEvidence {
            subject: line_id,
            by: user,
            matched: Some(match_id),
            reason: None,
        })?;
        decide(
            &mut tx,
            firm,
            client,
            SubjectKind::Line,
            line_id,
            Some("proposed"),
            "matched",
            user,
            "approval",
            &body,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Approved::Committed)
}

/// Reject `match_id` at `expected_version`: the match and its queue row end `rejected`,
/// the documents' allocations release, the lines return to `unreviewed`.
pub async fn reject(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    match_id: Uuid,
    user: Uuid,
    expected_version: i64,
) -> Result<(), ReviewError> {
    let mut tx = pool.begin().await?;
    let m = lock_match(&mut tx, firm, client, match_id)
        .await?
        .ok_or(ReviewError::NotFound)?;
    let covered = match_lines(&mut tx, firm, match_id).await?;
    if any_period_closed(&mut tx, firm, &covered).await? {
        return Err(ReviewError::PeriodClosed);
    }
    still_proposed(&mut tx, firm, client, &m, match_id, expected_version).await?;
    let moved = sqlx::query(
        "UPDATE recon.matches SET status = 'rejected', row_version = row_version + 1
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 AND status = 'proposed'
            AND row_version = $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(match_id)
    .bind(expected_version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved != 1 {
        return Err(ReviewError::Conflict(
            "the match changed; reload it",
            winner_of(&mut tx, firm, client, SubjectKind::Match, match_id).await?,
        ));
    }
    // The match is finished; deleting its document rows returns their allocated parts.
    sqlx::query("DELETE FROM recon.match_documents WHERE firm_id = $1 AND match_id = $2")
        .bind(firm.as_uuid())
        .bind(match_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE recon.proposals SET status = 'rejected'
          WHERE firm_id = $1 AND match_id = $2 AND status = 'open'",
    )
    .bind(firm.as_uuid())
    .bind(match_id)
    .execute(&mut *tx)
    .await?;
    let lines = sqlx::query(
        "UPDATE recon.statement_lines l SET status = 'unreviewed', row_version = l.row_version + 1
          WHERE l.firm_id = $1 AND l.client_id = $2 AND l.status = 'proposed' AND l.id IN
                (SELECT ml.line_id FROM recon.match_lines ml
                  WHERE ml.firm_id = $1 AND ml.match_id = $3)
         RETURNING l.id",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(match_id)
    .fetch_all(&mut *tx)
    .await?;
    let evidence_body = evidence(&ReviewEvidence {
        subject: match_id,
        by: user,
        matched: Some(match_id),
        reason: None,
    })?;
    decide(
        &mut tx,
        firm,
        client,
        SubjectKind::Match,
        match_id,
        Some("proposed"),
        "rejected",
        user,
        "rejection",
        &evidence_body,
    )
    .await?;
    for line in &lines {
        let line_id: Uuid = line.try_get("id")?;
        let body = evidence(&ReviewEvidence {
            subject: line_id,
            by: user,
            matched: Some(match_id),
            reason: Some("rejected"),
        })?;
        decide(
            &mut tx,
            firm,
            client,
            SubjectKind::Line,
            line_id,
            Some("proposed"),
            "unreviewed",
            user,
            "rejection",
            &body,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Exclude `line_id` at `expected_version` with a named reason. A proposed line is a
/// refusal: its proposal is decided first.
#[expect(
    clippy::too_many_arguments,
    reason = "the endpoint's own inputs; bundling ids and the reason hides the call shape"
)]
pub async fn exclude(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    line: LineId,
    user: Uuid,
    expected_version: i64,
    reason: ExcludeReason,
    note: Option<&str>,
) -> Result<i64, ReviewError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT status, row_version FROM recon.statement_lines
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(line.as_uuid())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ReviewError::NotFound)?;
    let status: String = row.try_get("status")?;
    let version: i64 = row.try_get("row_version")?;
    if any_period_closed(&mut tx, firm, &[line.as_uuid()]).await? {
        return Err(ReviewError::PeriodClosed);
    }
    if status == "proposed" {
        return Err(ReviewError::Refused(
            "the line has a proposal; decide the match first",
        ));
    }
    if !matches!(status.as_str(), "unreviewed" | "exception") {
        return Err(ReviewError::Conflict(
            "the line was already decided",
            winner_of(&mut tx, firm, client, SubjectKind::Line, line.as_uuid()).await?,
        ));
    }
    if version != expected_version {
        return Err(ReviewError::Conflict(
            "the line changed; reload it",
            winner_of(&mut tx, firm, client, SubjectKind::Line, line.as_uuid()).await?,
        ));
    }
    let from = if status == "exception" {
        "exception"
    } else {
        "unreviewed"
    };
    sqlx::query(
        "UPDATE recon.statement_lines SET status = 'excluded', row_version = row_version + 1
          WHERE firm_id = $1 AND id = $2 AND status IN ('unreviewed', 'exception')
            AND row_version = $3",
    )
    .bind(firm.as_uuid())
    .bind(line.as_uuid())
    .bind(expected_version)
    .execute(&mut *tx)
    .await?;
    let body = evidence(&ReviewEvidence {
        subject: line.as_uuid(),
        by: user,
        matched: None,
        reason: note.filter(|n| !n.is_empty()),
    })?;
    decide(
        &mut tx,
        firm,
        client,
        SubjectKind::Line,
        line.as_uuid(),
        Some(from),
        "excluded",
        user,
        reason.code(),
        &body,
    )
    .await?;
    let new_version = sqlx::query_scalar::<_, i64>(
        "SELECT row_version FROM recon.statement_lines WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(line.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(new_version)
}

/// A manual match: `documents` (allocated at their open remainder) plus named residuals
/// must balance the line exactly (`BalancedMatch`), else a named refusal. The match is
/// `proposed` and `four_eyes` — D-018 never lets a person commit their own manual match.
#[expect(
    clippy::too_many_lines,
    reason = "one transaction from lock to commit; splitting it splits the atomicity story"
)]
pub async fn manual_match(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    line: LineId,
    user: Uuid,
    documents: &[Uuid],
    residuals: &[(ResidualCode, i64)],
) -> Result<ManualMatch, ReviewError> {
    if documents.is_empty() {
        return Err(ReviewError::Refused("a manual match needs a document"));
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT status, amount_minor, currency FROM recon.statement_lines
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(line.as_uuid())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ReviewError::NotFound)?;
    let status: String = row.try_get("status")?;
    if any_period_closed(&mut tx, firm, &[line.as_uuid()]).await? {
        return Err(ReviewError::PeriodClosed);
    }
    if !matches!(status.as_str(), "unreviewed" | "exception") {
        return Err(ReviewError::Conflict(
            "only an unreviewed or exception line takes a manual match",
            winner_of(&mut tx, firm, client, SubjectKind::Line, line.as_uuid()).await?,
        ));
    }
    let currency = Currency::from_code(row.try_get::<&str, _>("currency")?)
        .map_err(|_| DbError::Inconsistent("line currency unknown".into()))?;
    let line_amount = Money::new(row.try_get("amount_minor")?, currency)
        .map_err(|_| DbError::Inconsistent("line amount out of range".into()))?;

    // The documents, locked; each contributes its open remainder in the line's currency.
    let doc_rows = sqlx::query(
        "SELECT id, amount_minor, allocated_minor, currency FROM recon.documents
          WHERE firm_id = $1 AND client_id = $2 AND id = ANY($3)
          ORDER BY id FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(documents)
    .fetch_all(&mut *tx)
    .await?;
    if doc_rows.len() != documents.len() {
        return Err(ReviewError::Refused("a document is unknown to this client"));
    }
    let mut doc_allocs: Vec<DocumentAllocation> = Vec::with_capacity(doc_rows.len());
    for d in &doc_rows {
        if d.try_get::<String, _>("currency")? != currency.code() {
            return Err(ReviewError::Refused("a document's currency differs"));
        }
        let open = d.try_get::<i64, _>("amount_minor")? - d.try_get::<i64, _>("allocated_minor")?;
        if open == 0 {
            // A live match holds the whole document — the loser of the
            // over-allocation race is told which decision holds it (case 7).
            let holder: Option<Uuid> = sqlx::query_scalar(
                "SELECT md.match_id FROM recon.match_documents md
                   JOIN recon.matches m
                     ON m.firm_id = md.firm_id AND m.id = md.match_id
                  WHERE md.firm_id = $1 AND md.client_id = $2 AND md.document_id = $3
                    AND m.status IN ('proposed', 'committed')
                  ORDER BY md.match_id LIMIT 1",
            )
            .bind(firm.as_uuid())
            .bind(client.as_uuid())
            .bind(d.try_get::<Uuid, _>("id")?)
            .fetch_optional(&mut *tx)
            .await?;
            return Err(match holder {
                Some(m) => ReviewError::Conflict(
                    "a document is fully allocated",
                    winner_of(&mut tx, firm, client, SubjectKind::Match, m).await?,
                ),
                None => ReviewError::Refused("a document is fully allocated"),
            });
        }
        doc_allocs.push(DocumentAllocation {
            document: DocumentId::from_uuid(d.try_get("id")?),
            amount: Money::new(open, currency)
                .map_err(|_| DbError::Inconsistent("document amount".into()))?,
        });
    }
    let mut residual_allocs: Vec<Residual> = Vec::with_capacity(residuals.len());
    for (code, amount) in residuals {
        residual_allocs.push(Residual {
            code: *code,
            amount: Money::new(*amount, currency)
                .map_err(|_| DbError::Inconsistent("residual amount".into()))?,
        });
    }
    let balanced = BalancedMatch::new(
        vec![LineAllocation {
            line,
            amount: line_amount,
        }],
        doc_allocs,
        residual_allocs,
    )
    .map_err(|r| ReviewError::Refused(r.code()))?;

    // An exception line passes `unreviewed` on the way to `proposed`: the allocation
    // guards admit only an unreviewed line, and each move writes its decision.
    if status == "exception" {
        sqlx::query(
            "UPDATE recon.statement_lines SET status = 'unreviewed', row_version = row_version + 1
              WHERE firm_id = $1 AND id = $2 AND status = 'exception'",
        )
        .bind(firm.as_uuid())
        .bind(line.as_uuid())
        .execute(&mut *tx)
        .await?;
        let body = evidence(&ReviewEvidence {
            subject: line.as_uuid(),
            by: user,
            matched: None,
            reason: Some("exception resolved by a manual match"),
        })?;
        decide(
            &mut tx,
            firm,
            client,
            SubjectKind::Line,
            line.as_uuid(),
            Some("exception"),
            "unreviewed",
            user,
            "manual_match",
            &body,
        )
        .await?;
    }

    let match_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO recon.matches (firm_id, id, client_id, tier, status, currency, four_eyes)
         VALUES ($1, $2, $3, 'manual', 'proposed', $4, true)",
    )
    .bind(firm.as_uuid())
    .bind(match_id)
    .bind(client.as_uuid())
    .bind(currency.code())
    .execute(&mut *tx)
    .await?;
    for a in balanced.lines() {
        sqlx::query(
            "INSERT INTO recon.match_lines (firm_id, client_id, match_id, line_id, amount_minor)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(firm.as_uuid())
        .bind(client.as_uuid())
        .bind(match_id)
        .bind(a.line.as_uuid())
        .bind(a.amount.minor())
        .execute(&mut *tx)
        .await?;
    }
    for a in balanced.documents() {
        sqlx::query(
            "INSERT INTO recon.match_documents (firm_id, client_id, match_id, document_id, amount_minor)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(firm.as_uuid())
        .bind(client.as_uuid())
        .bind(match_id)
        .bind(a.document.as_uuid())
        .bind(a.amount.minor())
        .execute(&mut *tx)
        .await?;
    }
    for r in balanced.residuals() {
        sqlx::query(
            "INSERT INTO recon.match_residuals (firm_id, client_id, match_id, code, amount_minor)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(firm.as_uuid())
        .bind(client.as_uuid())
        .bind(match_id)
        .bind(r.code.code())
        .bind(r.amount.minor())
        .execute(&mut *tx)
        .await?;
    }
    let line_version = sqlx::query_scalar::<_, i64>(
        "UPDATE recon.statement_lines SET status = 'proposed', row_version = row_version + 1
          WHERE firm_id = $1 AND id = $2 AND status = 'unreviewed'
         RETURNING row_version",
    )
    .bind(firm.as_uuid())
    .bind(line.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    let body = evidence(&ReviewEvidence {
        subject: match_id,
        by: user,
        matched: Some(match_id),
        reason: None,
    })?;
    decide(
        &mut tx,
        firm,
        client,
        SubjectKind::Match,
        match_id,
        None,
        "proposed",
        user,
        "manual_match",
        &body,
    )
    .await?;
    let body = evidence(&ReviewEvidence {
        subject: line.as_uuid(),
        by: user,
        matched: Some(match_id),
        reason: None,
    })?;
    decide(
        &mut tx,
        firm,
        client,
        SubjectKind::Line,
        line.as_uuid(),
        Some("unreviewed"),
        "proposed",
        user,
        "manual_match",
        &body,
    )
    .await?;
    tx.commit().await?;
    Ok(ManualMatch {
        match_id,
        line_version,
    })
}

/// A reviewer acknowledges `line`'s exception in place (D-006: a period closes only
/// once every exception carries a reviewer's acknowledgement). The line stays an
/// exception — its kind is the history; the acknowledgement is a decision on it.
pub async fn acknowledge(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    line: LineId,
    user: Uuid,
) -> Result<(), ReviewError> {
    let mut tx = pool.begin().await?;
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM recon.statement_lines
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(line.as_uuid())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ReviewError::NotFound)?;
    if any_period_closed(&mut tx, firm, &[line.as_uuid()]).await? {
        return Err(ReviewError::PeriodClosed);
    }
    if status != "exception" {
        return Err(ReviewError::Conflict(
            "only an exception is acknowledged",
            winner_of(&mut tx, firm, client, SubjectKind::Line, line.as_uuid()).await?,
        ));
    }
    let marked = sqlx::query(
        "UPDATE recon.line_exceptions
            SET acknowledged_by = $3, acknowledged_at = now()
          WHERE firm_id = $1 AND line_id = $2 AND acknowledged_at IS NULL",
    )
    .bind(firm.as_uuid())
    .bind(line.as_uuid())
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if marked == 0 {
        return Err(ReviewError::Conflict(
            "the exception was already acknowledged",
            winner_of(&mut tx, firm, client, SubjectKind::Line, line.as_uuid()).await?,
        ));
    }
    let kind: String = sqlx::query_scalar(
        "SELECT kind FROM recon.line_exceptions WHERE firm_id = $1 AND line_id = $2",
    )
    .bind(firm.as_uuid())
    .bind(line.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    let body = evidence(&serde_json::json!({
        "subject": line.as_uuid(),
        "by": user,
        "kind": kind,
    }))?;
    decide(
        &mut tx,
        firm,
        client,
        SubjectKind::Line,
        line.as_uuid(),
        Some("exception"),
        "exception",
        user,
        "exception_acknowledged",
        &body,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
