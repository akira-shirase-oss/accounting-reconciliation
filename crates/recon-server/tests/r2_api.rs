//! R2's matching routes in process (the router over a real test database): a job's start,
//! its status endpoint, the one-running-job-per-client conflict, the operator guard, and
//! the sweeper's resume of a job whose worker died.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_pass_by_value,
    reason = "integration tests: a panic is the failure report"
)]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_db::testing::TestDb;
use recon_domain::{ClientId, FirmId};
use recon_server::{AppState, Config, faults::Faults, router};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "r2-api-test-operator-token-0123456789abcd";

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

async fn call(state: &AppState, request: Request<Body>) -> (StatusCode, Value) {
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn authed(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
}

fn post(path: &str) -> Request<Body> {
    authed(Request::post(path)).body(Body::empty()).unwrap()
}

fn post_json(path: &str, body: &Value) -> Request<Body> {
    authed(Request::post(path))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(path: &str) -> Request<Body> {
    authed(Request::get(path)).body(Body::empty()).unwrap()
}

fn upload(path: &str, content_type: &str, bytes: impl Into<Body>) -> Request<Body> {
    authed(Request::post(path))
        .header(header::CONTENT_TYPE, content_type)
        .body(bytes.into())
        .unwrap()
}

struct Tenant {
    firm: String,
    client: String,
    account: String,
}

async fn tenant(state: &AppState) -> Tenant {
    let (s, firm) = call(state, post_json("/v1/firms", &json!({"name": "R2 Firm"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{firm}");
    let firm = firm["firm"]["id"].as_str().unwrap().to_owned();
    let (s, client) = call(
        state,
        post_json(
            &format!("/v1/firms/{firm}/clients"),
            &json!({"name": "Client"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    let client = client["client"]["id"].as_str().unwrap().to_owned();
    let (s, account) = call(
        state,
        post_json(
            &format!("/v1/firms/{firm}/clients/{client}/accounts"),
            &json!({"name": "Current", "currency": "USD"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{account}");
    Tenant {
        firm,
        client,
        account: account["account"]["id"].as_str().unwrap().to_owned(),
    }
}

/// One -1.00 statement line and one -1.00 purchase bill it names by number: a T1 exact match.
async fn seed_pair(state: &AppState, t: &Tenant) {
    let statement = "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
        2026-03-02,,-1.00,USD,PAYMENT ACME INV-1,INV-1,Acme Ltd,\n";
    let (s, body) = call(
        state,
        upload(
            &format!("/v1/firms/{}/accounts/{}/imports", t.firm, t.account),
            "text/csv",
            statement,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let documents = "kind,number,issue_date,due_date,amount,currency,counterparty,description\n\
        purchase_bill,INV-1,2026-03-01,2026-03-31,-1.00,USD,Acme Ltd,\n";
    let (s, body) = call(
        state,
        upload(
            &format!("/v1/firms/{}/clients/{}/document-imports", t.firm, t.client),
            "text/csv",
            documents,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
}

fn jobs_path(t: &Tenant) -> String {
    format!("/v1/firms/{}/clients/{}/matching-jobs", t.firm, t.client)
}

/// Polls the job until it leaves `running` or the deadline passes; answers its last body.
async fn wait_job(state: &AppState, t: &Tenant, job: &str, within: Duration) -> Value {
    let deadline = std::time::Instant::now() + within;
    loop {
        let (s, body) = call(state, get(&format!("{}/{job}", jobs_path(t)))).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        if body["job"]["status"] != "running" || std::time::Instant::now() >= deadline {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_matching_job_runs_to_completion_and_reports_its_digest() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state).await;
    seed_pair(&state, &t).await;

    let (s, body) = call(&state, post(&jobs_path(&t))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let job = body["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["job"]["status"], "running");

    let body = wait_job(&state, &t, &job, Duration::from_secs(15)).await;
    assert_eq!(body["job"]["status"], "complete", "{body}");
    assert_eq!(body["job"]["matched"], 1, "the pair is a T1 exact match");
    assert_eq!(body["job"]["cursor"], body["job"]["line_count"]);
    assert_eq!(
        body["job"]["run_digest"].as_str().unwrap().len(),
        64,
        "a sha-256 hex digest"
    );
    // The line is matched, the document allocated — through the API's own row too.
    let status: String =
        sqlx::query_scalar("SELECT status FROM recon.statement_lines WHERE client_id = $1")
            .bind(Uuid::parse_str(&t.client).unwrap())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "matched");

    // A finished job frees the client: a second run over the (now empty) open set.
    let (s, body) = call(&state, post(&jobs_path(&t))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let second = body["job"]["id"].as_str().unwrap().to_owned();
    let body = wait_job(&state, &t, &second, Duration::from_secs(15)).await;
    assert_eq!(body["job"]["status"], "complete");
    assert_eq!(body["job"]["line_count"], 0);
    db.cleanup().await;
}

#[tokio::test]
async fn a_second_start_while_a_job_runs_is_a_named_conflict() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state).await;
    // Hold the client with a database-side claim (no worker): the API start must refuse.
    let held = recon_db::matching::start(
        &db.pool,
        FirmId::from_uuid(Uuid::parse_str(&t.firm).unwrap()),
        ClientId::from_uuid(Uuid::parse_str(&t.client).unwrap()),
        Duration::from_secs(60),
        "match-v1",
    )
    .await
    .unwrap();
    let held_id = match held {
        recon_db::matching::Start::Created(l) => l.id,
        other => panic!("expected the claim, got {other:?}"),
    };
    let (s, body) = call(&state, post(&jobs_path(&t))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "client_busy");
    assert_eq!(body["error"]["job"], held_id.to_string());
    db.cleanup().await;
}

#[tokio::test]
async fn matching_jobs_need_the_operator_and_the_clients_firm() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state).await;

    // No token: refused before anything else.
    let r = router(state.clone())
        .oneshot(Request::post(jobs_path(&t)).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // A client that is not this firm's: the composite key refuses it as 404.
    let (s, body) = call(
        &state,
        post(&format!(
            "/v1/firms/{}/clients/{}/matching-jobs",
            t.firm,
            Uuid::now_v7()
        )),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");

    // A job of a different client is not this client's resource.
    let (s, job_body) = call(&state, post(&jobs_path(&t))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{job_body}");
    let job = job_body["job"]["id"].as_str().unwrap();
    let other_client = Uuid::now_v7();
    let (s, body) = call(
        &state,
        get(&format!(
            "/v1/firms/{}/clients/{}/matching-jobs/{job}",
            t.firm, other_client
        )),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    db.cleanup().await;
}

#[tokio::test]
async fn a_job_past_the_worker_limit_queues_for_the_sweeper() {
    let db = TestDb::new().await;
    // One worker permit: the first job drives; the second must queue, never refuse.
    let url = db.app_url();
    let config = Config::from_source(|name| match name {
        "RECON_DATABASE_URL" => Some(url.clone()),
        "RECON_OPERATOR_TOKEN" => Some(TOKEN.to_owned()),
        "RECON_MATCHING_CONCURRENCY" => Some("1".to_owned()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(db.pool.clone(), config, Faults::default());
    state.mark_checked();
    let t = tenant(&state).await;
    seed_pair(&state, &t).await;
    // A second client in the same firm, seeded with the same -1.00 pair.
    let (s, client) = call(
        &state,
        post_json(
            &format!("/v1/firms/{}/clients", t.firm),
            &json!({"name": "Client Two"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    let other_client = client["client"]["id"].as_str().unwrap().to_owned();
    let (s, account) = call(
        &state,
        post_json(
            &format!("/v1/firms/{}/clients/{other_client}/accounts", t.firm),
            &json!({"name": "Current", "currency": "USD"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{account}");
    let other = Tenant {
        firm: t.firm.clone(),
        client: other_client,
        account: account["account"]["id"].as_str().unwrap().to_owned(),
    };
    seed_pair(&state, &other).await;

    // The first job takes the only worker permit and drives.
    let (s, body) = call(&state, post(&jobs_path(&t))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let first = body["job"]["id"].as_str().unwrap().to_owned();

    // The second is accepted too: the durable row is the queue, claimed as workers free.
    let (s, body) = call(&state, post(&jobs_path(&other))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "queued start refused: {body}");
    let second = body["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["job"]["status"], "running");

    // While the first still runs, the second sits unclaimed: no cursor, no snapshot.
    let body = wait_job(&state, &t, &first, Duration::from_secs(15)).await;
    assert_eq!(body["job"]["status"], "complete", "{body}");
    let (_, body) = call(&state, get(&format!("{}/{second}", jobs_path(&other)))).await;
    assert_eq!(body["job"]["status"], "running");
    assert_eq!(body["job"]["cursor"], 0, "a queued job has not started");

    // The sweeper's round claims the queued job and drives it to completion.
    recon_server::matching::resume_expired(&state, CancellationToken::new()).await;
    let body = wait_job(&state, &other, &second, Duration::from_secs(15)).await;
    assert_eq!(body["job"]["status"], "complete", "{body}");
    assert_eq!(body["job"]["matched"], 1);
    db.cleanup().await;
}

#[tokio::test]
async fn the_sweeper_resumes_a_job_whose_worker_died() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state).await;
    seed_pair(&state, &t).await;

    // A job whose worker died: claimed, snapshotted, then abandoned (the lease is
    // already expired — zero length).
    let firm = FirmId::from_uuid(Uuid::parse_str(&t.firm).unwrap());
    let client = ClientId::from_uuid(Uuid::parse_str(&t.client).unwrap());
    let dead = match recon_db::matching::start(&db.pool, firm, client, Duration::ZERO, "match-v1")
        .await
        .unwrap()
    {
        recon_db::matching::Start::Created(l) => l,
        other => panic!("expected the claim, got {other:?}"),
    };
    recon_db::matching::snapshot(&db.pool, &dead).await.unwrap();

    // The sweeper's round: claims the expired job and drives it to completion.
    recon_server::matching::resume_expired(&state, CancellationToken::new()).await;
    let body = wait_job(&state, &t, &dead.id.to_string(), Duration::from_secs(15)).await;
    assert_eq!(body["job"]["status"], "complete", "{body}");
    assert_eq!(body["job"]["matched"], 1);
    db.cleanup().await;
}
