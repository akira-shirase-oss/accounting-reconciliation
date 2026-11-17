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

            /// Parses a stored code.
            pub fn from_code(code: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|c| c.as_str() == code)
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

impl RefusalCode {
    /// What is wrong and how to fix it, in plain words for the person who exported the
    /// file (D-068). A report adds the row, the column's name in the profile and the
    /// profile's formats.
    #[expect(
        clippy::too_many_lines,
        reason = "a table: one arm of plain words per refusal code"
    )]
    pub const fn explain(self) -> (&'static str, &'static str) {
        match self {
            Self::FileTooLarge => (
                "The file is larger than 50 MiB.",
                "Export a shorter period (one month at a time) and upload each file.",
            ),
            Self::TooManyRows => (
                "The file has more than 2,000,000 rows.",
                "Export a shorter period and upload each file.",
            ),
            Self::EmptyFile => (
                "The file has no rows after its header (or no header at all).",
                "Check that the export covers a period with transactions, and upload the file the bank produced, not a blank template.",
            ),
            Self::UnsupportedEncoding => (
                "The file's text encoding is not one this tool reads (or contradicts the profile).",
                "Export again as UTF-8 (\"CSV UTF-8\" in a spreadsheet), or declare \"windows1252\" in the profile if the bank exports that.",
            ),
            Self::HeaderMismatch => (
                "The first line is not the header the profile expects.",
                "Upload the export this profile was written for, unchanged; if the bank changed its layout, create a new profile version with the new header.",
            ),
            Self::ParseDeadlineExceeded => (
                "Reading the file took longer than 60 seconds.",
                "Upload again; if it repeats, split the file into shorter periods.",
            ),
            Self::AmbiguousDateFormat => (
                "The profile does not say whether dates are day-month or month-day, and the tool never guesses.",
                "Create a new version of the profile with \"order\" set in date_format to one of the options given, then upload the file again.",
            ),
            Self::DeclarationRefused => (
                "The OFX file contains a DTD, entity declaration or comment, which this tool never reads.",
                "Download the statement from the bank again; if it repeats, export it as CSV.",
            ),
            Self::EntityRefused => (
                "The OFX file uses an entity other than &amp; &lt; &gt; &quot; &apos; or a bare &.",
                "Download the statement again; if it repeats, export it as CSV.",
            ),
            Self::MalformedOfx => (
                "The OFX file is not a complete, well-formed statement (header, tags or nesting).",
                "Download the statement from the bank again; a file cut short or edited by hand cannot be read.",
            ),
            Self::MultipleStatements => (
                "The OFX file holds statements for more than one account.",
                "Download one account's statement per file and upload each into its own account.",
            ),
            Self::InvalidUtf8 => (
                "This line contains bytes that are not valid text in the file's encoding.",
                "Export again as UTF-8, or declare \"windows1252\" in the profile if the bank exports that.",
            ),
            Self::ForbiddenCharacter => (
                "This line contains an invisible or control character (a tab, a line break inside a field, a zero-width or text-direction mark).",
                "Remove the character in the spreadsheet or export again; these characters can make two identical narrations look different or reorder text on screen.",
            ),
            Self::LineTooLong => (
                "This line is longer than 64 KiB.",
                "Check the line for a field that runs into the next one (a missing quote or delimiter).",
            ),
            Self::FieldTooLong => (
                "A field on this line is longer than 8 KiB.",
                "Check for a missing closing quote, or shorten the text.",
            ),
            Self::TooManyFields => (
                "This line has more than 256 fields.",
                "Check the delimiter: the profile's delimiter may not be the one the file uses.",
            ),
            Self::FieldCountMismatch => (
                "This line has a different number of fields from the header.",
                "Check for an unquoted delimiter inside a narration or a missing field; quote fields that contain the delimiter.",
            ),
            Self::UnterminatedQuote => (
                "A quoted field on this line is never closed: the file ends inside it.",
                "Close the quote or export the file again; it may have been cut short.",
            ),
            Self::StrayQuote => (
                "A quote mark appears inside an unquoted field, or text follows a closing quote.",
                "Quote the whole field and write a quote inside it as two quotes (\"\").",
            ),
            Self::BareCarriageReturn => (
                "A carriage return appears on its own inside this line.",
                "Export again with standard line endings, or remove the stray character.",
            ),
            Self::EmptyRow => ("This line is empty.", "Remove blank lines between rows."),
            Self::MissingRequiredField => (
                "A field this profile requires is empty.",
                "Fill in the field; a row without it cannot be reconciled.",
            ),
            Self::InvalidDate => (
                "The date is not a real day written in the profile's date format.",
                "Correct the date in the file, or the profile's date_format if the bank writes dates differently.",
            ),
            Self::DateOutOfRange => (
                "The date is before 1990 or after 2100.",
                "Correct the date; a year typed short (26 for 2026) is a common cause.",
            ),
            Self::InvalidAmount => (
                "The amount is not a number in the profile's number format.",
                "Remove currency symbols and words, and check the decimal and grouping marks and the negative style against the profile's number_format.",
            ),
            Self::TooManyDecimalPlaces => (
                "The amount has more decimal places than the currency allows.",
                "Write the amount with the currency's places (2 for most, 0 for JPY, 3 for KWD); amounts are never rounded.",
            ),
            Self::AmountOutOfRange => (
                "The amount is larger than 10^12 minor units (10 billion in a 2-place currency).",
                "Check the amount; a number pasted twice or a reference in the amount column is a common cause.",
            ),
            Self::ZeroAmount => (
                "The amount is zero.",
                "Remove the row; a zero-value line has nothing to reconcile.",
            ),
            Self::UnknownCurrency => (
                "The currency code is not one this tool supports.",
                "Use an ISO code in upper case: INR, USD, EUR, GBP, AED, SGD, AUD, CAD, JPY or KWD.",
            ),
            Self::CurrencyMismatch => (
                "The row's currency is not the account's (or the statement's currency is not the account's).",
                "Upload the statement into the account in its currency; foreign-currency rows are not matched in this version.",
            ),
            Self::InvalidBalance => (
                "The running balance is not a number in the profile's number format.",
                "Correct the balance, or check the profile's number_format.",
            ),
            Self::BalanceDiscontinuity => (
                "The running balance does not follow from the previous balance and the amounts between them.",
                "Check for a missing, extra or edited row just above this one; if the bank lists newest first, set row_order to newest_first in the profile.",
            ),
            Self::FormulaCell => (
                "A text field starts with = or @, which a spreadsheet would run as a formula.",
                "Remove the leading = or @ (a bank export never needs it), then upload again.",
            ),
            Self::DebitAndCredit => (
                "Both the money-out and the money-in column have an amount on this row.",
                "Keep the amount in one column only; a row is either a payment or a receipt.",
            ),
            Self::UnexpectedSign => (
                "A sign appears where the profile expects a plain amount (money-out, money-in, or an amount signed by document kind).",
                "Write the amount without a sign; the column (or the document kind) already says which way the money went.",
            ),
            Self::InvalidDocumentKind => (
                "The document kind is not one of sales_invoice, purchase_bill, receipt, credit_note or book_entry (or a book entry is in a profile that signs amounts by kind).",
                "Use one of the five codes, or import book entries with a signed profile.",
            ),
            Self::DocumentNumberConflict => (
                "This document number is already used by another document of the same kind and counterparty, in this file or already imported.",
                "If this is a corrected copy, keep only one version in the file; if it is a different document, give it its own number.",
            ),
        }
    }
}

/// Which field of a row a refusal concerns; a report names it by the profile's column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// The booking date.
    BookingDate,
    /// The value date.
    ValueDate,
    /// The signed amount.
    Amount,
    /// The money-out column.
    MoneyOut,
    /// The money-in column.
    MoneyIn,
    /// The currency.
    Currency,
    /// The narration.
    Narration,
    /// The bank's reference.
    Reference,
    /// The counterparty.
    Counterparty,
    /// The running balance.
    BalanceAfter,
    /// A document's kind.
    Kind,
    /// A document's number.
    Number,
    /// A document's issue date.
    IssueDate,
    /// A document's due date.
    DueDate,
    /// A document's description.
    Description,
}

impl Field {
    /// Plain words for the field.
    pub const fn label(self) -> &'static str {
        match self {
            Self::BookingDate => "booking date",
            Self::ValueDate => "value date",
            Self::Amount => "amount",
            Self::MoneyOut => "money out",
            Self::MoneyIn => "money in",
            Self::Currency => "currency",
            Self::Narration => "narration",
            Self::Reference => "reference",
            Self::Counterparty => "counterparty",
            Self::BalanceAfter => "running balance",
            Self::Kind => "document kind",
            Self::Number => "document number",
            Self::IssueDate => "issue date",
            Self::DueDate => "due date",
            Self::Description => "description",
        }
    }
}

/// One refused row: its 1-based record number in the file (the header is row 1; every
/// record is one line, since line breaks inside fields are refused), why, and the field
/// when one field is at fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowRefusal {
    /// The record number.
    pub row: u32,
    /// The reason.
    pub code: RefusalCode,
    /// The field at fault, when one is (absent for a problem with the whole line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<Field>,
}

impl RowRefusal {
    /// A refusal of the whole line.
    pub const fn new(row: u32, code: RefusalCode) -> Self {
        Self {
            row,
            code,
            field: None,
        }
    }

    /// The same refusal, naming the field at fault.
    #[must_use]
    pub const fn at(self, field: Field) -> Self {
        Self {
            field: Some(field),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests: a panic is the failure report")]

    use super::*;

    #[test]
    fn every_code_says_what_is_wrong_and_how_to_fix_it() {
        for code in RefusalCode::ALL {
            let (problem, fix) = code.explain();
            for text in [problem, fix] {
                assert!(text.len() > 15 && text.ends_with('.'), "{code:?}: {text:?}");
            }
        }
    }

    #[test]
    fn a_row_refusal_without_a_field_reads_as_before() {
        let old: RowRefusal = serde_json::from_str(r#"{"row":3,"code":"invalid_date"}"#).unwrap();
        assert_eq!(old, RowRefusal::new(3, RefusalCode::InvalidDate));
        let at = old.at(Field::ValueDate);
        assert_eq!(
            serde_json::to_string(&at).unwrap(),
            r#"{"row":3,"code":"invalid_date","field":"value_date"}"#
        );
    }

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
