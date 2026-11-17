//! `recon import check` and `recon profile check` (D-068): read a file exactly as the server
//! would — same profile, same parser, same refusals — without uploading it, and say what
//! would be imported or, for every refused line, where it is, which column, what is wrong
//! and how to fix it. Nothing here touches a database or the network.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use recon_domain::import::{PROFILE_DOCUMENTS, PROFILE_OFX, PROFILE_R0};
use recon_domain::{AccountId, ClientId, Currency, Date, Money, MoneySum};
use recon_import::report::{
    ProfileRef, Report, describe_date, describe_number, file_report, rows_report,
};
use recon_import::{
    AccountContext, CompiledDocuments, CompiledStatement, CsvLimits, DocumentContext,
    DocumentParser, DocumentProfile, FileRefusal, OfxParser, ParseSummary, StatementParser,
    StatementProfile,
};
use serde_json::{Value, json};

/// Bytes read per chunk: the file is streamed, never held whole.
const CHUNK: usize = 64 * 1024;

/// A profile loaded for a check.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// `name vN`, or the built-in's name.
    pub label: String,
    /// The profile.
    pub profile: ProfileRef,
}

fn builtin(name: &str) -> Option<Loaded> {
    let profile = match name {
        PROFILE_R0 => ProfileRef::Statement(Arc::new(CompiledStatement::r0())),
        PROFILE_OFX => ProfileRef::Ofx,
        PROFILE_DOCUMENTS => ProfileRef::Documents(Arc::new(CompiledDocuments::builtin())),
        _ => return None,
    };
    Some(Loaded {
        label: format!("{name} (built in)"),
        profile,
    })
}

/// Loads a profile: a built-in name, or a JSON file in the form `POST …/profiles` takes
/// (`{"name", "kind", "definition"}`). A profile that cannot be used is refused with every
/// problem listed.
pub fn load_profile(spec: &str) -> anyhow::Result<Result<Loaded, Vec<String>>> {
    if let Some(b) = builtin(spec) {
        return Ok(Ok(b));
    }
    let text = std::fs::read_to_string(spec).with_context(|| {
        format!(
            "{spec} is neither a built-in profile ({PROFILE_R0}, {PROFILE_OFX}, {PROFILE_DOCUMENTS}) nor a readable profile file"
        )
    })?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("{spec} is not JSON"))?;
    let name = value["name"].as_str().unwrap_or("unnamed").to_owned();
    let Some(kind) = value["kind"].as_str() else {
        bail!("{spec} has no \"kind\" (statement_csv or documents_csv)");
    };
    let definition = value
        .get("definition")
        .cloned()
        .with_context(|| format!("{spec} has no \"definition\""))?;
    let unreadable =
        |_| vec!["definition: a field is missing, unknown or of the wrong type".to_owned()];
    Ok(match kind {
        "statement_csv" => serde_json::from_value::<StatementProfile>(definition)
            .map_err(unreadable)
            .and_then(|p| p.compile().map_err(|e| e.problems))
            .map(|c| Loaded {
                label: format!("{name} (from {spec})"),
                profile: ProfileRef::Statement(Arc::new(c)),
            }),
        "documents_csv" => serde_json::from_value::<DocumentProfile>(definition)
            .map_err(unreadable)
            .and_then(|p| p.compile().map_err(|e| e.problems))
            .map(|c| Loaded {
                label: format!("{name} (from {spec})"),
                profile: ProfileRef::Documents(Arc::new(c)),
            }),
        other => bail!("{spec}: kind {other:?} is not statement_csv or documents_csv"),
    })
}

/// What a check found.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// True when the file would import.
    pub ok: bool,
    /// The result for a terminal.
    pub text: String,
    /// The same as JSON (`--json`).
    pub json: Value,
}

/// `minor` units of `currency` written exactly: `-1234.50 INR`.
pub fn exact(minor: i128, currency: Currency) -> String {
    let exponent = usize::from(currency.exponent());
    let digits = minor.unsigned_abs().to_string();
    let sign = if minor < 0 { "-" } else { "" };
    if exponent == 0 {
        return format!("{sign}{digits} {currency}");
    }
    let padded = format!("{digits:0>width$}", width = exponent + 1);
    let (int_part, frac) = padded.split_at(padded.len() - exponent);
    format!("{sign}{int_part}.{frac} {currency}")
}

/// Totals of what would be imported, per currency.
#[derive(Debug, Default)]
struct Totals {
    rows: u64,
    first: Option<Date>,
    last: Option<Date>,
    money: BTreeMap<Currency, (MoneySum, MoneySum)>,
}

impl Totals {
    fn add(&mut self, date: Date, amount: Money) {
        self.rows += 1;
        self.first = Some(self.first.map_or(date, |d| d.min(date)));
        self.last = Some(self.last.map_or(date, |d| d.max(date)));
        let c = amount.currency();
        let (into, out) = self
            .money
            .entry(c)
            .or_insert((MoneySum::zero(c), MoneySum::zero(c)));
        // Sums are i128 and amounts bounded: adding one never fails.
        let _ = if amount.minor() >= 0 {
            into.add(amount)
        } else {
            out.add(amount)
        };
    }
}

fn stream(
    path: &str,
    mut push: impl FnMut(&[u8]) -> Result<(), FileRefusal>,
) -> anyhow::Result<Result<(), FileRefusal>> {
    let mut file = std::fs::File::open(path).with_context(|| format!("cannot open {path}"))?;
    let mut buf = vec![0_u8; CHUNK];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("cannot read {path}"))?;
        if n == 0 {
            return Ok(Ok(()));
        }
        if let Err(e) = push(buf.get(..n).unwrap_or_default()) {
            return Ok(Err(e));
        }
    }
}

/// Checks `path` with `profile`; `currency` is the account's (statements only).
pub fn check_file(
    path: &str,
    loaded: &Loaded,
    currency: Option<Currency>,
) -> anyhow::Result<Outcome> {
    let mut totals = Totals::default();
    let statement_context = || -> anyhow::Result<AccountContext> {
        Ok(AccountContext {
            account: AccountId::from_uuid(uuid::Uuid::nil()),
            currency: currency.context(
                "--currency is required for a statement: the account's currency, for example --currency INR",
            )?,
        })
    };
    let result: Result<ParseSummary, FileRefusal> = match &loaded.profile {
        ProfileRef::Statement(p) => {
            let mut parser = StatementParser::with_profile(
                statement_context()?,
                p.clone(),
                CsvLimits::default(),
            );
            let mut on = |l: recon_domain::import::NewLine| totals.add(l.booking_date, l.amount);
            match stream(path, |chunk| parser.push(chunk, &mut on))? {
                Ok(()) => parser.finish(&mut on),
                Err(e) => Err(e),
            }
        }
        ProfileRef::Ofx => {
            let mut parser = OfxParser::new(statement_context()?);
            let mut on = |l: recon_domain::import::NewLine| totals.add(l.booking_date, l.amount);
            match stream(path, |chunk| parser.push(chunk, &mut on))? {
                Ok(()) => parser.finish(&mut on),
                Err(e) => Err(e),
            }
        }
        ProfileRef::Documents(p) => {
            let mut parser = DocumentParser::with_profile(
                DocumentContext {
                    client: ClientId::from_uuid(uuid::Uuid::nil()),
                },
                p.clone(),
                CsvLimits::default(),
            );
            let mut on =
                |d: recon_domain::document::NewDocument| totals.add(d.issue_date, d.amount);
            match stream(path, |chunk| parser.push(chunk, &mut on))? {
                Ok(()) => parser.finish(&mut on),
                Err(e) => Err(e),
            }
        }
        ProfileRef::Unknown => bail!("no profile to read the file with"),
    };
    let view = loaded.profile.view();
    let refused = |report: Report, summary: Option<&ParseSummary>| Outcome {
        ok: false,
        text: report.to_text(path, &loaded.label),
        json: json!({
            "file": path, "ok": false, "profile": loaded.label,
            "rows_read": summary.map(|s| s.rows_read), "report": report,
        }),
    };
    Ok(match result {
        Err(refusal) => refused(file_report(&refusal, &view), None),
        Ok(summary) if !summary.is_clean() => refused(
            rows_report(
                summary.rows_read,
                summary.rows_refused,
                &summary.refusals,
                &view,
            ),
            Some(&summary),
        ),
        Ok(summary) => would_import(path, loaded, &summary, &totals),
    })
}

/// The result for a file that would import: rows, dates, money in and out exactly.
fn would_import(path: &str, loaded: &Loaded, summary: &ParseSummary, totals: &Totals) -> Outcome {
    let dates = match (totals.first, totals.last) {
        (Some(a), Some(b)) => format!("{a} to {b}"),
        _ => "none".to_owned(),
    };
    let mut text = format!(
        "{path}: would import {} rows.\n  Profile: {}\n  Dates:   {dates}\n",
        summary.rows_read, loaded.label
    );
    let mut money = Vec::new();
    for (currency, (into, out)) in &totals.money {
        let _ = writeln!(
            text,
            "  {currency}:     money in {}, money out {}",
            exact(into.minor(), *currency),
            exact(-out.minor(), *currency)
        );
        money.push(json!({
            "currency": currency.code(),
            "money_in": exact(into.minor(), *currency),
            "money_out": exact(-out.minor(), *currency),
        }));
    }
    let _ = writeln!(
        text,
        "  SHA-256: {}\n  Not checked here: rows already imported (duplicates) and changes to stored lines (revisions); the server reports those when you upload.",
        summary.file_hash
    );
    Outcome {
        ok: true,
        text,
        json: json!({
            "file": path, "ok": true, "profile": loaded.label,
            "rows_read": summary.rows_read, "dates": dates, "money": money,
            "sha256": summary.file_hash.to_string(),
        }),
    }
}

/// `recon profile check`: every problem, or how the profile will read files.
pub fn check_profile(path: &str) -> anyhow::Result<Outcome> {
    Ok(match load_profile(path)? {
        Err(problems) => Outcome {
            ok: false,
            text: format!(
                "{path}: this profile cannot be used ({} problems):\n{}\n",
                problems.len(),
                problems
                    .iter()
                    .map(|p| format!("  - {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            json: json!({ "file": path, "ok": false, "problems": problems }),
        },
        Ok(loaded) => {
            let (header, dates, numbers, sha) = match &loaded.profile {
                ProfileRef::Statement(c) => (
                    c.profile().header.join(","),
                    describe_date(c.profile().date_format),
                    describe_number(&c.profile().number_format),
                    hex::encode(c.sha256()),
                ),
                ProfileRef::Documents(c) => (
                    c.profile().header.join(","),
                    describe_date(c.profile().date_format),
                    describe_number(&c.profile().number_format),
                    hex::encode(c.sha256()),
                ),
                ProfileRef::Ofx | ProfileRef::Unknown => (
                    "(OFX has no header)".to_owned(),
                    "YYYYMMDD".to_owned(),
                    "like -1234.56".to_owned(),
                    String::new(),
                ),
            };
            Outcome {
                ok: true,
                text: format!(
                    "{path}: OK, {}.\n  Header:  {header}\n  Dates:   {dates}\n  Amounts: {numbers}\n  SHA-256: {sha} (recorded on every import read with it)\n",
                    loaded.label
                ),
                json: json!({
                    "file": path, "ok": true, "profile": loaded.label, "header": header,
                    "dates": dates, "amounts": numbers, "sha256": sha,
                }),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_are_written_exactly_with_their_currency() {
        assert_eq!(exact(-123_450, Currency::Inr), "-1234.50 INR");
        assert_eq!(exact(5, Currency::Usd), "0.05 USD");
        assert_eq!(exact(1_000, Currency::Jpy), "1000 JPY");
        assert_eq!(exact(-1, Currency::Kwd), "-0.001 KWD");
        assert_eq!(exact(0, Currency::Eur), "0.00 EUR");
    }
}
