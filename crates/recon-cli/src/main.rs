//! `recon`: operator and verification commands.
//!
//! ```text
//! recon db setup [--database NAME]   roles and database (RECON_ADMIN_DATABASE_URL,
//!                                    RECON_APP_PASSWORD)
//! recon db migrate [--database NAME] migrations, as the administrator acting as the
//!                                    NOLOGIN owner (RECON_ADMIN_DATABASE_URL)
//! recon audit anchor FIRM --out FILE append every client's signed, dated head to FILE
//!                                    (RECON_DATABASE_URL, RECON_ANCHOR_KEY_FILE)
//! recon audit verify FIRM [--anchors FILE]
//!                                    verify every client's chain, and against the anchors
//! recon audit replay FIRM            replay the log against the stored state
//! recon gen statement --seed N --lines N [--currency USD] [--start YYYY-MM-DD] [--days N]
//!                                    a seeded statement on stdout
//! ```

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::{Context as _, bail};
use recon_cli::generate::{self, StatementSpec};
use std::str::FromStr as _;

use recon_db::roles;
use recon_domain::anchor::Anchor;
use recon_domain::{ClientId, Currency, Date, FirmId};
use sqlx::Connection as _;
use sqlx::postgres::PgConnectOptions;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i.saturating_add(1)))
        .cloned()
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    let words: Vec<&str> = args.iter().map(String::as_str).take(2).collect();
    match words.as_slice() {
        ["db", "setup"] => setup(args).await,
        ["db", "migrate"] => migrate(args).await,
        ["audit", "verify"] => verify(args).await,
        ["audit", "anchor"] => anchor(args).await,
        ["audit", "replay"] => replay(args).await,
        ["gen", "statement"] => gen_statement(args),
        _ => bail!(
            "usage: recon db setup|db migrate|audit anchor FIRM --out FILE|audit verify FIRM [--anchors FILE]|audit replay FIRM|gen statement"
        ),
    }
}

async fn setup(args: &[String]) -> anyhow::Result<ExitCode> {
    let name = flag(args, "--database").unwrap_or_else(|| "recon".to_owned());
    let mut admin = sqlx::PgConnection::connect(&env("RECON_ADMIN_DATABASE_URL")?)
        .await
        .context("connecting as the administrator")?;
    roles::ensure_roles(&mut admin, &env("RECON_APP_PASSWORD")?)
        .await
        .context("creating the roles")?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
    )
    .bind(&name)
    .fetch_one(&mut admin)
    .await?;
    if exists {
        roles::lock_down_database(&mut admin, &name).await?;
    } else {
        roles::create_database(&mut admin, &name)
            .await
            .context("creating the database")?;
    }
    println!(
        "setup ok: roles {}, {}, {}; database {name}",
        roles::OWNER,
        roles::RUNTIME,
        roles::APP
    );
    Ok(ExitCode::SUCCESS)
}

async fn migrate(args: &[String]) -> anyhow::Result<ExitCode> {
    let name = flag(args, "--database").unwrap_or_else(|| "recon".to_owned());
    let options = PgConnectOptions::from_str(&env("RECON_ADMIN_DATABASE_URL")?)
        .context("RECON_ADMIN_DATABASE_URL does not parse")?
        .database(&name);
    recon_db::migrate(options)
        .await
        .context("migrating as recon_owner")?;
    println!(
        "migrate ok: database {name}, schema version {:?}",
        roles::expected_version()
    );
    Ok(ExitCode::SUCCESS)
}

/// The anchor key: at least 32 bytes from a file nobody else can read (D-049).
fn anchor_key() -> anyhow::Result<Vec<u8>> {
    let path = env("RECON_ANCHOR_KEY_FILE")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path)
            .with_context(|| format!("reading {path}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!("{path} must not be readable by group or others (chmod 600)");
        }
    }
    let mut key = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    while key.last().is_some_and(u8::is_ascii_whitespace) {
        key.pop();
    }
    if key.len() < recon_domain::anchor::MIN_KEY_BYTES {
        bail!(
            "the anchor key must be at least {} bytes",
            recon_domain::anchor::MIN_KEY_BYTES
        );
    }
    Ok(key)
}

async fn anchor(args: &[String]) -> anyhow::Result<ExitCode> {
    let firm = firm_arg(args)?;
    let out = flag(args, "--out").context("--out FILE is required")?;
    let key = anchor_key()?;
    let pool = recon_db::connect_lazy(
        &env("RECON_DATABASE_URL")?,
        &recon_db::PoolSettings::default(),
    )?;
    let mut lines = String::new();
    for (client, head, at) in recon_db::decisions::heads(&pool, firm).await? {
        let signed = Anchor::sign(&key, firm, client, head.seq, head.hash, at)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        lines.push_str(&serde_json::to_string(&signed)?);
        lines.push('\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out)
        .with_context(|| format!("opening {out}"))?;
    std::io::Write::write_all(&mut file, lines.as_bytes())?;
    file.sync_all()?;
    println!(
        "anchor ok: {} heads appended to {out}",
        lines.lines().count()
    );
    Ok(ExitCode::SUCCESS)
}

/// Anchored `(seq, head)` pairs per client.
type AnchorsByClient = std::collections::BTreeMap<ClientId, Vec<(i64, [u8; 32])>>;

/// The anchors in `path` for `firm`, signatures checked, grouped by client.
fn read_anchors(path: &str, firm: FirmId) -> anyhow::Result<AnchorsByClient> {
    let key = anchor_key()?;
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let mut by_client = AnchorsByClient::new();
    for (n, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let anchor: Anchor = serde_json::from_str(line)
            .with_context(|| format!("{path}:{}: not an anchor", n.saturating_add(1)))?;
        let (seq, head) = anchor
            .verify(&key)
            .map_err(|e| anyhow::anyhow!("{path}:{}: {e}", n.saturating_add(1)))?;
        if anchor.firm == firm.as_uuid() {
            by_client
                .entry(ClientId::from_uuid(anchor.client))
                .or_default()
                .push((seq, head));
        }
    }
    Ok(by_client)
}

fn firm_arg(args: &[String]) -> anyhow::Result<FirmId> {
    let raw = args.get(2).context("a firm id is required")?;
    Ok(FirmId::from_uuid(
        raw.parse().context("the firm id is not a UUID")?,
    ))
}

async fn verify(args: &[String]) -> anyhow::Result<ExitCode> {
    let firm = firm_arg(args)?;
    let anchors = match flag(args, "--anchors") {
        Some(path) => match read_anchors(&path, firm) {
            Ok(a) => a,
            Err(e) => {
                println!("anchors BROKEN: {e:#}");
                return Ok(ExitCode::from(3));
            }
        },
        None => std::collections::BTreeMap::new(),
    };
    let pool = recon_db::connect_lazy(
        &env("RECON_DATABASE_URL")?,
        &recon_db::PoolSettings::default(),
    )?;
    let clients = recon_db::decisions::clients(&pool, firm).await?;
    let mut broken = false;
    for client in anchors.keys().filter(|c| !clients.contains(c)) {
        println!("client {client}: BROKEN, anchored but absent from the database");
        broken = true;
    }
    for client in clients {
        let anchored = anchors.get(&client).map_or(&[][..], Vec::as_slice);
        match recon_db::decisions::verify_chain_paged(&pool, firm, client, 5_000, anchored).await? {
            Ok(head) => println!(
                "client {client}: ok, {} decisions, head {}, {} anchors checked",
                head.seq,
                hex::encode(head.hash),
                anchored.len()
            ),
            Err(b) => {
                println!("client {client}: BROKEN at seq {} ({:?})", b.seq, b.kind);
                broken = true;
            }
        }
    }
    Ok(if broken {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    })
}

async fn replay(args: &[String]) -> anyhow::Result<ExitCode> {
    let firm = firm_arg(args)?;
    let pool = recon_db::connect_lazy(
        &env("RECON_DATABASE_URL")?,
        &recon_db::PoolSettings::default(),
    )?;
    match recon_db::replay::check(&pool, firm).await? {
        Ok(r) => {
            println!(
                "replay ok: {} clients, {} decisions, {} imports ({} complete), {} lines",
                r.clients, r.decisions, r.imports, r.complete_imports, r.lines
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(m) => {
            println!("{m}");
            Ok(ExitCode::from(3))
        }
    }
}

fn gen_statement(args: &[String]) -> anyhow::Result<ExitCode> {
    let seed: u64 = flag(args, "--seed")
        .context("--seed is required")?
        .parse()?;
    let lines: u32 = flag(args, "--lines")
        .context("--lines is required")?
        .parse()?;
    let currency = Currency::from_code(&flag(args, "--currency").unwrap_or_else(|| "USD".into()))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let start = Date::parse_iso(&flag(args, "--start").unwrap_or_else(|| "2026-01-01".into()))
        .map_err(|e| anyhow::anyhow!("--start: {e}"))?;
    let days: u32 = flag(args, "--days").map_or(Ok(28), |d| d.parse())?;
    let spec = StatementSpec {
        days,
        ..StatementSpec::month(seed, lines, currency, start)
    };
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    generate::write_statement(&spec, &mut out)?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}
