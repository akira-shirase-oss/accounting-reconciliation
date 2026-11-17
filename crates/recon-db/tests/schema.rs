//! What the schema itself enforces, proven by trying to break it — as the server's role and,
//! where the guard must hold for raw SQL too, as the owner and the superuser.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests: a panic is the failure report"
)]

mod common;

use common::{OPERATOR, import, rows, sqlstate, tenant};
use recon_db::decisions::{self, NewDecision};
use recon_db::imports;
use recon_db::roles;
use recon_db::tenancy;
use recon_db::testing::TestDb;
use recon_domain::decision::{BreakKind, ChainBreak, SubjectKind};
use recon_domain::fingerprint::{TupleHash, fingerprint, occurrences};
use recon_domain::line::ActorKind;
use recon_domain::{AccountId, ClientId, Currency, FirmId};
use sqlx::{Connection, Executor, PgConnection, Row};
use uuid::Uuid;

/// Appends `n` decisions with varied fields to `client`'s chain.
async fn decide(db: &TestDb, firm: FirmId, client: ClientId, n: usize) {
    let mut tx = db.pool.begin().await.unwrap();
    for i in 0..n {
        let evidence = if i % 3 == 0 {
            Vec::new()
        } else {
            vec![u8::try_from(i % 256).unwrap(); i * 37]
        };
        let actor_ref = format!("person-{i}-é₹");
        decisions::insert(
            &mut tx,
            firm,
            &NewDecision {
                client,
                subject_kind: SubjectKind::Line,
                subject_id: Uuid::now_v7(),
                from_state: (i % 2 == 1).then_some("unreviewed"),
                to_state: "proposed",
                actor_kind: ActorKind::Engine,
                actor_ref: &actor_ref,
                reason_code: "t2_exact_amount",
                evidence: &evidence,
            },
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn the_decision_log_refuses_update_delete_and_truncate_even_for_its_owner() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    decide(&db, account.firm, account.client, 3).await;

    // The server's role has no such privilege at all.
    for sql in [
        "UPDATE recon.decision_log SET reason_code = 'approved'",
        "DELETE FROM recon.decision_log",
        "TRUNCATE recon.decision_log",
    ] {
        assert_eq!(sqlstate(db.pool.execute(sql).await), "42501", "{sql}");
    }
    // The owner has the privileges, and the triggers still refuse.
    let owner = db.owner_pool().await;
    for sql in [
        "UPDATE recon.decision_log SET reason_code = 'approved'",
        "DELETE FROM recon.decision_log",
        "TRUNCATE recon.decision_log",
    ] {
        assert_eq!(sqlstate(owner.execute(sql).await), "RCA01", "{sql}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM recon.decision_log")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 3);
    owner.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn a_cross_firm_reference_fails_in_the_schema() {
    let db = TestDb::new().await;
    let a = tenant(&db.pool).await;
    let b = tenant(&db.pool).await;
    // Raw SQL as the owner: the composite keys refuse what no repository would attempt.
    let owner = db.owner_pool().await;
    let account_in_b_for_a_client = sqlx::query(
        "INSERT INTO recon.accounts (firm_id, client_id, id, name, currency) VALUES ($1, $2, $3, 'x', 'USD')",
    )
    .bind(b.firm.as_uuid())
    .bind(a.client.as_uuid())
    .bind(Uuid::now_v7())
    .execute(&owner)
    .await;
    assert_eq!(sqlstate(account_in_b_for_a_client), "23503");

    let import_in_a_for_b_account = sqlx::query(
        "INSERT INTO recon.imports (firm_id, id, client_id, account_id, currency, profile, profile_version, lease_token, lease_until)
         VALUES ($1, $2, $3, $4, 'USD', 'r0-generic-csv', 1, $5, now())",
    )
    .bind(a.firm.as_uuid())
    .bind(Uuid::now_v7())
    .bind(a.client.as_uuid())
    .bind(b.id.as_uuid())
    .bind(Uuid::now_v7())
    .execute(&owner)
    .await;
    assert_eq!(sqlstate(import_in_a_for_b_account), "23503");

    // A line of firm B pointing at firm A's import.
    let staging = imports::begin(&db.pool, &a, common::LEASE).await.unwrap();
    let line = sqlx::query(
        "INSERT INTO recon.statement_lines (firm_id, client_id, account_id, import_id, currency, row_no,
             booking_date, amount_minor, narration_raw, narration_norm, tuple_hash, occurrence, fingerprint)
         VALUES ($1, $2, $3, $4, 'USD', 2, '2026-03-01', 100, 'x', 'X', $5, 0, $5)",
    )
    .bind(b.firm.as_uuid())
    .bind(a.client.as_uuid())
    .bind(a.id.as_uuid())
    .bind(staging.id.as_uuid())
    .bind(vec![0_u8; 32])
    .execute(&owner)
    .await;
    assert_eq!(sqlstate(line), "23503");

    // A decision of firm B on firm A's client has no chain to join.
    let decision = sqlx::query(
        "INSERT INTO recon.decision_log (firm_id, client_id, id, subject_kind, subject_id, to_state,
             actor_kind, actor_ref, reason_code, evidence, engine_version)
         VALUES ($1, $2, $3, 'import', $3, 'complete', 'operator', 'operator', 'x', '', 'v')",
    )
    .bind(b.firm.as_uuid())
    .bind(a.client.as_uuid())
    .bind(Uuid::now_v7())
    .execute(&owner)
    .await;
    assert_eq!(sqlstate(decision), "RCA02");

    // The repositories are scoped too: firm B cannot load firm A's account.
    assert!(matches!(
        tenancy::account(&db.pool, b.firm, a.id).await,
        Err(recon_db::DbError::NotFound)
    ));
    owner.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn the_runtime_role_has_no_power_beyond_its_grants() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    assert_eq!(
        roles::check_runtime(&db.pool).await.unwrap(),
        vec![],
        "the app role must be clean"
    );

    let mut conn = db.pool.acquire().await.unwrap();
    let attempts = [
        "CREATE TABLE recon.evil (x int)",
        "CREATE TABLE public.evil (x int)",
        "CREATE TEMP TABLE evil (x int)",
        "CREATE SCHEMA evil",
        "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
        "TRUNCATE recon.import_staging",
        "TRUNCATE recon.statement_lines",
        "TRUNCATE recon.imports",
        "ALTER TABLE recon.decision_log DISABLE TRIGGER ALL",
        "DROP TABLE recon.decision_log",
        "SET ROLE recon_owner",
        "SET session_replication_role = replica",
        "UPDATE recon.statement_lines SET status = 'matched'",
        "DELETE FROM recon.imports",
        "DELETE FROM recon.statement_lines",
        "UPDATE recon.chain_heads SET seq = 0",
        "INSERT INTO recon.import_flips VALUES (gen_random_uuid(), gen_random_uuid())",
        "UPDATE recon.imports SET created_at = now() - interval '1 year'",
        "UPDATE recon.accounts SET currency = 'EUR'",
    ];
    for sql in attempts {
        assert_eq!(
            sqlstate(conn.execute(sql).await),
            "42501",
            "{sql} must be refused"
        );
    }
    drop(conn);

    // The owner and the superuser are not runtime roles: the startup check says why.
    let owner = db.owner_pool().await;
    let problems = roles::runtime_problems(&owner).await.unwrap();
    assert!(
        problems.iter().any(|p| p.0.contains("owns objects")),
        "{problems:?}"
    );
    let admin = db.admin_pool().await;
    let problems = roles::runtime_problems(&admin).await.unwrap();
    assert!(
        problems.iter().any(|p| p.0.contains("superuser")),
        "{problems:?}"
    );
    owner.close().await;
    admin.close().await;
    let _ = account;
    db.cleanup().await;
}

#[tokio::test]
async fn every_function_pins_search_path_and_the_check_can_fail() {
    let db = TestDb::new().await;
    let functions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'recon'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(functions >= 12, "{functions} functions");
    assert_eq!(
        roles::unpinned_functions(&db.pool).await.unwrap(),
        Vec::<String>::new()
    );

    let owner = db.owner_pool().await;
    owner
        .execute("CREATE FUNCTION recon.unpinned() RETURNS int LANGUAGE sql AS 'SELECT 1'")
        .await
        .unwrap();
    assert_eq!(
        roles::unpinned_functions(&db.pool).await.unwrap(),
        ["unpinned"]
    );
    let problems = roles::check_runtime(&db.pool).await.unwrap();
    assert!(
        problems.iter().any(|p| p.0.contains("recon.unpinned")),
        "{problems:?}"
    );
    owner.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn a_search_path_hijack_cannot_reach_the_chain() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    // An attacker able to create objects in a schema the session searches first plants a
    // sha256 that always returns one zero byte.
    let owner = db.owner_pool().await;
    owner
        .execute(
            "CREATE FUNCTION public.sha256(bytea) RETURNS bytea LANGUAGE sql AS $$ SELECT '\\x00'::bytea $$;
             GRANT EXECUTE ON FUNCTION public.sha256(bytea) TO recon_runtime;",
        )
        .await
        .unwrap();
    let mut conn = db.pool.acquire().await.unwrap();
    conn.execute("SET search_path = public, pg_catalog")
        .await
        .unwrap();
    // Control: in an unpinned context the planted function wins.
    let hijacked: Vec<u8> = sqlx::query_scalar("SELECT sha256('\\x01'::bytea)")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        hijacked,
        [0_u8],
        "the control hijack must work, or this test proves nothing"
    );
    // The chain trigger pins its search_path, so the same session still hashes correctly.
    let mut tx = conn.begin().await.unwrap();
    decisions::insert(
        &mut tx,
        account.firm,
        &NewDecision {
            client: account.client,
            subject_kind: SubjectKind::Import,
            subject_id: Uuid::now_v7(),
            from_state: None,
            to_state: "complete",
            actor_kind: ActorKind::Operator,
            actor_ref: "operator",
            reason_code: "import_completed",
            evidence: b"{}",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(conn);
    let head = decisions::verify_chain(&db.pool, account.firm, account.client)
        .await
        .unwrap()
        .expect("chain verifies");
    assert_eq!(head.seq, 1);
    owner.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn the_database_and_the_domain_hash_decisions_identically() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let other = ClientId::from_uuid(Uuid::now_v7());
    tenancy::create_client(&db.pool, account.firm, other, "Second client")
        .await
        .unwrap();
    decide(&db, account.firm, account.client, 25).await;
    decide(&db, account.firm, other, 30).await;
    decide(&db, account.firm, account.client, 25).await;
    let records = decisions::page(&db.pool, account.firm, account.client, 0, 1_000)
        .await
        .unwrap();
    assert_eq!(records.len(), 50);
    for r in &records {
        assert_eq!(r.computed_hash(), r.record_hash, "seq {}", r.seq);
    }
    assert_eq!(
        decisions::verify_chain(&db.pool, account.firm, account.client)
            .await
            .unwrap()
            .unwrap()
            .seq,
        50
    );
    assert_eq!(
        decisions::verify_chain(&db.pool, account.firm, other)
            .await
            .unwrap()
            .unwrap()
            .seq,
        30
    );
    db.cleanup().await;
}

#[tokio::test]
async fn audit_verify_names_a_tampered_row_and_a_removed_tail() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let second = ClientId::from_uuid(Uuid::now_v7());
    tenancy::create_client(&db.pool, account.firm, second, "Second")
        .await
        .unwrap();
    decide(&db, account.firm, account.client, 5).await;
    decide(&db, account.firm, second, 5).await;

    // Only a superuser in replica mode gets around the triggers (the limit D-019 states).
    let mut admin = PgConnection::connect(&db.admin_url()).await.unwrap();
    admin
        .execute("SET session_replication_role = replica")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE recon.decision_log SET reason_code = 'approved' WHERE client_id = $1 AND seq = 3",
    )
    .bind(account.client.as_uuid())
    .execute(&mut admin)
    .await
    .unwrap();
    sqlx::query("DELETE FROM recon.decision_log WHERE client_id = $1 AND seq = 5")
        .bind(second.as_uuid())
        .execute(&mut admin)
        .await
        .unwrap();
    admin.close().await.unwrap();

    let broken = decisions::verify_chain(&db.pool, account.firm, account.client)
        .await
        .unwrap();
    assert_eq!(
        broken,
        Err(ChainBreak {
            seq: 3,
            kind: BreakKind::RecordHashMismatch
        })
    );
    let truncated = decisions::verify_chain(&db.pool, account.firm, second)
        .await
        .unwrap();
    assert_eq!(
        truncated,
        Err(ChainBreak {
            seq: 5,
            kind: BreakKind::HeadMismatch
        })
    );
    let replay = recon_db::replay::check(&db.pool, account.firm)
        .await
        .unwrap();
    assert!(
        replay.is_err(),
        "the replay check must fail on a broken chain"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn lines_become_visible_only_with_their_import_and_never_change() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let staging = imports::begin(&db.pool, &account, common::LEASE)
        .await
        .unwrap();
    let insert_line = "INSERT INTO recon.statement_lines (firm_id, client_id, account_id, import_id, currency, row_no,
             booking_date, amount_minor, narration_raw, narration_norm, tuple_hash, occurrence, fingerprint)
         VALUES ($1, $2, $3, $4, 'USD', $6, '2026-03-01', 100, 'x', 'X', $5, 0, $5)";
    // Lines for a staging import, committed without the flip: the commit fails.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query(insert_line)
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(account.id.as_uuid())
        .bind(staging.id.as_uuid())
        .bind(vec![1_u8; 32])
        .bind(2_i32)
        .execute(&mut *tx)
        .await
        .unwrap();
    let commit = tx.commit().await;
    let code = commit
        .unwrap_err()
        .as_database_error()
        .and_then(|d| d.code().map(std::borrow::Cow::into_owned));
    assert_eq!(code.as_deref(), Some("RCL02"));
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM recon.statement_lines")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(visible, 0);

    // A complete import is final, and its lines are fixed.
    let done = import(&db.pool, &account, &rows(0, 3))
        .await
        .unwrap()
        .complete();
    let late = sqlx::query(insert_line)
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(account.id.as_uuid())
        .bind(done.id)
        .bind(vec![2_u8; 32])
        .bind(999_i32)
        .execute(&db.pool)
        .await;
    assert_eq!(sqlstate(late), "RCL01");
    let reopen = sqlx::query("UPDATE recon.imports SET status = 'staging' WHERE id = $1")
        .bind(done.id)
        .execute(&db.pool)
        .await;
    assert_eq!(sqlstate(reopen), "RCI01");
    let owner = db.owner_pool().await;
    assert_eq!(
        sqlstate(
            owner
                .execute("UPDATE recon.statement_lines SET amount_minor = 1")
                .await
        ),
        "RCL03"
    );
    assert_eq!(
        sqlstate(owner.execute("DELETE FROM recon.statement_lines").await),
        "RCL03"
    );
    assert_eq!(
        sqlstate(
            owner
                .execute("TRUNCATE recon.statement_lines CASCADE")
                .await
        ),
        "RCL03"
    );
    assert_eq!(
        sqlstate(owner.execute("DELETE FROM recon.imports").await),
        "RCI01"
    );
    // Status may change (from R2), source fields may not.
    assert_eq!(
        sqlstate(
            owner
                .execute("UPDATE recon.statement_lines SET status = 'proposed'")
                .await
        ),
        "ok"
    );
    owner.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn fingerprints_computed_in_sql_equal_the_domain_definition() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let body = format!(
        "{}2026-03-05,,-10,USD,ATM,,,\n2026-03-05,,-10,USD,ATM,,,\n2026-03-05,,-10,USD,ATM,,,\n",
        rows(0, 40)
    );
    import(&db.pool, &account, &body).await.unwrap().complete();
    let stored = sqlx::query(
        "SELECT tuple_hash, occurrence, fingerprint FROM recon.statement_lines ORDER BY row_no",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), 43);
    let tuples: Vec<TupleHash> = stored
        .iter()
        .map(|r| TupleHash(r.get::<Vec<u8>, _>("tuple_hash").try_into().unwrap()))
        .collect();
    let expected_occurrences = occurrences(&tuples);
    for ((row, tuple), occurrence) in stored.iter().zip(&tuples).zip(expected_occurrences) {
        let stored_occurrence: i32 = row.get("occurrence");
        assert_eq!(u32::try_from(stored_occurrence).unwrap(), occurrence);
        let fp: Vec<u8> = row.get("fingerprint");
        assert_eq!(fp, fingerprint(tuple, occurrence).0);
    }
    db.cleanup().await;
}

#[tokio::test]
async fn sslmode_require_connects_over_rustls_and_verify_full_checks_the_certificate() {
    let db = TestDb::new().await;
    let base = db.app_url();
    let sep = if base.contains('?') { '&' } else { '?' };
    let mut tls = PgConnection::connect(&format!("{base}{sep}sslmode=require"))
        .await
        .expect("sslmode=require over rustls");
    let ssl: bool = sqlx::query_scalar(
        "SELECT ssl FROM pg_catalog.pg_stat_ssl WHERE pid = pg_catalog.pg_backend_pid()",
    )
    .fetch_one(&mut tls)
    .await
    .unwrap();
    assert!(ssl, "the session must be encrypted");
    tls.close().await.unwrap();
    // The local certificate is self-signed: full verification must refuse it.
    let verified = PgConnection::connect(&format!("{base}{sep}sslmode=verify-full")).await;
    assert!(
        verified.is_err(),
        "verify-full must check the certificate chain"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_new_account_starts_with_an_empty_verified_chain() {
    let db = TestDb::new().await;
    let firm = FirmId::from_uuid(Uuid::now_v7());
    let client = ClientId::from_uuid(Uuid::now_v7());
    tenancy::create_firm(&db.pool, firm, "F").await.unwrap();
    tenancy::create_client(&db.pool, firm, client, "C")
        .await
        .unwrap();
    tenancy::create_account(
        &db.pool,
        firm,
        client,
        AccountId::from_uuid(Uuid::now_v7()),
        "A",
        Currency::Kwd,
    )
    .await
    .unwrap();
    let head = decisions::verify_chain(&db.pool, firm, client)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.seq, 0);
    assert_eq!(
        head.hash,
        recon_domain::decision::genesis_hash(firm, client)
    );
    let _ = OPERATOR;
    db.cleanup().await;
}
