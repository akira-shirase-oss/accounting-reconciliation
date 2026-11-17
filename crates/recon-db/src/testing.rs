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
/// The owner's development password (the stack's public value).
pub const OWNER_PASSWORD: &str = "recon-owner-dev";
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
            crate::roles::ensure_roles(&mut admin, OWNER_PASSWORD, APP_PASSWORD)
                .await
                .expect("creating the roles");
            let _ = ROLES.set(());
        }
        let name = format!("recon_test_{}", Uuid::now_v7().simple());
        crate::roles::create_database(&mut admin, &name)
            .await
            .expect("creating the test database");
        admin.close().await.ok();
        let owner_url = url_for(&admin_url, &name, crate::roles::OWNER, OWNER_PASSWORD, None);
        let mut owner = PgConnection::connect(&owner_url)
            .await
            .expect("connecting as recon_owner");
        crate::migrator()
            .run(&mut owner)
            .await
            .expect("migrating the test database as its owner");
        owner.close().await.ok();
        let app_url = url_for(&admin_url, &name, crate::roles::APP, APP_PASSWORD, None);
        let options = crate::pool::connect_options(&app_url, &crate::PoolSettings::default())
            .expect("invariant: the test url parses");
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .expect("connecting as recon_app");
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

    /// The owner's URL (migrations only).
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

    /// A pool as the owner.
    pub async fn owner_pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .connect(&self.owner_url())
            .await
            .expect("connecting as recon_owner")
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
    pub async fn cleanup(self) {
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
