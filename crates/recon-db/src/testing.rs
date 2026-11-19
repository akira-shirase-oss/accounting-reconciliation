//! Test support: one throwaway database per test, set up exactly as production is — roles
//! from [`crate::roles::ensure_roles`], the database owned by `recon_owner` with `PUBLIC`
//! locked out, migrations run as the owner — and a pool connected as `recon_app`.
//!
//! `RECON_TEST_ADMIN_URL` names a role allowed to create roles and databases (the local
//! stack's superuser; `scripts/stack.sh env` prints it). A test that needs a database
//! fails loudly without it rather than passing silently.
#![expect(
    clippy::expect_used,
    reason = "test support: a setup failure must abort the test with its cause"
)]

use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, Executor, PgConnection};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// The variable naming the administrator's connection.
pub const ENV: &str = "RECON_TEST_ADMIN_URL";
/// A password for the owner's URL: never accepted, the owner cannot log in (D-049).
pub const OWNER_PASSWORD: &str = "recon-owner-cannot-log-in";
/// The server's development password.
pub const APP_PASSWORD: &str = "recon-app-dev";

static ROLES: OnceCell<()> = OnceCell::const_new();

fn url_for(admin_url: &str, name: &str, user: &str, password: &str, host: Option<&str>) -> String {
    let (admin_host, query) = host_and_query(admin_url);
    let host = host.unwrap_or(&admin_host);
    let mut url = format!("postgres://{user}:{password}@{host}/{name}");
    if let Some(q) = query {
        url.push('?');
        url.push_str(&q);
    }
    url
}

/// Splits a `postgres://user:pass@host:port/db?query` URL into `host:port` and the query.
fn host_and_query(url: &str) -> (String, Option<String>) {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .unwrap_or(url);
    let after_at = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let (path, query) = match after_at.split_once('?') {
        Some((p, q)) => (p, Some(q.to_owned())),
        None => (after_at, None),
    };
    let host = path.split_once('/').map_or(path, |(h, _)| h);
    (host.to_owned(), query)
}

/// A freshly created and migrated database, dropped by [`TestDb::cleanup`].
#[derive(Debug)]
pub struct TestDb {
    /// A pool on the new database as the runtime role `recon_app`.
    pub pool: PgPool,
    admin_url: String,
    name: String,
}

impl TestDb {
    /// Creates `recon_test_<uuid>`, locks it down, migrates it as the owner.
    pub async fn new() -> Self {
        let admin_url = std::env::var(ENV).unwrap_or_else(|_| {
            panic!("{ENV} is not set; start the stack (scripts/stack.sh up) and eval scripts/stack.sh env")
        });
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("connecting as the administrator (RECON_TEST_ADMIN_URL)");
        if ROLES.get().is_none() {
            // Concurrent callers are serialized by ensure_roles' advisory lock.
            crate::roles::ensure_roles(&mut admin, APP_PASSWORD)
                .await
                .expect("creating the roles");
            let _ = ROLES.set(());
        }
        // The name carries the test (`recon_test_<test>_<uuid8>`) so a database a failed
        // test leaves behind names its owner (the Drop impl keeps failed tests' databases).
        // The uuid tail, not its head: a v7's first bytes are the timestamp — equal for
        // tests run in the same second.
        let name = format!(
            "recon_test_{}_{}",
            test_tag(),
            &Uuid::now_v7().simple().to_string()[24..]
        );
        crate::roles::create_database(&mut admin, &name)
            .await
            .expect("creating the test database");
        admin.close().await.ok();
        let on_database = PgConnectOptions::from_str(&admin_url)
            .expect("invariant: RECON_TEST_ADMIN_URL parses")
            .database(&name);
        crate::migrate(on_database)
            .await
            .expect("migrating the test database as its owner");
        let app_url = url_for(&admin_url, &name, crate::roles::APP, APP_PASSWORD, None);
        let options = crate::pool::connect_options(&app_url, &crate::PoolSettings::default())
            .expect("invariant: the test url parses");
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .expect("connecting as recon_app");
        // The embedded migrator's contents are not a cargo-tracked input, so a
        // stale rlib can carry an old migration list — name it instead of letting
        // a missing migration surface as a planner or schema mystery.
        let (applied, expected) = (
            crate::roles::applied_version(&pool)
                .await
                .expect("reading the migration ledger"),
            crate::roles::expected_version(),
        );
        assert_eq!(
            applied, expected,
            "the embedded migrator is stale — `cargo clean -p recon-db` and rebuild"
        );
        Self {
            pool,
            admin_url,
            name,
        }
    }

    /// The database's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A URL for `user` on this database, at `host` (`host:port`; default the admin's).
    pub fn url_as(&self, user: &str, password: &str, host: Option<&str>) -> String {
        url_for(&self.admin_url, &self.name, user, password, host)
    }

    /// The runtime role's URL (what the server uses).
    pub fn app_url(&self) -> String {
        self.url_as(crate::roles::APP, APP_PASSWORD, None)
    }

    /// The owner's URL (refused: the owner cannot log in; tests prove it).
    pub fn owner_url(&self) -> String {
        self.url_as(crate::roles::OWNER, OWNER_PASSWORD, None)
    }

    /// The administrator on this database (tests that tamper around the guards).
    pub fn admin_url(&self) -> String {
        let (host, query) = host_and_query(&self.admin_url);
        let base = self.admin_url.split_once('@').map_or("", |(b, _)| b);
        let mut url = format!("{base}@{host}/{}", self.name);
        if let Some(q) = query {
            url.push('?');
            url.push_str(&q);
        }
        url
    }

    /// The `host:port` of the server (a test proxy forwards to it).
    pub fn host(&self) -> String {
        host_and_query(&self.admin_url).0
    }

    /// A pool acting as the owner: the administrator with `SET ROLE recon_owner` on every
    /// connection (the owner itself cannot log in).
    pub async fn owner_pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    conn.execute("SET ROLE recon_owner").await?;
                    Ok(())
                })
            })
            .connect(&self.admin_url())
            .await
            .expect("connecting as the administrator acting as recon_owner")
    }

    /// A pool as the administrator.
    pub async fn admin_pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&self.admin_url())
            .await
            .expect("connecting as the administrator")
    }

    /// Drops the database (forcing out any connection a test left behind).
    pub async fn cleanup(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.pool.close()).await;
        if let Ok(mut admin) = PgConnectOptions::from_str(&self.admin_url)
            .expect("invariant: RECON_TEST_ADMIN_URL parses")
            .connect()
            .await
        {
            let _ = admin
                .execute(AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                    self.name
                )))
                .await;
            admin.close().await.ok();
        }
        // The Drop backstop does nothing for a database already dropped.
        self.name.clear();
    }
}

/// The calling test's name, safe for a database identifier: `[a-z0-9_]`, at most 40
/// bytes (`recon_test_` + tag + `_` + the uuid suffix fits PostgreSQL's 63-byte limit).
/// A tokio worker's generic name becomes `test`.
fn test_tag() -> String {
    let raw = std::thread::current().name().unwrap_or("test").to_owned();
    let clean: String = raw
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if matches!(c, ':' | ' ' | '-' | '.' | '/' | '_') {
                Some('_')
            } else {
                None
            }
        })
        .collect();
    let clean = clean.trim_matches('_');
    let keep = if clean.is_empty() || clean.contains("tokio") {
        "test"
    } else {
        clean
    };
    keep.chars().take(40).collect()
}

impl Drop for TestDb {
    /// The backstop behind `cleanup`: a passed test's database is dropped even when the
    /// test forgot (or failed before reaching) `cleanup`; a failed test's database is
    /// kept for diagnosis — its name already names the test.
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        if name.is_empty() {
            return;
        }
        if std::thread::panicking() {
            eprintln!("keeping the failed test's database {name} for diagnosis");
            return;
        }
        let admin_url = std::mem::take(&mut self.admin_url);
        let (done_tx, done) = std::sync::mpsc::channel::<()>();
        // Its own thread and runtime: Drop runs on whatever thread the test ends on,
        // inside or outside a tokio runtime. The DROP's FORCE ends the pool's
        // connections server-side; a stuck drop cannot hang the harness — the join is
        // bounded.
        let spawn = std::thread::Builder::new()
            .name(format!("drop-{name}"))
            .spawn(move || {
                let _ = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|rt| {
                        rt.block_on(async move {
                            if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
                                let _ = admin
                                    .execute(AssertSqlSafe(format!(
                                        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
                                    )))
                                    .await;
                                let _ = admin.close().await;
                            }
                        });
                    });
                let _ = done_tx.send(());
            });
        if spawn.is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::host_and_query;

    #[test]
    fn urls_split_into_host_and_query() {
        assert_eq!(
            host_and_query("postgres://a:b@127.0.0.1:62432/x?sslmode=require"),
            (
                "127.0.0.1:62432".to_owned(),
                Some("sslmode=require".to_owned())
            )
        );
        assert_eq!(
            host_and_query("postgres://a:b@h:1/x"),
            ("h:1".to_owned(), None)
        );
    }
}
