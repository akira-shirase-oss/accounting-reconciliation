//! Configuration from the environment, loaded once and validated with every problem
//! reported together. Secrets (the database URL carries a password; the operator token) are
//! never printed: `Debug` redacts them.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use recon_db::PoolSettings;

/// The operator token: the R0 stand-in for sessions (D-041). Compared in constant time and
/// never logged.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// The bytes, for comparison only.
    pub fn expose(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(redacted)")
    }
}

/// The server's settings.
#[derive(Clone)]
pub struct Config {
    /// The runtime role's connection URL (never logged).
    pub database_url: String,
    /// Where to listen (loopback by default).
    pub listen: SocketAddr,
    /// The operator token.
    pub operator_token: Secret,
    /// Pool size and per-statement deadlines.
    pub pool: PoolSettings,
    /// Most imports in flight; more are refused with `503`, never queued.
    pub import_concurrency: usize,
    /// The staging lease, renewed by every staged batch.
    pub import_lease: Duration,
    /// Longest a parse may take (D-010: 60 s).
    pub parse_deadline: Duration,
    /// Longest the upload may pause between chunks.
    pub body_idle_timeout: Duration,
    /// Statement deadline inside the import flip's transaction.
    pub flip_timeout: Duration,
    /// Overall deadline for any request.
    pub request_timeout: Duration,
    /// How often the sweeper discards expired staging imports.
    pub sweep_interval: Duration,
    /// How long shutdown waits for requests in flight.
    pub drain_timeout: Duration,
    /// Longest a connection may take to send a request head, and longest it may sit idle
    /// between requests (D-047).
    pub header_read_timeout: Duration,
    /// Most open connections; more are closed at once (D-047).
    pub max_connections: usize,
    /// Most requests in flight; more are shed with `503 overloaded` (D-047).
    pub max_in_flight: usize,
    /// Bytes of parsed rows all uploads together may hold before staging (D-050).
    pub staging_budget_bytes: usize,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &"redacted")
            .field("listen", &self.listen)
            .field("operator_token", &self.operator_token)
            .field("pool", &self.pool)
            .field("import_concurrency", &self.import_concurrency)
            .field("import_lease", &self.import_lease)
            .field("parse_deadline", &self.parse_deadline)
            .field("body_idle_timeout", &self.body_idle_timeout)
            .field("flip_timeout", &self.flip_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("sweep_interval", &self.sweep_interval)
            .field("drain_timeout", &self.drain_timeout)
            .field("header_read_timeout", &self.header_read_timeout)
            .field("max_connections", &self.max_connections)
            .field("max_in_flight", &self.max_in_flight)
            .field("staging_budget_bytes", &self.staging_budget_bytes)
            .finish()
    }
}

/// Every configuration problem found.
#[derive(Debug, thiserror::Error)]
#[error("invalid configuration: {}", .0.join("; "))]
pub struct ConfigError(pub Vec<String>);

/// The default listen address: loopback, in the project's port range (D-036).
pub const DEFAULT_LISTEN: &str = "127.0.0.1:62480";

impl Config {
    /// Reads `RECON_*` variables through `get` (the environment in production).
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut problems = vec![];
        let mut millis = |name: &str, default: u64, min: u64, max: u64| -> Duration {
            match get(name).map(|v| v.parse::<u64>()) {
                None => Duration::from_millis(default),
                Some(Ok(v)) if (min..=max).contains(&v) => Duration::from_millis(v),
                Some(_) => {
                    problems.push(format!("{name} must be milliseconds in {min}..={max}"));
                    Duration::from_millis(default)
                }
            }
        };
        let import_lease = millis("RECON_IMPORT_LEASE_MS", 30_000, 100, 600_000);
        let parse_deadline = millis("RECON_PARSE_DEADLINE_MS", 60_000, 100, 600_000);
        let body_idle_timeout = millis("RECON_BODY_IDLE_MS", 15_000, 100, 120_000);
        let flip_timeout = millis("RECON_FLIP_TIMEOUT_MS", 60_000, 1_000, 600_000);
        let request_timeout = millis("RECON_REQUEST_TIMEOUT_MS", 150_000, 1_000, 900_000);
        let sweep_interval = millis("RECON_SWEEP_INTERVAL_MS", 10_000, 50, 3_600_000);
        let drain_timeout = millis("RECON_DRAIN_MS", 30_000, 0, 600_000);
        let statement_timeout = millis("RECON_STATEMENT_TIMEOUT_MS", 10_000, 100, 600_000);
        let acquire_timeout = millis("RECON_DB_ACQUIRE_MS", 1_500, 50, 60_000);
        let header_read_timeout = millis("RECON_HEADER_READ_TIMEOUT_MS", 10_000, 100, 120_000);

        let database_url = get("RECON_DATABASE_URL").unwrap_or_default();
        if database_url.is_empty() {
            problems
                .push("RECON_DATABASE_URL is required (the runtime role, not the owner)".into());
        }
        let token = get("RECON_OPERATOR_TOKEN").unwrap_or_default();
        if token.len() < 32 || token.bytes().any(|b| !b.is_ascii_graphic()) {
            problems
                .push("RECON_OPERATOR_TOKEN must be at least 32 printable ASCII characters".into());
        }
        let listen = get("RECON_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.into());
        let listen = listen.parse::<SocketAddr>().unwrap_or_else(|_| {
            problems.push("RECON_LISTEN must be an address such as 127.0.0.1:62480".into());
            ([127, 0, 0, 1], 62_480).into()
        });
        let count = |name: &str, default: u32, max: u32, problems: &mut Vec<String>| -> u32 {
            match get(name).map(|v| v.parse::<u32>()) {
                None => default,
                Some(Ok(v)) if (1..=max).contains(&v) => v,
                Some(_) => {
                    problems.push(format!("{name} must be in 1..={max}"));
                    default
                }
            }
        };
        let import_concurrency = count("RECON_IMPORT_CONCURRENCY", 4, 64, &mut problems);
        let max_connections = count("RECON_DB_MAX_CONNECTIONS", 32, 256, &mut problems);
        let max_open_connections = count("RECON_MAX_CONNECTIONS", 512, 65_536, &mut problems);
        let max_in_flight = count("RECON_MAX_IN_FLIGHT", 256, 4_096, &mut problems);
        // At least two batches' worth, so one upload can always make progress.
        let staging_budget_mib = count("RECON_STAGING_BUDGET_MIB", 64, 4_096, &mut problems);
        if staging_budget_mib < 8 {
            problems.push("RECON_STAGING_BUDGET_MIB must be at least 8".into());
        }
        if get("RECON_FAULT").is_some() && !cfg!(feature = "fault-injection") {
            problems.push("RECON_FAULT is set but this build has no fault injection".into());
        }
        if !problems.is_empty() {
            return Err(ConfigError(problems));
        }
        Ok(Self {
            database_url,
            listen,
            operator_token: Secret(token),
            pool: PoolSettings {
                max_connections,
                acquire_timeout,
                statement_timeout,
                lock_timeout: Duration::from_secs(5),
            },
            import_concurrency: usize::try_from(import_concurrency).unwrap_or(4),
            import_lease,
            parse_deadline,
            body_idle_timeout,
            flip_timeout,
            request_timeout,
            sweep_interval,
            drain_timeout,
            header_read_timeout,
            max_connections: usize::try_from(max_open_connections).unwrap_or(512),
            max_in_flight: usize::try_from(max_in_flight).unwrap_or(256),
            staging_budget_bytes: usize::try_from(staging_budget_mib)
                .unwrap_or(64)
                .saturating_mul(1024 * 1024),
        })
    }

    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|name| std::env::var(name).ok())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn every_problem_is_reported_and_secrets_never_print() {
        let err = Config::from_source(|name| match name {
            "RECON_OPERATOR_TOKEN" => Some("short".into()),
            "RECON_LISTEN" => Some("nowhere".into()),
            "RECON_IMPORT_CONCURRENCY" => Some("0".into()),
            _ => None,
        })
        .unwrap_err();
        assert_eq!(err.0.len(), 4, "{err}");
        let config = Config::from_source(|name| match name {
            "RECON_DATABASE_URL" => {
                Some("postgres://recon_app:hunter2@127.0.0.1:62432/recon".into())
            }
            "RECON_OPERATOR_TOKEN" => Some("x".repeat(40)),
            _ => None,
        })
        .unwrap();
        let printed = format!("{config:?}");
        assert!(
            !printed.contains("hunter2") && !printed.contains(&"x".repeat(40)),
            "{printed}"
        );
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
    }
}
