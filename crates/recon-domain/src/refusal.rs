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
    /// A byte-order mark of an encoding the profile does not declare (UTF-16, UTF-32).
    UnsupportedEncoding => "unsupported_encoding",
    /// The header is not exactly the profile's columns in the profile's order.
    HeaderMismatch => "header_mismatch",
    /// The parse took longer than 60 s.
    ParseDeadlineExceeded => "parse_deadline_exceeded",
    // Structural row refusals (the CSV layer).
    /// A field is not valid UTF-8.
    InvalidUtf8 => "invalid_utf8",
    /// A control, byte-order-mark or bidirectional-override character in a field.
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
    /// Not `YYYY-MM-DD`, or not a real day.
    InvalidDate => "invalid_date",
    /// A date outside 1990–2100.
    DateOutOfRange => "date_out_of_range",
    /// Not a plain decimal amount.
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
