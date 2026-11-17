//! The decision log (D-019, D-042): inserting decisions (the trigger assigns `seq`,
//! `prev_hash`, `decided_at` and `record_hash` under the client's chain lock) and walking a
//! chain to verify it.

use recon_domain::decision::{
    BreakKind, ChainBreak, ChainHead, ChainVerifier, DecisionRecord, SubjectKind,
};
use recon_domain::import::{PROFILE_R0, PROFILE_R0_VERSION};
use recon_domain::line::ActorKind;
use recon_domain::{ClientId, DecisionId, FirmId};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::DbError;

/// The versions in force, recorded on every decision (D-035).
pub fn engine_version() -> String {
    format!(
        "recon {} / {PROFILE_R0} v{PROFILE_R0_VERSION} / fingerprint {}",
        env!("CARGO_PKG_VERSION"),
        recon_domain::fingerprint::FINGERPRINT_VERSION
    )
}

/// A decision to append.
#[derive(Debug, Clone, Copy)]
pub struct NewDecision<'a> {
    /// The client whose chain it joins.
    pub client: ClientId,
    /// What it is about.
    pub subject_kind: SubjectKind,
    /// The subject's id.
    pub subject_id: Uuid,
    /// The state before (none for a creation).
    pub from_state: Option<&'a str>,
    /// The state after.
    pub to_state: &'a str,
    /// Who.
    pub actor_kind: ActorKind,
    /// Who exactly.
    pub actor_ref: &'a str,
    /// Why.
    pub reason_code: &'a str,
    /// Evidence bytes (canonical JSON).
    pub evidence: &'a [u8],
}

/// Appends a decision inside the caller's transaction; returns its sequence number and
/// hash. The chain lock is held until that transaction ends.
pub async fn insert(
    conn: &mut PgConnection,
    firm: FirmId,
    decision: &NewDecision<'_>,
) -> Result<(i64, [u8; 32]), DbError> {
    let row = sqlx::query(
        "INSERT INTO recon.decision_log
             (firm_id, client_id, id, subject_kind, subject_id, from_state, to_state,
              actor_kind, actor_ref, reason_code, evidence, engine_version)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         RETURNING seq, record_hash",
    )
    .bind(firm.as_uuid())
    .bind(decision.client.as_uuid())
    .bind(Uuid::now_v7())
    .bind(decision.subject_kind.code())
    .bind(decision.subject_id)
    .bind(decision.from_state)
    .bind(decision.to_state)
    .bind(decision.actor_kind.code())
    .bind(decision.actor_ref)
    .bind(decision.reason_code)
    .bind(decision.evidence)
    .bind(engine_version())
    .fetch_one(&mut *conn)
    .await?;
    Ok((row.try_get("seq")?, hash32(&row, "record_hash")?))
}

fn hash32(row: &PgRow, column: &str) -> Result<[u8; 32], DbError> {
    let bytes: Vec<u8> = row.try_get(column)?;
    bytes
        .try_into()
        .map_err(|_| DbError::Inconsistent(format!("{column} is not 32 bytes")))
}

const COLUMNS: &str = "firm_id, client_id, seq, id, prev_hash, record_hash, subject_kind,
    subject_id, from_state, to_state, actor_kind, actor_ref, reason_code, evidence,
    engine_version, (EXTRACT(EPOCH FROM decided_at) * 1000000)::bigint AS decided_at_micros";

fn record(row: &PgRow) -> Result<DecisionRecord, DbError> {
    Ok(DecisionRecord {
        firm: FirmId::from_uuid(row.try_get("firm_id")?),
        client: ClientId::from_uuid(row.try_get("client_id")?),
        seq: row.try_get("seq")?,
        id: DecisionId::from_uuid(row.try_get("id")?),
        prev_hash: hash32(row, "prev_hash")?,
        record_hash: hash32(row, "record_hash")?,
        subject_kind: row.try_get("subject_kind")?,
        subject_id: row.try_get("subject_id")?,
        from_state: row.try_get("from_state")?,
        to_state: row.try_get("to_state")?,
        actor_kind: row.try_get("actor_kind")?,
        actor_ref: row.try_get("actor_ref")?,
        reason_code: row.try_get("reason_code")?,
        evidence: row.try_get("evidence")?,
        engine_version: row.try_get("engine_version")?,
        decided_at_micros: row.try_get("decided_at_micros")?,
    })
}

/// Up to `limit` records of a client's chain after `after_seq`, in order.
pub async fn page(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<DecisionRecord>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM recon.decision_log
          WHERE firm_id = $1 AND client_id = $2 AND seq > $3
          ORDER BY seq LIMIT $4"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(firm.as_uuid())
        .bind(client.as_uuid())
        .bind(after_seq)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter().map(record).collect()
}

/// Every decision about one subject, in chain order.
pub async fn for_subject(
    pool: &PgPool,
    firm: FirmId,
    kind: SubjectKind,
    subject: Uuid,
) -> Result<Vec<DecisionRecord>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = $2 AND subject_id = $3
          ORDER BY seq"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(firm.as_uuid())
        .bind(kind.code())
        .bind(subject)
        .fetch_all(pool)
        .await?;
    rows.iter().map(record).collect()
}

/// Walks a client's whole chain in pages of 5,000 (constant memory), then checks the stored
/// head is its last record. `Ok(Err(..))` names the first broken link.
pub async fn verify_chain(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
) -> Result<Result<ChainHead, ChainBreak>, DbError> {
    let mut verifier = ChainVerifier::new(firm, client);
    let mut after = 0;
    loop {
        let records = page(pool, firm, client, after, 5_000).await?;
        let Some(last) = records.last() else { break };
        after = last.seq;
        for r in &records {
            if let Err(broken) = verifier.push(r) {
                return Ok(Err(broken));
            }
        }
    }
    let head = verifier.head();
    let stored = sqlx::query(
        "SELECT seq, head_hash FROM recon.chain_heads WHERE firm_id = $1 AND client_id = $2",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    let stored_seq: i64 = stored.try_get("seq")?;
    if stored_seq != head.seq || hash32(&stored, "head_hash")? != head.hash {
        return Ok(Err(ChainBreak {
            seq: head.seq.saturating_add(1),
            kind: BreakKind::HeadMismatch,
        }));
    }
    Ok(Ok(head))
}

/// Every client of `firm` (for verifying all chains).
pub async fn clients(pool: &PgPool, firm: FirmId) -> Result<Vec<ClientId>, DbError> {
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM recon.clients WHERE firm_id = $1 ORDER BY id")
            .bind(firm.as_uuid())
            .fetch_all(pool)
            .await?;
    Ok(ids.into_iter().map(ClientId::from_uuid).collect())
}
