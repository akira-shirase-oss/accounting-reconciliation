//! `recon-server`: load config, connect lazily, check the role, serve, drain.

use std::process::ExitCode;

use anyhow::Context as _;
use recon_server::serve::{ConnectionLimits, serve};
use recon_server::{AppState, Config, faults::Faults, router, tasks, telemetry};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// The exit code for a configuration the server refuses to run with (`EX_CONFIG`).
const EX_CONFIG: u8 = 78;

#[tokio::main]
async fn main() -> ExitCode {
    telemetry::init();
    match run().await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "server failed");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = ctrl_c.await;
            return;
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

async fn run() -> anyhow::Result<ExitCode> {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "refusing to start");
            return Ok(ExitCode::from(EX_CONFIG));
        }
    };
    let faults = match Faults::from_env() {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "refusing to start");
            return Ok(ExitCode::from(EX_CONFIG));
        }
    };
    let pool = recon_db::connect_lazy(&config.database_url, &config.pool)
        .context("configuring the pool")?;
    let listener = TcpListener::bind(config.listen)
        .await
        .context("binding the listen address")?;
    let drain = config.drain_timeout;
    let limits = ConnectionLimits {
        header_read_timeout: config.header_read_timeout,
        max_connections: config.max_connections,
        drain_timeout: config.drain_timeout,
        write_stall: config.write_stall,
        max_requests_per_connection: config.max_requests_per_connection,
        max_age: config.connection_max_age,
        request_grace: config.request_timeout,
    };
    let mut state = AppState::new(pool.clone(), config, faults);
    tracing::info!(listen = %listener.local_addr()?, "listening");

    let shutdown = CancellationToken::new();
    state.shutdown = shutdown.child_token();
    let (fatal_tx, fatal_rx) = oneshot::channel();
    let mut tasks = JoinSet::new();
    tasks.spawn(tasks::startup_check(
        state.clone(),
        fatal_tx,
        shutdown.clone(),
    ));
    tasks.spawn(tasks::sweeper(state.clone(), shutdown.clone()));
    tasks.spawn(tasks::readiness_probe(state.clone(), shutdown.clone()));
    let app = router(state);
    let mut server = tokio::spawn(serve(listener, app, limits, shutdown.clone()));

    let mut code = ExitCode::SUCCESS;
    tokio::select! {
        () = shutdown_signal() => tracing::info!("shutting down"),
        Ok(problems) = fatal_rx => {
            for problem in &problems {
                tracing::error!(problem = %problem, "unsafe database role or schema");
            }
            tracing::error!("refusing to serve (D-037)");
            code = ExitCode::from(EX_CONFIG);
        }
        result = &mut server => {
            tracing::error!(?result, "the server stopped");
            code = ExitCode::FAILURE;
        }
    }
    shutdown.cancel();
    // serve() drains connections up to the deadline itself; this bounds it once more.
    if tokio::time::timeout(
        drain.saturating_add(std::time::Duration::from_secs(1)),
        &mut server,
    )
    .await
    .is_err()
    {
        tracing::warn!("drain deadline passed; closing with requests in flight");
        server.abort();
    }
    tasks.shutdown().await;
    pool.close().await;
    Ok(code)
}
