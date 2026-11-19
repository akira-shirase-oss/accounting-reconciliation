//! Route handlers. The firm comes from the path until R3 (the operator acts for every firm);
//! from R3 it comes from the signed-in user's session and the path must agree with it.

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use recon_db::{imports, tenancy};
use recon_domain::import::PROFILE_DOCUMENTS;
use recon_domain::text::is_forbidden;
use recon_domain::{AccountId, ClientId, Currency, FirmId, ImportId};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;
use crate::{profiles, upload};

type ApiResult = Result<(StatusCode, Json<Value>), ApiError>;

fn id(text: &str) -> Result<Uuid, ApiError> {
    text.parse().map_err(|_| ApiError::not_found())
}

fn name(text: &str) -> Result<&str, ApiError> {
    let len = text.chars().count();
    if (1..=200).contains(&len) && !text.chars().any(is_forbidden) && text.trim() == text {
        Ok(text)
    } else {
        Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
            "a name is 1 to 200 characters without control characters or outer spaces",
        ))
    }
}

fn body<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload.map(|Json(v)| v).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "the request body is not the expected JSON",
        )
    })
}

/// Liveness: the process is up.
pub async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// Readiness: the startup check passed and the background probe reached the database within
/// the last 1.5 s (D-051). Served from that cached state, never a query per request; red
/// within two seconds of the database going away (drill `db-down`).
pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    if !state.startup_checked() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ready": false, "reason": "startup_check_pending" })),
        );
    }
    if state.db_ok_within(crate::tasks::READINESS_WINDOW) {
        (StatusCode::OK, Json(json!({ "ready": true })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ready": false, "reason": "database_unavailable" })),
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewNamed {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAccount {
    name: String,
    currency: String,
}

/// `POST /v1/firms`.
pub async fn create_firm(
    State(state): State<AppState>,
    payload: Result<Json<NewNamed>, JsonRejection>,
) -> ApiResult {
    let payload = body(payload)?;
    let firm = FirmId::from_uuid(Uuid::now_v7());
    tenancy::create_firm(&state.pool, firm, name(&payload.name)?).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "firm": { "id": firm, "name": payload.name } })),
    ))
}

/// `POST /v1/firms/{firm}/clients`.
pub async fn create_client(
    State(state): State<AppState>,
    Path(firm): Path<String>,
    payload: Result<Json<NewNamed>, JsonRejection>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let payload = body(payload)?;
    if !tenancy::firm_exists(&state.pool, firm).await? {
        return Err(ApiError::not_found());
    }
    let client = ClientId::from_uuid(Uuid::now_v7());
    tenancy::create_client(&state.pool, firm, client, name(&payload.name)?).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "client": { "id": client, "name": payload.name } })),
    ))
}

/// `POST /v1/firms/{firm}/clients/{client}/accounts`.
pub async fn create_account(
    State(state): State<AppState>,
    Path((firm, client)): Path<(String, String)>,
    payload: Result<Json<NewAccount>, JsonRejection>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let client = ClientId::from_uuid(id(&client)?);
    let payload = body(payload)?;
    let currency = Currency::from_code(&payload.currency).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_currency",
            "the currency is not in the supported table (D-005)",
        )
    })?;
    let account = AccountId::from_uuid(Uuid::now_v7());
    match tenancy::create_account(
        &state.pool,
        firm,
        client,
        account,
        name(&payload.name)?,
        currency,
    )
    .await
    {
        // The client is not this firm's (or does not exist): the composite key refused it.
        Err(recon_db::DbError::Constraint(_)) => return Err(ApiError::not_found()),
        other => other?,
    };
    Ok((
        StatusCode::CREATED,
        Json(json!({ "account": {
            "id": account, "client_id": client, "name": payload.name, "currency": currency.code()
        } })),
    ))
}

/// Which profile reads an upload: `?profile=NAME` (default: the built-in for the kind) and
/// `&version=N` (default: the latest).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileQuery {
    profile: Option<String>,
    version: Option<i32>,
}

fn query<T>(q: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    q.map(|Query(v)| v).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "the query string is not the expected parameters",
        )
    })
}

/// The file's name from `X-File-Name` (optional): shown in reports, never logged (D-068).
fn file_name(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get("x-file-name") else {
        return Ok(None);
    };
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_file_name",
            "X-File-Name is 1 to 255 characters of printable text, without a path",
        )
    };
    let text = value.to_str().map_err(|_| invalid())?.trim();
    let len = text.chars().count();
    if !(1..=255).contains(&len) || text.chars().any(is_forbidden) || text.contains(['/', '\\']) {
        return Err(invalid());
    }
    Ok(Some(text.to_owned()))
}

fn import_capacity(state: &AppState) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    if !state.startup_checked() {
        return Err(ApiError::database_unavailable());
    }
    state
        .import_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "import_capacity",
                "too many imports in flight; nothing was accepted, retry shortly",
            )
            .retry_after(1)
        })
}

/// `POST /v1/firms/{firm}/accounts/{account}/imports[?profile=&version=]`: a statement,
/// `text/csv` through a CSV profile (default `r0-generic-csv`) or `application/x-ofx` with
/// `profile=ofx`.
pub async fn upload_statement(
    State(state): State<AppState>,
    Path((firm, account)): Path<(String, String)>,
    params: Result<Query<ProfileQuery>, QueryRejection>,
    headers: HeaderMap,
    payload: Body,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let account = AccountId::from_uuid(id(&account)?);
    let params = query(params)?;
    let name = params
        .profile
        .as_deref()
        .unwrap_or(recon_domain::import::PROFILE_R0);
    let (profile, format, media) =
        profiles::statement(&state.pool, firm, name, params.version).await?;
    upload::precheck(&headers, media)?;
    let name = file_name(&headers)?;
    let _permit = import_capacity(&state)?;
    upload::import_statement(
        &state,
        firm,
        account,
        profile,
        format,
        name.as_deref(),
        payload,
    )
    .await
}

/// `POST /v1/firms/{firm}/clients/{client}/document-imports[?profile=&version=]`: invoices,
/// bills, receipts, credit notes or book entries as `text/csv` (default profile
/// `r1-generic-documents`).
pub async fn upload_documents(
    State(state): State<AppState>,
    Path((firm, client)): Path<(String, String)>,
    params: Result<Query<ProfileQuery>, QueryRejection>,
    headers: HeaderMap,
    payload: Body,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let client = ClientId::from_uuid(id(&client)?);
    let params = query(params)?;
    let name = params.profile.as_deref().unwrap_or(PROFILE_DOCUMENTS);
    let (profile, compiled) = profiles::documents(&state.pool, firm, name, params.version).await?;
    upload::precheck(&headers, upload::Media::Csv)?;
    let name = file_name(&headers)?;
    let _permit = import_capacity(&state)?;
    upload::import_documents(
        &state,
        firm,
        client,
        profile,
        compiled,
        name.as_deref(),
        payload,
    )
    .await
}

/// `GET /v1/firms/{firm}/imports/{import}`: the import, and for a refused one its report
/// (D-068).
pub async fn get_import(
    State(state): State<AppState>,
    Path((firm, import)): Path<(String, String)>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let import = ImportId::from_uuid(id(&import)?);
    let record = imports::get(&state.pool, firm, import).await?;
    let report = if record.status == "refused" {
        let profile = profiles::resolve(
            &state.pool,
            firm,
            &record.profile,
            Some(record.profile_version),
        )
        .await
        .map_or(recon_import::report::ProfileRef::Unknown, |r| r.report());
        upload::report_for(&record, &profile)
    } else {
        None
    };
    Ok((
        StatusCode::OK,
        Json(json!({ "import": record, "report": report })),
    ))
}

/// A page of a revision's changes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesQuery {
    after: Option<i32>,
    limit: Option<i64>,
}

/// `GET /v1/firms/{firm}/imports/{import}/changes[?after=&limit=]`: the differences a
/// revision lists (D-065), up to 1,000 per page, in order.
pub async fn revision_changes(
    State(state): State<AppState>,
    Path((firm, import)): Path<(String, String)>,
    params: Result<Query<ChangesQuery>, QueryRejection>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let import = ImportId::from_uuid(id(&import)?);
    let params = query(params)?;
    let record = imports::get(&state.pool, firm, import).await?;
    let limit = params.limit.unwrap_or(imports::MAX_CHANGES_PAGE);
    let changes = imports::revision_changes(
        &state.pool,
        firm,
        import,
        params.after.unwrap_or(0).max(0),
        limit,
    )
    .await?;
    let next = changes.last().map(|c| c.ordinal);
    Ok((
        StatusCode::OK,
        Json(json!({
            "import": record.id, "status": record.status, "total": record.changes,
            "changes": changes, "next_after": next,
        })),
    ))
}

/// `POST /v1/firms/{firm}/imports/{import}/accept`: accepts a held revision.
pub async fn accept_revision(
    State(state): State<AppState>,
    Path((firm, import)): Path<(String, String)>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let import = ImportId::from_uuid(id(&import)?);
    let record = imports::accept_revision(
        &state.pool,
        firm,
        import,
        upload::OPERATOR,
        state.config.flip_timeout,
    )
    .await?;
    tracing::info!(import = %import, lines = record.lines_created, "revision accepted");
    Ok((StatusCode::OK, Json(json!({ "import": record }))))
}

/// `POST /v1/firms/{firm}/imports/{import}/reject`: rejects a held revision.
pub async fn reject_revision(
    State(state): State<AppState>,
    Path((firm, import)): Path<(String, String)>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let import = ImportId::from_uuid(id(&import)?);
    let record = imports::reject_revision(&state.pool, firm, import, upload::OPERATOR).await?;
    tracing::info!(import = %import, "revision rejected");
    Ok((StatusCode::OK, Json(json!({ "import": record }))))
}

/// `POST /v1/firms/{firm}/profiles`: stores the next version of a profile.
pub async fn create_profile(
    State(state): State<AppState>,
    Path(firm): Path<String>,
    payload: Result<Json<profiles::NewProfile>, JsonRejection>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let payload = body(payload)?;
    let profile = profiles::create(&state.pool, firm, payload).await?;
    Ok((StatusCode::CREATED, Json(json!({ "profile": profile }))))
}

/// The version of a profile to describe.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionQuery {
    version: Option<i32>,
}

/// `GET /v1/firms/{firm}/profiles/{name}[?version=]`.
pub async fn get_profile(
    State(state): State<AppState>,
    Path((firm, name)): Path<(String, String)>,
    params: Result<Query<VersionQuery>, QueryRejection>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let params = query(params)?;
    let profile = profiles::describe(&state.pool, firm, &name, params.version).await?;
    Ok((StatusCode::OK, Json(json!({ "profile": profile }))))
}

/// `POST /v1/firms/{firm}/clients/{client}/matching-jobs`: claims the client's
/// deterministic matching job and drives it on a worker; `202` with the job's row while
/// it runs. One running job per client (D-033): a second start is `409 client_busy`
/// naming the running job.
pub async fn start_matching_job(
    State(state): State<AppState>,
    Path((firm, client)): Path<(String, String)>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let client = ClientId::from_uuid(id(&client)?);
    if !state.startup_checked() {
        return Err(ApiError::database_unavailable());
    }
    let lease = match crate::matching::start(&state, firm, client).await {
        Ok(
            recon_db::matching::Start::Created(lease) | recon_db::matching::Start::Resumed(lease),
        ) => lease,
        Ok(recon_db::matching::Start::Busy { job }) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "client_busy",
                "a matching job is already running for this client",
            )
            .with("job", json!(job))
            .retry_after(2));
        }
        // The client is not this firm's (or does not exist): the composite key refused it.
        Err(recon_db::DbError::Constraint(_)) => return Err(ApiError::not_found()),
        Err(e) => return Err(ApiError::from_db(&e)),
    };
    let record = recon_db::matching::get(&state.pool, firm, lease.id).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "job": record }))))
}

/// `GET /v1/firms/{firm}/clients/{client}/matching-jobs/{job}`: the job's durable row —
/// its status, cursor against the pinned snapshot, counters and (once complete) the run's
/// digest.
pub async fn get_matching_job(
    State(state): State<AppState>,
    Path((firm, client, job)): Path<(String, String, String)>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let client = ClientId::from_uuid(id(&client)?);
    let job = id(&job)?;
    let record = recon_db::matching::get(&state.pool, firm, job).await?;
    if record.client_id != client.as_uuid() {
        // A job of another client in the same firm is not this resource.
        return Err(ApiError::not_found());
    }
    Ok((StatusCode::OK, Json(json!({ "job": record }))))
}

/// Anything else.
pub async fn fallback() -> ApiError {
    ApiError::not_found()
}
