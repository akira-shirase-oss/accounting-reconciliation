//! Header spoofing: `Forwarded`, `X-Forwarded-*`, `X-Original-*` and a lying
//! `Host` change nothing — the server binds its decisions to the connection it
//! sees, not to headers a client invents.

use serde_json::json;

use super::harness::{Rig, get};
use crate::common::http;

#[tokio::test]
async fn forwarded_headers_change_nothing() {
    let rig = Rig::start().await;
    let session = rig.enroll("reviewer").await;
    let path = format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client);
    let baseline = get(&rig.server.base, &path, Some(&session)).await.0;
    for (name, value) in [
        ("forwarded", "for=evil.example;host=other.example"),
        ("x-forwarded-for", "1.2.3.4"),
        ("x-real-ip", "5.6.7.8"),
        ("x-forwarded-host", "other.example"),
        ("x-original-url", "/v1/admin"),
        ("x-rewrite-url", "/v1/admin"),
    ] {
        let r = http()
            .get(format!("{}{path}", rig.server.base))
            .header("cookie", format!("recon_session={}", session.cookie))
            .header("x-csrf-token", &session.csrf)
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), baseline, "{name} changed the answer");
    }
}

#[tokio::test]
async fn a_host_header_lie_changes_nothing() {
    let rig = Rig::start().await;
    for host in ["evil.example", "localhost", "10.0.0.1:62480"] {
        let r = http()
            .get(format!("{}/healthz", rig.server.base))
            .header("host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "host {host}");
    }
}

#[tokio::test]
async fn a_spoofed_client_ip_does_not_beat_the_sign_in_back_off() {
    let rig = Rig::start().await;
    // Real credential stuffing rotates BOTH the account and the claimed client
    // IP — the throttle's address key must see through the header lies and lock
    // the real peer anyway. (Rotating the email is what isolates the address
    // key: one email would lock on the account key either way.)
    let mut locked = false;
    for i in 0..32 {
        let r = http()
            .post(format!("{}/v1/auth/sign-in", rig.server.base))
            .header("x-forwarded-for", format!("10.9.8.{i}"))
            .json(&json!({
                "firm": rig.firm,
                "email": format!("stuffed-{i}-{}@example.test", &rig.firm[..8]),
                "password": "wrong-password-1",
            }))
            .send()
            .await
            .unwrap();
        if r.status() == 429 {
            locked = true;
            break;
        }
    }
    assert!(locked, "32 forged-IP attempts never hit the back-off");
    // No refused attempt created a session.
    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM recon.sessions WHERE firm_id = $1")
            .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
            .fetch_one(&rig.db.pool)
            .await
            .unwrap();
    assert_eq!(sessions, 0, "a hammered sign-in minted a session");
}
