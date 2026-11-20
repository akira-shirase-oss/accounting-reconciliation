//! `recon explain line` — the ship gate's "every one of those decisions can be explained"
//! as a command: the real binary against a test database shows the line's state, every
//! decision that put it there, the match covering it and its exception, and names an
//! unknown line without leaking anything else.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests: a panic is the failure report"
)]

use std::fmt::Write as _;
use std::process::Command;
use std::time::Duration;

use recon_db::imports::{self, Actor, FlipOutcome, NoHook, STAGING_BATCH_ROWS};
use recon_db::matching::{self, Decided, DecidedLine, Start};
use recon_db::tenancy::{self, Account};
use recon_db::testing::TestDb;
use recon_db::{auth, review};
use recon_domain::Role;
use recon_domain::line::{ActorKind, ExceptionKind, ExcludeReason};
use recon_domain::matching::{
    BalancedMatch, DocumentAllocation, LineAllocation, ScoreBreakdown, Tier,
};
use recon_domain::{AccountId, ClientId, Currency, DocumentId, FirmId, LineId, Money, UserId};
use recon_import::{
    AccountContext, CompiledDocuments, DocumentContext, DocumentParser, StatementParser,
};
use sqlx::PgPool;
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(30);
const TX_TIMEOUT: Duration = Duration::from_secs(60);
const ENGINE: &str = "gen-v3";
const HEADER: &str =
    "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";
const OPERATOR: Actor<'static> = Actor {
    kind: ActorKind::Operator,
    reference: "operator",
};

fn recon(args: &[&str], db: &TestDb) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_recon"))
        .args(args)
        .env("RECON_DATABASE_URL", db.app_url())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

async fn tenant(pool: &PgPool) -> Account {
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

fn rows(first: u32, n: u32) -> String {
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

async fn import(pool: &PgPool, account: &Account, body: &str) {
    let mut parser = StatementParser::new(AccountContext {
        account: account.id,
        currency: account.currency,
    });
    let mut lines = vec![];
    let bytes = format!("{HEADER}{body}");
    parser
        .push(bytes.as_bytes(), &mut |l| lines.push(l))
        .expect("file-level refusal in a cli test");
    let summary = parser
        .finish(&mut |l| lines.push(l))
        .expect("file-level refusal in a cli test");
    assert!(summary.is_clean(), "the fixture file must parse");
    let mut staging = imports::begin(pool, account, LEASE).await.unwrap();
    for batch in lines.chunks(STAGING_BATCH_ROWS) {
        imports::stage(pool, &mut staging, batch, LEASE)
            .await
            .unwrap();
    }
    match imports::flip(
        pool,
        staging,
        &summary,
        OPERATOR,
        &NoHook,
        Duration::from_secs(60),
    )
    .await
    .unwrap()
    {
        FlipOutcome::Complete(record) => assert_eq!(record.status, "complete"),
        other => panic!("the import was not completed: {other:?}"),
    }
}

async fn import_documents(pool: &PgPool, account: &Account, body: &str) {
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
        .expect("file-level refusal in a cli test");
    let summary = parser
        .finish(&mut |d| docs.push(d))
        .expect("file-level refusal in a cli test");
    assert!(summary.is_clean(), "the document file must parse");
    let mut staging =
        imports::begin_documents(pool, account.firm, account.client, &id, None, LEASE)
            .await
            .unwrap();
    for batch in docs.chunks(STAGING_BATCH_ROWS) {
        imports::stage_documents(pool, &mut staging, batch, LEASE)
            .await
            .unwrap();
    }
    match imports::flip(
        pool,
        staging,
        &summary,
        OPERATOR,
        &NoHook,
        Duration::from_secs(60),
    )
    .await
    .unwrap()
    {
        FlipOutcome::Complete(record) => assert_eq!(record.status, "complete"),
        other => panic!("the document import was not completed: {other:?}"),
    }
}

fn usd(minor: i64) -> Money {
    Money::new(minor, Currency::Usd).unwrap()
}

/// The line ids of `account`'s statement lines in `status`, oldest row first.
async fn lines_in(pool: &PgPool, account: &Account, status: &str) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM recon.statement_lines
          WHERE firm_id = $1 AND account_id = $2 AND status = $3 ORDER BY booking_date, row_no",
    )
    .bind(account.firm.as_uuid())
    .bind(account.id.as_uuid())
    .bind(status)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn start_job(db: &TestDb, account: &Account) -> matching::JobLease {
    let job = match matching::start(&db.pool, account.firm, account.client, LEASE, ENGINE)
        .await
        .unwrap()
    {
        Start::Created(l) | Start::Resumed(l) => l,
        Start::Busy { job } => panic!("the job {job} is busy"),
    };
    matching::snapshot(&db.pool, &job).await.unwrap();
    job
}

fn t1(line: LineId, doc: DocumentId, doc_amount: i64, line_minor: i64) -> DecidedLine {
    DecidedLine {
        seq: 1,
        line,
        outcome: Decided::Matched {
            tier: Tier::T1,
            matched: BalancedMatch::new(
                vec![LineAllocation {
                    line,
                    amount: usd(line_minor),
                }],
                vec![DocumentAllocation {
                    document: doc,
                    amount: usd(doc_amount),
                }],
                vec![],
            )
            .unwrap(),
            score: ScoreBreakdown::new(true, true, true, 8, true).unwrap(),
            commit: true,
        },
    }
}

/// A matched line's explanation carries the match and its decisions; an excluded one the
/// reviewer's; an exception line its queue row. `--json` carries the same, machine-read.
#[tokio::test]
async fn explain_line_shows_the_whole_chain() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user: UserId = auth::create_user(
        &db.pool,
        account.firm,
        "owner@example.test",
        "owner",
        Role::Owner,
        None,
    )
    .await
    .unwrap();
    import(&db.pool, &account, &rows(0, 3)).await;
    import_documents(
        &db.pool,
        &account,
        "purchase_bill,INV-A,2026-03-01,2026-03-31,-1.00,USD,Acme Ltd,\n",
    )
    .await;
    let mut undecided = lines_in(&db.pool, &account, "unreviewed").await;
    assert_eq!(undecided.len(), 3, "{undecided:?}");

    // Line 1: the engine commits a T1 match against the bill.
    let matched_line = LineId::from_uuid(undecided.remove(0));
    let line_minor: i64 = sqlx::query_scalar(
        "SELECT amount_minor FROM recon.statement_lines WHERE firm_id = $1 AND id = $2",
    )
    .bind(account.firm.as_uuid())
    .bind(matched_line.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let doc: Uuid = sqlx::query_scalar(
        "SELECT id FROM recon.documents WHERE firm_id = $1 AND kind = 'purchase_bill'",
    )
    .bind(account.firm.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let doc_amount: i64 =
        sqlx::query_scalar("SELECT amount_minor FROM recon.documents WHERE id = $1")
            .bind(doc)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let job = start_job(&db, &account).await;
    matching::commit_chunk(
        &db.pool,
        &job,
        &[
            t1(
                matched_line,
                DocumentId::from_uuid(doc),
                doc_amount,
                line_minor,
            ),
            DecidedLine {
                seq: 2,
                line: LineId::from_uuid(undecided[1]),
                outcome: Decided::Exception {
                    kind: ExceptionKind::NoCandidate,
                    candidates: vec![],
                },
            },
        ],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();

    // Line 2: a reviewer excludes it.
    let excluded_line = LineId::from_uuid(undecided[0]);
    review::exclude(
        &db.pool,
        account.firm,
        account.client,
        excluded_line,
        user.as_uuid(),
        1,
        ExcludeReason::OutOfScope,
        Some("outside the engagement"),
    )
    .await
    .unwrap();

    // Matched: status, the T1 match that covers it, both decisions, the engine actor.
    let arg = matched_line.as_uuid().to_string();
    let (code, text) = recon(&["explain", "line", &arg], &db);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains(&arg), "{text}");
    assert!(text.contains("matched"), "{text}");
    assert!(text.contains("t1"), "{text}");
    assert!(text.contains("committed"), "{text}");
    assert!(text.contains("unreviewed"), "{text}");
    assert!(text.contains("engine"), "{text}");
    assert!(text.contains("import"), "{text}");

    // Excluded: the reviewer by email and the reason code.
    let arg = excluded_line.as_uuid().to_string();
    let (code, text) = recon(&["explain", "line", &arg], &db);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("excluded"), "{text}");
    assert!(text.contains("unreviewed"), "{text}");
    assert!(text.contains("owner@example.test"), "{text}");
    assert!(text.contains("out_of_scope"), "{text}");

    // The exception line: its kind and that nobody has acknowledged it.
    let exception_line = undecided[1];
    let arg = exception_line.to_string();
    let (code, text) = recon(&["explain", "line", &arg], &db);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("exception"), "{text}");
    assert!(text.contains("no_candidate"), "{text}");
    assert!(text.contains("unacknowledged"), "{text}");

    // Machine-readable: the decisions array carries the trail.
    let arg = matched_line.as_uuid().to_string();
    let (code, text) = recon(&["explain", "line", &arg, "--json"], &db);
    assert_eq!(code, 0, "{text}");
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["line"]["status"], "matched");
    assert_eq!(json["line"]["id"], arg);
    let subjects: Vec<&str> = json["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["subject_kind"].as_str())
        .collect();
    assert!(subjects.contains(&"line"), "{subjects:?}");
    assert!(subjects.contains(&"match"), "{subjects:?}");
    assert!(subjects.contains(&"import"), "{subjects:?}");
    assert_eq!(json["matches"].as_array().unwrap().len(), 1);
}

/// An unknown line is named and exits 4 (nothing to explain); a malformed id is usage, 1.
#[tokio::test]
async fn explain_line_names_an_unknown_one() {
    let db = TestDb::new().await;
    let unknown = Uuid::now_v7().to_string();
    let (code, text) = recon(&["explain", "line", &unknown], &db);
    assert_eq!(code, 4, "{text}");
    assert!(text.contains(&unknown), "{text}");
    assert!(text.contains("no such line"), "{text}");

    let (code, text) = recon(&["explain", "line", "not-a-uuid"], &db);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("usage"), "{text}");
}
