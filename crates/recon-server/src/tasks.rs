//! Background work: the startup check, and the sweeper that discards abandoned staging
//! imports. Both stop on the shutdown token.

use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

/// Runs the startup check (D-037) until the database answers: a clean result marks the
/// server ready; any problem is sent to `fatal` and the server refuses to serve.
pub async fn startup_check(
    state: AppState,
    fatal: oneshot::Sender<Vec<String>>,
    shutdown: CancellationToken,
) {
    loop {
        let attempt = tokio::time::timeout(
            Duration::from_secs(5),
            recon_db::roles::check_runtime(&state.pool),
        );
        let result = tokio::select! {
            () = shutdown.cancelled() => return,
            r = attempt => r,
        };
        match result {
            Ok(Ok(problems)) if problems.is_empty() => {
                state.mark_checked();
                tracing::info!(schema = ?recon_db::roles::expected_version(), "startup check passed");
                return;
            }
            Ok(Ok(problems)) => {
                let _ = fatal.send(problems.into_iter().map(|p| p.0).collect());
                return;
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "startup check waiting for the database"),
            Err(_) => tracing::warn!("startup check timed out; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

/// How often the readiness probe reaches for the database, and how long it waits (D-051).
pub const READINESS_INTERVAL: Duration = Duration::from_millis(500);
/// The probe's deadline.
pub const READINESS_TIMEOUT: Duration = Duration::from_millis(800);
/// `/readyz` is red once the last successful probe is older than this (inside the 2 s the
/// `db-down` drill requires).
pub const READINESS_WINDOW: Duration = Duration::from_millis(1_500);

/// Probes the database every [`READINESS_INTERVAL`]: one query per interval however many
/// `/readyz` requests arrive.
pub async fn readiness_probe(state: AppState, shutdown: CancellationToken) {
    loop {
        let ping = tokio::time::timeout(
            READINESS_TIMEOUT,
            sqlx::query("SELECT 1").execute(&state.pool),
        );
        let reached = tokio::select! {
            () = shutdown.cancelled() => return,
            r = ping => matches!(r, Ok(Ok(_))),
        };
        if reached {
            state.record_db_ok();
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(READINESS_INTERVAL) => {}
        }
    }
}

/// Every `sweep_interval`: discard staging imports whose lease expired, delete staged rows
/// of finished imports. Errors are logged and the next round retries.
pub async fn sweeper(state: AppState, shutdown: CancellationToken) {
    let interval = state.config.sweep_interval;
    loop {
        if state.startup_checked() {
            let round = tokio::time::timeout(
                Duration::from_secs(30),
                recon_db::imports::sweep(&state.pool, 100, 50_000),
            );
            match round.await {
                Ok(Ok(report)) if report.discarded > 0 || report.staging_rows_deleted > 0 => {
                    tracing::info!(
                        discarded = report.discarded,
                        staged_rows_deleted = report.staging_rows_deleted,
                        "sweep"
                    );
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "sweep failed; retrying next round"),
                Err(_) => tracing::warn!("sweep timed out; retrying next round"),
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
    }
}
