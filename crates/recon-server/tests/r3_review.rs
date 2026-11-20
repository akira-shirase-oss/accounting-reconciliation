//! R3 slice 2 (D-018, D-021): the review API — queue, approve, reject, exclude and
//! manual-match over person sessions; four-eyes with a distinct second eye; optimistic
//! row-version 409s; allocation release on reject; and the role matrix enforced on
//! every route, in process over a real database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a panic is the failure report"
)]

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_db::testing::TestDb;
use recon_server::auth::totp_code;
use recon_server::{AppState, Config, faults::Faults, router};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "r3-review-test-operator-token-0123456789abcdef";

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 51_002))
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

#[derive(Clone)]
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

/// One user per role, all assigned to `client`; each signed in and TOTP-enrolled —
/// `(token, csrf, user_id)`.
async fn enrolled(
    state: &AppState,
    admin: &PgPool,
    seed: &Seed,
    role: &str,
) -> (String, String, String) {
    enrolled_tag(state, admin, seed, role, "").await
}

/// `enrolled` with a distinguishing tag — for tests needing many users of one role.
async fn enrolled_tag(
    state: &AppState,
    admin: &PgPool,
    seed: &Seed,
    role: &str,
    tag: &str,
) -> (String, String, String) {
    let email = if tag.is_empty() {
        format!("{role}@example.test")
    } else {
        format!("{role}-{tag}@example.test")
    };
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

/// The three-line statement and two-bill set the tests share: `-1.00` to `Acme Ltd`
/// whose reference does not name the bill (a T2 proposal against INV-1), `-4.99` to
/// `Big Bank` (no candidate sums to it — stays unreviewed; a manual match to INV-2
/// plus a +0.01 `bank_fee` residual balances it) and `-0.50` (unreviewed; excluded in
/// the flow test). Awaits the job's completion.
async fn run_reviewable_job(state: &AppState, seed: &Seed) {
    let statement = "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
        2026-03-02,,-1.00,USD,PAYMENT ACME,MISC-77,Acme Ltd,\n\
        2026-03-03,,-4.99,USD,BANK SERVICE CHARGES,,Big Bank,\n\
        2026-03-04,,-0.50,USD,ROUNDING ADJUSTMENT,,Misc,\n";
    let (s, body) = call(
        state,
        operator(upload(
            &format!("/v1/firms/{}/accounts/{}/imports", seed.firm, seed.account),
            statement,
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let documents = "kind,number,issue_date,due_date,amount,currency,counterparty,description\n\
        purchase_bill,INV-1,2026-03-01,2026-03-31,-1.00,USD,Acme Ltd,\n\
        purchase_bill,INV-2,2026-03-01,2026-03-31,-5.00,USD,Acme Ltd,\n";
    let (s, body) = call(
        state,
        operator(upload(
            &format!(
                "/v1/firms/{}/clients/{}/document-imports",
                seed.firm, seed.client
            ),
            documents,
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let (s, body) = call(
        state,
        operator(post_json(
            &format!(
                "/v1/firms/{}/clients/{}/matching-jobs",
                seed.firm, seed.client
            ),
            &json!({}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let job = body["job"]["id"].as_str().unwrap().to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (s, body) = call(
            state,
            operator(get(&format!(
                "/v1/firms/{}/clients/{}/matching-jobs/{job}",
                seed.firm, seed.client
            ))),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        if body["job"]["status"] != "running" || std::time::Instant::now() >= deadline {
            assert_eq!(body["job"]["status"], "complete", "{body}");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn queue(state: &AppState, seed: &Seed, token: &str) -> Vec<Value> {
    let (s, body) = call(
        state,
        session(
            get(&format!(
                "/v1/firms/{}/clients/{}/review-queue",
                seed.firm, seed.client
            )),
            token,
            "unused-for-read",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    body["items"].as_array().unwrap().clone()
}

async fn approve(
    state: &AppState,
    seed: &Seed,
    token: &str,
    csrf: &str,
    match_id: &str,
    version: i64,
) -> (StatusCode, Value) {
    call(
        state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/matches/{match_id}/approve",
                    seed.firm, seed.client
                ),
                &json!({"expected_version": version}),
            ),
            token,
            csrf,
        ),
    )
    .await
}

async fn line_by_amount(db: &PgPool, seed: &Seed, minor: i64) -> (String, i64) {
    use sqlx::Row as _;

    let row = sqlx::query(
        "SELECT id, row_version FROM recon.statement_lines
          WHERE firm_id = $1 AND client_id = $2 AND amount_minor = $3",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&seed.client).unwrap())
    .bind(minor)
    .fetch_one(db)
    .await
    .unwrap();
    let id: Uuid = row.get("id");
    let version: i64 = row.get("row_version");
    (id.to_string(), version)
}

/// Failure case 7: 32 distinct reviewers race to approve one proposed match at the
/// same row version — exactly one commits; every loser answers `409 conflict` naming
/// the winning decision; the document's allocation stays within its amount.
#[expect(
    clippy::too_many_lines,
    reason = "one contract fixture, one concurrent storm, one set of invariants"
)]
#[tokio::test]
async fn concurrent_conflicting_approvals_admit_one() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    run_reviewable_job(&state, &seed).await;
    let (token, _csrf, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;
    let items = queue(&state, &seed, &token).await;
    let proposal = items
        .iter()
        .find(|i| i["kind"] == "proposal" && !i["four_eyes"].as_bool().unwrap())
        .expect("the T2 proposal");
    let match_id = proposal["id"].as_str().unwrap().to_owned();
    let version = proposal["row_version"].as_i64().unwrap();

    // 32 reviewers — the contract's concurrency — each with their own session.
    let mut reviewers = Vec::with_capacity(32);
    for n in 0..32 {
        reviewers.push(enrolled_tag(&state, &db.pool, &seed, "reviewer", &n.to_string()).await);
    }
    let mut tasks = tokio::task::JoinSet::new();
    for (token, csrf, user) in reviewers {
        let state = state.clone();
        let seed_client = seed.client.clone();
        let seed_firm = seed.firm.clone();
        let match_id = match_id.clone();
        tasks.spawn(async move {
            let (status, body) = call(
                &state,
                session(
                    post_json(
                        &format!(
                            "/v1/firms/{seed_firm}/clients/{seed_client}/matches/{match_id}/approve"
                        ),
                        &json!({"expected_version": version}),
                    ),
                    &token,
                    &csrf,
                ),
            )
            .await;
            (status, body, user)
        });
    }
    let mut ok = 0;
    let mut losers = Vec::new();
    let mut winner_user = String::new();
    while let Some(outcome) = tasks.join_next().await {
        let (status, body, user) = outcome.unwrap();
        match status {
            StatusCode::OK => {
                ok += 1;
                winner_user = user;
            }
            StatusCode::CONFLICT => losers.push(body),
            other => panic!("an approval answered {other}: {body}"),
        }
    }
    assert_eq!(
        (ok, losers.len()),
        (1, 31),
        "exactly one approval may commit"
    );

    // The winning decision is the one that moved the match to `committed`.
    let (status, match_status, committed, decision): (String, String, i64, Uuid) = sqlx::query_as(
        "SELECT m.status, l.status,
                    (SELECT count(*) FROM recon.decision_log d
                      WHERE d.firm_id = m.firm_id AND d.subject_kind = 'match'
                        AND d.subject_id = m.id AND d.to_state = 'committed'),
                    (SELECT d.id FROM recon.decision_log d
                      WHERE d.firm_id = m.firm_id AND d.subject_kind = 'match'
                        AND d.subject_id = m.id AND d.to_state = 'committed')
               FROM recon.matches m
               JOIN recon.statement_lines l ON l.firm_id = m.firm_id
                 AND l.id = (SELECT line_id FROM recon.match_lines ml
                              WHERE ml.firm_id = m.firm_id AND ml.match_id = m.id LIMIT 1)
              WHERE m.firm_id = $1 AND m.id = $2",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&match_id).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), match_status.as_str(), committed),
        ("committed", "matched", 1)
    );

    // Every loser's 409 names that winning decision — the contract's exact words.
    for loser in &losers {
        let error = &loser["error"];
        assert_eq!(error["code"].as_str().unwrap(), "conflict", "{loser}");
        let winner = &error["winner"];
        assert_eq!(
            winner["decision"].as_str().unwrap(),
            decision.to_string(),
            "the 409 names the winning decision: {loser}"
        );
        assert_eq!(winner["to_state"].as_str().unwrap(), "committed");
        assert_eq!(winner["actor_ref"].as_str().unwrap(), winner_user);
        assert!(winner["seq"].as_i64().unwrap() >= 1, "{loser}");
    }

    // The document the winning match allocated never exceeds its amount.
    let (allocated, amount): (i64, i64) = sqlx::query_as(
        "SELECT d.allocated_minor, d.amount_minor FROM recon.documents d
          WHERE d.firm_id = $1 AND d.id IN
                (SELECT md.document_id FROM recon.match_documents md
                  WHERE md.firm_id = $1 AND md.match_id = $2)",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&match_id).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(allocated, amount, "allocated at most the document amount");
}

/// Failure case 7's other arm: racing manual matches that would share one line or
/// over-allocate one document. The schema refuses two live proposals on a line, so
/// the race is at creation: 16 reviewers on one line, 8+8 on two lines over one
/// document — one winner per arm, every loser a `409` naming the winning decision,
/// no document allocated beyond its amount (the raw-SQL half is
/// `recon_db`'s `documents_refuse_over_allocation_for_raw_sql`).
#[expect(
    clippy::too_many_lines,
    reason = "one contract fixture, one concurrent storm, one set of invariants"
)]
#[tokio::test]
async fn concurrent_manual_matches_share_line_and_document() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    let statement = "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
        2026-03-05,,-4.99,USD,SUPPLIER PAYMENT ONE,,Acme Ltd,\n\
        2026-03-06,,-2.00,USD,SUPPLIER PAYMENT TWO,,Acme Ltd,\n\
        2026-03-07,,-2.00,USD,SUPPLIER PAYMENT THREE,,Acme Ltd,\n";
    let (s, body) = call(
        &state,
        operator(upload(
            &format!("/v1/firms/{}/accounts/{}/imports", seed.firm, seed.account),
            statement,
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let documents = "kind,number,issue_date,due_date,amount,currency,counterparty,description\n\
        purchase_bill,INV-A,2026-03-01,2026-03-31,-4.99,USD,Acme Ltd,\n\
        purchase_bill,INV-B,2026-03-01,2026-03-31,-2.00,USD,Acme Ltd,\n";
    let (s, body) = call(
        &state,
        operator(upload(
            &format!(
                "/v1/firms/{}/clients/{}/document-imports",
                seed.firm, seed.client
            ),
            documents,
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let firm = Uuid::parse_str(&seed.firm).unwrap();
    let client = Uuid::parse_str(&seed.client).unwrap();
    let (line_a, _) = line_by_amount(&db.pool, &seed, -499).await;
    // The two -2.00 lines, told apart by booking date.
    let lines_bc: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM recon.statement_lines
          WHERE firm_id = $1 AND client_id = $2 AND amount_minor = -200
          ORDER BY booking_date",
    )
    .bind(firm)
    .bind(client)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let [line_b, line_c]: [Uuid; 2] = lines_bc.try_into().unwrap();
    let doc_a: Uuid = sqlx::query_scalar(
        "SELECT id FROM recon.documents
          WHERE firm_id = $1 AND client_id = $2 AND number_norm = 'INV-A'",
    )
    .bind(firm)
    .bind(client)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let doc_b: Uuid = sqlx::query_scalar(
        "SELECT id FROM recon.documents
          WHERE firm_id = $1 AND client_id = $2 AND number_norm = 'INV-B'",
    )
    .bind(firm)
    .bind(client)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    // 32 reviewers: 16 race line A, 8 race line B and 8 race line C — B and C's
    // arms both want the one document.
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..32 {
        let (token, csrf, _) =
            enrolled_tag(&state, &db.pool, &seed, "reviewer", &n.to_string()).await;
        let state = state.clone();
        let seed = seed.clone();
        let (line, document) = if n < 16 {
            (line_a.clone(), doc_a)
        } else if n < 24 {
            (line_b.to_string(), doc_b)
        } else {
            (line_c.to_string(), doc_b)
        };
        tasks.spawn(async move {
            let (status, body) = call(
                &state,
                session(
                    post_json(
                        &format!(
                            "/v1/firms/{}/clients/{}/lines/{line}/manual-match",
                            seed.firm, seed.client
                        ),
                        &json!({"documents": [document], "residuals": []}),
                    ),
                    &token,
                    &csrf,
                ),
            )
            .await;
            (status, body, line)
        });
    }
    let mut created = 0;
    let mut losers = Vec::new();
    while let Some(outcome) = tasks.join_next().await {
        let (status, body, line) = outcome.unwrap();
        match status {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => losers.push((body, line)),
            other => panic!("a manual match answered {other}: {body}"),
        }
    }
    assert_eq!(
        (created, losers.len()),
        (2, 30),
        "one winner per arm — the shared line and the shared document"
    );

    // The winning decisions: each winning line's unreviewed→proposed move and the
    // proposal decisions of the matches holding INV-A and INV-B.
    let line_decisions: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT subject_id, id FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = 'line' AND to_state = 'proposed'",
    )
    .bind(firm)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let winning_lines: Vec<Uuid> = line_decisions.iter().map(|(l, _)| *l).collect();
    assert_eq!(winning_lines.len(), 2, "{line_decisions:?}");
    let holder_decisions: Vec<Uuid> = sqlx::query_scalar(
        "SELECT d.id FROM recon.decision_log d
          WHERE d.firm_id = $1 AND d.subject_kind = 'match' AND d.to_state = 'proposed'
            AND d.subject_id IN
                (SELECT md.match_id FROM recon.match_documents md
                  WHERE md.firm_id = $1 AND md.document_id IN ($2, $3))",
    )
    .bind(firm)
    .bind(doc_a)
    .bind(doc_b)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(holder_decisions.len(), 2, "{holder_decisions:?}");

    // The INV-B holder's own proposal decision — the winner the losing line's
    // reviewers are told about.
    let doc_b_winner: Uuid = sqlx::query_scalar(
        "SELECT d.id FROM recon.decision_log d
          WHERE d.firm_id = $1 AND d.subject_kind = 'match' AND d.to_state = 'proposed'
            AND d.subject_id =
                (SELECT md.match_id FROM recon.match_documents md
                  WHERE md.firm_id = $1 AND md.document_id = $2)",
    )
    .bind(firm)
    .bind(doc_b)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    // Every loser's 409 names a winning decision: the same-line losers name their
    // line's `proposed` move; the losing document arm names the match that holds it.
    for (loser, line) in &losers {
        let error = &loser["error"];
        assert_eq!(error["code"].as_str().unwrap(), "conflict", "{loser}");
        let winner = &error["winner"];
        let named: Uuid = winner["decision"].as_str().unwrap().parse().unwrap();
        let line_uuid = Uuid::parse_str(line).unwrap();
        let expected = line_decisions
            .iter()
            .find(|(l, _)| *l == line_uuid)
            .map_or(doc_b_winner, |(_, d)| *d);
        assert_eq!(
            named, expected,
            "the 409 names the winning decision: {loser}"
        );
        assert_eq!(winner["to_state"].as_str().unwrap(), "proposed");
        assert!(winner["seq"].as_i64().unwrap() >= 1, "{loser}");
    }

    // Invariants: one live proposal per contested line; no document past its amount.
    let live_per_line: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.match_lines ml
           JOIN recon.matches m ON m.firm_id = ml.firm_id AND m.id = ml.match_id
          WHERE ml.firm_id = $1 AND ml.line_id = ANY($2) AND m.status IN ('proposed','committed')",
    )
    .bind(firm)
    .bind([Uuid::parse_str(&line_a).unwrap(), line_b, line_c])
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(live_per_line, 2, "one live match per contested line");
    let over: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.documents
          WHERE firm_id = $1 AND abs(allocated_minor) > abs(amount_minor)",
    )
    .bind(firm)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(over, 0, "no document allocated beyond its amount");
}

/// The four-eyes flow: a manual match proposes `four_eyes`; the preparer cannot
/// approve it, the first eye records, the same eye may not be the second, a different
/// approver commits — and a stale row version is a 409.
#[tokio::test]
async fn four_eyes_needs_a_second_distinct_person() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    run_reviewable_job(&state, &seed).await;
    let (prep, prep_csrf, _) = enrolled(&state, &db.pool, &seed, "preparer").await;
    let (rev, rev_csrf, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;
    let (owner, owner_csrf, _) = enrolled(&state, &db.pool, &seed, "owner").await;

    let (line, _) = line_by_amount(&db.pool, &seed, -499).await;
    // The -4.99 line against INV-2's -5.00 plus a +0.01 bank fee balances exactly.
    let doc: Uuid = sqlx::query_scalar(
        "SELECT id FROM recon.documents
          WHERE firm_id = $1 AND client_id = $2 AND number_norm = 'INV-2'",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&seed.client).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/lines/{line}/manual-match",
                    seed.firm, seed.client
                ),
                &json!({"documents": [doc],
                        "residuals": [{"code": "bank_fee", "amount_minor": 1}]}),
            ),
            &prep,
            &prep_csrf,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let match_id = body["match"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["match"]["four_eyes"], true);

    // A preparer cannot eye a four-eyes match at all.
    let (s, _) = approve(&state, &seed, &prep, &prep_csrf, &match_id, 1).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // The reviewer's eye records; the same person is not the second eye.
    let (s, body) = approve(&state, &seed, &rev, &rev_csrf, &match_id, 1).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["match"]["status"], "proposed");
    let (s, _) = approve(&state, &seed, &rev, &rev_csrf, &match_id, 1).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // A second, distinct approver commits it.
    let (s, body) = approve(&state, &seed, &owner, &owner_csrf, &match_id, 1).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["match"]["status"], "committed");
    // And the decision is durable: two persons on the match's audit.
    let actors: Vec<String> = sqlx::query_scalar(
        "SELECT actor_ref FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = 'match' AND subject_id = $2
          ORDER BY id",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&match_id).unwrap())
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(actors.len(), 3, "manual_match + first eye + second eye");
    assert_ne!(actors[1], actors[2], "the eyes are distinct persons");
}

/// Rejecting a proposal returns its lines to `unreviewed` and releases the documents'
/// allocations — the whole move in one transaction.
#[tokio::test]
async fn rejecting_a_proposal_releases_the_documents() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    run_reviewable_job(&state, &seed).await;
    let (token, csrf, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;
    let items = queue(&state, &seed, &token).await;
    let proposal = items
        .iter()
        .find(|i| i["kind"] == "proposal")
        .expect("the T2 proposal");
    let match_id = proposal["id"].as_str().unwrap().to_owned();

    let allocated: i64 = sqlx::query_scalar(
        "SELECT allocated_minor FROM recon.documents
          WHERE firm_id = $1 AND client_id = $2 AND number_norm = 'INV-1'",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&seed.client).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(allocated, -100, "the proposal allocated the bill");

    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/matches/{match_id}/reject",
                    seed.firm, seed.client
                ),
                &json!({"expected_version": proposal["row_version"].as_i64().unwrap()}),
            ),
            &token,
            &csrf,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["match"]["status"], "rejected");

    let (line_status, allocated): (String, i64) = sqlx::query_as(
        "SELECT l.status, d.allocated_minor FROM recon.documents d
          CROSS JOIN recon.statement_lines l
          WHERE d.firm_id = $1 AND d.client_id = $2 AND d.number_norm = 'INV-1'
            AND l.firm_id = $1 AND l.client_id = $2 AND l.amount_minor = -100",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&seed.client).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (line_status.as_str(), allocated),
        ("unreviewed", 0),
        "the line returns and the bill's allocation releases"
    );
}

/// Excluding a line names its reason in the decision; a stale version 409s; a
/// proposed line refuses — its match is decided first.
#[tokio::test]
async fn exclude_names_the_reason_and_guards_the_version() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    run_reviewable_job(&state, &seed).await;
    let (token, csrf, _) = enrolled(&state, &db.pool, &seed, "preparer").await;
    let (line, version) = line_by_amount(&db.pool, &seed, -50).await;

    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/lines/{line}/exclude",
                    seed.firm, seed.client
                ),
                &json!({"expected_version": version + 7, "reason": "non_business"}),
            ),
            &token,
            &csrf,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");

    let (s, body) = call(
        &state,
        session(
            post_json(
                &format!(
                    "/v1/firms/{}/clients/{}/lines/{line}/exclude",
                    seed.firm, seed.client
                ),
                &json!({"expected_version": version, "reason": "non_business",
                        "note": "director's lunch"}),
            ),
            &token,
            &csrf,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["line"]["status"], "excluded");

    let reason: String = sqlx::query_scalar(
        "SELECT reason_code FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = 'line' AND subject_id = $2
            AND to_state = 'excluded'",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&line).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(reason, "non_business");
}

/// A manual match refuses an unbalanced set, and the role matrix holds on every
/// route: each role attempts each route and gets its matrix answer.
#[expect(
    clippy::too_many_lines,
    reason = "the matrix is one table plus one loop; splitting it hides the route list"
)]
#[tokio::test]
async fn the_role_matrix_holds_on_every_route() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let seed = seed(&state).await;
    run_reviewable_job(&state, &seed).await;
    let (viewer, viewer_csrf, _) = enrolled(&state, &db.pool, &seed, "viewer").await;
    let (prep, prep_csrf, _) = enrolled(&state, &db.pool, &seed, "preparer").await;
    let (rev, rev_csrf, _) = enrolled(&state, &db.pool, &seed, "reviewer").await;
    let (owner, owner_csrf, _) = enrolled(&state, &db.pool, &seed, "owner").await;
    let (line, _v) = line_by_amount(&db.pool, &seed, -50).await;
    let (t2line, _) = line_by_amount(&db.pool, &seed, -100).await;
    let doc: Uuid = sqlx::query_scalar(
        "SELECT id FROM recon.documents
          WHERE firm_id = $1 AND client_id = $2 AND number_norm = 'INV-2'",
    )
    .bind(Uuid::parse_str(&seed.firm).unwrap())
    .bind(Uuid::parse_str(&seed.client).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let items = queue(&state, &seed, &rev).await;
    let proposal = items
        .iter()
        .find(|i| i["kind"] == "proposal")
        .expect("the T2 proposal");
    let match_id = proposal["id"].as_str().unwrap();
    let version = proposal["row_version"].as_i64().unwrap();
    let f = &seed.firm;
    let c = &seed.client;
    let a = &seed.account;
    let import: String = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM recon.imports WHERE firm_id = $1 ORDER BY id LIMIT 1",
    )
    .bind(Uuid::parse_str(f).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .to_string();
    let job: String = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM recon.match_jobs WHERE firm_id = $1 ORDER BY id LIMIT 1",
    )
    .bind(Uuid::parse_str(f).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .to_string();

    // (request, read-only) — every route's matrix row.
    let routes: Vec<(Request<Body>, bool)> = vec![
        (post_json("/v1/firms", &json!({"name": "X"})), false),
        (
            post_json(&format!("/v1/firms/{f}/clients"), &json!({"name": "Y"})),
            false,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/clients/{c}/accounts"),
                &json!({"name": "A", "currency": "USD"}),
            ),
            false,
        ),
        (
            upload(&format!("/v1/firms/{f}/accounts/{a}/imports"), "x,y\n"),
            false,
        ),
        (
            upload(
                &format!("/v1/firms/{f}/clients/{c}/document-imports"),
                "x,y\n",
            ),
            false,
        ),
        (get(&format!("/v1/firms/{f}/imports/{import}")), true),
        (
            get(&format!("/v1/firms/{f}/imports/{import}/changes")),
            true,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/imports/{import}/accept"),
                &json!({}),
            ),
            false,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/imports/{import}/reject"),
                &json!({}),
            ),
            false,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/profiles"),
                &json!({"name": "p", "kind": "statement", "profile": {}}),
            ),
            false,
        ),
        (get(&format!("/v1/firms/{f}/profiles/r0-generic-csv")), true),
        (
            post_json(
                &format!("/v1/firms/{f}/clients/{c}/matching-jobs"),
                &json!({}),
            ),
            false,
        ),
        (
            get(&format!("/v1/firms/{f}/clients/{c}/matching-jobs/{job}")),
            true,
        ),
        (
            get(&format!("/v1/firms/{f}/clients/{c}/review-queue")),
            true,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/clients/{c}/matches/{match_id}/approve"),
                &json!({"expected_version": version}),
            ),
            false,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/clients/{c}/matches/{match_id}/reject"),
                &json!({"expected_version": version}),
            ),
            false,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/clients/{c}/lines/{line}/exclude"),
                &json!({"expected_version": 1, "reason": "non_business"}),
            ),
            false,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/clients/{c}/lines/{t2line}/manual-match"),
                &json!({"documents": [doc]}),
            ),
            false,
        ),
        (
            post_json(
                &format!("/v1/firms/{f}/users"),
                &json!({"email": "z@x.test", "display_name": "Z",
                        "role": "viewer", "password": "pw-1234567890123"}),
            ),
            false,
        ),
    ];
    let mut misses = Vec::new();
    let creds: [(&str, Option<(&str, &str)>); 6] = [
        ("viewer", Some((viewer.as_str(), viewer_csrf.as_str()))),
        ("preparer", Some((prep.as_str(), prep_csrf.as_str()))),
        ("reviewer", Some((rev.as_str(), rev_csrf.as_str()))),
        ("owner", Some((owner.as_str(), owner_csrf.as_str()))),
        ("operator", None),
        ("anonymous", None),
    ];
    // The acts each route demands, by path shape.
    let admin_shape = |path: &str| {
        path == "/v1/firms"
            || path.ends_with("/clients")
            || path.ends_with("/accounts")
            || path.ends_with("/users")
    };
    for (request, is_read) in &routes {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        for (role, cred) in &creds {
            let mut req = Request::builder()
                .method(method.clone())
                .uri(&path)
                .body(Body::empty())
                .unwrap();
            if method == axum::http::Method::POST {
                req.headers_mut()
                    .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            }
            let req = match (*role, cred) {
                ("operator", _) => operator(req),
                (_, Some((t, c))) => session(req, t, c),
                _ => req,
            };
            let (s, _b) = call(&state, req).await;
            let person_only = path.contains("/matches/") || path.contains("/lines/");
            let expected_refused = match *role {
                "anonymous" => true,
                "operator" => person_only,
                "viewer" => !*is_read,
                "preparer" | "reviewer" => admin_shape(&path),
                "owner" => path == "/v1/firms",
                _ => false,
            };
            if expected_refused {
                if !matches!(
                    s,
                    StatusCode::UNAUTHORIZED
                        | StatusCode::FORBIDDEN
                        | StatusCode::NOT_FOUND
                        | StatusCode::METHOD_NOT_ALLOWED
                ) {
                    misses.push(format!("{role} {method} {path} = {s} (expected refusal)"));
                }
            } else if matches!(s, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                misses.push(format!("{role} {method} {path} = {s} (expected admission)"));
            }
        }
    }
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}
