//! Replay: a mutation sent twice can never double-decide — the second answer is
//! the same recorded outcome or a conflict naming the winner, never a second
//! decision.

use serde_json::json;
use sqlx::Row as _;

use super::harness::{Rig, post};
use crate::common::upload;

const CSV: &str = "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
    2026-04-02,,-1.00,USD,REPLAY FEE,REF-1,Big Bank,\n";

#[tokio::test]
async fn a_replayed_mutation_cannot_decide_twice() {
    let rig = Rig::start().await;
    let session = rig.enroll("reviewer").await;
    let (status, _) = upload(&rig.server.base, &rig.firm, &rig.account, CSV.into())
        .await
        .unwrap();
    assert_eq!(status, 201);
    let line: String =
        sqlx::query_scalar("SELECT id::text FROM recon.statement_lines WHERE firm_id = $1 LIMIT 1")
            .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
            .fetch_one(&rig.db.pool)
            .await
            .unwrap();
    let version: i64 = sqlx::query_scalar(
        "SELECT row_version FROM recon.statement_lines WHERE firm_id = $1 LIMIT 1",
    )
    .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();

    let path = format!(
        "/v1/firms/{}/clients/{}/lines/{line}/exclude",
        rig.firm, rig.client
    );
    let body = json!({"expected_version": version, "reason": "non_business"});
    let (first, _) = post(&rig.server.base, &path, &body, Some(&session)).await;
    assert_eq!(first, 200);

    // The exact replay — the second answer is a conflict, and the log shows
    // one exclusion decision, never two.
    let (second, body) = post(&rig.server.base, &path, &body, Some(&session)).await;
    assert_eq!(second, 409, "{body}");
    let decisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.decision_log
          WHERE firm_id = $1 AND subject_kind = 'line' AND to_state = 'excluded'",
    )
    .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!(decisions, 1, "a replay wrote a second decision");
}

#[tokio::test]
async fn a_replayed_sign_in_pending_token_is_dead() {
    let rig = Rig::start().await;
    let _session = rig.enroll("reviewer").await;
    // Sign in again for the same user: the new pending is the live one; a stale
    // pending from the first flow answers as refused, not as a session.
    let email = format!("reviewer-{}@example.test", &rig.firm[..8]);
    let (status, body) = post(
        &rig.server.base,
        "/v1/auth/sign-in",
        &json!({"firm": rig.firm, "email": email, "password": "a-fine-password-123"}),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let pending = body["pending"].as_str().unwrap().to_owned();
    // The pending used once is consumed: replaying the exact TOTP answer fails.
    let row = sqlx::query("SELECT totp_secret FROM recon.users WHERE firm_id = $1 AND email = $2")
        .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
        .bind(&email)
        .fetch_one(&rig.db.pool)
        .await
        .unwrap();
    let secret: Vec<u8> = row.get(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let code = format!("{:06}", recon_server::auth::totp_code(&secret, now));
    let (s1, _) = post(
        &rig.server.base,
        "/v1/auth/totp",
        &json!({"pending": pending, "code": code}),
        None,
    )
    .await;
    assert_eq!(s1, 200);
    let (s2, body) = post(
        &rig.server.base,
        "/v1/auth/totp",
        &json!({"pending": pending, "code": code}),
        None,
    )
    .await;
    assert!(
        s2 == 401 || s2 == 404 || s2 == 410,
        "replayed pending: {body}"
    );
}
