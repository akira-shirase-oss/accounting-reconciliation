//! Named refusals for imports (D-010): every refused file or row says why with one of
//! these stable codes, and reports carry codes and row numbers only — never the content
//! (D-025).

use serde::{Deserialize, Serialize};

use crate::date::DateError;
use crate::money::MoneyError;

macro_rules! codes {
    ($($(#[$doc:meta])* $variant:ident => $code:literal,)+) => {
        /// Why a file or a row was refused.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum RefusalCode {
            $($(#[$doc])* $variant,)+
        }

        impl RefusalCode {
            /// Every code.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The stable code.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $code,)+
                }
            }
        }
    };
}

codes! {
    // File-level refusals.
    /// The upload exceeds 50 MiB.
    FileTooLarge => "file_too_large",
    /// More than 2,000,000 data rows.
    TooManyRows => "too_many_rows",
    /// No header, or a header and no rows.
    EmptyFile => "empty_file",
    /// An encoding this tool does not read (UTF-32, a byte-order mark the profile's
    /// encoding contradicts, an OFX charset outside UTF-8, US-ASCII and Windows-1252).
    UnsupportedEncoding => "unsupported_encoding",
    /// The header is not exactly the profile's columns in the profile's order.
    HeaderMismatch => "header_mismatch",
    /// The parse took longer than 60 s.
    ParseDeadlineExceeded => "parse_deadline_exceeded",
    /// Day-month or month-day dates in a profile that does not declare which (`01/02/2026`):
    /// a question for the user, never a guess (D-010).
    AmbiguousDateFormat => "ambiguous_date_format",
    /// OFX markup that declares or comments (`<!DOCTYPE`, `<!ENTITY`, `<!--`): refused unread.
    DeclarationRefused => "declaration_refused",
    /// A reference to an entity other than the five XML predefined ones or a character
    /// reference: nothing is expanded or resolved.
    EntityRefused => "entity_refused",
    /// OFX that is not a well-formed statement (header, tags, nesting or a missing part).
    MalformedOfx => "malformed_ofx",
    /// More than one account statement in one OFX file.
    MultipleStatements => "multiple_statements",
    // Structural row refusals (the CSV layer).
    /// A field is not valid UTF-8.
    InvalidUtf8 => "invalid_utf8",
    /// A control, byte-order-mark, bidirectional or invisible format character in a field.
    ForbiddenCharacter => "forbidden_character",
    /// A record longer than 64 KiB.
    LineTooLong => "line_too_long",
    /// A field longer than 8 KiB.
    FieldTooLong => "field_too_long",
    /// More than 256 fields.
    TooManyFields => "too_many_fields",
    /// Not the header's number of fields.
    FieldCountMismatch => "field_count_mismatch",
    /// A quoted field still open at the end of the file.
    UnterminatedQuote => "unterminated_quote",
    /// A quote inside an unquoted field, or text after a closing quote.
    StrayQuote => "stray_quote",
    /// A carriage return not followed by a line feed.
    BareCarriageReturn => "bare_carriage_return",
    /// An empty record.
    EmptyRow => "empty_row",
    // Field refusals (the profile).
    /// A required field is empty.
    MissingRequiredField => "missing_required_field",
    /// Not a date in the profile's format, or not a real day.
    InvalidDate => "invalid_date",
    /// A date outside 1990–2100.
    DateOutOfRange => "date_out_of_range",
    /// Not an amount in the profile's number format.
    InvalidAmount => "invalid_amount",
    /// More decimal places than the currency has.
    TooManyDecimalPlaces => "too_many_decimal_places",
    /// Beyond ±10^12 minor units.
    AmountOutOfRange => "amount_out_of_range",
    /// A zero amount (nothing to reconcile; refused rather than guessed at).
    ZeroAmount => "zero_amount",
    /// A currency outside the D-005 table.
    UnknownCurrency => "unknown_currency",
    /// The row's currency is not the account's.
    CurrencyMismatch => "currency_mismatch",
    /// The running balance is not a valid amount.
    InvalidBalance => "invalid_balance",
    /// The running balance does not continue the previous one by this row's amount.
    BalanceDiscontinuity => "balance_discontinuity",
    /// A text field that a spreadsheet would run as a formula (it starts with `=` or `@`).
    FormulaCell => "formula_cell",
    /// Both the money-out and the money-in column carry an amount.
    DebitAndCredit => "debit_and_credit",
    /// A sign where the profile declares magnitudes (a money-out, money-in or by-kind column).
    UnexpectedSign => "unexpected_sign",
    /// A document kind outside the five the schema knows.
    InvalidDocumentKind => "invalid_document_kind",
    /// A document number already used by another document of the same kind and
    /// counterparty, in this file or already imported.
    DocumentNumberConflict => "document_number_conflict",
}

impl From<MoneyError> for RefusalCode {
    fn from(e: MoneyError) -> Self {
        match e {
            MoneyError::Empty => Self::MissingRequiredField,
            MoneyError::Invalid => Self::InvalidAmount,
            MoneyError::TooManyDecimals => Self::TooManyDecimalPlaces,
            MoneyError::OutOfRange => Self::AmountOutOfRange,
            MoneyError::CurrencyMismatch => Self::CurrencyMismatch,
            MoneyError::UnknownCurrency => Self::UnknownCurrency,
        }
    }
}

impl From<DateError> for RefusalCode {
    fn from(e: DateError) -> Self {
        match e {
            DateError::Invalid => Self::InvalidDate,
            DateError::OutOfRange => Self::DateOutOfRange,
        }
    }
}

/// One refused row: its 1-based record number in the file (the header is row 1) and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowRefusal {
    /// The record number.
    pub row: u32,
    /// The reason.
    pub code: RefusalCode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_snake_case_and_match_serde() {
        let mut seen = std::collections::HashSet::new();
        for code in RefusalCode::ALL {
            let s = code.as_str();
            assert!(seen.insert(s), "duplicate {s}");
            assert!(
                s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{s}"
            );
            let json = serde_json_like(*code);
            assert_eq!(json, s);
        }
    }

    // serde's snake_case renaming must equal as_str(); checked without serde_json by a
    // tiny serializer stand-in: the variant name lowered with underscores.
    fn serde_json_like(code: RefusalCode) -> String {
        let name = format!("{code:?}");
        let mut out = String::new();
        for (i, c) in name.chars().enumerate() {
            if c.is_ascii_uppercase() {
                if i > 0 {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }
}
