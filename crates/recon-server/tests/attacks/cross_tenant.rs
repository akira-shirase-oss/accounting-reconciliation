//! Cross-tenant access: two firms on the one instance — a real session of firm
//! B pointed at firm A's ids is refused with nothing back (D-020: the composite
//! key is the boundary). And the operator token is not a session.

use super::harness::{Rig, get};
use crate::common::{post_json, tenant};

#[tokio::test]
async fn another_firms_routes_answer_404_to_this_session() {
    let rig = Rig::start().await;
    // Firm B lives on the SAME server as firm A — the session is real, the
    // ids are someone else's.
    let (firm_b, client_b, _) = tenant(&rig.server.base).await;
    let session_b = rig.enroll_in(&firm_b, &client_b, "reviewer").await;
    for path in [
        format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        format!("/v1/firms/{}/clients/{}/rules", rig.firm, rig.client),
    ] {
        let (status, body) = get(&rig.server.base, &path, Some(&session_b)).await;
        assert!(status == 403 || status == 404, "{path}: {status} {body}");
        assert!(
            body["client"].is_null() && body["items"].is_null() && body["lines"].is_null(),
            "{path} returned data: {body}"
        );
    }
    // And a mutation: B's session cannot enrol a user into firm A.
    let (status, body) = super::harness::post(
        &rig.server.base,
        &format!("/v1/firms/{}/users", rig.firm),
        &serde_json::json!({
            "email": "planted@example.test",
            "display_name": "planted",
            "role": "owner",
            "password": "a-fine-password-123",
            "clients": [rig.client],
        }),
        Some(&session_b),
    )
    .await;
    assert!(
        status == 401 || status == 403 || status == 404,
        "B's session planted a user in firm A: {status} {body}"
    );
    // The stored-state check: firm A holds no planted user.
    let planted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.users WHERE firm_id = $1 AND email = 'planted@example.test'",
    )
    .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!(planted, 0, "the planted user exists in firm A's store");
}

#[tokio::test]
async fn the_operator_token_is_not_a_session() {
    let rig = Rig::start().await;
    // The operator credential on a person-only route is not a confused deputy
    // (rules are a person's act; D-041 is transitional, not an identity). Note
    // the operator legitimately manages users — this probe takes a route only a
    // person may use.
    let (status, body) = post_json(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/rules", rig.firm, rig.client),
        &serde_json::json!({
            "name": "x",
            "predicate": {"narration": {"op": "contains", "value": "x"}},
            "action": {"kind": "exclude", "reason": "non_business"},
        }),
    )
    .await;
    assert!(status == 401 || status == 403, "{status} {body}");
    // The store agrees: no rule exists by that act.
    let rules: i64 = sqlx::query_scalar("SELECT count(*) FROM recon.rules WHERE firm_id = $1")
        .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
        .fetch_one(&rig.db.pool)
        .await
        .unwrap();
    assert_eq!(rules, 0, "the operator credential wrote a rule");
}
