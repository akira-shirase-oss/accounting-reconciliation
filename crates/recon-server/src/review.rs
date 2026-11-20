//! The review API (R3 slice 2): the queue, approve, reject, exclude and the manual
//! match. Every mutation is a person with a role, a CSRF'd session and a decision in
//! the same transaction; row versions give optimistic conflicts their 409.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use recon_db::review::{self, Approved, ReviewError};
use recon_domain::auth::{Act, may};
use recon_domain::line::ExcludeReason;
use recon_domain::matching::ResidualCode;
use recon_domain::{ClientId, FirmId, LineId};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{Auth, Session, authorize, forbidden_role, person_authorized, session_required};
use crate::error::ApiError;
use crate::state::AppState;

type ApiResult = Result<(StatusCode, Json<Value>), ApiError>;

fn body<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload.map(|Json(v)| v).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_body",
            "the request body is not the expected JSON",
        )
    })
}

pub(crate) fn review_error(e: ReviewError) -> ApiError {
    match e {
        ReviewError::NotFound => ApiError::not_found(),
        ReviewError::Conflict(message, winner) => {
            let mut error = ApiError::new(StatusCode::CONFLICT, "conflict", message);
            if let Some(winner) = winner {
                error = error.with(
                    "winner",
                    serde_json::to_value(&winner).unwrap_or(serde_json::Value::Null),
                );
            }
            error
        }
        ReviewError::PeriodClosed => ApiError::new(
            StatusCode::CONFLICT,
            "period_closed",
            "the line's period is closed",
        ),
        ReviewError::Refused(message) => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "refused", message)
        }
        ReviewError::Db(db) => ApiError::from(db),
    }
}

fn path_ids(firm: &str, client: &str) -> Result<(FirmId, ClientId), ApiError> {
    Ok((
        FirmId::from_uuid(firm.parse().map_err(|_| ApiError::not_found())?),
        ClientId::from_uuid(client.parse().map_err(|_| ApiError::not_found())?),
    ))
}

/// `GET …/review-queue` — proposed matches (engine and manual) plus exception lines.
pub async fn queue(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client)): Path<(String, String)>,
    Query(params): Query<std::collections::BTreeMap<String, String>>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    if let Some(session) = authorize(&auth, firm, Act::Read)? {
        session.client_scope(&state.pool, client).await?;
    }
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(200);
    let items = review::queue(&state.pool, firm, client, limit).await?;
    let items: Vec<Value> = items
        .iter()
        .map(|i| {
            json!({
                "kind": i.kind, "id": i.id, "line": i.line, "detail": i.detail,
                "row_version": i.row_version, "four_eyes": i.four_eyes,
                "first_approver": i.first_approver,
            })
        })
        .collect();
    Ok((StatusCode::OK, Json(json!({"items": items}))))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecision {
    /// The row version read with the subject; a stale read answers 409.
    expected_version: i64,
}

/// The session authorized for the act `match_id`'s decision needs — four-eyes (flagged
/// or over the firm's threshold) takes `ApproveFourEyes`, an ordinary proposal
/// `ApproveProposal` — with the match read back for the handler's write.
async fn approve_scoped<'a>(
    state: &AppState,
    auth: &'a Auth,
    firm: FirmId,
    client: ClientId,
    match_id: Uuid,
) -> Result<(&'a Session, review::Reviewable), ApiError> {
    let Auth::Person(session) = auth else {
        return Err(session_required());
    };
    session.firm_scope(firm)?;
    if !(may(session.role, Act::ApproveProposal) || may(session.role, Act::ApproveFourEyes)) {
        return Err(forbidden_role());
    }
    session.client_scope(&state.pool, client).await?;
    let m = review::match_for_review(&state.pool, firm, client, match_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let act = if m.needs_four_eyes {
        Act::ApproveFourEyes
    } else {
        Act::ApproveProposal
    };
    if !may(session.role, act) {
        return Err(forbidden_role());
    }
    Ok((session, m))
}

/// `POST …/matches/{match}/approve` — commits an ordinary proposal, or one eye of a
/// four-eyes match (the second, distinct person finishes it; D-018).
pub async fn approve(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, match_id)): Path<(String, String, String)>,
    payload: Result<Json<ReviewDecision>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let match_id: Uuid = match_id.parse().map_err(|_| ApiError::not_found())?;
    let (session, m) = approve_scoped(&state, &auth, firm, client, match_id).await?;
    let payload = body(payload)?;
    let outcome = review::approve(
        &state.pool,
        firm,
        client,
        match_id,
        session.user.as_uuid(),
        payload.expected_version,
        m.needs_four_eyes,
    )
    .await
    .map_err(review_error)?;
    match outcome {
        Approved::Committed => Ok((
            StatusCode::OK,
            Json(json!({"match": {"id": match_id, "status": "committed"}})),
        )),
        Approved::AwaitingSecondEye => Ok((
            StatusCode::ACCEPTED,
            Json(json!({"match": {"id": match_id, "status": "proposed",
                                 "first_approver": session.user.as_uuid(),
                                 "awaiting": "second_eye"}})),
        )),
    }
}

/// `POST …/matches/{match}/reject` — the proposal ends `rejected`, the line returns to
/// `unreviewed` and the documents' allocations release.
pub async fn reject(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, match_id)): Path<(String, String, String)>,
    payload: Result<Json<ReviewDecision>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let match_id: Uuid = match_id.parse().map_err(|_| ApiError::not_found())?;
    let (session, _m) = approve_scoped(&state, &auth, firm, client, match_id).await?;
    let payload = body(payload)?;
    review::reject(
        &state.pool,
        firm,
        client,
        match_id,
        session.user.as_uuid(),
        payload.expected_version,
    )
    .await
    .map_err(review_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({"match": {"id": match_id, "status": "rejected"}})),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExcludeRequest {
    /// The line's row version as read.
    expected_version: i64,
    /// `internal_transfer`, `non_business`, `reversed_pair` or `out_of_scope`.
    reason: String,
    /// A note on the decision, kept in its evidence.
    #[serde(default)]
    note: Option<String>,
}

/// `POST …/lines/{line}/exclude` — a person takes the line out of matching with a named
/// reason (an `unreviewed` or `exception` line; a proposal is decided first).
pub async fn exclude(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, line)): Path<(String, String, String)>,
    payload: Result<Json<ExcludeRequest>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let line = LineId::from_uuid(line.parse().map_err(|_| ApiError::not_found())?);
    let session = person_authorized(&auth, firm, Act::Resolve)?;
    session.client_scope(&state.pool, client).await?;
    let payload = body(payload)?;
    let reason = match payload.reason.as_str() {
        "internal_transfer" => ExcludeReason::InternalTransfer,
        "non_business" => ExcludeReason::NonBusiness,
        "reversed_pair" => ExcludeReason::ReversedPair,
        "out_of_scope" => ExcludeReason::OutOfScope,
        _ => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_reason",
                "a reason is internal_transfer, non_business, reversed_pair or out_of_scope",
            ));
        }
    };
    let version = review::exclude(
        &state.pool,
        firm,
        client,
        line,
        session.user.as_uuid(),
        payload.expected_version,
        reason,
        payload.note.as_deref(),
    )
    .await
    .map_err(review_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({"line": {"id": line.as_uuid(), "status": "excluded",
                             "row_version": version}})),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualMatchRequest {
    /// The document ids the line pays; each allocates its open remainder.
    documents: Vec<Uuid>,
    /// Named residuals (`bank_fee`, `tax_withheld`, `rounding`) making the difference.
    #[serde(default)]
    residuals: Vec<ManualResidual>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualResidual {
    /// `bank_fee`, `tax_withheld` or `rounding`.
    code: String,
    /// Signed minor units.
    amount_minor: i64,
}

/// `POST …/lines/{line}/manual-match` — a person's own balanced match, always a
/// four-eyes `proposed` match; the second eye approves it like any other.
pub async fn manual_match(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, line)): Path<(String, String, String)>,
    payload: Result<Json<ManualMatchRequest>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let line = LineId::from_uuid(line.parse().map_err(|_| ApiError::not_found())?);
    let session = person_authorized(&auth, firm, Act::Resolve)?;
    session.client_scope(&state.pool, client).await?;
    let payload = body(payload)?;
    let mut residuals = Vec::with_capacity(payload.residuals.len());
    for r in &payload.residuals {
        let code = match r.code.as_str() {
            "bank_fee" => ResidualCode::BankFee,
            "tax_withheld" => ResidualCode::TaxWithheld,
            "rounding" => ResidualCode::Rounding,
            _ => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_residual",
                    "a residual code is bank_fee, tax_withheld or rounding",
                ));
            }
        };
        residuals.push((code, r.amount_minor));
    }
    let created = review::manual_match(
        &state.pool,
        firm,
        client,
        line,
        session.user.as_uuid(),
        &payload.documents,
        &residuals,
    )
    .await
    .map_err(review_error)?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"match": {"id": created.match_id, "status": "proposed",
                              "tier": "manual", "four_eyes": true},
                    "line": {"id": line.as_uuid(), "status": "proposed",
                             "row_version": created.line_version}}),
        ),
    ))
}
