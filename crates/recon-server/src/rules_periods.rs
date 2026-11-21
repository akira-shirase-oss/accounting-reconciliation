//! Rules and periods (R3 slice 3, D-015, D-006): typed rules proposed by a person and
//! approved into immutable versions, the owner's auto-apply mark on exclusions only,
//! period close/lock/reopen and proposal-only re-evaluation. Every mutation is a
//! person with a role, a CSRF'd session and a decision in the same transaction.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use recon_db::review::ReviewError;
use recon_db::{periods, rules};
use recon_domain::auth::{Act, Role};
use recon_domain::rule::{RuleAction, RulePredicate};
use recon_domain::{AccountId, ClientId, FirmId, LineId};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Date;
use uuid::Uuid;

use crate::auth::{Auth, Session, authorize, forbidden_role, person_authorized};
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

fn rule_error(e: rules::RuleError) -> ApiError {
    match e {
        rules::RuleError::NotFound => ApiError::not_found(),
        rules::RuleError::Invalid(message) => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_rule", message)
        }
        rules::RuleError::Conflict(message) => {
            ApiError::new(StatusCode::CONFLICT, "conflict", message)
        }
        rules::RuleError::Retired => {
            ApiError::new(StatusCode::CONFLICT, "rule_retired", "the rule is retired")
        }
        rules::RuleError::Db(db) => ApiError::from(db),
    }
}

fn period_error(e: periods::PeriodError) -> ApiError {
    match e {
        periods::PeriodError::NotFound => ApiError::not_found(),
        periods::PeriodError::Conflict(message) => {
            ApiError::new(StatusCode::CONFLICT, "conflict", message)
        }
        periods::PeriodError::Closed => ApiError::new(
            StatusCode::CONFLICT,
            "period_closed",
            "the period is closed",
        ),
        periods::PeriodError::UndecidedLines(count) => ApiError::new(
            StatusCode::CONFLICT,
            "period_has_undecided_lines",
            "lines in the month are still unreviewed or proposed",
        )
        .with("count", json!(count)),
        periods::PeriodError::Unacknowledged(count) => ApiError::new(
            StatusCode::CONFLICT,
            "period_has_unacknowledged_exceptions",
            "exceptions in the month lack a reviewer's acknowledgement",
        )
        .with("count", json!(count)),
        periods::PeriodError::AccountBusy => ApiError::new(
            StatusCode::CONFLICT,
            "account_busy",
            "an import into this account is being published; nothing changed, try again shortly",
        )
        .retry_after(2),
        periods::PeriodError::ReEvaluationBusy => ApiError::new(
            StatusCode::CONFLICT,
            "re_evaluation_busy",
            "a re-evaluation is already running for this firm; try again shortly",
        )
        .retry_after(2),
        periods::PeriodError::EvaluationTooLarge => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "period_evaluation_too_large",
            "re-evaluating the month would scan more work than the bound allows; split the rules or the month",
        ),
        periods::PeriodError::ReasonRequired => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reason_required",
            "reopening a period takes a non-empty reason",
        ),
        periods::PeriodError::Db(db) => ApiError::from(db),
    }
}

fn path_ids(firm: &str, client: &str) -> Result<(FirmId, ClientId), ApiError> {
    Ok((
        FirmId::from_uuid(firm.parse().map_err(|_| ApiError::not_found())?),
        ClientId::from_uuid(client.parse().map_err(|_| ApiError::not_found())?),
    ))
}

fn account_id(account: &str) -> Result<AccountId, ApiError> {
    Ok(AccountId::from_uuid(
        account.parse().map_err(|_| ApiError::not_found())?,
    ))
}

/// The `{month}` path part (`YYYY-MM`) as the first of the month.
fn month(month: &str) -> Result<Date, ApiError> {
    let Some((year, month)) = month.split_once('-') else {
        return Err(ApiError::not_found());
    };
    let (Ok(year), Ok(month)) = (year.parse::<i32>(), month.parse::<u8>()) else {
        return Err(ApiError::not_found());
    };
    let month = time::Month::try_from(month).map_err(|_| ApiError::not_found())?;
    Date::from_calendar_date(year, month, 1).map_err(|_| ApiError::not_found())
}

/// A person authorized for `act`, scoped to `client` — the shared gate every mutation
/// here needs.
async fn scoped<'a>(
    state: &AppState,
    auth: &'a Auth,
    firm: FirmId,
    client: ClientId,
    act: Act,
) -> Result<&'a Session, ApiError> {
    let session = person_authorized(auth, firm, act)?;
    session.client_scope(&state.pool, client).await?;
    Ok(session)
}

fn period_json(p: &periods::Period) -> Value {
    json!({
        "id": p.id, "status": p.status, "month": p.month.to_string(),
        "row_version": p.row_version,
        "chain_seq": p.chain_seq,
        "digest": p.digest.as_ref().map(hex::encode),
        "chain_head": p.chain_head.as_ref().map(hex::encode),
    })
}

// --- Rules ----------------------------------------------------------------------------------

/// A stored rule's JSON, versions included.
fn rule_json(r: &rules::Rule) -> Value {
    let versions: Vec<Value> = r
        .versions
        .iter()
        .map(|v| {
            json!({
                "version": v.version, "predicate": v.predicate, "action": v.action,
                "auto_apply": v.auto_apply, "status": v.status,
                "proposed_by": v.proposed_by, "approved_by": v.approved_by,
            })
        })
        .collect();
    json!({
        "id": r.id, "name": r.name, "status": r.status,
        "active_version": r.active_version, "versions": versions,
    })
}

/// `GET …/rules` — the client's rules and their versions (read is any role's right).
pub async fn list_rules(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client)): Path<(String, String)>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    if let Some(session) = authorize(&auth, firm, Act::Read)? {
        session.client_scope(&state.pool, client).await?;
    }
    let items: Vec<Value> = rules::list(&state.pool, firm, client)
        .await?
        .iter()
        .map(rule_json)
        .collect();
    Ok((StatusCode::OK, Json(json!({"items": items}))))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDefinition {
    /// The rule's display name (1-200 bytes).
    name: String,
    /// The typed predicate.
    predicate: RulePredicate,
    /// What a matched rule does.
    action: RuleAction,
}

/// `POST …/rules` — a rule with version 1 `proposed`, from a person with
/// `ManageRules` (a reviewer or owner).
pub async fn create_rule(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client)): Path<(String, String)>,
    payload: Result<Json<RuleDefinition>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let session = scoped(&state, &auth, firm, client, Act::ManageRules).await?;
    let payload = body(payload)?;
    let rule = rules::create(
        &state.pool,
        firm,
        client,
        session.user,
        &payload.name,
        &payload.predicate,
        &payload.action,
    )
    .await
    .map_err(rule_error)?;
    Ok((StatusCode::CREATED, Json(json!({"rule": rule_json(&rule)}))))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionDefinition {
    /// The typed predicate.
    predicate: RulePredicate,
    /// What a matched rule does.
    action: RuleAction,
}

/// `POST …/rules/{rule}/versions` — the next numbered version, `proposed`.
pub async fn propose_version(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, rule)): Path<(String, String, String)>,
    payload: Result<Json<VersionDefinition>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let rule: Uuid = rule.parse().map_err(|_| ApiError::not_found())?;
    let session = scoped(&state, &auth, firm, client, Act::ManageRules).await?;
    let payload = body(payload)?;
    let version = rules::propose_version(
        &state.pool,
        firm,
        client,
        rule,
        session.user,
        &payload.predicate,
        &payload.action,
    )
    .await
    .map_err(rule_error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"rule": {"id": rule, "version": version, "status": "proposed"}})),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// The owner's automatic-apply mark — exclusions only, owner only (D-015).
    #[serde(default)]
    auto_apply: bool,
}

/// `POST …/rules/{rule}/versions/{version}/approve` — the version goes `approved` and
/// becomes the rule's active one. `auto_apply` is the owner's mark only.
pub async fn approve_version(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, rule, version)): Path<(String, String, String, i32)>,
    payload: Result<Json<Approval>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let rule: Uuid = rule.parse().map_err(|_| ApiError::not_found())?;
    let session = scoped(&state, &auth, firm, client, Act::ManageRules).await?;
    let payload = body(payload)?;
    if payload.auto_apply && session.role != Role::Owner {
        return Err(forbidden_role());
    }
    rules::approve(
        &state.pool,
        firm,
        client,
        rule,
        version,
        session.user,
        payload.auto_apply,
    )
    .await
    .map_err(rule_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({"rule": {"id": rule, "version": version, "status": "approved"}})),
    ))
}

/// `POST …/rules/{rule}/retire` — the rule stops applying and takes no new versions.
pub async fn retire_rule(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, rule)): Path<(String, String, String)>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let rule: Uuid = rule.parse().map_err(|_| ApiError::not_found())?;
    let session = scoped(&state, &auth, firm, client, Act::ManageRules).await?;
    rules::retire(&state.pool, firm, client, rule, session.user)
        .await
        .map_err(rule_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({"rule": {"id": rule, "status": "retired"}})),
    ))
}

// --- Periods --------------------------------------------------------------------------------

/// `GET …/accounts/{account}/periods` — the account's periods, month order.
pub async fn list_periods(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, account)): Path<(String, String, String)>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let account = account_id(&account)?;
    if let Some(session) = authorize(&auth, firm, Act::Read)? {
        session.client_scope(&state.pool, client).await?;
    }
    let items: Vec<Value> = periods::list(&state.pool, firm, client, account)
        .await?
        .iter()
        .map(period_json)
        .collect();
    Ok((StatusCode::OK, Json(json!({"items": items}))))
}

/// `POST …/accounts/{account}/periods/{month}/close` — refuses while the month holds
/// undecided lines or unacknowledged exceptions; otherwise records the digest and
/// chain head and locks the month (D-006).
pub async fn close_period(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, account, m)): Path<(String, String, String, String)>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let (account, m) = (account_id(&account)?, month(&m)?);
    let session = scoped(&state, &auth, firm, client, Act::ClosePeriod).await?;
    let period = periods::close(&state.pool, firm, client, account, m, session.user)
        .await
        .map_err(period_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({"period": period_json(&period)})),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReopenRequest {
    /// Why the month opens again — required, and kept in the decision's evidence.
    reason: String,
}

/// `POST …/accounts/{account}/periods/{month}/reopen` — owner only, always with a
/// reason, itself a decision (D-015).
pub async fn reopen_period(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, account, m)): Path<(String, String, String, String)>,
    payload: Result<Json<ReopenRequest>, JsonRejection>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let (account, m) = (account_id(&account)?, month(&m)?);
    let session = scoped(&state, &auth, firm, client, Act::ReopenPeriod).await?;
    let payload = body(payload)?;
    let period = periods::reopen(
        &state.pool,
        firm,
        client,
        account,
        m,
        session.user,
        &payload.reason,
    )
    .await
    .map_err(period_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({"period": period_json(&period)})),
    ))
}

/// `POST …/accounts/{account}/periods/{month}/re-evaluate` — the period's undecided
/// lines against the rules now in force: a proposal diff, never a commit. A closed
/// period refuses until an owner reopens it (D-015).
pub async fn re_evaluate(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, account, m)): Path<(String, String, String, String)>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let (account, m) = (account_id(&account)?, month(&m)?);
    let session = scoped(&state, &auth, firm, client, Act::ManageRules).await?;
    let diff = periods::re_evaluate(&state.pool, firm, client, account, m, session.user)
        .await
        .map_err(period_error)?;
    let proposals: Vec<Value> = diff
        .proposals
        .iter()
        .map(|p| {
            json!({"line": p.line, "rule": p.rule, "version": p.version,
                   "action": p.action})
        })
        .collect();
    Ok((
        StatusCode::OK,
        Json(json!({
            "period": {"id": diff.period, "status": diff.status},
            "evaluated": diff.evaluated, "proposals": proposals,
            "truncated": diff.truncated,
        })),
    ))
}

/// `POST …/lines/{line}/acknowledge` — a reviewer acknowledges an exception in place:
/// the line stays an exception, the acknowledgement is a decision on it (D-006).
pub async fn acknowledge(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path((firm, client, line)): Path<(String, String, String)>,
) -> ApiResult {
    let (firm, client) = path_ids(&firm, &client)?;
    let line = LineId::from_uuid(line.parse().map_err(|_| ApiError::not_found())?);
    let session = scoped(&state, &auth, firm, client, Act::AcknowledgeException).await?;
    recon_db::review::acknowledge(&state.pool, firm, client, line, session.user.as_uuid())
        .await
        .map_err(|e| match e {
            ReviewError::PeriodClosed => ApiError::new(
                StatusCode::CONFLICT,
                "period_closed",
                "the line's period is closed",
            ),
            other => super::review::review_error(other),
        })?;
    Ok((
        StatusCode::OK,
        Json(json!({"line": {"id": line.as_uuid(), "status": "exception",
                             "acknowledged": true}})),
    ))
}
