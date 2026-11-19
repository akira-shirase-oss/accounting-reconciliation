//! The Accounting Reconciliation API server (ARCHITECTURE: `recon-server`). It serves firms,
//! clients and accounts; import profiles; streaming statement (CSV and OFX) and document
//! imports through staging; held revisions and their accept or reject; liveness and
//! readiness. Sessions, roles and the review API arrive in R3; until then an operator token
//! guards every `/v1` route (D-041).

pub mod auth;
pub mod config;
pub mod error;
pub mod faults;
pub mod handlers;
pub mod matching;
pub mod profiles;
pub mod review;
pub mod serve;
pub mod state;
pub mod tasks;
pub mod telemetry;
pub mod upload;

use std::time::Duration;

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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

/// A request beyond the in-flight limit, shed at once (D-047, D-075).
async fn overloaded(_error: tower::BoxError) -> Response {
    ApiError::overloaded().into_response()
}

fn panic_response(_panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("a handler panicked");
    ApiError::internal().into_response()
}

/// Drops any client-supplied `x-request-id`, so the id in the log is always the server's
/// (D-055).
async fn strip_request_id(mut request: Request) -> Request {
    request.headers_mut().remove("x-request-id");
    request
}

/// `/v1` opens to either credential — the operator token or a session — through
/// `auth::require_actor`; each handler's `authorize` call names the `Act` the role
/// matrix demands (D-018, D-021).
/// The three auth gates (D-021): sign-in is public; `Session` routes need a signed-in
/// person; `actor` routes open to either credential and the handler decides what the actor
/// may do (an owner manages users).
fn auth_routes(state: &AppState) -> Router<AppState> {
    let sign_in = Router::new()
        .route("/v1/auth/sign-in", post(auth::sign_in))
        .route("/v1/auth/totp", post(auth::totp))
        .route("/v1/auth/sign-out", post(auth::sign_out));
    // Session-only routes (the session middleware adds the CSRF check on mutations).
    let session = Router::new()
        .route("/v1/auth/session", get(auth::whoami))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));
    let actor = Router::new()
        .route("/v1/firms/{firm}/users", post(auth::create_user))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_actor,
        ));
    Router::<AppState>::new()
        .merge(sign_in)
        .merge(session)
        .merge(actor)
}

/// The application with its middleware, outermost first: drop the client's request id →
/// set the server's → propagate it →
/// trace (method, route, status, latency; never headers or bodies) → catch panics into the
/// envelope → the overall deadline → shed requests beyond the in-flight limit → body limits.
#[expect(
    clippy::too_many_lines,
    reason = "the route table is one list; splitting it hides which middleware each route sits under"
)]
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
            "/v1/firms/{firm}/clients/{client}/document-imports",
            post(handlers::upload_documents).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/firms/{firm}/imports/{import}",
            get(handlers::get_import),
        )
        .route(
            "/v1/firms/{firm}/imports/{import}/changes",
            get(handlers::revision_changes),
        )
        .route(
            "/v1/firms/{firm}/imports/{import}/accept",
            post(handlers::accept_revision),
        )
        .route(
            "/v1/firms/{firm}/imports/{import}/reject",
            post(handlers::reject_revision),
        )
        .route(
            "/v1/firms/{firm}/clients/{client}/matching-jobs",
            post(handlers::start_matching_job),
        )
        .route(
            "/v1/firms/{firm}/clients/{client}/matching-jobs/{job}",
            get(handlers::get_matching_job),
        )
        .route("/v1/firms/{firm}/profiles", post(handlers::create_profile))
        .route(
            "/v1/firms/{firm}/profiles/{name}",
            get(handlers::get_profile),
        )
        .route(
            "/v1/firms/{firm}/clients/{client}/review-queue",
            get(review::queue),
        )
        .route(
            "/v1/firms/{firm}/clients/{client}/matches/{match}/approve",
            post(review::approve),
        )
        .route(
            "/v1/firms/{firm}/clients/{client}/matches/{match}/reject",
            post(review::reject),
        )
        .route(
            "/v1/firms/{firm}/clients/{client}/lines/{line}/exclude",
            post(review::exclude),
        )
        .route(
            "/v1/firms/{firm}/clients/{client}/lines/{line}/manual-match",
            post(review::manual_match),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_actor,
        ));
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .merge(v1)
        .merge(auth_routes(&state))
        .fallback(handlers::fallback)
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
        .layer(
            ServiceBuilder::new()
                .layer(middleware::map_request(strip_request_id))
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
