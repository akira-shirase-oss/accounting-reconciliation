//! R2's matching storage, proven against the schema and the `matching` module: the
//! over-allocation refusal for raw SQL, the one-running-job-per-client exclusion under
//! concurrency, fencing after a lease is lost, the candidate index by EXPLAIN, and the
//! durable chunk commit's idempotency.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests: a panic is the failure report"
)]

mod common;

use std::time::Duration;

use common::{import, import_documents, rows, sqlstate, tenant};
use recon_db::matching::{self, Decided, DecidedLine, Start};
use recon_db::tenancy::Account;
use recon_db::testing::TestDb;
use recon_domain::line::ExceptionKind;
use recon_domain::matching::{
    BalancedMatch, DocumentAllocation, LineAllocation, ScoreBreakdown, Tier,
};
use recon_domain::{Currency, DocumentId, LineId, Money};
use sqlx::Row;
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(30);
const TX_TIMEOUT: Duration = Duration::from_secs(15);
const ENGINE: &str = "match-v1";

fn usd(minor: i64) -> Money {
    Money::new(minor, Currency::Usd).unwrap()
}

/// One USD line (-1.00) and one paid bill (-1.00 as the bank sees it, D-007),
/// imported the way the server does.
async fn line_and_document(db: &TestDb) -> (Account, LineId, DocumentId) {
    let account = tenant(&db.pool).await;
    import(&db.pool, &account, &rows(0, 1))
        .await
        .unwrap()
        .complete();
    import_documents(
        &db.pool,
        &account,
        "purchase_bill,INV-1,2026-03-01,2026-03-31,-1.00,USD,Acme Ltd,\n",
    )
    .await
    .unwrap()
    .complete();
    let line = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM recon.statement_lines WHERE status = 'unreviewed'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let doc = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM recon.documents WHERE kind = 'purchase_bill'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    (account, LineId::from_uuid(line), DocumentId::from_uuid(doc))
}

/// The document's stored amount with the bank's sign — a -1.00 payment's counterpart.
async fn doc_amount(db: &TestDb, doc: DocumentId) -> i64 {
    sqlx::query_scalar("SELECT amount_minor FROM recon.documents WHERE id = $1")
        .bind(doc.as_uuid())
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

fn t1_match(line: LineId, doc: DocumentId, doc_amount: i64) -> BalancedMatch {
    BalancedMatch::new(
        vec![LineAllocation {
            line,
            amount: usd(-100),
        }],
        vec![DocumentAllocation {
            document: doc,
            amount: usd(doc_amount),
        }],
        vec![],
    )
    .unwrap()
}

fn score(exact_reference: bool) -> ScoreBreakdown {
    ScoreBreakdown::new(true, exact_reference, true, 8, true).unwrap()
}

/// The storage-level outcome a T1 commit produces for line `seq`.
fn decided_t1(seq: i64, line: LineId, doc: DocumentId, doc_amount: i64) -> DecidedLine {
    DecidedLine {
        seq,
        line,
        outcome: Decided::Matched {
            tier: Tier::T1,
            matched: t1_match(line, doc, doc_amount),
            score: score(true),
            commit: true,
        },
    }
}

async fn start_or_panic(db: &TestDb, account: &Account, lease: Duration) -> matching::JobLease {
    match matching::start(&db.pool, account.firm, account.client, lease, ENGINE)
        .await
        .unwrap()
    {
        Start::Created(l) | Start::Resumed(l) => l,
        Start::Busy { job } => panic!("expected to hold the job, it is busy with {job}"),
    }
}

// --- The schema itself --------------------------------------------------------------------------------

#[tokio::test]
async fn documents_refuse_over_allocation_for_raw_sql() {
    let db = TestDb::new().await;
    let (account, _line, doc) = line_and_document(&db).await;
    let amount = doc_amount(&db, doc).await;
    // Inside a transaction (the match row must exist but nothing commits): the trigger
    // refuses an allocation past the document's amount.
    let mut tx = db.pool.begin().await.unwrap();
    let match_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO recon.matches (firm_id, id, client_id, tier, status, currency)
         VALUES ($1, $2, $3, 't1', 'committed', 'USD')",
    )
    .bind(account.firm.as_uuid())
    .bind(match_id)
    .bind(account.client.as_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    let over = amount.signum() * (amount.abs() + 1);
    let state = sqlstate(
        sqlx::query(
            "INSERT INTO recon.match_documents
                 (firm_id, client_id, match_id, document_id, amount_minor)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(match_id)
        .bind(doc.as_uuid())
        .bind(over)
        .execute(&mut *tx)
        .await,
    );
    assert_eq!(state, "RCM03", "the trigger must refuse over-allocation");
    drop(tx);
    // And the column CHECK itself, reached past the bookkeeping guard by bumping the
    // row version as an honest writer would.
    let state = sqlstate(
        sqlx::query(
            "UPDATE recon.documents
                SET allocated_minor = amount_minor + pg_catalog.sign(amount_minor),
                    row_version = row_version + 1
              WHERE id = $1",
        )
        .bind(doc.as_uuid())
        .execute(&db.pool)
        .await,
    );
    assert_eq!(state, "23514", "the documents CHECK must refuse it too");
    db.cleanup().await;
}

#[tokio::test]
async fn a_committed_match_must_be_t1_even_for_raw_sql() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let state = sqlstate(
        sqlx::query(
            "INSERT INTO recon.matches (firm_id, id, client_id, tier, status, currency)
             VALUES ($1, $2, $3, 't2', 'committed', 'USD')",
        )
        .bind(account.firm.as_uuid())
        .bind(Uuid::now_v7())
        .bind(account.client.as_uuid())
        .execute(&db.pool)
        .await,
    );
    assert_eq!(state, "RCM01");
    db.cleanup().await;
}

#[tokio::test]
async fn two_concurrent_starts_run_exactly_one() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let (a, b) = tokio::join!(
        matching::start(&db.pool, account.firm, account.client, LEASE, ENGINE),
        matching::start(&db.pool, account.firm, account.client, LEASE, ENGINE),
    );
    let created =
        u8::from(matches!(a, Ok(Start::Created(_)))) + u8::from(matches!(b, Ok(Start::Created(_))));
    assert_eq!(created, 1, "exactly one starter wins: {a:?} vs {b:?}");
    for result in [a, b] {
        match result {
            Ok(Start::Created(_) | Start::Busy { .. }) | Err(recon_db::DbError::ClientBusy) => {}
            other => panic!("the loser is busy or refused, got {other:?}"),
        }
    }
    let running: i64 =
        sqlx::query_scalar("SELECT count(*) FROM recon.match_jobs WHERE status = 'running'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(running, 1, "the partial unique index enforces it");
    db.cleanup().await;
}

#[tokio::test]
async fn an_expired_lease_fences_the_stale_worker() {
    let db = TestDb::new().await;
    let (account, line, doc) = line_and_document(&db).await;
    let amount = doc_amount(&db, doc).await;
    let stale = start_or_panic(&db, &account, Duration::ZERO).await;
    matching::snapshot(&db.pool, &stale).await.unwrap();
    // The lease is already expired; a second starter reclaims the job.
    let fresh = match matching::start(&db.pool, account.firm, account.client, LEASE, ENGINE)
        .await
        .unwrap()
    {
        Start::Resumed(l) => l,
        other => panic!("expected the stale job to be reclaimed, got {other:?}"),
    };
    // The stale worker's commit is refused.
    let refused = matching::commit_chunk(
        &db.pool,
        &stale,
        &[decided_t1(1, line, doc, amount)],
        LEASE,
        TX_TIMEOUT,
    )
    .await;
    assert!(
        matches!(refused, Err(recon_db::DbError::LeaseLost)),
        "the stale token must lose its lease: {refused:?}"
    );
    // The fresh holder's commit lands.
    let report = matching::commit_chunk(
        &db.pool,
        &fresh,
        &[decided_t1(1, line, doc, amount)],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!((report.matched, report.skipped), (1, 0));
    db.cleanup().await;
}

#[tokio::test]
async fn the_candidate_query_uses_the_open_documents_index() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    import_documents(
        &db.pool,
        &account,
        "purchase_bill,INV-1,2026-03-01,2026-03-31,-10.00,USD,Acme Ltd,\n\
         purchase_bill,INV-2,2026-03-02,2026-04-01,-20.00,USD,Acme Ltd,\n",
    )
    .await
    .unwrap()
    .complete();
    let plan: Vec<String> = sqlx::query(
        "EXPLAIN SELECT id, amount_minor - allocated_minor
           FROM recon.documents
          WHERE firm_id = $1 AND client_id = $2 AND currency = 'USD'
            AND allocated_minor <> amount_minor
            AND issue_date BETWEEN '2026-02-01' AND '2026-04-30'",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .fetch_all(&db.pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>(0))
    .collect();
    let plan = plan.join("\n");
    assert!(
        plan.contains("documents_open_candidates"),
        "the candidate query must use the (firm, client, currency, open) index:\n{plan}"
    );
    db.cleanup().await;
}

// --- The module's job flow ----------------------------------------------------------------------------

#[tokio::test]
async fn a_chunk_commits_match_allocations_lines_decisions_and_cursor_together() {
    let db = TestDb::new().await;
    let (account, line, doc) = line_and_document(&db).await;
    let amount = doc_amount(&db, doc).await;
    let job = start_or_panic(&db, &account, LEASE).await;
    let n = matching::snapshot(&db.pool, &job).await.unwrap();
    assert_eq!(n, 1);

    let report = matching::commit_chunk(
        &db.pool,
        &job,
        &[decided_t1(1, line, doc, amount)],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(
        (
            report.matched,
            report.proposed,
            report.exceptions,
            report.skipped
        ),
        (1, 0, 0, 0)
    );

    // Line -> matched, document allocated, one match + its two allocation rows, and the
    // decisions (one for the match, one for the line) in the same transaction.
    let status: String =
        sqlx::query_scalar("SELECT status FROM recon.statement_lines WHERE id = $1")
            .bind(line.as_uuid())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "matched");
    let allocated: i64 =
        sqlx::query_scalar("SELECT allocated_minor FROM recon.documents WHERE id = $1")
            .bind(doc.as_uuid())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(allocated, amount);
    let (m, ml, md): (i64, i64, i64) = sqlx::query(
        "SELECT (SELECT count(*) FROM recon.matches),
                (SELECT count(*) FROM recon.match_lines),
                (SELECT count(*) FROM recon.match_documents)",
    )
    .fetch_one(&db.pool)
    .await
    .map(|r| (r.get(0), r.get(1), r.get(2)))
    .unwrap();
    assert_eq!((m, ml, md), (1, 1, 1));
    let decisions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM recon.decision_log WHERE job_id = $1")
            .bind(job.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(decisions, 2, "the match's and the line's decisions");
    let job_row = matching::get(&db.pool, account.firm, job.id).await.unwrap();
    assert_eq!((job_row.cursor, job_row.matched), (1, 1));

    matching::finish(&db.pool, &job, [7_u8; 32]).await.unwrap();
    let job_row = matching::get(&db.pool, account.firm, job.id).await.unwrap();
    assert_eq!(job_row.status, "complete");
    assert_eq!(
        job_row.run_digest.as_deref(),
        Some("07".repeat(32).as_str())
    );
    db.cleanup().await;
}

#[tokio::test]
async fn proposed_matches_write_a_proposal_and_the_line_goes_to_proposed() {
    let db = TestDb::new().await;
    let (account, line, doc) = line_and_document(&db).await;
    let amount = doc_amount(&db, doc).await;
    let job = start_or_panic(&db, &account, LEASE).await;
    matching::snapshot(&db.pool, &job).await.unwrap();
    let report = matching::commit_chunk(
        &db.pool,
        &job,
        &[DecidedLine {
            seq: 1,
            line,
            outcome: Decided::Matched {
                tier: Tier::T2,
                matched: t1_match(line, doc, amount),
                score: score(false),
                commit: false,
            },
        }],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!((report.proposed, report.matched), (1, 0));
    let status: String =
        sqlx::query_scalar("SELECT status FROM recon.statement_lines WHERE id = $1")
            .bind(line.as_uuid())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "proposed");
    let (actor, ver): (String, String) =
        sqlx::query("SELECT actor, engine_version FROM recon.proposals WHERE line_id = $1")
            .bind(line.as_uuid())
            .fetch_one(&db.pool)
            .await
            .map(|r| (r.get(0), r.get(1)))
            .unwrap();
    assert_eq!((actor.as_str(), ver.as_str()), ("engine", ENGINE));
    db.cleanup().await;
}

#[tokio::test]
async fn exceptions_queue_the_line_and_leave_no_match() {
    let db = TestDb::new().await;
    let (account, line, _doc) = line_and_document(&db).await;
    let job = start_or_panic(&db, &account, LEASE).await;
    matching::snapshot(&db.pool, &job).await.unwrap();
    let report = matching::commit_chunk(
        &db.pool,
        &job,
        &[DecidedLine {
            seq: 1,
            line,
            outcome: Decided::Exception {
                kind: ExceptionKind::NoCandidate,
                candidates: vec![],
            },
        }],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!((report.exceptions, report.matched), (1, 0));
    let kind: String =
        sqlx::query_scalar("SELECT kind FROM recon.line_exceptions WHERE line_id = $1")
            .bind(line.as_uuid())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(kind, "no_candidate");
    let status: String =
        sqlx::query_scalar("SELECT status FROM recon.statement_lines WHERE id = $1")
            .bind(line.as_uuid())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "exception");
    db.cleanup().await;
}

#[tokio::test]
async fn a_decided_subject_gets_no_second_decision_from_the_same_job() {
    let db = TestDb::new().await;
    let (account, line, doc) = line_and_document(&db).await;
    let amount = doc_amount(&db, doc).await;
    let job = start_or_panic(&db, &account, LEASE).await;
    matching::snapshot(&db.pool, &job).await.unwrap();
    matching::commit_chunk(
        &db.pool,
        &job,
        &[decided_t1(1, line, doc, amount)],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();
    // A duplicate decision for the same subject in the same job is refused outright —
    // this is what makes a resumed run idempotent (D-033).
    let state = sqlstate(
        sqlx::query(
            "INSERT INTO recon.decision_log
                 (firm_id, client_id, id, subject_kind, subject_id, to_state, actor_kind,
                  actor_ref, reason_code, evidence, engine_version, job_id)
             VALUES ($1, $2, uuidv7(), 'line', $3, 'matched', 'engine', 'match-v1',
                     'auto_commit', ''::bytea, 'recon test', $4)",
        )
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(line.as_uuid())
        .bind(job.id)
        .execute(&db.pool)
        .await,
    );
    assert_eq!(state, "23505", "one decision per (job, subject)");
    db.cleanup().await;
}

#[tokio::test]
async fn the_line_held_check_uses_the_match_lines_index() {
    let db = TestDb::new().await;
    let (account, line, _doc) = line_and_document(&db).await;
    // `match_lines_guard`'s per-row "the line is already held by a live match" lookup
    // must use match_lines_by_line: without it each check scans the whole table, which
    // is what gave the first 1M run a twenty-minute drive tail (0010). The index
    // carries INCLUDE (match_id) for a reason: the planner under-estimates the pkey
    // index-only scan (it costs the non-prefix line_id qual as a bound, so ~9 units
    // at any table size once pages are all-visible) — only a covering by-line index
    // is index-only-capable itself and stays the chosen plan in every vacuum state.
    // The firm still gets ~20,000 rows so real scan costs diverge decisively; the
    // fill is synthetic and suspends the row guards (the fill is plan evidence, not
    // matching's own write path).
    let (client_id, import_id, account_id): (Uuid, Uuid, Uuid) = sqlx::query_as(
        "SELECT client_id, id, account_id FROM recon.imports
          WHERE firm_id = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(account.firm.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let owner = db.owner_pool().await;
    sqlx::query("ALTER TABLE recon.matches DISABLE TRIGGER matches_have_their_decision")
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE recon.statement_lines DISABLE TRIGGER statement_lines_inserted")
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE recon.match_lines DISABLE TRIGGER match_lines_guard")
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO recon.statement_lines
           (firm_id, client_id, account_id, import_id, currency, row_no, booking_date,
            amount_minor, narration_raw, narration_norm, occurrence, fingerprint,
            tuple_hash, status)
         SELECT $1, $2, $3, $4, 'USD', 900000 + g, date '2026-01-01' + (g % 28),
                -1000 - g, 'SCALEFILL ' || g, 'scalefill ' || g, 1,
                decode(md5($2::text || ':' || g) || md5('fp' || g), 'hex'),
                decode(md5('th' || g) || md5(g::text || $2::text), 'hex'), 'unreviewed'
           FROM generate_series(1, 20000) g",
    )
    .bind(account.firm.as_uuid())
    .bind(client_id)
    .bind(account_id)
    .bind(import_id)
    .execute(&owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recon.matches (firm_id, client_id, tier, status, currency)
         SELECT $1, $2, 't1', 'proposed', 'USD' FROM generate_series(1, 20000) g",
    )
    .bind(account.firm.as_uuid())
    .bind(client_id)
    .execute(&owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recon.match_lines (firm_id, client_id, match_id, line_id, amount_minor)
         SELECT m.firm_id, m.client_id, m.id, l.id, -1000
           FROM (SELECT firm_id, client_id, id, row_number() OVER (ORDER BY id) rn
                   FROM recon.matches WHERE firm_id = $1) m
           JOIN (SELECT id, row_number() OVER (ORDER BY id) rn FROM recon.statement_lines
                  WHERE firm_id = $1 AND row_no >= 900000) l ON l.rn = m.rn",
    )
    .bind(account.firm.as_uuid())
    .execute(&owner)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE recon.matches ENABLE TRIGGER matches_have_their_decision")
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE recon.statement_lines ENABLE TRIGGER statement_lines_inserted")
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE recon.match_lines ENABLE TRIGGER match_lines_guard")
        .execute(&owner)
        .await
        .unwrap();
    owner.close().await;
    // VACUUM ANALYZE, not just ANALYZE: all-visible pages are the dangerous state —
    // that is when the pkey's index-only scan gets its optimistic ~9-unit estimate.
    sqlx::query("VACUUM ANALYZE recon.match_lines")
        .execute(&db.pool)
        .await
        .unwrap();
    let plan: Vec<String> = sqlx::query(
        "EXPLAIN SELECT 1 FROM recon.match_lines ml
                  WHERE ml.firm_id = $1 AND ml.line_id = $2 AND ml.match_id <> $3",
    )
    .bind(account.firm.as_uuid())
    .bind(line.as_uuid())
    .bind(Uuid::now_v7())
    .fetch_all(&db.pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>(0))
    .collect();
    let plan = plan.join("\n");
    assert!(
        plan.contains("match_lines_by_line"),
        "the line-held check must use the (firm, line) index:\n{plan}"
    );
    db.cleanup().await;
}
