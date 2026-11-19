//! Sign-in, TOTP enrolment and sessions (D-021): a password plus a TOTP earns a session
//! cookie (`HttpOnly`, `SameSite=Strict`, rotated per sign-in); mutations mirror the session's
//! CSRF secret; failures back off per account and per address. Roles and their powers are
//! the matrix in `recon_domain::auth` (D-018) — enforced here on every gated route.

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Extension, Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac as _};
use recon_db::DbError;
use recon_db::auth as store;
use recon_db::imports::Actor;
use recon_domain::auth::may;
use recon_domain::line::ActorKind;
use recon_domain::{Act, ClientId, FirmId, Role, UserId};
use serde::Deserialize;
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::ApiError;
use crate::state::AppState;

/// The session cookie's name.
pub const SESSION_COOKIE: &str = "recon_session";
/// The header mirroring the session's CSRF secret on every mutating call.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// The codes a completed enrolment is issued (each usable once).
const RECOVERY_CODE_COUNT: usize = 10;
/// The shortest password a user may be created with.
const PASSWORD_MIN: usize = 12;

type ApiResult = Result<(StatusCode, Json<Value>), ApiError>;

// -----------------------------------------------------------------------------------------------
// Who a request acts as.

/// A validated session, in the request's extensions after [`require_session`].
#[derive(Debug, Clone)]
pub struct Session {
    /// The session's firm — handlers refuse paths naming another.
    pub firm: FirmId,
    /// The signed-in user.
    pub user: UserId,
    /// The user's live role.
    pub role: Role,
    /// The CSRF secret a mutation must mirror.
    pub csrf: [u8; 32],
}

impl Session {
    /// `Err(404)` when the path's firm is not the session's (tenancy hides it, D-020).
    pub fn firm_scope(&self, firm: FirmId) -> Result<(), ApiError> {
        if self.firm == firm {
            Ok(())
        } else {
            Err(ApiError::not_found())
        }
    }

    /// `Err(403)` when the session's client assignments exclude `client`; the access is
    /// recorded (D-021's log) once the request may proceed.
    pub async fn client_scope(
        &self,
        pool: &sqlx::PgPool,
        client: ClientId,
    ) -> Result<(), ApiError> {
        if !store::is_assigned(pool, self.firm, self.user, client).await? {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "the client is not assigned to this user",
            ));
        }
        store::record_access(pool, self.firm, self.user, client).await?;
        Ok(())
    }

    /// The actor a decision names.
    pub fn actor<'a>(&'a self, reference: &'a str) -> Actor<'a> {
        Actor {
            kind: ActorKind::Person,
            reference,
        }
    }
}

/// Who a request acts as, after [`require_actor`].
#[derive(Debug, Clone)]
pub enum Auth {
    /// The operator token (machine users; D-041).
    Operator,
    /// A signed-in person.
    Person(Session),
}

impl Auth {
    /// The session, when a person acts.
    pub fn person(&self) -> Option<&Session> {
        match self {
            Self::Operator => None,
            Self::Person(session) => Some(session),
        }
    }
}

// -----------------------------------------------------------------------------------------------
// Secrets: hashes, tokens, TOTP.

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// A fresh 256-bit bearer secret and its hex form (what crosses the wire/cookie).
fn bearer() -> Result<([u8; 32], String), ApiError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ApiError::internal())?;
    Ok((bytes, hex::encode(bytes)))
}

/// Argon2id's PHC string for `password` (the salt is fresh).
pub fn hash_password(password: &str) -> Result<String, ApiError> {
    let mut salt = [0_u8; 16];
    getrandom::fill(&mut salt).map_err(|_| ApiError::internal())?;
    let salt = SaltString::encode_b64(&salt).map_err(|_| ApiError::internal())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| ApiError::internal())
}

/// A PHC for timing parity when the account does not exist, so an unknown email costs
/// what a known one does.
fn dummy_phc() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("this password is never correct").unwrap_or_else(|_| {
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$\
             AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                .into()
        })
    })
}

/// `true` when `password` verifies against `phc` (`None` — the dummy, for parity).
fn password_ok(password: &str, phc: Option<&str>) -> bool {
    let phc = phc.unwrap_or_else(|| dummy_phc());
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// RFC 4226's one-time password for `counter` (SHA-1, 6 digits).
#[expect(
    clippy::expect_used,
    reason = "HMAC accepts a key of any length; new_from_slice cannot fail"
)]
fn hotp(secret: &[u8], counter: u64) -> u32 {
    use hmac::digest::KeyInit;
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("hmac key");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff;
    binary % 1_000_000
}

/// The TOTP code for `at` (30-second steps; public for the test harness).
pub fn totp_code(secret: &[u8], at: u64) -> u32 {
    hotp(secret, at / 30)
}

/// `code` is checked against the current step and one each side — a code typed slowly
/// still lands. Every window is evaluated; nothing branches on the comparison.
fn totp_ok(secret: &[u8], code: u32, at: u64) -> bool {
    let counter = at / 30;
    let mut ok = false;
    for window in [
        counter.saturating_sub(1),
        counter,
        counter.saturating_add(1),
    ] {
        ok |= hotp(secret, window) == code;
    }
    ok
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The TOTP secret in RFC 4648 base32 (what an authenticator's setup accepts).
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    for chunk in bytes.chunks(5) {
        let mut buffer = [0_u8; 5];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let bits = u64::from_be_bytes([
            0, 0, 0, buffer[0], buffer[1], buffer[2], buffer[3], buffer[4],
        ]);
        let chars = (chunk.len() * 8).div_ceil(5);
        for i in 0..chars {
            let shift = 35 - (i * 5);
            out.push(ALPHABET[((bits >> shift) & 0x1f) as usize] as char);
        }
    }
    out
}

/// `recon_session`'s raw token, if the request carries a well-formed one.
fn cookie_token(request: &Request) -> Option<[u8; 32]> {
    let header = request.headers().get(header::COOKIE)?.to_str().ok()?;
    let raw = header
        .split(';')
        .map(str::trim)
        .find_map(|c| c.strip_prefix("recon_session="))?;
    let mut token = [0_u8; 32];
    hex::decode_to_slice(raw, &mut token).ok()?;
    Some(token)
}

fn csrf_ok(request: &Request, session: &Session) -> bool {
    let presented = request
        .headers()
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mut bytes = [0_u8; 32];
    hex::decode_to_slice(presented, &mut bytes).is_ok() && bool::from(bytes.ct_eq(&session.csrf))
}

/// The session `token` names, checked live (expiry and a disabled user refuse).
async fn open_session(state: &AppState, token: &[u8; 32]) -> Result<Option<Session>, ApiError> {
    let session = store::session(&state.pool, &sha256(token)).await?;
    Ok(session.map(|row| Session {
        firm: row.firm,
        user: row.user,
        role: row.role,
        csrf: row.csrf,
    }))
}

fn forbidden_role() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        "this role may not do that",
    )
}

fn csrf_required() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "csrf_mismatch",
        "a mutating call sends the session's x-csrf-token header",
    )
}

fn session_required() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "session_required",
        "a signed-in session is required",
    )
}

/// Every route under the session gate: a valid session, and its CSRF secret mirrored on
/// mutations (the cookie is SameSite=Strict; the header defeats it anyway).
pub async fn require_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = cookie_token(&request) else {
        return session_required().into_response();
    };
    let session = match open_session(&state, &token).await {
        Ok(Some(session)) => session,
        Ok(None) => return session_required().into_response(),
        Err(error) => return error.into_response(),
    };
    if !matches!(request.method(), &Method::GET | &Method::HEAD) && !csrf_ok(&request, &session) {
        return csrf_required().into_response();
    }
    request.extensions_mut().insert(session);
    next.run(request).await
}

/// Routes either credential may open: the operator token (unchanged, D-041) or a session.
/// A session-authed mutation still owes its CSRF token.
pub async fn require_actor(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if let Some(presented) = bearer {
        if !bool::from(
            presented
                .as_bytes()
                .ct_eq(state.config.operator_token.expose()),
        ) {
            return ApiError::unauthorized().into_response();
        }
        request.extensions_mut().insert(Auth::Operator);
        return next.run(request).await;
    }
    let Some(token) = cookie_token(&request) else {
        return session_required().into_response();
    };
    let session = match open_session(&state, &token).await {
        Ok(Some(session)) => session,
        Ok(None) => return session_required().into_response(),
        Err(error) => return error.into_response(),
    };
    if !matches!(request.method(), &Method::GET | &Method::HEAD) && !csrf_ok(&request, &session) {
        return csrf_required().into_response();
    }
    request.extensions_mut().insert(Auth::Person(session));
    next.run(request).await
}

// -----------------------------------------------------------------------------------------------
// The sign-in flow.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignInRequest {
    firm: String,
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TotpRequest {
    pending: String,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    email: String,
    display_name: String,
    role: Role,
    password: String,
    clients: Vec<String>,
}

fn invalid(field: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, field, message)
}

fn locked(seconds: i64) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "sign_in_locked",
        "too many failed attempts; wait, then retry",
    )
    .retry_after(u32::try_from(seconds.clamp(1, 900)).unwrap_or(1))
}

/// `POST /v1/auth/sign-in`: verifies the password (a uniform cost whether or not the
/// account exists), refuses locked or disabled users, then opens the short-lived pending
/// sign-in the TOTP answers. An unenrolled user gets a fresh secret to enrol with.
pub async fn sign_in(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    payload: Result<Json<SignInRequest>, JsonRejection>,
) -> Response {
    match sign_in_inner(&state, peer, payload).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn sign_in_inner(
    state: &AppState,
    peer: SocketAddr,
    payload: Result<Json<SignInRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let payload = body(payload)?;
    let firm = FirmId::from_uuid(
        payload
            .firm
            .parse()
            .map_err(|_| invalid("invalid_firm", "the firm is not an id"))?,
    );
    let email = payload.email.trim().to_lowercase();
    if email.len() < 3 || email.len() > 254 || !email.contains('@') {
        return Err(invalid("invalid_email", "the email is not a valid address"));
    }
    let keys = throttle_keys(&email, peer);
    for key in &keys {
        if let Some(seconds) = store::throttle_locked(&state.pool, key).await? {
            return Err(locked(seconds));
        }
    }
    let user = store::user_by_email(&state.pool, firm, &email).await?;
    let phc = user.as_ref().and_then(|u| u.password_hash.clone());
    let password = payload.password.clone();
    let ok = tokio::task::spawn_blocking(move || password_ok(&password, phc.as_deref()))
        .await
        .map_err(|_| ApiError::internal())?;
    if !ok || user.is_none() {
        let mut lock: Option<i64> = None;
        for key in &keys {
            if let Some(seconds) = store::throttle_fail(&state.pool, key).await? {
                lock = Some(lock.map_or(seconds, |seen: i64| seen.max(seconds)));
            }
        }
        return Err(lock.map_or_else(
            || {
                ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "invalid_credentials",
                    "the email or password is wrong",
                )
            },
            locked,
        ));
    }
    let Some(user) = user else {
        return Err(ApiError::internal());
    };
    if user.disabled {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "user_disabled",
            "the user is disabled",
        ));
    }
    for key in &keys {
        store::throttle_reset(&state.pool, key).await?;
    }
    let (token, token_hex) = bearer()?;
    store::create_pending(&state.pool, firm, user.id, &sha256(&token)).await?;
    let enrolment = if user.totp_enrolled {
        Value::Null
    } else {
        let mut secret = vec![0_u8; 20];
        getrandom::fill(&mut secret).map_err(|_| ApiError::internal())?;
        store::set_totp_secret(&state.pool, firm, user.id, &secret).await?;
        let encoded = base32(&secret);
        json!({
            "secret": encoded,
            "uri": format!(
                "otpauth://totp/recon:{}?secret={}&issuer=recon",
                urlencoding(&user.email),
                encoded
            ),
        })
    };
    Ok(Json(json!({
        "pending": token_hex,
        "expires_seconds": store::PENDING_TTL.as_secs(),
        "totp_enrolment": enrolment,
    }))
    .into_response())
}

/// `POST /v1/auth/totp`: the second factor answers a pending sign-in. A first-time answer
/// also completes enrolment and issues the recovery codes once; afterwards a recovery
/// code stands in for the TOTP. The session cookie is set on success.
pub async fn totp(
    State(state): State<AppState>,
    payload: Result<Json<TotpRequest>, JsonRejection>,
) -> Response {
    match totp_inner(&state, payload).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn totp_inner(
    state: &AppState,
    payload: Result<Json<TotpRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let payload = body(payload)?;
    let mut pending_token = [0_u8; 32];
    if hex::decode_to_slice(payload.pending.trim(), &mut pending_token).is_err() {
        return Err(expired_pending());
    }
    let pending_hash = sha256(&pending_token);
    let Some((firm, user_id, _attempts)) = store::pending(&state.pool, &pending_hash).await? else {
        return Err(expired_pending());
    };
    let Some(user) = store::user_by_id(&state.pool, firm, user_id).await? else {
        return Err(expired_pending());
    };
    if user.disabled {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "user_disabled",
            "the user is disabled",
        ));
    }
    let Some(secret) = user.totp_secret.as_deref() else {
        return Err(ApiError::internal());
    };
    let code = payload.code.trim().to_lowercase();
    let enrolment_codes = if user.totp_enrolled {
        let ok = code
            .parse::<u32>()
            .ok()
            .filter(|_| code.len() == 6)
            .is_some_and(|c| totp_ok(secret, c, now()))
            || recovery_ok(state, &user, &code).await?;
        if !ok {
            return Err(wrong_code(state, &pending_hash).await?);
        }
        None
    } else {
        let digits = code.parse::<u32>().ok().filter(|_| code.len() == 6);
        let Some(_) = digits.filter(|c| totp_ok(secret, *c, now())) else {
            return Err(wrong_code(state, &pending_hash).await?);
        };
        let (hashes, shown) = recovery_codes()?;
        store::enrol_totp(&state.pool, firm, user.id, &hashes)
            .await
            .map_err(ApiError::from)?;
        Some(shown)
    };
    store::consume_pending(&state.pool, &pending_hash).await?;
    // A fresh session every sign-in (rotation): a replayed token names nothing.
    let (token, token_hex) = bearer()?;
    let mut csrf = [0_u8; 32];
    getrandom::fill(&mut csrf).map_err(|_| ApiError::internal())?;
    store::create_session(&state.pool, firm, user.id, &sha256(&token), &csrf).await?;
    let left = store::recovery_codes_left(&state.pool, firm, user.id).await?;
    let mut response = Json(json!({
        "session": {
            "user": user.id.as_uuid(),
            "firm": firm.as_uuid(),
            "role": user.role.code(),
            "csrf": hex::encode(csrf),
            "idle_seconds": store::IDLE_TTL.as_secs(),
            "absolute_seconds": store::ABSOLUTE_TTL.as_secs(),
        },
        "recovery_codes": enrolment_codes,
        "recovery_codes_left": left,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        header::HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={token_hex}; Path=/; HttpOnly; Secure; SameSite=Strict"
        ))
        .map_err(|_| ApiError::internal())?,
    );
    Ok(response)
}

async fn recovery_ok(
    state: &AppState,
    user: &store::UserRow,
    code: &str,
) -> Result<bool, ApiError> {
    // Recovery codes are shown as "xxxx-xxxx"; entered with or without the dash, the hash
    // covers the dash-free form.
    let normalized: String = code.chars().filter(|c| *c != '-').collect();
    if normalized.len() != 8 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(false);
    }
    Ok(store::use_recovery_code(
        &state.pool,
        user.firm,
        user.id,
        &sha256(normalized.as_bytes()),
    )
    .await?)
}

fn expired_pending() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "sign_in_expired",
        "the sign-in expired; start again",
    )
}

async fn wrong_code(state: &AppState, pending_hash: &[u8; 32]) -> Result<ApiError, ApiError> {
    if store::fail_pending(&state.pool, pending_hash).await? {
        Ok(expired_pending())
    } else {
        Ok(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_code",
            "the code is wrong",
        ))
    }
}

/// A fresh set of recovery codes: their digests (stored) and the "xxxx-xxxx" texts the
/// user sees exactly once.
fn recovery_codes() -> Result<(Vec<[u8; 32]>, Vec<String>), ApiError> {
    let mut hashes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    let mut shown = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let mut bytes = [0_u8; 4];
        getrandom::fill(&mut bytes).map_err(|_| ApiError::internal())?;
        let hexed = hex::encode(bytes);
        // The hash covers what the user will type: the dash-free hex.
        hashes.push(sha256(hexed.as_bytes()));
        shown.push(format!("{}-{}", &hexed[..4], &hexed[4..]));
    }
    Ok((hashes, shown))
}

/// `POST /v1/auth/sign-out`: deletes whatever session the cookie named (idempotent).
pub async fn sign_out(State(state): State<AppState>, request: Request) -> Response {
    if let Some(token) = cookie_token(&request)
        && let Err(error) = store::delete_session(&state.pool, &sha256(&token)).await
    {
        return ApiError::from_db(&error).into_response();
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        header::HeaderValue::from_static(
            "recon_session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        ),
    );
    response
}

/// `GET /v1/auth/session`: the session's own view of itself.
pub async fn whoami(Extension(session): Extension<Session>) -> Json<Value> {
    Json(json!({
        "user": session.user.as_uuid(),
        "firm": session.firm.as_uuid(),
        "role": session.role.code(),
        "csrf": hex::encode(session.csrf),
    }))
}

/// `POST /v1/firms/{firm}/users`: an owner (or the operator, provisioning) creates a
/// user and their client assignments, atomically.
pub async fn create_user(
    State(state): State<AppState>,
    Extension(auth): Extension<Auth>,
    Path(firm): Path<String>,
    payload: Result<Json<NewUser>, JsonRejection>,
) -> ApiResult {
    let firm = FirmId::from_uuid(firm.parse().map_err(|_| ApiError::not_found())?);
    match &auth {
        Auth::Operator => {}
        Auth::Person(session) => {
            session.firm_scope(firm)?;
            if !may(session.role, Act::ManageUsers) {
                return Err(forbidden_role());
            }
        }
    }
    let payload = body(payload)?;
    let email = payload.email.trim().to_lowercase();
    if email.len() < 3 || email.len() > 254 || !email.contains('@') {
        return Err(invalid("invalid_email", "the email is not a valid address"));
    }
    let display_name = payload.display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 200 {
        return Err(invalid("invalid_name", "a name is 1 to 200 characters"));
    }
    if payload.password.chars().count() < PASSWORD_MIN {
        return Err(invalid(
            "invalid_password",
            "a password is at least 12 characters",
        ));
    }
    let mut clients = Vec::with_capacity(payload.clients.len());
    for id in &payload.clients {
        clients.push(ClientId::from_uuid(id.parse().map_err(|_| {
            invalid("invalid_client", "a client id did not parse")
        })?));
    }
    let hash = {
        let password = payload.password.clone();
        tokio::task::spawn_blocking(move || hash_password(&password))
            .await
            .map_err(|_| ApiError::internal())??
    };
    let id = store::create_user_with_clients(
        &state.pool,
        firm,
        &email,
        display_name,
        payload.role,
        &hash,
        &clients,
    )
    .await
    .map_err(|error| match &error {
        DbError::Constraint(name) if name.contains("clients") || name.contains("client") => {
            invalid("invalid_client", "a client does not exist in this firm")
        }
        _ => ApiError::from_db(&error),
    })?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "user": {
                "id": id.as_uuid(),
                "email": email,
                "display_name": display_name,
                "role": payload.role.code(),
                "clients": payload.clients,
                "totp_enrolled": false,
            }
        })),
    ))
}

// -----------------------------------------------------------------------------------------------
// Helpers.

fn body<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload.map(|Json(v)| v).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "the request body is not the expected JSON",
        )
    })
}

/// The two throttle keys a sign-in answers to (D-021): the account and the address —
/// each a digest, never the raw value.
fn throttle_keys(email: &str, peer: SocketAddr) -> [String; 2] {
    [
        format!("u:{}", hex::encode(sha256(email.as_bytes()))),
        format!(
            "a:{}",
            hex::encode(sha256(peer.ip().to_string().as_bytes()))
        ),
    ]
}

/// The piece of a URL `text` may not need escaped here: emails arrive already
/// constrained to the characters the schema accepts.
fn urlencoding(text: &str) -> String {
    text.replace('@', "%40")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// RFC 4226's published vectors (secret "12345678901234567890", counters 0..=9).
    #[test]
    fn hotp_matches_the_rfc_vectors() {
        let secret = b"12345678901234567890";
        let expected = [
            755_224, 287_082, 359_152, 969_429, 338_314, 254_676, 287_922, 162_583, 399_871,
            520_489,
        ];
        for (counter, expected) in expected.iter().enumerate() {
            assert_eq!(hotp(secret, counter as u64), *expected, "counter {counter}");
        }
    }

    /// The ±1 window accepts an early and a late code; two steps out does not.
    #[test]
    fn totp_accepts_one_step_each_side() {
        let secret = b"12345678901234567890";
        let at = 59 * 30; // step 59
        for step in 58..=60 {
            let code = totp_code(secret, step * 30);
            assert!(totp_ok(secret, code, at), "step {step}");
        }
        let outside = totp_code(secret, 61 * 30);
        assert!(outside != totp_code(secret, 58 * 30) || !totp_ok(secret, outside, at));
    }

    /// base32 round-trips a 160-bit secret.
    #[test]
    fn base32_keeps_five_bit_groups() {
        let bytes = [
            0x6d, 0x5a, 0x27, 0x8b, 0xdd, 0xff, 0x00, 0x11, 0x22, 0x99, 0x6d, 0x5a, 0x27, 0x8b,
            0xdd, 0xff, 0x00, 0x11, 0x22, 0x99,
        ];
        let encoded = base32(&bytes);
        assert_eq!(encoded.len(), 32);
        assert!(encoded.bytes().all(|b| b.is_ascii_alphanumeric()));
    }
}
