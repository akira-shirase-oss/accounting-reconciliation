//! The Accounting Reconciliation API server (ARCHITECTURE: `recon-server`). R0 serves the
//! import spine: firms, clients and accounts; streaming statement imports through staging;
//! liveness and readiness. Sessions, roles and the review API arrive in R3; until then an
//! operator token guards `/v1` (D-041).

pub mod config;
pub mod error;
pub mod faults;
pub mod handlers;
pub mod serve;
pub mod state;
pub mod tasks;
pub mod telemetry;
pub mod upload;

use std::time::Duration;

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use subtle::ConstantTimeEq as _;
use tower::ServiceBuilder;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use config::Config;
pub use state::AppState;

use crate::error::ApiError;

/// JSON bodies are tiny; uploads have their own bound (`upload.rs`).
const JSON_BODY_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
struct MakeId;

impl MakeRequestId for MakeId {
    fn make_request_id<B>(&mut self, _request: &axum::http::Request<B>) -> Option<RequestId> {
        HeaderValue::from_str(&uuid::Uuid::now_v7().to_string())
            .ok()
            .map(RequestId::new)
    }
}

/// A request beyond the in-flight limit, shed at once (D-047).
async fn overloaded(_error: tower::BoxError) -> Response {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "overloaded",
        "the server is at its request limit; nothing was accepted, retry shortly",
    )
    .retry_after(1)
    .into_response()
}

fn panic_response(_panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("a handler panicked");
    ApiError::internal().into_response()
}

/// `/v1` requires `Authorization: Bearer <operator token>` (constant-time comparison).
async fn require_operator(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let expected = state.config.operator_token.expose();
    if bool::from(presented.as_bytes().ct_eq(expected)) {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

/// The application with its middleware, outermost first: request id → propagate it →
/// trace (method, route, status, latency; never headers or bodies) → catch panics into the
/// envelope → the overall deadline → shed requests beyond the in-flight limit → body limits.
pub fn router(state: AppState) -> Router {
    let request_timeout: Duration = state.config.request_timeout;
    let max_in_flight = state.config.max_in_flight;
    let v1 = Router::new()
        .route("/v1/firms", post(handlers::create_firm))
        .route("/v1/firms/{firm}/clients", post(handlers::create_client))
        .route(
            "/v1/firms/{firm}/clients/{client}/accounts",
            post(handlers::create_account),
        )
        .route(
            "/v1/firms/{firm}/accounts/{account}/imports",
            post(handlers::upload_statement).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/firms/{firm}/imports/{import}",
            get(handlers::get_import),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_operator,
        ));
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .merge(v1)
        .fallback(handlers::fallback)
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeId))
                .layer(PropagateRequestIdLayer::x_request_id())
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|request: &Request| {
                            let route = request
                                .extensions()
                                .get::<MatchedPath>()
                                .map_or("unmatched", MatchedPath::as_str)
                                .to_owned();
                            let id = request
                                .headers()
                                .get("x-request-id")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_owned();
                            tracing::info_span!("request", method = %request.method(), route, request_id = id)
                        })
                        .on_request(())
                        .on_response(
                            |response: &axum::http::Response<_>, latency: Duration, _span: &tracing::Span| {
                                tracing::info!(status = response.status().as_u16(), ms = latency.as_millis(), "response");
                            },
                        )
                        .on_failure(()),
                )
                .layer(CatchPanicLayer::custom(panic_response))
                .layer(TimeoutLayer::with_status_code(StatusCode::SERVICE_UNAVAILABLE, request_timeout))
                .layer(HandleErrorLayer::new(overloaded))
                .load_shed()
                // One semaphore for the whole router: Router::layer instantiates the layer
                // per route, and a per-route limit would not bound the server.
                .layer(GlobalConcurrencyLimitLayer::new(max_in_flight)),
        )
        .with_state(state)
}
