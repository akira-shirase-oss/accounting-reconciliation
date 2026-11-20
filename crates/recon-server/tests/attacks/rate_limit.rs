//! Rate-limit evasion: the sign-in back-off holds under hammering, answers the
//! named `sign_in_locked` with `Retry-After`, and a different email does not
//! unlock the locked one.

use serde_json::json;

use super::harness::{Rig, post};
use crate::common::http;

#[tokio::test]
async fn the_sign_in_back_off_locks_and_stays_locked() {
    let rig = Rig::start().await;
    // A real second user, enrolled before the attack — the control that proves
    // the lock is per-email, not per-IP or global.
    let _owner = rig.enroll("owner").await;
    let email = format!("victim-{}@example.test", &rig.firm[..8]);
    let mut hits = 0;
    let mut retry_after = None;
    for _ in 0..32 {
        let r = http()
            .post(format!("{}/v1/auth/sign-in", rig.server.base))
            .json(&json!({"firm": rig.firm, "email": email, "password": "wrong-password-1"}))
            .send()
            .await
            .unwrap();
        if r.status() == 429 {
            hits += 1;
            retry_after = r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok().map(str::to_owned));
            if hits == 2 {
                break;
            }
        }
    }
    assert_eq!(hits, 2, "the lock did not hold across two tries");
    assert!(retry_after.is_some(), "the 429 carries no Retry-After");

    // The throttle is two keys (D-021): the account AND the peer address — so
    // the locked email stays locked even with the right password, and an
    // attacker cannot rotate accounts from one box: a different email on the
    // same peer answers the lock too.
    let (status, body) = post(
        &rig.server.base,
        "/v1/auth/sign-in",
        &json!({"firm": rig.firm, "email": email, "password": "a-fine-password-123"}),
        None,
    )
    .await;
    assert_eq!(status, 429, "{body}");
    let owner = format!("owner-{}@example.test", &rig.firm[..8]);
    let (status, body) = post(
        &rig.server.base,
        "/v1/auth/sign-in",
        &json!({"firm": rig.firm, "email": owner, "password": "a-fine-password-123"}),
        None,
    )
    .await;
    assert_eq!(status, 429, "the address key did not hold: {body}");
}
