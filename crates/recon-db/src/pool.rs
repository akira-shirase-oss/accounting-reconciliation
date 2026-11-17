//! The connection pool: bounded, with deadlines on acquiring a connection and on every
//! statement, lock wait and idle transaction, so a slow or stuck database becomes a fast,
//! honest `503` instead of a pile of stalled requests.
//!
//! URLs may carry `sslmode` (`require`, `verify-ca`, `verify-full`): TLS is rustls with the
//! webpki roots, so a Neon-style `?sslmode=require` URL works without a system OpenSSL.
//! Production should use `verify-full`.

use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::DbError;

/// Pool limits and per-statement deadlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolSettings {
    /// Most open connections.
    pub max_connections: u32,
    /// Longest wait for a free connection (or a new one).
    pub acquire_timeout: Duration,
    /// Longest any single statement may run (`statement_timeout`); the import flip raises
    /// it for its own transaction.
    pub statement_timeout: Duration,
    /// Longest any statement may wait for a lock (`lock_timeout`).
    pub lock_timeout: Duration,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            max_connections: 32,
            acquire_timeout: Duration::from_millis(1_500),
            statement_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(5),
        }
    }
}

/// Parses `url` and applies the deadlines as session settings.
pub fn connect_options(url: &str, settings: &PoolSettings) -> Result<PgConnectOptions, DbError> {
    let options = PgConnectOptions::from_str(url).map_err(DbError::Other)?;
    Ok(options
        .application_name("accounting-reconciliation")
        .options([
            (
                "statement_timeout",
                format!("{}ms", settings.statement_timeout.as_millis()),
            ),
            (
                "lock_timeout",
                format!("{}ms", settings.lock_timeout.as_millis()),
            ),
            // A connection left idle inside a transaction (a crashed or stalled handler) is
            // cut off instead of holding its locks.
            ("idle_in_transaction_session_timeout", "30000ms".to_owned()),
        ]))
}

/// Builds the pool without connecting, so the server starts (and reports not-ready) while
/// the database is down, and recovers by itself when it comes back.
pub fn connect_lazy(url: &str, settings: &PoolSettings) -> Result<PgPool, DbError> {
    let options = connect_options(url, settings)?;
    Ok(PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .acquire_timeout(settings.acquire_timeout)
        .idle_timeout(Some(Duration::from_secs(300)))
        // A connection is checked before reuse, so one broken by an outage is replaced
        // instead of failing a request.
        .test_before_acquire(true)
        .connect_lazy_with(options))
}
