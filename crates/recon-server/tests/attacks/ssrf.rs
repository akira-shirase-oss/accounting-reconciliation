//! SSRF: there is no outbound-capable surface. No route accepts a URL to fetch
//! (D-024: no URL import, no webhook), metadata addresses in any field are just
//! strings the schema refuses or stores as inert data, and nothing named like a
//! fetch/proxy/import-from-url endpoint exists.

use serde_json::json;

use super::harness::{Rig, get, post};
use crate::common::TOKEN;

const METADATA: &str = "http://169.254.169.254/latest/meta-data/";

#[tokio::test]
async fn no_route_fetches_a_url() {
    let rig = Rig::start().await;
    // Authenticated — an anonymous probe only exercises the auth gate: a 401
    // can't tell "no such route" apart from "route exists, you're not on it".
    // With a real session every one of these must be the fallback's 404.
    let session = rig.enroll("owner").await;
    for path in [
        format!("/v1/firms/{}/imports", rig.firm),
        format!(
            "/v1/firms/{}/accounts/{}/imports/from-url",
            rig.firm, rig.account
        ),
        format!("/v1/firms/{}/fetch", rig.firm),
        "/v1/proxy".to_owned(),
        "/v1/webhooks".to_owned(),
    ] {
        let (status, body) = post(
            &rig.server.base,
            &path,
            &json!({"url": METADATA}),
            Some(&session),
        )
        .await;
        assert_eq!(
            status, 404,
            "{path} exists as an outbound-capable surface: {status} {body}"
        );
    }
}

#[tokio::test]
async fn a_url_in_a_schema_field_is_refused_or_inert() {
    let rig = Rig::start().await;
    // Every documented body is a closed schema: an unknown `url`/`uri`/`endpoint`
    // field is refused, never fetched.
    // Each route's own VALID body plus exactly one unknown URL field, so the
    // refusal names the extra field alone — an unrelated bad field would make
    // this test prove nothing about the schema's closedness.
    for (path, valid_body, extra_key) in [
        ("/v1/firms".to_owned(), json!({"name": "Attack Co"}), "url"),
        (
            format!("/v1/firms/{}/clients", rig.firm),
            json!({"name": "Attack Client"}),
            "uri",
        ),
        (
            format!("/v1/firms/{}/clients/{}/accounts", rig.firm, rig.client),
            json!({"name": "Attack Account", "currency": "USD"}),
            "endpoint",
        ),
    ] {
        let mut body = valid_body;
        body[extra_key] = json!(METADATA);
        let r = crate::common::http()
            .post(format!("{}{path}", rig.server.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            r.status() == 422 || r.status() == 400,
            "{path} accepted a fetchable field: {} {}",
            r.status(),
            r.text().await.unwrap_or_default()
        );
    }
    // The stored-state check: the refused bodies created nothing — one firm,
    // one client, one account, exactly as `tenant` made them.
    let (firms, clients, accounts): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM recon.firms), (SELECT count(*) FROM recon.clients),
                (SELECT count(*) FROM recon.accounts)",
    )
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!(
        (firms, clients, accounts),
        (1, 1, 1),
        "a refused create wrote a row"
    );
}

#[tokio::test]
async fn metadata_addresses_in_content_are_inert_strings() {
    let rig = Rig::start().await;
    // The egress assertion: the URL in the content points at a listener this
    // test owns — if anything resolves or fetches it, a connection lands here
    // and is counted. A listener that counts is stronger than inspecting
    // strings: it watches the behavior, not the payload's shape.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accept = {
        let hits = hits.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        drop(stream);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
    };
    // A narration that is a metadata-looking URL is data in a field — the
    // schema never turns it into a request (D-016's content-is-data rule).
    let url = format!("http://127.0.0.1:{port}/latest/meta-data/");
    let session = rig.enroll("reviewer").await;
    let csv = format!(
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
         2026-04-02,,-1.00,USD,{url},REF-1,Big Bank,\n"
    );
    let (status, refusal) =
        crate::common::upload(&rig.server.base, &rig.firm, &rig.account, csv.into_bytes())
            .await
            .unwrap();
    assert_eq!(status, 201, "{refusal}");
    // It imported as a line (a string), and a queue read serves the string
    // back — nothing was fetched. The store holds it VERBATIM.
    let (status, body) = get(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let stored: String = sqlx::query_scalar(
        "SELECT narration_raw FROM recon.statement_lines WHERE firm_id = $1 LIMIT 1",
    )
    .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!(stored, url, "the narration was interpreted, not stored");
    accept.join().unwrap();
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the content's URL was fetched"
    );
}
