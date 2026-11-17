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

    /// 503 for an unreachable or overloaded database.
    pub fn database_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "the database is unavailable; nothing was accepted, retry later",
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
            DbError::Unavailable(_) | DbError::Timeout(_) | DbError::Retryable(_) => {
                tracing::warn!(error = %error, sqlstate = ?error.sqlstate(), "database unavailable");
                Self::database_unavailable()
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
            DbError::Constraint(name) => {
                tracing::warn!(constraint = %name, "constraint refused a write");
                Self::new(
                    StatusCode::CONFLICT,
                    "conflict",
                    "the request conflicts with existing data",
                )
            }
            DbError::Refused(_) | DbError::Inconsistent(_) | DbError::Other(_) => {
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
