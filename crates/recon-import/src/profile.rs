//! The R0 statement profile `r0-generic-csv` v1 (D-040): one fixed layout, UTF-8, the
//! header exactly
//!
//! ```text
//! booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after
//! ```
//!
//! Dates are `YYYY-MM-DD`; amounts and balances are plain decimals with `.` and at most the
//! currency's places; the currency must be the account's; booking date, amount, currency
//! and narration are required, the rest may be empty. Any refused row refuses the file
//! (D-010); the report lists up to 1,000 of them with their codes and row numbers, and
//! counts them all.
//!
//! Once a row has been refused no further lines are handed on (the file will not be
//! imported), but reading continues so the report is complete.

use sha2::{Digest, Sha256};

use recon_domain::fingerprint::{LineTuple, tuple_hash};
use recon_domain::import::{FileHash, NewLine};
use recon_domain::limits::MAX_REPORTED_ROW_ERRORS;
use recon_domain::refusal::{RefusalCode, RowRefusal};
use recon_domain::text::normalize;
use recon_domain::{AccountId, Currency, Date, Money};

use crate::csv::{CsvLimits, CsvReader, FileRefusal, Record};

/// The profile's columns, in order.
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

/// A streaming parser for one statement file.
#[derive(Debug)]
pub struct StatementParser {
    reader: CsvReader,
    context: AccountContext,
    hasher: Sha256,
    header_seen: bool,
    rows_read: u32,
    rows_refused: u32,
    refusals: Vec<RowRefusal>,
}

impl StatementParser {
    /// A parser with the D-010 bounds.
    pub fn new(context: AccountContext) -> Self {
        Self::with_limits(context, CsvLimits::default())
    }

    /// A parser with other bounds (tests).
    pub fn with_limits(context: AccountContext, limits: CsvLimits) -> Self {
        Self {
            reader: CsvReader::new(limits),
            context,
            hasher: Sha256::new(),
            header_seen: false,
            rows_read: 0,
            rows_refused: 0,
            refusals: Vec::new(),
        }
    }

    /// Rows refused so far.
    pub const fn rows_refused(&self) -> u32 {
        self.rows_refused
    }

    /// Bytes read so far.
    pub const fn file_bytes(&self) -> u64 {
        self.reader.file_bytes()
    }

    /// Feeds the next chunk; each valid line goes to `on_line` until a row is refused.
    pub fn push(
        &mut self,
        chunk: &[u8],
        on_line: &mut impl FnMut(NewLine),
    ) -> Result<(), FileRefusal> {
        self.hasher.update(chunk);
        let Self {
            reader,
            context,
            header_seen,
            rows_read,
            rows_refused,
            refusals,
            ..
        } = self;
        let mut state = RowState {
            context: *context,
            header_seen,
            rows_read,
            rows_refused,
            refusals,
            on_line,
        };
        reader.push(chunk, &mut |outcome| state.handle(outcome))
    }

    /// Ends the file and reports.
    pub fn finish(
        mut self,
        on_line: &mut impl FnMut(NewLine),
    ) -> Result<ParseSummary, FileRefusal> {
        let Self {
            reader,
            context,
            header_seen,
            rows_read,
            rows_refused,
            refusals,
            ..
        } = &mut self;
        let mut state = RowState {
            context: *context,
            header_seen,
            rows_read,
            rows_refused,
            refusals,
            on_line,
        };
        reader.finish(&mut |outcome| state.handle(outcome))?;
        if !self.header_seen || self.rows_read == 0 {
            return Err(FileRefusal {
                code: RefusalCode::EmptyFile,
                row: None,
            });
        }
        Ok(ParseSummary {
            file_hash: FileHash(self.hasher.finalize().into()),
            file_bytes: self.reader.file_bytes(),
            rows_read: self.rows_read,
            rows_refused: self.rows_refused,
            refusals: self.refusals,
        })
    }
}

struct RowState<'s, F> {
    context: AccountContext,
    header_seen: &'s mut bool,
    rows_read: &'s mut u32,
    rows_refused: &'s mut u32,
    refusals: &'s mut Vec<RowRefusal>,
    on_line: &'s mut F,
}

impl<F: FnMut(NewLine)> RowState<'_, F> {
    fn handle(&mut self, outcome: Result<Record<'_>, RowRefusal>) -> Result<(), FileRefusal> {
        if !*self.header_seen {
            *self.header_seen = true;
            return match outcome {
                Ok(record) if record.fields().eq(HEADER) => Ok(()),
                _ => Err(FileRefusal {
                    code: RefusalCode::HeaderMismatch,
                    row: Some(1),
                }),
            };
        }
        *self.rows_read = self.rows_read.saturating_add(1);
        let result = outcome.and_then(|record| parse_row(self.context, &record));
        match result {
            Ok(line) if *self.rows_refused == 0 => (self.on_line)(line),
            Ok(_) => {}
            Err(refusal) => {
                *self.rows_refused = self.rows_refused.saturating_add(1);
                if self.refusals.len() < MAX_REPORTED_ROW_ERRORS {
                    self.refusals.push(refusal);
                }
            }
        }
        Ok(())
    }
}

fn optional(field: &str) -> Option<&str> {
    (!field.is_empty()).then_some(field)
}

/// Validates and normalizes one data row.
pub fn parse_row(context: AccountContext, record: &Record<'_>) -> Result<NewLine, RowRefusal> {
    let row = record.row;
    let refuse = |code: RefusalCode| RowRefusal { row, code };
    if record.len() != HEADER.len() {
        return Err(refuse(RefusalCode::FieldCountMismatch));
    }
    let required =
        |i: usize| optional(record.field(i)).ok_or(refuse(RefusalCode::MissingRequiredField));

    let booking_date = Date::parse_iso(required(0)?).map_err(|e| refuse(e.into()))?;
    let value_date = optional(record.field(1))
        .map(Date::parse_iso)
        .transpose()
        .map_err(|e| refuse(e.into()))?;
    let amount_text = required(2)?;
    let currency = Currency::from_code(required(3)?).map_err(|e| refuse(e.into()))?;
    if currency != context.currency {
        return Err(refuse(RefusalCode::CurrencyMismatch));
    }
    let amount = Money::parse(amount_text, currency).map_err(|e| refuse(e.into()))?;
    if amount.is_zero() {
        return Err(refuse(RefusalCode::ZeroAmount));
    }
    let narration_raw = required(4)?;
    let narration_norm = normalize(narration_raw);
    if narration_norm.is_empty() {
        return Err(refuse(RefusalCode::MissingRequiredField));
    }
    let reference = optional(record.field(5));
    let counterparty_raw = optional(record.field(6));
    let balance_after = optional(record.field(7))
        .map(|b| Money::parse(b, currency))
        .transpose()
        .map_err(|_| refuse(RefusalCode::InvalidBalance))?;

    let tuple = tuple_hash(&LineTuple {
        account: context.account,
        booking_date,
        amount,
        narration_norm: &narration_norm,
        reference,
    });
    Ok(NewLine {
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
    })
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
    use uuid::Uuid;

    const HEAD: &str =
        "booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";

    fn ctx() -> AccountContext {
        AccountContext {
            account: AccountId::from_uuid(Uuid::from_u128(9)),
            currency: Currency::Usd,
        }
    }

    fn parse(body: &str) -> (Result<ParseSummary, FileRefusal>, Vec<NewLine>) {
        let mut lines = vec![];
        let mut parser = StatementParser::new(ctx());
        let input = format!("{HEAD}{body}");
        let mut push = |l| lines.push(l);
        if let Err(e) = parser.push(input.as_bytes(), &mut push) {
            return (Err(e), lines);
        }
        let summary = parser.finish(&mut push);
        (summary, lines)
    }

    #[test]
    fn a_clean_file_yields_normalized_lines_and_a_hash() {
        let (summary, lines) = parse(
            "2026-03-01,2026-03-02,-125.50,USD,  neft  acme   ltd ,INV-77,Acme Ltd,1000.00\n\
             2026-03-01,,40,USD,Card fee,,,\n",
        );
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
        let expected: [u8; 32] = Sha256::digest(format!(
            "{HEAD}2026-03-01,2026-03-02,-125.50,USD,  neft  acme   ltd ,INV-77,Acme Ltd,1000.00\n\
             2026-03-01,,40,USD,Card fee,,,\n"
        ))
        .into();
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
                    2026-03-01,,10,USD,x,,,\n";
        let (summary, lines) = parse(body);
        let summary = summary.unwrap();
        assert!(!summary.is_clean());
        assert_eq!(summary.rows_read, 14);
        assert_eq!(summary.rows_refused, 12);
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
        assert_eq!(
            err,
            FileRefusal {
                code: C::HeaderMismatch,
                row: Some(1)
            }
        );
        let parser = StatementParser::new(ctx());
        assert_eq!(parser.finish(&mut |_| {}).unwrap_err().code, C::EmptyFile);
        let mut parser = StatementParser::new(ctx());
        parser.push(HEAD.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(parser.finish(&mut |_| {}).unwrap_err().code, C::EmptyFile);
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
}
