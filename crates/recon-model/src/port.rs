//! The one trait (D-016): `Proposer`. A caller wraps it with its own deadline
//! (20 s per call, at most two automatic retries for transient codes — the
//! bounded wrapper arrives with the egress adapters; the port stays transport-
//! free so the recorded adapter is sync-fast and `dyn`-compatible without a new
//! dependency).

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::{Answer, ProposeRequest};

/// A boxed, sendable future — the trait's object-safe async shape.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One proposer. Implementations: the recorded adapter here, then the
/// OpenAI-compatible HTTP adapter and the loopback local-model adapter with
/// their egress plumbing (D-024). `propose` never commits anything — it returns
/// the model's raw answer; [`crate::validate`] decides what it may become.
pub trait Proposer: Send + Sync {
    /// Asks for one answer to `request`. An `Err` is the provider's state, not the
    /// answer's: the caller decides the retry and exception policy.
    fn propose<'a>(
        &'a self,
        request: &'a ProposeRequest,
    ) -> BoxFuture<'a, Result<Answer, ProposeError>>;
}

/// Why a call produced no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposeError {
    /// The provider could not be reached or answered unusably (failure case 9).
    Unavailable,
    /// A recorded adapter had no answer registered for the request's prompt
    /// version — a misconfigured fixture, reported rather than guessed.
    Unrecorded,
    /// The answer bytes were not the schema.
    Malformed,
    /// The job's call budget was already spent — no call was made (failure
    /// case 9's `model_budget_exhausted`).
    Budget,
}

impl ProposeError {
    /// The stable machine code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Unrecorded => "unrecorded",
            Self::Malformed => "malformed",
            Self::Budget => "budget",
        }
    }
}

impl fmt::Display for ProposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ProposeError {}
