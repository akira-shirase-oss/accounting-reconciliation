//! D-070 against a real server: `postgres-tls` (deploy/compose.yaml) presents a certificate
//! for `localhost` signed by the development CA (scripts/dev-tls.sh). Through the same
//! options the server and the CLI use (`recon_db::pool::checked_options`):
//!
//! * `verify-full` with that CA connects, encrypted;
//! * `verify-full` to a name the certificate does not carry (`127.0.0.1`) is refused, and
//!   so is an unknown CA (the public roots alone) — an impostor cannot pass either check;
//! * `require`, which sqlx lets accept that same impostor, is refused for any host that is
//!   not on this machine, before a connection is tried.
//!
//! `RECON_TLS_TEST_URL` and `RECON_TLS_TEST_CA` come from `scripts/stack.sh env`; without
//! them these tests fail loudly rather than pass silently.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

use recon_db::DbError;
use recon_db::pool::{UrlRefusal, checked_options};
use sqlx::{Connection as _, PgConnection};

fn tls_env() -> (String, String) {
    let missing = |name: &str| {
        format!("{name} is not set; run scripts/stack.sh up and eval \"$(scripts/stack.sh env)\"")
    };
    (
        std::env::var("RECON_TLS_TEST_URL")
            .unwrap_or_else(|_| panic!("{}", missing("RECON_TLS_TEST_URL"))),
        std::env::var("RECON_TLS_TEST_CA")
            .unwrap_or_else(|_| panic!("{}", missing("RECON_TLS_TEST_CA"))),
    )
}

async fn connect(url: &str) -> Result<PgConnection, sqlx::Error> {
    let options = checked_options(url).expect("the policy admits this URL");
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        PgConnection::connect_with(&options),
    )
    .await
    .expect("connecting within 20 s")
}

async fn encrypted(conn: &mut PgConnection) -> bool {
    sqlx::query_scalar(
        "SELECT ssl FROM pg_catalog.pg_stat_ssl WHERE pid = pg_catalog.pg_backend_pid()",
    )
    .fetch_one(conn)
    .await
    .unwrap()
}

/// A refused handshake: sqlx reports rustls' certificate errors either as `Tls` or as an
/// I/O error of kind `InvalidData` carrying the certificate error.
fn is_tls_refusal(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Tls(_) => true,
        sqlx::Error::Io(e) => {
            e.kind() == std::io::ErrorKind::InvalidData && e.to_string().contains("certificate")
        }
        _ => false,
    }
}

#[tokio::test]
async fn verify_full_with_the_private_ca_connects_encrypted() {
    let (url, ca) = tls_env();
    let mut conn = connect(&format!("{url}?sslmode=verify-full&sslrootcert={ca}"))
        .await
        .expect("verify-full with the development CA and the certificate's own name");
    assert!(encrypted(&mut conn).await);
    conn.close().await.unwrap();
    // The server's pool takes the same path.
    let pool = recon_db::connect_lazy(
        &format!("{url}?sslmode=verify-full&sslrootcert={ca}"),
        &recon_db::PoolSettings::default(),
    )
    .unwrap();
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(one, 1);
    pool.close().await;
}

#[tokio::test]
async fn verify_full_refuses_a_certificate_for_another_name_or_from_an_unknown_ca() {
    let (url, ca) = tls_env();
    // The certificate names localhost only: reached as 127.0.0.1, its name does not match.
    let wrong_name = url.replacen("@localhost:", "@127.0.0.1:", 1);
    assert_ne!(wrong_name, url, "the test URL names localhost");
    let refused = connect(&format!(
        "{wrong_name}?sslmode=verify-full&sslrootcert={ca}"
    ))
    .await;
    assert!(
        refused.as_ref().is_err_and(is_tls_refusal),
        "a certificate for another name must be refused: {refused:?}"
    );
    // Without the private CA (the public roots only) the chain does not verify.
    let refused = connect(&format!("{url}?sslmode=verify-full")).await;
    assert!(
        refused.as_ref().is_err_and(is_tls_refusal),
        "an unknown CA must be refused: {refused:?}"
    );
}

#[tokio::test]
async fn require_accepts_an_impostor_so_it_is_refused_for_remote_hosts() {
    let (url, _) = tls_env();
    // What the review found: with `require`, the same certificate, reached under a name it
    // does not carry and from a CA nobody configured, is accepted. That is harmless only
    // on this machine (127.0.0.1 is loopback, which the policy admits)...
    let wrong_name = url.replacen("@localhost:", "@127.0.0.1:", 1);
    let mut conn = connect(&format!("{wrong_name}?sslmode=require"))
        .await
        .expect("require accepts any certificate");
    assert!(
        encrypted(&mut conn).await,
        "encrypted, but not authenticated"
    );
    conn.close().await.unwrap();
    // ...so for any other host it is refused before a connection is tried.
    let remote = wrong_name.replacen("@127.0.0.1:", "@db.recon.test:", 1);
    for query in ["?sslmode=require", "", "?sslmode=prefer"] {
        let refused = recon_db::connect_lazy(
            &format!("{remote}{query}"),
            &recon_db::PoolSettings::default(),
        );
        assert!(
            matches!(
                refused,
                Err(DbError::Url(UrlRefusal::TlsUnverified { ref host, .. })) if host == "db.recon.test"
            ),
            "{query}: {refused:?}"
        );
    }
    assert!(matches!(
        checked_options(&format!("{remote}?sslmode=verify-ca")),
        Err(UrlRefusal::VerifyCaWithoutRoot { .. })
    ));
}
