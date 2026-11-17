//! Shared helpers for the database tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    unreachable_pub,
    reason = "test helpers: a panic is the failure report"
)]

use std::fmt::Write as _;
use std::time::Duration;

use recon_db::DbError;
use recon_db::imports::{
    self, Actor, FlipOutcome, ImportRecord, NoHook, Refusal, STAGING_BATCH_ROWS,
};
use recon_db::tenancy::{self, Account};
use recon_db::testing::TestDb;
use recon_domain::line::ActorKind;
use recon_domain::{AccountId, ClientId, Currency, FirmId};
use recon_import::{
    AccountContext, CompiledDocuments, DocumentContext, DocumentParser, StatementParser,
};
use sqlx::PgPool;
use uuid::Uuid;

pub const HEADER: &str =
    "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";

pub const OPERATOR: Actor<'static> = Actor {
    kind: ActorKind::Operator,
    reference: "operator",
};
pub const LEASE: Duration = Duration::from_secs(30);
pub const FLIP_TIMEOUT: Duration = Duration::from_secs(60);

/// A firm, a client and a USD account.
pub async fn tenant(pool: &PgPool) -> Account {
    let firm = FirmId::from_uuid(Uuid::now_v7());
    let client = ClientId::from_uuid(Uuid::now_v7());
    tenancy::create_firm(pool, firm, "Test firm").await.unwrap();
    tenancy::create_client(pool, firm, client, "Test client")
        .await
        .unwrap();
    tenancy::create_account(
        pool,
        firm,
        client,
        AccountId::from_uuid(Uuid::now_v7()),
        "Current",
        Currency::Usd,
    )
    .await
    .unwrap()
}

/// `n` distinct rows starting at day `first` of March 2026 (cycling through 28 days).
pub fn rows(first: u32, n: u32) -> String {
    let mut out = String::new();
    for i in first..first + n {
        let day = 1 + i % 28;
        writeln!(
            out,
            "2026-03-{day:02},,{}.{:02},USD,PAYMENT {i} ACME,REF-{i},Acme Ltd,",
            -(i64::from(i) + 1),
            i % 100
        )
        .unwrap();
    }
    out
}

/// `n` distinct rows on consecutive days of March 2026 from day `first + 1` (no wrapping:
/// `first + n` is at most 31), so two files of them overlap only where their days do.
pub fn month_rows(first: u32, n: u32) -> String {
    assert!(first + n <= 31, "March has 31 days");
    let mut out = String::new();
    for i in first..first + n {
        writeln!(
            out,
            "2026-03-{:02},,-{}.{:02},USD,PAYMENT {i} ACME,REF-{i},Acme Ltd,",
            i + 1,
            i + 1,
            i % 100
        )
        .unwrap();
    }
    out
}

pub enum Imported {
    Flipped(FlipOutcome),
    Refused(ImportRecord),
}

impl Imported {
    pub fn complete(self) -> ImportRecord {
        match self {
            Self::Flipped(FlipOutcome::Complete(r)) => r,
            other => panic!("expected a complete import, got {}", other.describe()),
        }
    }

    pub fn revision(self) -> ImportRecord {
        match self {
            Self::Flipped(FlipOutcome::Revision(r)) => r,
            other => panic!("expected a held revision, got {}", other.describe()),
        }
    }

    pub fn refused(self) -> ImportRecord {
        match self {
            Self::Refused(r) | Self::Flipped(FlipOutcome::Refused(r)) => r,
            other @ Self::Flipped(_) => {
                panic!("expected a refused import, got {}", other.describe())
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Flipped(FlipOutcome::Complete(r)) => format!("complete {}", r.id),
            Self::Flipped(FlipOutcome::DuplicateFile(r)) => format!("duplicate of {}", r.id),
            Self::Flipped(FlipOutcome::Revision(r)) => {
                format!("revision {} with {:?} changes", r.id, r.changes)
            }
            Self::Flipped(FlipOutcome::Refused(r)) | Self::Refused(r) => {
                format!("refused {:?} {:?}", r.refusal_code, r.refusals)
            }
        }
    }
}

/// Parses `body` (after the header) and imports it the way the server does.
pub async fn import(pool: &PgPool, account: &Account, body: &str) -> Result<Imported, DbError> {
    import_bytes(pool, account, format!("{HEADER}{body}").as_bytes()).await
}

pub async fn import_bytes(
    pool: &PgPool,
    account: &Account,
    bytes: &[u8],
) -> Result<Imported, DbError> {
    let mut parser = StatementParser::new(AccountContext {
        account: account.id,
        currency: account.currency,
    });
    let mut lines = vec![];
    let mut staging = imports::begin(pool, account, LEASE).await?;
    for chunk in bytes.chunks(64 * 1024) {
        parser
            .push(chunk, &mut |l| lines.push(l))
            .expect("file-level refusal in a db test");
        while lines.len() >= STAGING_BATCH_ROWS {
            let batch: Vec<_> = lines.drain(..STAGING_BATCH_ROWS).collect();
            imports::stage(pool, &mut staging, &batch, LEASE).await?;
        }
    }
    let summary = parser
        .finish(&mut |l| lines.push(l))
        .expect("file-level refusal in a db test");
    if !summary.is_clean() {
        let record = imports::refuse(
            pool,
            staging,
            &Refusal {
                code: "rows_refused",
                rows_read: Some(summary.rows_read),
                rows_refused: summary.rows_refused,
                refusals: &summary.refusals,
                file_hash: Some(summary.file_hash),
                file_bytes: summary.file_bytes,
                question: None,
            },
            OPERATOR,
        )
        .await?;
        return Ok(Imported::Refused(record));
    }
    for batch in lines.chunks(STAGING_BATCH_ROWS) {
        imports::stage(pool, &mut staging, batch, LEASE).await?;
    }
    let outcome = imports::flip(pool, staging, &summary, OPERATOR, &NoHook, FLIP_TIMEOUT).await?;
    Ok(Imported::Flipped(outcome))
}

/// Imports a document file with the built-in profile, the way the server does.
pub async fn import_documents(
    pool: &PgPool,
    account: &Account,
    body: &str,
) -> Result<Imported, DbError> {
    let profile = std::sync::Arc::new(CompiledDocuments::builtin());
    let id = imports::ProfileId {
        name: recon_domain::import::PROFILE_DOCUMENTS.to_owned(),
        version: recon_domain::import::PROFILE_DOCUMENTS_VERSION,
        sha256: profile.sha256(),
    };
    let mut parser = DocumentParser::with_profile(
        DocumentContext {
            client: account.client,
        },
        profile,
        recon_import::CsvLimits::default(),
    );
    let mut docs = vec![];
    let bytes =
        format!("kind,number,issue_date,due_date,amount,currency,counterparty,description\n{body}");
    parser
        .push(bytes.as_bytes(), &mut |d| docs.push(d))
        .expect("file-level refusal in a db test");
    let summary = parser
        .finish(&mut |d| docs.push(d))
        .expect("file-level refusal in a db test");
    let mut staging =
        imports::begin_documents(pool, account.firm, account.client, &id, None, LEASE).await?;
    if !summary.is_clean() {
        let record = imports::refuse(
            pool,
            staging,
            &Refusal {
                code: "rows_refused",
                rows_read: Some(summary.rows_read),
                rows_refused: summary.rows_refused,
                refusals: &summary.refusals,
                file_hash: Some(summary.file_hash),
                file_bytes: summary.file_bytes,
                question: None,
            },
            OPERATOR,
        )
        .await?;
        return Ok(Imported::Refused(record));
    }
    for batch in docs.chunks(STAGING_BATCH_ROWS) {
        imports::stage_documents(pool, &mut staging, batch, LEASE).await?;
    }
    let outcome = imports::flip(pool, staging, &summary, OPERATOR, &NoHook, FLIP_TIMEOUT).await?;
    Ok(Imported::Flipped(outcome))
}

pub async fn count(pool: &PgPool, sql: &'static str, firm: FirmId) -> i64 {
    sqlx::query_scalar(sql)
        .bind(firm.as_uuid())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The SQLSTATE of a failed statement.
pub fn sqlstate(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(e) => e
            .as_database_error()
            .and_then(|d| d.code().map(std::borrow::Cow::into_owned))
            .unwrap_or_else(|| format!("not a database error: {e}")),
    }
}

pub async fn replay_ok(db: &TestDb, firm: FirmId) -> recon_db::replay::ReplayReport {
    recon_db::replay::check(&db.pool, firm)
        .await
        .unwrap()
        .expect("the decision log replays to the stored state")
}
