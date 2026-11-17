//! The real binary refuses to run with powers beyond the runtime role's (D-037) or with
//! an unsafe configuration, and exits with `EX_CONFIG` (78).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

mod common;

use std::time::Duration;

use common::Server;
use recon_db::testing::TestDb;

#[tokio::test]
async fn the_server_refuses_to_serve_as_the_owner_or_the_superuser() {
    let db = TestDb::new().await;
    for (who, url) in [("owner", db.owner_url()), ("superuser", db.admin_url())] {
        let server = Server::spawn(&url, &[]);
        let (code, log) = server.exit_code(Duration::from_secs(30)).await;
        assert_eq!(code, Some(78), "as the {who}: {log}");
        assert!(log.contains("refusing to serve"), "{log}");
    }
    // The runtime role is accepted.
    let server = Server::start(&db.app_url(), &[]).await;
    server.stop().await;
    db.cleanup().await;
}

#[tokio::test]
async fn the_server_refuses_an_unsafe_configuration() {
    let db = TestDb::new().await;
    for env in [
        &[("RECON_OPERATOR_TOKEN", "short")][..],
        &[("RECON_LISTEN", "not-an-address")],
        &[("RECON_IMPORT_CONCURRENCY", "0")],
    ] {
        let server = Server::spawn(&db.app_url(), env);
        let (code, log) = server.exit_code(Duration::from_secs(30)).await;
        assert_eq!(code, Some(78), "{env:?}: {log}");
        assert!(
            !log.contains("recon-app-dev"),
            "the database password must not be printed: {log}"
        );
    }
    #[cfg(not(feature = "fault-injection"))]
    {
        let server = Server::spawn(&db.app_url(), &[("RECON_FAULT", "before_flip")]);
        let (code, _) = server.exit_code(Duration::from_secs(30)).await;
        assert_eq!(
            code,
            Some(78),
            "a release-shaped build refuses fault settings"
        );
    }
    db.cleanup().await;
}
