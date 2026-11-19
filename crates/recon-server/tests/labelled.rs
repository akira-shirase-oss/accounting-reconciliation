//! The labelled-set measurement (D-028, `END_GOAL`'s zero-false-auto-match contract): a
//! `gen-v3` month is imported and matched end to end through the real API. The ground
//! truth then judges every line: every auto-committed match must be one the truth
//! labelled `t1` (zero false auto-matches), every proposal must carry its labelled
//! tier, every exception its labelled kind, and every line must hold exactly one
//! final state.
//!
//! `RECON_LABELLED_LINES` sizes the run (default 2,000; the ≥100,000-line measurement
//! is a load-run variant).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests: a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_cli::generate::{MatchSpec, match_set};
use recon_db::testing::TestDb;
use recon_server::{AppState, Config, faults::Faults, router};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "labelled-test-operator-token-0123456789ab";

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

fn authed(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
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

async fn upload(state: &AppState, path: &str, bytes: Vec<u8>) {
    let (s, body) = call(
        state,
        authed(Request::post(path))
            .header(header::CONTENT_TYPE, "text/csv")
            .body(Body::from(bytes))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
}

async fn tenant(state: &AppState) -> (String, String, String) {
    let (s, firm) = call(
        state,
        authed(Request::post("/v1/firms"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"name": "Labelled firm"}).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{firm}");
    let firm = firm["firm"]["id"].as_str().unwrap().to_owned();
    let (s, client) = call(
        state,
        authed(Request::post(format!("/v1/firms/{firm}/clients")))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"name": "Client"}).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    let client = client["client"]["id"].as_str().unwrap().to_owned();
    let (s, account) = call(
        state,
        authed(Request::post(format!(
            "/v1/firms/{firm}/clients/{client}/accounts"
        )))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"name": "Current", "currency": "USD"}).to_string(),
        ))
        .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{account}");
    (
        firm,
        client,
        account["account"]["id"].as_str().unwrap().to_owned(),
    )
}

/// `row_no` (the CSV row, header = 1) of every truth line, by expected outcome.
fn truth(truth: &[u8]) -> BTreeMap<i32, (String, Vec<String>)> {
    String::from_utf8(truth.to_vec())
        .unwrap()
        .lines()
        .skip(1)
        .map(|row| {
            let f: Vec<&str> = row.split(',').collect();
            (
                f[0].parse::<i32>().unwrap() + 1, // header row shifts row_no by one
                (
                    f[1].to_owned(),
                    f[2].split('+')
                        .filter(|n| !n.is_empty())
                        .map(str::to_owned)
                        .collect(),
                ),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_labelled_set_has_zero_false_auto_matches() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let lines = std::env::var("RECON_LABELLED_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_u32);
    let spec = MatchSpec {
        seed: 2026_1119,
        lines,
        distractors: lines / 10,
        currency: recon_domain::Currency::Usd,
        start: recon_domain::Date::new(2026, 3, 1).unwrap(),
        days: 28,
    };
    let set = match_set(&spec);
    let truth = truth(&set.truth);
    assert_eq!(u32::try_from(truth.len()).unwrap(), lines);

    let (firm, client, account) = tenant(&state).await;
    // The firm's residual tolerances, pinned by the job (D-014): bank fees to 10.00.
    let admin = db.admin_pool().await;
    let n = sqlx::query("UPDATE recon.firms SET residual_tolerances = $1 WHERE id = $2")
        .bind(json!({"USD": {"bank_fee": 1000, "tax_withheld": 0, "rounding": 0}}))
        .bind(Uuid::parse_str(&firm).unwrap())
        .execute(&admin)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(n, 1, "the firm row was not found");
    let pinned: Value =
        sqlx::query_scalar("SELECT residual_tolerances FROM recon.firms WHERE id = $1")
            .bind(Uuid::parse_str(&firm).unwrap())
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(pinned["USD"]["bank_fee"], 1000, "{pinned}");

    upload(
        &state,
        &format!("/v1/firms/{firm}/accounts/{account}/imports"),
        set.statement,
    )
    .await;
    upload(
        &state,
        &format!("/v1/firms/{firm}/clients/{client}/document-imports"),
        set.documents,
    )
    .await;

    let jobs = format!("/v1/firms/{firm}/clients/{client}/matching-jobs");
    let (s, body) = call(
        &state,
        authed(Request::post(&jobs)).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let job = body["job"]["id"].as_str().unwrap().to_owned();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let body = loop {
        let (s, body) = call(
            &state,
            authed(Request::get(format!("{jobs}/{job}")))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        if body["job"]["status"] != "running" || std::time::Instant::now() >= deadline {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(body["job"]["status"], "complete", "{body}");
    let job_uuid = Uuid::parse_str(&job).unwrap();

    // Outcomes per line: its status, its proposal's tier, its exception's kind, and
    // the document numbers of any committed match.
    let firm_uuid = Uuid::parse_str(&firm).unwrap();
    let client_uuid = Uuid::parse_str(&client).unwrap();
    let statuses: Vec<(i32, String)> = sqlx::query_as(
        "SELECT row_no, status::text FROM recon.statement_lines
         WHERE firm_id = $1 AND client_id = $2 ORDER BY row_no",
    )
    .bind(firm_uuid)
    .bind(client_uuid)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let proposals: Vec<(i32, String)> = sqlx::query_as(
        "SELECT l.row_no, m.tier::text FROM recon.proposals p
         JOIN recon.statement_lines l ON l.firm_id = p.firm_id AND l.id = p.line_id
         JOIN recon.matches m ON m.firm_id = p.firm_id AND m.id = p.match_id
         WHERE p.firm_id = $1 AND p.client_id = $2",
    )
    .bind(firm_uuid)
    .bind(client_uuid)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let exceptions: Vec<(i32, String)> = sqlx::query_as(
        "SELECT l.row_no, e.kind::text FROM recon.line_exceptions e
         JOIN recon.statement_lines l ON l.firm_id = e.firm_id AND l.id = e.line_id
         WHERE e.firm_id = $1 AND e.client_id = $2",
    )
    .bind(firm_uuid)
    .bind(client_uuid)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let committed: Vec<(i32, String)> = sqlx::query_as(
        "SELECT l.row_no, d.number_norm FROM recon.matches m
         JOIN recon.match_lines ml ON ml.firm_id = m.firm_id AND ml.match_id = m.id
         JOIN recon.statement_lines l ON l.firm_id = m.firm_id AND l.id = ml.line_id
         JOIN recon.match_documents md ON md.firm_id = m.firm_id AND md.match_id = m.id
         JOIN recon.documents d ON d.firm_id = m.firm_id AND d.id = md.document_id
         WHERE m.firm_id = $1 AND m.client_id = $2 AND m.status = 'committed'
         ORDER BY l.row_no, d.number_norm",
    )
    .bind(firm_uuid)
    .bind(client_uuid)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let mut matched_docs: BTreeMap<i32, Vec<String>> = BTreeMap::new();
    for (row_no, number) in committed {
        matched_docs.entry(row_no).or_default().push(number);
    }
    let proposal_tier: BTreeMap<i32, String> = proposals.into_iter().collect();
    let exception_kind: BTreeMap<i32, String> = exceptions.into_iter().collect();

    // The measurement: every line judged against its label.
    let mut per_bucket: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
    let mut false_auto = Vec::new();
    let mut wrong = Vec::new();
    for (row_no, status) in &statuses {
        let (label, expected_docs) = &truth[row_no];
        *per_bucket
            .entry(label.clone())
            .or_default()
            .entry(status.clone())
            .or_insert(0) += 1;
        let expected_status = match label.as_str() {
            "t1" => "matched",
            "t2" | "t3" | "t4" => "proposed",
            _ => "exception",
        };
        if status != expected_status {
            wrong.push(format!(
                "row {row_no}: labelled {label}, status {status}, exception {:?}",
                exception_kind.get(row_no)
            ));
            continue;
        }
        match label.as_str() {
            "t1" => {
                let docs = matched_docs.get(row_no).cloned().unwrap_or_default();
                if &docs != expected_docs {
                    wrong.push(format!(
                        "row {row_no}: committed against {docs:?}, truth {expected_docs:?}"
                    ));
                }
            }
            "t2" | "t3" | "t4" => {
                if proposal_tier.get(row_no) != Some(label) {
                    wrong.push(format!(
                        "row {row_no}: proposal tier {:?}, labelled {label}",
                        proposal_tier.get(row_no)
                    ));
                }
            }
            _ => {
                if exception_kind.get(row_no) != Some(label) {
                    wrong.push(format!(
                        "row {row_no}: exception {:?}, labelled {label}",
                        exception_kind.get(row_no)
                    ));
                }
            }
        }
    }
    // Zero false auto-matches: every committed match is a truth-labelled t1 pairing.
    for (row_no, docs) in &matched_docs {
        let (label, expected) = &truth[row_no];
        if label != "t1" || docs != expected {
            false_auto.push(format!(
                "row {row_no}: committed {docs:?}, truth {label} {expected:?}"
            ));
        }
    }
    eprintln!("labelled set outcome matrix (label -> status -> count): {per_bucket:#?}");
    assert!(false_auto.is_empty(), "false auto-matches: {false_auto:?}");
    assert!(wrong.is_empty(), "outcome mismatches: {wrong:?}");
    // Every line decided, nothing unreviewed left behind.
    assert!(
        statuses.iter().all(|(_, s)| s != "unreviewed"),
        "undecided lines remain"
    );
    // One decision per subject for the job.
    let duplicates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT subject_id FROM recon.decision_log
             WHERE firm_id = $1 AND job_id = $2
             GROUP BY subject_id HAVING count(*) > 1) d",
    )
    .bind(firm_uuid)
    .bind(job_uuid)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(duplicates, 0, "the job wrote two decisions about a line");
    // The log replays to the stored state.
    recon_db::replay::check(&db.pool, recon_domain::FirmId::from_uuid(firm_uuid))
        .await
        .unwrap()
        .expect("the decision log replays to the stored state");
    db.cleanup().await;
}
