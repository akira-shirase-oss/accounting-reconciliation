//! R3 slice 1 (D-021): argon2id passwords, mandatory TOTP with recovery codes, sessions
//! (30 min idle inside 12 h absolute, rotated at every sign-in, SHA-256 digests stored),
//! CSRF on mutations, the sign-in back-off and the role gate on user management — in
//! process over a real database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a panic is the failure report"
)]

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_db::testing::TestDb;
use recon_server::auth::totp_code;
use recon_server::{AppState, Config, faults::Faults, router};
use serde_json::{Value, json};
use sha2::Digest as _;
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "r3-auth-test-operator-token-0123456789abcdef";

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 51_001))
}

fn checked(db: &TestDb) -> AppState {
    let url = db.app_url();
    let config = Config::from_source(|name| match name {
        "RECON_DATABASE_URL" => Some(url.clone()),
        "RECON_OPERATOR_TOKEN" => Some(TOKEN.to_owned()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(db.pool.clone(), config, Faults::default());
    state.mark_checked();
    state
}

/// `oneshot` with the peer injected the way the serve loop does.
async fn call(state: &AppState, mut request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    request.extensions_mut().insert(ConnectInfo(peer()));
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn post_json(path: &str, body: &Value) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

fn operator(request: Request<Body>) -> Request<Body> {
    let mut request = request;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    request
}

fn session(request: Request<Body>, token: &str, csrf: Option<&str>) -> Request<Body> {
    let mut request = request;
    request.headers_mut().insert(
        header::COOKIE,
        format!("recon_session={token}").parse().unwrap(),
    );
    if let Some(csrf) = csrf {
        request
            .headers_mut()
            .insert("x-csrf-token", csrf.parse().unwrap());
    }
    request
}

async fn new_firm(state: &AppState) -> String {
    let (s, _, body) = call(
        state,
        operator(post_json("/v1/firms", &json!({"name": "R3 Firm"}))),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    body["firm"]["id"].as_str().unwrap().to_owned()
}

async fn new_client(state: &AppState, firm: &str) -> String {
    let (s, _, body) = call(
        state,
        operator(post_json(
            &format!("/v1/firms/{firm}/clients"),
            &json!({"name": "Client"}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    body["client"]["id"].as_str().unwrap().to_owned()
}

async fn make_user(
    state: &AppState,
    firm: &str,
    email: &str,
    role: &str,
    password: &str,
    clients: &[&str],
) -> String {
    let (s, _, body) = call(
        state,
        operator(post_json(
            &format!("/v1/firms/{firm}/users"),
            &json!({
                "email": email,
                "display_name": "Test User",
                "role": role,
                "password": password,
                "clients": clients,
            }),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    body["user"]["id"].as_str().unwrap().to_owned()
}

async fn sign_in(state: &AppState, firm: &str, email: &str, password: &str) -> (StatusCode, Value) {
    let (s, _, body) = call(
        state,
        post_json(
            "/v1/auth/sign-in",
            &json!({"firm": firm, "email": email, "password": password}),
        ),
    )
    .await;
    (s, body)
}

async fn totp(state: &AppState, pending: &str, code: &str) -> (StatusCode, HeaderMap, Value) {
    call(
        state,
        post_json("/v1/auth/totp", &json!({"pending": pending, "code": code})),
    )
    .await
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// RFC 4648 base32 without padding (the TOTP enrolment secret's encoding).
fn base32_decode(text: &str) -> Vec<u8> {
    let mut bits: u64 = 0;
    let mut len = 0_u32;
    let mut out = Vec::new();
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => panic!("not base32: {text}"),
        };
        bits = (bits << 5) | u64::from(v);
        len += 5;
        if len >= 8 {
            out.push(((bits >> (len - 8)) & 0xff) as u8);
            len -= 8;
        }
    }
    out
}

fn cookie_of(headers: &HeaderMap) -> String {
    let set = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let cookie = set.split(';').next().unwrap();
    cookie.strip_prefix("recon_session=").unwrap().to_owned()
}

/// The user's stored TOTP secret (test visibility only — a client never sees it after
/// enrolment).
async fn stored_secret(admin: &PgPool, firm: &str, email: &str) -> Vec<u8> {
    sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT totp_secret FROM recon.users WHERE firm_id = $1 AND email = $2",
    )
    .bind(firm.parse::<Uuid>().unwrap())
    .bind(email)
    .fetch_one(admin)
    .await
    .unwrap()
}

/// A fully signed-in user: enrolment when the account needs it, then a session token and
/// its csrf — `(token, csrf, totp_body)`.
async fn enrolled(
    state: &AppState,
    admin: &PgPool,
    firm: &str,
    email: &str,
    password: &str,
) -> (String, String, Value) {
    let (s, body) = sign_in(state, firm, email, password).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let pending = body["pending"].as_str().unwrap().to_owned();
    let secret = if body["totp_enrolment"].is_null() {
        stored_secret(admin, firm, email).await
    } else {
        base32_decode(body["totp_enrolment"]["secret"].as_str().unwrap())
    };
    let code = format!("{:06}", totp_code(&secret, now()));
    let (s, headers, body) = totp(state, &pending, &code).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    (
        cookie_of(&headers),
        body["session"]["csrf"].as_str().unwrap().to_owned(),
        body,
    )
}

/// The sign-in dance in one test: enrolment issues the recovery codes once, the session
/// answers `whoami`, and the user id is the created one.
#[tokio::test]
async fn a_user_signs_in_with_totp_and_the_session_answers() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let firm = new_firm(&state).await;
    let user = make_user(
        &state,
        &firm,
        "owner@firm.test",
        "owner",
        "correct-horse-battery",
        &[],
    )
    .await;

    let (s, body) = sign_in(&state, &firm, "owner@firm.test", "correct-horse-battery").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let pending = body["pending"].as_str().unwrap();
    let enrolment = &body["totp_enrolment"];
    let secret = base32_decode(enrolment["secret"].as_str().unwrap());
    assert_eq!(secret.len(), 20);
    assert!(
        enrolment["uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/")
    );

    let code = format!("{:06}", totp_code(&secret, now()));
    let (s, headers, body) = totp(&state, pending, &code).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["recovery_codes"].as_array().unwrap().len(), 10);
    let cookie = cookie_of(&headers);
    assert_eq!(body["session"]["user"].as_str().unwrap(), user);
    assert_eq!(body["session"]["role"].as_str().unwrap(), "owner");

    let (s, _, body) = call(&state, session(get("/v1/auth/session"), &cookie, None)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["user"].as_str().unwrap(), user);
    assert_eq!(body["role"].as_str().unwrap(), "owner");
    db.cleanup().await;
}

/// The password alone never opens a session: pending rows cap at three tries, a wrong
/// code fails plainly, and an expired or replayed pending token answers `sign_in_expired`.
#[tokio::test]
async fn a_user_cannot_act_before_totp_enrolment() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let firm = new_firm(&state).await;
    make_user(
        &state,
        &firm,
        "p@firm.test",
        "preparer",
        "correct-horse-battery",
        &[],
    )
    .await;

    // No session, no routes — even the session probe itself.
    let (s, _, body) = call(&state, get("/v1/auth/session")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"].as_str().unwrap(), "session_required");

    let (s, body) = sign_in(&state, &firm, "p@firm.test", "correct-horse-battery").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let pending = body["pending"].as_str().unwrap().to_owned();
    let secret = base32_decode(body["totp_enrolment"]["secret"].as_str().unwrap());
    // A code that is wrong even inside the ±1 window: the real one plus one (mod 10^6).
    let wrong = format!(
        "{:06}",
        totp_code(&secret, now()).wrapping_add(1) % 1_000_000
    );

    // Three wrong codes: the first two say so, the third destroys the pending sign-in.
    for attempt in 1..=3 {
        let (s, _, body) = totp(&state, &pending, &wrong).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
        let code = body["error"]["code"].as_str().unwrap();
        assert_eq!(
            code,
            if attempt < 3 {
                "invalid_code"
            } else {
                "sign_in_expired"
            },
            "attempt {attempt}"
        );
    }
    let (s, _, body) = totp(&state, &pending, &wrong).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"].as_str().unwrap(), "sign_in_expired");
    db.cleanup().await;
}

/// The back-off: the third failure locks the account key, a correct password still waits
/// the lock out, and a later success clears the count so the lock does not grow forever.
#[tokio::test]
async fn sign_in_back_off_locks_then_a_success_clears() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let admin = db.admin_pool().await;
    let firm = new_firm(&state).await;
    make_user(
        &state,
        &firm,
        "b@firm.test",
        "viewer",
        "correct-horse-battery",
        &[],
    )
    .await;

    for attempt in 1..=3 {
        let (s, body) = sign_in(&state, &firm, "b@firm.test", "wrong-password-here").await;
        if attempt < 3 {
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(
                body["error"]["code"].as_str().unwrap(),
                "invalid_credentials"
            );
        } else {
            assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{body}");
            assert_eq!(body["error"]["code"].as_str().unwrap(), "sign_in_locked");
        }
    }
    // Locked: even the right password waits.
    let (s, body) = sign_in(&state, &firm, "b@firm.test", "correct-horse-battery").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // Move the lock into the past (as the wall clock would), then sign in cleanly.
    sqlx::query("UPDATE recon.sign_in_throttle SET locked_until = now() - interval '1 second'")
        .execute(&admin)
        .await
        .unwrap();
    let (s, body) = sign_in(&state, &firm, "b@firm.test", "correct-horse-battery").await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // The count cleared: two fresh wrong passwords still only warn.
    for _ in 0..2 {
        let (s, body) = sign_in(&state, &firm, "b@firm.test", "wrong-password-here").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    }
    db.cleanup().await;
}

/// Mutations under a session owe the CSRF header; the cookie alone is not enough, and a
/// session never reaches another firm's routes.
#[tokio::test]
async fn the_session_gate_enforces_csrf_and_tenancy() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let admin = db.admin_pool().await;
    let firm = new_firm(&state).await;
    let other = new_firm(&state).await;
    make_user(
        &state,
        &firm,
        "o@firm.test",
        "owner",
        "correct-horse-battery",
        &[],
    )
    .await;
    let (cookie, csrf, _) = enrolled(
        &state,
        &admin,
        &firm,
        "o@firm.test",
        "correct-horse-battery",
    )
    .await;
    let create = |email: &str| {
        post_json(
            &format!("/v1/firms/{firm}/users"),
            &json!({"email": email, "display_name": "N", "role": "viewer",
                    "password": "correct-horse-battery", "clients": []}),
        )
    };

    // No CSRF on a mutation: refused.
    let (s, _, body) = call(&state, session(create("a@f.t"), &cookie, None)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"].as_str().unwrap(), "csrf_mismatch");

    // A wrong CSRF is refused too.
    let (s, _, body) = call(
        &state,
        session(create("b@f.t"), &cookie, Some(&"00".repeat(32))),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");

    // With it: created.
    let (s, _, body) = call(&state, session(create("n@firm.test"), &cookie, Some(&csrf))).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // Another firm's route hides as 404.
    let (s, _, body) = call(
        &state,
        session(
            post_json(
                &format!("/v1/firms/{other}/users"),
                &json!({"email": "x@firm.test", "display_name": "X", "role": "viewer",
                        "password": "correct-horse-battery", "clients": []}),
            ),
            &cookie,
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    db.cleanup().await;
}

/// Sign-out deletes the session; an idled or disabled user is refused on the next call —
/// three ways a session ends, all checked live.
#[tokio::test]
async fn sessions_end_on_sign_out_idle_and_disable() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let admin = db.admin_pool().await;
    let firm = new_firm(&state).await;
    let user = make_user(
        &state,
        &firm,
        "s@firm.test",
        "owner",
        "correct-horse-battery",
        &[],
    )
    .await;

    // Sign-out.
    let (cookie, _, _) = enrolled(
        &state,
        &admin,
        &firm,
        "s@firm.test",
        "correct-horse-battery",
    )
    .await;
    let (s, _, _) = call(
        &state,
        session(post_json("/v1/auth/sign-out", &json!({})), &cookie, None),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, body) = call(&state, session(get("/v1/auth/session"), &cookie, None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");

    // Idle expiry (the row backdated the way the clock would leave it).
    let (cookie, _, _) = enrolled(
        &state,
        &admin,
        &firm,
        "s@firm.test",
        "correct-horse-battery",
    )
    .await;
    sqlx::query("UPDATE recon.sessions SET idle_expires_at = now() - interval '1 second'")
        .execute(&admin)
        .await
        .unwrap();
    let (s, _, body) = call(&state, session(get("/v1/auth/session"), &cookie, None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");

    // Disabled.
    let (cookie, _, _) = enrolled(
        &state,
        &admin,
        &firm,
        "s@firm.test",
        "correct-horse-battery",
    )
    .await;
    sqlx::query("UPDATE recon.users SET disabled_at = now() WHERE id = $1")
        .bind(user.parse::<Uuid>().unwrap())
        .execute(&admin)
        .await
        .unwrap();
    let (s, _, body) = call(&state, session(get("/v1/auth/session"), &cookie, None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");

    // And a disabled user does not sign in at all.
    let (s, body) = sign_in(&state, &firm, "s@firm.test", "correct-horse-battery").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"].as_str().unwrap(), "user_disabled");
    db.cleanup().await;
}

/// A recovery code signs in once: the second use of the same code fails.
#[tokio::test]
async fn recovery_codes_are_single_use() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let admin = db.admin_pool().await;
    let firm = new_firm(&state).await;
    make_user(
        &state,
        &firm,
        "r@firm.test",
        "owner",
        "correct-horse-battery",
        &[],
    )
    .await;
    let (_, _, first) = enrolled(
        &state,
        &admin,
        &firm,
        "r@firm.test",
        "correct-horse-battery",
    )
    .await;
    let code = first["recovery_codes"][0].as_str().unwrap().to_owned();
    assert_eq!(first["recovery_codes_left"].as_i64().unwrap(), 10);

    // A fresh sign-in answers with the recovery code instead of the TOTP.
    let (s, body) = sign_in(&state, &firm, "r@firm.test", "correct-horse-battery").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body["totp_enrolment"].is_null());
    let pending = body["pending"].as_str().unwrap().to_owned();
    let (s, headers, body) = totp(&state, &pending, &code).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["recovery_codes_left"].as_i64().unwrap(), 9);
    assert!(headers.contains_key(header::SET_COOKIE));

    // The same code never works twice.
    let (s, body) = sign_in(&state, &firm, "r@firm.test", "correct-horse-battery").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let pending = body["pending"].as_str().unwrap().to_owned();
    let (s, _, body) = totp(&state, &pending, &code).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"].as_str().unwrap(), "invalid_code");
    db.cleanup().await;
}

/// The role gate on `users` itself: an owner creates users, a preparer may not, the
/// operator may (provisioning).
#[tokio::test]
async fn only_an_owner_manages_users() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let admin = db.admin_pool().await;
    let firm = new_firm(&state).await;
    make_user(
        &state,
        &firm,
        "prep@firm.test",
        "preparer",
        "correct-horse-battery",
        &[],
    )
    .await;
    let (cookie, csrf, _) = enrolled(
        &state,
        &admin,
        &firm,
        "prep@firm.test",
        "correct-horse-battery",
    )
    .await;
    let (s, _, body) = call(
        &state,
        session(
            post_json(
                &format!("/v1/firms/{firm}/users"),
                &json!({"email": "v@firm.test", "display_name": "V", "role": "viewer",
                        "password": "correct-horse-battery", "clients": []}),
            ),
            &cookie,
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"].as_str().unwrap(), "forbidden");
    db.cleanup().await;
}

/// Client assignment gates the client scope the review routes will use: an unassigned
/// client refuses, an assigned one is recorded in the access log (D-021).
#[tokio::test]
async fn client_assignment_gates_and_the_access_log_records() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let admin = db.admin_pool().await;
    let firm = new_firm(&state).await;
    let client = new_client(&state, &firm).await;
    let stranger = new_client(&state, &firm).await;
    let user = make_user(
        &state,
        &firm,
        "c@firm.test",
        "preparer",
        "correct-horse-battery",
        &[&client],
    )
    .await;
    let (cookie, _, _) = enrolled(
        &state,
        &admin,
        &firm,
        "c@firm.test",
        "correct-horse-battery",
    )
    .await;

    // The session the cookie names, the way the middleware resolves it.
    let mut token = [0_u8; 32];
    hex::decode_to_slice(&cookie, &mut token).unwrap();
    let row = recon_db::auth::session(&state.pool, &sha2::Sha256::digest(token).into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.user.as_uuid().to_string(), user);
    let session = recon_server::auth::Session {
        firm: row.firm,
        user: row.user,
        role: row.role,
        csrf: row.csrf,
    };
    session
        .client_scope(
            &state.pool,
            recon_domain::ClientId::from_uuid(stranger.parse().unwrap()),
        )
        .await
        .expect_err("the unassigned client must refuse");
    session
        .client_scope(
            &state.pool,
            recon_domain::ClientId::from_uuid(client.parse().unwrap()),
        )
        .await
        .expect("the assigned client passes");
    let recorded: i64 =
        sqlx::query_scalar("SELECT requests FROM recon.access_log WHERE client_id = $1")
            .bind(client.parse::<Uuid>().unwrap())
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(recorded, 1);
    db.cleanup().await;
}
