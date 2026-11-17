//! The connection pool: bounded, with deadlines on acquiring a connection and on every
//! statement, lock wait and idle transaction, so a slow or stuck database becomes a fast,
//! honest `503` instead of a pile of stalled requests.
//!
//! TLS is rustls with the webpki roots (D-070). With sqlx, `sslmode=require` encrypts but
//! accepts any certificate for any name, so anyone on the network path could impersonate
//! the database; `verify-ca` checks the chain but not the name. So every URL goes through
//! [`checked_options`]: a database on another machine must use `sslmode=verify-full`, or
//! `verify-ca` with an explicit `sslrootcert`; `channel_binding=require`, which sqlx would
//! silently ignore, is refused rather than trusted. A database on this machine (a loopback
//! address, `localhost` or a Unix socket) never crosses a network and may use any mode.

use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};

use crate::DbError;

/// Why a database URL is refused before any connection is tried (D-070). The message
/// names the host (never the password) and says how to fix the URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlRefusal {
    /// The URL does not parse.
    #[error(
        "database_url_invalid: the database URL does not parse; write it as postgres://USER:PASSWORD@HOST:PORT/DATABASE?sslmode=verify-full"
    )]
    Invalid,
    /// A remote database without certificate verification.
    #[error(
        "database_tls_unverified: the database at {host} is on another machine and sslmode={mode} does not check its certificate, so anyone on the network path could impersonate it; use sslmode=verify-full (add sslrootcert=/path/to/ca.pem when a private CA signs its certificate)"
    )]
    TlsUnverified {
        /// The host.
        host: String,
        /// The mode in force.
        mode: &'static str,
    },
    /// `verify-ca` without an explicit root: any certificate from any public CA would do.
    #[error(
        "database_tls_no_root_certificate: sslmode=verify-ca for the database at {host} checks the certificate chain but not the name, so it is accepted only with an explicit sslrootcert=/path/to/ca.pem; otherwise use sslmode=verify-full"
    )]
    VerifyCaWithoutRoot {
        /// The host.
        host: String,
    },
    /// `channel_binding=require`, which this client does not implement.
    #[error(
        "database_channel_binding_unsupported: channel_binding=require is not implemented by this client and would be ignored; remove it and keep sslmode=verify-full, which authenticates the server"
    )]
    ChannelBindingUnsupported,
}

impl UrlRefusal {
    /// The stable code.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid => "database_url_invalid",
            Self::TlsUnverified { .. } => "database_tls_unverified",
            Self::VerifyCaWithoutRoot { .. } => "database_tls_no_root_certificate",
            Self::ChannelBindingUnsupported => "database_channel_binding_unsupported",
        }
    }
}

const fn mode_name(mode: PgSslMode) -> &'static str {
    match mode {
        PgSslMode::Disable => "disable",
        PgSslMode::Allow => "allow",
        PgSslMode::Prefer => "prefer",
        PgSslMode::Require => "require",
        PgSslMode::VerifyCa => "verify-ca",
        PgSslMode::VerifyFull => "verify-full",
    }
}

/// True for a host that never crosses a network: a loopback address or `localhost`.
fn is_local(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The query parameters of a URL, keys as written.
fn query_params(url: &str) -> impl Iterator<Item = (&str, &str)> {
    url.split_once('?')
        .map_or("", |(_, q)| q.split_once('#').map_or(q, |(q, _)| q))
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
}

/// Parses `url` and applies the TLS policy of D-070 (the module's doc): the options to
/// connect with, or the named refusal.
pub fn checked_options(url: &str) -> Result<PgConnectOptions, UrlRefusal> {
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err(UrlRefusal::Invalid);
    }
    let options = PgConnectOptions::from_str(url).map_err(|_| UrlRefusal::Invalid)?;
    if query_params(url).any(|(k, v)| k == "channel_binding" && v.eq_ignore_ascii_case("require")) {
        return Err(UrlRefusal::ChannelBindingUnsupported);
    }
    if options.get_socket().is_some() || is_local(options.get_host()) {
        return Ok(options);
    }
    let host = options.get_host().to_owned();
    match options.get_ssl_mode() {
        PgSslMode::VerifyFull => Ok(options),
        PgSslMode::VerifyCa => {
            let in_url = query_params(url).any(|(k, v)| {
                matches!(k, "sslrootcert" | "ssl-root-cert" | "ssl-ca") && !v.is_empty()
            });
            let in_env = std::env::var_os("PGSSLROOTCERT").is_some_and(|v| !v.is_empty());
            if in_url || in_env {
                Ok(options)
            } else {
                Err(UrlRefusal::VerifyCaWithoutRoot { host })
            }
        }
        mode => Err(UrlRefusal::TlsUnverified {
            host,
            mode: mode_name(mode),
        }),
    }
}

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

/// Parses `url` under the TLS policy ([`checked_options`]) and applies the deadlines as
/// session settings.
pub fn connect_options(url: &str, settings: &PoolSettings) -> Result<PgConnectOptions, DbError> {
    let options = checked_options(url)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(url: &str) -> Option<&'static str> {
        checked_options(url).err().map(|r| r.code())
    }

    /// D-070: a remote database needs `verify-full` (or `verify-ca` with a root); a local
    /// one may use any mode; `channel_binding=require` is refused, not ignored.
    #[test]
    fn remote_databases_need_verified_tls() {
        let remote = "postgres://u:secret-pw@db.example.com:5432/recon";
        for (query, expected) in [
            ("?sslmode=require", Some("database_tls_unverified")),
            ("?sslmode=prefer", Some("database_tls_unverified")),
            ("?sslmode=disable", Some("database_tls_unverified")),
            ("", Some("database_tls_unverified")),
            (
                "?sslmode=verify-ca",
                Some("database_tls_no_root_certificate"),
            ),
            ("?sslmode=verify-ca&sslrootcert=/etc/recon/db-ca.pem", None),
            ("?sslmode=verify-full", None),
            (
                "?sslmode=verify-full&sslrootcert=/etc/recon/db-ca.pem",
                None,
            ),
            (
                "?sslmode=verify-full&channel_binding=require",
                Some("database_channel_binding_unsupported"),
            ),
            ("?sslmode=verify-full&channel_binding=prefer", None),
            // A `host` parameter overrides the URL's host: it is what is checked.
            ("?host=127.0.0.1&sslmode=require", None),
            (
                "?hostaddr=10.0.0.5&sslmode=require",
                Some("database_tls_unverified"),
            ),
        ] {
            assert_eq!(refusal(&format!("{remote}{query}")), expected, "{query}");
        }
        for local in [
            "postgres://u:p@127.0.0.1:62432/recon?sslmode=require",
            "postgres://u:p@127.8.9.10/recon?sslmode=disable",
            "postgres://u:p@localhost:5432/recon",
            "postgres://u:p@LOCALHOST/recon?sslmode=prefer",
            "postgres://u:p@[::1]:5432/recon?sslmode=require",
            "postgres://u:p@%2Frun%2Fpostgresql/recon",
            "postgres://u:p@db.example.com/recon?host=/run/postgresql",
        ] {
            assert_eq!(refusal(local), None, "{local}");
        }
        for invalid in ["mysql://u:p@h/db", "postgres://u:p@h:notaport/db", ""] {
            assert_eq!(refusal(invalid), Some("database_url_invalid"), "{invalid}");
        }
        let message = checked_options(&format!("{remote}?sslmode=require"))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("db.example.com")
                && message.contains("verify-full")
                && !message.contains("secret-pw"),
            "{message}"
        );
    }
}
