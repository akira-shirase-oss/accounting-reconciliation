//! The import spine against a real PostgreSQL: whole or nothing, idempotent on the file
//! hash, duplicates reported, refusals persisting nothing, stale processes fenced out.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a panic is the failure report"
)]

mod common;

use std::time::Duration;

use common::{
    FLIP_TIMEOUT, HEADER, Imported, LEASE, OPERATOR, count, import, replay_ok, rows, tenant,
};
use recon_db::DbError;
use recon_db::imports::{self, FlipOutcome, NoHook};
use recon_db::testing::TestDb;
use recon_import::{AccountContext, StatementParser};
use sqlx::Executor;

#[tokio::test]
async fn a_clean_file_imports_whole_and_the_log_replays() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let record = import(&db.pool, &account, &rows(0, 12_345))
        .await
        .unwrap()
        .complete();
    assert_eq!(record.status, "complete");
    assert_eq!(
        (record.rows_read, record.lines_created, record.duplicates),
        (Some(12_345), Some(12_345), Some(0))
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1",
            account.firm
        )
        .await,
        12_345
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.import_staging WHERE firm_id = $1",
            account.firm
        )
        .await,
        0
    );
    let report = replay_ok(&db, account.firm).await;
    assert_eq!(
        (report.complete_imports, report.lines, report.decisions),
        (1, 12_345, 1)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn reimporting_the_same_file_answers_with_the_earlier_import() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let body = rows(0, 500);
    let first = import(&db.pool, &account, &body).await.unwrap().complete();
    let Imported::Flipped(FlipOutcome::DuplicateFile(earlier)) =
        import(&db.pool, &account, &body).await.unwrap()
    else {
        panic!("the same file twice must answer with the earlier import");
    };
    assert_eq!(earlier.id, first.id);
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1",
            account.firm
        )
        .await,
        500
    );
    let all = imports::for_account(&db.pool, account.firm, account.id)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].status, "discarded");
    assert_eq!(all[1].discard_reason.as_deref(), Some("duplicate_file"));
    assert_eq!(all[1].duplicate_of, Some(first.id));
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

#[tokio::test]
async fn identical_twins_in_one_file_are_both_kept() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let twins = "2026-03-01,,-20.00,USD,ATM WITHDRAWAL,,,\n".repeat(3);
    let record = import(&db.pool, &account, &twins).await.unwrap().complete();
    assert_eq!(record.lines_created, Some(3));
    let occurrences: Vec<i32> =
        sqlx::query_scalar("SELECT occurrence FROM recon.statement_lines ORDER BY row_no")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(occurrences, [0, 1, 2]);
    db.cleanup().await;
}

#[tokio::test]
async fn an_overlapping_statement_adds_only_the_new_lines_and_names_the_rest() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    import(&db.pool, &account, &rows(0, 20))
        .await
        .unwrap()
        .complete();
    let second = import(&db.pool, &account, &rows(14, 17))
        .await
        .unwrap()
        .complete();
    assert_eq!(
        (second.rows_read, second.lines_created, second.duplicates),
        (Some(17), Some(11), Some(6))
    );
    let named: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.import_duplicates d
           JOIN recon.statement_lines l ON l.firm_id = d.firm_id AND l.id = d.existing_line_id
          WHERE d.import_id = $1 AND l.import_id <> $1",
    )
    .bind(second.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        named, 6,
        "every duplicate names an existing line of an earlier import"
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1",
            account.firm
        )
        .await,
        31
    );
    let distinct: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT fingerprint) FROM recon.statement_lines")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(distinct, 31);
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

#[tokio::test]
async fn a_refused_file_persists_nothing_and_says_why() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    // 6,000 good rows (one batch is staged before the bad row is reached), then one bad.
    let body = format!("{}2026-03-01,,1.234,USD,BAD,,,\n", rows(0, 6_000));
    let Imported::Refused(record) = import(&db.pool, &account, &body).await.unwrap() else {
        panic!("a malformed row must refuse the file");
    };
    assert_eq!(record.status, "refused");
    assert_eq!(record.refusal_code.as_deref(), Some("rows_refused"));
    assert_eq!(
        (record.rows_read, record.rows_refused),
        (Some(6_001), Some(1))
    );
    assert_eq!(record.refusals.len(), 1);
    assert_eq!(record.refusals[0].row, 6_002);
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1",
            account.firm
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.import_staging WHERE firm_id = $1",
            account.firm
        )
        .await,
        0
    );
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

#[tokio::test]
async fn a_flip_refuses_when_staged_rows_went_missing() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let bytes = format!("{HEADER}{}", rows(0, 7_000));
    let mut parser = StatementParser::new(AccountContext {
        account: account.id,
        currency: account.currency,
    });
    let mut lines = vec![];
    parser
        .push(bytes.as_bytes(), &mut |l| lines.push(l))
        .unwrap();
    let summary = parser.finish(&mut |l| lines.push(l)).unwrap();
    let mut staging = imports::begin(&db.pool, &account, LEASE).await.unwrap();
    for batch in lines.chunks(imports::STAGING_BATCH_ROWS) {
        imports::stage(&db.pool, &mut staging, batch, LEASE)
            .await
            .unwrap();
    }
    // What a crash of the unlogged table looks like: rows gone, the import still staging.
    db.pool
        .execute("DELETE FROM recon.import_staging WHERE row_no % 7 = 0")
        .await
        .unwrap();
    let flipped = imports::flip(&db.pool, staging, &summary, OPERATOR, &NoHook, FLIP_TIMEOUT).await;
    assert!(
        matches!(flipped, Err(DbError::StagingIncomplete)),
        "{flipped:?}"
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1",
            account.firm
        )
        .await,
        0
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_stale_process_is_fenced_out_and_the_sweeper_discards_its_import() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let bytes = format!("{HEADER}{}", rows(0, 100));
    let mut parser = StatementParser::new(AccountContext {
        account: account.id,
        currency: account.currency,
    });
    let mut lines = vec![];
    parser
        .push(bytes.as_bytes(), &mut |l| lines.push(l))
        .unwrap();
    let summary = parser.finish(&mut |l| lines.push(l)).unwrap();
    let short = Duration::from_millis(200);
    let mut staging = imports::begin(&db.pool, &account, short).await.unwrap();
    imports::stage(&db.pool, &mut staging, &lines[..50], short)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    // The lease expired: the next batch stages nothing.
    assert!(matches!(
        imports::stage(&db.pool, &mut staging, &lines[50..], short).await,
        Err(DbError::LeaseLost)
    ));
    let swept = imports::sweep(&db.pool, 100, 100_000).await.unwrap();
    assert_eq!(swept.discarded, 1);
    assert_eq!(swept.staging_rows_deleted, 50);
    // A flip by the stale process is refused by its fencing token.
    let flipped = imports::flip(&db.pool, staging, &summary, OPERATOR, &NoHook, FLIP_TIMEOUT).await;
    assert!(matches!(flipped, Err(DbError::LeaseLost)), "{flipped:?}");
    let all = imports::for_account(&db.pool, account.firm, account.id)
        .await
        .unwrap();
    assert_eq!(all[0].discard_reason.as_deref(), Some("lease_expired"));
    // The file then imports cleanly, exactly once.
    import(&db.pool, &account, &rows(0, 100))
        .await
        .unwrap()
        .complete();
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1",
            account.firm
        )
        .await,
        100
    );
    replay_ok(&db, account.firm).await;
    db.cleanup().await;
}
