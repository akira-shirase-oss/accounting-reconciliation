//! D-025: logs carry ids, codes, counts and durations, never secrets or financial content.
//! The real binary runs with debug logging through every R0 path (clean import, repeat,
//! refusal, a wrong token, a lookup), then its whole log is scanned for the fixture's
//! distinctive content.
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
