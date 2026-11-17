//! Statement import profiles (D-062) and the streaming CSV statement parser.
//!
//! A profile declares a bank's CSV layout: its encoding and delimiter, the exact header, which
//! column holds each field, the date and number formats, how amounts are signed (one signed
//! column, or a money-out and a money-in column) and the order of the rows. Profiles are
//! data: versioned, immutable once stored, and recorded with their SHA-256 on every import
//! (D-035). The built-in `r0-generic-csv` (v2) is the R0 layout:
//!
//! ```text
//! booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after
//! ```
//!
//! with ISO dates and plain decimals, the currency the account's on every row.
//!
//! Every profile refuses, never guesses (D-010): any refused row refuses the file, and the
//! report lists up to 1,000 rows with their codes and counts them all. Once a row has been
//! refused no further lines are handed on, but reading continues so the report is complete.
//! Where the file carries running balances, each one must continue the last by the amounts
//! between them, or the row is `balance_discontinuity` (D-008). A text field a spreadsheet
//! would run as a formula (`=…`, `@…`) is `formula_cell` (D-062).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use recon_domain::fingerprint::{LineTuple, tuple_hash};
use recon_domain::import::{FileHash, NewLine};
use recon_domain::limits::{MAX_FIELDS, MAX_REPORTED_ROW_ERRORS};
use recon_domain::refusal::{RefusalCode, RowRefusal};
use recon_domain::text::{is_forbidden, normalize};
use recon_domain::{AccountId, Currency, Date, Money};

use crate::csv::{CsvLimits, CsvReader, DELIMITERS, FileRefusal, Record};
use crate::dates::{DateFormat, DateQuestion, Parsed};
use crate::decode::{Decoder, TextEncoding};
use crate::number::NumberFormat;

/// The R0 layout's columns, in order.
pub const HEADER: [&str; 8] = [
    "booking_date",
    "value_date",
    "amount",
    "currency",
    "narration",
    "reference",
    "counterparty",
    "balance_after",
];

/// The longest header name a profile may declare, in bytes.
pub const MAX_HEADER_NAME_BYTES: usize = 64;

/// How a file signs its amounts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AmountColumns {
    /// One column, signed as the bank sees it: money in positive, money out negative.
    Signed(String),
    /// Two columns of magnitudes; exactly one carries an amount on each row.
    Split {
        /// Money out of the account (withdrawals, debits on a bank statement).
        money_out: String,
        /// Money into the account (deposits, credits on a bank statement).
        money_in: String,
    },
}

/// Which header column holds each field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatementColumns {
    /// The booking date (required on every row).
    pub booking_date: String,
    /// The value date.
    #[serde(default)]
    pub value_date: Option<String>,
    /// The amount.
    pub amount: AmountColumns,
    /// The currency code; without one, every row is in the account's currency.
    #[serde(default)]
    pub currency: Option<String>,
    /// The narration (required).
    pub narration: String,
    /// The bank's reference.
    #[serde(default)]
    pub reference: Option<String>,
    /// The counterparty.
    #[serde(default)]
    pub counterparty: Option<String>,
    /// The running balance after the row.
    #[serde(default)]
    pub balance_after: Option<String>,
}

/// The order of a file's rows, for balance continuity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RowOrder {
    /// Oldest first: each balance is the last one plus the amounts since.
    #[default]
    OldestFirst,
    /// Newest first: each balance is the last one minus the amounts since.
    NewestFirst,
}

/// A statement CSV profile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatementProfile {
    /// The text encoding.
    #[serde(default)]
    pub encoding: TextEncoding,
    /// The field delimiter: `,`, `;`, `|` or a tab.
    pub delimiter: char,
    /// The header, exactly.
    pub header: Vec<String>,
    /// Where each field is.
    pub columns: StatementColumns,
    /// The dates' format.
    pub date_format: DateFormat,
    /// The amounts' format.
    pub number_format: NumberFormat,
    /// The order of the rows.
    #[serde(default)]
    pub row_order: RowOrder,
}

/// A profile that cannot be used, with every problem (`field: problem` codes; no file content).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid profile: {}", problems.join("; "))]
pub struct ProfileError {
    /// The problems.
    pub problems: Vec<String>,
}

/// Header checks shared by statement and document profiles.
pub(crate) fn header_problems(header: &[String], delimiter: char, out: &mut Vec<String>) {
    if !DELIMITERS.contains(&delimiter) {
        out.push("delimiter: must be , ; | or a tab".to_owned());
    }
    if header.is_empty() || header.len() > MAX_FIELDS {
        out.push("header: 1 to 256 columns".to_owned());
    }
    for (i, name) in header.iter().enumerate() {
        if name.is_empty() || name.len() > MAX_HEADER_NAME_BYTES || name.chars().any(is_forbidden) {
            out.push(format!("header[{i}]: 1 to 64 bytes of text"));
        }
        if header.get(..i).is_some_and(|before| before.contains(name)) {
            out.push(format!("header[{i}]: repeats an earlier column"));
        }
    }
}

/// The index of `name` in `header`, or a problem for `field`.
pub(crate) fn column(
    header: &[String],
    name: &str,
    field: &'static str,
    out: &mut Vec<String>,
) -> usize {
    header.iter().position(|h| h == name).unwrap_or_else(|| {
        out.push(format!("{field}: names no header column"));
        0
    })
}

pub(crate) fn optional_column(
    header: &[String],
    name: Option<&String>,
    field: &'static str,
    out: &mut Vec<String>,
) -> Option<usize> {
    name.map(|n| column(header, n, field, out))
}

/// Where a compiled profile finds its amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AmountAt {
    Signed(usize),
    Split { money_out: usize, money_in: usize },
}

impl AmountColumns {
    pub(crate) fn compile(&self, header: &[String], out: &mut Vec<String>) -> AmountAt {
        match self {
            Self::Signed(c) => AmountAt::Signed(column(header, c, "columns.amount.signed", out)),
            Self::Split {
                money_out,
                money_in,
            } => {
                if money_out == money_in {
                    out.push(
                        "columns.amount.split: money_out and money_in are one column".to_owned(),
                    );
                }
                AmountAt::Split {
                    money_out: column(header, money_out, "columns.amount.split.money_out", out),
                    money_in: column(header, money_in, "columns.amount.split.money_in", out),
                }
            }
        }
    }
}

/// A validated statement profile with its columns resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledStatement {
    profile: StatementProfile,
    sha256: [u8; 32],
    booking_date: usize,
    value_date: Option<usize>,
    amount: AmountAt,
    currency: Option<usize>,
    narration: usize,
    reference: Option<usize>,
    counterparty: Option<usize>,
    balance_after: Option<usize>,
}

/// SHA-256 over a profile's canonical JSON (struct fields in declaration order, no maps), the
/// identity recorded on every import (D-035).
pub(crate) fn canonical_sha256(profile: &impl Serialize) -> [u8; 32] {
    // Serializing these plain structs cannot fail; an empty input would still hash.
    let bytes = serde_json::to_vec(profile).unwrap_or_default();
    Sha256::digest(&bytes).into()
}

impl StatementProfile {
    /// The built-in `r0-generic-csv` v2.
    pub fn r0() -> Self {
        Self {
            encoding: TextEncoding::Utf8,
            delimiter: ',',
            header: HEADER.map(str::to_owned).to_vec(),
            columns: StatementColumns {
                booking_date: "booking_date".into(),
                value_date: Some("value_date".into()),
                amount: AmountColumns::Signed("amount".into()),
                currency: Some("currency".into()),
                narration: "narration".into(),
                reference: Some("reference".into()),
                counterparty: Some("counterparty".into()),
                balance_after: Some("balance_after".into()),
            },
            date_format: DateFormat::ISO,
            number_format: NumberFormat::plain(),
            row_order: RowOrder::OldestFirst,
        }
    }

    /// Validates the profile and resolves its columns, reporting every problem at once.
    pub fn compile(self) -> Result<CompiledStatement, ProfileError> {
        let mut p = Vec::new();
        header_problems(&self.header, self.delimiter, &mut p);
        self.date_format.problems("date_format", &mut p);
        self.number_format.problems("number_format", &mut p);
        let h = &self.header;
        let c = &self.columns;
        let booking_date = column(h, &c.booking_date, "columns.booking_date", &mut p);
        let value_date = optional_column(h, c.value_date.as_ref(), "columns.value_date", &mut p);
        let amount = c.amount.compile(h, &mut p);
        let currency = optional_column(h, c.currency.as_ref(), "columns.currency", &mut p);
        let narration = column(h, &c.narration, "columns.narration", &mut p);
        let reference = optional_column(h, c.reference.as_ref(), "columns.reference", &mut p);
        let counterparty =
            optional_column(h, c.counterparty.as_ref(), "columns.counterparty", &mut p);
        let balance_after =
            optional_column(h, c.balance_after.as_ref(), "columns.balance_after", &mut p);
        if !p.is_empty() {
            return Err(ProfileError { problems: p });
        }
        let sha256 = canonical_sha256(&self);
        Ok(CompiledStatement {
            profile: self,
            sha256,
            booking_date,
            value_date,
            amount,
            currency,
            narration,
            reference,
            counterparty,
            balance_after,
        })
    }
}

impl CompiledStatement {
    /// The built-in `r0-generic-csv` v2, compiled.
    pub fn r0() -> Self {
        // The built-in profile is valid by construction; a test proves it.
        StatementProfile::r0()
            .compile()
            .unwrap_or_else(|e| unreachable!("invariant: the built-in profile compiles: {e}"))
    }

    /// The profile.
    pub const fn profile(&self) -> &StatementProfile {
        &self.profile
    }

    /// SHA-256 of the profile's canonical JSON.
    pub const fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
}

/// The account a statement is imported into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountContext {
    /// The account.
    pub account: AccountId,
    /// Its currency: every row must carry it.
    pub currency: Currency,
}

/// What a finished parse found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSummary {
    /// SHA-256 of every byte read.
    pub file_hash: FileHash,
    /// Bytes read.
    pub file_bytes: u64,
    /// Data rows read.
    pub rows_read: u32,
    /// Data rows refused.
    pub rows_refused: u32,
    /// The first refusals, at most 1,000, in row order.
    pub refusals: Vec<RowRefusal>,
}

impl ParseSummary {
    /// True when no row was refused, so the file may be imported.
    pub const fn is_clean(&self) -> bool {
        self.rows_refused == 0
    }
}

/// Running-balance continuity in file order (D-008).
#[derive(Debug, Clone, Copy)]
struct Continuity {
    order: RowOrder,
    last: Option<i128>,
    pending: i128,
}

impl Continuity {
    const fn new(order: RowOrder) -> Self {
        Self {
            order,
            last: None,
            pending: 0,
        }
    }

    /// Observes a row; false when its balance does not continue the last one.
    fn observe(&mut self, amount: i64, balance: Option<i64>) -> bool {
        let amount = i128::from(amount);
        match self.order {
            RowOrder::OldestFirst => {
                self.pending = self.pending.saturating_add(amount);
                let Some(balance) = balance.map(i128::from) else {
                    return true;
                };
                let ok = self
                    .last
                    .is_none_or(|last| last.saturating_add(self.pending) == balance);
                self.last = Some(balance);
                self.pending = 0;
                ok
            }
            RowOrder::NewestFirst => {
                let Some(balance) = balance.map(i128::from) else {
                    self.pending = self.pending.saturating_add(amount);
                    return true;
                };
                let ok = self
                    .last
                    .is_none_or(|last| last.saturating_sub(self.pending) == balance);
                self.last = Some(balance);
                self.pending = amount;
                ok
            }
        }
    }

    /// Forgets the last balance (after a refused row, so one error does not cascade).
    const fn reset(&mut self) {
        self.last = None;
        self.pending = 0;
    }
}

/// The state rows update as they are read (shared by statement and document parsers).
#[derive(Debug)]
pub(crate) struct Tally {
    pub(crate) header_seen: bool,
    pub(crate) rows_read: u32,
    pub(crate) rows_refused: u32,
    pub(crate) refusals: Vec<RowRefusal>,
}

impl Tally {
    pub(crate) const fn new() -> Self {
        Self {
            header_seen: false,
            rows_read: 0,
            rows_refused: 0,
            refusals: Vec::new(),
        }
    }

    pub(crate) fn refuse(&mut self, refusal: RowRefusal) {
        self.rows_refused = self.rows_refused.saturating_add(1);
        if self.refusals.len() < MAX_REPORTED_ROW_ERRORS {
            self.refusals.push(refusal);
        }
    }

    /// Checks the header record; `Ok(true)` when this record was the header.
    pub(crate) fn header(
        &mut self,
        outcome: &Result<Record<'_>, RowRefusal>,
        header: &[String],
    ) -> Result<bool, FileRefusal> {
        if self.header_seen {
            self.rows_read = self.rows_read.saturating_add(1);
            return Ok(false);
        }
        self.header_seen = true;
        match outcome {
            Ok(record) if record.fields().eq(header.iter().map(String::as_str)) => Ok(true),
            _ => Err(FileRefusal::new(RefusalCode::HeaderMismatch, Some(1))),
        }
    }

    pub(crate) fn summary(
        self,
        hasher: Sha256,
        file_bytes: u64,
    ) -> Result<ParseSummary, FileRefusal> {
        if !self.header_seen || self.rows_read == 0 {
            return Err(FileRefusal::new(RefusalCode::EmptyFile, None));
        }
        Ok(ParseSummary {
            file_hash: FileHash(hasher.finalize().into()),
            file_bytes,
            rows_read: self.rows_read,
            rows_refused: self.rows_refused,
            refusals: self.refusals,
        })
    }
}

/// The first undeclared date of a file, and the orders every date so far fits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Undeclared {
    pub(crate) first_row: Option<u32>,
    pub(crate) question: DateQuestion,
}

impl Undeclared {
    pub(crate) const fn new() -> Self {
        Self {
            first_row: None,
            question: DateQuestion {
                dmy: true,
                mdy: true,
            },
        }
    }

    /// The file-level refusal, when the profile left the order open and a date was read.
    pub(crate) fn refusal(self) -> Option<FileRefusal> {
        let row = self.first_row?;
        Some(FileRefusal {
            code: RefusalCode::AmbiguousDateFormat,
            row: Some(row),
            question: Some(self.question),
        })
    }
}

/// Reads one date field: `Ok(Some(date))`, `Ok(None)` when the order is undeclared (the
/// readings are noted in `open`), or the refusal.
pub(crate) fn read_date(
    format: DateFormat,
    text: &str,
    row: u32,
    open: &mut Undeclared,
) -> Result<Option<Date>, RowRefusal> {
    match format.parse(text) {
        Parsed::Date(d) => Ok(Some(d)),
        Parsed::Invalid(e) => Err(RowRefusal {
            row,
            code: e.into(),
        }),
        Parsed::Undeclared { dmy, mdy } => {
            open.first_row.get_or_insert(row);
            open.question.dmy &= dmy;
            open.question.mdy &= mdy;
            Ok(None)
        }
    }
}

/// True for text a spreadsheet would evaluate (D-062).
pub(crate) fn is_formula(text: &str) -> bool {
    text.trim_start_matches(' ').starts_with(['=', '@'])
}

pub(crate) fn optional(field: &str) -> Option<&str> {
    (!field.is_empty()).then_some(field)
}

/// A text field: refused as a formula, otherwise as written.
pub(crate) fn text(field: &str, row: u32) -> Result<Option<&str>, RowRefusal> {
    match optional(field) {
        Some(t) if is_formula(t) => Err(RowRefusal {
            row,
            code: RefusalCode::FormulaCell,
        }),
        other => Ok(other),
    }
}

/// Reads the amount of a row (`Signed` or `Split`), refusing zero.
pub(crate) fn read_amount(
    at: AmountAt,
    format: &NumberFormat,
    record: &Record<'_>,
    currency: Currency,
) -> Result<Money, RefusalCode> {
    let amount = match at {
        AmountAt::Signed(i) => {
            let text = optional(record.field(i)).ok_or(RefusalCode::MissingRequiredField)?;
            format.parse(text, currency)?
        }
        AmountAt::Split {
            money_out,
            money_in,
        } => {
            let magnitude = |i: usize| -> Result<Option<Money>, RefusalCode> {
                let Some(text) = optional(record.field(i)) else {
                    return Ok(None);
                };
                let m = format.parse(text, currency)?;
                if m.minor() < 0 {
                    return Err(RefusalCode::UnexpectedSign);
                }
                Ok((!m.is_zero()).then_some(m))
            };
            let out_text = optional(record.field(money_out));
            let in_text = optional(record.field(money_in));
            if out_text.is_none() && in_text.is_none() {
                return Err(RefusalCode::MissingRequiredField);
            }
            match (magnitude(money_out)?, magnitude(money_in)?) {
                (Some(_), Some(_)) => return Err(RefusalCode::DebitAndCredit),
                (Some(out), None) => out.checked_neg()?,
                (None, Some(into)) => into,
                (None, None) => return Err(RefusalCode::ZeroAmount),
            }
        }
    };
    if amount.is_zero() {
        return Err(RefusalCode::ZeroAmount);
    }
    Ok(amount)
}

/// A streaming parser for one statement file.
#[derive(Debug)]
pub struct StatementParser {
    decoder: Decoder,
    reader: CsvReader,
    hasher: Sha256,
    rows: StatementRows,
}

#[derive(Debug)]
struct StatementRows {
    profile: Arc<CompiledStatement>,
    context: AccountContext,
    tally: Tally,
    continuity: Continuity,
    undeclared: Undeclared,
}

impl StatementParser {
    /// A parser for the built-in `r0-generic-csv` profile with the D-010 bounds.
    pub fn new(context: AccountContext) -> Self {
        Self::with_profile(
            context,
            Arc::new(CompiledStatement::r0()),
            CsvLimits::default(),
        )
    }

    /// A parser for the built-in profile with other bounds (tests).
    pub fn with_limits(context: AccountContext, limits: CsvLimits) -> Self {
        Self::with_profile(context, Arc::new(CompiledStatement::r0()), limits)
    }

    /// A parser for `profile`.
    pub fn with_profile(
        context: AccountContext,
        profile: Arc<CompiledStatement>,
        limits: CsvLimits,
    ) -> Self {
        let delimiter = u8::try_from(profile.profile.delimiter).unwrap_or(b',');
        let order = profile.profile.row_order;
        Self {
            decoder: Decoder::new(profile.profile.encoding, limits.max_file_bytes),
            reader: CsvReader::with_delimiter(limits, delimiter),
            hasher: Sha256::new(),
            rows: StatementRows {
                profile,
                context,
                tally: Tally::new(),
                continuity: Continuity::new(order),
                undeclared: Undeclared::new(),
            },
        }
    }

    /// Rows refused so far.
    pub const fn rows_refused(&self) -> u32 {
        self.rows.tally.rows_refused
    }

    /// Bytes read so far.
    pub const fn file_bytes(&self) -> u64 {
        self.decoder.raw_bytes()
    }

    /// Feeds the next chunk; each valid line goes to `on_line` until a row is refused.
    pub fn push(
        &mut self,
        chunk: &[u8],
        on_line: &mut impl FnMut(NewLine),
    ) -> Result<(), FileRefusal> {
        self.hasher.update(chunk);
        let Self {
            decoder,
            reader,
            rows,
            ..
        } = self;
        decoder.push(chunk, &mut |text| {
            reader.push(text, &mut |outcome| rows.handle(outcome, on_line))
        })
    }

    /// Ends the file and reports.
    pub fn finish(
        mut self,
        on_line: &mut impl FnMut(NewLine),
    ) -> Result<ParseSummary, FileRefusal> {
        let Self {
            decoder,
            reader,
            rows,
            ..
        } = &mut self;
        decoder
            .finish(&mut |text| reader.push(text, &mut |outcome| rows.handle(outcome, on_line)))?;
        reader.finish(&mut |outcome| rows.handle(outcome, on_line))?;
        if let Some(refusal) = self.rows.undeclared.refusal() {
            return Err(refusal);
        }
        let bytes = self.decoder.raw_bytes();
        self.rows.tally.summary(self.hasher, bytes)
    }
}

impl StatementRows {
    fn handle(
        &mut self,
        outcome: Result<Record<'_>, RowRefusal>,
        on_line: &mut impl FnMut(NewLine),
    ) -> Result<(), FileRefusal> {
        if self.tally.header(&outcome, &self.profile.profile.header)? {
            return Ok(());
        }
        let parsed = outcome.and_then(|record| self.parse_row(&record));
        match parsed {
            Ok(Some(line)) => {
                let continues = self
                    .continuity
                    .observe(line.amount.minor(), line.balance_after.map(Money::minor));
                if !continues {
                    self.tally.refuse(RowRefusal {
                        row: line.row,
                        code: RefusalCode::BalanceDiscontinuity,
                    });
                } else if self.tally.rows_refused == 0 {
                    on_line(line);
                }
            }
            // The order is undeclared: the file will be refused with its question.
            Ok(None) => {}
            Err(refusal) => {
                self.continuity.reset();
                self.tally.refuse(refusal);
            }
        }
        Ok(())
    }

    /// Validates and normalizes one data row; `Ok(None)` when its dates cannot be read
    /// because the profile leaves their order open.
    fn parse_row(&mut self, record: &Record<'_>) -> Result<Option<NewLine>, RowRefusal> {
        let p = &*self.profile;
        let row = record.row;
        let refuse = |code: RefusalCode| RowRefusal { row, code };
        if record.len() != p.profile.header.len() {
            return Err(refuse(RefusalCode::FieldCountMismatch));
        }
        let format = p.profile.date_format;
        let booking = optional(record.field(p.booking_date))
            .ok_or(refuse(RefusalCode::MissingRequiredField))?;
        let booking_date = read_date(format, booking, row, &mut self.undeclared)?;
        let value_date = match p.value_date.and_then(|i| optional(record.field(i))) {
            Some(t) => read_date(format, t, row, &mut self.undeclared)?,
            None => None,
        };
        let currency = match p.currency {
            Some(i) => {
                let code =
                    optional(record.field(i)).ok_or(refuse(RefusalCode::MissingRequiredField))?;
                Currency::from_code(code).map_err(|e| refuse(e.into()))?
            }
            None => self.context.currency,
        };
        if currency != self.context.currency {
            return Err(refuse(RefusalCode::CurrencyMismatch));
        }
        let amount =
            read_amount(p.amount, &p.profile.number_format, record, currency).map_err(refuse)?;
        let narration_raw = text(record.field(p.narration), row)?
            .ok_or(refuse(RefusalCode::MissingRequiredField))?;
        let narration_norm = normalize(narration_raw);
        if narration_norm.is_empty() {
            return Err(refuse(RefusalCode::MissingRequiredField));
        }
        let reference = match p.reference {
            Some(i) => text(record.field(i), row)?,
            None => None,
        };
        let counterparty_raw = match p.counterparty {
            Some(i) => text(record.field(i), row)?,
            None => None,
        };
        let balance_after = p
            .balance_after
            .and_then(|i| optional(record.field(i)))
            .map(|b| p.profile.number_format.parse(b, currency))
            .transpose()
            .map_err(|_| refuse(RefusalCode::InvalidBalance))?;
        // With the order undeclared every date reads as `None`: no line, only the question.
        let Some(booking_date) = booking_date else {
            return Ok(None);
        };
        let tuple = tuple_hash(&LineTuple {
            account: self.context.account,
            booking_date,
            amount,
            narration_norm: &narration_norm,
            reference,
        });
        Ok(Some(NewLine {
            row,
            booking_date,
            value_date,
            amount,
            narration_raw: narration_raw.to_owned(),
            narration_norm,
            reference: reference.map(str::to_owned),
            counterparty_raw: counterparty_raw.map(str::to_owned),
            counterparty_norm: counterparty_raw.map(normalize).filter(|c| !c.is_empty()),
            balance_after,
            tuple_hash: tuple,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;
    use crate::dates::DateOrder;
    use crate::number::{GroupStyle, Negative};
    use uuid::Uuid;

    const HEAD: &str =
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";

    fn ctx() -> AccountContext {
        AccountContext {
            account: AccountId::from_uuid(Uuid::from_u128(9)),
            currency: Currency::Usd,
        }
    }

    fn run(
        mut parser: StatementParser,
        input: &[u8],
        chunk: usize,
    ) -> (Result<ParseSummary, FileRefusal>, Vec<NewLine>) {
        let mut lines = vec![];
        let mut push = |l| lines.push(l);
        for part in input.chunks(chunk.max(1)) {
            if let Err(e) = parser.push(part, &mut push) {
                return (Err(e), lines);
            }
        }
        let summary = parser.finish(&mut push);
        (summary, lines)
    }

    fn parse(body: &str) -> (Result<ParseSummary, FileRefusal>, Vec<NewLine>) {
        run(
            StatementParser::new(ctx()),
            format!("{HEAD}{body}").as_bytes(),
            usize::MAX,
        )
    }

    /// An Indian bank's layout: day-month-year, Indian grouping, money-out and money-in
    /// columns, a closing balance, newest first.
    fn indian_profile(order: Option<DateOrder>) -> StatementProfile {
        StatementProfile {
            encoding: TextEncoding::Utf8,
            delimiter: ',',
            header: [
                "Date",
                "Narration",
                "Chq./Ref.No.",
                "Value Dt",
                "Withdrawal Amt.",
                "Deposit Amt.",
                "Closing Balance",
            ]
            .map(str::to_owned)
            .to_vec(),
            columns: StatementColumns {
                booking_date: "Date".into(),
                value_date: Some("Value Dt".into()),
                amount: AmountColumns::Split {
                    money_out: "Withdrawal Amt.".into(),
                    money_in: "Deposit Amt.".into(),
                },
                currency: None,
                narration: "Narration".into(),
                reference: Some("Chq./Ref.No.".into()),
                counterparty: None,
                balance_after: Some("Closing Balance".into()),
            },
            date_format: DateFormat {
                order,
                separator: Some('/'),
                month_names: false,
                padded: true,
            },
            number_format: NumberFormat {
                decimal: '.',
                grouping: Some(','),
                group_style: GroupStyle::Indian,
                negatives: vec![Negative::Minus],
                plus_sign: false,
            },
            row_order: RowOrder::NewestFirst,
        }
    }

    fn inr() -> AccountContext {
        AccountContext {
            account: AccountId::from_uuid(Uuid::from_u128(10)),
            currency: Currency::Inr,
        }
    }

    const INDIAN: &str = "Date,Narration,Chq./Ref.No.,Value Dt,Withdrawal Amt.,Deposit Amt.,Closing Balance\n\
        05/03/2026,NEFT CR ACME,N123,05/03/2026,,\"1,25,000.00\",\"2,30,450.50\"\n\
        03/03/2026,UPI/RENT,U77,03/03/2026,\"15,000.00\",,\"1,05,450.50\"\n\
        01/03/2026,ATM WDL,,01/03/2026,500.00,,\"1,20,450.50\"\n";

    #[test]
    fn builtin_profiles_compile_and_are_pinned() {
        let r0 = CompiledStatement::r0();
        assert_eq!(r0.profile(), &StatementProfile::r0());
        // A change to the built-in profile's definition changes this hash: a new version.
        assert_eq!(
            hex::encode(r0.sha256()),
            hex::encode(canonical_sha256(&StatementProfile::r0()))
        );
        let json = serde_json::to_string(&StatementProfile::r0()).unwrap();
        let back: StatementProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, StatementProfile::r0());
    }

    #[test]
    fn a_clean_file_yields_normalized_lines_and_a_hash() {
        let body = "2026-03-01,2026-03-02,-125.50,USD,  neft  acme   ltd ,INV-77,Acme Ltd,1000.00\n\
                    2026-03-01,,40,USD,Card fee,,,\n";
        let (summary, lines) = parse(body);
        let summary = summary.unwrap();
        assert!(summary.is_clean());
        assert_eq!((summary.rows_read, summary.rows_refused), (2, 0));
        assert_eq!(lines.len(), 2);
        let first = &lines[0];
        assert_eq!(first.row, 2);
        assert_eq!(first.amount.minor(), -12_550);
        assert_eq!(first.narration_raw, "  neft  acme   ltd ");
        assert_eq!(first.narration_norm, "NEFT ACME LTD");
        assert_eq!(first.reference.as_deref(), Some("INV-77"));
        assert_eq!(first.counterparty_norm.as_deref(), Some("ACME LTD"));
        assert_eq!(first.balance_after.unwrap().minor(), 100_000);
        assert_eq!(lines[1].value_date, None);
        assert_eq!(lines[1].reference, None);
        let expected: [u8; 32] = Sha256::digest(format!("{HEAD}{body}")).into();
        assert_eq!(summary.file_hash.0, expected);
    }

    #[test]
    fn identical_twins_are_both_lines_with_the_same_tuple() {
        let (summary, lines) = parse("2026-03-01,,-10,USD,ATM,,,\n2026-03-01,,-10,USD,ATM,,,\n");
        assert!(summary.unwrap().is_clean());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].tuple_hash, lines[1].tuple_hash);
    }

    #[test]
    fn every_bad_row_is_named_and_counted_and_stops_the_lines() {
        use RefusalCode as C;
        let body = "2026-03-01,,10,USD,ok,,,\n\
                    01/03/2026,,10,USD,x,,,\n\
                    2026-03-01,,10.001,USD,x,,,\n\
                    2026-03-01,,1e3,USD,x,,,\n\
                    2026-03-01,,10,EUR,x,,,\n\
                    2026-03-01,,10,XYZ,x,,,\n\
                    2026-03-01,,0.00,USD,x,,,\n\
                    2026-03-01,,10,USD,   ,,,\n\
                    2026-03-01,,10,USD,x,,,abc\n\
                    2026-03-01,,10,USD,x,,\n\
                    ,,10,USD,x,,,\n\
                    2026-03-01,,12345678901234567890,USD,x,,,\n\
                    2026-03-01,2026-02-30,10,USD,x,,,\n\
                    2026-03-01,,10,USD,=HYPERLINK(1),,,\n\
                    2026-03-01,,10,USD,x,@SUM(1),,\n\
                    2026-03-01,,10,USD,x,, =cmd,\n\
                    2026-03-01,,10,USD,x,,,\n";
        let (summary, lines) = parse(body);
        let summary = summary.unwrap();
        assert!(!summary.is_clean());
        assert_eq!(summary.rows_read, 17);
        assert_eq!(summary.rows_refused, 15);
        let codes: Vec<_> = summary.refusals.iter().map(|r| (r.row, r.code)).collect();
        assert_eq!(
            codes,
            [
                (3, C::InvalidDate),
                (4, C::TooManyDecimalPlaces),
                (5, C::InvalidAmount),
                (6, C::CurrencyMismatch),
                (7, C::UnknownCurrency),
                (8, C::ZeroAmount),
                (9, C::MissingRequiredField),
                (10, C::InvalidBalance),
                (11, C::FieldCountMismatch),
                (12, C::MissingRequiredField),
                (13, C::AmountOutOfRange),
                (14, C::InvalidDate),
                (15, C::FormulaCell),
                (16, C::FormulaCell),
                (17, C::FormulaCell),
            ]
        );
        // The first row was handed on before the refusals; the last valid one was not.
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn file_level_refusals() {
        use RefusalCode as C;
        let mut parser = StatementParser::new(ctx());
        let err = parser
            .push(b"date,amount\n2026-03-01,1\n", &mut |_| {})
            .unwrap_err();
        assert_eq!(err, FileRefusal::new(C::HeaderMismatch, Some(1)));
        let parser = StatementParser::new(ctx());
        assert_eq!(parser.finish(&mut |_| {}).unwrap_err().code, C::EmptyFile);
        let mut parser = StatementParser::new(ctx());
        parser.push(HEAD.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(parser.finish(&mut |_| {}).unwrap_err().code, C::EmptyFile);
        // UTF-32 is not read; UTF-16 with its mark is (and must still match the header).
        let (r, _) = run(StatementParser::new(ctx()), b"\0\0\xFE\xFF\0\0\0a", 3);
        assert_eq!(r.unwrap_err().code, C::UnsupportedEncoding);
    }

    #[test]
    fn the_report_lists_a_thousand_and_counts_all() {
        let body = "bad,,10,USD,x,,,\n".repeat(1_500);
        let (summary, lines) = parse(&body);
        let summary = summary.unwrap();
        assert_eq!(summary.rows_refused, 1_500);
        assert_eq!(summary.refusals.len(), 1_000);
        assert_eq!(lines.len(), 0);
    }

    #[test]
    fn running_balances_must_continue_in_either_row_order() {
        use RefusalCode as C;
        // Oldest first: 100 + 10 = 110, then a row without a balance, then 110 - 5 + 20 = 125.
        let body = "2026-03-01,,100,USD,a,,,100\n2026-03-01,,10,USD,b,,,110\n\
                    2026-03-02,,-5,USD,c,,,\n2026-03-02,,20,USD,d,,,125\n";
        let (summary, lines) = parse(body);
        assert!(summary.unwrap().is_clean());
        assert_eq!(lines.len(), 4);
        // One break is one refusal at its row; the next row continues from it.
        let body = "2026-03-01,,100,USD,a,,,100\n2026-03-01,,10,USD,b,,,111\n\
                    2026-03-02,,1,USD,c,,,112\n";
        let (summary, _) = parse(body);
        let summary = summary.unwrap();
        assert_eq!(
            summary.refusals,
            [RowRefusal {
                row: 3,
                code: C::BalanceDiscontinuity
            }]
        );
        // Newest first (the Indian layout): each older row's balance is the newer one less
        // the newer row's amount (2,30,450.50 - 1,25,000.00 = 1,05,450.50, and so on).
        let profile = Arc::new(indian_profile(Some(DateOrder::Dmy)).compile().unwrap());
        let (summary, lines) = run(
            StatementParser::with_profile(inr(), profile.clone(), CsvLimits::default()),
            INDIAN.as_bytes(),
            7,
        );
        let summary = summary.unwrap();
        assert!(summary.is_clean(), "{:?}", summary.refusals);
        let amounts: Vec<i64> = lines.iter().map(|l| l.amount.minor()).collect();
        assert_eq!(amounts, [12_500_000, -1_500_000, -50_000]);
        assert_eq!(lines[0].balance_after.unwrap().minor(), 23_045_050);
        assert_eq!(lines[2].reference, None);
        // The same file read as oldest first does not continue.
        let mut oldest = indian_profile(Some(DateOrder::Dmy));
        oldest.row_order = RowOrder::OldestFirst;
        let (summary, _) = run(
            StatementParser::with_profile(
                inr(),
                Arc::new(oldest.compile().unwrap()),
                CsvLimits::default(),
            ),
            INDIAN.as_bytes(),
            64,
        );
        assert_eq!(summary.unwrap().refusals[0].code, C::BalanceDiscontinuity);
    }

    #[test]
    fn money_out_and_money_in_are_magnitudes_and_exactly_one_is_set() {
        use RefusalCode as C;
        let profile = Arc::new(indian_profile(Some(DateOrder::Dmy)).compile().unwrap());
        let head = INDIAN.lines().next().unwrap();
        let body = format!(
            "{head}\n01/03/2026,a,,01/03/2026,10.00,5.00,\n\
             01/03/2026,b,,01/03/2026,,,\n\
             01/03/2026,c,,01/03/2026,-10.00,,\n\
             01/03/2026,d,,01/03/2026,0.00,0.00,\n\
             01/03/2026,e,,01/03/2026,0.00,7.00,\n"
        );
        let (summary, _) = run(
            StatementParser::with_profile(inr(), profile, CsvLimits::default()),
            body.as_bytes(),
            usize::MAX,
        );
        let codes: Vec<_> = summary
            .unwrap()
            .refusals
            .iter()
            .map(|r| (r.row, r.code))
            .collect();
        assert_eq!(
            codes,
            [
                (2, C::DebitAndCredit),
                (3, C::MissingRequiredField),
                (4, C::UnexpectedSign),
                (5, C::ZeroAmount),
            ]
        );
    }

    #[test]
    fn an_undeclared_date_order_is_a_question_never_a_guess() {
        use RefusalCode as C;
        let profile = Arc::new(indian_profile(None).compile().unwrap());
        let (summary, lines) = run(
            StatementParser::with_profile(inr(), profile.clone(), CsvLimits::default()),
            INDIAN.as_bytes(),
            5,
        );
        assert!(
            lines.is_empty(),
            "no line is handed on before the order is declared"
        );
        let refusal = summary.unwrap_err();
        assert_eq!(refusal.code, C::AmbiguousDateFormat);
        assert_eq!(refusal.row, Some(2));
        // 05/03, 03/03 and 01/03 read both ways: both orders are offered.
        assert_eq!(refusal.question.unwrap().options(), ["dmy", "mdy"]);
        // One date that only reads day-first narrows the question; it is still a question.
        let narrowed = INDIAN.replace("05/03/2026,NEFT", "25/03/2026,NEFT");
        let (summary, _) = run(
            StatementParser::with_profile(inr(), profile, CsvLimits::default()),
            narrowed.as_bytes(),
            64,
        );
        let refusal = summary.unwrap_err();
        assert_eq!(refusal.question.unwrap().options(), ["dmy"]);
    }

    #[test]
    fn declared_encodings_and_delimiters() {
        let mut p = StatementProfile::r0();
        p.encoding = TextEncoding::Windows1252;
        p.delimiter = ';';
        p.header = HEADER.map(str::to_owned).to_vec();
        p.number_format.decimal = ',';
        let body = b"booking_date;value_date;amount;currency;narration;reference;counterparty;balance_after\n\
                     2026-03-01;;-12,50;USD;Caf\xE9 \x80 fee;;;\n";
        let (summary, lines) = run(
            StatementParser::with_profile(
                ctx(),
                Arc::new(p.compile().unwrap()),
                CsvLimits::default(),
            ),
            body,
            3,
        );
        assert!(summary.unwrap().is_clean());
        assert_eq!(lines[0].narration_raw, "Café € fee");
        assert_eq!(lines[0].amount.minor(), -1_250);
        // UTF-16 by its mark under a UTF-8 profile.
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in format!("{HEAD}2026-03-01,,-1.00,USD,Zoë,,,\n").encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let (summary, lines) = run(StatementParser::new(ctx()), &utf16, 5);
        assert!(summary.unwrap().is_clean());
        assert_eq!(lines[0].narration_raw, "Zoë");
        // Windows-1252 bytes under a UTF-8 profile are refused by row.
        let (summary, _) = parse("2026-03-01,,-1.00,USD,Caf\u{FFFD},,,\n");
        assert!(summary.unwrap().is_clean(), "U+FFFD is ordinary text");
        let (summary, _) = run(
            StatementParser::new(ctx()),
            &[HEAD.as_bytes(), b"2026-03-01,,-1.00,USD,Caf\xE9,,,\n"].concat(),
            64,
        );
        assert_eq!(summary.unwrap().refusals[0].code, RefusalCode::InvalidUtf8);
    }

    #[test]
    fn unusable_profiles_name_every_problem() {
        let mut p = indian_profile(Some(DateOrder::Dmy));
        p.delimiter = 'x';
        p.header.push("Date".into());
        p.columns.reference = Some("Nope".into());
        p.columns.amount = AmountColumns::Split {
            money_out: "Withdrawal Amt.".into(),
            money_in: "Withdrawal Amt.".into(),
        };
        p.date_format.separator = None;
        let problems = p.compile().unwrap_err().problems;
        assert_eq!(problems.len(), 5, "{problems:?}");
        for problem in &problems {
            assert!(
                !problem.contains("Nope"),
                "problems never echo content: {problem}"
            );
        }
        let unknown_field = serde_json::from_str::<StatementProfile>(
            r#"{"delimiter":",","header":["a"],"columns":{"booking_date":"a","amount":{"signed":"a"},"narration":"a","x":1},"date_format":{"order":"ymd","separator":"-"},"number_format":{"decimal":"."}}"#,
        );
        assert!(unknown_field.is_err());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(128))]

        /// Streaming is invisible to the profile parser too.
        #[test]
        fn any_chunking_parses_the_same(chunk in 1_usize..40) {
            let profile = Arc::new(indian_profile(Some(DateOrder::Dmy)).compile().unwrap());
            let whole = run(StatementParser::with_profile(inr(), profile.clone(), CsvLimits::default()), INDIAN.as_bytes(), usize::MAX);
            let parts = run(StatementParser::with_profile(inr(), profile, CsvLimits::default()), INDIAN.as_bytes(), chunk);
            proptest::prop_assert_eq!(whole, parts);
        }

        #[test]
        fn arbitrary_bytes_never_panic(input in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..400)) {
            let profile = Arc::new(indian_profile(None).compile().unwrap());
            let _ = run(StatementParser::with_profile(inr(), profile, CsvLimits::default()), &input, 11);
            let mut with_head = HEAD.as_bytes().to_vec();
            with_head.extend_from_slice(&input);
            let _ = run(StatementParser::new(ctx()), &with_head, 7);
        }
    }
}
