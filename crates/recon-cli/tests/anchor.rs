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

async fn firm_with_decisions(db: &TestDb, n: usize) -> FirmId {
    let firm = FirmId::from_uuid(Uuid::now_v7());
    let client = ClientId::from_uuid(Uuid::now_v7());
    tenancy::create_firm(&db.pool, firm, "F").await.unwrap();
    tenancy::create_client(&db.pool, firm, client, "C")
        .await
        .unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    for _ in 0..n {
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
    firm
}

fn key_in(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let key = dir.join("anchor.key");
    std::fs::write(&key, [42_u8; 32]).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    key
}

/// N-1: anchoring signs only chains that verify — inside the database and against the
/// anchors already in the file — so a rewritten head is never signed.
#[tokio::test]
async fn anchor_refuses_to_sign_a_chain_that_does_not_verify() {
    let db = TestDb::new().await;
    let firm = firm_with_decisions(&db, 4).await;
    let dir = std::env::temp_dir().join(format!("recon-anchor-n1-{}", Uuid::now_v7()));
    let key = key_in(&dir);
    let anchors = dir.join("anchors.jsonl");
    let (path, firm_arg) = (anchors.to_str().unwrap(), firm.to_string());
    let (code, text) = recon(&["audit", "anchor", &firm_arg, "--out", path], &db, &key);
    assert_eq!(code, 0, "{text}");
    // The owner rewrites the tail consistently; then someone anchors again.
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
    let before = std::fs::read_to_string(&anchors).unwrap();
    let (code, text) = recon(&["audit", "anchor", &firm_arg, "--out", path], &db, &key);
    assert_ne!(code, 0, "a rewritten head was signed: {text}");
    assert_eq!(
        std::fs::read_to_string(&anchors).unwrap(),
        before,
        "nothing may be appended"
    );

    // A chain broken inside the database is refused even with no earlier anchor.
    let tampered = firm_with_decisions(&db, 3).await;
    let admin = db.admin_pool().await;
    let mut conn = admin.acquire().await.unwrap();
    conn.execute("SET session_replication_role = replica")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE recon.decision_log SET reason_code = 'approved' WHERE firm_id = $1 AND seq = 2",
    )
    .bind(tampered.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    admin.close().await;
    let fresh = dir.join("fresh.jsonl");
    let (code, text) = recon(
        &[
            "audit",
            "anchor",
            &tampered.to_string(),
            "--out",
            fresh.to_str().unwrap(),
        ],
        &db,
        &key,
    );
    assert_ne!(code, 0, "a tampered chain was signed: {text}");
    assert!(
        !fresh.exists() || std::fs::read_to_string(&fresh).unwrap().is_empty(),
        "nothing may be written"
    );
    let _ = std::fs::remove_dir_all(&dir);
    db.cleanup().await;
}

/// N-2: verifying a firm with no chains says so and fails, unless empty is expected.
#[tokio::test]
async fn verify_of_a_firm_with_no_chains_fails_unless_empty_is_expected() {
    let db = TestDb::new().await;
    let firm = FirmId::from_uuid(Uuid::now_v7());
    tenancy::create_firm(&db.pool, firm, "Empty").await.unwrap();
    let dir = std::env::temp_dir().join(format!("recon-anchor-n2-{}", Uuid::now_v7()));
    let key = key_in(&dir);
    let firm_arg = firm.to_string();
    let (code, text) = recon(&["audit", "verify", &firm_arg], &db, &key);
    assert_ne!(code, 0, "an empty verification passed: {text}");
    assert!(text.contains("0 chains checked"), "{text}");
    let (code, text) = recon(&["audit", "verify", &firm_arg, "--allow-empty"], &db, &key);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("0 chains checked"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
    db.cleanup().await;
}
