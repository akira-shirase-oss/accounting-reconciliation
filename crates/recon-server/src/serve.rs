//! The accept loop (D-047). `axum::serve` builds hyper without a timer, so hyper's
//! header-read timeout never runs and a client can hold a connection forever by trickling
//! headers or saying nothing. Here every connection is served by hyper's HTTP/1 builder with
//! a timer and a header-read timeout — which also bounds the idle time between requests on a
//! keep-alive connection, because the timer is armed whenever the connection waits for a
//! request head — and holds one permit of a connection semaphore: with none left the new
//! socket is closed at once, never queued. Shutdown stops accepting and drains open
//! connections up to a deadline.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Connection bounds.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// Longest a request head may take, and longest a connection may idle between requests.
    pub header_read_timeout: Duration,
    /// Most open connections.
    pub max_connections: usize,
    /// How long shutdown waits for open connections.
    pub drain_timeout: Duration,
}

/// Serves `app` on `listener` until `shutdown`, then drains.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    limits: ConnectionLimits,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let graceful = GracefulShutdown::new();
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout)
        .keep_alive(true)
        .max_buf_size(64 * 1024);
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok((stream, _peer)) => stream,
            Err(e) => {
                // Out of file descriptors and the like: back off instead of spinning.
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::debug!("connection limit reached; closing a new connection");
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let service = TowerToHyperService::new(app.clone());
        let connection = graceful.watch(http.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            let _ = connection.await;
            drop(permit);
        });
    }
    drop(listener);
    if tokio::time::timeout(limits.drain_timeout, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("drain deadline passed; closing with connections open");
    }
}
