//! One error type for storage, classified so callers can answer honestly: an unreachable
//! database is `503`, a guard the schema enforces is a named refusal, a lost lease means
//! another process (or the sweeper) owns the import now.

/// Storage failures.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A guard in the schema refused the write (SQLSTATE class RC, the migration lists
    /// them). The code names the guard.
    #[error("the database refused the write ({0})")]
    Refused(&'static str),
    /// A row the caller needs does not exist (in this firm).
    #[error("not found")]
    NotFound,
    /// A unique, foreign-key, check or not-null constraint refused the write.
    #[error("constraint {0} refused the write")]
    Constraint(String),
    /// The import is no longer this caller's to finish: its lease expired, the sweeper
    /// discarded it, or the token is wrong (fencing).
    #[error("the import's lease was lost")]
    LeaseLost,
    /// Another flip holds the account: the import was discarded; upload the file again
    /// (D-053).
    #[error("another import into this account is being published")]
    AccountBusy,
    /// A matching job already runs for the client (D-033: one running job per client).
    #[error("a matching job is already running for this client")]
    ClientBusy,
    /// The firm already holds the most profile names it may (D-062).
    #[error("the firm's profile limit is reached")]
    ProfileLimit,
    /// The import is not a held revision (it was never one, or it was already decided).
    #[error("the import is not a held revision")]
    NotARevision,
    /// The stored lines changed since the revision was listed: it no longer says what
    /// accepting it would do. Nothing changed; upload the file again.
    #[error("the revision is stale")]
    RevisionStale,
    /// A line the revision would replace is no longer `unreviewed` (R3 handles decided
    /// lines through `source_changed`, D-009).
    #[error("the revision replaces a decided line")]
    RevisionTouchesDecidedLine,
    /// The staging table does not hold every row this import staged (an unlogged table
    /// emptied by a crash): the flip refuses rather than publish a partial import.
    #[error("staged rows are missing; the import was not published")]
    StagingIncomplete,
    /// The database could not be reached in time (pool timeout, connection refused, I/O).
    #[error("database unavailable")]
    Unavailable(#[source] sqlx::Error),
    /// A statement or lock exceeded its deadline.
    #[error("database deadline exceeded")]
    Timeout(#[source] sqlx::Error),
    /// A serialization failure or deadlock; the whole transaction may be retried.
    #[error("transaction conflict; retry")]
    Retryable(#[source] sqlx::Error),
    /// The database's own consistency checks disagree with what was written: a defect.
    #[error("inconsistent: {0}")]
    Inconsistent(String),
    /// A migration failed.
    #[error("migration failed")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// Anything else.
    #[error("database error")]
    Other(#[source] sqlx::Error),
}

impl DbError {
    /// True when the database itself (not the request) is the problem, so the caller
    /// should answer `503` and the client may retry later.
    pub const fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Unavailable(_) | Self::Timeout(_) | Self::Retryable(_)
        )
    }

    /// The PostgreSQL SQLSTATE behind this error, if any.
    pub fn sqlstate(&self) -> Option<String> {
        match self {
            Self::Unavailable(e) | Self::Timeout(e) | Self::Retryable(e) | Self::Other(e) => e
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .map(std::borrow::Cow::into_owned),
            _ => None,
        }
    }
}

/// The schema's custom SQLSTATEs and their names.
const GUARDS: [(&str, &str); 23] = [
    ("RCA01", "decision_log_append_only"),
    ("RCA02", "no_decision_chain"),
    ("RCA03", "decision_already_recorded"),
    ("RCA04", "chain_head_names_no_record"),
    ("RCI01", "import_final"),
    ("RCI02", "import_counts_mismatch"),
    ("RCI03", "import_identity_immutable"),
    ("RCI04", "duplicates_need_staging_import"),
    ("RCI05", "import_needs_its_decision"),
    ("RCI06", "revision_rows_guarded"),
    ("RCD01", "documents_need_staging_import"),
    ("RCD02", "documents_immutable"),
    ("RCP01", "profiles_immutable"),
    ("RCL01", "lines_need_staging_import"),
    ("RCL02", "lines_need_complete_import"),
    ("RCL03", "lines_immutable"),
    ("RCL04", "line_needs_its_decision"),
    ("RCL05", "line_exception_state_mismatch"),
    ("RCJ01", "match_job_refused"),
    ("RCM01", "match_refused"),
    ("RCM02", "match_needs_its_decision"),
    ("RCM03", "allocation_refused"),
    ("RCD03", "document_bookkeeping_refused"),
];

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::Protocol(_)
            | sqlx::Error::WorkerCrashed => Self::Unavailable(error),
            sqlx::Error::RowNotFound => Self::NotFound,
            sqlx::Error::Database(db) => {
                let code = db
                    .code()
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default();
                if let Some((_, name)) = GUARDS.iter().find(|(c, _)| *c == code) {
                    return Self::Refused(name);
                }
                match code.as_str() {
                    "23505" | "23503" | "23514" | "23502" => {
                        Self::Constraint(db.constraint().unwrap_or("unnamed").to_owned())
                    }
                    // query_canceled (statement_timeout), lock_not_available (lock_timeout)
                    "57014" | "55P03" => Self::Timeout(error),
                    "40001" | "40P01" => Self::Retryable(error),
                    // admin_shutdown, crash_shutdown, cannot_connect_now, too_many_connections
                    "57P01" | "57P02" | "57P03" | "53300" => Self::Unavailable(error),
                    _ => Self::Other(error),
                }
            }
            _ => Self::Other(error),
        }
    }
}
