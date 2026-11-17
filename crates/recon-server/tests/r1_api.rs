//! R1's routes in process (the router over a real test database): profiles, declared-profile
//! and OFX statement uploads, the ambiguous-date question, document imports, and held
//! revisions with their changes, accept and reject. Every new route needs the operator token
//! and bounds its body.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "integration tests: a panic is the failure report, and a scenario reads top to bottom"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use recon_db::testing::TestDb;
use recon_domain::FirmId;
use recon_server::{AppState, Config, faults::Faults, router};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "r1-api-test-operator-token-0123456789abcd";

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

async fn tenant(state: &AppState, currency: &str) -> Tenant {
    let (s, firm) = call(state, post_json("/v1/firms", &json!({"name": "R1 Firm"}))).await;
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
            &json!({"name": "Current", "currency": currency}),
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

fn indian_profile(order: Value) -> Value {
    json!({
        "delimiter": ",",
        "header": ["Date", "Narration", "Chq./Ref.No.", "Value Dt", "Withdrawal Amt.", "Deposit Amt.", "Closing Balance"],
        "columns": {
            "booking_date": "Date", "value_date": "Value Dt", "narration": "Narration",
            "reference": "Chq./Ref.No.", "balance_after": "Closing Balance",
            "amount": {"split": {"money_out": "Withdrawal Amt.", "money_in": "Deposit Amt."}}
        },
        "date_format": {"order": order, "separator": "/", "padded": true},
        "number_format": {"decimal": ".", "grouping": ",", "group_style": "indian"},
        "row_order": "newest_first"
    })
}

const INDIAN: &str = "Date,Narration,Chq./Ref.No.,Value Dt,Withdrawal Amt.,Deposit Amt.,Closing Balance\n\
    05/03/2026,NEFT CR ACME,N123,05/03/2026,,\"1,25,000.00\",\"2,30,450.50\"\n\
    03/03/2026,UPI/RENT,U77,03/03/2026,\"15,000.00\",,\"1,05,450.50\"\n\
    01/03/2026,ATM WDL,,01/03/2026,500.00,,\"1,20,450.50\"\n";

#[tokio::test]
async fn profiles_are_validated_versioned_and_drive_uploads() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state, "INR").await;
    let profiles = format!("/v1/firms/{}/profiles", t.firm);

    // Invalid: every problem named, nothing stored.
    let mut broken = indian_profile(json!("dmy"));
    broken["columns"]["reference"] = json!("Nope");
    broken["delimiter"] = json!("x");
    let (s, body) = call(
        &state,
        post_json(
            &profiles,
            &json!({"name": "hdfc", "kind": "statement_csv", "definition": broken}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "invalid_profile");
    assert_eq!(body["error"]["problems"].as_array().unwrap().len(), 2);
    for (name, definition) in [
        ("ofx", indian_profile(json!("dmy"))),
        ("Bad Name", indian_profile(json!("dmy"))),
        ("hdfc", json!({"unknown": true})),
    ] {
        let (s, body) = call(
            &state,
            post_json(
                &profiles,
                &json!({"name": name, "kind": "statement_csv", "definition": definition}),
            ),
        )
        .await;
        assert_eq!(
            (s, body["error"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_profile")),
            "{name}"
        );
    }

    // An undeclared order: stored, and its file is refused with the question.
    let (s, body) = call(
        &state,
        post_json(&profiles, &json!({"name": "hdfc", "kind": "statement_csv", "definition": indian_profile(Value::Null)})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["profile"]["version"], 1);
    let imports = format!(
        "/v1/firms/{}/accounts/{}/imports?profile=hdfc",
        t.firm, t.account
    );
    let (s, body) = call(&state, upload(&imports, "text/csv", INDIAN)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "ambiguous_date_format");
    let import = &body["error"]["import"];
    assert_eq!(import["status"], "refused");
    assert_eq!(import["question"]["options"], json!(["dmy", "mdy"]));
    assert_eq!(import["question"]["first_row"], 2);

    // The answer is a new version that declares the order.
    let (s, body) = call(
        &state,
        post_json(&profiles, &json!({"name": "hdfc", "kind": "statement_csv", "definition": indian_profile(json!("dmy"))})),
    )
    .await;
    assert_eq!(
        (s, body["profile"]["version"].as_i64()),
        (StatusCode::CREATED, Some(2))
    );
    let (s, body) = call(&state, upload(&imports, "text/csv", INDIAN)).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["import"]["profile"], "hdfc");
    assert_eq!(body["import"]["profile_version"], 2);
    assert_eq!(body["import"]["lines_created"], 3);
    assert_eq!(body["import"]["first_date"], "2026-03-01");
    // Version 1 is still there, unchanged, and can be named.
    let (s, body) = call(&state, get(&format!("{profiles}/hdfc?version=1"))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body["profile"]["definition"]["date_format"]["order"],
        Value::Null
    );
    let (s, body) = call(&state, get(&format!("{profiles}/r0-generic-csv"))).await;
    assert_eq!(
        (s, body["profile"]["version"].as_i64()),
        (StatusCode::OK, Some(2))
    );
    let (s, body) = call(&state, get(&format!("{profiles}/nope"))).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("unknown_profile"))
    );
    // A document profile cannot read a statement.
    let (s, body) = call(
        &state,
        upload(
            &format!(
                "/v1/firms/{}/accounts/{}/imports?profile=r1-generic-documents",
                t.firm, t.account
            ),
            "text/csv",
            INDIAN,
        ),
    )
    .await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("profile_kind_mismatch")
        )
    );
    // Another firm does not see this firm's profile.
    let other = tenant(&state, "INR").await;
    let (s, _) = call(
        &state,
        get(&format!("/v1/firms/{}/profiles/hdfc", other.firm)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    recon_db::replay::check(&db.pool, FirmId::from_uuid(t.firm.parse().unwrap()))
        .await
        .unwrap()
        .unwrap();
    db.cleanup().await;
}

const OFX: &str = "OFXHEADER:100\r\nDATA:OFXSGML\r\nVERSION:102\r\nSECURITY:NONE\r\nENCODING:USASCII\r\n\
CHARSET:1252\r\nCOMPRESSION:NONE\r\nOLDFILEUID:NONE\r\nNEWFILEUID:NONE\r\n\r\n\
<OFX><BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS>\
<STMTRS><CURDEF>USD<BANKACCTFROM><BANKID>1<ACCTID>2<ACCTTYPE>CHECKING</BANKACCTFROM>\
<BANKTRANLIST><DTSTART>20260301<DTEND>20260331\
<STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20260302120000<TRNAMT>-42.50<FITID>F1<NAME>ACME SUPPLY</STMTTRN>\
<STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260303<TRNAMT>100.00<FITID>F2<NAME>CLIENT PAY<MEMO>INV 7</STMTTRN>\
</BANKTRANLIST></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>\r\n";

#[tokio::test]
async fn ofx_statements_import_by_their_media_type_and_entities_are_refused() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state, "USD").await;
    let path = format!(
        "/v1/firms/{}/accounts/{}/imports?profile=ofx",
        t.firm, t.account
    );
    let (s, _) = call(&state, upload(&path, "text/csv", OFX)).await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (s, body) = call(&state, upload(&path, "application/x-ofx", OFX)).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["import"]["profile"], "ofx");
    assert_eq!(body["import"]["lines_created"], 2);
    let (s, body) = call(&state, upload(&path, "application/ofx", OFX)).await;
    assert_eq!(
        (s, body["duplicate_file"].as_bool()),
        (StatusCode::OK, Some(true))
    );
    let hostile = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hostile-files/ofx-billion-laughs.ofx"
    ))
    .unwrap();
    let (s, body) = call(&state, upload(&path, "application/x-ofx", hostile)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["import"]["refusal_code"],
        "declaration_refused"
    );
    db.cleanup().await;
}

const DOCS: &str = "kind,number,issue_date,due_date,amount,currency,counterparty,description\n\
    sales_invoice,INV-1,2026-04-01,2026-05-01,1180.00,INR,Acme Traders,\n\
    purchase_bill,B-9,2026-04-03,,-250.00,INR,Kestrel Freight,\n";

#[tokio::test]
async fn documents_import_through_their_route() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state, "INR").await;
    let path = format!("/v1/firms/{}/clients/{}/document-imports", t.firm, t.client);
    let (s, body) = call(&state, upload(&path, "text/csv", DOCS)).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["import"]["kind"], "documents");
    assert_eq!(body["import"]["profile"], "r1-generic-documents");
    assert_eq!(body["import"]["lines_created"], 2);
    let (s, body) = call(&state, upload(&path, "text/csv", DOCS)).await;
    assert_eq!(
        (s, body["duplicate_file"].as_bool()),
        (StatusCode::OK, Some(true))
    );
    let conflict = "kind,number,issue_date,due_date,amount,currency,counterparty,description\n\
        sales_invoice,INV-1,2026-04-01,,1.00,INR,Acme Traders,\n";
    let (s, body) = call(&state, upload(&path, "text/csv", conflict)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["import"]["refusals"],
        json!([{"row": 2, "code": "document_number_conflict"}])
    );
    // Another firm's client is not found; a statement profile cannot read documents.
    let other = tenant(&state, "INR").await;
    let (s, _) = call(
        &state,
        upload(
            &format!(
                "/v1/firms/{}/clients/{}/document-imports",
                other.firm, t.client
            ),
            "text/csv",
            DOCS,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, body) = call(
        &state,
        upload(&format!("{path}?profile=r0-generic-csv"), "text/csv", DOCS),
    )
    .await;
    assert_eq!(body["error"]["code"], "profile_kind_mismatch", "{s}");
    db.cleanup().await;
}

fn statement(rows: &[String]) -> String {
    let mut out =
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n"
            .to_owned();
    for r in rows {
        out.push_str(r);
        out.push('\n');
    }
    out
}

fn month(n: u32) -> Vec<String> {
    (0..n)
        .map(|i| {
            format!(
                "2026-03-{:02},,-{}.00,USD,PAYMENT {i},REF-{i},,",
                i + 1,
                i + 1
            )
        })
        .collect()
}

#[tokio::test]
async fn a_revision_is_held_listed_and_decided_through_the_api() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state, "USD").await;
    let imports = format!("/v1/firms/{}/accounts/{}/imports", t.firm, t.account);
    let (s, _) = call(&state, upload(&imports, "text/csv", statement(&month(10)))).await;
    assert_eq!(s, StatusCode::CREATED);
    let mut changed = month(10);
    changed[3] = changed[3].replace("-4.00", "-4.50");
    changed.remove(6);
    let (s, body) = call(&state, upload(&imports, "text/csv", statement(&changed))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["import"]["status"], "revision");
    assert_eq!(body["revision"]["changes"], 2);
    let id = body["import"]["id"].as_str().unwrap().to_owned();
    let base = format!("/v1/firms/{}/imports/{id}", t.firm);
    let (s, body) = call(&state, get(&format!("{base}/changes"))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let kinds: Vec<&str> = body["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["change"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["changed", "removed"]);
    assert_eq!(body["changes"][0]["fields"], json!(["amount"]));
    assert_eq!(body["changes"][0]["row"]["amount_minor"], -450);
    let (s, body) = call(&state, get(&format!("{base}/changes?after=1&limit=1"))).await;
    assert_eq!(
        (s, body["changes"][0]["ordinal"].as_i64()),
        (StatusCode::OK, Some(2))
    );
    let (s, _) = call(&state, get(&format!("{base}/changes?bogus=1"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let accept = authed(Request::post(format!("{base}/accept")))
        .body(Body::empty())
        .unwrap();
    let (s, body) = call(&state, accept).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["import"]["status"], "complete");
    assert_eq!(body["import"]["lines_created"], 1);
    let again = authed(Request::post(format!("{base}/accept")))
        .body(Body::empty())
        .unwrap();
    let (s, body) = call(&state, again).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_a_revision"))
    );
    // Another firm cannot see or decide it.
    let other = tenant(&state, "USD").await;
    let (s, _) = call(
        &state,
        get(&format!("/v1/firms/{}/imports/{id}/changes", other.firm)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let foreign = authed(Request::post(format!(
        "/v1/firms/{}/imports/{id}/reject",
        other.firm
    )))
    .body(Body::empty())
    .unwrap();
    assert_eq!(call(&state, foreign).await.0, StatusCode::NOT_FOUND);
    recon_db::replay::check(&db.pool, FirmId::from_uuid(t.firm.parse().unwrap()))
        .await
        .unwrap()
        .unwrap();
    db.cleanup().await;
}

/// Every R1 route needs the operator token, and JSON bodies stay bounded.
#[tokio::test]
async fn every_new_route_needs_the_token_and_bounds_its_body() {
    let db = TestDb::new().await;
    let state = checked(&db);
    let t = tenant(&state, "USD").await;
    let import = Uuid::now_v7();
    let routes = [
        ("POST", format!("/v1/firms/{}/profiles", t.firm)),
        (
            "GET",
            format!("/v1/firms/{}/profiles/r0-generic-csv", t.firm),
        ),
        (
            "POST",
            format!("/v1/firms/{}/clients/{}/document-imports", t.firm, t.client),
        ),
        (
            "GET",
            format!("/v1/firms/{}/imports/{import}/changes", t.firm),
        ),
        (
            "POST",
            format!("/v1/firms/{}/imports/{import}/accept", t.firm),
        ),
        (
            "POST",
            format!("/v1/firms/{}/imports/{import}/reject", t.firm),
        ),
    ];
    for (method, path) in &routes {
        let request = Request::builder()
            .method(*method)
            .uri(path)
            .header(header::CONTENT_TYPE, "text/csv")
            .body(Body::from("x"))
            .unwrap();
        let (s, body) = call(&state, request).await;
        assert_eq!(
            (s, body["error"]["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("unauthorized")),
            "{method} {path}"
        );
    }
    let big = post_json(
        &format!("/v1/firms/{}/profiles", t.firm),
        &json!({"name": "big", "kind": "statement_csv", "definition": {"pad": "x".repeat(100 * 1024)}}),
    );
    let (s, body) = call(&state, big).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_body"))
    );
    let declared_too_large = authed(Request::post(format!(
        "/v1/firms/{}/clients/{}/document-imports",
        t.firm, t.client
    )))
    .header(header::CONTENT_TYPE, "text/csv")
    .header(header::CONTENT_LENGTH, (50 * 1024 * 1024 + 1).to_string())
    .body(Body::from("x"))
    .unwrap();
    let (s, body) = call(&state, declared_too_large).await;
    assert_eq!(
        (s, body["error"]["code"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("file_too_large"))
    );
    db.cleanup().await;
}
