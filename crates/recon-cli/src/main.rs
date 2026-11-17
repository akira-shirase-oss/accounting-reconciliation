//! `recon`: operator and verification commands.
//!
//! ```text
//! recon db setup [--database NAME]   roles and database (RECON_ADMIN_DATABASE_URL,
//!                                    RECON_OWNER_PASSWORD, RECON_APP_PASSWORD)
//! recon db migrate                   migrations as the owner (RECON_MIGRATE_DATABASE_URL)
//! recon audit verify FIRM            verify every client's chain (RECON_DATABASE_URL)
//! recon audit replay FIRM            replay the log against the stored state
//! recon gen statement --seed N --lines N [--currency USD] [--start YYYY-MM-DD] [--days N]
//!                                    a seeded statement on stdout
//! ```

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::{Context as _, bail};
use recon_cli::generate::{self, StatementSpec};
use recon_db::roles;
use recon_domain::{Currency, Date, FirmId};
use sqlx::Connection as _;

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
        ["db", "migrate"] => migrate().await,
        ["audit", "verify"] => verify(args).await,
        ["audit", "replay"] => replay(args).await,
        ["gen", "statement"] => gen_statement(args),
        _ => bail!(
            "usage: recon db setup|db migrate|audit verify FIRM|audit replay FIRM|gen statement"
        ),
    }
}

async fn setup(args: &[String]) -> anyhow::Result<ExitCode> {
    let name = flag(args, "--database").unwrap_or_else(|| "recon".to_owned());
    let mut admin = sqlx::PgConnection::connect(&env("RECON_ADMIN_DATABASE_URL")?)
        .await
        .context("connecting as the administrator")?;
    roles::ensure_roles(
        &mut admin,
        &env("RECON_OWNER_PASSWORD")?,
        &env("RECON_APP_PASSWORD")?,
    )
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

async fn migrate() -> anyhow::Result<ExitCode> {
    let mut owner = sqlx::PgConnection::connect(&env("RECON_MIGRATE_DATABASE_URL")?)
        .await
        .context("connecting as the owner")?;
    recon_db::migrator()
        .run(&mut owner)
        .await
        .context("migrating")?;
    println!("migrate ok: schema version {:?}", roles::expected_version());
    Ok(ExitCode::SUCCESS)
}

fn firm_arg(args: &[String]) -> anyhow::Result<FirmId> {
    let raw = args.get(2).context("a firm id is required")?;
    Ok(FirmId::from_uuid(
        raw.parse().context("the firm id is not a UUID")?,
    ))
}

async fn verify(args: &[String]) -> anyhow::Result<ExitCode> {
    let firm = firm_arg(args)?;
    let pool = recon_db::connect_lazy(
        &env("RECON_DATABASE_URL")?,
        &recon_db::PoolSettings::default(),
    )?;
    let mut broken = false;
    for client in recon_db::decisions::clients(&pool, firm).await? {
        match recon_db::decisions::verify_chain(&pool, firm, client).await? {
            Ok(head) => println!(
                "client {client}: ok, {} decisions, head {}",
                head.seq,
                hex::encode(head.hash)
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
