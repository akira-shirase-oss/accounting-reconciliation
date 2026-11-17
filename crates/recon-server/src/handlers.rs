//! Route handlers. The firm comes from the path in R0 (the operator acts for every firm);
//! from R3 it comes from the signed-in user's session and the path must agree with it.

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use recon_db::{imports, tenancy};
use recon_domain::text::is_forbidden;
use recon_domain::{AccountId, ClientId, Currency, FirmId, ImportId};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;
use crate::upload;

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

/// `POST /v1/firms/{firm}/accounts/{account}/imports`: a `text/csv` statement.
pub async fn upload_statement(
    State(state): State<AppState>,
    Path((firm, account)): Path<(String, String)>,
    headers: HeaderMap,
    payload: Body,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let account = AccountId::from_uuid(id(&account)?);
    upload::precheck(&headers)?;
    if !state.startup_checked() {
        return Err(ApiError::database_unavailable());
    }
    let Ok(_permit) = state.import_permits.clone().try_acquire_owned() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "import_capacity",
            "too many imports in flight; nothing was accepted, retry shortly",
        )
        .retry_after(1));
    };
    upload::import(&state, firm, account, payload).await
}

/// `GET /v1/firms/{firm}/imports/{import}`.
pub async fn get_import(
    State(state): State<AppState>,
    Path((firm, import)): Path<(String, String)>,
) -> ApiResult {
    let firm = FirmId::from_uuid(id(&firm)?);
    let import = ImportId::from_uuid(id(&import)?);
    let record = imports::get(&state.pool, firm, import).await?;
    Ok((StatusCode::OK, Json(json!({ "import": record }))))
}

/// Anything else.
pub async fn fallback() -> ApiError {
    ApiError::not_found()
}
