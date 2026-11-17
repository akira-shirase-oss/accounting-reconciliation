//! Shared harness for the server tests and drills: the real `recon-server` binary as a
//! child process, ports inside the project's range, a TCP proxy that can cut the server
//! off from PostgreSQL, HTTP helpers, and the log scanner (D-025).
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    reason = "test harness: a panic is the failure report"
)]

use std::future::Future;
use std::net::TcpListener as StdListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// The operator token every test server uses (never a real secret; scanned for in logs).
pub const TOKEN: &str = "test-operator-token-0123456789abcdef-ZXQ";

static NEXT_PORT: AtomicU16 = AtomicU16::new(0);

/// A free port in 62400–62479 (D-036): tests never take ports other projects use.
pub fn free_port() -> u16 {
    let offset = u16::try_from(std::process::id() % 80).unwrap_or(0);
    for _ in 0..400 {
        let n = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        let port = 62_400 + (offset + n) % 80;
        if StdListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no free port in 62400-62479");
}

/// A running server process.
pub struct Server {
    pub child: Child,
    pub base: String,
    pub log: PathBuf,
}

impl Server {
    /// Starts the binary against `database_url` with extra environment, and waits until
    /// it is ready. The returned future borrows nothing, so it can run in a spawned task.
    pub fn start(
        database_url: &str,
        extra: &[(&str, &str)],
    ) -> impl Future<Output = Self> + Send + 'static {
        let server = Self::spawn(database_url, extra);
        async move {
            server.wait_ready(Duration::from_secs(60)).await;
            server
        }
    }

    /// Starts the binary without waiting.
    pub fn spawn(database_url: &str, extra: &[(&str, &str)]) -> Self {
        let port = free_port();
        let log =
            std::env::temp_dir().join(format!("recon-server-{}-{port}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_recon-server"));
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("RECON_DATABASE_URL", database_url)
            .env("RECON_LISTEN", format!("127.0.0.1:{port}"))
            .env("RECON_OPERATOR_TOKEN", TOKEN)
            .env("RECON_LOG", "info")
            .env("RECON_DB_ACQUIRE_MS", "1000")
            // stdout and stderr both: a println! leak must not escape the scan (D-025).
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .kill_on_drop(true);
        for (k, v) in extra {
            command.env(k, v);
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            log,
        }
    }

    pub async fn wait_ready(&self, within: Duration) {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Ok(r) = http().get(format!("{}/readyz", self.base)).send().await
                && r.status() == 200
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "the server never became ready; log: {}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }

    /// `SIGKILL`.
    pub async fn kill(mut self) -> PathBuf {
        let _ = self.child.kill().await;
        self.log
    }

    /// `SIGTERM`, then wait for a clean exit.
    pub async fn stop(mut self) -> PathBuf {
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
        }
        let _ = tokio::time::timeout(Duration::from_secs(40), self.child.wait()).await;
        let _ = self.child.kill().await;
        self.log
    }

    /// Waits for the process to exit by itself; returns its code.
    pub async fn exit_code(mut self, within: Duration) -> (Option<i32>, String) {
        let status = tokio::time::timeout(within, self.child.wait()).await;
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        if let Ok(Ok(s)) = status {
            (s.code(), log)
        } else {
            let _ = self.child.kill().await;
            (None, log)
        }
    }
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .unwrap()
}

pub async fn post_json(base: &str, path: &str, body: &Value) -> (u16, Value) {
    let r = http()
        .post(format!("{base}{path}"))
        .bearer_auth(TOKEN)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// Creates a firm, a client and a USD account through the API.
pub async fn tenant(base: &str) -> (String, String, String) {
    let (s, firm) = post_json(base, "/v1/firms", &json!({"name": "Drill Firm"})).await;
    assert_eq!(s, 201, "{firm}");
    let firm = firm["firm"]["id"].as_str().unwrap().to_owned();
    let (s, client) = post_json(
        base,
        &format!("/v1/firms/{firm}/clients"),
        &json!({"name": "Drill Client"}),
    )
    .await;
    assert_eq!(s, 201, "{client}");
    let client = client["client"]["id"].as_str().unwrap().to_owned();
    let (s, account) = post_json(
        base,
        &format!("/v1/firms/{firm}/clients/{client}/accounts"),
        &json!({"name": "Current", "currency": "USD"}),
    )
    .await;
    assert_eq!(s, 201, "{account}");
    (
        firm,
        client,
        account["account"]["id"].as_str().unwrap().to_owned(),
    )
}

/// Uploads a statement; `Err` when the connection failed (a killed server).
pub async fn upload(
    base: &str,
    firm: &str,
    account: &str,
    body: Vec<u8>,
) -> Result<(u16, Value), reqwest::Error> {
    let r = http()
        .post(format!("{base}/v1/firms/{firm}/accounts/{account}/imports"))
        .bearer_auth(TOKEN)
        .header("content-type", "text/csv")
        .body(body)
        .send()
        .await?;
    let status = r.status().as_u16();
    Ok((status, r.json().await.unwrap_or(Value::Null)))
}

/// Fails if the log holds any of `forbidden` (D-025): tokens, narrations, counterparties,
/// references, amounts.
pub fn assert_log_clean(log: &std::path::Path, forbidden: &[&str]) {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    assert!(
        !text.is_empty(),
        "the server wrote no log at {}",
        log.display()
    );
    for needle in forbidden {
        assert!(
            !text.contains(needle),
            "the log at {} contains {needle:?}",
            log.display()
        );
    }
}

/// What the generator's statements contain that a log must never show.
pub const GENERATED_CONTENT: [&str; 6] = [
    TOKEN,
    "Bluefin Logistics",
    "Kestrel Freight",
    "Indigo Print",
    "INV-2026-",
    "Payment Acme",
];

/// A TCP proxy between the server and PostgreSQL that can be cut (drill `db-down`).
pub struct Proxy {
    pub port: u16,
    up: Arc<AtomicBool>,
    generation: watch::Sender<u64>,
}

impl Proxy {
    pub async fn start(upstream: String) -> Self {
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let up = Arc::new(AtomicBool::new(true));
        let (generation, _) = watch::channel(0_u64);
        let (up2, generation2) = (up.clone(), generation.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    continue;
                };
                if !up2.load(Ordering::SeqCst) {
                    drop(inbound);
                    continue;
                }
                let upstream = upstream.clone();
                let mut cut = generation2.subscribe();
                tokio::spawn(async move {
                    let Ok(mut outbound) = TcpStream::connect(&upstream).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                        _ = cut.changed() => {}
                    }
                });
            }
        });
        Self {
            port,
            up,
            generation,
        }
    }

    /// Refuses new connections and cuts every live one.
    pub fn down(&self) {
        self.up.store(false, Ordering::SeqCst);
        self.generation.send_modify(|g| *g += 1);
    }

    pub fn up(&self) {
        self.up.store(true, Ordering::SeqCst);
    }

    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}
