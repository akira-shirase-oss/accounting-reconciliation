//! Staged, atomic, idempotent imports (D-008, D-010, D-039), statements and documents.
//!
//! 1. [`begin`] creates a `staging` import under a lease with a fencing token.
//! 2. [`stage`] adds a batch of rows to the (unlogged) staging table; the same transaction
//!    renews the lease and counts the rows, and stages nothing if the lease is gone.
//! 3. [`flip`] publishes the whole file in one transaction, serialized per account (or per
//!    client for documents): it checks the lease and that every staged row is present,
//!    answers with the earlier import when the same file is already complete (or held),
//!    and otherwise compares the file with the stored lines in the dates it shares with
//!    them (D-065):
//!    * nothing stored is missing from the file — the lines are inserted (occurrence
//!      indexes and fingerprints computed in SQL, D-008), rows that existing lines already
//!      have are recorded as duplicates naming them, the overlap is reported, and the
//!      completion decision flips the import to `complete`;
//!    * a stored line in that range is missing or changed — the file is held as a
//!      `revision`: every difference listed, its rows kept in a logged table, its decision
//!      written, nothing visible (D-009). [`accept_revision`] then supersedes the replaced
//!      lines (each with its decision) and publishes the file; [`reject_revision`] discards
//!      it.
//!
//! Nothing is visible before the flip commits, and the schema refuses lines committed
//! without it, so a crash at any point leaves the import complete, held or invisible. A
//! `staging` import whose lease expires is discarded by [`sweep`].

use std::future::Future;
use std::time::Duration;

use recon_domain::decision::{ImportEvidence, RevisionEvidence, SubjectKind};
use recon_domain::document::NewDocument;
use recon_domain::fingerprint::FINGERPRINT_VERSION;
use recon_domain::import::{
    DiscardReason, FileHash, ImportKind, ImportStatus, NewLine, PROFILE_R0, PROFILE_R0_VERSION,
    StagedRow,
};
use recon_domain::line::ActorKind;
use recon_domain::refusal::{RefusalCode, RowRefusal};
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
/// Bytes per staged batch ([`StagedRow::heap_bytes`]): wide rows flush before 5,000 (D-050).
pub const STAGING_BATCH_BYTES: usize = 4 * 1024 * 1024;
/// The document fingerprint's version, recorded with document imports (D-064).
pub const DOCUMENT_FINGERPRINT_VERSION: &str = "document-v1";
/// Most revision changes one page lists.
pub const MAX_CHANGES_PAGE: i64 = 1_000;

/// Who is acting: a kind and a reference (never a secret).
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    /// The kind.
    pub kind: ActorKind,
    /// The reference: `operator`, `sweeper`, later a user id.
    pub reference: &'a str,
}

/// The profile an import is read with: recorded on the import and its decisions (D-035).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileId {
    /// The name.
    pub name: String,
    /// The version.
    pub version: i32,
    /// SHA-256 of its canonical JSON.
    pub sha256: [u8; 32],
}

impl ProfileId {
    /// The built-in `r0-generic-csv`.
    pub fn r0() -> Self {
        Self {
            name: PROFILE_R0.to_owned(),
            version: PROFILE_R0_VERSION,
            sha256: recon_import::CompiledStatement::r0().sha256(),
        }
    }
}

/// What an import brings in, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A statement into this account.
    Statement {
        /// The account.
        account: AccountId,
        /// Its currency.
        currency: Currency,
    },
    /// Documents of the import's client.
    Documents,
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
    /// Statement or documents.
    pub target: Target,
    /// The profile.
    pub profile: ProfileId,
    lease_token: Uuid,
    staged_rows: u32,
}

impl StagingImport {
    /// Rows staged so far.
    pub const fn staged_rows(&self) -> u32 {
        self.staged_rows
    }

    /// The account, for a statement.
    pub const fn account(&self) -> Option<AccountId> {
        match self.target {
            Target::Statement { account, .. } => Some(account),
            Target::Documents => None,
        }
    }

    const fn kind(&self) -> ImportKind {
        match self.target {
            Target::Statement { .. } => ImportKind::Statement,
            Target::Documents => ImportKind::Documents,
        }
    }
}

/// The dates an import shares with earlier imports of its account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overlap {
    /// The first shared date.
    pub first_date: String,
    /// The last shared date.
    pub last_date: String,
    /// Rows of this file inside the shared dates.
    pub rows: i32,
}

/// An import as stored (the API's view of it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportRecord {
    /// The import.
    pub id: Uuid,
    /// `statement` or `documents`.
    pub kind: String,
    /// The client.
    pub client_id: Uuid,
    /// The account (statements).
    pub account_id: Option<Uuid>,
    /// `staging`, `complete`, `refused`, `discarded` or `revision`.
    pub status: String,
    /// The profile's name.
    pub profile: String,
    /// The profile's version.
    pub profile_version: i32,
    /// SHA-256 of the profile's canonical JSON, hex (R1 imports).
    pub profile_sha256: Option<String>,
    /// SHA-256 of the file, hex, once read.
    pub file_hash: Option<String>,
    /// Bytes read.
    pub file_bytes: Option<i64>,
    /// Data rows read.
    pub rows_read: Option<i32>,
    /// New lines (or documents).
    pub lines_created: Option<i32>,
    /// Rows already present as lines (or documents).
    pub duplicates: Option<i32>,
    /// Refused rows.
    pub rows_refused: Option<i32>,
    /// Why the file was refused.
    pub refusal_code: Option<String>,
    /// The first refused rows (codes and row numbers only).
    pub refusals: Vec<RowRefusal>,
    /// The question to answer before the file can be imported (`ambiguous_date_format`).
    pub question: Option<serde_json::Value>,
    /// The earlier import of the same file, when this one repeated it.
    pub duplicate_of: Option<Uuid>,
    /// Why it was discarded.
    pub discard_reason: Option<String>,
    /// The first and last booking (or issue) date of the file.
    pub first_date: Option<String>,
    /// The last date.
    pub last_date: Option<String>,
    /// The dates shared with earlier imports of the account.
    pub overlap: Option<Overlap>,
    /// How many differences a revision lists.
    pub changes: Option<i32>,
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
    /// Lines and duplicates are inserted (or a revision listed); nothing is committed.
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
    /// The lines (or documents) are visible.
    Complete(ImportRecord),
    /// The same file is already complete (or held) for the account: this import was
    /// discarded and the earlier one answers.
    DuplicateFile(ImportRecord),
    /// The statement disagrees with stored lines: held as a revision, nothing visible.
    Revision(ImportRecord),
    /// Documents whose numbers conflict with each other or with stored documents: the
    /// file is refused whole, with the rows.
    Refused(ImportRecord),
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

fn iso(d: time::Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

fn count_i32(n: u32) -> Result<i32, DbError> {
    i32::try_from(n).map_err(|_| DbError::Inconsistent("count out of range".into()))
}

fn count_u32(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// Starts a `staging` statement import into `account` with the built-in profile.
pub async fn begin(
    pool: &PgPool,
    account: &Account,
    lease: Duration,
) -> Result<StagingImport, DbError> {
    begin_with(pool, account, &ProfileId::r0(), lease).await
}

/// Starts a `staging` statement import into `account`, read with `profile`.
pub async fn begin_with(
    pool: &PgPool,
    account: &Account,
    profile: &ProfileId,
    lease: Duration,
) -> Result<StagingImport, DbError> {
    let target = Target::Statement {
        account: account.id,
        currency: account.currency,
    };
    insert_import(pool, account.firm, account.client, target, profile, lease).await
}

/// Starts a `staging` document import for `firm`'s `client`, read with `profile`.
/// [`DbError::NotFound`] when the client is not the firm's.
pub async fn begin_documents(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    profile: &ProfileId,
    lease: Duration,
) -> Result<StagingImport, DbError> {
    match insert_import(pool, firm, client, Target::Documents, profile, lease).await {
        Err(DbError::Constraint(_)) => Err(DbError::NotFound),
        other => other,
    }
}

async fn insert_import(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    target: Target,
    profile: &ProfileId,
    lease: Duration,
) -> Result<StagingImport, DbError> {
    let id = ImportId::from_uuid(Uuid::now_v7());
    let token = Uuid::now_v7();
    let (kind, account, currency) = match target {
        Target::Statement { account, currency } => {
            ("statement", Some(account.as_uuid()), Some(currency.code()))
        }
        Target::Documents => ("documents", None, None),
    };
    sqlx::query(
        "INSERT INTO recon.imports
             (firm_id, id, client_id, account_id, currency, kind, profile, profile_version,
              profile_sha256, lease_token, lease_until)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                 now() + ($11::bigint * interval '1 millisecond'))",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .bind(client.as_uuid())
    .bind(account)
    .bind(currency)
    .bind(kind)
    .bind(&profile.name)
    .bind(profile.version)
    .bind(profile.sha256.as_slice())
    .bind(token)
    .bind(millis(lease))
    .execute(pool)
    .await?;
    Ok(StagingImport {
        firm,
        id,
        client,
        target,
        profile: profile.clone(),
        lease_token: token,
        staged_rows: 0,
    })
}

/// One statement batch as column arrays for `unnest`.
struct LineColumns<'a> {
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

impl<'a> LineColumns<'a> {
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

/// One document batch as column arrays.
struct DocumentColumns<'a> {
    row_no: Vec<i32>,
    kind: Vec<&'static str>,
    number_raw: Vec<Option<&'a str>>,
    number_norm: Vec<Option<&'a str>>,
    issue: Vec<time::Date>,
    due: Vec<Option<time::Date>>,
    amount: Vec<i64>,
    currency: Vec<&'static str>,
    counterparty_raw: Vec<Option<&'a str>>,
    counterparty_norm: Vec<Option<&'a str>>,
    description: Vec<Option<&'a str>>,
    tuple: Vec<&'a [u8]>,
}

impl<'a> DocumentColumns<'a> {
    fn from_rows(rows: &'a [NewDocument]) -> Result<Self, DbError> {
        let n = rows.len();
        let mut c = Self {
            row_no: Vec::with_capacity(n),
            kind: Vec::with_capacity(n),
            number_raw: Vec::with_capacity(n),
            number_norm: Vec::with_capacity(n),
            issue: Vec::with_capacity(n),
            due: Vec::with_capacity(n),
            amount: Vec::with_capacity(n),
            currency: Vec::with_capacity(n),
            counterparty_raw: Vec::with_capacity(n),
            counterparty_norm: Vec::with_capacity(n),
            description: Vec::with_capacity(n),
            tuple: Vec::with_capacity(n),
        };
        for d in rows {
            c.row_no.push(count_i32(d.row)?);
            c.kind.push(d.kind.code());
            c.number_raw.push(d.number_raw.as_deref());
            c.number_norm.push(d.number_norm.as_deref());
            c.issue.push(date(d.issue_date)?);
            c.due.push(d.due_date.map(date).transpose()?);
            c.amount.push(d.amount.minor());
            c.currency.push(d.amount.currency().code());
            c.counterparty_raw.push(d.counterparty_raw.as_deref());
            c.counterparty_norm.push(d.counterparty_norm.as_deref());
            c.description.push(d.description.as_deref());
            c.tuple.push(d.tuple_hash.0.as_slice());
        }
        Ok(c)
    }
}

/// Rows that can be staged for a flip: statement lines and documents.
pub trait StageRows: StagedRow + Sized + Send + Sync {
    /// Stages one batch (at most [`STAGING_BATCH_ROWS`] rows) and renews the lease.
    fn stage<'a>(
        pool: &'a PgPool,
        import: &'a mut StagingImport,
        rows: &'a [Self],
        lease: Duration,
    ) -> impl Future<Output = Result<(), DbError>> + Send + 'a;
}

impl StageRows for NewLine {
    fn stage<'a>(
        pool: &'a PgPool,
        import: &'a mut StagingImport,
        rows: &'a [Self],
        lease: Duration,
    ) -> impl Future<Output = Result<(), DbError>> + Send + 'a {
        stage(pool, import, rows, lease)
    }
}

impl StageRows for NewDocument {
    fn stage<'a>(
        pool: &'a PgPool,
        import: &'a mut StagingImport,
        rows: &'a [Self],
        lease: Duration,
    ) -> impl Future<Output = Result<(), DbError>> + Send + 'a {
        stage_documents(pool, import, rows, lease)
    }
}

/// Locks the live staging import first (the sweeper skips a locked import, so a slow batch
/// cannot be reaped mid-way), then runs `insert`, then extends the lease from the clock
/// after the insert (a batch can outlast a lease on a loaded machine, D-044).
async fn stage_batch(
    pool: &PgPool,
    import: &mut StagingImport,
    batch: u32,
    lease: Duration,
    insert: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<(), DbError> {
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
    let inserted = insert.execute(&mut *tx).await?.rows_affected();
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

fn batch_len(n: usize) -> Result<u32, DbError> {
    u32::try_from(n).map_err(|_| DbError::Inconsistent("batch too large".into()))
}

/// Stages one statement batch (at most [`STAGING_BATCH_ROWS`] rows) and renews the lease,
/// in one transaction. Refused with [`DbError::LeaseLost`] when the lease is gone.
pub async fn stage(
    pool: &PgPool,
    import: &mut StagingImport,
    rows: &[NewLine],
    lease: Duration,
) -> Result<(), DbError> {
    if rows.is_empty() {
        return Ok(());
    }
    let Target::Statement { currency, .. } = import.target else {
        return Err(DbError::Inconsistent(
            "statement rows for a document import".into(),
        ));
    };
    let batch = batch_len(rows.len())?;
    let c = LineColumns::from_rows(rows, currency)?;
    let insert = sqlx::query(
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
    .bind(c.row_no)
    .bind(c.booking)
    .bind(c.value)
    .bind(c.amount)
    .bind(c.narration_raw)
    .bind(c.narration_norm)
    .bind(c.reference)
    .bind(c.counterparty_raw)
    .bind(c.counterparty_norm)
    .bind(c.balance)
    .bind(c.tuple);
    stage_batch(pool, import, batch, lease, insert).await
}

/// Stages one document batch and renews the lease.
pub async fn stage_documents(
    pool: &PgPool,
    import: &mut StagingImport,
    rows: &[NewDocument],
    lease: Duration,
) -> Result<(), DbError> {
    if rows.is_empty() {
        return Ok(());
    }
    if import.target != Target::Documents {
        return Err(DbError::Inconsistent(
            "document rows for a statement import".into(),
        ));
    }
    let batch = batch_len(rows.len())?;
    let c = DocumentColumns::from_rows(rows)?;
    let insert = sqlx::query(
        "INSERT INTO recon.document_staging
             (firm_id, import_id, row_no, kind, number_raw, number_norm, issue_date, due_date,
              amount_minor, currency, counterparty_raw, counterparty_norm, description, tuple_hash)
         SELECT $1, $2, t.*
           FROM ROWS FROM (
                    pg_catalog.unnest($3::int4[]), pg_catalog.unnest($4::text[]),
                    pg_catalog.unnest($5::text[]), pg_catalog.unnest($6::text[]),
                    pg_catalog.unnest($7::date[]), pg_catalog.unnest($8::date[]),
                    pg_catalog.unnest($9::int8[]), pg_catalog.unnest($10::text[]),
                    pg_catalog.unnest($11::text[]), pg_catalog.unnest($12::text[]),
                    pg_catalog.unnest($13::text[]), pg_catalog.unnest($14::bytea[])) AS t",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(c.row_no)
    .bind(c.kind)
    .bind(c.number_raw)
    .bind(c.number_norm)
    .bind(c.issue)
    .bind(c.due)
    .bind(c.amount)
    .bind(c.currency)
    .bind(c.counterparty_raw)
    .bind(c.counterparty_norm)
    .bind(c.description)
    .bind(c.tuple);
    stage_batch(pool, import, batch, lease, insert).await
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

const fn staging_table(kind: ImportKind) -> &'static str {
    match kind {
        ImportKind::Statement => "recon.import_staging",
        ImportKind::Documents => "recon.document_staging",
    }
}

async fn delete_staging(conn: &mut PgConnection, import: &StagingImport) -> Result<u64, DbError> {
    let sql = format!(
        "DELETE FROM {} WHERE firm_id = $1 AND import_id = $2",
        staging_table(import.kind())
    );
    Ok(sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(import.firm.as_uuid())
        .bind(import.id.as_uuid())
        .execute(&mut *conn)
        .await?
        .rows_affected())
}

fn json(value: &impl Serialize) -> Result<Vec<u8>, DbError> {
    serde_json::to_vec(value).map_err(|e| DbError::Inconsistent(format!("evidence: {e}")))
}

/// A flip's transaction: every statement bounded by `flip_timeout`, no nested loops (D-043:
/// the staging table is never analyzed, the runtime role cannot, and a fresh account's
/// lines look empty to the planner, which then picks nested loops that walk every index
/// entry the statement inserts — quadratic; the joins here are all bulk joins, and the
/// ON CONFLICT and foreign-key probes use their indexes regardless).
async fn flip_transaction(
    pool: &PgPool,
    flip_timeout: Duration,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = '{}ms'",
        millis(flip_timeout)
    )))
    .execute(&mut *tx)
    .await?;
    sqlx::query("SET LOCAL enable_nestloop = off")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Serializes flips into one account (or one client's documents), refusing at once when
/// another holds it (D-053).
async fn try_lock(conn: &mut PgConnection, key: Uuid, space: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT pg_catalog.pg_try_advisory_xact_lock(pg_catalog.hashtextextended($1::text, $2))",
    )
    .bind(key)
    .bind(space)
    .fetch_one(&mut *conn)
    .await?)
}

const ACCOUNT_LOCK: i64 = 7262;
const CLIENT_DOCUMENTS_LOCK: i64 = 7263;

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
    match import.target {
        Target::Statement { account, currency } => {
            let dest = Dest {
                firm: import.firm,
                import: import.id,
                client: import.client,
                account,
                currency,
            };
            flip_statement(pool, import, dest, summary, actor, hook, flip_timeout).await
        }
        Target::Documents => flip_documents(pool, import, summary, actor, hook, flip_timeout).await,
    }
}

/// Where a statement's rows are published.
#[derive(Debug, Clone, Copy)]
struct Dest {
    firm: FirmId,
    import: ImportId,
    client: ClientId,
    account: AccountId,
    currency: Currency,
}

/// Where a statement's rows are read from at the flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rows {
    /// The staging table (a new upload).
    Staged,
    /// A held revision's rows (being accepted).
    Held,
}

impl Rows {
    const fn table(self) -> &'static str {
        match self {
            Self::Staged => "recon.import_staging",
            Self::Held => "recon.import_held_rows",
        }
    }
}

/// Checks the lease and that every staged row is present; answers a repeated file.
async fn check_staged(
    tx: &mut PgConnection,
    import: &StagingImport,
    rows_read: i32,
) -> Result<(), DbError> {
    let staged = lock_staging(tx, import).await?;
    let sql = format!(
        "SELECT count(*) FROM {} WHERE firm_id = $1 AND import_id = $2",
        staging_table(import.kind())
    );
    let present: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(import.firm.as_uuid())
        .bind(import.id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
    if staged != rows_read || present != i64::from(rows_read) {
        return Err(DbError::StagingIncomplete);
    }
    Ok(())
}

async fn flip_statement<H: FlipHook>(
    pool: &PgPool,
    import: StagingImport,
    dest: Dest,
    summary: &ParseSummary,
    actor: Actor<'_>,
    hook: &H,
    flip_timeout: Duration,
) -> Result<FlipOutcome, DbError> {
    let rows_read = count_i32(summary.rows_read)?;
    let mut tx = flip_transaction(pool, flip_timeout).await?;
    if !try_lock(&mut tx, dest.account.as_uuid(), ACCOUNT_LOCK).await? {
        drop(tx);
        discard_for(pool, import, DiscardReason::AccountBusy, actor).await?;
        return Err(DbError::AccountBusy);
    }
    check_staged(&mut tx, &import, rows_read).await?;
    // The same file again answers with the earlier import, complete or held.
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM recon.imports
          WHERE firm_id = $1 AND account_id = $2 AND file_hash = $3
            AND status IN ('complete', 'revision')",
    )
    .bind(dest.firm.as_uuid())
    .bind(dest.account.as_uuid())
    .bind(summary.file_hash.0.as_slice())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        discard_as_duplicate(&mut tx, &import, existing, summary, actor).await?;
        tx.commit().await?;
        let earlier = get(pool, import.firm, ImportId::from_uuid(existing)).await?;
        return Ok(FlipOutcome::DuplicateFile(earlier));
    }
    let plan = plan(&mut tx, dest, Rows::Staged).await?;
    if plan.removed > 0 {
        hold(&mut tx, &import, dest, summary, &plan, actor).await?;
        hook.reached(FlipPoint::LinesInserted).await;
        hook.reached(FlipPoint::BeforeCommit).await;
        tx.commit().await?;
        return Ok(FlipOutcome::Revision(
            get(pool, import.firm, import.id).await?,
        ));
    }
    let published = insert_lines(&mut tx, dest, Rows::Staged, rows_read).await?;
    hook.reached(FlipPoint::LinesInserted).await;
    delete_staging(&mut tx, &import).await?;
    let completion = Completion {
        summary_rows: summary.rows_read,
        file_hash: summary.file_hash,
        file_bytes: summary.file_bytes,
        published: &published,
        plan: Some(&plan),
        superseded: None,
        revision_digest: None,
    };
    record_completion(
        &mut tx,
        &import_ref(&import),
        &completion,
        ImportStatus::Staging,
        "import_completed",
        actor,
    )
    .await?;
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

/// The fingerprinted rows of a statement being flipped (a CTE prefix: `staged`, `fp`, `span`,
/// `covered`). `$1` firm, `$2` import, `$3` account.
fn fingerprinted(rows: Rows) -> String {
    format!(
        "WITH staged AS (
             SELECT s.row_no, s.booking_date, s.value_date, s.amount_minor, s.narration_raw,
                    s.narration_norm, s.reference, s.counterparty_raw, s.counterparty_norm,
                    s.balance_after_minor, s.tuple_hash,
                    (pg_catalog.row_number() OVER (PARTITION BY s.tuple_hash ORDER BY s.row_no) - 1)::int4
                        AS occurrence
               FROM {table} s
              WHERE s.firm_id = $1 AND s.import_id = $2
         ),
         fp AS (
             SELECT st.*,
                    pg_catalog.sha256(pg_catalog.convert_to('recon-fp-v1', 'UTF8')
                        || st.tuple_hash || pg_catalog.int4send(st.occurrence)) AS fingerprint
               FROM staged st
         ),
         span AS (SELECT min(fp.booking_date) AS lo, max(fp.booking_date) AS hi FROM fp),
         -- The dates of this file that complete imports of the account already cover.
         covered AS (
             SELECT GREATEST(i.first_date, span.lo) AS lo, LEAST(i.last_date, span.hi) AS hi
               FROM recon.imports i CROSS JOIN span
              WHERE i.firm_id = $1 AND i.account_id = $3 AND i.kind = 'statement'
                AND i.status = 'complete' AND i.id <> $2
                AND i.first_date <= span.hi AND i.last_date >= span.lo
         )",
        table = rows.table()
    )
}

/// What comparing a file with the stored lines of its dates found.
#[derive(Debug, Clone, Copy)]
struct Plan {
    lo: Option<time::Date>,
    hi: Option<time::Date>,
    /// Stored live lines in the file's dates that the file does not have.
    removed: i64,
    overlap_rows: i64,
    overlap_first: Option<time::Date>,
    overlap_last: Option<time::Date>,
}

async fn plan(tx: &mut PgConnection, dest: Dest, rows: Rows) -> Result<Plan, DbError> {
    let sql = format!(
        "{prefix},
         overlap AS (
             SELECT fp.booking_date FROM fp
              WHERE EXISTS (SELECT 1 FROM covered c WHERE fp.booking_date BETWEEN c.lo AND c.hi)
         ),
         removed AS (
             SELECT l.id FROM recon.statement_lines l CROSS JOIN span
              WHERE l.firm_id = $1 AND l.account_id = $3 AND l.status <> 'superseded'
                AND l.booking_date BETWEEN span.lo AND span.hi
                AND NOT EXISTS (SELECT 1 FROM fp WHERE fp.fingerprint = l.fingerprint)
         )
         SELECT (SELECT lo FROM span) AS lo, (SELECT hi FROM span) AS hi,
                (SELECT count(*) FROM removed) AS removed,
                (SELECT count(*) FROM overlap) AS overlap_rows,
                (SELECT min(booking_date) FROM overlap) AS overlap_first,
                (SELECT max(booking_date) FROM overlap) AS overlap_last",
        prefix = fingerprinted(rows)
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(dest.firm.as_uuid())
        .bind(dest.import.as_uuid())
        .bind(dest.account.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
    Ok(Plan {
        lo: row.try_get("lo")?,
        hi: row.try_get("hi")?,
        removed: row.try_get("removed")?,
        overlap_rows: row.try_get("overlap_rows")?,
        overlap_first: row.try_get("overlap_first")?,
        overlap_last: row.try_get("overlap_last")?,
    })
}

/// The differences between a file and the stored lines of the dates it shares with them
/// (D-065), numbered in a fixed order (date, row, line fingerprint), with a digest over the
/// list. A stored line and a new row are one change when they are the only removed line and
/// the only added row with the same booking date and reference (or narration, when neither
/// has a reference). `insert` writes the list for the import.
fn changes_sql(rows: Rows, insert: bool) -> String {
    let write = if insert {
        "ins AS (
             INSERT INTO recon.import_revision_changes
                 (firm_id, import_id, ordinal, change, row_no, line_id, fields)
             SELECT $1, $2, n.ordinal, n.change, n.row_no, n.line_id, n.fields FROM numbered n
             RETURNING 1
         ),"
    } else {
        ""
    };
    let inserted = if insert {
        "(SELECT count(*) FROM ins)"
    } else {
        "0::int8"
    };
    format!(
        "{prefix},
         live AS (
             SELECT l.id, l.row_no, l.booking_date, l.amount_minor, l.narration_norm, l.reference,
                    l.value_date, l.counterparty_norm, l.balance_after_minor, l.fingerprint
               FROM recon.statement_lines l CROSS JOIN span
              WHERE l.firm_id = $1 AND l.account_id = $3 AND l.status <> 'superseded'
                AND l.booking_date BETWEEN span.lo AND span.hi
         ),
         removed AS (
             SELECT lv.*, CASE WHEN lv.reference IS NOT NULL THEN 'R' || lv.reference
                               ELSE 'N' || lv.narration_norm END AS k
               FROM live lv
              WHERE NOT EXISTS (SELECT 1 FROM fp WHERE fp.fingerprint = lv.fingerprint)
         ),
         added AS (
             SELECT fp.*, CASE WHEN fp.reference IS NOT NULL THEN 'R' || fp.reference
                               ELSE 'N' || fp.narration_norm END AS k
               FROM fp
              WHERE EXISTS (SELECT 1 FROM covered c WHERE fp.booking_date BETWEEN c.lo AND c.hi)
                AND NOT EXISTS (
                    SELECT 1 FROM recon.statement_lines l
                     WHERE l.firm_id = $1 AND l.account_id = $3 AND l.status <> 'superseded'
                       AND l.fingerprint = fp.fingerprint)
         ),
         rk AS (SELECT r.*, count(*) OVER (PARTITION BY r.booking_date, r.k) AS n FROM removed r),
         ak AS (SELECT a.*, count(*) OVER (PARTITION BY a.booking_date, a.k) AS n FROM added a),
         pairs AS (
             SELECT rk.id AS line_id, ak.row_no, rk.booking_date, rk.fingerprint AS line_fp,
                    pg_catalog.array_remove(ARRAY[
                        CASE WHEN rk.amount_minor <> ak.amount_minor THEN 'amount' END,
                        CASE WHEN rk.narration_norm <> ak.narration_norm THEN 'narration' END,
                        CASE WHEN rk.reference IS DISTINCT FROM ak.reference THEN 'reference' END,
                        CASE WHEN rk.value_date IS DISTINCT FROM ak.value_date THEN 'value_date' END,
                        CASE WHEN rk.counterparty_norm IS DISTINCT FROM ak.counterparty_norm
                             THEN 'counterparty' END,
                        CASE WHEN rk.balance_after_minor IS DISTINCT FROM ak.balance_after_minor
                             THEN 'balance_after' END
                    ]::text[], NULL) AS fields
               FROM rk JOIN ak ON ak.booking_date = rk.booking_date AND ak.k = rk.k
              WHERE rk.n = 1 AND ak.n = 1
         ),
         changes AS (
             SELECT 'changed'::text AS change, p.row_no, p.line_id, p.fields, p.booking_date,
                    p.line_fp
               FROM pairs p
             UNION ALL
             SELECT 'removed', NULL::int4, rk.id, NULL::text[], rk.booking_date, rk.fingerprint
               FROM rk WHERE NOT EXISTS (SELECT 1 FROM pairs p WHERE p.line_id = rk.id)
             UNION ALL
             SELECT 'added', ak.row_no, NULL::uuid, NULL::text[], ak.booking_date, NULL::bytea
               FROM ak WHERE NOT EXISTS (SELECT 1 FROM pairs p WHERE p.row_no = ak.row_no)
         ),
         numbered AS (
             SELECT c.*, pg_catalog.row_number() OVER (
                        ORDER BY c.booking_date, coalesce(c.row_no, 0),
                                 coalesce(c.line_fp, ''::bytea), c.change)::int4 AS ordinal
               FROM changes c
         ),
         {write}
         summary AS (
             SELECT count(*) FILTER (WHERE change = 'added') AS added,
                    count(*) FILTER (WHERE change = 'removed') AS removed,
                    count(*) FILTER (WHERE change = 'changed') AS changed,
                    pg_catalog.sha256(coalesce(pg_catalog.string_agg(pg_catalog.convert_to(
                        pg_catalog.concat_ws('|', n.ordinal, n.change, coalesce(n.row_no::text, ''),
                            coalesce(pg_catalog.encode(n.line_fp, 'hex'), ''),
                            coalesce(pg_catalog.array_to_string(n.fields, ','), '')) || E'\\n',
                        'UTF8'), ''::bytea ORDER BY n.ordinal), ''::bytea)) AS digest
               FROM numbered n
         )
         SELECT s.added, s.removed, s.changed, s.digest, {inserted} AS inserted FROM summary s",
        prefix = fingerprinted(rows)
    )
}

/// Counts and digest of a revision's changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    added: i64,
    removed: i64,
    changed: i64,
    digest: Vec<u8>,
}

impl Listed {
    fn total(&self) -> i64 {
        self.added
            .saturating_add(self.removed)
            .saturating_add(self.changed)
    }
}

async fn list_changes(
    tx: &mut PgConnection,
    dest: Dest,
    rows: Rows,
    insert: bool,
) -> Result<Listed, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(changes_sql(rows, insert)))
        .bind(dest.firm.as_uuid())
        .bind(dest.import.as_uuid())
        .bind(dest.account.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
    let listed = Listed {
        added: row.try_get("added")?,
        removed: row.try_get("removed")?,
        changed: row.try_get("changed")?,
        digest: row.try_get("digest")?,
    };
    let inserted: i64 = row.try_get("inserted")?;
    if insert && inserted != listed.total() {
        return Err(DbError::Inconsistent(format!(
            "listed {inserted} of {} revision changes",
            listed.total()
        )));
    }
    Ok(listed)
}

/// Holds a staged statement as a revision: the changes listed, the rows moved to the logged
/// held table, its decision, nothing visible (D-009, D-065).
async fn hold(
    tx: &mut PgConnection,
    import: &StagingImport,
    dest: Dest,
    summary: &ParseSummary,
    plan: &Plan,
    actor: Actor<'_>,
) -> Result<(), DbError> {
    let listed = list_changes(tx, dest, Rows::Staged, true).await?;
    let moved = sqlx::query(
        "INSERT INTO recon.import_held_rows
             (firm_id, import_id, row_no, booking_date, value_date, amount_minor, narration_raw,
              narration_norm, reference, counterparty_raw, counterparty_norm,
              balance_after_minor, tuple_hash)
         SELECT firm_id, import_id, row_no, booking_date, value_date, amount_minor, narration_raw,
                narration_norm, reference, counterparty_raw, counterparty_norm,
                balance_after_minor, tuple_hash
           FROM recon.import_staging WHERE firm_id = $1 AND import_id = $2",
    )
    .bind(dest.firm.as_uuid())
    .bind(dest.import.as_uuid())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved != u64::from(summary.rows_read) {
        return Err(DbError::StagingIncomplete);
    }
    delete_staging(tx, import).await?;
    let digest_hex = hex::encode(&listed.digest);
    let evidence = json(&RevisionEvidence {
        import: dest.import.as_uuid(),
        account: dest.account.as_uuid(),
        file_hash: summary.file_hash.to_string(),
        profile: import.profile.name.clone(),
        profile_version: import.profile.version,
        profile_sha256: hex::encode(import.profile.sha256),
        rows_read: summary.rows_read,
        added: count_u32(listed.added),
        removed: count_u32(listed.removed),
        changed: count_u32(listed.changed),
        revision_digest: digest_hex,
    })?;
    decisions::insert(
        tx,
        import.firm,
        &NewDecision {
            client: import.client,
            subject_kind: SubjectKind::Import,
            subject_id: import.id.as_uuid(),
            from_state: Some(ImportStatus::Staging.code()),
            to_state: ImportStatus::Revision.code(),
            actor_kind: actor.kind,
            actor_ref: actor.reference,
            reason_code: "revision_detected",
            evidence: &evidence,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE recon.imports
            SET status = 'revision', file_hash = $3, file_bytes = $4, rows_read = $5,
                changes = $6, revision_digest = $7, first_date = $8, last_date = $9,
                overlap_first = $10, overlap_last = $11, overlap_rows = $12
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(summary.file_hash.0.as_slice())
    .bind(i64::try_from(summary.file_bytes).unwrap_or(i64::MAX))
    .bind(count_i32(summary.rows_read)?)
    .bind(i32::try_from(listed.total()).unwrap_or(i32::MAX))
    .bind(&listed.digest)
    .bind(plan.lo)
    .bind(plan.hi)
    .bind(plan.overlap_first)
    .bind(plan.overlap_last)
    .bind(i32::try_from(plan.overlap_rows).unwrap_or(i32::MAX))
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The counts and digest of the rows a flip published.
struct Published {
    lines_created: i32,
    duplicates: i32,
    digest: Vec<u8>,
    first: Option<time::Date>,
    last: Option<time::Date>,
}

/// Inserts a statement's rows as lines (occurrence indexes and fingerprints computed here,
/// D-008) and records rows that live lines of the account already have as duplicates.
async fn insert_lines(
    tx: &mut PgConnection,
    dest: Dest,
    rows: Rows,
    rows_read: i32,
) -> Result<Published, DbError> {
    let sql = format!(
        "{prefix},
         inserted AS (
             INSERT INTO recon.statement_lines
                 (firm_id, client_id, account_id, import_id, currency, row_no, booking_date,
                  value_date, amount_minor, narration_raw, narration_norm, reference,
                  counterparty_raw, counterparty_norm, balance_after_minor, tuple_hash,
                  occurrence, fingerprint)
             SELECT $1, $4, $3, $2, $5, fp.row_no, fp.booking_date, fp.value_date,
                    fp.amount_minor, fp.narration_raw, fp.narration_norm, fp.reference,
                    fp.counterparty_raw, fp.counterparty_norm, fp.balance_after_minor,
                    fp.tuple_hash, fp.occurrence, fp.fingerprint
               FROM fp ORDER BY fp.row_no
             ON CONFLICT (firm_id, account_id, fingerprint) WHERE status <> 'superseded' DO NOTHING
             RETURNING row_no
         ),
         -- Every CTE reads the snapshot from before this statement, so this join sees only
         -- lines that existed before (never this import's): exactly the rows ON CONFLICT
         -- skipped. Nested loops are off (see flip_transaction): one pass over the account.
         dups AS (
             INSERT INTO recon.import_duplicates (firm_id, import_id, row_no, existing_line_id)
             SELECT $1, $2, fp.row_no, l.id
               FROM fp
               JOIN recon.statement_lines l
                 ON l.firm_id = $1 AND l.account_id = $3 AND l.fingerprint = fp.fingerprint
                AND l.status <> 'superseded'
             RETURNING row_no
         )
         SELECT (SELECT count(*) FROM inserted) AS lines_created,
                (SELECT count(*) FROM dups) AS duplicates,
                (SELECT count(*) FROM fp) AS total,
                (SELECT lo FROM span) AS first, (SELECT hi FROM span) AS last,
                pg_catalog.sha256(coalesce(
                    (SELECT pg_catalog.string_agg(fp.fingerprint, ''::bytea ORDER BY fp.row_no) FROM fp),
                    ''::bytea)) AS digest",
        prefix = fingerprinted(rows)
    );
    let counts = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(dest.firm.as_uuid())
        .bind(dest.import.as_uuid())
        .bind(dest.account.as_uuid())
        .bind(dest.client.as_uuid())
        .bind(dest.currency.code())
        .fetch_one(&mut *tx)
        .await?;
    published(&counts, rows_read)
}

fn published(counts: &PgRow, rows_read: i32) -> Result<Published, DbError> {
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
        first: counts.try_get("first")?,
        last: counts.try_get("last")?,
    })
}

/// The import a completion is for.
struct ImportRef<'a> {
    firm: FirmId,
    id: ImportId,
    client: ClientId,
    account: Option<AccountId>,
    kind: ImportKind,
    profile: &'a ProfileId,
}

fn import_ref(import: &StagingImport) -> ImportRef<'_> {
    ImportRef {
        firm: import.firm,
        id: import.id,
        client: import.client,
        account: import.account(),
        kind: import.kind(),
        profile: &import.profile,
    }
}

struct Completion<'a> {
    summary_rows: u32,
    file_hash: FileHash,
    file_bytes: u64,
    published: &'a Published,
    plan: Option<&'a Plan>,
    superseded: Option<u32>,
    revision_digest: Option<String>,
}

/// Writes the completion decision and flips the import to `complete`.
async fn record_completion(
    tx: &mut PgConnection,
    import: &ImportRef<'_>,
    c: &Completion<'_>,
    from: ImportStatus,
    reason: &str,
    actor: Actor<'_>,
) -> Result<(), DbError> {
    let fingerprint_version = match import.kind {
        ImportKind::Statement => FINGERPRINT_VERSION,
        ImportKind::Documents => DOCUMENT_FINGERPRINT_VERSION,
    };
    let evidence = json(&ImportEvidence {
        import: import.id.as_uuid(),
        account: import.account.map(AccountId::as_uuid),
        file_hash: c.file_hash.to_string(),
        profile: import.profile.name.clone(),
        profile_version: import.profile.version,
        fingerprint_version: fingerprint_version.to_owned(),
        rows_read: c.summary_rows,
        lines_created: u32::try_from(c.published.lines_created).unwrap_or(0),
        duplicates: u32::try_from(c.published.duplicates).unwrap_or(0),
        lines_digest: hex::encode(&c.published.digest),
        kind: Some(import.kind.code().to_owned()),
        profile_sha256: Some(hex::encode(import.profile.sha256)),
        overlap_rows: c.plan.map(|p| count_u32(p.overlap_rows)),
        superseded: c.superseded,
        revision_digest: c.revision_digest.clone(),
    })?;
    decisions::insert(
        tx,
        import.firm,
        &NewDecision {
            client: import.client,
            subject_kind: SubjectKind::Import,
            subject_id: import.id.as_uuid(),
            from_state: Some(from.code()),
            to_state: ImportStatus::Complete.code(),
            actor_kind: actor.kind,
            actor_ref: actor.reference,
            reason_code: reason,
            evidence: &evidence,
        },
    )
    .await?;
    let overlap = c.plan.filter(|p| p.overlap_rows > 0);
    sqlx::query(
        "UPDATE recon.imports
            SET status = 'complete', file_hash = $3, file_bytes = $4, rows_read = $5,
                lines_created = $6, duplicates = $7, rows_refused = 0,
                first_date = $8, last_date = $9, overlap_first = $10, overlap_last = $11,
                overlap_rows = $12
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(c.file_hash.0.as_slice())
    .bind(i64::try_from(c.file_bytes).unwrap_or(i64::MAX))
    .bind(count_i32(c.summary_rows)?)
    .bind(c.published.lines_created)
    .bind(c.published.duplicates)
    .bind(c.published.first)
    .bind(c.published.last)
    .bind(overlap.and_then(|p| p.overlap_first))
    .bind(overlap.and_then(|p| p.overlap_last))
    .bind(
        c.plan
            .map(|p| i32::try_from(p.overlap_rows).unwrap_or(i32::MAX)),
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// A held revision, locked for a decision.
struct Held {
    client: ClientId,
    account: AccountId,
    currency: Currency,
    profile: ProfileId,
    file_hash: FileHash,
    file_bytes: u64,
    rows_read: i32,
    digest: Vec<u8>,
}

async fn lock_held(conn: &mut PgConnection, firm: FirmId, id: ImportId) -> Result<Held, DbError> {
    let row = sqlx::query(
        "SELECT client_id, account_id, currency, status, profile, profile_version,
                profile_sha256, file_hash, file_bytes, rows_read, revision_digest
           FROM recon.imports WHERE firm_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::NotFound)?;
    if row.try_get::<String, _>("status")? != ImportStatus::Revision.code() {
        return Err(DbError::NotARevision);
    }
    let hash32 = |column: &str| -> Result<[u8; 32], DbError> {
        let bytes: Option<Vec<u8>> = row.try_get(column)?;
        bytes
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| DbError::Inconsistent(format!("{column} is not 32 bytes")))
    };
    let currency: String = row.try_get("currency")?;
    Ok(Held {
        client: ClientId::from_uuid(row.try_get("client_id")?),
        account: AccountId::from_uuid(
            row.try_get::<Option<Uuid>, _>("account_id")?
                .ok_or_else(|| DbError::Inconsistent("a revision without an account".into()))?,
        ),
        currency: Currency::from_code(&currency)
            .map_err(|_| DbError::Inconsistent(format!("stored currency {currency:?}")))?,
        profile: ProfileId {
            name: row.try_get("profile")?,
            version: row.try_get("profile_version")?,
            sha256: hash32("profile_sha256")?,
        },
        file_hash: FileHash(hash32("file_hash")?),
        file_bytes: u64::try_from(row.try_get::<Option<i64>, _>("file_bytes")?.unwrap_or(0))
            .unwrap_or(0),
        rows_read: row.try_get::<Option<i32>, _>("rows_read")?.unwrap_or(0),
        digest: row
            .try_get::<Option<Vec<u8>>, _>("revision_digest")?
            .unwrap_or_default(),
    })
}

/// Supersedes every stored line a held revision lists (removed or changed), each with its
/// own decision in the list's order; refused when one is no longer `unreviewed`.
async fn supersede_listed(
    tx: &mut PgConnection,
    firm: FirmId,
    id: ImportId,
    client: ClientId,
    actor: Actor<'_>,
) -> Result<u64, DbError> {
    let replaced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.import_revision_changes
          WHERE firm_id = $1 AND import_id = $2 AND line_id IS NOT NULL",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    let superseded = sqlx::query(
        "UPDATE recon.statement_lines l
            SET status = 'superseded', row_version = l.row_version + 1
           FROM recon.import_revision_changes c
          WHERE c.firm_id = $1 AND c.import_id = $2 AND c.line_id IS NOT NULL
            AND l.firm_id = $1 AND l.id = c.line_id AND l.status = 'unreviewed'",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if i64::try_from(superseded).ok() != Some(replaced) {
        return Err(DbError::RevisionTouchesDecidedLine);
    }
    // One decision per superseded line, in the list's order (the chain trigger links each).
    let decided = sqlx::query(
        "INSERT INTO recon.decision_log
             (firm_id, client_id, id, subject_kind, subject_id, from_state, to_state,
              actor_kind, actor_ref, reason_code, evidence, engine_version)
         SELECT $1, $3, pg_catalog.uuidv7(), 'line', c.line_id, 'unreviewed', 'superseded',
                $4, $5, 'revision_accepted',
                pg_catalog.convert_to(pg_catalog.jsonb_build_object(
                    'import', $2::uuid, 'change', c.change, 'ordinal', c.ordinal)::text, 'UTF8'),
                $6
           FROM recon.import_revision_changes c
          WHERE c.firm_id = $1 AND c.import_id = $2 AND c.line_id IS NOT NULL
          ORDER BY c.ordinal",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .bind(client.as_uuid())
    .bind(actor.kind.code())
    .bind(actor.reference)
    .bind(decisions::engine_version())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if decided != superseded {
        return Err(DbError::Inconsistent(
            "superseded lines without their decisions".into(),
        ));
    }
    Ok(superseded)
}

/// Accepts a held revision (D-009, D-065): recomputes its differences against the lines as
/// they are now (refused with [`DbError::RevisionStale`] when they no longer match the list a
/// person reviewed), supersedes every stored line it removes or changes — each with its own
/// decision — publishes the file's rows as the flip would, and records the completion.
/// Refused with [`DbError::RevisionTouchesDecidedLine`] when a replaced line is no longer
/// `unreviewed` (R3 adds the `source_changed` path for those).
pub async fn accept_revision(
    pool: &PgPool,
    firm: FirmId,
    id: ImportId,
    actor: Actor<'_>,
    flip_timeout: Duration,
) -> Result<ImportRecord, DbError> {
    let mut tx = flip_transaction(pool, flip_timeout).await?;
    let held = lock_held(&mut tx, firm, id).await?;
    if !try_lock(&mut tx, held.account.as_uuid(), ACCOUNT_LOCK).await? {
        return Err(DbError::AccountBusy);
    }
    let dest = Dest {
        firm,
        import: id,
        client: held.client,
        account: held.account,
        currency: held.currency,
    };
    let now = list_changes(&mut tx, dest, Rows::Held, false).await?;
    if now.digest != held.digest {
        return Err(DbError::RevisionStale);
    }
    let plan = plan(&mut tx, dest, Rows::Held).await?;
    let superseded = supersede_listed(&mut tx, firm, id, held.client, actor).await?;
    let published = insert_lines(&mut tx, dest, Rows::Held, held.rows_read).await?;
    sqlx::query("DELETE FROM recon.import_held_rows WHERE firm_id = $1 AND import_id = $2")
        .bind(firm.as_uuid())
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    let import = ImportRef {
        firm,
        id,
        client: held.client,
        account: Some(held.account),
        kind: ImportKind::Statement,
        profile: &held.profile,
    };
    let completion = Completion {
        summary_rows: u32::try_from(held.rows_read).unwrap_or(0),
        file_hash: held.file_hash,
        file_bytes: held.file_bytes,
        published: &published,
        plan: Some(&plan),
        superseded: Some(u32::try_from(superseded).unwrap_or(u32::MAX)),
        revision_digest: Some(hex::encode(&held.digest)),
    };
    record_completion(
        &mut tx,
        &import,
        &completion,
        ImportStatus::Revision,
        "revision_accepted",
        actor,
    )
    .await?;
    tx.commit().await?;
    get(pool, firm, id).await
}

/// Rejects a held revision: its rows are deleted, the import discarded with its decision,
/// and the stored lines stay as they are.
pub async fn reject_revision(
    pool: &PgPool,
    firm: FirmId,
    id: ImportId,
    actor: Actor<'_>,
) -> Result<ImportRecord, DbError> {
    let mut tx = pool.begin().await?;
    let held = lock_held(&mut tx, firm, id).await?;
    sqlx::query("DELETE FROM recon.import_held_rows WHERE firm_id = $1 AND import_id = $2")
        .bind(firm.as_uuid())
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    let evidence = json(&serde_json::json!({
        "import": id.as_uuid(),
        "revision_digest": hex::encode(&held.digest),
    }))?;
    decisions::insert(
        &mut tx,
        firm,
        &NewDecision {
            client: held.client,
            subject_kind: SubjectKind::Import,
            subject_id: id.as_uuid(),
            from_state: Some(ImportStatus::Revision.code()),
            to_state: ImportStatus::Discarded.code(),
            actor_kind: actor.kind,
            actor_ref: actor.reference,
            reason_code: DiscardReason::RevisionRejected.code(),
            evidence: &evidence,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE recon.imports SET status = 'discarded', discard_reason = 'revision_rejected'
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    get(pool, firm, id).await
}

/// One difference a revision lists, with what a reviewer needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevisionChangeRecord {
    /// Its position in the list.
    pub ordinal: i32,
    /// `added`, `removed` or `changed`.
    pub change: String,
    /// The fields that differ (a change).
    pub fields: Option<Vec<String>>,
    /// The stored line (removed, changed).
    pub line: Option<ChangeSide>,
    /// The file's row (added, changed).
    pub row: Option<ChangeSide>,
}

/// One side of a difference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeSide {
    /// The stored line's id (the stored side only).
    pub id: Option<Uuid>,
    /// The row number in its file.
    pub row_no: i32,
    /// The booking date.
    pub booking_date: String,
    /// Signed minor units.
    pub amount_minor: i64,
    /// The narration as written.
    pub narration: String,
    /// The reference.
    pub reference: Option<String>,
}

fn side(row: &PgRow, prefix: &str, with_id: bool) -> Result<Option<ChangeSide>, DbError> {
    let row_no: Option<i32> = row.try_get(format!("{prefix}row_no").as_str())?;
    let Some(row_no) = row_no else {
        return Ok(None);
    };
    let booking: Option<time::Date> = row.try_get(format!("{prefix}booking_date").as_str())?;
    Ok(Some(ChangeSide {
        id: if with_id {
            row.try_get(format!("{prefix}id").as_str())?
        } else {
            None
        },
        row_no,
        booking_date: booking.map(iso).unwrap_or_default(),
        amount_minor: row
            .try_get::<Option<i64>, _>(format!("{prefix}amount_minor").as_str())?
            .unwrap_or(0),
        narration: row
            .try_get::<Option<String>, _>(format!("{prefix}narration_raw").as_str())?
            .unwrap_or_default(),
        reference: row.try_get(format!("{prefix}reference").as_str())?,
    }))
}

/// Up to `limit` (at most [`MAX_CHANGES_PAGE`]) of a revision's changes after `after`, in
/// order. The new file's side is read from the held rows while the revision is held.
pub async fn revision_changes(
    pool: &PgPool,
    firm: FirmId,
    id: ImportId,
    after: i32,
    limit: i64,
) -> Result<Vec<RevisionChangeRecord>, DbError> {
    let rows = sqlx::query(
        "SELECT c.ordinal, c.change, c.fields,
                l.id AS l_id, l.row_no AS l_row_no, l.booking_date AS l_booking_date,
                l.amount_minor AS l_amount_minor, l.narration_raw AS l_narration_raw,
                l.reference AS l_reference,
                h.row_no AS h_row_no, h.booking_date AS h_booking_date,
                h.amount_minor AS h_amount_minor, h.narration_raw AS h_narration_raw,
                h.reference AS h_reference
           FROM recon.import_revision_changes c
           LEFT JOIN recon.statement_lines l ON l.firm_id = c.firm_id AND l.id = c.line_id
           LEFT JOIN recon.import_held_rows h
             ON h.firm_id = c.firm_id AND h.import_id = c.import_id AND h.row_no = c.row_no
          WHERE c.firm_id = $1 AND c.import_id = $2 AND c.ordinal > $3
          ORDER BY c.ordinal LIMIT $4",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .bind(after)
    .bind(limit.clamp(1, MAX_CHANGES_PAGE))
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(RevisionChangeRecord {
                ordinal: r.try_get("ordinal")?,
                change: r.try_get("change")?,
                fields: r.try_get("fields")?,
                line: side(r, "l_", true)?,
                row: side(r, "h_", false)?,
            })
        })
        .collect()
}

/// The fingerprinted rows of a document file being flipped. `$1` firm, `$2` import.
const DOCUMENTS_FP: &str = "WITH staged AS (
             SELECT s.*, (pg_catalog.row_number() OVER (PARTITION BY s.tuple_hash ORDER BY s.row_no) - 1)::int4
                        AS occurrence
               FROM recon.document_staging s WHERE s.firm_id = $1 AND s.import_id = $2
         ),
         fp AS (
             SELECT st.*,
                    pg_catalog.sha256(pg_catalog.convert_to('recon-fp-v1', 'UTF8')
                        || st.tuple_hash || pg_catalog.int4send(st.occurrence)) AS fingerprint
               FROM staged st
         )";

/// Rows whose number another document already has (D-064): a later row of the file with an
/// earlier row's number, or a row whose number a different stored document of the client
/// has. `Some((count, the first 1,000 rows))` when there are any.
async fn document_conflicts(
    tx: &mut PgConnection,
    import: &StagingImport,
) -> Result<Option<(i64, Vec<i32>)>, DbError> {
    // A number names one document per client, kind and counterparty (D-064): a later row
    // of the file with an earlier row's number, or a row whose number a different stored
    // document has, refuses the file.
    let conflicts = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{DOCUMENTS_FP},
         numbered AS (
             SELECT fp.row_no, fp.kind, fp.number_norm, coalesce(fp.counterparty_norm, '') AS cp,
                    fp.fingerprint,
                    pg_catalog.row_number() OVER (
                        PARTITION BY fp.kind, coalesce(fp.counterparty_norm, ''), fp.number_norm
                        ORDER BY fp.row_no) AS nth
               FROM fp WHERE fp.number_norm IS NOT NULL
         ),
         bad AS (
             SELECT n.row_no FROM numbered n WHERE n.nth > 1
             UNION
             SELECT n.row_no FROM numbered n
               JOIN recon.documents d
                 ON d.firm_id = $1 AND d.client_id = $3 AND d.kind = n.kind
                AND coalesce(d.counterparty_norm, '') = n.cp AND d.number_norm = n.number_norm
              WHERE d.fingerprint <> n.fingerprint
         )
         SELECT (SELECT count(*) FROM bad) AS n,
                coalesce((SELECT pg_catalog.array_agg(b.row_no ORDER BY b.row_no)
                            FROM (SELECT row_no FROM bad ORDER BY row_no LIMIT 1000) b),
                         '{{}}'::int4[]) AS rows"
    )))
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(import.client.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    let n: i64 = conflicts.try_get("n")?;
    Ok((n > 0)
        .then(|| conflicts.try_get("rows"))
        .transpose()?
        .map(|rows| (n, rows)))
}

/// Inserts a document file's rows (occurrence indexes and fingerprints computed here) and
/// records rows that stored documents of the client already have as duplicates.
async fn insert_documents(
    tx: &mut PgConnection,
    import: &StagingImport,
    rows_read: i32,
) -> Result<Published, DbError> {
    let counts = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{DOCUMENTS_FP},
         inserted AS (
             INSERT INTO recon.documents
                 (firm_id, client_id, import_id, row_no, kind, number_raw, number_norm,
                  issue_date, due_date, amount_minor, currency, counterparty_raw,
                  counterparty_norm, description, tuple_hash, occurrence, fingerprint)
             SELECT $1, $3, $2, fp.row_no, fp.kind, fp.number_raw, fp.number_norm, fp.issue_date,
                    fp.due_date, fp.amount_minor, fp.currency, fp.counterparty_raw,
                    fp.counterparty_norm, fp.description, fp.tuple_hash, fp.occurrence,
                    fp.fingerprint
               FROM fp ORDER BY fp.row_no
             ON CONFLICT (firm_id, client_id, fingerprint) DO NOTHING
             RETURNING row_no
         ),
         dups AS (
             INSERT INTO recon.document_duplicates (firm_id, import_id, row_no, existing_document_id)
             SELECT $1, $2, fp.row_no, d.id
               FROM fp
               JOIN recon.documents d
                 ON d.firm_id = $1 AND d.client_id = $3 AND d.fingerprint = fp.fingerprint
             RETURNING row_no
         )
         SELECT (SELECT count(*) FROM inserted) AS lines_created,
                (SELECT count(*) FROM dups) AS duplicates,
                (SELECT count(*) FROM fp) AS total,
                (SELECT min(issue_date) FROM fp) AS first, (SELECT max(issue_date) FROM fp) AS last,
                pg_catalog.sha256(coalesce(
                    (SELECT pg_catalog.string_agg(fp.fingerprint, ''::bytea ORDER BY fp.row_no) FROM fp),
                    ''::bytea)) AS digest"
    )))
    .bind(import.firm.as_uuid())
    .bind(import.id.as_uuid())
    .bind(import.client.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    published(&counts, rows_read)
}

async fn flip_documents<H: FlipHook>(
    pool: &PgPool,
    import: StagingImport,
    summary: &ParseSummary,
    actor: Actor<'_>,
    hook: &H,
    flip_timeout: Duration,
) -> Result<FlipOutcome, DbError> {
    let rows_read = count_i32(summary.rows_read)?;
    let mut tx = flip_transaction(pool, flip_timeout).await?;
    if !try_lock(&mut tx, import.client.as_uuid(), CLIENT_DOCUMENTS_LOCK).await? {
        drop(tx);
        discard_for(pool, import, DiscardReason::AccountBusy, actor).await?;
        return Err(DbError::AccountBusy);
    }
    check_staged(&mut tx, &import, rows_read).await?;
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM recon.imports
          WHERE firm_id = $1 AND client_id = $2 AND kind = 'documents' AND file_hash = $3
            AND status = 'complete'",
    )
    .bind(import.firm.as_uuid())
    .bind(import.client.as_uuid())
    .bind(summary.file_hash.0.as_slice())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        discard_as_duplicate(&mut tx, &import, existing, summary, actor).await?;
        tx.commit().await?;
        let earlier = get(pool, import.firm, ImportId::from_uuid(existing)).await?;
        return Ok(FlipOutcome::DuplicateFile(earlier));
    }

    let conflicts = document_conflicts(&mut tx, &import).await?;
    if let Some((n_conflicts, rows)) = conflicts {
        let refusals: Vec<RowRefusal> = rows
            .into_iter()
            .map(|r| RowRefusal {
                row: u32::try_from(r).unwrap_or(0),
                code: RefusalCode::DocumentNumberConflict,
            })
            .collect();
        let refusal = Refusal {
            code: "rows_refused",
            rows_read: Some(summary.rows_read),
            rows_refused: count_u32(n_conflicts),
            refusals: &refusals,
            file_hash: Some(summary.file_hash),
            file_bytes: summary.file_bytes,
            question: None,
        };
        refuse_in(&mut tx, &import, &refusal, actor).await?;
        tx.commit().await?;
        return Ok(FlipOutcome::Refused(
            get(pool, import.firm, import.id).await?,
        ));
    }
    let published = insert_documents(&mut tx, &import, rows_read).await?;
    hook.reached(FlipPoint::LinesInserted).await;
    delete_staging(&mut tx, &import).await?;
    let completion = Completion {
        summary_rows: summary.rows_read,
        file_hash: summary.file_hash,
        file_bytes: summary.file_bytes,
        published: &published,
        plan: None,
        superseded: None,
        revision_digest: None,
    };
    record_completion(
        &mut tx,
        &import_ref(&import),
        &completion,
        ImportStatus::Staging,
        "import_completed",
        actor,
    )
    .await?;
    hook.reached(FlipPoint::BeforeCommit).await;
    tx.commit().await?;
    Ok(FlipOutcome::Complete(
        get(pool, import.firm, import.id).await?,
    ))
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
    /// The question to put to the user (`ambiguous_date_format`).
    pub question: Option<serde_json::Value>,
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
    refuse_in(&mut tx, &import, refusal, actor).await?;
    tx.commit().await?;
    get(pool, import.firm, import.id).await
}

async fn refuse_in(
    tx: &mut PgConnection,
    import: &StagingImport,
    refusal: &Refusal<'_>,
    actor: Actor<'_>,
) -> Result<(), DbError> {
    delete_staging(tx, import).await?;
    let evidence = json(&serde_json::json!({
        "import": import.id.as_uuid(),
        "refusal_code": refusal.code,
        "rows_read": refusal.rows_read,
        "rows_refused": refusal.rows_refused,
    }))?;
    decisions::insert(
        tx,
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
                rows_refused = $6, file_hash = $7, file_bytes = $8, question = $9
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
    .bind(refusal.question.as_ref().map(Json))
    .execute(&mut *tx)
    .await?;
    Ok(())
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
/// decision) and deletes up to `max_rows` staged rows (of each staging table) that belong
/// to finished imports.
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
    let mut deleted = 0_u64;
    for table in [
        staging_table(ImportKind::Statement),
        staging_table(ImportKind::Documents),
    ] {
        let sql = format!(
            "DELETE FROM {table} s
              USING (SELECT s2.firm_id, s2.import_id, s2.row_no
                       FROM {table} s2
                       JOIN recon.imports i ON i.firm_id = s2.firm_id AND i.id = s2.import_id
                      WHERE i.status <> 'staging'
                      LIMIT $1) d
              WHERE s.firm_id = d.firm_id AND s.import_id = d.import_id AND s.row_no = d.row_no"
        );
        deleted = deleted.saturating_add(
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(max_rows)
                .execute(pool)
                .await?
                .rows_affected(),
        );
    }
    Ok(SweepReport {
        discarded: u32::try_from(expired.len()).unwrap_or(u32::MAX),
        staging_rows_deleted: deleted,
    })
}

const RECORD_COLUMNS: &str = "id, kind, client_id, account_id, status, profile, profile_version,
    profile_sha256, file_hash, file_bytes, rows_read, lines_created, duplicates, rows_refused,
    refusal_code, refusals, question, duplicate_of, discard_reason, first_date, last_date,
    overlap_first, overlap_last, overlap_rows, changes";

fn import_record(row: &PgRow) -> Result<ImportRecord, DbError> {
    let file_hash: Option<Vec<u8>> = row.try_get("file_hash")?;
    let profile_sha256: Option<Vec<u8>> = row.try_get("profile_sha256")?;
    let refusals: Option<Json<Vec<RowRefusal>>> = row.try_get("refusals")?;
    let question: Option<Json<serde_json::Value>> = row.try_get("question")?;
    let first: Option<time::Date> = row.try_get("first_date")?;
    let last: Option<time::Date> = row.try_get("last_date")?;
    let overlap_first: Option<time::Date> = row.try_get("overlap_first")?;
    let overlap_last: Option<time::Date> = row.try_get("overlap_last")?;
    let overlap_rows: Option<i32> = row.try_get("overlap_rows")?;
    let overlap = match (overlap_first, overlap_last, overlap_rows) {
        (Some(f), Some(l), Some(rows)) if rows > 0 => Some(Overlap {
            first_date: iso(f),
            last_date: iso(l),
            rows,
        }),
        _ => None,
    };
    Ok(ImportRecord {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        client_id: row.try_get("client_id")?,
        account_id: row.try_get("account_id")?,
        status: row.try_get("status")?,
        profile: row.try_get("profile")?,
        profile_version: row.try_get("profile_version")?,
        profile_sha256: profile_sha256.map(hex::encode),
        file_hash: file_hash.map(hex::encode),
        file_bytes: row.try_get("file_bytes")?,
        rows_read: row.try_get("rows_read")?,
        lines_created: row.try_get("lines_created")?,
        duplicates: row.try_get("duplicates")?,
        rows_refused: row.try_get("rows_refused")?,
        refusal_code: row.try_get("refusal_code")?,
        refusals: refusals.map(|j| j.0).unwrap_or_default(),
        question: question.map(|j| j.0),
        duplicate_of: row.try_get("duplicate_of")?,
        discard_reason: row.try_get("discard_reason")?,
        first_date: first.map(iso),
        last_date: last.map(iso),
        overlap,
        changes: row.try_get("changes")?,
    })
}

/// Loads `firm`'s import `id`.
pub async fn get(pool: &PgPool, firm: FirmId, id: ImportId) -> Result<ImportRecord, DbError> {
    let sql = format!("SELECT {RECORD_COLUMNS} FROM recon.imports WHERE firm_id = $1 AND id = $2");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
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
    let sql = format!(
        "SELECT {RECORD_COLUMNS} FROM recon.imports WHERE firm_id = $1 AND account_id = $2
          ORDER BY created_at, id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
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
