//! `POST /v1/firms/{firm}/accounts/{account}/imports`: a statement file streamed through the
//! R0 profile into staging, then published whole by the flip (ARCHITECTURE, *Import*).
//!
//! Bounded at every step: one import permit per upload (none free: `503`, never queued);
//! 50 MiB (by `Content-Length` before reading, and by counting while reading); 60 s to
//! parse; 15 s of silence between chunks; one staged batch of 5,000 rows in memory, and the
//! next chunk is not read until it is stored (backpressure). Any refused row refuses the
//! file and nothing is persisted. A `201` or `200` is sent only after the flip committed;
//! every other outcome leaves nothing visible.

use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_db::imports::{self, Actor, FlipOutcome, ImportRecord, Refusal, StagingImport};
use recon_db::{DbError, tenancy};
use recon_domain::import::NewLine;
use recon_domain::limits::MAX_UPLOAD_BYTES;
use recon_domain::line::ActorKind;
use recon_domain::refusal::RefusalCode;
use recon_domain::{AccountId, FirmId};
use recon_import::{AccountContext, FileRefusal, ParseSummary, StatementParser};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::error::ApiError;
use crate::faults::{FaultHook, FaultPoint};
use crate::state::AppState;

const OPERATOR: Actor<'static> = Actor {
    kind: ActorKind::Operator,
    reference: "operator",
};

/// How reading the body ended.
enum ReadEnd {
    /// The body ended; the parser has every byte.
    Finished,
    /// The file was refused before its end.
    Refused(FileRefusal),
}

/// Why reading the body failed.
enum ReadFailure {
    /// The client went away or the body broke.
    Body,
    /// No chunk arrived within the idle timeout.
    Stalled,
    /// The parse deadline passed.
    Deadline,
    /// Storing a batch failed.
    Db(DbError),
}

/// Checks the request before anything is read: the media type and the declared size.
pub fn precheck(headers: &HeaderMap) -> Result<(), ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        });
    if content_type.as_deref() != Some("text/csv") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "statements are uploaded as text/csv",
        ));
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_UPLOAD_BYTES) {
        return Err(file_too_large());
    }
    Ok(())
}

fn file_too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "file_too_large",
        "statements are at most 50 MiB",
    )
}

/// Imports one statement file.
pub async fn import(
    state: &AppState,
    firm: FirmId,
    account: AccountId,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let started = Instant::now();
    let config = &state.config;
    let account = tenancy::account(&state.pool, firm, account).await?;
    let mut staging = imports::begin(&state.pool, &account, config.import_lease).await?;
    let import_id = staging.id;
    let mut parser = StatementParser::new(AccountContext {
        account: account.id,
        currency: account.currency,
    });
    let mut batch: Vec<NewLine> = Vec::with_capacity(imports::STAGING_BATCH_ROWS);
    let read = read_and_stage(state, &mut staging, &mut parser, &mut batch, body, started).await;
    let outcome = match read {
        Err(failure) => return Err(fail(state, staging, &parser, failure).await),
        Ok(ReadEnd::Refused(refusal)) => {
            let record = refuse_file(state, staging, &parser, refusal).await?;
            return Err(refused_response(refusal.code, &record));
        }
        Ok(ReadEnd::Finished) => {
            let mut tail = Vec::new();
            match parser.finish(&mut |line| tail.push(line)) {
                Err(refusal) => {
                    let record = refuse_file_finished(state, staging, refusal).await?;
                    return Err(refused_response(refusal.code, &record));
                }
                Ok(summary) => {
                    batch.extend(tail);
                    publish(state, staging, &summary, batch).await
                }
            }
        }
    };
    let (status, record, duplicate) = match outcome {
        Ok(FlipOutcome::Complete(record)) => (StatusCode::CREATED, record, false),
        Ok(FlipOutcome::DuplicateFile(earlier)) => (StatusCode::OK, earlier, true),
        Err(response) => return Err(response),
    };
    state.faults.reached(FaultPoint::AfterCommit).await;
    tracing::info!(
        import = %import_id,
        answered_by = %record.id,
        duplicate_file = duplicate,
        rows = record.rows_read,
        lines = record.lines_created,
        duplicates = record.duplicates,
        bytes = record.file_bytes,
        ms = started.elapsed().as_millis(),
        "import finished"
    );
    Ok((
        status,
        Json(json!({ "import": record, "duplicate_file": duplicate })),
    ))
}

/// Reads the body chunk by chunk into the parser and stages full batches as they fill.
async fn read_and_stage(
    state: &AppState,
    staging: &mut StagingImport,
    parser: &mut StatementParser,
    batch: &mut Vec<NewLine>,
    mut body: Body,
    started: Instant,
) -> Result<ReadEnd, ReadFailure> {
    let config = &state.config;
    let deadline = started + config.parse_deadline;
    let mut staged_batches: u32 = 0;
    // The lease is renewed by every staged batch, and on a timer between them, so a slow
    // but live upload keeps it until the parse deadline.
    let renew_every = config.import_lease / 3;
    let mut renewed_at = Instant::now();
    let mut heard_at = Instant::now();
    loop {
        if renewed_at.elapsed() >= renew_every {
            imports::renew(&state.pool, staging, config.import_lease)
                .await
                .map_err(ReadFailure::Db)?;
            renewed_at = Instant::now();
        }
        let idle_until = heard_at + config.body_idle_timeout;
        let wake = deadline.min(idle_until).min(renewed_at + renew_every);
        // Dropping a pending frame() is safe: nothing is buffered in it.
        let frame = match tokio::time::timeout_at(wake, body.frame()).await {
            Err(_) if Instant::now() >= deadline => return Err(ReadFailure::Deadline),
            Err(_) if Instant::now() >= idle_until => return Err(ReadFailure::Stalled),
            // A renewal is due: renew, then keep waiting.
            Err(_) => continue,
            Ok(None) => return Ok(ReadEnd::Finished),
            Ok(Some(Err(_))) => return Err(ReadFailure::Body),
            Ok(Some(Ok(frame))) => frame,
        };
        heard_at = Instant::now();
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if let Err(refusal) = parser.push(&data, &mut |line| batch.push(line)) {
            return Ok(ReadEnd::Refused(refusal));
        }
        if parser.rows_refused() > 0 {
            // The file will be refused; stage nothing more, keep reading for the report.
            batch.clear();
            continue;
        }
        while batch.len() >= imports::STAGING_BATCH_ROWS {
            let rest = batch.split_off(imports::STAGING_BATCH_ROWS);
            let full = std::mem::replace(batch, rest);
            imports::stage(&state.pool, staging, &full, config.import_lease)
                .await
                .map_err(ReadFailure::Db)?;
            renewed_at = Instant::now();
            staged_batches = staged_batches.saturating_add(1);
            state
                .faults
                .reached(FaultPoint::StagedBatch(staged_batches))
                .await;
        }
    }
}

/// Stages the last rows and flips. Refused files never get here with lines.
async fn publish(
    state: &AppState,
    mut staging: StagingImport,
    summary: &ParseSummary,
    batch: Vec<NewLine>,
) -> Result<FlipOutcome, ApiError> {
    if !summary.is_clean() {
        let record = imports::refuse(
            &state.pool,
            staging,
            &Refusal {
                code: "rows_refused",
                rows_read: Some(summary.rows_read),
                rows_refused: summary.rows_refused,
                refusals: &summary.refusals,
                file_hash: Some(summary.file_hash),
                file_bytes: summary.file_bytes,
            },
            OPERATOR,
        )
        .await?;
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "file_refused",
            "the file was refused whole; nothing was imported",
        )
        .with("import", json!(record)));
    }
    for chunk in batch.chunks(imports::STAGING_BATCH_ROWS) {
        if let Err(e) =
            imports::stage(&state.pool, &mut staging, chunk, state.config.import_lease).await
        {
            return Err(discard_after(state, staging, &e).await);
        }
    }
    state.faults.reached(FaultPoint::BeforeFlip).await;
    let hook = FaultHook(&state.faults);
    Ok(imports::flip(
        &state.pool,
        staging,
        summary,
        OPERATOR,
        &hook,
        state.config.flip_timeout,
    )
    .await?)
}

fn refused_response(code: RefusalCode, record: &ImportRecord) -> ApiError {
    let base = if code == RefusalCode::FileTooLarge {
        file_too_large()
    } else if code == RefusalCode::ParseDeadlineExceeded {
        ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "parse_deadline_exceeded",
            "the file took longer than 60 s to read and was refused; nothing was imported",
        )
    } else {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "file_refused",
            "the file was refused whole; nothing was imported",
        )
    };
    base.with("import", json!(record))
}

async fn refuse_file(
    state: &AppState,
    staging: StagingImport,
    parser: &StatementParser,
    refusal: FileRefusal,
) -> Result<ImportRecord, ApiError> {
    let refusals: Vec<_> = refusal
        .row
        .map(|row| recon_domain::RowRefusal {
            row,
            code: refusal.code,
        })
        .into_iter()
        .collect();
    Ok(imports::refuse(
        &state.pool,
        staging,
        &Refusal {
            code: refusal.code.as_str(),
            rows_read: None,
            rows_refused: parser.rows_refused(),
            refusals: &refusals,
            file_hash: None,
            file_bytes: parser.file_bytes(),
        },
        OPERATOR,
    )
    .await?)
}

async fn refuse_file_finished(
    state: &AppState,
    staging: StagingImport,
    refusal: FileRefusal,
) -> Result<ImportRecord, ApiError> {
    let refusals: Vec<_> = refusal
        .row
        .map(|row| recon_domain::RowRefusal {
            row,
            code: refusal.code,
        })
        .into_iter()
        .collect();
    Ok(imports::refuse(
        &state.pool,
        staging,
        &Refusal {
            code: refusal.code.as_str(),
            rows_read: None,
            rows_refused: 0,
            refusals: &refusals,
            file_hash: None,
            file_bytes: 0,
        },
        OPERATOR,
    )
    .await?)
}

/// Cleans up after a failed read, best effort and bounded: the sweeper discards whatever
/// this cannot (the lease expires).
async fn fail(
    state: &AppState,
    staging: StagingImport,
    parser: &StatementParser,
    failure: ReadFailure,
) -> ApiError {
    match failure {
        ReadFailure::Db(e) => discard_after(state, staging, &e).await,
        ReadFailure::Deadline => {
            match refuse_file(
                state,
                staging,
                parser,
                FileRefusal {
                    code: RefusalCode::ParseDeadlineExceeded,
                    row: None,
                },
            )
            .await
            {
                Ok(record) => refused_response(RefusalCode::ParseDeadlineExceeded, &record),
                Err(e) => e,
            }
        }
        ReadFailure::Body | ReadFailure::Stalled => {
            let stalled = matches!(failure, ReadFailure::Stalled);
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                imports::discard(&state.pool, staging, OPERATOR),
            )
            .await;
            if stalled {
                ApiError::new(
                    StatusCode::REQUEST_TIMEOUT,
                    "upload_stalled",
                    "the upload paused for too long; nothing was imported",
                )
            } else {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "upload_failed",
                    "the upload ended early; nothing was imported",
                )
            }
        }
    }
}

async fn discard_after(state: &AppState, staging: StagingImport, error: &DbError) -> ApiError {
    let response = ApiError::from_db(error);
    if !matches!(error, DbError::LeaseLost) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            imports::discard(&state.pool, staging, OPERATOR),
        )
        .await;
    }
    response
}
