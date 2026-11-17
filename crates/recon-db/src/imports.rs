//! Staged, atomic, idempotent imports (D-008, D-010, D-039).
//!
//! 1. [`begin`] creates a `staging` import under a lease with a fencing token.
//! 2. [`stage`] adds a batch of rows to the (unlogged) staging table; the same statement
//!    renews the lease and counts the rows, and stages nothing if the lease is gone.
//! 3. [`flip`] publishes the whole file in one transaction: serialized per account, it
//!    checks the lease and that every staged row is present, answers with the earlier import
//!    when the file is already complete, otherwise inserts the lines (occurrence indexes and
//!    fingerprints computed in SQL, D-008), records duplicates of existing lines, writes the
//!    completion decision and flips the import to `complete`.
//!
//! Nothing is visible before the flip commits, and the schema refuses lines committed
//! without it, so a crash at any point leaves the import complete or invisible. A `staging`
//! import whose lease expires is discarded by [`sweep`].

use std::future::Future;
use std::time::Duration;

use recon_domain::decision::{ImportEvidence, SubjectKind};
use recon_domain::fingerprint::FINGERPRINT_VERSION;
use recon_domain::import::{
    DiscardReason, FileHash, ImportStatus, NewLine, PROFILE_R0, PROFILE_R0_VERSION,
};
use recon_domain::line::ActorKind;
use recon_domain::refusal::RowRefusal;
use recon_domain::{AccountId, ClientId, Currency, Date, FirmId, ImportId};
use recon_import::ParseSummary;
use serde::Serialize;
use sqlx::postgres::PgRow;
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::DbError;
use crate::decisions::{self, NewDecision};
use crate::tenancy::Account;

/// Rows per staged batch: one statement, one short transaction.
pub const STAGING_BATCH_ROWS: usize = 5_000;
/// Bytes per staged batch ([`NewLine::heap_bytes`]): wide rows flush before 5,000 (D-050).
pub const STAGING_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Who is acting: a kind and a reference (never a secret).
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    /// The kind.
    pub kind: ActorKind,
    /// The reference: `operator`, `sweeper`, later a user id.
    pub reference: &'a str,
}

/// An import being staged by this process (it holds the fencing token).
#[derive(Debug)]
pub struct StagingImport {
    /// The firm.
    pub firm: FirmId,
    /// The import.
    pub id: ImportId,
    /// The client.
    pub client: ClientId,
    /// The account.
    pub account: AccountId,
    /// The account's currency.
    pub currency: Currency,
    lease_token: Uuid,
    staged_rows: u32,
}

impl StagingImport {
    /// Rows staged so far.
    pub const fn staged_rows(&self) -> u32 {
        self.staged_rows
    }
}

/// An import as stored (the API's view of it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportRecord {
    /// The import.
    pub id: Uuid,
    /// The client.
    pub client_id: Uuid,
    /// The account.
    pub account_id: Uuid,
    /// `staging`, `complete`, `refused` or `discarded`.
    pub status: String,
    /// SHA-256 of the file, hex, once read.
    pub file_hash: Option<String>,
    /// Bytes read.
    pub file_bytes: Option<i64>,
    /// Data rows read.
    pub rows_read: Option<i32>,
    /// New lines.
    pub lines_created: Option<i32>,
    /// Rows already present as lines of the account.
    pub duplicates: Option<i32>,
    /// Refused rows.
    pub rows_refused: Option<i32>,
    /// Why the file was refused.
    pub refusal_code: Option<String>,
    /// The first refused rows (codes and row numbers only).
    pub refusals: Vec<RowRefusal>,
    /// The earlier import of the same file, when this one repeated it.
    pub duplicate_of: Option<Uuid>,
    /// Why it was discarded.
    pub discard_reason: Option<String>,
}

impl ImportRecord {
    /// The status as a type.
    pub fn status(&self) -> Option<ImportStatus> {
        ImportStatus::from_code(&self.status)
    }
}

/// Where a flip can be observed (the drills pause the server at these points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipPoint {
    /// Lines and duplicates are inserted; nothing is committed.
    LinesInserted,
    /// Everything is written; the commit is next.
    BeforeCommit,
}

/// Observes a flip from inside its transaction.
pub trait FlipHook: Sync {
    /// Called at each point; the flip continues when the future completes.
    fn reached(&self, point: FlipPoint) -> impl Future<Output = ()> + Send;
}

/// The hook that does nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHook;

impl FlipHook for NoHook {
    async fn reached(&self, _point: FlipPoint) {}
}

/// What a flip did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlipOutcome {
    /// The lines are visible.
    Complete(ImportRecord),
    /// The same file is already complete for the account: this import was discarded and
    /// the earlier one answers.
    DuplicateFile(ImportRecord),
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn date(d: Date) -> Result<time::Date, DbError> {
    let month = time::Month::try_from(d.month())
        .map_err(|_| DbError::Inconsistent("month out of range".into()))?;
    time::Date::from_calendar_date(i32::from(d.year()), month, d.day())
        .map_err(|_| DbError::Inconsistent("invalid date".into()))
}

fn count_i32(n: u32) -> Result<i32, DbError> {
    i32::try_from(n).map_err(|_| DbError::Inconsistent("count out of range".into()))
}

/// Starts a `staging` import into `account` with a lease of `lease`.
pub async fn begin(
    pool: &PgPool,
    account: &Account,
    lease: Duration,
) -> Result<StagingImport, DbError> {
    let id = ImportId::from_uuid(Uuid::now_v7());
    let token = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO recon.imports
             (firm_id, id, client_id, account_id, currency, profile, profile_version,
              lease_token, lease_until)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() + ($9::bigint * interval '1 millisecond'))",
    )
    .bind(account.firm.as_uuid())
    .bind(id.as_uuid())
    .bind(account.client.as_uuid())
    .bind(account.id.as_uuid())
    .bind(account.currency.code())
    .bind(PROFILE_R0)
    .bind(PROFILE_R0_VERSION)
    .bind(token)
    .bind(millis(lease))
    .execute(pool)
    .await?;
    Ok(StagingImport {
        firm: account.firm,
        id,
        client: account.client,
        account: account.id,
        currency: account.currency,
        lease_token: token,
        staged_rows: 0,
    })
}

/// One batch as column arrays for `unnest`.
struct Columns<'a> {
    row_no: Vec<i32>,
    booking: Vec<time::Date>,
    value: Vec<Option<time::Date>>,
    amount: Vec<i64>,
    narration_raw: Vec<&'a str>,
    narration_norm: Vec<&'a str>,
    reference: Vec<Option<&'a str>>,
    counterparty_raw: Vec<Option<&'a str>>,
    counterparty_norm: Vec<Option<&'a str>>,
    balance: Vec<Option<i64>>,
    tuple: Vec<&'a [u8]>,
}

impl<'a> Columns<'a> {
    fn from_rows(rows: &'a [NewLine], currency: Currency) -> Result<Self, DbError> {
        let n = rows.len();
        let mut c = Self {
            row_no: Vec::with_capacity(n),
            booking: Vec::with_capacity(n),
            value: Vec::with_capacity(n),
            amount: Vec::with_capacity(n),
            narration_raw: Vec::with_capacity(n),
            narration_norm: Vec::with_capacity(n),
            reference: Vec::with_capacity(n),
            counterparty_raw: Vec::with_capacity(n),
            counterparty_norm: Vec::with_capacity(n),
            balance: Vec::with_capacity(n),
            tuple: Vec::with_capacity(n),
        };
        for r in rows {
            if r.amount.currency() != currency {
                return Err(DbError::Inconsistent(
                    "row currency is not the account's".into(),
                ));
            }
            c.row_no.push(count_i32(r.row)?);
            c.booking.push(date(r.booking_date)?);
            c.value.push(r.value_date.map(date).transpose()?);
            c.amount.push(r.amount.minor());
            c.narration_raw.push(r.narration_raw.as_str());
            c.narration_norm.push(r.narration_norm.as_str());
            c.reference.push(r.reference.as_deref());
            c.counterparty_raw.push(r.counterparty_raw.as_deref());
            c.counterparty_norm.push(r.counterparty_norm.as_deref());
            c.balance
                .push(r.balance_after.map(recon_domain::Money::minor));
            c.tuple.push(r.tuple_hash.0.as_slice());
        }
        Ok(c)
    }
}

/// Stages one batch (at most [`STAGING_BATCH_ROWS`] rows) and renews the lease, in one
/// transaction. Refused with [`DbError::LeaseLost`] when the lease is gone.
pub async fn stage(
    pool: &PgPool,
    import: &mut StagingImport,
    rows: &[NewLine],
    lease: Duration,
) -> Result<(), DbError> {
    if rows.is_empty() {
        return Ok(());
    }
    let n = rows.len();
    let c = Columns::from_rows(rows, import.currency)?;
    let batch = u32::try_from(n).map_err(|_| DbError::Inconsistent("batch too large".into()))?;
    // Fenced and locked first: the sweeper skips a locked import, so a slow batch cannot
    // be reaped mid-way. The lease is extended from the clock after the insert, so it
    // always outlives the batch that renewed it (a batch can take longer than a lease on a
    // loaded machine; a lease computed from the statement's start would already be over).
    let mut tx = pool.begin().await?;
    let live: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM recon.imports
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3
            AND status = 'staging' AND lease_until > pg_catalog.clock_timestamp()
            FOR UPDATE",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(import.lease_token)
    .fetch_optional(&mut *tx)
    .await?;
    if live.is_none() {
        return Err(DbError::LeaseLost);
    }
    let inserted = sqlx::query(
        "INSERT INTO recon.import_staging
             (firm_id, import_id, row_no, booking_date, value_date, amount_minor, narration_raw,
              narration_norm, reference, counterparty_raw, counterparty_norm,
              balance_after_minor, tuple_hash)
         SELECT $1, $2, t.*
           FROM ROWS FROM (
                    pg_catalog.unnest($3::int4[]), pg_catalog.unnest($4::date[]),
                    pg_catalog.unnest($5::date[]), pg_catalog.unnest($6::int8[]),
                    pg_catalog.unnest($7::text[]), pg_catalog.unnest($8::text[]),
                    pg_catalog.unnest($9::text[]), pg_catalog.unnest($10::text[]),
                    pg_catalog.unnest($11::text[]), pg_catalog.unnest($12::int8[]),
                    pg_catalog.unnest($13::bytea[])) AS t",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(&c.row_no)
    .bind(&c.booking)
    .bind(&c.value)
    .bind(&c.amount)
    .bind(&c.narration_raw)
    .bind(&c.narration_norm)
    .bind(&c.reference)
    .bind(&c.counterparty_raw)
    .bind(&c.counterparty_norm)
    .bind(&c.balance)
    .bind(&c.tuple)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted != u64::from(batch) {
        return Err(DbError::Inconsistent(
            "a staged batch was partly inserted".into(),
        ));
    }
    sqlx::query(
        "UPDATE recon.imports
            SET lease_until = pg_catalog.clock_timestamp() + ($4::bigint * interval '1 millisecond'),
                staged_rows = staged_rows + $5
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(import.lease_token)
    .bind(millis(lease))
    .bind(count_i32(batch)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    import.staged_rows = import.staged_rows.saturating_add(batch);
    Ok(())
}

/// Renews the lease without staging (an upload slower than a batch per lease period).
/// Refused with [`DbError::LeaseLost`] when the lease is already gone.
pub async fn renew(pool: &PgPool, import: &StagingImport, lease: Duration) -> Result<(), DbError> {
    let renewed = sqlx::query(
        "UPDATE recon.imports
            SET lease_until = pg_catalog.clock_timestamp() + ($4::bigint * interval '1 millisecond')
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3
            AND status = 'staging' AND lease_until > pg_catalog.clock_timestamp()",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(import.lease_token)
    .bind(millis(lease))
    .execute(pool)
    .await?
    .rows_affected();
    if renewed == 0 {
        return Err(DbError::LeaseLost);
    }
    Ok(())
}

/// Locks the staging import (fencing): `Ok(staged_rows)` while the lease is live.
async fn lock_staging(conn: &mut PgConnection, import: &StagingImport) -> Result<i32, DbError> {
    let row = sqlx::query(
        "SELECT staged_rows, lease_until > pg_catalog.clock_timestamp() AS live
           FROM recon.imports
          WHERE firm_id = $1 AND id = $2 AND lease_token = $3 AND status = 'staging'
            FOR UPDATE",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(import.lease_token)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::LeaseLost)?;
    if !row.try_get::<bool, _>("live")? {
        return Err(DbError::LeaseLost);
    }
    Ok(row.try_get("staged_rows")?)
}

async fn delete_staging(conn: &mut PgConnection, import: &StagingImport) -> Result<u64, DbError> {
    Ok(
        sqlx::query("DELETE FROM recon.import_staging WHERE firm_id = $1 AND import_id = $2")
            .bind(import.firm.as_uuid())
            .bind(import.id.as_uuid())
            .execute(&mut *conn)
            .await?
            .rows_affected(),
    )
}

fn json(value: &impl Serialize) -> Result<Vec<u8>, DbError> {
    serde_json::to_vec(value).map_err(|e| DbError::Inconsistent(format!("evidence: {e}")))
}

/// Publishes a clean, fully staged file (see the module docs). `flip_timeout` bounds every
/// statement of the flip's transaction.
pub async fn flip<H: FlipHook>(
    pool: &PgPool,
    import: StagingImport,
    summary: &ParseSummary,
    actor: Actor<'_>,
    hook: &H,
    flip_timeout: Duration,
) -> Result<FlipOutcome, DbError> {
    if !summary.is_clean() {
        return Err(DbError::Inconsistent(
            "a refused file cannot be flipped".into(),
        ));
    }
    let rows_read = count_i32(summary.rows_read)?;
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = '{}ms'",
        millis(flip_timeout)
    )))
    .execute(&mut *tx)
    .await?;
    // No nested loops in the flip (D-043): the staging table is never analyzed (the runtime
    // role cannot) and a fresh account's lines look empty to the planner, which then picks
    // nested loops that walk every index entry this statement inserts - quadratic (over
    // 120 s for 30,000 rows on a fresh database, 0.46 s with a hash join). Its joins are
    // all bulk joins; the ON CONFLICT and foreign-key probes use their indexes regardless.
    sqlx::query("SET LOCAL enable_nestloop = off")
        .execute(&mut *tx)
        .await?;
    // Imports into one account are serialized, so duplicate detection never races. A flip
    // that finds the account held is refused at once and discarded (D-053), rather than
    // waiting out the lock timeout behind a large flip.
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_catalog.pg_try_advisory_xact_lock(pg_catalog.hashtextextended($1::text, 7262))",
    )
    .bind(import.account.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if !acquired {
        drop(tx);
        discard_for(pool, import, DiscardReason::AccountBusy, actor).await?;
        return Err(DbError::AccountBusy);
    }
    let staged = lock_staging(&mut tx, &import).await?;
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.import_staging WHERE firm_id = $1 AND import_id = $2",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if staged != rows_read || present != i64::from(rows_read) {
        return Err(DbError::StagingIncomplete);
    }
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM recon.imports
          WHERE firm_id = $1 AND account_id = $2 AND file_hash = $3 AND status = 'complete'",
    )
    .bind(import.firm.as_uuid())
    .bind(import.account.as_uuid())
    .bind(summary.file_hash.0.as_slice())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        discard_as_duplicate(&mut tx, &import, existing, summary, actor).await?;
        tx.commit().await?;
        let earlier = get(pool, import.firm, ImportId::from_uuid(existing)).await?;
        return Ok(FlipOutcome::DuplicateFile(earlier));
    }
    let published = insert_lines(&mut tx, &import, rows_read).await?;
    hook.reached(FlipPoint::LinesInserted).await;
    delete_staging(&mut tx, &import).await?;
    record_completion(&mut tx, &import, summary, &published, actor).await?;
    hook.reached(FlipPoint::BeforeCommit).await;
    tx.commit().await?;
    Ok(FlipOutcome::Complete(
        get(pool, import.firm, import.id).await?,
    ))
}

async fn discard_as_duplicate(
    tx: &mut PgConnection,
    import: &StagingImport,
    existing: Uuid,
    summary: &ParseSummary,
    actor: Actor<'_>,
) -> Result<(), DbError> {
    delete_staging(tx, import).await?;
    let evidence = json(&serde_json::json!({
        "import": import.id.as_uuid(),
        "duplicate_of": existing,
        "file_hash": summary.file_hash.to_string(),
        "rows_read": summary.rows_read,
    }))?;
    decisions::insert(
        tx,
        import.firm,
        &NewDecision {
            client: import.client,
            subject_kind: SubjectKind::Import,
            subject_id: import.id.as_uuid(),
            from_state: Some(ImportStatus::Staging.code()),
            to_state: ImportStatus::Discarded.code(),
            actor_kind: actor.kind,
            actor_ref: actor.reference,
            reason_code: DiscardReason::DuplicateFile.code(),
            evidence: &evidence,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE recon.imports
            SET status = 'discarded', discard_reason = 'duplicate_file', duplicate_of = $3,
                file_hash = $4, file_bytes = $5, rows_read = $6
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(existing)
    .bind(summary.file_hash.0.as_slice())
    .bind(i64::try_from(summary.file_bytes).unwrap_or(i64::MAX))
    .bind(count_i32(summary.rows_read)?)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The counts and digest of the lines a flip inserted.
struct Published {
    lines_created: i32,
    duplicates: i32,
    digest: Vec<u8>,
}

/// Inserts the staged rows as lines (occurrence indexes and fingerprints computed here,
/// D-008) and records rows that existing lines already have as duplicates.
async fn insert_lines(
    tx: &mut PgConnection,
    import: &StagingImport,
    rows_read: i32,
) -> Result<Published, DbError> {
    let counts = sqlx::query(
        "WITH staged AS (
             SELECT s.row_no, s.booking_date, s.value_date, s.amount_minor, s.narration_raw,
                    s.narration_norm, s.reference, s.counterparty_raw, s.counterparty_norm,
                    s.balance_after_minor, s.tuple_hash,
                    (pg_catalog.row_number() OVER (PARTITION BY s.tuple_hash ORDER BY s.row_no) - 1)::int4
                        AS occurrence
               FROM recon.import_staging s
              WHERE s.firm_id = $1 AND s.import_id = $2
         ),
         fp AS (
             SELECT st.*,
                    pg_catalog.sha256(pg_catalog.convert_to('recon-fp-v1', 'UTF8')
                        || st.tuple_hash || pg_catalog.int4send(st.occurrence)) AS fingerprint
               FROM staged st
         ),
         inserted AS (
             INSERT INTO recon.statement_lines
                 (firm_id, client_id, account_id, import_id, currency, row_no, booking_date,
                  value_date, amount_minor, narration_raw, narration_norm, reference,
                  counterparty_raw, counterparty_norm, balance_after_minor, tuple_hash,
                  occurrence, fingerprint)
             SELECT $1, $3, $4, $2, $5, fp.row_no, fp.booking_date, fp.value_date,
                    fp.amount_minor, fp.narration_raw, fp.narration_norm, fp.reference,
                    fp.counterparty_raw, fp.counterparty_norm, fp.balance_after_minor,
                    fp.tuple_hash, fp.occurrence, fp.fingerprint
               FROM fp ORDER BY fp.row_no
             ON CONFLICT (firm_id, account_id, fingerprint) DO NOTHING
             RETURNING row_no
         ),
         -- Every CTE reads the snapshot from before this statement, so this join sees only
         -- lines that existed before (never this import's): exactly the rows ON CONFLICT
         -- skipped. The flip runs with nested loops disabled (see `flip`), so this is a hash
         -- or merge join: one pass over the account's lines.
         dups AS (
             INSERT INTO recon.import_duplicates (firm_id, import_id, row_no, existing_line_id)
             SELECT $1, $2, fp.row_no, l.id
               FROM fp
               JOIN recon.statement_lines l
                 ON l.firm_id = $1 AND l.account_id = $4 AND l.fingerprint = fp.fingerprint
             RETURNING row_no
         )
         SELECT (SELECT count(*) FROM inserted) AS lines_created,
                (SELECT count(*) FROM dups) AS duplicates,
                (SELECT count(*) FROM fp) AS total,
                pg_catalog.sha256(coalesce(
                    (SELECT pg_catalog.string_agg(fp.fingerprint, ''::bytea ORDER BY fp.row_no) FROM fp),
                    ''::bytea)) AS digest",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(import.client.as_uuid())
    .bind(import.account.as_uuid())
    .bind(import.currency.code())
    .fetch_one(&mut *tx)
    .await?;
    let lines_created: i64 = counts.try_get("lines_created")?;
    let duplicates: i64 = counts.try_get("duplicates")?;
    let total: i64 = counts.try_get("total")?;
    if total != i64::from(rows_read) || lines_created.checked_add(duplicates) != Some(total) {
        return Err(DbError::Inconsistent(format!(
            "flip accounted for {lines_created} + {duplicates} of {total} rows, read {rows_read}"
        )));
    }
    Ok(Published {
        lines_created: i32::try_from(lines_created).unwrap_or(i32::MAX),
        duplicates: i32::try_from(duplicates).unwrap_or(i32::MAX),
        digest: counts.try_get("digest")?,
    })
}

/// Writes the completion decision and flips the import to `complete`.
async fn record_completion(
    tx: &mut PgConnection,
    import: &StagingImport,
    summary: &ParseSummary,
    published: &Published,
    actor: Actor<'_>,
) -> Result<(), DbError> {
    let evidence = json(&ImportEvidence {
        import: import.id.as_uuid(),
        account: import.account.as_uuid(),
        file_hash: summary.file_hash.to_string(),
        profile: PROFILE_R0.to_owned(),
        profile_version: PROFILE_R0_VERSION,
        fingerprint_version: FINGERPRINT_VERSION.to_owned(),
        rows_read: summary.rows_read,
        lines_created: u32::try_from(published.lines_created).unwrap_or(0),
        duplicates: u32::try_from(published.duplicates).unwrap_or(0),
        lines_digest: hex::encode(&published.digest),
    })?;
    decisions::insert(
        tx,
        import.firm,
        &NewDecision {
            client: import.client,
            subject_kind: SubjectKind::Import,
            subject_id: import.id.as_uuid(),
            from_state: Some(ImportStatus::Staging.code()),
            to_state: ImportStatus::Complete.code(),
            actor_kind: actor.kind,
            actor_ref: actor.reference,
            reason_code: "import_completed",
            evidence: &evidence,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE recon.imports
            SET status = 'complete', file_hash = $3, file_bytes = $4, rows_read = $5,
                lines_created = $6, duplicates = $7, rows_refused = 0
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(summary.file_hash.0.as_slice())
    .bind(i64::try_from(summary.file_bytes).unwrap_or(i64::MAX))
    .bind(count_i32(summary.rows_read)?)
    .bind(published.lines_created)
    .bind(published.duplicates)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Why a file was refused: a file-level code, or `rows_refused` with the report.
#[derive(Debug, Clone)]
pub struct Refusal<'a> {
    /// The code stored on the import (`rows_refused` or a file-level code).
    pub code: &'a str,
    /// Data rows read (when known).
    pub rows_read: Option<u32>,
    /// Refused rows.
    pub rows_refused: u32,
    /// The first refusals.
    pub refusals: &'a [RowRefusal],
    /// The file's hash, when it was read to the end.
    pub file_hash: Option<FileHash>,
    /// Bytes read.
    pub file_bytes: u64,
}

/// Marks the import `refused` (nothing persisted: staged rows are deleted) and records the
/// decision, in one transaction.
pub async fn refuse(
    pool: &PgPool,
    import: StagingImport,
    refusal: &Refusal<'_>,
    actor: Actor<'_>,
) -> Result<ImportRecord, DbError> {
    let mut tx = pool.begin().await?;
    lock_staging(&mut tx, &import).await?;
    delete_staging(&mut tx, &import).await?;
    let evidence = json(&serde_json::json!({
        "import": import.id.as_uuid(),
        "refusal_code": refusal.code,
        "rows_read": refusal.rows_read,
        "rows_refused": refusal.rows_refused,
    }))?;
    decisions::insert(
        &mut tx,
        import.firm,
        &NewDecision {
            client: import.client,
            subject_kind: SubjectKind::Import,
            subject_id: import.id.as_uuid(),
            from_state: Some(ImportStatus::Staging.code()),
            to_state: ImportStatus::Refused.code(),
            actor_kind: actor.kind,
            actor_ref: actor.reference,
            reason_code: refusal.code,
            evidence: &evidence,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE recon.imports
            SET status = 'refused', refusal_code = $3, refusals = $4, rows_read = $5,
                rows_refused = $6, file_hash = $7, file_bytes = $8
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(refusal.code)
    .bind(Json(refusal.refusals))
    .bind(refusal.rows_read.map(count_i32).transpose()?)
    .bind(count_i32(refusal.rows_refused)?)
    .bind(refusal.file_hash.as_ref().map(|h| h.0.to_vec()))
    .bind(i64::try_from(refusal.file_bytes).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    get(pool, import.firm, import.id).await
}

/// Discards an import whose upload failed (a client that went away, a deadline). Returns
/// `false` when the lease was already lost (the sweeper will, or did, discard it).
pub async fn discard(
    pool: &PgPool,
    import: StagingImport,
    actor: Actor<'_>,
) -> Result<bool, DbError> {
    discard_for(pool, import, DiscardReason::UploadFailed, actor).await
}

/// Discards a staging import for `reason`, with its decision; `false` when the lease was
/// already lost.
async fn discard_for(
    pool: &PgPool,
    import: StagingImport,
    reason: DiscardReason,
    actor: Actor<'_>,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    match lock_staging(&mut tx, &import).await {
        Ok(_) => {}
        Err(DbError::LeaseLost) => return Ok(false),
        Err(e) => return Err(e),
    }
    delete_staging(&mut tx, &import).await?;
    let evidence = json(&serde_json::json!({ "import": import.id.as_uuid() }))?;
    decisions::insert(
        &mut tx,
        import.firm,
        &NewDecision {
            client: import.client,
            subject_kind: SubjectKind::Import,
            subject_id: import.id.as_uuid(),
            from_state: Some(ImportStatus::Staging.code()),
            to_state: ImportStatus::Discarded.code(),
            actor_kind: actor.kind,
            actor_ref: actor.reference,
            reason_code: reason.code(),
            evidence: &evidence,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE recon.imports SET status = 'discarded', discard_reason = $3
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(reason.code())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// What one sweep did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Staging imports whose lease had expired, now discarded.
    pub discarded: u32,
    /// Staged rows of finished imports deleted.
    pub staging_rows_deleted: u64,
}

/// Discards up to `max_imports` staging imports whose lease expired (each with its
/// decision) and deletes up to `max_rows` staged rows that belong to finished imports.
pub async fn sweep(pool: &PgPool, max_imports: i64, max_rows: i64) -> Result<SweepReport, DbError> {
    let mut tx = pool.begin().await?;
    let expired = sqlx::query(
        "WITH expired AS (
             SELECT firm_id, id FROM recon.imports
              WHERE status = 'staging' AND lease_until < now()
              ORDER BY lease_until
              LIMIT $1
                FOR UPDATE SKIP LOCKED)
         UPDATE recon.imports i
            SET status = 'discarded', discard_reason = 'lease_expired'
           FROM expired e
          WHERE i.firm_id = e.firm_id AND i.id = e.id
         RETURNING i.firm_id, i.id, i.client_id",
    )
    .bind(max_imports)
    .fetch_all(&mut *tx)
    .await?;
    for row in &expired {
        let firm = FirmId::from_uuid(row.try_get("firm_id")?);
        let id: Uuid = row.try_get("id")?;
        let evidence = json(&serde_json::json!({ "import": id }))?;
        decisions::insert(
            &mut tx,
            firm,
            &NewDecision {
                client: ClientId::from_uuid(row.try_get("client_id")?),
                subject_kind: SubjectKind::Import,
                subject_id: id,
                from_state: Some(ImportStatus::Staging.code()),
                to_state: ImportStatus::Discarded.code(),
                actor_kind: ActorKind::System,
                actor_ref: "sweeper",
                reason_code: DiscardReason::LeaseExpired.code(),
                evidence: &evidence,
            },
        )
        .await?;
    }
    tx.commit().await?;
    let deleted = sqlx::query(
        "DELETE FROM recon.import_staging s
          USING (SELECT s2.firm_id, s2.import_id, s2.row_no
                   FROM recon.import_staging s2
                   JOIN recon.imports i ON i.firm_id = s2.firm_id AND i.id = s2.import_id
                  WHERE i.status <> 'staging'
                  LIMIT $1) d
          WHERE s.firm_id = d.firm_id AND s.import_id = d.import_id AND s.row_no = d.row_no",
    )
    .bind(max_rows)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(SweepReport {
        discarded: u32::try_from(expired.len()).unwrap_or(u32::MAX),
        staging_rows_deleted: deleted,
    })
}

fn import_record(row: &PgRow) -> Result<ImportRecord, DbError> {
    let file_hash: Option<Vec<u8>> = row.try_get("file_hash")?;
    let refusals: Option<Json<Vec<RowRefusal>>> = row.try_get("refusals")?;
    Ok(ImportRecord {
        id: row.try_get("id")?,
        client_id: row.try_get("client_id")?,
        account_id: row.try_get("account_id")?,
        status: row.try_get("status")?,
        file_hash: file_hash.map(hex::encode),
        file_bytes: row.try_get("file_bytes")?,
        rows_read: row.try_get("rows_read")?,
        lines_created: row.try_get("lines_created")?,
        duplicates: row.try_get("duplicates")?,
        rows_refused: row.try_get("rows_refused")?,
        refusal_code: row.try_get("refusal_code")?,
        refusals: refusals.map(|j| j.0).unwrap_or_default(),
        duplicate_of: row.try_get("duplicate_of")?,
        discard_reason: row.try_get("discard_reason")?,
    })
}

/// Loads `firm`'s import `id`.
pub async fn get(pool: &PgPool, firm: FirmId, id: ImportId) -> Result<ImportRecord, DbError> {
    let row = sqlx::query(
        "SELECT id, client_id, account_id, status, file_hash, file_bytes, rows_read,
                lines_created, duplicates, rows_refused, refusal_code, refusals, duplicate_of,
                discard_reason
           FROM recon.imports WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    import_record(&row)
}

/// Every import of `firm`'s account, oldest first (tests, drills, reports).
pub async fn for_account(
    pool: &PgPool,
    firm: FirmId,
    account: AccountId,
) -> Result<Vec<ImportRecord>, DbError> {
    let rows = sqlx::query(
        "SELECT id, client_id, account_id, status, file_hash, file_bytes, rows_read,
                lines_created, duplicates, rows_refused, refusal_code, refusals, duplicate_of,
                discard_reason
           FROM recon.imports WHERE firm_id = $1 AND account_id = $2
          ORDER BY created_at, id",
    )
    .bind(firm.as_uuid())
    .bind(account.as_uuid())
    .fetch_all(pool)
    .await?;
    rows.iter().map(import_record).collect()
}

/// A digest of every line of an account, ordered by row and fingerprint, over every
/// stored field: two accounts with equal digests hold identical lines (drills compare a crashed
/// and re-run import against an uninterrupted one with it).
pub async fn account_lines_digest(
    pool: &PgPool,
    firm: FirmId,
    account: AccountId,
) -> Result<(i64, String), DbError> {
    let row = sqlx::query(
        "SELECT count(*) AS n,
                pg_catalog.encode(pg_catalog.sha256(coalesce(pg_catalog.string_agg(
                    pg_catalog.convert_to(pg_catalog.concat_ws('|', l.row_no, pg_catalog.encode(l.fingerprint, 'hex'),
                        l.booking_date, l.value_date, l.amount_minor, l.currency, l.narration_raw,
                        l.narration_norm, l.reference, l.counterparty_raw, l.counterparty_norm,
                        l.balance_after_minor, l.occurrence, l.status), 'UTF8'),
                    '\\x0a'::bytea ORDER BY l.row_no, l.fingerprint), ''::bytea)), 'hex') AS digest
           FROM recon.statement_lines l
          WHERE l.firm_id = $1 AND l.account_id = $2",
    )
    .bind(firm.as_uuid())
    .bind(account.as_uuid())
    .fetch_one(pool)
    .await?;
    Ok((row.try_get("n")?, row.try_get("digest")?))
}
