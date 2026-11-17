//! Shared state, built once at startup.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
}
