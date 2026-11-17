//! The accept loop (D-047, D-058). `axum::serve` builds hyper without a timer, so hyper's
//! header-read timeout never runs. Here every connection is served by hyper's HTTP/1 builder
//! with a timer and these bounds:
//!
//! * a header-read timeout, which also bounds the idle time between requests on a
//!   keep-alive connection (the timer is armed whenever the connection waits for a head);
//! * a write-stall deadline: a response write or flush that makes no progress for it fails
//!   and closes the connection — the header timer does not run while a write is pending, so
//!   a client that sends requests and never reads the answers would otherwise hold its slot;
//! * a cap on requests per connection: the limiting response carries `Connection: close`,
//!   so pipelining cannot keep one connection busy forever;
//! * a lifetime: past it the connection finishes the request in flight (within the request
//!   deadline) and closes — a client making slow but steady progress cannot stay for ever;
//! * one permit of a connection semaphore: with none left a new socket is closed at once.
//!
//! Shutdown stops accepting, asks every connection to finish, and waits up to a deadline.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Connection bounds.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// Longest a request head may take, and longest a connection may idle between requests.
    pub header_read_timeout: Duration,
    /// Most open connections.
    pub max_connections: usize,
    /// How long shutdown waits for open connections.
    pub drain_timeout: Duration,
    /// Longest a write may make no progress.
    pub write_stall: Duration,
    /// Most requests one connection serves.
    pub max_requests_per_connection: usize,
    /// Longest a connection lives before it is asked to finish.
    pub max_age: Duration,
    /// How long a connection past its age may take to finish its request in flight.
    pub request_grace: Duration,
}

/// An IO that fails a write or flush making no progress for `stall`.
struct StallGuard<T> {
    inner: T,
    stall: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<T> StallGuard<T> {
    const fn new(inner: T, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            stalled: None,
        }
    }

    /// Called when a write is pending: arms the deadline, and reports when it has passed.
    fn stalled_out(&mut self, cx: &mut Context<'_>) -> Option<io::Error> {
        let stall = self.stall;
        let timer = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(stall)));
        timer
            .as_mut()
            .poll(cx)
            .is_ready()
            .then(|| io::Error::new(io::ErrorKind::TimedOut, "response write made no progress"))
    }

    fn guard<R>(
        &mut self,
        cx: &mut Context<'_>,
        polled: Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        match polled {
            Poll::Ready(result) => {
                self.stalled = None;
                Poll::Ready(result)
            }
            Poll::Pending => match self.stalled_out(cx) {
                Some(e) => Poll::Ready(Err(e)),
                None => Poll::Pending,
            },
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for StallGuard<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for StallGuard<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.guard(cx, polled)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.guard(cx, polled)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_flush(cx);
        this.guard(cx, polled)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.guard(cx, polled)
    }
}

/// Serves `app` on `listener` until `shutdown`, then drains.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    limits: ConnectionLimits,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let connections = TaskTracker::new();
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
        // Count this connection's answers; the limiting one says Connection: close.
        let served = Arc::new(AtomicUsize::new(0));
        let cap = limits.max_requests_per_connection;
        let service = tower::ServiceExt::<axum::http::Request<hyper::body::Incoming>>::map_response(
            app.clone(),
            move |mut response: Response| {
                if served.fetch_add(1, Ordering::Relaxed).saturating_add(1) >= cap {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }
                response
            },
        );
        let io = TokioIo::new(StallGuard::new(stream, limits.write_stall));
        let connection = http.serve_connection(io, TowerToHyperService::new(service));
        let shutdown = shutdown.clone();
        connections.spawn(async move {
            tokio::pin!(connection);
            let aged = tokio::time::sleep(limits.max_age);
            tokio::pin!(aged);
            tokio::select! {
                _ = connection.as_mut() => {}
                () = &mut aged => {
                    connection.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(limits.request_grace, connection.as_mut()).await;
                }
                () = shutdown.cancelled() => {
                    connection.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(limits.drain_timeout, connection.as_mut()).await;
                }
            }
            drop(permit);
        });
    }
    drop(listener);
    connections.close();
    if tokio::time::timeout(limits.drain_timeout, connections.wait())
        .await
        .is_err()
    {
        tracing::warn!("drain deadline passed; closing with connections open");
    }
}
