//! R1's import guarantees against a real PostgreSQL: overlap reporting, statement revisions
//! (failure case 2's detect half, drill `statement-revision`, D-009, D-065), document
//! imports (D-064) and the guards behind them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::format_collect,
    reason = "integration tests: a panic is the failure report, and a drill reads top to bottom"
)]

mod common;

use std::fmt::Write as _;

use common::{
    FLIP_TIMEOUT, OPERATOR, count, import, import_documents, month_rows, replay_ok, sqlstate,
    tenant,
};
use recon_db::DbError;
use recon_db::imports::{self, FlipOutcome};
use recon_db::testing::TestDb;
use recon_domain::ImportId;
use sqlx::Executor;

/// Twenty rows on days 1 to 20 (the generator's shape, without balances).
fn base() -> Vec<String> {
    month_rows(0, 20).lines().map(str::to_owned).collect()
}

/// The bank's re-export of `base`: row 5's amount changed, row 8 dropped, a new row on day
/// 10, row 12's narration changed (the line a reviewer will have matched by R3).
fn re_export() -> String {
    let mut rows = base();
    rows[4] = rows[4].replace("-5.04", "-5.40");
    rows[11] = rows[11].replace("PAYMENT 11 ACME", "PAYMENT 11 ACME CORRECTED");
    rows.insert(
        10,
        "2026-03-10,,-99.99,USD,LATE POSTING,REF-NEW,Acme Ltd,".to_owned(),
    );
    rows.remove(7);
    let mut out = String::new();
    for r in rows {
        writeln!(out, "{r}").unwrap();
    }
    out
}

async fn lines_digest(db: &TestDb, account: &recon_db::tenancy::Account) -> (i64, String) {
    imports::account_lines_digest(&db.pool, account.firm, account.id)
        .await
        .unwrap()
}

/// Failure case 2, detect half (drill `statement-revision`): the revision lists exactly the
/// four differences, nothing is overwritten or deleted, and accepting it supersedes the
/// replaced lines — each with its decision — and publishes the rest.
#[tokio::test]
async fn drill_statement_revision() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let base_body: String = base().iter().map(|r| format!("{r}\n")).collect();
    import(&db.pool, &account, &base_body)
        .await
        .unwrap()
        .complete();
    let before = lines_digest(&db, &account).await;

    let held = import(&db.pool, &account, &re_export())
        .await
        .unwrap()
        .revision();
    assert_eq!(held.status, "revision");
    assert_eq!(held.changes, Some(4));
    let id = ImportId::from_uuid(held.id);
    let changes = imports::revision_changes(&db.pool, account.firm, id, 0, 100)
        .await
        .unwrap();
    let listed: Vec<(String, Option<Vec<String>>, Option<i32>, Option<i32>)> = changes
        .iter()
        .map(|c| {
            (
                c.change.clone(),
                c.fields.clone(),
                c.line.as_ref().map(|l| l.row_no),
                c.row.as_ref().map(|r| r.row_no),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (
                "changed".into(),
                Some(vec!["amount".into()]),
                Some(6),
                Some(6)
            ),
            ("removed".into(), None, Some(9), None),
            ("added".into(), None, None, Some(11)),
            (
                "changed".into(),
                Some(vec!["narration".into()]),
                Some(13),
                Some(13)
            ),
        ],
        "the revision lists exactly the four differences, in date order"
    );
    assert_eq!(changes[0].line.as_ref().unwrap().amount_minor, -504);
    assert_eq!(changes[0].row.as_ref().unwrap().amount_minor, -540);
    // Nothing overwritten, nothing deleted, nothing visible.
    assert_eq!(lines_digest(&db, &account).await, before);
    let held_rows = count(
        &db.pool,
        "SELECT count(*) FROM recon.import_held_rows WHERE firm_id = $1",
        account.firm,
    )
    .await;
    assert_eq!(held_rows, 20);
    replay_ok(&db, account.firm).await;

    // The same file again answers with the held revision.
    let again = import(&db.pool, &account, &re_export()).await.unwrap();
    assert!(
        matches!(&again, common::Imported::Flipped(FlipOutcome::DuplicateFile(r)) if r.id == held.id),
        "a repeat of a held file answers with it"
    );

    let accepted = imports::accept_revision(&db.pool, account.firm, id, OPERATOR, FLIP_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(accepted.status, "complete");
    assert_eq!(
        (accepted.lines_created, accepted.duplicates),
        (Some(3), Some(17))
    );
    let superseded: Vec<i32> = sqlx::query_scalar(
        "SELECT row_no FROM recon.statement_lines WHERE status = 'superseded' ORDER BY row_no",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(superseded, [6, 9, 13]);
    let decided: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.decision_log
          WHERE subject_kind = 'line' AND to_state = 'superseded' AND reason_code = 'revision_accepted'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(decided, 3, "every superseded line has its own decision");
    let (total, _) = lines_digest(&db, &account).await;
    assert_eq!(total, 23, "the replaced lines are kept, superseded");
    let live_distinct: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT fingerprint) FROM recon.statement_lines WHERE status <> 'superseded'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(live_distinct, 20);
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.import_held_rows WHERE firm_id = $1",
            account.firm
        )
        .await,
        0
    );
    let report = replay_ok(&db, account.firm).await;
    assert_eq!(report.lines, 23);
    // Decided once: a second acceptance finds no held revision.
    assert!(matches!(
        imports::accept_revision(&db.pool, account.firm, id, OPERATOR, FLIP_TIMEOUT).await,
        Err(DbError::NotARevision)
    ));
    db.cleanup().await;
}

#[tokio::test]
async fn a_rejected_revision_leaves_the_lines_and_keeps_its_list() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let base_body: String = base().iter().map(|r| format!("{r}\n")).collect();
    import(&db.pool, &account, &base_body)
        .await
        .unwrap()
        .complete();
    let before = lines_digest(&db, &account).await;
    let held = import(&db.pool, &account, &re_export())
        .await
        .unwrap()
        .revision();
    let rejected = imports::reject_revision(
        &db.pool,
        account.firm,
        ImportId::from_uuid(held.id),
        OPERATOR,
    )
    .await
    .unwrap();
    assert_eq!(
        (rejected.status.as_str(), rejected.discard_reason.as_deref()),
        ("discarded", Some("revision_rejected"))
    );
    assert_eq!(lines_digest(&db, &account).await, before);
    let listed =
        imports::revision_changes(&db.pool, account.firm, ImportId::from_uuid(held.id), 0, 10)
            .await
            .unwrap();
    assert_eq!(listed.len(), 4, "the list stays as history");
    assert!(
        listed.iter().all(|c| c.row.is_none()),
        "the held rows are gone"
    );
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

/// A revision lists what accepting it would do; when another import changes the lines it
/// compared against, accepting is refused (`revision_stale`) and nothing changes.
#[tokio::test]
async fn a_stale_revision_cannot_be_accepted() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let base_body: String = base().iter().map(|r| format!("{r}\n")).collect();
    import(&db.pool, &account, &base_body)
        .await
        .unwrap()
        .complete();
    let held = import(&db.pool, &account, &re_export())
        .await
        .unwrap()
        .revision();
    // A superset of the base file (one late row on day 7) completes and adds a line the
    // held revision does not have.
    let superset = format!("{base_body}2026-03-07,,-1.00,USD,LATE FEE,REF-LATE,,\n");
    import(&db.pool, &account, &superset)
        .await
        .unwrap()
        .complete();
    let before = lines_digest(&db, &account).await;
    let result = imports::accept_revision(
        &db.pool,
        account.firm,
        ImportId::from_uuid(held.id),
        OPERATOR,
        FLIP_TIMEOUT,
    )
    .await;
    assert!(matches!(result, Err(DbError::RevisionStale)), "{result:?}");
    assert_eq!(lines_digest(&db, &account).await, before);
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

#[tokio::test]
async fn an_overlap_that_only_adds_completes_and_is_reported() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let first = import(&db.pool, &account, &month_rows(0, 20))
        .await
        .unwrap()
        .complete();
    assert_eq!(
        (first.first_date.as_deref(), first.last_date.as_deref()),
        (Some("2026-03-01"), Some("2026-03-20"))
    );
    assert_eq!(first.overlap, None);
    // Days 15 to 31, plus a late-posted row on day 16 the first file did not have: still
    // no stored line is missing, so the file completes.
    let body = format!(
        "{}2026-03-16,,-7.00,USD,LATE POSTING,REF-L,,\n",
        month_rows(14, 17)
    );
    let second = import(&db.pool, &account, &body).await.unwrap().complete();
    assert_eq!(
        (second.rows_read, second.lines_created, second.duplicates),
        (Some(18), Some(12), Some(6))
    );
    let overlap = second.overlap.unwrap();
    assert_eq!(
        (
            overlap.first_date.as_str(),
            overlap.last_date.as_str(),
            overlap.rows
        ),
        ("2026-03-15", "2026-03-20", 7)
    );
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

/// A line's status changes only with its decision, in the same transaction (RCL04), and a
/// superseded line is final (RCL03) — even from raw SQL as the runtime role.
#[tokio::test]
async fn a_line_status_needs_its_decision_and_superseded_is_final() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    import(&db.pool, &account, &month_rows(0, 3))
        .await
        .unwrap()
        .complete();
    let mut tx = db.pool.begin().await.unwrap();
    tx.execute(
        "UPDATE recon.statement_lines SET status = 'superseded', row_version = row_version + 1
          WHERE row_no = 2",
    )
    .await
    .unwrap();
    let committed = tx.commit().await;
    let code = committed
        .unwrap_err()
        .as_database_error()
        .and_then(|e| e.code().map(std::borrow::Cow::into_owned));
    assert_eq!(code.as_deref(), Some("RCL04"));
    let mut conn = db.pool.acquire().await.unwrap();
    assert_eq!(
        sqlstate(
            conn.execute("UPDATE recon.statement_lines SET status = 'matched' WHERE row_no = 2")
                .await
        ),
        "RCL03",
        "a status change must advance the row version"
    );
    assert_eq!(
        sqlstate(
            conn.execute("UPDATE recon.statement_lines SET narration_norm = 'X' WHERE row_no = 2")
                .await
        ),
        "42501",
        "source fields are not even granted"
    );
    drop(conn);
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

const DOCS: &str = "sales_invoice,INV-1,2026-04-01,2026-05-01,1180.00,INR,Acme Traders,\n\
                    sales_invoice,INV-2,2026-04-02,,500.00,INR,Acme Traders,\n\
                    purchase_bill,B-9,2026-04-03,,-250.00,INR,Kestrel Freight,\n\
                    book_entry,,2026-04-04,,-12.00,INR,,Bank charges\n\
                    book_entry,,2026-04-04,,-12.00,INR,,Bank charges\n";

#[tokio::test]
async fn documents_import_once_dedupe_and_refuse_a_reused_number() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let first = import_documents(&db.pool, &account, DOCS)
        .await
        .unwrap()
        .complete();
    assert_eq!(first.kind, "documents");
    assert_eq!(first.account_id, None);
    assert_eq!(
        (first.rows_read, first.lines_created, first.duplicates),
        (Some(5), Some(5), Some(0)),
        "identical book entries in one file are both kept"
    );
    // The same file again answers with the earlier import.
    match import_documents(&db.pool, &account, DOCS).await.unwrap() {
        common::Imported::Flipped(FlipOutcome::DuplicateFile(r)) => assert_eq!(r.id, first.id),
        _ => panic!("a repeated document file must answer with the earlier import"),
    }
    // An overlapping export: two known invoices and a new one.
    let overlap = "sales_invoice,INV-2,2026-04-02,,500.00,INR,Acme Traders,\n\
                   sales_invoice,INV-1,2026-04-01,2026-05-01,1180.00,INR,Acme Traders,\n\
                   sales_invoice,INV-3,2026-04-09,,75.00,INR,Acme Traders,\n";
    let second = import_documents(&db.pool, &account, overlap)
        .await
        .unwrap()
        .complete();
    assert_eq!(
        (second.lines_created, second.duplicates),
        (Some(1), Some(2))
    );
    // INV-1 again with another amount, and INV-4 twice in one file: refused whole, by row.
    let conflicting = "sales_invoice,INV-4,2026-04-10,,10.00,INR,Acme Traders,\n\
                       sales_invoice,INV-1,2026-04-01,,999.00,INR,Acme Traders,\n\
                       sales_invoice,INV-4,2026-04-11,,11.00,INR,Acme Traders,\n";
    let refused = import_documents(&db.pool, &account, conflicting)
        .await
        .unwrap()
        .refused();
    assert_eq!(refused.refusal_code.as_deref(), Some("rows_refused"));
    let rows: Vec<(u32, &str)> = refused
        .refusals
        .iter()
        .map(|r| (r.row, r.code.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (3, "document_number_conflict"),
            (4, "document_number_conflict")
        ]
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.documents WHERE firm_id = $1",
            account.firm
        )
        .await,
        6,
        "nothing of a refused file is persisted"
    );
    // The same number from another counterparty is another document.
    let other = "purchase_bill,INV-1,2026-04-12,,-5.00,INR,Bluefin Logistics,\n";
    import_documents(&db.pool, &account, other)
        .await
        .unwrap()
        .complete();
    let report = replay_ok(&db, account.firm).await;
    assert_eq!(report.complete_imports, 3);
    db.cleanup().await;
}

#[tokio::test]
async fn documents_and_profiles_are_immutable_and_documents_need_their_import() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    import_documents(&db.pool, &account, DOCS)
        .await
        .unwrap()
        .complete();
    recon_db::profiles::create(
        &db.pool,
        account.firm,
        "bank-a",
        "statement_csv",
        &serde_json::json!({"x": 1}),
        [7; 32],
    )
    .await
    .unwrap();
    let owner = db.owner_pool().await;
    for (sql, code) in [
        ("UPDATE recon.documents SET amount_minor = 1", "RCD02"),
        ("DELETE FROM recon.documents", "RCD02"),
        ("UPDATE recon.import_profiles SET version = 9", "RCP01"),
        ("DELETE FROM recon.import_profiles", "RCP01"),
        ("DELETE FROM recon.import_revision_changes", "RCI06"),
    ] {
        assert_eq!(sqlstate(owner.execute(sql).await), code, "{sql}");
    }
    owner.close().await;
    let next = recon_db::profiles::create(
        &db.pool,
        account.firm,
        "bank-a",
        "statement_csv",
        &serde_json::json!({"x": 2}),
        [8; 32],
    )
    .await
    .unwrap();
    assert_eq!(next.version, 2);
    let latest = recon_db::profiles::get(&db.pool, account.firm, "bank-a", None)
        .await
        .unwrap();
    assert_eq!((latest.version, latest.sha256), (2, [8; 32]));
    let first = recon_db::profiles::get(&db.pool, account.firm, "bank-a", Some(1))
        .await
        .unwrap();
    assert_eq!(first.definition, serde_json::json!({"x": 1}));
    // Built-in names are reserved.
    assert!(
        recon_db::profiles::create(
            &db.pool,
            account.firm,
            "ofx",
            "statement_csv",
            &serde_json::json!({}),
            [0; 32]
        )
        .await
        .is_err()
    );
    db.cleanup().await;
}
