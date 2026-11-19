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
    /// Only the rows booked on these day indexes (inclusive, from 0) are written; the
    /// sequence is generated whole, so two windows of one spec share their common days'
    /// rows byte for byte, running balances included (the `r1-import` overlaps).
    pub window: Option<(u32, u32)>,
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
            window: None,
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

/// Writes the statement as `r0-generic-csv` to `out`. Returns the number of data rows
/// written (all of them, or the window's).
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
    // The previous row without its balance, its amount and its day, for a twin.
    let mut previous: Option<(Vec<u8>, i64, u32)> = None;
    let mut written = 0_u32;
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
        let in_window = |day: u32| spec.window.is_none_or(|(a, b)| (a..=b).contains(&day));
        if let Some((head, amount, day)) = previous.as_ref().filter(|_| twin) {
            // An identical twin: same day, amount, narration and reference; the balance
            // moves on by the amount again. It is booked on the original's day.
            balance = balance.saturating_add(*amount);
            if in_window(*day) {
                buf.extend_from_slice(head);
                push_balance(&mut buf, balance, spec.currency);
                written = written.saturating_add(1);
            }
        } else {
            let (head, amount) = row(&mut rng, spec, date, unit, &mut balance, i);
            if in_window(day_index) {
                buf.extend_from_slice(&head);
                push_balance(&mut buf, balance, spec.currency);
                written = written.saturating_add(1);
            }
            previous = Some((head, amount, day_index));
        }
        if buf.len() >= 192 * 1024 {
            out.write_all(&buf)?;
            buf.clear();
        }
    }
    out.write_all(&buf)?;
    out.flush()?;
    Ok(written)
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

// ---------------------------------------------------------------------------------------------
// gen-v3: the labelled matching set (D-028).

/// The matching-set generator's version, recorded with every result it feeds.
pub const MATCH_GENERATOR_VERSION: &str = "gen-v3";

/// Unique amounts live here: every bucket that needs an amount no other document can
/// share takes from `UNIQUE_BASE + n·UNIQUE_STEP`, above every randomized amount, so
/// ambiguity is only ever the labelled kind. The step is larger than any residual
/// tolerance plus the largest generated fee, so a bank-fee twin never lands on
/// another line's unique amount and a slack window holds exactly its own document.
const UNIQUE_BASE: i64 = 500_000_000;
/// The unique-amount spacing: > tolerance (`1_000`) + max fee (`500`).
const UNIQUE_STEP: i64 = 2_000;

/// What a labelled matching set covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchSpec {
    /// The seed.
    pub seed: u64,
    /// Statement lines.
    pub lines: u32,
    /// Extra documents beyond the ones the lines name (the load run's distractors,
    /// D-027: 1.1 documents per line).
    pub distractors: u32,
    /// Currency shared by lines and documents (cross-currency traps use another).
    pub currency: Currency,
    /// The first booking date.
    pub start: Date,
    /// Calendar days spanned (at least 1).
    pub days: u32,
}

/// The three files of one set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSet {
    /// `r0-generic-csv` statement bytes.
    pub statement: Vec<u8>,
    /// `r1-generic-documents` bytes.
    pub documents: Vec<u8>,
    /// The ground truth: `line,expected,documents,residual_minor` per data row.
    pub truth: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    /// Exact reference and amount: T1 (D-027's 70%).
    ExactReference,
    /// Exact amount, unique in the window, no reference: T2 (15%).
    ExactAmount,
    /// One line to several documents of one counterparty: T3 (5%).
    ManyToOne,
    /// One document a named residual short of the line: T4 (5%).
    BankFee,
    /// The "no match" fifth, split into the real no-match and D-028's traps:
    /// an equal-amount pair (`multiple_candidates`), a foreign-currency twin
    /// (`cross_currency`) and a referenced-but-wrong-amount document
    /// (`amount_mismatch`).
    NoCandidate,
    MultipleCandidates,
    CrossCurrency,
    AmountMismatch,
}

impl Bucket {
    fn label(self) -> &'static str {
        match self {
            Self::ExactReference => "t1",
            Self::ExactAmount => "t2",
            Self::ManyToOne => "t3",
            Self::BankFee => "t4",
            Self::MultipleCandidates => "multiple_candidates",
            // The foreign twin keeps the line from any same-currency outcome but
            // cannot empty the same-currency candidate set; the engine's answer
            // is deterministically `no_candidate` (`cross_currency` needs an empty
            // set — only a client whose every in-window document is foreign).
            Self::NoCandidate | Self::CrossCurrency => "no_candidate",
            Self::AmountMismatch => "amount_mismatch",
        }
    }
}

/// One document waiting to be written.
struct DocRow {
    number: String,
    kind: &'static str,
    issue: Date,
    due: Date,
    amount: i64,
    counterparty: String,
    /// A cross-currency twin: written with another currency's code.
    foreign: bool,
}

/// The monotonic counters behind generated names and amounts.
#[derive(Default)]
struct Seqs {
    doc: u64,
    unique: i64,
    bulk: u64,
}

fn next_doc(
    seqs: &mut Seqs,
    kind: &'static str,
    issue: Date,
    due: Date,
    amount: i64,
    cp: &str,
) -> DocRow {
    seqs.doc = seqs.doc.saturating_add(1);
    DocRow {
        number: format!("D-{:06}", seqs.doc),
        kind,
        issue,
        due,
        amount,
        counterparty: cp.to_owned(),
        foreign: false,
    }
}

fn unique_amount(seqs: &mut Seqs) -> i64 {
    seqs.unique = seqs.unique.saturating_add(1);
    UNIQUE_BASE.saturating_add(seqs.unique.saturating_mul(UNIQUE_STEP))
}

/// What one line's bucket already knows before its documents are built.
struct LineCtx<'a> {
    /// The line's index (unique names derive from it).
    i: u32,
    /// `1` inbound, `-1` outbound.
    sign: i64,
    /// The document kind the direction pairs with.
    kind: &'static str,
    /// The shared counterparty of the line.
    cp: &'a str,
    /// The documents' issue and due days.
    issue: Date,
    due: Date,
}

/// What a bucket produces for its line.
struct BucketOut {
    amount: i64,
    docs: Vec<DocRow>,
    narration: String,
    reference: String,
    counterparty: String,
    residual: String,
}

/// The bucket's amount, documents and line fields, in the rng order `match_set`
/// has always used (determinism is over call order, not helpers).
fn bucket_docs(bucket: Bucket, ctx: &LineCtx<'_>, rng: &mut Rng, seqs: &mut Seqs) -> BucketOut {
    let mut out = BucketOut {
        amount: 0,
        docs: Vec::new(),
        narration: String::new(),
        reference: String::new(),
        counterparty: ctx.cp.to_owned(),
        residual: String::new(),
    };
    match bucket {
        Bucket::ExactReference => {
            out.amount = ctx.sign
                * i64::try_from(rng.below(2_490_000).saturating_add(10_000)).unwrap_or(10_000);
            let doc = next_doc(seqs, ctx.kind, ctx.issue, ctx.due, out.amount, ctx.cp);
            out.reference.clone_from(&doc.number);
            out.narration = format!("PAYMENT {} {}", ctx.cp.to_uppercase(), doc.number);
            out.docs.push(doc);
        }
        Bucket::ExactAmount => {
            out.amount = ctx.sign * unique_amount(seqs);
            let doc = next_doc(seqs, ctx.kind, ctx.issue, ctx.due, out.amount, ctx.cp);
            out.narration = format!("SETTLEMENT {}", ctx.cp.to_uppercase());
            out.docs.push(doc);
        }
        Bucket::ManyToOne => {
            out.amount = ctx.sign * unique_amount(seqs);
            // A counterparty of its own per group: the combination pool holds
            // exactly this group's parts, so the decomposition is unique.
            seqs.bulk = seqs.bulk.saturating_add(1);
            out.counterparty = format!("BULK{:04}", seqs.bulk);
            let parts = usize::try_from(rng.below(4).saturating_add(2)).unwrap_or(2);
            let mut left = out.amount.unsigned_abs();
            for p in 0..parts {
                let last = p == parts - 1;
                let share = if last {
                    left
                } else {
                    // A positive split, never a zero part.
                    let room = left.saturating_sub(u64::try_from(parts - p - 1).unwrap_or(0));
                    rng.below(room.saturating_sub(1)).saturating_add(1)
                };
                left = left.saturating_sub(share);
                let amount = ctx.sign * i64::try_from(share).unwrap_or(0);
                out.docs.push(next_doc(
                    seqs,
                    ctx.kind,
                    ctx.issue,
                    ctx.due,
                    amount,
                    &out.counterparty,
                ));
            }
            out.narration = format!("BULK TRANSFER {}", out.counterparty);
        }
        Bucket::BankFee => {
            out.amount = ctx.sign * unique_amount(seqs);
            // Its own counterparty: the slack search's pool then holds only the
            // twin — a shared one would exhaust the node budget on small parts.
            out.counterparty = format!("FEE{:06}", ctx.i);
            let fee = i64::try_from(rng.below(400).saturating_add(100)).unwrap_or(100);
            let amount = out.amount.saturating_add(ctx.sign.saturating_mul(fee));
            out.docs.push(next_doc(
                seqs,
                ctx.kind,
                ctx.issue,
                ctx.due,
                amount,
                &out.counterparty,
            ));
            out.residual = fee.to_string();
            out.narration = format!("TRF {} FEE", out.counterparty.to_uppercase());
        }
        Bucket::NoCandidate => {
            out.amount = ctx.sign * unique_amount(seqs);
            out.counterparty = format!("UNMATCHED{:06}", ctx.i);
            "MISC PAYMENT".clone_into(&mut out.narration);
        }
        Bucket::MultipleCandidates => {
            out.amount = ctx.sign * unique_amount(seqs);
            for _ in 0..2 {
                out.docs.push(next_doc(
                    seqs, ctx.kind, ctx.issue, ctx.due, out.amount, ctx.cp,
                ));
            }
            out.narration = format!("PAYMENT {}", ctx.cp.to_uppercase());
        }
        Bucket::CrossCurrency => {
            out.amount = ctx.sign * unique_amount(seqs);
            // The twin in another currency with the line's amount: never
            // matchable. The counterparty is unique and nothing names the
            // twin, so the line deterministically ends `no_candidate` —
            // `cross_currency` itself needs an empty same-currency candidate
            // set (only a client whose every in-window document is foreign;
            // the engine's own tests cover that branch).
            let mut doc = next_doc(seqs, ctx.kind, ctx.issue, ctx.due, out.amount, ctx.cp);
            doc.foreign = true;
            out.counterparty = format!("FOREIGN{:06}", ctx.i);
            out.narration = format!("PAYMENT {}", out.counterparty.to_uppercase());
            out.docs.push(doc);
        }
        Bucket::AmountMismatch => {
            out.amount = ctx.sign * unique_amount(seqs);
            let amount = out.amount.saturating_add(ctx.sign.saturating_mul(777));
            let doc = next_doc(seqs, ctx.kind, ctx.issue, ctx.due, amount, ctx.cp);
            out.reference.clone_from(&doc.number);
            out.narration = format!("PAYMENT {} {}", ctx.cp.to_uppercase(), doc.number);
            out.docs.push(doc);
        }
    }
    out
}

/// The distractor documents: extra open documents from the shared pools, amounts in
/// the randomized range below `UNIQUE_BASE` so they compete for attention without
/// ever stealing a labelled outcome.
fn distractors(spec: &MatchSpec, rng: &mut Rng, seqs: &mut Seqs, days: u32) -> Vec<DocRow> {
    (0..spec.distractors)
        .map(|_| {
            let inbound = rng.below(3) == 0;
            let kind = if inbound {
                "sales_invoice"
            } else {
                "purchase_bill"
            };
            let cp = COUNTERPARTIES[usize::try_from(rng.below(12)).unwrap_or(0)];
            let issue = add_days(
                spec.start,
                u32::try_from(rng.below(u64::from(days))).unwrap_or(0),
            );
            let amount = (if inbound { 1 } else { -1 })
                * i64::try_from(rng.below(2_490_000).saturating_add(10_000)).unwrap_or(10_000);
            next_doc(seqs, kind, issue, add_days(issue, 30), amount, cp)
        })
        .collect()
}

/// The documents file bytes (`r1-generic-documents`), foreign twins in their own
/// currency's code.
fn render_docs(docs: &[DocRow], currency: Currency) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(docs.len() * 100);
    out.extend_from_slice(
        b"kind,number,issue_date,due_date,amount,currency,counterparty,description\n",
    );
    let foreign = if currency == Currency::Eur {
        Currency::Usd
    } else {
        Currency::Eur
    };
    for doc in docs {
        let doc_currency = if doc.foreign { foreign } else { currency };
        let money = Money::new(doc.amount, doc_currency)
            .map(Money::to_decimal_string)
            .unwrap_or_default();
        csv_field(&mut out, doc.kind);
        out.push(b',');
        csv_field(&mut out, &doc.number);
        out.push(b',');
        csv_field(&mut out, &doc.issue.to_iso());
        out.push(b',');
        csv_field(&mut out, &doc.due.to_iso());
        out.push(b',');
        out.extend_from_slice(money.as_bytes());
        out.push(b',');
        out.extend_from_slice(doc_currency.code().as_bytes());
        out.push(b',');
        csv_field(&mut out, &doc.counterparty);
        out.push(b',');
        out.push(b'\n');
    }
    out
}

/// The three files of `spec`: statement lines, documents and the ground truth, byte
/// for byte reproducible from the seed alone.
///
/// Buckets follow the D-027 mix (70/15/5/5/5) with the "no match" fifth split between
/// a true `no_candidate` and the D-028 traps. Every amount that must be unique is taken
/// from a disjoint sequence, every many-to-one group gets a counterparty of its own, so
/// the outcome a line is labelled is the outcome the engine is required to produce.
pub fn match_set(spec: &MatchSpec) -> MatchSet {
    let mut rng = Rng::new(spec.seed);
    let unit = 10_i64.pow(u32::from(spec.currency.exponent()));
    let mut balance: i64 = i64::try_from(rng.below(5_000_000))
        .unwrap_or(0)
        .saturating_mul(unit);
    let mut statement: Vec<u8> = Vec::with_capacity(spec.lines as usize * 120);
    statement.extend_from_slice(
        b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n",
    );
    let mut docs: Vec<DocRow> = Vec::new();
    let mut truth: Vec<u8> = Vec::with_capacity(spec.lines as usize * 40);
    truth.extend_from_slice(b"line,expected,documents,residual_minor\n");
    let mut seqs = Seqs::default();
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
        // 70/15/5/5/5 (D-027); the "no match" fifth splits into no_candidate and traps.
        let bucket = match rng.below(20) {
            r if r < 14 => Bucket::ExactReference,
            r if r < 17 => Bucket::ExactAmount,
            17 => Bucket::ManyToOne,
            18 => Bucket::BankFee,
            _ => match rng.below(4) {
                0 => Bucket::NoCandidate,
                1 => Bucket::MultipleCandidates,
                2 => Bucket::CrossCurrency,
                _ => Bucket::AmountMismatch,
            },
        };
        let inbound = rng.below(3) == 0;
        let sign: i64 = if inbound { 1 } else { -1 };
        let kind = if inbound {
            "sales_invoice"
        } else {
            "purchase_bill"
        };
        let cp = COUNTERPARTIES[usize::try_from(rng.below(12)).unwrap_or(0)];
        // The document's issue day is up to six days before the booking day, inside
        // the engine's window (issue − 7 .. due + 90, D-011).
        let issue = add_days(
            spec.start,
            day_index.saturating_sub(u32::try_from(rng.below(7)).unwrap_or(0)),
        );
        let ctx = LineCtx {
            i,
            sign,
            kind,
            cp,
            issue,
            due: add_days(issue, 30),
        };
        let out = bucket_docs(bucket, &ctx, &mut rng, &mut seqs);
        let named: Vec<String> = out.docs.iter().map(|d| d.number.clone()).collect();
        docs.extend(out.docs);
        balance = balance.saturating_add(out.amount);
        truth.extend_from_slice(
            format!(
                "{},{},{},{}\n",
                i.saturating_add(1),
                bucket.label(),
                named.join("+"),
                out.residual
            )
            .as_bytes(),
        );
        let money = Money::new(out.amount, spec.currency)
            .map(Money::to_decimal_string)
            .unwrap_or_default();
        csv_field(&mut statement, &date.to_iso());
        statement.push(b',');
        csv_field(&mut statement, &date.to_iso());
        statement.push(b',');
        statement.extend_from_slice(money.as_bytes());
        statement.push(b',');
        statement.extend_from_slice(spec.currency.code().as_bytes());
        statement.push(b',');
        csv_field(&mut statement, &out.narration);
        statement.push(b',');
        csv_field(&mut statement, &out.reference);
        statement.push(b',');
        csv_field(&mut statement, &out.counterparty);
        statement.push(b',');
        let bal = Money::new(balance, spec.currency)
            .map(Money::to_decimal_string)
            .unwrap_or_default();
        statement.extend_from_slice(bal.as_bytes());
        statement.push(b'\n');
    }
    docs.extend(distractors(spec, &mut rng, &mut seqs, days));
    MatchSet {
        statement,
        documents: render_docs(&docs, spec.currency),
        truth,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests: a panic is the failure report")]

    use super::*;
    use recon_domain::AccountId;
    use recon_import::{AccountContext, DocumentParser, StatementParser};
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
            // Two windows of one spec share their common days' rows byte for byte, and
            // each window's balances continue (the parser accepts it).
            let first = statement(&StatementSpec {
                window: Some((0, 14)),
                ..spec
            });
            let second = statement(&StatementSpec {
                window: Some((10, 27)),
                ..spec
            });
            let rows = |b: &[u8]| -> Vec<Vec<u8>> {
                b.split(|c| *c == b'\n')
                    .skip(1)
                    .map(<[u8]>::to_vec)
                    .collect()
            };
            let later: std::collections::HashSet<Vec<u8>> = rows(&second).into_iter().collect();
            let shared: Vec<_> = rows(&first)
                .into_iter()
                .filter(|r| later.contains(r))
                .collect();
            assert!(shared.len() > 500, "{} shared rows", shared.len());
            let mut parser = StatementParser::new(AccountContext {
                account: AccountId::from_uuid(Uuid::from_u128(1)),
                currency,
            });
            parser.push(&second, &mut |_| {}).unwrap();
            assert!(parser.finish(&mut |_| {}).unwrap().is_clean());
            assert!(lines.last().unwrap().booking_date <= Date::new(2026, 3, 1).unwrap());
        }
    }

    #[test]
    fn gen_v3_is_deterministic_parses_and_labels_every_line() {
        let spec = MatchSpec {
            seed: 7,
            lines: 2_000,
            distractors: 500,
            currency: Currency::Usd,
            start: Date::new(2026, 3, 1).unwrap(),
            days: 28,
        };
        let set = match_set(&spec);
        // Byte-for-byte deterministic, and the seed matters.
        assert_eq!(set, match_set(&spec));
        assert_ne!(
            set.statement,
            match_set(&MatchSpec { seed: 8, ..spec }).statement
        );

        // The statement parses clean.
        let mut parser = StatementParser::new(AccountContext {
            account: AccountId::from_uuid(Uuid::from_u128(1)),
            currency: Currency::Usd,
        });
        let mut lines = vec![];
        parser.push(&set.statement, &mut |l| lines.push(l)).unwrap();
        let summary = parser.finish(&mut |l| lines.push(l)).unwrap();
        assert!(summary.is_clean(), "{:?}", summary.refusals);
        assert_eq!(lines.len(), 2_000);

        // The documents parse clean under the builtin profile (including the
        // foreign-currency twins, which parse as documents of their own currency).
        let mut parser = DocumentParser::with_profile(
            recon_import::DocumentContext {
                client: recon_domain::ClientId::from_uuid(Uuid::from_u128(2)),
            },
            std::sync::Arc::new(recon_import::CompiledDocuments::builtin()),
            recon_import::CsvLimits::default(),
        );
        let mut docs = vec![];
        parser.push(&set.documents, &mut |d| docs.push(d)).unwrap();
        let summary = parser.finish(&mut |d| docs.push(d)).unwrap();
        assert!(summary.is_clean(), "{:?}", summary.refusals);

        // The truth names one expected outcome per line, and every document it names
        // exists in the documents file (which adds exactly the distractors besides).
        let numbers: std::collections::HashSet<String> =
            docs.iter().filter_map(|d| d.number_norm.clone()).collect();
        let text = String::from_utf8(set.truth.clone()).unwrap();
        let mut named_docs = 0_usize;
        let mut counts = std::collections::BTreeMap::new();
        for (n, row) in text.lines().skip(1).enumerate() {
            let fields: Vec<&str> = row.split(',').collect();
            assert_eq!(
                fields[0].parse::<u32>().unwrap(),
                u32::try_from(n).unwrap() + 1
            );
            *counts.entry(fields[1].to_owned()).or_insert(0_u32) += 1;
            for number in fields[2].split('+').filter(|n| !n.is_empty()) {
                named_docs += 1;
                assert!(numbers.contains(number), "line {n}: unknown doc {number}");
            }
        }
        assert_eq!(docs.len(), named_docs + 500);
        eprintln!("gen-v3 bucket mix: {counts:?}");
        // The D-027 mix holds within a couple of points at this size.
        let share = |k: &str| f64::from(counts.get(k).copied().unwrap_or(0)) / 2_000.0;
        assert!(share("t1") > 0.65 && share("t1") < 0.75, "{counts:?}");
        assert!(share("t2") > 0.10 && share("t2") < 0.20, "{counts:?}");
        assert!(share("t3") > 0.02, "{counts:?}");
        assert!(share("t4") > 0.02, "{counts:?}");
        assert!(share("no_candidate") > 0.0, "{counts:?}");
        assert!(share("multiple_candidates") > 0.0, "{counts:?}");
    }
}
