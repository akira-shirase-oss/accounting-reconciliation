//! Scratch-database hygiene: the load harness's trap and the test harness's backstop
//! drop what they create (the workspace standard: nothing a run creates outlives it).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a panic is the failure report"
)]

use recon_db::roles;
use recon_db::testing::{ENV, TestDb};
use sqlx::{Connection, PgConnection};

/// `recon db drop`/`roles::drop_database` is what `scripts/load.sh`'s trap calls. The
/// harness runs twice here: two scratch databases, each created and dropped over TCP,
/// and `pg_database` shows none of them left behind.
#[tokio::test]
async fn the_load_harness_leaves_no_scratch_database() {
    let admin_url = std::env::var(ENV).expect("RECON_TEST_ADMIN_URL is required");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connecting as the administrator");
    for tag in ["one", "two"] {
        let name = format!("recon_load_harness_test_{tag}");
        roles::create_database(&mut admin, &name)
            .await
            .expect("creating the scratch database");
        roles::drop_database(&mut admin, &name)
            .await
            .expect("the harness's drop");
    }
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*)::int8 FROM pg_catalog.pg_database
           WHERE datname LIKE 'recon_load_harness_test_%'",
    )
    .fetch_one(&mut admin)
    .await
    .expect("counting leftover scratch databases");
    assert_eq!(left, 0, "the harness left scratch databases behind");
}

/// A test that never calls `cleanup` still leaves nothing: `TestDb`'s `Drop` drops the
/// database on the passed path (a failed test keeps it, its name carrying the test).
#[tokio::test]
async fn the_test_harness_drops_a_passed_tests_database() {
    let admin_url = std::env::var(ENV).expect("RECON_TEST_ADMIN_URL is required");
    let name = {
        let db = TestDb::new().await;
        let name = db.name().to_owned();
        assert!(
            name.starts_with("recon_test_the_test_harness_drops"),
            "the database names its test: {name}"
        );
        drop(db); // no cleanup(): the backstop does the work
        name
    };
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connecting as the administrator");
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
    )
    .bind(&name)
    .fetch_one(&mut admin)
    .await
    .expect("checking the dropped database");
    assert!(!exists, "the passed test's database {name} was kept");
    admin.close().await.expect("closing the administrator");
}
