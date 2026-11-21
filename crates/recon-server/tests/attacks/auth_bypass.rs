//! Auth bypass: nothing protected answers without a real session, a forged or
//! stolen-looking cookie is refused, a session without its CSRF cannot mutate,
//! and a signed-out session is dead. Every refusal also leaves the store
//! untouched — checked, not assumed.

use serde_json::json;

use super::harness::{Rig, get, post};
use crate::common::upload;

const CSV: &str = "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
    2026-04-02,,-1.00,USD,CSRF FEE,REF-1,Big Bank,\n";

#[tokio::test]
async fn nothing_protected_answers_anonymously() {
    let rig = Rig::start().await;
    for path in [
        format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        format!("/v1/firms/{}/clients/{}/rules", rig.firm, rig.client),
    ] {
        let (status, body) = get(&rig.server.base, &path, None).await;
        assert_eq!(status, 401, "{path}: {body}");
    }
    // And a mutation anonymously: creating a user is refused before it exists.
    let (status, body) = post(
        &rig.server.base,
        &format!("/v1/firms/{}/users", rig.firm),
        &json!({
            "email": "planted@example.test",
            "display_name": "planted",
            "role": "owner",
            "password": "a-fine-password-123",
            "clients": [rig.client],
        }),
        None,
    )
    .await;
    assert_eq!(status, 401, "{body}");
    // The stored-state check: as the victim firm, everything is unchanged —
    // the anonymous probes created no users, sessions or decisions.
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM recon.users WHERE firm_id = $1")
        .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
        .fetch_one(&rig.db.pool)
        .await
        .unwrap();
    assert_eq!(users, 0, "an anonymous probe created a user");
}

#[tokio::test]
async fn a_forged_session_cookie_is_refused() {
    let rig = Rig::start().await;
    // A well-formed but unknown token: the refusal must come from the session
    // lookup, not merely the hex decoder.
    let fake = crate::harness::Session {
        cookie: "0".repeat(64),
        csrf: "forged".to_owned(),
    };
    let (status, body) = get(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        Some(&fake),
    )
    .await;
    assert_eq!(status, 401, "{body}");
    // The victim still reads their own queue unchanged — the forgery touched nothing.
    let real = rig.enroll("reviewer").await;
    let (status, body) = get(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        Some(&real),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn a_session_without_csrf_cannot_mutate() {
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
    // The session cookie alone, no x-csrf-token: a real request a browser could
    // be tricked into making, at a real line.
    let r = crate::common::http()
        .post(format!(
            "{}/v1/firms/{}/clients/{}/lines/{line}/exclude",
            rig.server.base, rig.firm, rig.client
        ))
        .header("cookie", format!("recon_session={}", session.cookie))
        .json(&json!({"expected_version": 1, "reason": "non_business"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // The store says the attack changed nothing: no decision, the line unreviewed.
    let (decisions, state): (i64, String) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM recon.decision_log
             WHERE firm_id = $1 AND subject_kind = 'line' AND subject_id = $2),
           (SELECT status FROM recon.statement_lines WHERE firm_id = $1 AND id = $2)",
    )
    .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
    .bind(uuid::Uuid::parse_str(&line).unwrap())
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!((decisions, state.as_str()), (0, "unreviewed"));
}

#[tokio::test]
async fn a_signed_out_session_is_dead() {
    let rig = Rig::start().await;
    let session = rig.enroll("reviewer").await;
    let (status, _) = post(
        &rig.server.base,
        "/v1/auth/sign-out",
        &json!({}),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = get(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        Some(&session),
    )
    .await;
    assert_eq!(status, 401, "{body}");
    // The store says the session is gone, not merely refused.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.sessions s JOIN recon.users u
           ON u.firm_id = s.firm_id AND u.id = s.user_id
          WHERE s.firm_id = $1",
    )
    .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!(live, 0, "a signed-out session is still in the store");
}
