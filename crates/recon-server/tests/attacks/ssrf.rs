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
    // The plausible shapes a URL-import or fetch route would take — all absent.
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
        let (status, _) = post(&rig.server.base, &path, &json!({"url": METADATA}), None).await;
        assert!(
            status == 401 || status == 404,
            "{path} exists as an outbound-capable surface: {status}"
        );
    }
}

#[tokio::test]
async fn a_url_in_a_schema_field_is_refused_or_inert() {
    let rig = Rig::start().await;
    // Every documented body is a closed schema: an unknown `url`/`uri`/`endpoint`
    // field is refused, never fetched.
    for (path, extra_key) in [
        ("/v1/firms".to_owned(), "url"),
        (format!("/v1/firms/{}/clients", rig.firm), "uri"),
        (
            format!("/v1/firms/{}/clients/{}/accounts", rig.firm, rig.client),
            "endpoint",
        ),
    ] {
        let r = crate::common::http()
            .post(format!("{}{path}", rig.server.base))
            .bearer_auth(TOKEN)
            .json(&json!({
                "name": "Attack Co",
                "currency": "USD",
                extra_key: METADATA,
            }))
            .send()
            .await
            .unwrap();
        assert!(
            r.status() == 422 || r.status() == 400,
            "{path} accepted a fetchable field: {}",
            r.status()
        );
    }
}

#[tokio::test]
async fn metadata_addresses_in_content_are_inert_strings() {
    let rig = Rig::start().await;
    // A narration that is a metadata URL is data in a field — the schema never
    // turns it into a request (D-016's content-is-data rule, proven here too).
    let session = rig.enroll("reviewer").await;
    let csv = format!(
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
         2026-04-02,,-1.00,USD,{METADATA},REF-1,Big Bank,\n"
    );
    let (status, _) =
        crate::common::upload(&rig.server.base, &rig.firm, &rig.account, csv.into_bytes())
            .await
            .unwrap();
    assert_eq!(status, 201);
    // The only assertion needed: it imported as a line (a string), and a queue
    // read serves the string back — nothing was fetched.
    let (status, body) = get(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}
