//! Shared state, built once at startup.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::faults::Faults;

/// Everything a handler needs.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The runtime role's pool.
    pub pool: PgPool,
    /// Settings.
    pub config: Arc<Config>,
    /// One permit per import in flight; none left means `503`, never a queue.
    pub import_permits: Arc<Semaphore>,
    /// The staging budget in KiB permits (D-050): parsed rows waiting to be stored, across
    /// every upload.
    pub staging_budget: Arc<Semaphore>,
    /// Fault points (drills only).
    pub faults: Arc<Faults>,
    checked: Arc<AtomicBool>,
    /// When the readiness probe last reached the database, in milliseconds since `born`
    /// (0: never). `/readyz` reads this instead of querying (D-051).
    db_ok_at: Arc<AtomicU64>,
    born: Instant,
}

impl AppState {
    /// The state for `config`.
    pub fn new(pool: PgPool, config: Config, faults: Faults) -> Self {
        Self {
            pool,
            import_permits: Arc::new(Semaphore::new(config.import_concurrency)),
            staging_budget: Arc::new(Semaphore::new(config.staging_budget_bytes / 1024)),
            config: Arc::new(config),
            faults: Arc::new(faults),
            checked: Arc::new(AtomicBool::new(false)),
            db_ok_at: Arc::new(AtomicU64::new(0)),
            born: Instant::now(),
        }
    }

    /// True once the startup check passed (safe role, this build's schema).
    pub fn startup_checked(&self) -> bool {
        self.checked.load(Ordering::Acquire)
    }

    /// Records that the startup check passed.
    pub fn mark_checked(&self) {
        self.checked.store(true, Ordering::Release);
    }

    fn since_born(&self) -> u64 {
        u64::try_from(self.born.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    /// Records that the database answered the readiness probe now.
    pub fn record_db_ok(&self) {
        self.db_ok_at.store(self.since_born(), Ordering::Release);
    }

    /// True when the probe reached the database within `window`.
    pub fn db_ok_within(&self, window: Duration) -> bool {
        let at = self.db_ok_at.load(Ordering::Acquire);
        let window = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        at != 0 && self.since_born().saturating_sub(at) <= window
    }
}
