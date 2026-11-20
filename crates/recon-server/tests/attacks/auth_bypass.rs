//! Auth bypass: nothing protected answers without a real session, a forged or
//! stolen-looking cookie is refused, a session without its CSRF cannot mutate,
//! and a signed-out session is dead.

use serde_json::json;

use super::harness::{Rig, get, post};

#[tokio::test]
async fn nothing_protected_answers_anonymously() {
    let rig = Rig::start().await;
    for path in [
        format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        format!("/v1/firms/{}/clients/{}/rules", rig.firm, rig.client),
        format!("/v1/firms/{}/users", rig.firm),
    ] {
        let (status, body) = get(&rig.server.base, &path, None).await;
        assert_eq!(status, 401, "{path}: {body}");
    }
}

#[tokio::test]
async fn a_forged_session_cookie_is_refused() {
    let rig = Rig::start().await;
    let fake = crate::harness::Session {
        cookie: "forged-session-token-not-real".to_owned(),
        csrf: "forged".to_owned(),
    };
    let (status, body) = get(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        Some(&fake),
    )
    .await;
    assert_eq!(status, 401, "{body}");
}

#[tokio::test]
async fn a_session_without_csrf_cannot_mutate() {
    let rig = Rig::start().await;
    let session = rig.enroll("reviewer").await;
    // The session cookie alone, no x-csrf-token: a real request a browser could
    // be tricked into making.
    let r = crate::common::http()
        .post(format!(
            "{}/v1/firms/{}/clients/{}/lines/{}/exclude",
            rig.server.base,
            rig.firm,
            rig.client,
            uuid::Uuid::new_v4()
        ))
        .header("cookie", format!("recon_session={}", session.cookie))
        .json(&json!({"expected_version": 1, "reason": "non_business"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
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
}
