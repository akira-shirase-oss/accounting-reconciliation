//! `recon audit anchor` and `recon audit verify --anchors` (D-049), the real binary against a
//! test database: anchors verify, a key others can read is refused, an altered anchor line is
//! named, and a chain rewritten by its owner is caught although it is consistent inside the
//! database.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use recon_db::decisions::{self, NewDecision};
use recon_db::tenancy;
use recon_db::testing::TestDb;
use recon_domain::decision::SubjectKind;
use recon_domain::line::ActorKind;
use recon_domain::{ClientId, FirmId};
use sqlx::Executor as _;
use uuid::Uuid;

fn recon(args: &[&str], db: &TestDb, key: &std::path::Path) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_recon"))
        .args(args)
        .env("RECON_DATABASE_URL", db.app_url())
        .env("RECON_ANCHOR_KEY_FILE", key)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

#[tokio::test]
async fn anchors_verify_and_catch_tampering_and_an_owner_rewrite() {
    let db = TestDb::new().await;
    let firm = FirmId::from_uuid(Uuid::now_v7());
    let client = ClientId::from_uuid(Uuid::now_v7());
    tenancy::create_firm(&db.pool, firm, "F").await.unwrap();
    tenancy::create_client(&db.pool, firm, client, "C")
        .await
        .unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    for _ in 0..4 {
        decisions::insert(
            &mut tx,
            firm,
            &NewDecision {
                client,
                subject_kind: SubjectKind::Import,
                subject_id: Uuid::now_v7(),
                from_state: Some("staging"),
                to_state: "complete",
                actor_kind: ActorKind::Operator,
                actor_ref: "operator",
                reason_code: "import_completed",
                evidence: b"{}",
            },
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let dir = std::env::temp_dir().join(format!("recon-anchor-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("anchor.key");
    std::fs::write(&key, [42_u8; 32]).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let anchors = dir.join("anchors.jsonl");
    let anchors_arg = anchors.to_str().unwrap();
    let firm_arg = firm.to_string();

    let (code, text) = recon(
        &["audit", "anchor", &firm_arg, "--out", anchors_arg],
        &db,
        &key,
    );
    assert_ne!(code, 0, "a key others can read must be refused: {text}");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (code, text) = recon(
        &["audit", "anchor", &firm_arg, "--out", anchors_arg],
        &db,
        &key,
    );
    assert_eq!(code, 0, "{text}");
    let (code, text) = recon(
        &["audit", "verify", &firm_arg, "--anchors", anchors_arg],
        &db,
        &key,
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("1 anchors checked"), "{text}");

    // An altered anchor line is named.
    let original = std::fs::read_to_string(&anchors).unwrap();
    std::fs::write(&anchors, original.replace("\"seq\":4", "\"seq\":3")).unwrap();
    let (code, text) = recon(
        &["audit", "verify", &firm_arg, "--anchors", anchors_arg],
        &db,
        &key,
    );
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("anchor_signature_mismatch"), "{text}");
    std::fs::write(&anchors, &original).unwrap();

    // The owner rewrites the tail consistently: only the anchor catches it.
    let owner = db.owner_pool().await;
    let mut tx = owner.begin().await.unwrap();
    for sql in [
        "ALTER TABLE recon.decision_log DISABLE TRIGGER USER",
        "ALTER TABLE recon.chain_heads DISABLE TRIGGER USER",
        "DELETE FROM recon.decision_log WHERE seq > 2",
        "UPDATE recon.chain_heads h SET seq = 2, head_hash = d.record_hash FROM recon.decision_log d
          WHERE d.firm_id = h.firm_id AND d.client_id = h.client_id AND d.seq = 2",
        "ALTER TABLE recon.decision_log ENABLE TRIGGER USER",
        "ALTER TABLE recon.chain_heads ENABLE TRIGGER USER",
    ] {
        tx.execute(sql).await.unwrap();
    }
    tx.commit().await.unwrap();
    owner.close().await;
    let (code, text) = recon(&["audit", "verify", &firm_arg], &db, &key);
    assert_eq!(
        code, 0,
        "without anchors the rewrite is invisible (the stated limit): {text}"
    );
    let (code, text) = recon(
        &["audit", "verify", &firm_arg, "--anchors", anchors_arg],
        &db,
        &key,
    );
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("AnchorMismatch"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
    db.cleanup().await;
}
