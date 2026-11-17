//! The seeded statement generator (`gen-v2`, D-028): the same seed always writes the same
//! bytes, so drills and load runs are reproducible and two runs can be compared by digest.
//!
//! It generates bank statement lines (documents and the labelled ground truth come with
//! matching, R2). The lines are shaped like real exports: references on most rows, a
//! handful of counterparties, quoted narrations with commas, running balances that continue
//! row to row, and identical same-day twins (two genuine transactions D-008 must keep: the
//! same date, amount, narration and reference, each with its own running balance).
//! `gen-v1` copied a twin's balance too, which balance continuity (D-008, R1) refuses.

use std::io::{self, Write};

use recon_domain::{Currency, Date, Money};

/// The generator's version, recorded with every result it feeds.
pub const GENERATOR_VERSION: &str = "gen-v2";

/// What to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementSpec {
    /// The seed.
    pub seed: u64,
    /// Data rows.
    pub lines: u32,
    /// The account's currency.
    pub currency: Currency,
    /// The first booking date.
    pub start: Date,
    /// Calendar days the statement spans (at least 1).
    pub days: u32,
    /// Every n-th row repeats the previous one exactly (0: never).
    pub twin_every: u32,
}

impl StatementSpec {
    /// A month of `lines` rows from `start`, a twin every 997 rows.
    pub const fn month(seed: u64, lines: u32, currency: Currency, start: Date) -> Self {
        Self {
            seed,
            lines,
            currency,
            start,
            days: 28,
            twin_every: 997,
        }
    }
}

/// `SplitMix64`: tiny, fast, and fixed forever for `gen-v1`.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (`n > 0`; 0 when `n == 0`).
    pub const fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next_u64() % n
    }
}

const COUNTERPARTIES: [&str; 12] = [
    "Acme Traders",
    "Bluefin Logistics",
    "Cobalt Software",
    "Deccan Spices",
    "Evergreen Rentals",
    "Fairway Insurance",
    "Granite Builders",
    "Harbor Utilities",
    "Indigo Print, Ltd",
    "Juniper Payroll",
    "Kestrel Freight",
    "Lumen Electric",
];

fn csv_field(out: &mut Vec<u8>, field: &str) {
    if field.contains([',', '"']) {
        out.push(b'"');
        out.extend_from_slice(field.replace('"', "\"\"").as_bytes());
        out.push(b'"');
    } else {
        out.extend_from_slice(field.as_bytes());
    }
}

fn add_days(start: Date, days: u32) -> Date {
    let mut d = start;
    for _ in 0..days {
        match d.succ() {
            Some(next) => d = next,
            None => break,
        }
    }
    d
}

/// Writes the statement as `r0-generic-csv` v1 to `out`. Returns the number of data rows.
pub fn write_statement(spec: &StatementSpec, out: &mut impl Write) -> io::Result<u32> {
    let mut rng = Rng::new(spec.seed);
    let exponent = u32::from(spec.currency.exponent());
    let unit = 10_i64.pow(exponent);
    let mut balance: i64 = i64::try_from(rng.below(5_000_000))
        .unwrap_or(0)
        .saturating_mul(unit);
    let mut buf = Vec::with_capacity(256 * 1024);
    buf.extend_from_slice(
        b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n",
    );
    // The previous row without its balance, and its amount, for a twin.
    let mut previous: Option<(Vec<u8>, i64)> = None;
    let days = spec.days.max(1);
    let mut date = spec.start;
    let mut day_index = 0_u32;
    for i in 0..spec.lines {
        let target_day =
            u32::try_from(u64::from(i) * u64::from(days) / u64::from(spec.lines.max(1)))
                .unwrap_or(0);
        if target_day > day_index {
            date = add_days(date, target_day - day_index);
            day_index = target_day;
        }
        let twin = spec.twin_every > 0 && i > 0 && i % spec.twin_every == 0;
        if let Some((head, amount)) = previous.as_ref().filter(|_| twin) {
            // An identical twin: same day, amount, narration and reference; the balance
            // moves on by the amount again.
            balance = balance.saturating_add(*amount);
            buf.extend_from_slice(head);
            push_balance(&mut buf, balance, spec.currency);
        } else {
            let (head, amount) = row(&mut rng, spec, date, unit, &mut balance, i);
            buf.extend_from_slice(&head);
            push_balance(&mut buf, balance, spec.currency);
            previous = Some((head, amount));
        }
        if buf.len() >= 192 * 1024 {
            out.write_all(&buf)?;
            buf.clear();
        }
    }
    out.write_all(&buf)?;
    out.flush()?;
    Ok(spec.lines)
}

fn push_balance(out: &mut Vec<u8>, balance: i64, currency: Currency) {
    let text = Money::new(balance, currency)
        .map(Money::to_decimal_string)
        .unwrap_or_default();
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
}

/// One row up to and including the comma before its balance, and its amount; `balance`
/// moves by the amount.
fn row(
    rng: &mut Rng,
    spec: &StatementSpec,
    date: Date,
    unit: i64,
    balance: &mut i64,
    i: u32,
) -> (Vec<u8>, i64) {
    let counterparty = COUNTERPARTIES[usize::try_from(rng.below(12)).unwrap_or(0)];
    // Amounts from 1 minor unit up to 25,000 major units, a third of them receipts.
    let magnitude = i64::try_from(rng.below(2_500_000))
        .unwrap_or(0)
        .saturating_add(1)
        * unit
        / 100;
    let magnitude = magnitude.max(1);
    let mut amount = if rng.below(3) == 0 {
        magnitude
    } else {
        -magnitude
    };
    if balance.saturating_add(amount).abs() > 900_000_000_000 {
        amount = -amount;
    }
    *balance = balance.saturating_add(amount);
    let reference = match rng.below(10) {
        0..=6 => format!("INV-{}-{:05}", date.year(), rng.below(100_000)),
        7 => format!("CHQ{:06}", rng.below(1_000_000)),
        _ => String::new(),
    };
    let narration = match rng.below(5) {
        0 => format!(
            "NEFT/{}/{}",
            rng.below(1_000_000_000),
            counterparty.to_uppercase()
        ),
        1 => format!("UPI/{}/{counterparty}", rng.below(1_000_000_000_000)),
        2 => format!("CARD {counterparty} #{}", rng.below(10_000)),
        3 => format!("Payment {counterparty}, ref {i}"),
        _ => format!("TRF {counterparty} {}", rng.below(100_000)),
    };
    let value_date = if rng.below(4) == 0 {
        String::new()
    } else {
        date.to_iso()
    };
    let money = Money::new(amount, spec.currency)
        .map(Money::to_decimal_string)
        .unwrap_or_default();
    let mut out = Vec::with_capacity(160);
    out.extend_from_slice(date.to_iso().as_bytes());
    out.push(b',');
    out.extend_from_slice(value_date.as_bytes());
    out.push(b',');
    out.extend_from_slice(money.as_bytes());
    out.push(b',');
    out.extend_from_slice(spec.currency.code().as_bytes());
    out.push(b',');
    csv_field(&mut out, &narration);
    out.push(b',');
    csv_field(&mut out, &reference);
    out.push(b',');
    csv_field(&mut out, counterparty);
    out.push(b',');
    (out, amount)
}

/// The statement as bytes (tests and the load driver).
pub fn statement(spec: &StatementSpec) -> Vec<u8> {
    let mut out = Vec::new();
    // Writing to a Vec cannot fail.
    let _ = write_statement(spec, &mut out);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests: a panic is the failure report")]

    use super::*;
    use recon_domain::AccountId;
    use recon_import::{AccountContext, StatementParser};
    use uuid::Uuid;

    #[test]
    fn the_same_seed_writes_the_same_bytes_and_the_profile_accepts_them() {
        for currency in [Currency::Usd, Currency::Jpy, Currency::Kwd] {
            let spec = StatementSpec::month(42, 5_000, currency, Date::new(2026, 2, 1).unwrap());
            let a = statement(&spec);
            assert_eq!(a, statement(&spec));
            assert_ne!(a, statement(&StatementSpec { seed: 43, ..spec }));
            let mut parser = StatementParser::new(AccountContext {
                account: AccountId::from_uuid(Uuid::from_u128(1)),
                currency,
            });
            let mut lines = vec![];
            parser.push(&a, &mut |l| lines.push(l)).unwrap();
            let summary = parser.finish(&mut |l| lines.push(l)).unwrap();
            assert!(summary.is_clean(), "{currency}: {:?}", summary.refusals);
            assert_eq!(lines.len(), 5_000);
            // Twins are present and identical.
            assert_eq!(lines[997].tuple_hash, lines[996].tuple_hash);
            assert!(lines.last().unwrap().booking_date <= Date::new(2026, 3, 1).unwrap());
        }
    }
}
