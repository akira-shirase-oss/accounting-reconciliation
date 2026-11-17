//! D-025: logs carry ids, codes, counts and durations, never secrets or financial content.
//! The real binary runs with debug logging through every R0 path (clean import, repeat,
//! refusal, a wrong token, a lookup) and every R1 path (a stored profile, an ambiguous
//! date, OFX, documents, a held revision listed and accepted), then its whole log is
//! scanned for the fixtures' distinctive content.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

mod common;

use common::{Server, TOKEN, assert_log_clean, http, post_json, upload};
use recon_db::testing::TestDb;
use serde_json::json;

#[tokio::test]
async fn logs_carry_no_secrets_or_financial_content() {
    let db = TestDb::new().await;
    let server = Server::start(&db.app_url(), &[("RECON_LOG", "debug")]).await;
    let base = server.base.clone();
    let (s, firm) = post_json(&base, "/v1/firms", &json!({"name": "Firm MARMOT-NAME"})).await;
    assert_eq!(s, 201);
    let firm = firm["firm"]["id"].as_str().unwrap().to_owned();
    let (_, client) = post_json(
        &base,
        &format!("/v1/firms/{firm}/clients"),
        &json!({"name": "Client OCELOT-NAME"}),
    )
    .await;
    let client = client["client"]["id"].as_str().unwrap().to_owned();
    let (_, account) = post_json(
        &base,
        &format!("/v1/firms/{firm}/clients/{client}/accounts"),
        &json!({"name": "Account 4401-9988-QUAIL", "currency": "USD"}),
    )
    .await;
    let account = account["account"]["id"].as_str().unwrap().to_owned();

    let header =
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";
    let clean = format!(
        "{header}2026-05-01,,98765.43,USD,NARR-QUOKKA-77 salary,REF-PLATYPUS-1,CPTY-WOMBAT Pty,123456.78\n\
         2026-05-02,,-4321.09,USD,\"NARR-AXOLOTL, rent\",REF-PLATYPUS-2,CPTY-WOMBAT Pty,\n"
    );
    let (s, body) = upload(&base, &firm, &account, clean.clone().into_bytes())
        .await
        .unwrap();
    assert_eq!(s, 201, "{body}");
    let (s, _) = upload(&base, &firm, &account, clean.into_bytes())
        .await
        .unwrap();
    assert_eq!(s, 200);
    let refused = format!(
        "{header}2026-05-03,,55.555,USD,NARR-PANGOLIN refused,REF-PLATYPUS-3,CPTY-NUMBAT,\n"
    );
    let (s, _) = upload(&base, &firm, &account, refused.into_bytes())
        .await
        .unwrap();
    assert_eq!(s, 422);
    let wrong = http()
        .post(format!("{base}/v1/firms"))
        .bearer_auth("wrong-token-SECRET-GUESS-0123456789abcdef")
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let import_id = body["import"]["id"].as_str().unwrap();
    let got = http()
        .get(format!("{base}/v1/firms/{firm}/imports/{import_id}"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    r1_paths(&base, &firm, &client, &account).await;
    let log = server.stop().await;
    assert_log_clean(
        &log,
        &[
            TOKEN,
            "SECRET-GUESS",
            "recon-app-dev",
            "NARR-QUOKKA",
            "NARR-AXOLOTL",
            "NARR-PANGOLIN",
            "REF-PLATYPUS",
            "CPTY-WOMBAT",
            "CPTY-NUMBAT",
            "98765.43",
            "9876543",
            "4321.09",
            "432109",
            "123456.78",
            "12345678",
            "55.555",
            "4401-9988-QUAIL",
            "MARMOT-NAME",
            "OCELOT-NAME",
            "NARR-CAPYBARA",
            "REF-TAPIR",
            "NARR-OKAPI",
            "FITID-GECKO",
            "CPTY-IBEX",
            "INV-KOALA",
            "CPTY-LEMUR",
            "DOC-DESC-NARWHAL",
            "PROFILE-COLUMN-YAK",
            "7777.77",
        ],
    );
    // The log is not empty of useful facts: the import's id and outcome are there.
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains(import_id) && text.contains("import finished"),
        "{text}"
    );
    db.cleanup().await;
}

/// R1's paths with distinctive content, every one answered as expected.
async fn r1_paths(base: &str, firm: &str, client: &str, account: &str) {
    let send = |path: String, content_type: &'static str, body: String| async move {
        let r = http()
            .post(format!("{base}{path}"))
            .bearer_auth(TOKEN)
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (
            status,
            r.json::<serde_json::Value>().await.unwrap_or_default(),
        )
    };
    // A stored profile whose dates are undeclared: the file is a question.
    let profile = json!({"name": "logged", "kind": "statement_csv", "definition": {
        "delimiter": ",", "header": ["PROFILE-COLUMN-YAK", "Narration", "Amount"],
        "columns": {"booking_date": "PROFILE-COLUMN-YAK", "narration": "Narration", "amount": {"signed": "Amount"}},
        "date_format": {"separator": "/"}, "number_format": {"decimal": "."}}});
    let (s, _) = post_json(base, &format!("/v1/firms/{firm}/profiles"), &profile).await;
    assert_eq!(s, 201);
    let (s, _) = send(
        format!("/v1/firms/{firm}/accounts/{account}/imports?profile=logged"),
        "text/csv",
        "PROFILE-COLUMN-YAK,Narration,Amount\n01/02/2026,NARR-OKAPI fee,-7777.77\n".into(),
    )
    .await;
    assert_eq!(s, 422);
    // OFX.
    let ofx = "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nENCODING:USASCII\nCHARSET:1252\n\n\
        <OFX><BANKMSGSRSV1><STMTTRNRS><STMTRS><CURDEF>USD<BANKTRANLIST>\
        <STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20260601<TRNAMT>-7777.77<FITID>FITID-GECKO\
        <NAME>CPTY-IBEX</STMTTRN></BANKTRANLIST></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>\n";
    let (s, body) = send(
        format!("/v1/firms/{firm}/accounts/{account}/imports?profile=ofx"),
        "application/x-ofx",
        ofx.into(),
    )
    .await;
    assert_eq!(s, 201, "{body}");
    // Documents.
    let docs = "kind,number,issue_date,due_date,amount,currency,counterparty,description\n\
        sales_invoice,INV-KOALA-1,2026-06-01,,7777.77,USD,CPTY-LEMUR,DOC-DESC-NARWHAL\n";
    let (s, body) = send(
        format!("/v1/firms/{firm}/clients/{client}/document-imports"),
        "text/csv",
        docs.into(),
    )
    .await;
    assert_eq!(s, 201, "{body}");
    // A held revision: the earlier statement's May rows again, one amount changed.
    let header =
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";
    let revised = format!(
        "{header}2026-05-01,,98765.43,USD,NARR-QUOKKA-77 salary,REF-PLATYPUS-1,CPTY-WOMBAT Pty,123456.78\n\
         2026-05-02,,-4321.10,USD,\"NARR-AXOLOTL, rent\",REF-PLATYPUS-2,CPTY-WOMBAT Pty,\n\
         2026-05-02,,-1.00,USD,NARR-CAPYBARA,REF-TAPIR,,\n"
    );
    let (s, body) = send(
        format!("/v1/firms/{firm}/accounts/{account}/imports"),
        "text/csv",
        revised,
    )
    .await;
    assert_eq!(s, 202, "{body}");
    let id = body["import"]["id"].as_str().unwrap().to_owned();
    let changes = http()
        .get(format!("{base}/v1/firms/{firm}/imports/{id}/changes"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(changes.status(), 200);
    let (s, body) = send(
        format!("/v1/firms/{firm}/imports/{id}/accept"),
        "application/json",
        String::new(),
    )
    .await;
    assert_eq!(s, 200, "{body}");
}

/// The scan's control: a log that does hold a needle must fail it, or a clean result proves
/// nothing.
#[test]
fn the_log_scan_catches_a_planted_leak() {
    let path = std::env::temp_dir().join(format!("recon-log-control-{}.log", std::process::id()));
    std::fs::write(
        &path,
        "INFO import finished rows=2\nINFO narration=NARR-QUOKKA-77 salary\n",
    )
    .unwrap();
    let caught = std::panic::catch_unwind(|| assert_log_clean(&path, &["NARR-QUOKKA"]));
    let _ = std::fs::remove_file(&path);
    assert!(
        caught.is_err(),
        "the scan passed a log that holds a narration"
    );
}
