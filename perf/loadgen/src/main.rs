//! `recon-loadgen`: the `r0-import`, `r1-import` and `r2-match` load scenarios (ROADMAP
//! R0, R1 and R2; the unit, D-027, is one statement line taken through import — for
//! `r2-match`, one line taken through matching: snapshot, engine, committed chunks).
//!
//! `r2-match` gives each client one `gen-v3` month — statement plus documents plus the
//! truth file — uploads both, then posts the client's matching job and waits for it.
//! A line is done when the job is `complete` with a digest; the database is then checked:
//! every line in exactly one terminal state, the job's counters equal the truth's label
//! counts, zero committed matches outside tier `t1` (zero false auto-matches), no subject
//! decided twice, and the decision log replaying to the stored state.
//!
//! `r1-import` (D-067) gives each account one continuous 140-day statement cut into five
//! files whose day windows overlap by seven days, so every file after the first repeats the
//! last week of the one before byte for byte: those rows must come back as duplicates
//! naming the existing lines, never as new lines and never as a revision; identical twins
//! inside a file must both stay. Uploads that meet a busy account (`409 account_busy`) are
//! retried after `Retry-After`, and counted.
//!
//! It starts the release `recon-server` under `/usr/bin/time` (for an exact peak RSS),
//! creates one firm with `clients` clients and one account each, then `workers` concurrent
//! uploaders import `files_per_account` seeded `gen-v1` statements per account (disjoint
//! months, so no line repeats). Every tenth file is uploaded again afterwards and must be
//! answered with the earlier import. Finally it checks the database: exactly the expected
//! lines, every fingerprint unique per account, every import complete, nothing left in
//! staging, the decision log replaying to the stored state. The result, with machine,
//! commit and load average, is written as JSON; thresholds decide `pass`.
//!
//! Configured by `scripts/load.sh` through the environment (see `Plan::from_env`).

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use recon_cli::generate::{GENERATOR_VERSION, StatementSpec, statement};
use recon_domain::{Currency, Date, FirmId};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::Mutex;

struct Plan {
    scenario: String,
    mode: String,
    clients: usize,
    files_per_account: usize,
    lines_per_file: u32,
    workers: usize,
    port: u16,
    database_url: String,
    out: String,
    server_bin: String,
    machine: String,
    commit: String,
    load_before: String,
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

impl Plan {
    fn from_env() -> anyhow::Result<Self> {
        let scenario = std::env::var("SCENARIO").unwrap_or_else(|_| "r0-import".into());
        if !matches!(scenario.as_str(), "r0-import" | "r1-import" | "r2-match") {
            bail!("SCENARIO must be r0-import, r1-import or r2-match, not {scenario:?}");
        }
        let mode = std::env::var("MODE").unwrap_or_else(|_| "smoke".into());
        let (clients, files, lines) = match (scenario.as_str(), mode.as_str()) {
            ("r2-match", "full") => (200, 1, 5_000),
            ("r2-match", "smoke") => (10, 1, 1_000),
            (_, "full") => (200, 5, 1_000),
            (_, "smoke") => (20, 5, 500),
            (_, other) => bail!("MODE must be full or smoke, not {other:?}"),
        };
        // A functional check with fewer accounts (not a load result; see perf/README.md).
        let clients = std::env::var("CLIENTS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(clients);
        Ok(Self {
            scenario,
            mode,
            clients,
            files_per_account: files,
            lines_per_file: lines,
            workers: std::env::var("WORKERS")
                .ok()
                .and_then(|w| w.parse().ok())
                .unwrap_or(8),
            port: env("PORT")?.parse()?,
            database_url: env("RECON_DATABASE_URL")?,
            out: env("OUT")?,
            server_bin: env("SERVER_BIN")?,
            machine: env("MACHINE")?,
            commit: env("COMMIT")?,
            load_before: env("LOAD_BEFORE")?,
        })
    }

    fn total_lines(&self) -> u64 {
        u64::try_from(self.clients * self.files_per_account).unwrap_or(0)
            * u64::from(self.lines_per_file)
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0 * (sorted.len() as f64 - 1.0)).round();
    sorted[(rank as usize).min(sorted.len() - 1)]
}

/// Peak RSS in bytes from `/usr/bin/time` output (macOS `-l`: bytes; GNU `-v`: KiB).
fn peak_rss(time_output: &str) -> Option<u64> {
    for line in time_output.lines() {
        let line = line.trim();
        if let Some(n) = line.strip_suffix("maximum resident set size") {
            return n.trim().parse().ok();
        }
        if let Some(n) = line.strip_prefix("Maximum resident set size (kbytes):") {
            return n.trim().parse::<u64>().ok().map(|k| k * 1024);
        }
    }
    None
}

async fn wait_ready(base: &str) -> anyhow::Result<()> {
    let http = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(r) = http.get(format!("{base}/readyz")).send().await
            && r.status() == 200
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("the server never became ready")
}

async fn post(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: Value,
) -> anyhow::Result<Value> {
    let r = http
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
    let status = r.status();
    let body: Value = r.json().await?;
    if status != 201 {
        bail!("{path}: {status} {body}");
    }
    Ok(body)
}

/// `r1-import`'s day window for file `file`: 28 days each, the first seven repeating the
/// previous file's last week.
fn window(file: usize) -> (u32, u32) {
    let start = u32::try_from(file * 28).unwrap_or(0);
    (start.saturating_sub(7), start + 27)
}

fn spec(plan: &Plan, client: usize, file: usize) -> StatementSpec {
    if plan.scenario == "r1-import" {
        let seed = 0x5EED_1000_u64 ^ (u64::try_from(client).unwrap_or(0) << 8);
        let lines = plan.lines_per_file * u32::try_from(plan.files_per_account).unwrap_or(1);
        return StatementSpec {
            days: 28 * u32::try_from(plan.files_per_account).unwrap_or(1),
            twin_every: 997,
            window: Some(window(file)),
            ..StatementSpec::month(
                seed,
                lines,
                Currency::Usd,
                Date::new(2026, 1, 1).expect("invariant: a real date"),
            )
        };
    }
    let seed = 0x5EED_0000_u64
        ^ (u64::try_from(client).unwrap_or(0) << 8)
        ^ u64::try_from(file).unwrap_or(0);
    let month = u8::try_from(file + 1).unwrap_or(1);
    StatementSpec {
        days: 28,
        twin_every: 997,
        ..StatementSpec::month(
            seed,
            plan.lines_per_file,
            Currency::Usd,
            Date::new(2026, month, 1)
                .unwrap_or(Date::new(2026, 1, 1).expect("invariant: a real date")),
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plan = Arc::new(Plan::from_env()?);
    let token = format!(
        "load-{:032x}",
        u128::from(std::process::id()) * 0x9E37_79B9_7F4A_7C15_u128
    );
    let base = format!("http://127.0.0.1:{}", plan.port);
    // /usr/bin/time reports the exact peak RSS (macOS `-l`, GNU `-v`); without it the
    // server runs directly and the RSS is recorded as unknown.
    let time_flag = if cfg!(target_os = "macos") {
        "-l"
    } else {
        "-v"
    };
    let mut command = if std::path::Path::new("/usr/bin/time").exists() {
        let mut c = Command::new("/usr/bin/time");
        c.arg(time_flag).arg(&plan.server_bin);
        c
    } else {
        Command::new(&plan.server_bin)
    };
    let mut server = command
        .env("RECON_DATABASE_URL", &plan.database_url)
        .env("RECON_LISTEN", format!("127.0.0.1:{}", plan.port))
        .env("RECON_OPERATOR_TOKEN", &token)
        .env("RECON_LOG", "warn")
        .env("RECON_IMPORT_CONCURRENCY", plan.workers.to_string())
        .env("RECON_DB_MAX_CONNECTIONS", "32")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting the server under /usr/bin/time")?;
    let mut server_stderr = server.stderr.take().context("server stderr")?;
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        let _ = server_stderr.read_to_string(&mut text).await;
        text
    });
    wait_ready(&base).await?;

    let pool = recon_db::connect_lazy(&plan.database_url, &recon_db::PoolSettings::default())?;
    let db_before: i64 =
        sqlx::query_scalar("SELECT pg_catalog.pg_database_size(pg_catalog.current_database())")
            .fetch_one(&pool)
            .await?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let firm = post(
        &http,
        &base,
        &token,
        "/v1/firms",
        json!({"name": "Load firm"}),
    )
    .await?;
    let firm_id = firm["firm"]["id"].as_str().context("firm id")?.to_owned();
    // r2-match needs the client id (document imports and matching jobs are client-scoped)
    // as well as the account id (statement imports are account-scoped).
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(plan.clients);
    for c in 0..plan.clients {
        let client = post(
            &http,
            &base,
            &token,
            &format!("/v1/firms/{firm_id}/clients"),
            json!({"name": format!("Client {c}")}),
        )
        .await?;
        let client_id = client["client"]["id"]
            .as_str()
            .context("client id")?
            .to_owned();
        let account = post(
            &http,
            &base,
            &token,
            &format!("/v1/firms/{firm_id}/clients/{client_id}/accounts"),
            json!({"name": "Current", "currency": "USD"}),
        )
        .await?;
        pairs.push((
            client_id,
            account["account"]["id"]
                .as_str()
                .context("account id")?
                .to_owned(),
        ));
    }
    let pairs = Arc::new(pairs);

    // The jobs, interleaved across accounts so concurrent uploads hit different accounts.
    if plan.scenario == "r2-match" {
        return r2_match(
            plan,
            pairs,
            http,
            base,
            token,
            firm_id,
            pool,
            db_before,
            server,
            stderr_task,
        )
        .await;
    }
    let jobs: Vec<(usize, usize)> = (0..plan.files_per_account)
        .flat_map(|f| (0..plan.clients).map(move |c| (c, f)))
        .collect();
    let jobs = Arc::new(jobs);
    let next = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(Mutex::new(Vec::<f64>::with_capacity(jobs.len())));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let busy = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..plan.workers {
        let (plan, jobs, next, latencies, errors, pairs, busy) = (
            plan.clone(),
            jobs.clone(),
            next.clone(),
            latencies.clone(),
            errors.clone(),
            pairs.clone(),
            busy.clone(),
        );
        let (http, base, token, firm_id) =
            (http.clone(), base.clone(), token.clone(), firm_id.clone());
        workers.spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(client, file)) = jobs.get(i) else {
                    break;
                };
                let body = statement(&spec(&plan, client, file));
                let sent = Instant::now();
                let mut attempt = 0;
                let result = loop {
                    let result = http
                        .post(format!(
                            "{base}/v1/firms/{firm_id}/accounts/{}/imports",
                            pairs[client].1
                        ))
                        .bearer_auth(&token)
                        .header("content-type", "text/csv")
                        .body(body.clone())
                        .send()
                        .await;
                    match result {
                        Ok(r) if r.status() == 409 && attempt < 10 => {
                            attempt += 1;
                            busy.fetch_add(1, Ordering::Relaxed);
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                        other => break other,
                    }
                };
                let elapsed = sent.elapsed().as_secs_f64() * 1_000.0;
                match result {
                    Ok(r) if r.status() == 201 => latencies.lock().await.push(elapsed),
                    Ok(r) => {
                        let status = r.status();
                        let text = r.text().await.unwrap_or_default();
                        errors.lock().await.push(format!(
                            "{status}: {}",
                            text.chars().take(200).collect::<String>()
                        ));
                    }
                    Err(e) => errors.lock().await.push(format!("request failed: {e}")),
                }
            }
        });
    }
    while let Some(w) = workers.join_next().await {
        w?;
    }
    let import_seconds = started.elapsed().as_secs_f64();

    // Idempotency under load: every tenth file again must answer with the earlier import.
    let mut repeats_ok = 0_u64;
    let mut repeats = 0_u64;
    for (i, &(client, file)) in jobs.iter().enumerate().filter(|(i, _)| i % 10 == 0) {
        let _ = i;
        repeats += 1;
        let r = http
            .post(format!(
                "{base}/v1/firms/{firm_id}/accounts/{}/imports",
                pairs[client].1
            ))
            .bearer_auth(&token)
            .header("content-type", "text/csv")
            .body(statement(&spec(&plan, client, file)))
            .send()
            .await?;
        let status = r.status();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        if status == 200 && body["duplicate_file"] == true {
            repeats_ok += 1;
        }
    }

    // Verification against the database.
    let firm = FirmId::from_uuid(firm_id.parse()?);
    let (lines, distinct, complete, staged, staging_imports): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1),
                (SELECT count(*) FROM (SELECT DISTINCT account_id, fingerprint FROM recon.statement_lines WHERE firm_id = $1) d),
                (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'complete'),
                (SELECT count(*) FROM recon.import_staging WHERE firm_id = $1),
                (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'staging')",
    )
    .bind(firm.as_uuid())
    .fetch_one(&pool)
    .await?;
    let (rows_read, lines_created, duplicates, revisions): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT coalesce(sum(rows_read) FILTER (WHERE status = 'complete'), 0)::int8,
                coalesce(sum(lines_created), 0)::int8, coalesce(sum(duplicates), 0)::int8,
                count(*) FILTER (WHERE status = 'revision')
           FROM recon.imports WHERE firm_id = $1",
    )
    .bind(firm.as_uuid())
    .fetch_one(&pool)
    .await?;
    let replay = recon_db::replay::check(&pool, firm).await?;
    let db_after: i64 =
        sqlx::query_scalar("SELECT pg_catalog.pg_database_size(pg_catalog.current_database())")
            .fetch_one(&pool)
            .await?;
    pool.close().await;

    // Stop the server and read its peak RSS. /usr/bin/time forwards SIGTERM to its child
    // and prints the summary when it exits; pkill -P reaches the same child directly when
    // the process list is readable. If neither lands, kill the wrapper and bound every
    // wait so a stop failure can never hang the run — the RSS is then honestly unknown.
    if let Some(pid) = server.id() {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        let _ = std::process::Command::new("pkill")
            .arg("-TERM")
            .arg("-P")
            .arg(pid.to_string())
            .status();
    }
    if tokio::time::timeout(Duration::from_secs(60), server.wait())
        .await
        .is_err()
    {
        let _ = server.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(10), server.wait()).await;
    }
    let server_log = tokio::time::timeout(Duration::from_secs(10), stderr_task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let rss = peak_rss(&server_log);

    let mut latencies = latencies.lock().await.clone();
    latencies.sort_by(f64::total_cmp);
    let errors = errors.lock().await.clone();
    let expected = plan.total_lines();
    let lines_per_s = expected as f64 / import_seconds;
    let rows_per_s = rows_read as f64 / import_seconds;
    let p99 = percentile(&latencies, 99.0);
    // Full runs gate on capacity thresholds; an r1-import smoke gates on its own looser
    // ones (a shared CI runner); an r0-import smoke records them only (as in R0).
    let (lines_min, p99_max) = if plan.mode == "full" {
        (10_000.0, 3_000.0)
    } else {
        (1_000.0, 10_000.0)
    };
    let thresholds = json!({
        "lines_per_s_min": lines_min,
        "file_p99_ms_max": p99_max,
        "server_peak_rss_bytes_max": 268_435_456_u64,
        "errors_max": 0,
    });
    let mut checks = json!({
        "every_line_exactly_once": u64::try_from(lines).unwrap_or(0) == expected,
        "fingerprints_unique_per_account": distinct == lines,
        "every_file_complete": usize::try_from(complete).unwrap_or(0) == jobs.len(),
        "nothing_left_in_staging": staged == 0 && staging_imports == 0,
        "repeats_answered_by_the_earlier_import": repeats_ok == repeats,
        "decision_log_replays": replay.is_ok(),
    });
    if plan.scenario == "r1-import" {
        checks["overlaps_are_duplicates_never_lines"] = json!(
            lines_created == lines && lines_created + duplicates == rows_read && duplicates > 0
        );
        checks["no_revision_held"] = json!(revisions == 0);
    }
    let checks_pass = checks
        .as_object()
        .is_some_and(|o| o.values().all(|v| v == true));
    let thresholds_pass = errors.is_empty()
        && lines_per_s >= lines_min
        && p99 <= p99_max
        && rss.is_some_and(|r| r <= 268_435_456);
    let result = json!({
        "scenario": plan.scenario,
        "mode": plan.mode,
        "unit": "statement line imported and fingerprinted (D-027, import half); r1-import also counts overlap rows read as duplicates",
        "machine": plan.machine,
        "commit": plan.commit,
        "load_average_before": plan.load_before,
        "load_average_after": std::process::Command::new("uptime").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default(),
        "generator": GENERATOR_VERSION,
        "shape": {
            "firms": 1, "clients": plan.clients, "accounts": plan.clients,
            "files": jobs.len(), "lines_per_file": plan.lines_per_file, "lines": expected,
            "workers": plan.workers, "server_import_concurrency": plan.workers,
        },
        "results": {
            "import_seconds": (import_seconds * 1000.0).round() / 1000.0,
            "lines_per_s": lines_per_s.round(),
            "file_latency_ms": {
                "p50": percentile(&latencies, 50.0).round(),
                "p95": percentile(&latencies, 95.0).round(),
                "p99": p99.round(),
                "max": latencies.last().copied().unwrap_or(0.0).round(),
            },
            "errors": errors.len(),
            "account_busy_retries": busy.load(Ordering::Relaxed),
            "rows_read": rows_read, "rows_per_s": rows_per_s.round(),
            "lines_created": lines_created, "duplicates": duplicates, "revisions_held": revisions,
            "first_errors": errors.iter().take(5).collect::<Vec<_>>(),
            "repeats": repeats, "repeats_ok": repeats_ok,
            "lines_in_database": lines,
            "server_peak_rss_bytes": rss,
            "database_growth_bytes": db_after - db_before,
            "replay": match &replay { Ok(r) => json!({"clients": r.clients, "decisions": r.decisions, "imports": r.imports, "lines": r.lines}), Err(m) => json!(m.0) },
        },
        "checks": checks,
        "thresholds": thresholds,
        "checks_pass": checks_pass,
        "thresholds_pass": thresholds_pass,
        "pass": checks_pass && thresholds_pass,
    });
    std::fs::write(&plan.out, serde_json::to_string_pretty(&result)? + "\n")?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    // Correctness always gates. Thresholds gate a full run and an r1-import smoke (its
    // looser smoke thresholds); an r0-import smoke records them, labelled. No smoke is
    // capacity evidence (a shared CI runner or a busy laptop).
    let gate_thresholds = plan.mode == "full" || plan.scenario == "r1-import";
    if !checks_pass || (gate_thresholds && !thresholds_pass) {
        std::process::exit(1);
    }
    Ok(())
}

/// Truth-file labels → the line's expected terminal status.
fn truth_counts(truth: &[u8]) -> anyhow::Result<(u64, u64, u64)> {
    let text = std::str::from_utf8(truth).context("truth is not UTF-8")?;
    let (mut matched, mut proposed, mut exception) = (0_u64, 0_u64, 0_u64);
    for row in text.lines().skip(1) {
        let label = row.split(',').nth(1).unwrap_or("");
        match label {
            "t1" => matched += 1,
            "t2" | "t3" | "t4" => proposed += 1,
            _ => exception += 1,
        }
    }
    Ok((matched, proposed, exception))
}

/// The `r2-match` scenario: every client gets one generated `gen-v3` month (statement +
/// documents + truth), both uploads go through the API, then the client's matching job
/// runs and the database is checked line-for-line against the truth's label counts.
#[allow(clippy::too_many_arguments, reason = "the scenario's shared run state")]
async fn r2_match(
    plan: Arc<Plan>,
    pairs: Arc<Vec<(String, String)>>,
    http: reqwest::Client,
    base: String,
    token: String,
    firm_id: String,
    pool: sqlx::PgPool,
    db_before: i64,
    mut server: tokio::process::Child,
    stderr_task: tokio::task::JoinHandle<String>,
) -> anyhow::Result<()> {
    // The firm's residual tolerances, pinned into each job (D-014): bank fees to 10.00.
    sqlx::query("UPDATE recon.firms SET residual_tolerances = $1 WHERE id = $2::uuid")
        .bind(json!({"USD": {"bank_fee": 1000, "tax_withheld": 0, "rounding": 0}}))
        .bind(&firm_id)
        .execute(&pool)
        .await
        .context("setting the firm's residual tolerances")?;

    // Phase A: generate and upload each client's statement and documents.
    let distractors = plan.lines_per_file / 10;
    let next = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let truth = Arc::new(Mutex::new(Vec::<(String, u64, u64, u64)>::new()));
    let upload_started = Instant::now();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..plan.workers {
        let (plan, next, errors, truth, pairs) = (
            plan.clone(),
            next.clone(),
            errors.clone(),
            truth.clone(),
            pairs.clone(),
        );
        let (http, base, token, firm_id) =
            (http.clone(), base.clone(), token.clone(), firm_id.clone());
        workers.spawn(async move {
            loop {
                let c = next.fetch_add(1, Ordering::Relaxed);
                let Some((client, account)) = pairs.get(c) else {
                    break;
                };
                let set = recon_cli::generate::match_set(&recon_cli::generate::MatchSpec {
                    seed: 0x5EED_3000_u64 ^ (u64::try_from(c).unwrap_or(0) << 8),
                    lines: plan.lines_per_file,
                    distractors,
                    currency: Currency::Usd,
                    start: Date::new(2026, 1, 1).expect("invariant: a real date"),
                    days: 28,
                });
                for (path, body) in [
                    (
                        format!("/v1/firms/{firm_id}/accounts/{account}/imports"),
                        set.statement,
                    ),
                    (
                        format!("/v1/firms/{firm_id}/clients/{client}/document-imports"),
                        set.documents,
                    ),
                ] {
                    let r = http
                        .post(format!("{base}{path}"))
                        .bearer_auth(&token)
                        .header("content-type", "text/csv")
                        .body(body)
                        .send()
                        .await;
                    match r {
                        Ok(r) if r.status() == 201 => {}
                        Ok(r) => {
                            let status = r.status();
                            let text = r.text().await.unwrap_or_default();
                            errors.lock().await.push(format!(
                                "client {c} {path}: {status}: {}",
                                text.chars().take(200).collect::<String>()
                            ));
                            break;
                        }
                        Err(e) => {
                            errors
                                .lock()
                                .await
                                .push(format!("client {c} {path}: request failed: {e}"));
                            break;
                        }
                    }
                }
                match truth_counts(&set.truth) {
                    Ok((m, p, e)) => truth.lock().await.push((client.clone(), m, p, e)),
                    Err(e) => errors.lock().await.push(format!("client {c} truth: {e}")),
                }
            }
        });
    }
    while let Some(w) = workers.join_next().await {
        w?;
    }
    let upload_seconds = upload_started.elapsed().as_secs_f64();
    let truth = truth.lock().await.clone();

    // Phase B: one matching job per client; the server's matching_concurrency bounds how
    // many run at once — every client gets its own job (mutual exclusion is per client).
    let job_path = |client: &str| format!("/v1/firms/{firm_id}/clients/{client}/matching-jobs");
    let match_started = Instant::now();
    let mut pending: Vec<(String, String, Instant)> = Vec::with_capacity(truth.len());
    for (client, ..) in &truth {
        // A 5xx start ("database_unavailable", "nothing was accepted") is retried like a
        // busy upload; a 409 client_busy means a job already runs — impossible here.
        let mut attempt = 0;
        let r = loop {
            let r = http
                .post(format!("{base}{}", job_path(client)))
                .bearer_auth(&token)
                .header("content-length", "0")
                .send()
                .await;
            match &r {
                Ok(r) if r.status().is_server_error() && attempt < 10 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                _ => break r,
            }
        };
        match r {
            Ok(r) if r.status() == 202 => {
                let body: Value = r.json().await.unwrap_or(Value::Null);
                let id = body["job"]["id"].as_str().unwrap_or_default().to_owned();
                pending.push((client.clone(), id, Instant::now()));
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                errors.lock().await.push(format!(
                    "job start {client}: {status}: {}",
                    text.chars().take(200).collect::<String>()
                ));
            }
            Err(e) => errors
                .lock()
                .await
                .push(format!("job start {client}: request failed: {e}")),
        }
    }
    // Poll until every job is terminal or the overall deadline passes. The bound: every
    // client's job gets the server's matching deadline; a stuck run is an error, not a
    // hang (drill: drill_matching_deadline_exceeded).
    let poll_deadline = Instant::now()
        + Duration::from_secs(
            (u64::try_from(plan.clients).unwrap_or(1) * 90
                / u64::try_from(plan.workers.max(1)).unwrap_or(1))
            .max(300),
        );
    let mut latencies = Vec::<f64>::with_capacity(pending.len());
    let mut jobs_failed = 0_u64;
    while !pending.is_empty() && Instant::now() < poll_deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut still = Vec::with_capacity(pending.len());
        for (client, id, sent) in pending {
            let r = http
                .get(format!("{base}{}/{id}", job_path(&client)))
                .bearer_auth(&token)
                .send()
                .await;
            match r {
                Ok(r) if r.status() == 200 => {
                    let body: Value = r.json().await.unwrap_or(Value::Null);
                    match body["job"]["status"].as_str().unwrap_or("") {
                        "complete" => latencies.push(sent.elapsed().as_secs_f64() * 1_000.0),
                        "failed" => {
                            jobs_failed += 1;
                            errors.lock().await.push(format!(
                                "job {id} failed: {}",
                                body["job"]["error_code"].as_str().unwrap_or("?")
                            ));
                        }
                        _ => still.push((client, id, sent)),
                    }
                }
                Ok(r) => errors
                    .lock()
                    .await
                    .push(format!("job poll {id}: {}", r.status())),
                Err(e) => errors
                    .lock()
                    .await
                    .push(format!("job poll {id}: request failed: {e}")),
            }
        }
        pending = still;
    }
    for (_, id, _) in &pending {
        errors
            .lock()
            .await
            .push(format!("job {id} never reached a terminal state"));
    }
    let match_seconds = match_started.elapsed().as_secs_f64();

    // Verification against the database: terminal line states per client equal the
    // truth's label counts; committed matches are all t1; no subject decided twice.
    let firm = FirmId::from_uuid(firm_id.parse()?);
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT client_id::text, status::text, count(*)::int8 FROM recon.statement_lines
           WHERE firm_id = $1 GROUP BY 1, 2",
    )
    .bind(firm.as_uuid())
    .fetch_all(&pool)
    .await?;
    let mut status_by_client: std::collections::HashMap<
        String,
        std::collections::HashMap<String, i64>,
    > = std::collections::HashMap::new();
    for (client, status, n) in rows {
        *status_by_client
            .entry(client)
            .or_default()
            .entry(status)
            .or_default() += n;
    }
    let mut states_ok = true;
    for (client, matched, proposed, exception) in &truth {
        let counts = status_by_client.get(client).cloned().unwrap_or_default();
        let get = |s: &str| counts.get(s).copied().unwrap_or(0);
        if get("matched") != i64::try_from(*matched).unwrap_or(-1)
            || get("proposed") != i64::try_from(*proposed).unwrap_or(-1)
            || get("exception") != i64::try_from(*exception).unwrap_or(-1)
            || counts
                .keys()
                .any(|s| !matches!(s.as_str(), "matched" | "proposed" | "exception"))
        {
            states_ok = false;
            errors.lock().await.push(format!(
                "client {client}: states {counts:?} != truth ({matched}, {proposed}, {exception})"
            ));
        }
    }
    let (committed_t1, committed_other, proposals, exceptions, jobs_complete, jobs_no_digest, dup): (
        i64, i64, i64, i64, i64, i64, i64,
    ) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM recon.matches WHERE firm_id = $1 AND status = 'committed' AND tier = 't1'),
                (SELECT count(*) FROM recon.matches WHERE firm_id = $1 AND status = 'committed' AND tier <> 't1'),
                (SELECT count(*) FROM recon.proposals WHERE firm_id = $1),
                (SELECT count(*) FROM recon.line_exceptions WHERE firm_id = $1),
                (SELECT count(*) FROM recon.match_jobs WHERE firm_id = $1 AND status = 'complete'),
                (SELECT count(*) FROM recon.match_jobs WHERE firm_id = $1 AND status = 'complete' AND run_digest IS NULL),
                (SELECT count(*) FROM (SELECT subject_id FROM recon.decision_log
                   WHERE firm_id = $1 AND job_id IS NOT NULL
                   GROUP BY job_id, subject_id HAVING count(*) > 1) d)",
    )
    .bind(firm.as_uuid())
    .fetch_one(&pool)
    .await?;
    let replay = recon_db::replay::check(&pool, firm).await?;
    let db_after: i64 =
        sqlx::query_scalar("SELECT pg_catalog.pg_database_size(pg_catalog.current_database())")
            .fetch_one(&pool)
            .await?;
    pool.close().await;

    if let Some(pid) = server.id() {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        let _ = std::process::Command::new("pkill")
            .arg("-TERM")
            .arg("-P")
            .arg(pid.to_string())
            .status();
    }
    if tokio::time::timeout(Duration::from_secs(60), server.wait())
        .await
        .is_err()
    {
        let _ = server.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(10), server.wait()).await;
    }
    let server_log = tokio::time::timeout(Duration::from_secs(10), stderr_task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let rss = peak_rss(&server_log);

    latencies.sort_by(f64::total_cmp);
    let errors = errors.lock().await.clone();
    let expected = plan.total_lines();
    let (m_total, p_total, e_total): (u64, u64, u64) = truth
        .iter()
        .fold((0, 0, 0), |(m, p, e), (_, a, b, c)| (m + a, p + b, e + c));
    // Throughput counts the lines actually decided, not the ones intended.
    let decided = committed_t1
        .saturating_add(proposals)
        .saturating_add(exceptions);
    let lines_per_s = decided as f64 / match_seconds;
    let p99 = percentile(&latencies, 99.0);
    let (lines_min, p99_max) = if plan.mode == "full" {
        (2_000.0, 120_000.0)
    } else {
        (300.0, 60_000.0)
    };
    let thresholds = json!({
        "lines_per_s_min": lines_min,
        "job_p99_ms_max": p99_max,
        "server_peak_rss_bytes_max": 536_870_912_u64,
        "errors_max": 0,
    });
    let checks = json!({
        "every_job_complete_with_digest": usize::try_from(jobs_complete).unwrap_or(0) == truth.len() && jobs_no_digest == 0 && jobs_failed == 0 && pending.is_empty(),
        "line_states_equal_truth_per_client": states_ok,
        "zero_false_auto_matches": committed_other == 0,
        "proposals_equal_truth": u64::try_from(proposals).unwrap_or(0) == p_total,
        "exceptions_equal_truth": u64::try_from(exceptions).unwrap_or(0) == e_total,
        "no_subject_decided_twice": dup == 0,
        "decision_log_replays": replay.is_ok(),
    });
    let checks_pass = checks
        .as_object()
        .is_some_and(|o| o.values().all(|v| v == true));
    let thresholds_pass = errors.is_empty()
        && lines_per_s >= lines_min
        && p99 <= p99_max
        && rss.is_some_and(|r| r <= 536_870_912);
    let result = json!({
        "scenario": plan.scenario,
        "mode": plan.mode,
        "unit": "statement line decided by a matching job (D-027, matching half): snapshot, engine, committed chunks",
        "machine": plan.machine,
        "commit": plan.commit,
        "load_average_before": plan.load_before,
        "load_average_after": std::process::Command::new("uptime").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default(),
        "generator": recon_cli::generate::MATCH_GENERATOR_VERSION,
        "shape": {
            "firms": 1, "clients": plan.clients,
            "lines_per_client": plan.lines_per_file, "distractors_per_client": distractors,
            "lines": expected, "workers": plan.workers,
        },
        "results": {
            "upload_seconds": (upload_seconds * 1000.0).round() / 1000.0,
            "match_seconds": (match_seconds * 1000.0).round() / 1000.0,
            "lines_per_s": lines_per_s.round(),
            "job_latency_ms": {
                "p50": percentile(&latencies, 50.0).round(),
                "p95": percentile(&latencies, 95.0).round(),
                "p99": p99.round(),
                "max": latencies.last().copied().unwrap_or(0.0).round(),
            },
            "jobs_completed": latencies.len(), "jobs_failed": jobs_failed,
            "errors": errors.len(),
            "first_errors": errors.iter().take(5).collect::<Vec<_>>(),
            "truth": { "t1_matched": m_total, "proposed": p_total, "exception": e_total },
            "database": {
                "committed_t1": committed_t1, "committed_other_tiers": committed_other,
                "proposals": proposals, "line_exceptions": exceptions,
                "jobs_complete": jobs_complete, "jobs_without_digest": jobs_no_digest,
                "duplicate_decisions": dup,
            },
            "server_peak_rss_bytes": rss,
            "database_growth_bytes": db_after - db_before,
            "replay": match &replay { Ok(r) => json!({"clients": r.clients, "decisions": r.decisions, "imports": r.imports, "lines": r.lines}), Err(m) => json!(m.0) },
        },
        "checks": checks,
        "thresholds": thresholds,
        "checks_pass": checks_pass,
        "thresholds_pass": thresholds_pass,
        "pass": checks_pass && thresholds_pass,
    });
    std::fs::write(&plan.out, serde_json::to_string_pretty(&result)? + "\n")?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
