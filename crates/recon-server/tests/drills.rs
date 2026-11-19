//! R0's drills, against the real `recon-server` binary (built with `fault-injection`):
//!
//! * `kill-mid-import` (failure case 4, import half): a 200,000-row statement is imported
//!   and the server is `SIGKILL`ed at 20 seeded points — after staged batches, before the
//!   flip, inside the flip's transaction, after its commit. Each import must be complete
//!   or invisible; a re-run must complete exactly once with lines identical to an
//!   uninterrupted import.
//! * `kill-mid-batch` (failure case 4, matching half): a `gen-v3` month is imported and
//!   its matching job `SIGKILL`ed at seeded points — after the snapshot, after chunk
//!   commits, before and after the job's `complete`. A fresh server's sweeper resumes
//!   the job: every line ends in exactly one state, no subject sees a second decision,
//!   and the outcomes are identical to an uninterrupted run.
//! * `db-down`: PostgreSQL is cut off (a proxy drops every connection): readiness goes red
//!   within 2 s, uploads are answered `503` and never accepted then lost, an upload cut off
//!   mid-way publishes nothing, and after recovery every file imports exactly once.
//!
//! After each, the decision log is replayed against the stored state and every server log
//! is scanned for the fixture's content (D-025).
//!
//! Run with `cargo test -p recon-server --features fault-injection --test drills`.
//! `RECON_DRILL_ROWS`, `RECON_DRILL_POINTS` and `RECON_DRILL_PARALLEL` shrink a local run;
//! CI runs the defaults (200,000 rows, 20 points).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "drills: a panic is the failure report"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    GENERATED_CONTENT, Proxy, Server, TOKEN, assert_log_clean, http, post_json, tenant, upload,
};
use recon_cli::generate::{MatchSpec, Rng, StatementSpec, match_set, statement};
use recon_db::testing::{APP_PASSWORD, TestDb};
use recon_db::{imports, replay, tenancy};
use recon_domain::{AccountId, ClientId, Currency, Date, FirmId};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

const SEED: u64 = 20_261_116;

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The same firm, client and account ids in every drill database, so fingerprints (which
/// include the account) are byte-identical across runs.
const FIRM: FirmId = FirmId::from_uuid(Uuid::from_u128(0x0a11_0000_0000_0000_0000_0000_0000_0001));
const CLIENT: ClientId =
    ClientId::from_uuid(Uuid::from_u128(0x0a11_0000_0000_0000_0000_0000_0000_0002));
const ACCOUNT: AccountId =
    AccountId::from_uuid(Uuid::from_u128(0x0a11_0000_0000_0000_0000_0000_0000_0003));

async fn fixed_tenant(db: &TestDb) {
    tenancy::create_firm(&db.pool, FIRM, "Drill firm")
        .await
        .unwrap();
    tenancy::create_client(&db.pool, FIRM, CLIENT, "Drill client")
        .await
        .unwrap();
    tenancy::create_account(&db.pool, FIRM, CLIENT, ACCOUNT, "Current", Currency::Usd)
        .await
        .unwrap();
}

/// The seeded kill points: distinct staged batches, then every point of the flip.
fn kill_points(batches: u32, count: usize) -> Vec<String> {
    let mut rng = Rng::new(SEED);
    let mut pool: Vec<u32> = (1..=batches).collect();
    let fixed = [
        "before_flip",
        "flip_lines_inserted",
        "flip_before_commit",
        "after_commit",
    ];
    let wanted = count.saturating_sub(fixed.len()).min(pool.len());
    for i in 0..wanted {
        let j = i + usize::try_from(rng.below(u64::try_from(pool.len() - i).unwrap())).unwrap();
        pool.swap(i, j);
    }
    let mut chosen: Vec<u32> = pool[..wanted].to_vec();
    chosen.sort_unstable();
    let mut points: Vec<String> = chosen
        .into_iter()
        .map(|b| format!("staged_batch:{b}"))
        .collect();
    points.extend(fixed.iter().map(|p| (*p).to_owned()));
    points.truncate(count);
    points
}

async fn counts(db: &TestDb) -> (i64, i64, i64, i64) {
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1),
                (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'complete'),
                (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'staging'),
                (SELECT count(*) FROM recon.import_staging WHERE firm_id = $1)",
    )
    .bind(FIRM.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    row
}

/// Waits for the sweeper to discard abandoned staging imports and their rows.
async fn wait_swept(db: &TestDb) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, _, staging, staged) = counts(db).await;
        if staging == 0 && staged == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the sweeper left {staging} staging imports and {staged} rows"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const DRILL_ENV: [(&str, &str); 3] = [
    ("RECON_IMPORT_LEASE_MS", "5000"),
    ("RECON_SWEEP_INTERVAL_MS", "200"),
    ("RECON_DB_MAX_CONNECTIONS", "8"),
];

/// The kill drill's servers: `DRILL_ENV` plus a 5 s window to acquire a database connection.
/// The harness's 1 s suits `db-down` (a prompt 503 is what it tests) but, on a machine at
/// load average 30, made a kill-drill re-run fail with a false `database_unavailable`.
const KILL_ENV: [(&str, &str); 4] = [
    ("RECON_IMPORT_LEASE_MS", "5000"),
    ("RECON_SWEEP_INTERVAL_MS", "200"),
    ("RECON_DB_MAX_CONNECTIONS", "8"),
    ("RECON_DB_ACQUIRE_MS", "5000"),
];

fn firm_path() -> (String, String) {
    (FIRM.to_string(), ACCOUNT.to_string())
}

/// An uninterrupted import of the drill file: the digest every re-run must equal.
async fn reference_digest(file: &Arc<Vec<u8>>, rows: i64) -> String {
    let db = TestDb::new().await;
    fixed_tenant(&db).await;
    let server = Server::start(&db.app_url(), &KILL_ENV).await;
    let (firm, account) = firm_path();
    let (status, body) = upload(&server.base, &firm, &account, file.to_vec())
        .await
        .unwrap();
    assert_eq!(status, 201, "{body}");
    let (n, digest) = imports::account_lines_digest(&db.pool, FIRM, ACCOUNT)
        .await
        .unwrap();
    assert_eq!(n, rows);
    replay::check(&db.pool, FIRM)
        .await
        .unwrap()
        .expect("the reference run replays");
    let log = server.stop().await;
    assert_log_clean(&log, &GENERATED_CONTENT);
    db.cleanup().await;
    digest
}

async fn kill_at(point: String, file: Arc<Vec<u8>>, rows: i64, reference: Arc<String>) {
    let db = TestDb::new().await;
    fixed_tenant(&db).await;
    let marker = std::env::temp_dir().join(format!(
        "recon-drill-{}-{}.marker",
        std::process::id(),
        Uuid::now_v7()
    ));
    let mut env = KILL_ENV.to_vec();
    let marker_text = marker.display().to_string();
    env.push(("RECON_FAULT", point.as_str()));
    env.push(("RECON_FAULT_MARKER", marker_text.as_str()));
    let server = Server::start(&db.app_url(), &env).await;
    let (firm, account) = firm_path();
    let base = server.base.clone();
    let body = file.to_vec();
    let in_flight = tokio::spawn(async move { upload(&base, &firm, &account, body).await });
    let deadline = Instant::now() + Duration::from_secs(240);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "{point}: the fault point was never reached"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let killed_log = server.kill().await;
    let answered = in_flight.await.unwrap();
    assert!(
        !matches!(answered, Ok((200 | 201, _))),
        "{point}: a killed server must not have answered success: {answered:?}"
    );

    // Complete or invisible: never a partial import.
    let (lines, complete, _, _) = counts(&db).await;
    if point == "after_commit" {
        assert_eq!(
            (lines, complete),
            (rows, 1),
            "{point}: the commit happened before the kill"
        );
    } else {
        assert_eq!(
            (lines, complete),
            (0, 0),
            "{point}: nothing may be visible before the commit"
        );
    }

    // A fresh process: the re-run completes exactly once.
    let server = Server::start(&db.app_url(), &KILL_ENV).await;
    let (firm, account) = firm_path();
    let (status, body) = upload(&server.base, &firm, &account, file.to_vec())
        .await
        .unwrap();
    if point == "after_commit" {
        assert_eq!(
            (status, body["duplicate_file"].as_bool()),
            (200, Some(true)),
            "{point}: {body}"
        );
    } else {
        assert_eq!(status, 201, "{point}: {body}");
    }
    wait_swept(&db).await;
    let (lines, complete, staging, staged) = counts(&db).await;
    assert_eq!(
        (lines, complete, staging, staged),
        (rows, 1, 0, 0),
        "{point}"
    );
    let (_, digest) = imports::account_lines_digest(&db.pool, FIRM, ACCOUNT)
        .await
        .unwrap();
    assert_eq!(
        &digest,
        reference.as_ref(),
        "{point}: the re-run's lines differ from an uninterrupted import"
    );
    let report = replay::check(&db.pool, FIRM)
        .await
        .unwrap()
        .unwrap_or_else(|m| panic!("{point}: {m}"));
    assert_eq!(report.lines, rows);
    let log = server.stop().await;
    assert_log_clean(&killed_log, &GENERATED_CONTENT);
    assert_log_clean(&log, &GENERATED_CONTENT);
    let _ = std::fs::remove_file(&marker);
    db.cleanup().await;
    eprintln!("kill-mid-import: {point} ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drill_kill_mid_import() {
    let rows = u32::try_from(env_or("RECON_DRILL_ROWS", 200_000)).unwrap();
    let points = kill_points(rows.div_ceil(5_000), env_or("RECON_DRILL_POINTS", 20));
    let parallel = env_or("RECON_DRILL_PARALLEL", 4).max(1);
    let file = Arc::new(statement(&StatementSpec::month(
        SEED,
        rows,
        Currency::Usd,
        Date::new(2026, 3, 1).unwrap(),
    )));
    let started = Instant::now();
    let reference = Arc::new(reference_digest(&file, i64::from(rows)).await);
    eprintln!(
        "kill-mid-import: {rows} rows, {} points {points:?}, reference {reference}",
        points.len()
    );
    let permits = Arc::new(Semaphore::new(parallel));
    let mut runs = tokio::task::JoinSet::new();
    for point in points {
        let (file, reference, permits) = (file.clone(), reference.clone(), permits.clone());
        runs.spawn(async move {
            let _permit = permits.acquire_owned().await.unwrap();
            kill_at(point, file, i64::from(rows), reference).await;
        });
    }
    while let Some(run) = runs.join_next().await {
        if let Err(e) = run {
            std::panic::resume_unwind(e.into_panic());
        }
    }
    eprintln!(
        "kill-mid-import: all points passed in {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drill_db_down() {
    let db = TestDb::new().await;
    let proxy = Proxy::start(db.host()).await;
    let url = db.url_as(recon_db::roles::APP, APP_PASSWORD, Some(&proxy.addr()));
    let server = Server::start(&url, &DRILL_ENV).await;
    let base = server.base.clone();
    let (firm, _, account) = tenant(&base).await;
    let file = |seed: u64, lines: u32, month: u8| {
        statement(&StatementSpec::month(
            seed,
            lines,
            Currency::Usd,
            Date::new(2026, month, 1).unwrap(),
        ))
    };
    let (file_a, file_b, file_c, file_d) = (
        file(1, 2_000, 1),
        file(2, 3_000, 2),
        file(3, 4_000, 3),
        file(4, 50_000, 4),
    );
    let (status, first) = upload(&base, &firm, &account, file_a.clone())
        .await
        .unwrap();
    assert_eq!(status, 201, "{first}");

    // The database goes away: readiness is red within 2 s, liveness stays green.
    let cut = Instant::now();
    proxy.down();
    loop {
        let ready = http().get(format!("{base}/readyz")).send().await.unwrap();
        if ready.status() == 503 {
            break;
        }
        assert!(
            cut.elapsed() < Duration::from_secs(2),
            "readiness still green after {:?}",
            cut.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let red_after = cut.elapsed();
    assert!(
        red_after <= Duration::from_secs(2),
        "readiness went red after {red_after:?}"
    );
    assert_eq!(
        http()
            .get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    for file in [&file_b, &file_c] {
        let asked = Instant::now();
        let (status, body) = upload(&base, &firm, &account, file.clone()).await.unwrap();
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (503, Some("database_unavailable")),
            "{body}"
        );
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "a 503 must be prompt: {:?}",
            asked.elapsed()
        );
    }

    // Back, then cut again while a large upload is half staged.
    proxy.up();
    server.wait_ready(Duration::from_secs(10)).await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(4);
    let request = http()
        .post(format!("{base}/v1/firms/{firm}/accounts/{account}/imports"))
        .bearer_auth(TOKEN)
        .header("content-type", "text/csv")
        .body(reqwest::Body::wrap_stream(ReceiverStream::new(rx)))
        .send();
    let in_flight = tokio::spawn(request);
    let chunks: Vec<Vec<u8>> = file_d.chunks(64 * 1024).map(<[u8]>::to_vec).collect();
    let (head, tail) = chunks.split_at(chunks.len() / 2);
    for chunk in head {
        tx.send(Ok(chunk.clone())).await.unwrap();
    }
    let staged_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let staged: i64 = sqlx::query_scalar("SELECT coalesce(max(staged_rows), 0)::int8 FROM recon.imports WHERE status = 'staging'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        if staged >= 5_000 {
            break;
        }
        assert!(
            Instant::now() < staged_deadline,
            "the large upload never staged a batch"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    proxy.down();
    for chunk in tail {
        if tx.send(Ok(chunk.clone())).await.is_err() {
            break;
        }
    }
    drop(tx);
    match in_flight.await.unwrap() {
        Ok(response) => {
            let status = response.status().as_u16();
            assert_eq!(
                status, 503,
                "an upload cut off from the database must be refused"
            );
        }
        Err(e) => {
            eprintln!("db-down: the cut-off upload ended without a response ({e}); checked below");
        }
    }
    proxy.up();
    let back = Instant::now();
    server.wait_ready(Duration::from_secs(10)).await;
    let recovered_after = back.elapsed();
    // Nothing of the cut-off upload, nor of B and C, was published.
    let (_, complete, _, _) = {
        let firm_id = FirmId::from_uuid(firm.parse().unwrap());
        let row: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT 0::int8, (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'complete'), 0::int8, 0::int8",
        )
        .bind(firm_id.as_uuid())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        row
    };
    assert_eq!(
        complete, 1,
        "only the file answered 201 before the outage is complete"
    );

    // Every refused file now imports exactly once; the first one is recognized.
    for file in [&file_b, &file_c, &file_d] {
        let (status, body) = upload(&base, &firm, &account, file.clone()).await.unwrap();
        assert_eq!(status, 201, "{body}");
    }
    let (status, body) = upload(&base, &firm, &account, file_a.clone())
        .await
        .unwrap();
    assert_eq!(
        (status, body["duplicate_file"].as_bool()),
        (200, Some(true)),
        "{body}"
    );
    let firm_id = FirmId::from_uuid(firm.parse().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (staging, staged): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'staging'),
                    (SELECT count(*) FROM recon.import_staging WHERE firm_id = $1)",
        )
        .bind(firm_id.as_uuid())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if staging == 0 && staged == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "abandoned staging imports were not swept"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (lines, complete): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1),
                (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'complete')",
    )
    .bind(firm_id.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((lines, complete), (2_000 + 3_000 + 4_000 + 50_000, 4));
    replay::check(&db.pool, firm_id)
        .await
        .unwrap()
        .expect("the log replays after the outage");
    let log = server.stop().await;
    assert_log_clean(&log, &GENERATED_CONTENT);
    eprintln!(
        "db-down: readiness red after {red_after:?}, ready again {recovered_after:?} after recovery"
    );
    db.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// kill-mid-batch: the matching half of failure case 4.

/// The matching drill's servers: like `KILL_ENV`, plus a 1 s job lease so the dead
/// worker's job is claimable almost at once.
const MATCH_ENV: [(&str, &str); 5] = [
    ("RECON_IMPORT_LEASE_MS", "5000"),
    ("RECON_SWEEP_INTERVAL_MS", "200"),
    ("RECON_DB_MAX_CONNECTIONS", "8"),
    ("RECON_DB_ACQUIRE_MS", "5000"),
    ("RECON_MATCHING_LEASE_MS", "1000"),
];

/// The seeded kill points: distinct chunk commits, then every lifecycle edge.
fn match_kill_points(chunks: u32, count: usize) -> Vec<String> {
    let mut rng = Rng::new(SEED);
    let mut pool: Vec<u32> = (1..=chunks).collect();
    let fixed = ["match_snapshot", "match_before_finish", "match_finished"];
    let wanted = count.saturating_sub(fixed.len()).min(pool.len());
    for i in 0..wanted {
        let j = i + usize::try_from(rng.below(u64::try_from(pool.len() - i).unwrap())).unwrap();
        pool.swap(i, j);
    }
    let mut chosen: Vec<u32> = pool[..wanted].to_vec();
    chosen.sort_unstable();
    let mut points: Vec<String> = chosen
        .into_iter()
        .map(|b| format!("match_chunk:{b}"))
        .collect();
    points.extend(fixed.iter().map(|p| (*p).to_owned()));
    points.truncate(count);
    points
}

/// Uploads the documents file through its own route.
async fn upload_documents(base: &str, firm: &str, client: &str, body: Vec<u8>) {
    let r = http()
        .post(format!(
            "{base}/v1/firms/{firm}/clients/{client}/document-imports"
        ))
        .bearer_auth(TOKEN)
        .header("content-type", "text/csv")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text: Value = r.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 201, "{text}");
}

/// Posts a matching job and answers its id.
async fn post_job(base: &str, firm: &str, client: &str) -> String {
    let (status, body) = post_json(
        base,
        &format!("/v1/firms/{firm}/clients/{client}/matching-jobs"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    body["job"]["id"].as_str().unwrap().to_owned()
}

/// Polls the job row until it leaves `running` or the deadline passes.
async fn wait_job(db: &TestDb, job: Uuid, within: Duration) -> String {
    let deadline = Instant::now() + within;
    loop {
        let status: String = sqlx::query_scalar(
            "SELECT status FROM recon.match_jobs WHERE firm_id = $1 AND id = $2",
        )
        .bind(FIRM.as_uuid())
        .bind(job)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if status != "running" || Instant::now() >= deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every line's outcome as `(status, match/proposal tier, exception kind)`, ordered by
/// its CSV row: stable across databases because `row_no` is the file's own order.
async fn outcomes(db: &TestDb) -> Vec<(i32, String, String, String)> {
    sqlx::query_as(
        "SELECT l.row_no, l.status::text,
                coalesce(m.tier, pm.tier, '')::text,
                coalesce(e.kind, '')::text
           FROM recon.statement_lines l
           LEFT JOIN recon.match_lines ml
                  ON ml.firm_id = l.firm_id AND ml.line_id = l.id
           LEFT JOIN recon.matches m
                  ON m.firm_id = l.firm_id AND m.id = ml.match_id
           LEFT JOIN recon.proposals p
                  ON p.firm_id = l.firm_id AND p.line_id = l.id
           LEFT JOIN recon.matches pm
                  ON pm.firm_id = l.firm_id AND pm.id = p.match_id
           LEFT JOIN recon.line_exceptions e
                  ON e.firm_id = l.firm_id AND e.line_id = l.id
          WHERE l.firm_id = $1 AND l.client_id = $2
          ORDER BY l.row_no",
    )
    .bind(FIRM.as_uuid())
    .bind(CLIENT.as_uuid())
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

/// No subject may carry two decisions of one job (resumed or not).
async fn duplicate_decisions(db: &TestDb) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT job_id, subject_id FROM recon.decision_log
              WHERE firm_id = $1 AND job_id IS NOT NULL
              GROUP BY job_id, subject_id HAVING count(*) > 1) d",
    )
    .bind(FIRM.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

/// Imports the set, runs the job to completion, answers the outcome rows every
/// re-run must equal.
async fn reference_outcomes(
    set: &recon_cli::generate::MatchSet,
) -> Vec<(i32, String, String, String)> {
    let db = TestDb::new().await;
    fixed_tenant(&db).await;
    let server = Server::start(&db.app_url(), &MATCH_ENV).await;
    let (firm, account) = firm_path();
    let client = CLIENT.to_string();
    let (status, body) = upload(&server.base, &firm, &account, set.statement.clone())
        .await
        .unwrap();
    assert_eq!(status, 201, "{body}");
    upload_documents(&server.base, &firm, &client, set.documents.clone()).await;
    let job = post_job(&server.base, &firm, &client).await;
    assert_eq!(
        wait_job(&db, Uuid::parse_str(&job).unwrap(), Duration::from_mins(15)).await,
        "complete"
    );
    let out = outcomes(&db).await;
    replay::check(&db.pool, FIRM)
        .await
        .unwrap()
        .expect("the reference matching run replays");
    let log = server.stop().await;
    assert_log_clean(&log, &GENERATED_CONTENT);
    db.cleanup().await;
    out
}

async fn kill_match_at(
    point: String,
    set: Arc<recon_cli::generate::MatchSet>,
    reference: Arc<Vec<(i32, String, String, String)>>,
) {
    let db = TestDb::new().await;
    fixed_tenant(&db).await;
    let marker = std::env::temp_dir().join(format!(
        "recon-match-drill-{}-{}.marker",
        std::process::id(),
        Uuid::now_v7()
    ));
    let mut env = MATCH_ENV.to_vec();
    let marker_text = marker.display().to_string();
    env.push(("RECON_FAULT", point.as_str()));
    env.push(("RECON_FAULT_MARKER", marker_text.as_str()));
    let server = Server::start(&db.app_url(), &env).await;
    let (firm, account) = firm_path();
    let client = CLIENT.to_string();
    let (status, body) = upload(&server.base, &firm, &account, set.statement.clone())
        .await
        .unwrap();
    assert_eq!(status, 201, "{body}");
    upload_documents(&server.base, &firm, &client, set.documents.clone()).await;
    let job = post_job(&server.base, &firm, &client).await;
    let deadline = Instant::now() + Duration::from_secs(600);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "{point}: the fault point was never reached"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let killed_log = server.kill().await;

    // Whatever the kill caught, the state left behind must be whole: the chain replays
    // to it, every line holds exactly one state, no subject carries two decisions.
    replay::check(&db.pool, FIRM)
        .await
        .unwrap()
        .unwrap_or_else(|m| panic!("{point}: the log does not replay after the kill: {m}"));
    assert_eq!(
        duplicate_decisions(&db).await,
        0,
        "{point}: a subject carries two decisions"
    );
    let expected_status = if point == "match_finished" {
        "complete"
    } else {
        "running"
    };
    assert_eq!(
        wait_job(&db, Uuid::parse_str(&job).unwrap(), Duration::from_secs(5)).await,
        expected_status,
        "{point}: the killed job's status"
    );

    // A fresh process: the sweeper claims the dead worker's lease and resumes from the
    // cursor — never re-deciding a committed line.
    let server = Server::start(&db.app_url(), &MATCH_ENV).await;
    assert_eq!(
        wait_job(&db, Uuid::parse_str(&job).unwrap(), Duration::from_mins(15)).await,
        "complete",
        "{point}: the resumed job did not finish"
    );
    let out = outcomes(&db).await;
    assert_eq!(
        &out,
        reference.as_ref(),
        "{point}: the resumed run's outcomes differ from an uninterrupted run"
    );
    assert_eq!(
        duplicate_decisions(&db).await,
        0,
        "{point}: the resume wrote a second decision for a subject"
    );
    replay::check(&db.pool, FIRM)
        .await
        .unwrap()
        .unwrap_or_else(|m| panic!("{point}: {m}"));
    let log = server.stop().await;
    assert_log_clean(&killed_log, &GENERATED_CONTENT);
    assert_log_clean(&log, &GENERATED_CONTENT);
    let _ = std::fs::remove_file(&marker);
    db.cleanup().await;
    eprintln!("kill-mid-batch: {point} ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drill_kill_mid_batch() {
    // The engine scans its client's documents per line, so the drill's single client
    // keeps to a month-sized set: enough for twenty chunk commits and every lifecycle
    // edge, fast enough that the heartbeat, not the lease window, carries the compute.
    let rows = u32::try_from(env_or("RECON_DRILL_ROWS", 10_000)).unwrap();
    let points = match_kill_points(rows.div_ceil(500), env_or("RECON_DRILL_POINTS", 20));
    let parallel = env_or("RECON_DRILL_PARALLEL", 4).max(1);
    let set = Arc::new(match_set(&MatchSpec {
        seed: SEED,
        lines: rows,
        distractors: rows / 10,
        currency: Currency::Usd,
        start: Date::new(2026, 3, 1).unwrap(),
        days: 28,
    }));
    let started = Instant::now();
    let reference = Arc::new(reference_outcomes(&set).await);
    eprintln!(
        "kill-mid-batch: {rows} lines, {} points {points:?}, {} reference outcomes",
        points.len(),
        reference.len()
    );
    let permits = Arc::new(Semaphore::new(parallel));
    let mut runs = tokio::task::JoinSet::new();
    for point in points {
        let (set, reference, permits) = (set.clone(), reference.clone(), permits.clone());
        runs.spawn(async move {
            let _permit = permits.acquire_owned().await.unwrap();
            kill_match_at(point, set, reference).await;
        });
    }
    while let Some(run) = runs.join_next().await {
        if let Err(e) = run {
            std::panic::resume_unwind(e.into_panic());
        }
    }
    eprintln!(
        "kill-mid-batch: all points passed in {:?}",
        started.elapsed()
    );
}
