//! `recon-loadgen`: the `r0-import` load scenario (ROADMAP R0; the unit, D-027, is one
//! statement line taken through import).
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
        let mode = std::env::var("MODE").unwrap_or_else(|_| "smoke".into());
        let (clients, files, lines) = match mode.as_str() {
            "full" => (200, 5, 1_000),
            "smoke" => (20, 5, 500),
            other => bail!("MODE must be full or smoke, not {other:?}"),
        };
        Ok(Self {
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

fn spec(plan: &Plan, client: usize, file: usize) -> StatementSpec {
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
    let mut accounts = Vec::with_capacity(plan.clients);
    for c in 0..plan.clients {
        let client = post(
            &http,
            &base,
            &token,
            &format!("/v1/firms/{firm_id}/clients"),
            json!({"name": format!("Client {c}")}),
        )
        .await?;
        let client_id = client["client"]["id"].as_str().context("client id")?;
        let account = post(
            &http,
            &base,
            &token,
            &format!("/v1/firms/{firm_id}/clients/{client_id}/accounts"),
            json!({"name": "Current", "currency": "USD"}),
        )
        .await?;
        accounts.push(
            account["account"]["id"]
                .as_str()
                .context("account id")?
                .to_owned(),
        );
    }
    let accounts = Arc::new(accounts);

    // The jobs, interleaved across accounts so concurrent uploads hit different accounts.
    let jobs: Vec<(usize, usize)> = (0..plan.files_per_account)
        .flat_map(|f| (0..plan.clients).map(move |c| (c, f)))
        .collect();
    let jobs = Arc::new(jobs);
    let next = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(Mutex::new(Vec::<f64>::with_capacity(jobs.len())));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let started = Instant::now();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..plan.workers {
        let (plan, jobs, next, latencies, errors, accounts) = (
            plan.clone(),
            jobs.clone(),
            next.clone(),
            latencies.clone(),
            errors.clone(),
            accounts.clone(),
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
                let result = http
                    .post(format!(
                        "{base}/v1/firms/{firm_id}/accounts/{}/imports",
                        accounts[client]
                    ))
                    .bearer_auth(&token)
                    .header("content-type", "text/csv")
                    .body(body)
                    .send()
                    .await;
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
                accounts[client]
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
    let replay = recon_db::replay::check(&pool, firm).await?;
    let db_after: i64 =
        sqlx::query_scalar("SELECT pg_catalog.pg_database_size(pg_catalog.current_database())")
            .fetch_one(&pool)
            .await?;
    pool.close().await;

    // Stop the server and read its peak RSS.
    if let Some(pid) = server.id() {
        // /usr/bin/time forwards SIGTERM to its child and prints the summary when it exits.
        let _ = std::process::Command::new("pkill")
            .arg("-TERM")
            .arg("-P")
            .arg(pid.to_string())
            .status();
    }
    let _ = tokio::time::timeout(Duration::from_secs(60), server.wait()).await;
    let server_log = stderr_task.await.unwrap_or_default();
    let rss = peak_rss(&server_log);

    let mut latencies = latencies.lock().await.clone();
    latencies.sort_by(f64::total_cmp);
    let errors = errors.lock().await.clone();
    let expected = plan.total_lines();
    let lines_per_s = expected as f64 / import_seconds;
    let p99 = percentile(&latencies, 99.0);
    let thresholds = json!({
        "lines_per_s_min": 10_000,
        "file_p99_ms_max": 3_000,
        "server_peak_rss_bytes_max": 268_435_456_u64,
        "errors_max": 0,
    });
    let checks = json!({
        "every_line_exactly_once": u64::try_from(lines).unwrap_or(0) == expected,
        "fingerprints_unique_per_account": distinct == lines,
        "every_file_complete": usize::try_from(complete).unwrap_or(0) == jobs.len(),
        "nothing_left_in_staging": staged == 0 && staging_imports == 0,
        "repeats_answered_by_the_earlier_import": repeats_ok == repeats,
        "decision_log_replays": replay.is_ok(),
    });
    let checks_pass = checks
        .as_object()
        .is_some_and(|o| o.values().all(|v| v == true));
    let thresholds_pass = errors.is_empty()
        && lines_per_s >= 10_000.0
        && p99 <= 3_000.0
        && rss.is_some_and(|r| r <= 268_435_456);
    let result = json!({
        "scenario": "r0-import",
        "mode": plan.mode,
        "unit": "statement line imported and fingerprinted (D-027, import half)",
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
    // Correctness always gates. Thresholds gate a full run; a smoke records them, labelled,
    // but is not capacity evidence (a shared CI runner or a busy laptop).
    if !checks_pass || (plan.mode == "full" && !thresholds_pass) {
        std::process::exit(1);
    }
    Ok(())
}
