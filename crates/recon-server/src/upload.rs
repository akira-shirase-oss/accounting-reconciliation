//! Uploads: a statement (CSV through a profile, or OFX) into an account, or a document file
//! into a client, streamed through its parser into staging, then published whole by the flip
//! (ARCHITECTURE, *Import*).
//!
//! Bounded at every step: one import permit per upload (none free: `503`, never queued);
//! 50 MiB (by `Content-Length` before reading, and by counting while reading); 60 s to
//! parse; 15 s of silence between chunks; one staged batch of 5,000 rows or 1 MiB in memory (D-067),
//! under a staging budget shared by every upload, and the next chunk is not read until it is
//! stored (backpressure). Any refused row refuses the file and nothing is persisted. A `201`
//! (complete), `200` (the same file again) or `202` (held as a revision, D-065) is sent only
//! after the flip committed; every other outcome leaves nothing visible.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_db::imports::{
    self, Actor, FlipOutcome, ImportRecord, ProfileId, Refusal, StageRows, StagingImport,
};
use recon_db::{DbError, tenancy};
use recon_domain::document::NewDocument;
use recon_domain::import::{NewLine, StagedRow};
use recon_domain::limits::MAX_UPLOAD_BYTES;
use recon_domain::line::ActorKind;
use recon_domain::refusal::RefusalCode;
use recon_domain::{AccountId, ClientId, FirmId};
use recon_import::{
    AccountContext, CompiledDocuments, CompiledStatement, CsvLimits, DocumentContext,
    DocumentParser, FileRefusal, OfxParser, ParseSummary, StatementParser,
};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::error::ApiError;
use crate::faults::{FaultHook, FaultPoint};
use crate::state::AppState;

/// The operator, the only actor until sessions exist (D-041).
pub const OPERATOR: Actor<'static> = Actor {
    kind: ActorKind::Operator,
    reference: "operator",
};

/// How a statement file is read.
#[derive(Debug, Clone)]
pub enum StatementFormat {
    /// CSV through a profile.
    Csv(Arc<CompiledStatement>),
    /// OFX 1.x or 2.x (profile `ofx` v1).
    Ofx,
}

/// The media types an upload of this kind accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    /// `text/csv`.
    Csv,
    /// `application/x-ofx` or `application/ofx`.
    Ofx,
}

impl Media {
    fn accepts(self, content_type: &str) -> bool {
        match self {
            Self::Csv => content_type == "text/csv",
            Self::Ofx => matches!(content_type, "application/x-ofx" | "application/ofx"),
        }
    }
}

/// A parser the upload loop drives: rows go to `on_row` until one is refused.
pub trait FileParser: Send {
    /// The rows it produces.
    type Row: StageRows;
    /// Feeds the next chunk.
    fn push<F: FnMut(Self::Row)>(
        &mut self,
        chunk: &[u8],
        on_row: &mut F,
    ) -> Result<(), FileRefusal>;
    /// Ends the file.
    fn finish<F: FnMut(Self::Row)>(self, on_row: &mut F) -> Result<ParseSummary, FileRefusal>;
    /// Rows refused so far.
    fn rows_refused(&self) -> u32;
    /// Bytes read so far.
    fn file_bytes(&self) -> u64;
}

/// A statement parser of either format (boxed: one per upload, and the two differ in size).
#[derive(Debug)]
pub enum StatementFile {
    /// CSV.
    Csv(Box<StatementParser>),
    /// OFX.
    Ofx(Box<OfxParser>),
}

impl FileParser for StatementFile {
    type Row = NewLine;

    fn push<F: FnMut(NewLine)>(&mut self, chunk: &[u8], on_row: &mut F) -> Result<(), FileRefusal> {
        match self {
            Self::Csv(p) => p.push(chunk, on_row),
            Self::Ofx(p) => p.push(chunk, on_row),
        }
    }

    fn finish<F: FnMut(NewLine)>(self, on_row: &mut F) -> Result<ParseSummary, FileRefusal> {
        match self {
            Self::Csv(p) => p.finish(on_row),
            Self::Ofx(p) => p.finish(on_row),
        }
    }

    fn rows_refused(&self) -> u32 {
        match self {
            Self::Csv(p) => p.rows_refused(),
            Self::Ofx(p) => p.rows_refused(),
        }
    }

    fn file_bytes(&self) -> u64 {
        match self {
            Self::Csv(p) => p.file_bytes(),
            Self::Ofx(p) => p.file_bytes(),
        }
    }
}

impl FileParser for DocumentParser {
    type Row = NewDocument;

    fn push<F: FnMut(NewDocument)>(
        &mut self,
        chunk: &[u8],
        on_row: &mut F,
    ) -> Result<(), FileRefusal> {
        Self::push(self, chunk, on_row)
    }

    fn finish<F: FnMut(NewDocument)>(self, on_row: &mut F) -> Result<ParseSummary, FileRefusal> {
        Self::finish(self, on_row)
    }

    fn rows_refused(&self) -> u32 {
        Self::rows_refused(self)
    }

    fn file_bytes(&self) -> u64 {
        Self::file_bytes(self)
    }
}

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
pub fn precheck(headers: &HeaderMap, media: Media) -> Result<(), ApiError> {
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
    if !content_type.is_some_and(|t| media.accepts(&t)) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "CSV profiles take text/csv; the ofx profile takes application/x-ofx",
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
        "files are at most 50 MiB",
    )
}

/// Imports one statement file into `account`.
pub async fn import_statement(
    state: &AppState,
    firm: FirmId,
    account: AccountId,
    profile: ProfileId,
    format: StatementFormat,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let account = tenancy::account(&state.pool, firm, account).await?;
    let context = AccountContext {
        account: account.id,
        currency: account.currency,
    };
    let parser = match format {
        StatementFormat::Csv(p) => StatementFile::Csv(Box::new(StatementParser::with_profile(
            context,
            p,
            CsvLimits::default(),
        ))),
        StatementFormat::Ofx => StatementFile::Ofx(Box::new(OfxParser::new(context))),
    };
    let staging =
        imports::begin_with(&state.pool, &account, &profile, state.config.import_lease).await?;
    run(state, staging, parser, body).await
}

/// Imports one document file for `client`.
pub async fn import_documents(
    state: &AppState,
    firm: FirmId,
    client: ClientId,
    profile: ProfileId,
    compiled: Arc<CompiledDocuments>,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let parser =
        DocumentParser::with_profile(DocumentContext { client }, compiled, CsvLimits::default());
    let staging = imports::begin_documents(
        &state.pool,
        firm,
        client,
        &profile,
        state.config.import_lease,
    )
    .await?;
    run(state, staging, parser, body).await
}

async fn run<P: FileParser>(
    state: &AppState,
    mut staging: StagingImport,
    mut parser: P,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let started = Instant::now();
    let import_id = staging.id;
    let mut batch: Vec<P::Row> = Vec::with_capacity(imports::STAGING_BATCH_ROWS);
    let read = read_and_stage(state, &mut staging, &mut parser, &mut batch, body, started).await;
    let outcome = match read {
        Err(failure) => return Err(fail(state, staging, &parser, failure).await),
        Ok(ReadEnd::Refused(refusal)) => {
            let counts = (parser.file_bytes(), parser.rows_refused());
            let record = refuse_file(state, staging, refusal, counts).await?;
            return Err(refused_response(refusal.code, &record));
        }
        Ok(ReadEnd::Finished) => {
            let mut tail = Vec::new();
            // finish() consumes the parser; keep its counts for a refusal raised at the end.
            let counts = (parser.file_bytes(), parser.rows_refused());
            match parser.finish(&mut |row| tail.push(row)) {
                Err(refusal) => {
                    let record = refuse_file(state, staging, refusal, counts).await?;
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
        Ok(FlipOutcome::Revision(record)) => (StatusCode::ACCEPTED, record, false),
        Ok(FlipOutcome::Refused(record)) => {
            return Err(refused_response(
                RefusalCode::DocumentNumberConflict,
                &record,
            ));
        }
        Err(response) => return Err(response),
    };
    state.faults.reached(FaultPoint::AfterCommit).await;
    tracing::info!(
        import = %import_id,
        answered_by = %record.id,
        status = %record.status,
        duplicate_file = duplicate,
        rows = record.rows_read,
        lines = record.lines_created,
        duplicates = record.duplicates,
        changes = record.changes,
        bytes = record.file_bytes,
        ms = started.elapsed().as_millis(),
        "import finished"
    );
    let revision = (status == StatusCode::ACCEPTED).then(|| {
        json!({
            "changes": record.changes,
            "message": "the file disagrees with stored lines in the dates they share; nothing was imported. \
                        Review GET .../changes, then POST .../accept or .../reject",
        })
    });
    Ok((
        status,
        Json(json!({ "import": record, "duplicate_file": duplicate, "revision": revision })),
    ))
}

/// Reads the body chunk by chunk into the parser and stages full batches as they fill.
async fn read_and_stage<P: FileParser>(
    state: &AppState,
    staging: &mut StagingImport,
    parser: &mut P,
    batch: &mut Vec<P::Row>,
    mut body: Body,
    started: Instant,
) -> Result<ReadEnd, ReadFailure> {
    let config = &state.config;
    let deadline = started + config.parse_deadline;
    let mut staged_batches: u32 = 0;
    // Bytes of parsed rows buffered, and the staging budget reserved for them (D-050).
    let mut batch_bytes: usize = 0;
    let mut reserved: Option<tokio::sync::OwnedSemaphorePermit> = None;
    let budget_kib = u32::try_from(config.staging_budget_bytes / 1024).unwrap_or(u32::MAX);
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
        let pushed = parser.push(&data, &mut |row| {
            batch_bytes = batch_bytes.saturating_add(row.heap_bytes());
            batch.push(row);
        });
        if let Err(refusal) = pushed {
            return Ok(ReadEnd::Refused(refusal));
        }
        if parser.rows_refused() > 0 {
            // The file will be refused; stage nothing more, keep reading for the report.
            batch.clear();
            batch_bytes = 0;
            reserved = None;
            continue;
        }
        // Reserve budget for what is buffered; with none free, stop reading until another
        // upload stores its batch (backpressure on the body, bounded by the parse deadline).
        let held = reserved
            .as_ref()
            .map_or(0, |p| u32::try_from(p.num_permits()).unwrap_or(u32::MAX));
        let need = u32::try_from(batch_bytes.div_ceil(1024))
            .unwrap_or(u32::MAX)
            .min(budget_kib);
        if need > held {
            let more = tokio::time::timeout_at(
                deadline,
                state.staging_budget.clone().acquire_many_owned(need - held),
            )
            .await
            .map_err(|_| ReadFailure::Deadline)?
            .map_err(|_| ReadFailure::Deadline)?;
            match reserved.as_mut() {
                Some(p) => p.merge(more),
                None => reserved = Some(more),
            }
        }
        // Exact cuts: 5,000 rows, or fewer when 1 MiB comes first (at least one row).
        while batch.len() >= imports::STAGING_BATCH_ROWS
            || batch_bytes >= imports::STAGING_BATCH_BYTES
        {
            let (cut, cut_bytes) = batch_cut(batch);
            let rest = batch.split_off(cut);
            let full = std::mem::replace(batch, rest);
            P::Row::stage(&state.pool, staging, &full, config.import_lease)
                .await
                .map_err(ReadFailure::Db)?;
            drop(full);
            batch_bytes = batch_bytes.saturating_sub(cut_bytes);
            renewed_at = Instant::now();
            staged_batches = staged_batches.saturating_add(1);
            state
                .faults
                .reached(FaultPoint::StagedBatch(staged_batches))
                .await;
        }
        // Return the budget beyond what is still buffered.
        let keep = batch_bytes.div_ceil(1024);
        if keep == 0 {
            reserved = None;
        } else if let Some(p) = reserved.as_mut()
            && p.num_permits() > keep
        {
            drop(p.split(p.num_permits() - keep));
        }
    }
}

/// The first batch to stage: up to 5,000 rows and 1 MiB, at least one row. Returns the row
/// count and its bytes.
fn batch_cut<R: StagedRow>(batch: &[R]) -> (usize, usize) {
    let mut cut = 0_usize;
    let mut bytes = 0_usize;
    for row in batch.iter().take(imports::STAGING_BATCH_ROWS) {
        let b = row.heap_bytes();
        if cut > 0 && bytes.saturating_add(b) > imports::STAGING_BATCH_BYTES {
            break;
        }
        cut = cut.saturating_add(1);
        bytes = bytes.saturating_add(b);
    }
    (cut, bytes)
}

/// Stages the last rows and flips. Refused files never get here with rows.
async fn publish<R: StageRows>(
    state: &AppState,
    mut staging: StagingImport,
    summary: &ParseSummary,
    batch: Vec<R>,
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
                question: None,
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
        if let Err(e) = R::stage(&state.pool, &mut staging, chunk, state.config.import_lease).await
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
    let base = match code {
        RefusalCode::FileTooLarge => file_too_large(),
        RefusalCode::ParseDeadlineExceeded => ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "parse_deadline_exceeded",
            "the file took longer than 60 s to read and was refused; nothing was imported",
        ),
        RefusalCode::AmbiguousDateFormat => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ambiguous_date_format",
            "the profile does not declare whether dates are day-month or month-day; nothing was \
             imported. Declare the order (one of the options) in a new profile version",
        ),
        _ => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "file_refused",
            "the file was refused whole; nothing was imported",
        ),
    };
    base.with("import", json!(record))
}

/// The question an `ambiguous_date_format` refusal asks (D-062).
fn question(refusal: &FileRefusal) -> Option<Value> {
    refusal.question.map(|q| {
        json!({
            "kind": "date_order",
            "fields": "every date column",
            "options": q.options(),
            "first_row": refusal.row,
        })
    })
}

async fn refuse_file(
    state: &AppState,
    staging: StagingImport,
    refusal: FileRefusal,
    (file_bytes, rows_refused): (u64, u32),
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
            rows_refused,
            refusals: &refusals,
            file_hash: None,
            file_bytes,
            question: question(&refusal),
        },
        OPERATOR,
    )
    .await?)
}

/// Cleans up after a failed read, best effort and bounded: the sweeper discards whatever
/// this cannot (the lease expires).
async fn fail<P: FileParser>(
    state: &AppState,
    staging: StagingImport,
    parser: &P,
    failure: ReadFailure,
) -> ApiError {
    match failure {
        ReadFailure::Db(e) => discard_after(state, staging, &e).await,
        ReadFailure::Deadline => {
            match refuse_file(
                state,
                staging,
                FileRefusal::new(RefusalCode::ParseDeadlineExceeded, None),
                (parser.file_bytes(), parser.rows_refused()),
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
