//! The replay check (AGENTS.md testing rule): the decision log, replayed, must equal the
//! stored current state. Run after every drill and by `recon audit replay`.
//!
//! For one firm it checks that every client's chain verifies to its stored head; that every
//! decision about an import or a line names one of its own client; that every import's
//! decisions follow the import state machine from `staging` and lead to its status (no
//! decision: still `staging`); that every complete import's lines (or documents) and
//! duplicates are exactly the counts and the fingerprint digest its completion decision
//! recorded; that a held revision keeps every row it read and lists as many changes as it
//! says; and that every line belongs to a complete import and is in the state its own
//! decisions lead to (`unreviewed` until the first one; `superseded` through an accepted
//! revision, D-065).

use recon_domain::FirmId;
use recon_domain::decision::ImportEvidence;
use recon_domain::import::ImportStatus;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::DbError;
use crate::decisions;

/// What a passing replay covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayReport {
    /// Clients whose chains verified.
    pub clients: u32,
    /// Decisions verified.
    pub decisions: i64,
    /// Imports checked.
    pub imports: u32,
    /// Complete imports whose lines were checked.
    pub complete_imports: u32,
    /// Lines checked.
    pub lines: i64,
}

/// A disagreement between the log and the stored state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("replay mismatch: {0}")]
pub struct ReplayMismatch(pub String);

type Checked = Result<Result<(), ReplayMismatch>, DbError>;

#[expect(
    clippy::unnecessary_wraps,
    reason = "shapes every check's early return"
)]
fn mismatch(message: String) -> Checked {
    Ok(Err(ReplayMismatch(message)))
}

/// Replays `firm`'s log against its stored state.
pub async fn check(
    pool: &PgPool,
    firm: FirmId,
) -> Result<Result<ReplayReport, ReplayMismatch>, DbError> {
    let mut report = ReplayReport::default();
    if let Err(m) = check_chains(pool, firm, &mut report).await? {
        return Ok(Err(m));
    }
    if let Err(m) = check_imports(pool, firm, &mut report).await? {
        return Ok(Err(m));
    }
    if let Err(m) = check_lines(pool, firm, &mut report).await? {
        return Ok(Err(m));
    }
    Ok(Ok(report))
}

async fn check_chains(pool: &PgPool, firm: FirmId, report: &mut ReplayReport) -> Checked {
    for client in decisions::clients(pool, firm).await? {
        match decisions::verify_chain(pool, firm, client).await? {
            Ok(head) => {
                report.clients = report.clients.saturating_add(1);
                report.decisions = report.decisions.saturating_add(head.seq);
            }
            Err(broken) => return mismatch(format!("client {client}: {broken}")),
        }
    }
    Ok(Ok(()))
}

async fn check_imports(pool: &PgPool, firm: FirmId, report: &mut ReplayReport) -> Checked {
    // A decision about an import or a line that is not one of its own client (another
    // firm's, another client's, or none) would otherwise be examined by no one (D-056).
    let strays: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.decision_log d
          WHERE d.firm_id = $1
            AND ((d.subject_kind = 'import'
                  AND NOT EXISTS (SELECT 1 FROM recon.imports i
                                   WHERE i.firm_id = d.firm_id AND i.id = d.subject_id
                                     AND i.client_id = d.client_id))
              OR (d.subject_kind = 'line'
                  AND NOT EXISTS (SELECT 1 FROM recon.statement_lines l
                                   WHERE l.firm_id = d.firm_id AND l.id = d.subject_id
                                     AND l.client_id = d.client_id)))",
    )
    .bind(firm.as_uuid())
    .fetch_one(pool)
    .await?;
    if strays != 0 {
        return mismatch(format!(
            "{strays} decisions name an import or line that is not their client's"
        ));
    }
    let imports = sqlx::query(
        "SELECT i.id, i.kind, i.status, i.lines_created, i.duplicates, i.rows_read, i.changes,
                coalesce((SELECT pg_catalog.array_agg(coalesce(d.from_state, '') || '>' || d.to_state
                                                       ORDER BY d.seq)
                            FROM recon.decision_log d
                           WHERE d.firm_id = i.firm_id AND d.subject_kind = 'import'
                             AND d.subject_id = i.id), '{}') AS steps,
                (SELECT d.evidence FROM recon.decision_log d
                  WHERE d.firm_id = i.firm_id AND d.subject_kind = 'import' AND d.subject_id = i.id
                  ORDER BY d.seq DESC LIMIT 1) AS evidence,
                (SELECT count(*) FROM recon.import_held_rows h
                  WHERE h.firm_id = i.firm_id AND h.import_id = i.id) AS held_rows,
                (SELECT count(*) FROM recon.import_revision_changes c
                  WHERE c.firm_id = i.firm_id AND c.import_id = i.id) AS listed
           FROM recon.imports i
          WHERE i.firm_id = $1",
    )
    .bind(firm.as_uuid())
    .fetch_all(pool)
    .await?;
    for row in &imports {
        report.imports = report.imports.saturating_add(1);
        let id: Uuid = row.try_get("id")?;
        let status: String = row.try_get("status")?;
        let steps: Vec<String> = row.try_get("steps")?;
        // Replay the import state machine from `staging`.
        let mut state = ImportStatus::Staging;
        for step in &steps {
            let (from, to) = step.split_once('>').unwrap_or_default();
            let next = ImportStatus::from_code(to);
            match next {
                Some(next) if from == state.code() && state.can_become(next) => state = next,
                _ => {
                    return mismatch(format!(
                        "import {id}: decision {step} does not follow from {}",
                        state.code()
                    ));
                }
            }
        }
        if status != state.code() {
            return mismatch(format!(
                "import {id} is {status}, its decisions say {}",
                state.code()
            ));
        }
        let held: i64 = row.try_get("held_rows")?;
        let listed: i64 = row.try_get("listed")?;
        if state == ImportStatus::Revision {
            let rows_read = row.try_get::<Option<i32>, _>("rows_read")?.map(i64::from);
            let changes = row.try_get::<Option<i32>, _>("changes")?.map(i64::from);
            if rows_read != Some(held) || changes != Some(listed) {
                return mismatch(format!(
                    "revision {id}: {held} held rows and {listed} changes against {rows_read:?} \
                     read and {changes:?} listed"
                ));
            }
        } else if held != 0 {
            return mismatch(format!("import {id} is {status} but holds {held} rows"));
        }
        if state != ImportStatus::Complete {
            continue;
        }
        report.complete_imports = report.complete_imports.saturating_add(1);
        if let Err(m) = check_completion(pool, firm, row, id).await? {
            return Ok(Err(m));
        }
    }
    Ok(Ok(()))
}

/// A complete import's rows (lines or documents) and duplicates are exactly the counts and
/// the fingerprint digest its completion decision recorded.
async fn check_completion(
    pool: &PgPool,
    firm: FirmId,
    row: &sqlx::postgres::PgRow,
    id: Uuid,
) -> Checked {
    let evidence: Vec<u8> = row.try_get("evidence")?;
    let Ok(evidence) = serde_json::from_slice::<ImportEvidence>(&evidence) else {
        return mismatch(format!("import {id}: unreadable completion evidence"));
    };
    let kind: String = row.try_get("kind")?;
    let (rows_table, dups_table, existing) = if kind == "documents" {
        (
            "recon.documents",
            "recon.document_duplicates",
            "existing_document_id",
        )
    } else {
        (
            "recon.statement_lines",
            "recon.import_duplicates",
            "existing_line_id",
        )
    };
    let sql = format!(
        "SELECT (SELECT count(*) FROM {rows_table}
                  WHERE firm_id = $1 AND import_id = $2) AS lines,
                (SELECT count(*) FROM {dups_table}
                  WHERE firm_id = $1 AND import_id = $2) AS duplicates,
                pg_catalog.encode(pg_catalog.sha256(coalesce((
                    SELECT pg_catalog.string_agg(x.f, ''::bytea ORDER BY x.r) FROM (
                        SELECT l.row_no AS r, l.fingerprint AS f
                          FROM {rows_table} l
                         WHERE l.firm_id = $1 AND l.import_id = $2
                        UNION ALL
                        SELECT d.row_no, l.fingerprint
                          FROM {dups_table} d
                          JOIN {rows_table} l
                            ON l.firm_id = d.firm_id AND l.id = d.{existing}
                         WHERE d.firm_id = $1 AND d.import_id = $2) x), ''::bytea)), 'hex')
                    AS digest"
    );
    let actual = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(firm.as_uuid())
        .bind(id)
        .fetch_one(pool)
        .await?;
    let lines: i64 = actual.try_get("lines")?;
    let duplicates: i64 = actual.try_get("duplicates")?;
    let digest: String = actual.try_get("digest")?;
    if lines != i64::from(evidence.lines_created)
        || duplicates != i64::from(evidence.duplicates)
        || digest != evidence.lines_digest
        || Some(lines)
            != row
                .try_get::<Option<i32>, _>("lines_created")?
                .map(i64::from)
        || Some(duplicates) != row.try_get::<Option<i32>, _>("duplicates")?.map(i64::from)
    {
        return mismatch(format!(
            "import {id}: {lines} rows and {duplicates} duplicates (digest {digest}) \
             against the decision's {} and {} (digest {})",
            evidence.lines_created, evidence.duplicates, evidence.lines_digest
        ));
    }
    Ok(Ok(()))
}

async fn check_lines(pool: &PgPool, firm: FirmId, report: &mut ReplayReport) -> Checked {
    // One pass each over the firm's lines and its line decisions (a join, not a correlated
    // probe per line: statistics may be missing, D-043).
    let lines = sqlx::query(
        "WITH latest AS (
             SELECT DISTINCT ON (d.subject_id) d.subject_id, d.to_state
               FROM recon.decision_log d
              WHERE d.firm_id = $1 AND d.subject_kind = 'line'
              ORDER BY d.subject_id, d.seq DESC)
         SELECT count(*) AS total,
                count(*) FILTER (WHERE i.status IS DISTINCT FROM 'complete') AS orphaned,
                count(*) FILTER (WHERE l.status <> coalesce(latest.to_state, 'unreviewed')) AS diverged
           FROM recon.statement_lines l
           LEFT JOIN recon.imports i ON i.firm_id = l.firm_id AND i.id = l.import_id
           LEFT JOIN latest ON latest.subject_id = l.id
          WHERE l.firm_id = $1",
    )
    .bind(firm.as_uuid())
    .fetch_one(pool)
    .await?;
    let orphaned: i64 = lines.try_get("orphaned")?;
    let diverged: i64 = lines.try_get("diverged")?;
    if orphaned != 0 || diverged != 0 {
        return mismatch(format!(
            "{orphaned} lines outside a complete import, {diverged} lines not in their decided state"
        ));
    }
    report.lines = lines.try_get("total")?;
    Ok(Ok(()))
}
