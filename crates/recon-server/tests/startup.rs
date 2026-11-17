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
    // The owner cannot log in (D-049); a superuser acting as it is the way to be it.
    let as_owner = format!("{}?options=-c%20role%3Drecon_owner", db.admin_url());
    for (who, url) in [("owner", as_owner), ("superuser", db.admin_url())] {
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

/// A database on another machine is reached only over TLS that checks its certificate and
/// its name (D-070): `sslmode=require` encrypts but accepts any certificate, so anyone on
/// the network path could impersonate the database. Each refusal is named, says how to
/// fix the URL, and never prints the password.
#[tokio::test]
async fn the_server_refuses_a_remote_database_without_verified_tls() {
    let remote = "recon_app:hunter2-secret@db.recon.test:5432/recon";
    for (url, code) in [
        (
            format!("postgres://{remote}?sslmode=require"),
            "database_tls_unverified",
        ),
        (format!("postgres://{remote}"), "database_tls_unverified"),
        (
            format!("postgres://{remote}?sslmode=prefer"),
            "database_tls_unverified",
        ),
        (
            "postgres://recon_app:hunter2-secret@10.1.2.3:5432/recon?sslmode=verify-ca".into(),
            "database_tls_no_root_certificate",
        ),
        (
            format!("postgres://{remote}?sslmode=verify-full&channel_binding=require"),
            "database_channel_binding_unsupported",
        ),
    ] {
        let server = Server::spawn(&url, &[]);
        let (exit, log) = server.exit_code(Duration::from_secs(15)).await;
        assert_eq!(exit, Some(78), "{url}: {log}");
        assert!(
            log.contains(code) && log.contains("verify-full"),
            "{url}: the refusal names {code} and the fix: {log}"
        );
        assert!(
            !log.contains("hunter2-secret"),
            "the password must not be printed: {log}"
        );
    }
}
