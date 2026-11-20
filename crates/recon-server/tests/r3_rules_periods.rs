//! R3 slice 3 (D-015, D-006): rules and periods over person sessions — typed rules
//! proposed and approved into immutable versions with a decision each time, the
//! owner's `auto_apply` mark on exclusions only (applied inside the import's flip),
//! a month that closes only once every line is decided and every exception is
//! acknowledged, the closed month's named refusals on review mutations, imports and
//! re-evaluation, and the owner-only reopen with a reason.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests: a panic is the failure report"
)]

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_db::matching::{self, Decided, DecidedLine, Start};
use recon_db::testing::TestDb;
use recon_domain::line::ExceptionKind;
use recon_server::auth::totp_code;
use recon_server::{AppState, Config, faults::Faults, router};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "r3-rules-test-operator-token-0123456789abcdef";
const LEASE: Duration = Duration::from_secs(30);
const TX_TIMEOUT: Duration = Duration::from_secs(15);
const ENGINE: &str = "match-v1";

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 51_003))
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

async fn call(state: &AppState, mut request: Request<Body>) -> (StatusCode, Value) {
    request.extensions_mut().insert(ConnectInfo(peer()));
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
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

fn upload(path: &str, bytes: &str) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "text/csv")
        .body(Body::from(bytes.to_owned()))
        .unwrap()
}

fn operator(request: Request<Body>) -> Request<Body> {
    let mut request = request;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    request
}

fn session(request: Request<Body>, token: &str, csrf: &str) -> Request<Body> {
    let mut request = request;
    request.headers_mut().insert(
        header::COOKIE,
        format!("recon_session={token}").parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("x-csrf-token", csrf.parse().unwrap());
    request
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Seed {
    firm: String,
    client: String,
    account: String,
}

/// A firm, client and USD account, built through the API by the operator.
async fn seed(state: &AppState) -> Seed {
    let (s, firm) = call(
        state,
        operator(post_json("/v1/firms", &json!({"name": "R3 Firm"}))),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{firm}");
    let firm = firm["firm"]["id"].as_str().unwrap().to_owned();
    let (s, client) = call(
        state,
        operator(post_json(
            &format!("/v1/firms/{firm}/clients"),
            &json!({"name": "Client"}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    let client = client["client"]["id"].as_str().unwrap().to_owned();
    let (s, account) = call(
        state,
        operator(post_json(
            &format!("/v1/firms/{firm}/clients/{client}/accounts"),
            &json!({"name": "Current", "currency": "USD"}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{account}");
    Seed {
        firm,
        client,
        account: account["account"]["id"].as_str().unwrap().to_owned(),
    }
}

/// One user of `role`, assigned to the seed's client, signed in and TOTP-enrolled —
/// `(token, csrf, user_id)`.
async fn enrolled(
    state: &AppState,
    admin: &PgPool,
    seed: &Seed,
    role: &str,
) -> (String, String, String) {
    let email = format!("{role}-{}@example.test", &seed.firm[..8]);
    let (s, body) = call(
        state,
        operator(post_json(
            &format!("/v1/firms/{}/users", seed.firm),
            &json!({
                "email": email,
                "display_name": role,
                "role": role,
                "password": "a-fine-password-123",
                "clients": [seed.client],
            }),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let user = body["user"]["id"].as_str().unwrap().to_owned();
    let (s, body) = call(
        state,
        post_json(
            "/v1/auth/sign-in",
            &json!({"firm": seed.firm, "email": email, "password": "a-fine-password-123"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let secret: Vec<u8> =
        sqlx::query_scalar("SELECT totp_secret FROM recon.users WHERE firm_id = $1 AND email = $2")
            .bind(Uuid::parse_str(&seed.firm).unwrap())
            .bind(&email)
            .fetch_one(admin)
            .await
            .unwrap();
    let code = format!("{:06}", totp_code(&secret, now()));
    let (s, headers, body) = {
        let mut request = post_json(
            "/v1/auth/totp",
            &json!({"pending": body["pending"].as_str().unwrap(), "code": code}),
        );
        request.extensions_mut().insert(ConnectInfo(peer()));
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            headers,
            serde_json::from_slice::<Value>(&bytes).unwrap(),
        )
    };
    assert_eq!(s, StatusCode::OK, "{body:?}");
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("recon_session=")
        .unwrap()
        .to_owned();
    (
        cookie,
        body["session"]["csrf"].as_str().unwrap().to_owned(),
        user,
    )
}

/// `days.len()` March 2026 statement rows, each narrated `{fee_tag} <day>`.
fn statement(days: &[u32], fee_tag: &str) -> String {
    let mut rows = String::from(
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n",
    );
    for day in days {
        writeln!(
            rows,
            "2026-03-{day:02},,-1.00,USD,{fee_tag} {day},REF-{day},Big Bank,"
        )
        .unwrap();
    }
    rows
}

/// The `narration contains <value>` → `exclude` body the rule API takes.
fn exclude_rule(name: &str, narration: &str, reason: &str) -> Value {
    json!({
        "name": name,
        "predicate": {"narration": {"op": "contains", "value": narration}},
        "action": {"kind": "exclude", "reason": reason},
    })
}

/// The seed's statement lines — `(id, status, row_version)`, oldest row first.
async fn lines(pool: &PgPool, seed: &Seed) -> Vec<(String, String, i64)> {
    sqlx::query_as(
        "SELECT id::text, status, row_version FROM recon.statement_lines
          WHERE firm_id = $1 AND account_id = $2 ORDER BY booking_date, row_no",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&seed.account).unwrap())
    .fetch_all(pool)
    .await
    .unwrap()
}

fn rules_path(seed: &Seed) -> String {
    format!("/v1/firms/{}/clients/{}/rules", seed.firm, seed.client)
}

fn period_path(seed: &Seed, verb: &str) -> String {
    format!(
        "/v1/firms/{}/clients/{}/accounts/{}/periods/2026-03/{verb}",
        seed.firm, seed.client, seed.account
    )
}

/// Uploads `statement` through the operator (imports are an operator act in tests).
async fn upload_statement(state: &AppState, seed: &Seed, csv: &str) -> (StatusCode, Value) {
    call(
        state,
        operator(upload(
            &format!("/v1/firms/{}/accounts/{}/imports", seed.firm, seed.account),
            csv,
        )),
    )
    .await
}

/// A reviewer creates and approves rules; only an owner may mark `auto_apply`, and a
/// retired rule takes no more versions.
#[tokio::test]
async fn rules_move_through_their_roles_and_lock() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    let (rt, rc, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;
    let (pt, pc, _) = enrolled(&state, &db.pool, &seed, "preparer").await;
    let (ot, oc, _) = enrolled(&state, &db.pool, &seed, "owner").await;

    // A preparer may not manage rules at all.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &rules_path(&seed),
                &exclude_rule("fees", "bank fee", "non_business"),
            ),
            &pt,
            &pc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");

    // A reviewer creates: version 1 is proposed, nothing is active yet.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &rules_path(&seed),
                &exclude_rule("fees", "bank fee", "non_business"),
            ),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let rule = body["rule"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["rule"]["status"], "active");
    assert_eq!(body["rule"]["versions"][0]["status"], "proposed");
    assert_eq!(body["rule"]["versions"][0]["version"], 1);

    let approve_path =
        |version: i32| format!("{}/{rule}/versions/{version}/approve", rules_path(&seed));

    // A reviewer approves — the owner's mark is not theirs to give.
    let (s, body) = call(
        &state,
        session(
            post_json(&approve_path(1), &json!({"auto_apply": true})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    let (s, body) = call(
        &state,
        session(post_json(&approve_path(1), &json!({})), &rt, &rc),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // The next proposal is version 2; only the owner approves it with the mark.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!("{}/{rule}/versions", rules_path(&seed)),
                &json!({
                    "predicate": {"narration": {"op": "contains", "value": "fee"}},
                    "action": {"kind": "exclude", "reason": "non_business"},
                }),
            ),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["rule"]["version"], 2);
    let (s, body) = call(
        &state,
        session(
            post_json(&approve_path(2), &json!({"auto_apply": true})),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // The listing shows the rule's history: v1 approved, v2 active and marked.
    let (s, body) = call(&state, session(get(&rules_path(&seed)), &rt, &rc)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let listed = &body["items"][0];
    assert_eq!(listed["active_version"], 2);
    assert_eq!(listed["versions"][1]["auto_apply"], true);

    // Retired takes no more versions.
    let (s, body) = call(
        &state,
        session(
            post_json(&format!("{}/{rule}/retire", rules_path(&seed)), &json!({})),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!("{}/{rule}/versions", rules_path(&seed)),
                &json!({
                    "predicate": {"narration": {"op": "contains", "value": "x"}},
                    "action": {"kind": "exclude", "reason": "non_business"},
                }),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
}

/// An owner-marked exclusion rule excludes matching lines inside the import's flip —
/// the line lands `excluded` with the rule's decision, the rest stay `unreviewed`.
#[tokio::test]
async fn an_auto_apply_exclusion_lands_at_import() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    let (ot, oc, _) = enrolled(&state, &db.pool, &seed, "owner").await;

    let (s, body) = call(
        &state,
        session(
            post_json(
                &rules_path(&seed),
                &exclude_rule("fees", "bank fee", "non_business"),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let rule = body["rule"]["id"].as_str().unwrap().to_owned();
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!("{}/{rule}/versions/1/approve", rules_path(&seed)),
                &json!({"auto_apply": true}),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let csv = "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
        2026-03-02,,-1.00,USD,BANK FEE 02,REF-1,Big Bank,\n\
        2026-03-03,,-4.20,USD,OFFICE SUPPLIES,REF-2,Staples,\n";
    let (s, body) = upload_statement(&state, &seed, csv).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    let got = lines(&db.pool, &seed).await;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].1, "excluded", "{got:?}");
    assert_eq!(got[1].1, "unreviewed", "{got:?}");

    // The exclusion's decision names the rule version as its actor.
    let actor: String = sqlx::query_scalar(
        "SELECT actor_ref FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = 'line' AND subject_id = $2
            AND to_state = 'excluded'",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&got[0].0).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(actor, format!("rule:{rule}@v1"));
}

/// Close refuses while lines are undecided, then locks the month: review mutations,
/// imports and re-evaluation all answer `period_closed`, until an owner reopens it
/// with a reason.
#[tokio::test]
async fn a_period_locks_until_an_owner_reopens_it() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    let (rt, rc, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;
    let (ot, oc, _) = enrolled(&state, &db.pool, &seed, "owner").await;

    let (s, body) = upload_statement(&state, &seed, &statement(&[2, 3], "BANK FEE")).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // Undecided lines keep the month open — the refusal names its count.
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "close"), &json!({})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "period_has_undecided_lines");
    assert_eq!(body["error"]["count"], 2);

    // Excluding each line (a reviewer's act) decides it.
    for (line, _, version) in lines(&db.pool, &seed).await {
        let (s, body) = call(
            &state,
            session(
                post_json(
                    &format!(
                        "/v1/firms/{}/clients/{}/lines/{line}/exclude",
                        seed.firm, seed.client
                    ),
                    &json!({"expected_version": version, "reason": "non_business"}),
                ),
                &rt,
                &rc,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }

    // Now the close lands: closed, with the month's digest and chain head recorded.
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "close"), &json!({})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["period"]["status"], "closed");
    assert!(body["period"]["digest"].is_string(), "{body}");
    assert!(body["period"]["chain_seq"].as_i64().unwrap() >= 1, "{body}");

    // Review mutations on the month's lines answer the named refusal.
    let (line, _, version) = lines(&db.pool, &seed).await.pop().unwrap();
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/lines/{line}/exclude",
                    seed.firm, seed.client
                ),
                &json!({"expected_version": version + 1, "reason": "out_of_scope"}),
            ),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "period_closed");

    // So does an import whose rows land in the closed month.
    let (s, body) = upload_statement(&state, &seed, &statement(&[20, 21], "LATE FEE")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "period_closed");

    // And re-evaluation.
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "re-evaluate"), &json!({})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "period_closed");

    // Reopening is the owner's act, with a non-empty reason.
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "reopen"), &json!({"reason": "fix it"})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "reopen"), &json!({"reason": "  "})),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "reason_required");
    let (s, body) = call(
        &state,
        session(
            post_json(
                &period_path(&seed, "reopen"),
                &json!({"reason": "correction"}),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["period"]["status"], "reopened");

    // The reopened month takes mutations again.
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "re-evaluate"), &json!({})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

/// Failure case 5, end to end: a rule change never rewrites a past decision — the
/// closed month's recorded proof (digest, chain head, chain seq) and every earlier
/// record's hash survive a new rule version byte-for-byte, and the new version only
/// reaches work that lands after it.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the end-to-end digest check walks import -> close -> rule change -> import"
)]
async fn a_rule_change_never_rewrites_a_closed_months_proof() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    let (rt, rc, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;
    let (ot, oc, _) = enrolled(&state, &db.pool, &seed, "owner").await;

    // v1: narrations containing "bank fee" exclude themselves at import.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &rules_path(&seed),
                &exclude_rule("fees", "bank fee", "non_business"),
            ),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let rule = body["rule"]["id"].as_str().unwrap().to_owned();
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!("{}/{rule}/versions/1/approve", rules_path(&seed)),
                &json!({"auto_apply": true}),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // March imports under v1: both lines carry the tag and land excluded.
    let (s, body) = upload_statement(&state, &seed, &statement(&[2, 3], "BANK FEE")).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    for (_, status, _) in lines(&db.pool, &seed).await {
        assert_eq!(status, "excluded");
    }

    // The month closes; its proof is the recorded digest, chain seq and chain head.
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "close"), &json!({})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let firm = Uuid::parse_str(&seed.firm).unwrap();
    let client = Uuid::parse_str(&seed.client).unwrap();
    let account = Uuid::parse_str(&seed.account).unwrap();
    let before: (Vec<u8>, i64, Vec<u8>, i64) = sqlx::query_as(
        "SELECT digest, chain_seq, chain_head, row_version FROM recon.periods
          WHERE firm_id = $1 AND account_id = $2 AND month = '2026-03-01'",
    )
    .bind(firm)
    .bind(account)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    // And the whole chain so far: every record hash, in order.
    let hashes_before: Vec<String> = sqlx::query_scalar(
        "SELECT encode(record_hash, 'hex') FROM recon.decision_log
          WHERE firm_id = $1 AND client_id = $2 ORDER BY seq",
    )
    .bind(firm)
    .bind(client)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert!(hashes_before.len() >= 4);

    // The rule changes: v2 now excludes "office supplies" instead.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!("{}/{rule}/versions", rules_path(&seed)),
                &json!({
                    "predicate": {"narration": {"op": "contains", "value": "office"}},
                    "action": {"kind": "exclude", "reason": "non_business"},
                }),
            ),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!("{}/{rule}/versions/2/approve", rules_path(&seed)),
                &json!({"auto_apply": true}),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // The closed month's proof is byte-for-byte what was recorded at close…
    let after: (Vec<u8>, i64, Vec<u8>, i64) = sqlx::query_as(
        "SELECT digest, chain_seq, chain_head, row_version FROM recon.periods
          WHERE firm_id = $1 AND account_id = $2 AND month = '2026-03-01'",
    )
    .bind(firm)
    .bind(account)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        before, after,
        "a rule change rewrote the closed month's proof"
    );

    // …and every earlier record's hash is untouched; the log only grew.
    let hashes_after: Vec<String> = sqlx::query_scalar(
        "SELECT encode(record_hash, 'hex') FROM recon.decision_log
          WHERE firm_id = $1 AND client_id = $2 ORDER BY seq",
    )
    .bind(firm)
    .bind(client)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert!(hashes_after.len() > hashes_before.len());
    assert_eq!(&hashes_after[..hashes_before.len()], &hashes_before[..]);

    // April imports under v2: "office" excludes, the old predicate no longer applies
    // — the change reaches only what lands after it.
    let csv = "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
        2026-04-02,,-1.00,USD,BANK FEE 02,REF-1,Big Bank,\n\
        2026-04-03,,-4.20,USD,OFFICE SUPPLIES,REF-2,Staples,\n";
    let (s, body) = upload_statement(&state, &seed, csv).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    let got = lines(&db.pool, &seed).await;
    assert_eq!(got.len(), 4);
    assert_eq!(got[2].1, "unreviewed", "{got:?}");
    assert_eq!(got[3].1, "excluded", "{got:?}");
    let actor: String = sqlx::query_scalar(
        "SELECT actor_ref FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = 'line' AND subject_id = $2
            AND to_state = 'excluded'",
    )
    .bind(firm)
    .bind(Uuid::parse_str(&got[3].0).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(actor, format!("rule:{rule}@v2"));

    // And March's rule decision still names v1 — the past keeps its actor.
    let actor: String = sqlx::query_scalar(
        "SELECT actor_ref FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = 'line' AND subject_id = $2
            AND to_state = 'excluded'",
    )
    .bind(firm)
    .bind(Uuid::parse_str(&got[0].0).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(actor, format!("rule:{rule}@v1"));
}

/// A month holding an unacknowledged exception does not close; a reviewer's
/// acknowledgement in place (the line stays an exception) is what it takes (D-006).
#[tokio::test]
async fn exceptions_need_an_acknowledgement() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    let (rt, rc, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;

    let (s, body) = upload_statement(&state, &seed, &statement(&[2, 3], "BANK FEE")).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let mut rows = lines(&db.pool, &seed).await;

    // One line becomes an exception through the engine's own path (no candidate).
    let (exception, _, _) = rows.pop().unwrap();
    let job = match matching::start(
        &db.pool,
        recon_domain::FirmId::from_uuid(Uuid::parse_str(&seed.firm).unwrap()),
        recon_domain::ClientId::from_uuid(Uuid::parse_str(&seed.client).unwrap()),
        LEASE,
        ENGINE,
    )
    .await
    .unwrap()
    {
        Start::Created(l) | Start::Resumed(l) => l,
        Start::Busy { job } => panic!("the job {job} is busy"),
    };
    matching::snapshot(&db.pool, &job).await.unwrap();
    matching::commit_chunk(
        &db.pool,
        &job,
        &[DecidedLine {
            seq: 1,
            line: recon_domain::LineId::from_uuid(Uuid::parse_str(&exception).unwrap()),
            outcome: Decided::Exception {
                kind: ExceptionKind::NoCandidate,
                candidates: vec![],
            },
        }],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();

    // The other line is excluded; the exception keeps the month from closing.
    let (other, _, version) = rows.pop().unwrap();
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/lines/{other}/exclude",
                    seed.firm, seed.client
                ),
                &json!({"expected_version": version, "reason": "non_business"}),
            ),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "close"), &json!({})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"]["code"],
        "period_has_unacknowledged_exceptions"
    );
    assert_eq!(body["error"]["count"], 1);

    // The reviewer acknowledges it in place; the close then lands.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/lines/{exception}/acknowledge",
                    seed.firm, seed.client
                ),
                &json!({}),
            ),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["line"]["status"], "exception");
    assert_eq!(body["line"]["acknowledged"], true);
    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "close"), &json!({})),
            &rt,
            &rc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["period"]["status"], "closed");
}

/// Re-evaluation yields proposals — the rules in force against the month's undecided
/// lines — and commits nothing.
#[tokio::test]
async fn re_evaluation_proposes_and_never_commits() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    let (ot, oc, _) = enrolled(&state, &db.pool, &seed, "owner").await;

    let (s, body) = upload_statement(&state, &seed, &statement(&[2, 3], "BANK FEE")).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // An approved (not auto-applied) rule that names a counterparty.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &rules_path(&seed),
                &json!({
                    "name": "name the bank",
                    "predicate": {"narration": {"op": "contains", "value": "bank fee"}},
                    "action": {"kind": "assign_counterparty", "counterparty": "big bank"},
                }),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let rule = body["rule"]["id"].as_str().unwrap().to_owned();
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!("{}/{rule}/versions/1/approve", rules_path(&seed)),
                &json!({}),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let (s, body) = call(
        &state,
        session(
            post_json(&period_path(&seed, "re-evaluate"), &json!({})),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["evaluated"], 2);
    assert_eq!(body["proposals"].as_array().unwrap().len(), 2, "{body}");
    assert_eq!(body["proposals"][0]["rule"].as_str().unwrap(), rule);
    assert_eq!(body["truncated"], false);

    // Nothing committed: both lines are still unreviewed.
    for (_, status, _) in lines(&db.pool, &seed).await {
        assert_eq!(status, "unreviewed");
    }
}

/// Paths naming another firm hide everything (404), and a person's client scope
/// denies the clients they were not assigned (403) — rules and periods included.
#[tokio::test]
async fn rules_and_periods_keep_their_tenant() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = self::seed(&state).await;
    let other = self::seed(&state).await;
    let (ot, oc, _) = enrolled(&state, &db.pool, &seed, "owner").await;

    // A rule in the first firm, fetched under the second's paths: nothing found.
    let (s, body) = call(
        &state,
        session(
            post_json(
                &rules_path(&seed),
                &exclude_rule("fees", "bank fee", "non_business"),
            ),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let rule = body["rule"]["id"].as_str().unwrap().to_owned();
    let (s, body) = call(&state, session(get(&rules_path(&other)), &ot, &oc)).await;
    // The session's firm is the first seed's: the second firm's paths are not its
    // world at all (404, D-020).
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");

    // Same firm, a client the user was never assigned: 403.
    let (s, second) = call(
        &state,
        operator(post_json(
            &format!("/v1/firms/{}/clients", seed.firm),
            &json!({"name": "Other Client"}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{second}");
    let other_client = second["client"]["id"].as_str().unwrap();
    let (s, body) = call(
        &state,
        session(
            get(&format!(
                "/v1/firms/{}/clients/{other_client}/rules",
                seed.firm
            )),
            &ot,
            &oc,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");

    // The rule itself is listed under its own client only.
    let (s, body) = call(&state, session(get(&rules_path(&seed)), &ot, &oc)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [rule.as_str()]);
}
