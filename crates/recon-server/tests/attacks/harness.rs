//! The attack rig: a real spawned server over a scratch database, one tenant, and
//! sessions enrolled end-to-end over HTTP — the attacks see only what a client
//! could. Dropping the rig kills the child (the spawn sets `kill_on_drop`).

use std::time::{SystemTime, UNIX_EPOCH};

use recon_db::testing::TestDb;
use recon_server::auth::totp_code;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{Server, http, post_json, tenant};

pub(crate) struct Rig {
    pub(crate) db: TestDb,
    pub(crate) server: Server,
    pub(crate) firm: String,
    pub(crate) client: String,
    pub(crate) account: String,
}

pub(crate) struct Session {
    pub(crate) cookie: String,
    pub(crate) csrf: String,
}

impl Rig {
    /// A migrated scratch database, a real server on a free port, one tenant.
    pub(crate) async fn start() -> Self {
        let db = TestDb::new().await;
        let server = Server::start(&db.app_url(), &[]).await;
        let (firm, client, account) = tenant(&server.base).await;
        Self {
            db,
            server,
            firm,
            client,
            account,
        }
    }

    /// One enrolled user of `role` on this rig's client — the full HTTP flow, so
    /// the session the attacks carry is a real one.
    pub(crate) async fn enroll(&self, role: &str) -> Session {
        self.enroll_in(&self.firm, &self.client, role).await
    }

    /// One enrolled user of `role` on `firm`/`client` — the cross-tenant tests
    /// put two firms on the one server, and a session belongs to exactly one.
    pub(crate) async fn enroll_in(&self, firm: &str, client: &str, role: &str) -> Session {
        let email = format!("{role}-{}@example.test", &firm[..8]);
        let (s, body) = post_json(
            &self.server.base,
            &format!("/v1/firms/{firm}/users"),
            &json!({
                "email": email,
                "display_name": role,
                "role": role,
                "password": "a-fine-password-123",
                "clients": [client],
            }),
        )
        .await;
        assert_eq!(s, 201, "{body}");

        let (s, body) = post_json(
            &self.server.base,
            "/v1/auth/sign-in",
            &json!({"firm": firm, "email": email, "password": "a-fine-password-123"}),
        )
        .await;
        assert_eq!(s, 200, "{body}");
        let pending = body["pending"].as_str().unwrap().to_owned();

        let secret: Vec<u8> = sqlx::query_scalar(
            "SELECT totp_secret FROM recon.users WHERE firm_id = $1 AND email = $2",
        )
        .bind(Uuid::parse_str(firm).unwrap())
        .bind(&email)
        .fetch_one(&self.db.pool)
        .await
        .unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let code = format!("{:06}", totp_code(&secret, now));

        let response = http()
            .post(format!("{}/v1/auth/totp", self.server.base))
            .json(&json!({"pending": pending, "code": code}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let cookie = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("recon_session=")
            .unwrap()
            .to_owned();
        let body: Value = response.json().await.unwrap();
        Session {
            cookie,
            csrf: body["session"]["csrf"].as_str().unwrap().to_owned(),
        }
    }
}

/// A GET to `path` as `session` — or anonymously when `session` is `None`.
pub(crate) async fn get(base: &str, path: &str, session: Option<&Session>) -> (u16, Value) {
    let mut request = http().get(format!("{base}{path}"));
    if let Some(s) = session {
        request = request
            .header("cookie", format!("recon_session={}", s.cookie))
            .header("x-csrf-token", &s.csrf);
    }
    let r = request.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// A POST with `body` as `session` — or anonymously.
pub(crate) async fn post(
    base: &str,
    path: &str,
    body: &Value,
    session: Option<&Session>,
) -> (u16, Value) {
    let mut request = http().post(format!("{base}{path}")).json(body);
    if let Some(s) = session {
        request = request
            .header("cookie", format!("recon_session={}", s.cookie))
            .header("x-csrf-token", &s.csrf);
    }
    let r = request.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}
