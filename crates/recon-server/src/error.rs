//! One error envelope for every failure: `{"error": {"code", "message", ...details}}` with
//! a stable code, a safe message (never file content, never a database error's detail) and
//! `Retry-After` where retrying is the right move.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use recon_db::DbError;
use serde_json::{Map, Value, json};

/// An API failure.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    details: Map<String, Value>,
    retry_after: Option<u32>,
}

impl ApiError {
    /// A failure with a code and a message.
    pub fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            details: Map::new(),
            retry_after: None,
        }
    }

    /// Adds a detail field.
    #[must_use]
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.details.insert(key.to_owned(), value);
        self
    }

    /// Asks the client to retry after `seconds`.
    #[must_use]
    pub fn retry_after(mut self, seconds: u32) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// The status.
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// The code.
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// 401.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid operator token is required",
        )
    }

    /// 404.
    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such resource in this firm",
        )
    }

    /// 503 for an unreachable database.
    pub fn database_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "the database is unavailable; nothing was accepted, retry later",
        )
        .retry_after(1)
    }

    /// 503 when the service is at a local capacity bound — the in-flight request
    /// limit or the connection pool (D-075). The database may be perfectly healthy;
    /// retrying after the hint is the right move.
    pub fn overloaded() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded",
            "the service is at capacity; nothing was accepted, retry after the hint",
        )
        .retry_after(1)
    }

    /// 500 for a defect (logged where it is handled).
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "an internal error occurred",
        )
    }

    /// Classifies a storage failure, logging it once (codes and variants only: a database
    /// error's detail can carry row values, D-025).
    pub fn from_db(error: &DbError) -> Self {
        match error {
            DbError::NotFound => Self::not_found(),
            DbError::Unavailable(e) | DbError::Timeout(e) | DbError::Retryable(e) => {
                // Honest naming (D-075): a saturated connection pool, a lock or
                // serialization conflict, or the database refusing connections is
                // backpressure — `overloaded` — not `database_unavailable`, which is
                // kept for the database actually being unreachable or restarting.
                let contention = matches!(e, sqlx::Error::PoolTimedOut)
                    || matches!(
                        error.sqlstate().as_deref(),
                        Some("55P03" | "53300" | "40001" | "40P01" | "57014")
                    );
                if contention {
                    tracing::warn!(error = %error, sqlstate = ?error.sqlstate(), "service overloaded");
                    Self::overloaded()
                } else {
                    tracing::warn!(error = %error, sqlstate = ?error.sqlstate(), "database unavailable");
                    Self::database_unavailable()
                }
            }
            DbError::LeaseLost | DbError::StagingIncomplete => {
                tracing::warn!(error = %error, "import interrupted");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "import_interrupted",
                    "the import was interrupted and nothing was published; upload the file again",
                )
                .retry_after(1)
            }
            DbError::AccountBusy => Self::new(
                StatusCode::CONFLICT,
                "account_busy",
                "another import into this account (or this client's documents) is being published; nothing changed, try again shortly",
            )
            .retry_after(2),
            DbError::ClientBusy => Self::new(
                StatusCode::CONFLICT,
                "client_busy",
                "a matching job is already running for this client",
            )
            .retry_after(2),
            DbError::PeriodClosed => Self::new(
                StatusCode::CONFLICT,
                "period_closed",
                "the line's period is closed",
            ),
            DbError::NotARevision => Self::new(
                StatusCode::CONFLICT,
                "not_a_revision",
                "the import is not a held revision (it may already be decided)",
            ),
            DbError::RevisionStale => Self::new(
                StatusCode::CONFLICT,
                "revision_stale",
                "the stored lines changed since the revision was listed; nothing changed, upload the file again",
            ),
            DbError::RevisionTouchesDecidedLine => Self::new(
                StatusCode::CONFLICT,
                "revision_touches_decided_line",
                "the revision replaces a line that is no longer unreviewed; nothing changed",
            ),
            DbError::ProfileLimit => Self::new(
                StatusCode::CONFLICT,
                "profile_limit",
                "the firm holds the most profile names it may",
            ),
            DbError::Constraint(name) => {
                tracing::warn!(constraint = %name, "constraint refused a write");
                Self::new(
                    StatusCode::CONFLICT,
                    "conflict",
                    "the request conflicts with existing data",
                )
            }
            DbError::Refused(_)
            | DbError::Inconsistent(_)
            | DbError::Migrate(_)
            | DbError::Other(_) => {
                tracing::error!(error = %error, sqlstate = ?error.sqlstate(), "storage defect");
                Self::internal()
            }
        }
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        Self::from_db(&error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = Map::new();
        body.insert("code".into(), json!(self.code));
        body.insert("message".into(), json!(self.message));
        body.extend(self.details);
        let mut response = (self.status, Json(json!({ "error": body }))).into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A saturated connection pool is backpressure named `overloaded` — not
    /// `database_unavailable`, which is kept for a database actually unreachable
    /// (D-075: the r2-match full run's 3,301 poll 503s were pool contention while
    /// the database was healthy).
    #[test]
    fn a_saturated_pool_is_named_overloaded() {
        let e = ApiError::from_db(&DbError::Unavailable(sqlx::Error::PoolTimedOut));
        assert_eq!(
            (e.status(), e.code()),
            (StatusCode::SERVICE_UNAVAILABLE, "overloaded")
        );
        assert!(e.retry_after.is_some());
    }

    /// A transport failure to the database stays `database_unavailable`.
    #[test]
    fn an_unreachable_database_is_named_unavailable() {
        let e = ApiError::from_db(&DbError::Unavailable(sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ))));
        assert_eq!(
            (e.status(), e.code()),
            (StatusCode::SERVICE_UNAVAILABLE, "database_unavailable")
        );
    }
}
