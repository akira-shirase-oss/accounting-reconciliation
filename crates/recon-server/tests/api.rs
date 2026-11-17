//! The API in process (the router over a real test database): the envelope, the operator
//! token, readiness, and the import path end to end with its bounds.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a panic is the failure report"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_cli::generate::{StatementSpec, statement};
use recon_db::testing::TestDb;
use recon_domain::{Currency, Date, FirmId};
use recon_server::{AppState, Config, faults::Faults, router};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "api-test-operator-token-0123456789abcdef";

fn state(db: &TestDb, overrides: &[(&str, &str)]) -> AppState {
    let url = db.app_url();
    let overrides: Vec<(String, String)> = overrides
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let config = Config::from_source(|name| {
        if let Some((_, v)) = overrides.iter().find(|(k, _)| k == name) {
            return Some(v.clone());
        }
        match name {
            "RECON_DATABASE_URL" => Some(url.clone()),
            "RECON_OPERATOR_TOKEN" => Some(TOKEN.to_owned()),
            _ => None,
        }
    })
    .unwrap();
    AppState::new(db.pool.clone(), config, Faults::default())
}

async fn checked(db: &TestDb, overrides: &[(&str, &str)]) -> AppState {
    let state = state(db, overrides);
    assert_eq!(
        recon_db::roles::check_runtime(&db.pool).await.unwrap(),
        vec![]
    );
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

fn post(path: &str, body: &Value) -> Request<Body> {
    Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn upload(firm: &str, account: &str, csv: Vec<u8>) -> Request<Body> {
    Request::post(format!("/v1/firms/{firm}/accounts/{account}/imports"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "text/csv")
        .body(Body::from(csv))
        .unwrap()
}

async fn tenant(state: &AppState) -> (String, String, String) {
    let (s, firm) = call(state, post("/v1/firms", &json!({"name": "Api Firm"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{firm}");
    let firm = firm["firm"]["id"].as_str().unwrap().to_owned();
    let (s, client) = call(
        state,
        post(
            &format!("/v1/firms/{firm}/clients"),
            &json!({"name": "Client"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    let client = client["client"]["id"].as_str().unwrap().to_owned();
    let (s, account) = call(
        state,
        post(
            &format!("/v1/firms/{firm}/clients/{client}/accounts"),
            &json!({"name": "Current", "currency": "USD"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{account}");
    (
        firm,
        client,
        account["account"]["id"].as_str().unwrap().to_owned(),
    )
}

fn generated(seed: u64, lines: u32) -> Vec<u8> {
    statement(&StatementSpec::month(
        seed,
        lines,
        Currency::Usd,
        Date::new(2026, 4, 1).unwrap(),
    ))
}

#[tokio::test]
async fn liveness_readiness_and_the_operator_token() {
    let db = TestDb::new().await;
    let state = state(&db, &[]);
    let (s, _) = call(
        &state,
        Request::get("/healthz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // Not ready until the startup check has passed.
    let (s, body) = call(&state, Request::get("/readyz").body(Body::empty()).unwrap()).await;
    assert_eq!(
        (s, body["reason"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("startup_check_pending")
        )
    );
    state.mark_checked();
    // Green once the background probe has reached the database (D-051).
    let shutdown = tokio_util::sync::CancellationToken::new();
    let probe = tokio::spawn(recon_server::tasks::readiness_probe(
        state.clone(),
        shutdown.clone(),
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (s, _) = call(&state, Request::get("/readyz").body(Body::empty()).unwrap()).await;
        if s == StatusCode::OK {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "readiness never turned green"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    probe.await.unwrap();

    let no_token = Request::post("/v1/firms").body(Body::from("{}")).unwrap();
    let (s, body) = call(&state, no_token).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("unauthorized"))
    );
    let wrong = Request::post("/v1/firms")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}x"))
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(call(&state, wrong).await.0, StatusCode::UNAUTHORIZED);
    let (s, body) = call(
        &state,
        Request::get("/nowhere").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_statement_imports_whole_once_and_a_repeat_answers_with_the_earlier_import() {
    let db = TestDb::new().await;
    let state = checked(&db, &[]).await;
    let (firm, _, account) = tenant(&state).await;
    let csv = generated(11, 12_000);
    let (s, body) = call(&state, upload(&firm, &account, csv.clone())).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["import"]["status"], "complete");
    assert_eq!(body["import"]["lines_created"], 12_000);
    assert_eq!(body["duplicate_file"], false);
    let first = body["import"]["id"].as_str().unwrap().to_owned();

    let (s, body) = call(&state, upload(&firm, &account, csv)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["duplicate_file"], true);
    assert_eq!(body["import"]["id"], first.as_str());

    let get = Request::get(format!("/v1/firms/{firm}/imports/{first}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (s, body) = call(&state, get).await;
    assert_eq!(
        (s, body["import"]["rows_read"].as_i64()),
        (StatusCode::OK, Some(12_000))
    );
    let firm_id = FirmId::from_uuid(firm.parse().unwrap());
    let report = recon_db::replay::check(&db.pool, firm_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (report.lines, report.complete_imports, report.imports),
        (12_000, 1, 2)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_malformed_file_is_refused_whole_with_row_numbers() {
    let db = TestDb::new().await;
    let state = checked(&db, &[]).await;
    let (firm, _, account) = tenant(&state).await;
    let mut csv = generated(12, 9_000);
    csv.extend_from_slice(
        b"2026-04-30,,12.345,USD,too precise,,,\n01/05/2026,,1.00,USD,ambiguous date,,,\n",
    );
    let (s, body) = call(&state, upload(&firm, &account, csv)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "file_refused");
    let import = &body["error"]["import"];
    assert_eq!(import["status"], "refused");
    assert_eq!(import["rows_refused"], 2);
    assert_eq!(
        import["refusals"],
        json!([
            {"row": 9_002, "code": "too_many_decimal_places"},
            {"row": 9_003, "code": "invalid_date"},
        ])
    );
    let lines: i64 = sqlx::query_scalar("SELECT count(*) FROM recon.statement_lines")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let staged: i64 = sqlx::query_scalar("SELECT count(*) FROM recon.import_staging")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!((lines, staged), (0, 0));

    let (s, body) = call(
        &state,
        upload(&firm, &account, b"date,amount\n2026-01-01,1\n".to_vec()),
    )
    .await;
    assert_eq!(
        (s, body["error"]["import"]["refusal_code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("header_mismatch"))
    );
    db.cleanup().await;
}

#[tokio::test]
async fn uploads_and_bodies_are_bounded_and_admission_is_refused_not_queued() {
    let db = TestDb::new().await;
    let state = checked(&db, &[("RECON_IMPORT_CONCURRENCY", "1")]).await;
    let (firm, _, account) = tenant(&state).await;

    let wrong_type = Request::post(format!("/v1/firms/{firm}/accounts/{account}/imports"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("[]"))
        .unwrap();
    assert_eq!(
        call(&state, wrong_type).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let declared_too_large = Request::post(format!("/v1/firms/{firm}/accounts/{account}/imports"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "text/csv")
        .header(header::CONTENT_LENGTH, (50 * 1024 * 1024 + 1).to_string())
        .body(Body::from("x"))
        .unwrap();
    let (s, body) = call(&state, declared_too_large).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("file_too_large"))
    );

    // Every permit taken: the next upload is refused at once, never queued.
    let held = state.import_permits.clone().try_acquire_owned().unwrap();
    let (s, body) = call(&state, upload(&firm, &account, generated(13, 10))).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("import_capacity"))
    );
    drop(held);
    assert_eq!(
        call(&state, upload(&firm, &account, generated(13, 10)))
            .await
            .0,
        StatusCode::CREATED
    );

    let big_json = post("/v1/firms", &json!({"name": "x".repeat(100 * 1024)}));
    assert_eq!(call(&state, big_json).await.0, StatusCode::BAD_REQUEST);
    db.cleanup().await;
}

#[tokio::test]
async fn another_firms_accounts_are_not_found_and_inputs_are_validated() {
    let db = TestDb::new().await;
    let state = checked(&db, &[]).await;
    let (firm_a, client_a, account_a) = tenant(&state).await;
    let (firm_b, _, _) = tenant(&state).await;
    let (s, body) = call(&state, upload(&firm_b, &account_a, generated(14, 5))).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    let (s, _) = call(
        &state,
        post(
            &format!("/v1/firms/{firm_b}/clients/{client_a}/accounts"),
            &json!({"name": "x", "currency": "USD"}),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "a firm cannot open an account for another firm's client"
    );
    let (s, body) = call(
        &state,
        post(
            &format!("/v1/firms/{firm_a}/clients/{client_a}/accounts"),
            &json!({"name": "x", "currency": "XYZ"}),
        ),
    )
    .await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("unknown_currency"))
    );
    let (s, body) = call(&state, post("/v1/firms", &json!({"name": " padded "}))).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_name"))
    );
    let (s, _) = call(
        &state,
        post(
            &format!("/v1/firms/{}/clients", Uuid::now_v7()),
            &json!({"name": "x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&state, upload("not-a-uuid", &account_a, generated(14, 5))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    db.cleanup().await;
}

/// Regression: the lease was renewed only by staged batches, so an upload slower than one
/// batch per lease period was reaped by the sweeper while still live. Here a 1.5 s lease,
/// a sweeper every 100 ms, and an upload that trickles in over 4.5 s (margin for a loaded
/// machine; without timed renewal the import is still reaped after 1.5 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_upload_keeps_its_lease_until_the_parse_deadline() {
    let db = TestDb::new().await;
    let state = checked(
        &db,
        &[
            ("RECON_IMPORT_LEASE_MS", "1500"),
            ("RECON_SWEEP_INTERVAL_MS", "100"),
        ],
    )
    .await;
    let (firm, _, account) = tenant(&state).await;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let sweeper = tokio::spawn(recon_server::tasks::sweeper(
        state.clone(),
        shutdown.clone(),
    ));
    let csv = generated(15, 2_000);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
    let request = Request::post(format!("/v1/firms/{firm}/accounts/{account}/imports"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "text/csv")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap();
    let response = tokio::spawn({
        let state = state.clone();
        async move { call(&state, request).await }
    });
    for chunk in csv.chunks(csv.len().div_ceil(10)) {
        tx.send(Ok(axum::body::Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    }
    drop(tx);
    let (status, body) = response.await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    shutdown.cancel();
    sweeper.await.unwrap();
    db.cleanup().await;
}

fn wide_rows(first: u32, n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for i in first..first + n {
        let narration = format!("WIDE {i} {}", "N".repeat(7_900));
        let counterparty = format!("CP {i} {}", "C".repeat(3_900));
        out.extend_from_slice(
            format!(
                "2026-04-01,,-{}.00,USD,{narration},REF-{i},{counterparty},\n",
                i + 1
            )
            .as_bytes(),
        );
    }
    out
}

/// M-3 regression: staging batches were capped by rows only, so 5,000 wide rows (about
/// 24 KB of field text each, held raw and normalized) sat in memory before the first batch
/// was stored. Batches are now also capped by bytes: after 600 wide rows, one must already be
/// staged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wide_rows_are_staged_in_byte_bounded_batches() {
    let db = TestDb::new().await;
    let state = checked(&db, &[]).await;
    let (firm, _, account) = tenant(&state).await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(4);
    let request = Request::post(format!("/v1/firms/{firm}/accounts/{account}/imports"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "text/csv")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap();
    let response = tokio::spawn({
        let state = state.clone();
        async move { call(&state, request).await }
    });
    let head =
        b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n"
            .to_vec();
    tx.send(Ok(head.into())).await.unwrap();
    for chunk in wide_rows(0, 600).chunks(64 * 1024) {
        tx.send(Ok(axum::body::Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let staged = loop {
        let staged: i32 = sqlx::query_scalar(
            "SELECT coalesce(max(staged_rows), 0) FROM recon.imports WHERE status = 'staging'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if staged > 0 || std::time::Instant::now() > deadline {
            break staged;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(
        staged > 0 && staged < 600,
        "after 600 wide rows {staged} were staged: batches are not byte-bounded"
    );
    for chunk in wide_rows(600, 200).chunks(64 * 1024) {
        tx.send(Ok(axum::body::Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
    }
    drop(tx);
    let (status, body) = response.await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["import"]["lines_created"], 800);
    db.cleanup().await;
}

/// The staging budget (D-050) is shared: two wide uploads under the smallest budget both
/// complete (one waits while the other stores its batch), and every permit comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_staging_budget_is_shared_and_returned() {
    let db = TestDb::new().await;
    let state = checked(&db, &[("RECON_STAGING_BUDGET_MIB", "8")]).await;
    let total = state.staging_budget.available_permits();
    let (firm, client, account_a) = tenant(&state).await;
    let (s, account_b) = call(
        &state,
        post(
            &format!("/v1/firms/{firm}/clients/{client}/accounts"),
            &json!({"name": "Second", "currency": "USD"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let account_b = account_b["account"]["id"].as_str().unwrap().to_owned();
    let header =
        b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n"
            .to_vec();
    let mut uploads = tokio::task::JoinSet::new();
    for (account, first) in [(account_a, 0), (account_b, 10_000)] {
        let (state, firm) = (state.clone(), firm.clone());
        let mut csv = header.clone();
        csv.extend(wide_rows(first, 1_200));
        uploads.spawn(async move { call(&state, upload(&firm, &account, csv)).await });
    }
    while let Some(done) = uploads.join_next().await {
        let (status, body) = done.unwrap();
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    assert_eq!(
        state.staging_budget.available_permits(),
        total,
        "the budget must be returned in full"
    );
    db.cleanup().await;
}

/// M-4 regression: unauthenticated `/readyz` ran a database round trip per request with no
/// cap, so a flood of probes competed with uploads for the pool. Readiness is now a cached
/// state a background probe refreshes: 20 probes against an unreachable database answer at
/// once instead of waiting on it each time.
#[tokio::test]
async fn readiness_is_served_from_a_cache_not_a_query_per_probe() {
    let url = "postgres://recon_app:x@127.0.0.1:1/none";
    let config = Config::from_source(|name| match name {
        "RECON_DATABASE_URL" => Some(url.to_owned()),
        "RECON_OPERATOR_TOKEN" => Some(TOKEN.to_owned()),
        _ => None,
    })
    .unwrap();
    let pool = recon_db::connect_lazy(url, &config.pool).unwrap();
    let state = AppState::new(pool, config, Faults::default());
    state.mark_checked();
    let started = std::time::Instant::now();
    for _ in 0..20 {
        let (s, _) = call(&state, Request::get("/readyz").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "20 probes took {elapsed:?}: each one queried the database"
    );
}
