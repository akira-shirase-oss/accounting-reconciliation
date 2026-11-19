//! `recon`: the command line for accountants and operators. `recon help` lists every
//! command and `recon help COMMAND` explains one, with examples; the texts are in [`HELP`].

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

/// Every command's help, by its two words (`""` for the overview).
const HELP: &[(&str, &str)] = &[
    (
        "",
        r"recon: the command line for Accounting Reconciliation.

Before you upload (no server or database needed):
  recon import check FILE --profile PROFILE [--currency CODE] [--json]
                     Read a statement or document file exactly as the server would and
                     say what it would import, or, for every refused line, where it is,
                     which column, what is wrong and how to fix it.
  recon profile check PROFILE.json [--json]
                     Check a profile you wrote: every problem at once, or how it reads.

Operating the server:
  recon db setup [--database NAME]      create the roles and the database
  recon db migrate [--database NAME]    apply the migrations
  recon db drop --database NAME         drop a scratch database (recon_load_*, recon_test_*)
  recon audit verify FIRM [--anchors FILE] [--allow-empty]
                                        verify every client's decision chain
  recon audit anchor FIRM --out FILE    sign and append every verified chain head
  recon audit replay FIRM               replay the decision log against the stored state
  recon gen statement --seed N --lines N [--currency CODE] [--start YYYY-MM-DD] [--days N]
                                        a seeded test statement on stdout

Run `recon help COMMAND` for details and examples, for example `recon help import check`.
The examples/ folder of the repository has profiles and files to try them on.

Exit status: 0 success; 1 the command could not run (usage, a missing file, the
database); 3 a check found problems (a refused file, an unusable profile, a broken
chain); 4 nothing to verify.
",
    ),
    (
        "import check",
        r#"recon import check FILE --profile PROFILE [--currency CODE] [--json]

Reads FILE exactly as the server would on upload, with the same profile and the same
rules, without uploading it. Nothing is stored and nothing is sent anywhere.

  --profile PROFILE  a built-in profile (r0-generic-csv, ofx, r1-generic-documents) or a
                     profile file: the JSON you POST to /v1/firms/{firm}/profiles,
                     {"name": ..., "kind": "statement_csv" | "documents_csv",
                     "definition": {...}}
  --currency CODE    the account's currency, for a statement (INR, USD, EUR, GBP, AED,
                     SGD, AUD, CAD, JPY, KWD); documents carry their own
  --json             print the result as JSON (the same report the server returns)

If the file would import, it prints the rows, the dates and the money in and out per
currency, exactly. If not, it prints every refused line (the first 1,000): the line, the
profile's column, what is wrong and how to fix it. A file is imported whole or not at
all, so fix every line and check again. Rows already imported (duplicates) and changes to
lines already imported (revisions) can only be found by the server, on upload.

Examples:
  recon import check examples/statements/hdfc-march-2026.csv       --profile examples/profiles/hdfc-savings.json --currency INR
  recon import check examples/statements/hdfc-march-2026-with-errors.csv       --profile examples/profiles/hdfc-savings.json --currency INR
  recon import check examples/documents/sales-register-march-2026.csv       --profile examples/profiles/sales-register.json

Exit status: 0 the file would import; 3 it would be refused; 1 the check could not run.
"#,
    ),
    (
        "profile check",
        r"recon profile check PROFILE.json [--json]

Checks a profile file (the JSON you POST to /v1/firms/{firm}/profiles) and lists every
problem at once, or shows how it will read files: the header it expects, its dates and
amounts in words, and the SHA-256 recorded on every import read with it.

Example:
  recon profile check examples/profiles/hdfc-savings.json

Exit status: 0 the profile can be used; 3 it cannot; 1 the file could not be read.
",
    ),
    (
        "db setup",
        r"recon db setup [--database NAME]

Creates the roles (recon_owner, which cannot log in; recon_runtime; recon_app, the
server's login) and the database (default recon), owned by recon_owner with PUBLIC locked
out. Run once per database as the cluster administrator.

Environment: RECON_ADMIN_DATABASE_URL (the administrator), RECON_APP_PASSWORD (the
password the server logs in with; sent only as a SCRAM verifier).
",
    ),
    (
        "db migrate",
        r"recon db migrate [--database NAME]

Applies the migrations as the administrator acting as recon_owner. Safe to run again;
run it before starting a new release of the server (which refuses to start on an older
or newer schema).

Environment: RECON_ADMIN_DATABASE_URL.
",
    ),
    (
        "db drop",
        r"recon db drop --database NAME

Drops a scratch database (its name must start with recon_load_ or recon_test_ — the
load and test harnesses' own names), forcing out anything still connected. A named
database like the development or production one is dropped by hand, never here.

Environment: RECON_ADMIN_DATABASE_URL.
",
    ),
    (
        "audit verify",
        r"recon audit verify FIRM [--anchors FILE] [--allow-empty]

Verifies every client's decision chain of FIRM record by record, and, with --anchors,
against the heads signed into FILE by `recon audit anchor`: it names the first broken link.
A firm with no chains fails unless --allow-empty.

Environment: RECON_DATABASE_URL; with --anchors, RECON_ANCHOR_KEY_FILE.
Exit status: 0 every chain verifies; 3 one is broken; 4 nothing to verify.
",
    ),
    (
        "audit anchor",
        r"recon audit anchor FIRM --out FILE

Verifies every chain of FIRM (inside the database and against FILE), then appends each
verified head, signed with HMAC-SHA256 and dated by the database clock, to FILE. Keep FILE
where neither the server nor the database owner can write (another host, write-once
storage), and anchor after every period close and before every backup.

Environment: RECON_DATABASE_URL, RECON_ANCHOR_KEY_FILE (at least 32 bytes, mode 0600).
Exit status: 0 anchored; 3 a chain did not verify and nothing was written.
",
    ),
    (
        "audit replay",
        r"recon audit replay FIRM

Replays FIRM's decision log against its stored state: imports, lines, documents, held
revisions. Run after a restore or an incident.

Environment: RECON_DATABASE_URL. Exit status: 0 equal; 3 a mismatch, named.
",
    ),
    (
        "gen statement",
        r"recon gen statement --seed N --lines N [--currency CODE] [--start YYYY-MM-DD] [--days N]

Writes a seeded statement in the r0-generic-csv layout to stdout (gen-v2): the same seed
always writes the same bytes. For tests, drills and load runs; never real data.
",
    ),
];

fn help(topic: &str) -> ExitCode {
    if let Some((_, text)) = HELP.iter().find(|(t, _)| *t == topic) {
        print!("{text}");
        return ExitCode::SUCCESS;
    }
    eprintln!("no such command: {topic}\n");
    eprint!("{}", HELP.first().map_or("", |(_, t)| *t));
    ExitCode::FAILURE
}

async fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let wants_help = words.iter().any(|w| matches!(*w, "--help" | "-h"));
    match words.as_slice() {
        [] | ["help"] | ["--help" | "-h", ..] => return Ok(help("")),
        ["help", topic @ ..] => return Ok(help(&topic.join(" "))),
        [a, b, ..] if wants_help => return Ok(help(&format!("{a} {b}"))),
        _ => {}
    }
    match words.get(..2).unwrap_or(&words) {
        ["db", "setup"] => setup(args).await,
        ["db", "migrate"] => migrate(args).await,
        ["db", "drop"] => drop_database(args).await,
        ["audit", "verify"] => verify(args).await,
        ["audit", "anchor"] => anchor(args).await,
        ["audit", "replay"] => replay(args).await,
        ["gen", "statement"] => gen_statement(args),
        ["import", "check"] => import_check(args),
        ["profile", "check"] => profile_check(args),
        _ => {
            eprint!("{}", HELP.first().map_or("", |(_, t)| *t));
            bail!("unknown command: {}", words.join(" "))
        }
    }
}

fn print_outcome(outcome: &recon_cli::check::Outcome, json: bool) -> anyhow::Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome.json)?);
    } else {
        print!("{}", outcome.text);
    }
    Ok(if outcome.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

fn import_check(args: &[String]) -> anyhow::Result<ExitCode> {
    let file = args.get(2).filter(|a| !a.starts_with("--")).context(
        "which file? usage: recon import check FILE --profile PROFILE [--currency CODE]",
    )?;
    let spec = flag(args, "--profile").context(
        "--profile is required: a built-in (r0-generic-csv, ofx, r1-generic-documents) or a profile file",
    )?;
    let currency = flag(args, "--currency")
        .map(|c| {
            Currency::from_code(&c)
                .map_err(|_| anyhow::anyhow!("--currency {c:?} is not a supported ISO code"))
        })
        .transpose()?;
    let loaded = match recon_cli::check::load_profile(&spec)? {
        Ok(l) => l,
        Err(problems) => {
            println!("{spec}: this profile cannot be used:");
            for p in problems {
                println!("  - {p}");
            }
            return Ok(ExitCode::from(3));
        }
    };
    let outcome = recon_cli::check::check_file(file, &loaded, currency)?;
    print_outcome(&outcome, args.iter().any(|a| a == "--json"))
}

fn profile_check(args: &[String]) -> anyhow::Result<ExitCode> {
    let file = args
        .get(2)
        .filter(|a| !a.starts_with("--"))
        .context("which profile? usage: recon profile check PROFILE.json")?;
    let outcome = recon_cli::check::check_profile(file)?;
    print_outcome(&outcome, args.iter().any(|a| a == "--json"))
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

async fn drop_database(args: &[String]) -> anyhow::Result<ExitCode> {
    let name = flag(args, "--database").unwrap_or_default();
    if !(name.starts_with("recon_load_") || name.starts_with("recon_test_")) {
        bail!(
            "refusing to drop {name:?}: `recon db drop` drops only the harnesses' \
             scratch databases (recon_load_*, recon_test_*); a named database is \
             dropped by hand"
        );
    }
    let mut admin = sqlx::PgConnection::connect(&env("RECON_ADMIN_DATABASE_URL")?)
        .await
        .context("connecting as the administrator")?;
    roles::drop_database(&mut admin, &name)
        .await
        .context("dropping the database")?;
    println!("drop ok: database {name}");
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
    // Only verified heads are signed: every chain must verify inside the database and
    // against the anchors already in the file, or nothing is written (D-059).
    let previous = if std::path::Path::new(&out).exists() {
        match read_anchors(&out, firm) {
            Ok(a) => a,
            Err(e) => {
                println!("anchors BROKEN: {e:#}; nothing anchored");
                return Ok(ExitCode::from(3));
            }
        }
    } else {
        AnchorsByClient::new()
    };
    let pool = recon_db::connect_lazy(
        &env("RECON_DATABASE_URL")?,
        &recon_db::PoolSettings::default(),
    )?;
    let heads = recon_db::decisions::heads(&pool, firm).await?;
    let mut broken = false;
    for client in previous
        .keys()
        .filter(|c| !heads.iter().any(|(h, _, _)| h == *c))
    {
        println!("client {client}: BROKEN, anchored but absent from the database");
        broken = true;
    }
    let mut lines = String::new();
    for (client, _, at) in &heads {
        let anchored = previous.get(client).map_or(&[][..], Vec::as_slice);
        match recon_db::decisions::verify_chain_paged(&pool, firm, *client, 5_000, anchored).await?
        {
            Ok(head) => {
                let signed = Anchor::sign(&key, firm, *client, head.seq, head.hash, *at)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                lines.push_str(&serde_json::to_string(&signed)?);
                lines.push('\n');
            }
            Err(b) => {
                println!("client {client}: BROKEN at seq {} ({:?})", b.seq, b.kind);
                broken = true;
            }
        }
    }
    if broken {
        println!("nothing anchored: a chain does not verify");
        return Ok(ExitCode::from(3));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out)
        .with_context(|| format!("opening {out}"))?;
    std::io::Write::write_all(&mut file, lines.as_bytes())?;
    file.sync_all()?;
    println!(
        "anchor ok: {} verified heads appended to {out}",
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
    let checked = clients.len();
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
    println!("{checked} chains checked");
    if checked == 0 && !args.iter().any(|a| a == "--allow-empty") {
        println!(
            "nothing verified: firm {firm} has no chains (pass --allow-empty if that is expected)"
        );
        return Ok(ExitCode::from(4));
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
