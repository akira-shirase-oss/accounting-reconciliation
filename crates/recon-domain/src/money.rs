//! Exact money: signed `i64` minor units with an explicit currency and its fixed exponent
//! (D-005). Amount strings are parsed digit by digit and never pass through a float; every
//! addition is checked; a single amount is at most [`MAX_ABS_MINOR`] in absolute value, so
//! a sum of up to 10^6 of them fits in `i64` and any sum fits in the `i128` of [`MoneySum`].

use std::fmt;

use serde::{Deserialize, Serialize};

/// The largest absolute amount one line, document, allocation or residual may carry, in
/// minor units (D-005): 10^12.
pub const MAX_ABS_MINOR: i64 = 1_000_000_000_000;

/// 10^n for the exponents in the table (0, 2, 3) and the padding they need.
const POW10: [i64; 4] = [1, 10, 100, 1_000];

/// The supported currencies and their fixed exponents (D-005). Nothing else is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Currency {
    /// Indian rupee, 2 places.
    Inr,
    /// US dollar, 2 places.
    Usd,
    /// Euro, 2 places.
    Eur,
    /// Pound sterling, 2 places.
    Gbp,
    /// UAE dirham, 2 places.
    Aed,
    /// Singapore dollar, 2 places.
    Sgd,
    /// Australian dollar, 2 places.
    Aud,
    /// Canadian dollar, 2 places.
    Cad,
    /// Japanese yen, 0 places.
    Jpy,
    /// Kuwaiti dinar, 3 places.
    Kwd,
}

impl Currency {
    /// Every supported currency, in table order.
    pub const ALL: [Self; 10] = [
        Self::Inr,
        Self::Usd,
        Self::Eur,
        Self::Gbp,
        Self::Aed,
        Self::Sgd,
        Self::Aud,
        Self::Cad,
        Self::Jpy,
        Self::Kwd,
    ];

    /// The ISO 4217 code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Inr => "INR",
            Self::Usd => "USD",
            Self::Eur => "EUR",
            Self::Gbp => "GBP",
            Self::Aed => "AED",
            Self::Sgd => "SGD",
            Self::Aud => "AUD",
            Self::Cad => "CAD",
            Self::Jpy => "JPY",
            Self::Kwd => "KWD",
        }
    }

    /// Decimal places of the minor unit.
    pub const fn exponent(self) -> u8 {
        match self {
            Self::Jpy => 0,
            Self::Kwd => 3,
            _ => 2,
        }
    }

    /// Parses an exact, upper-case ISO 4217 code from the table.
    pub fn from_code(code: &str) -> Result<Self, MoneyError> {
        Self::ALL
            .into_iter()
            .find(|c| c.code() == code)
            .ok_or(MoneyError::UnknownCurrency)
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Why an amount was refused. Each has a stable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    /// The amount field is empty.
    #[error("empty_amount")]
    Empty,
    /// Something other than an optional leading `-`, digits and one `.` followed by digits.
    #[error("invalid_amount")]
    Invalid,
    /// More fractional digits than the currency's exponent: refused, never rounded.
    #[error("too_many_decimal_places")]
    TooManyDecimals,
    /// Beyond ±10^12 minor units (or the sum of two amounts would be).
    #[error("amount_out_of_range")]
    OutOfRange,
    /// Two amounts in different currencies were combined.
    #[error("currency_mismatch")]
    CurrencyMismatch,
    /// A code outside the D-005 table.
    #[error("unknown_currency")]
    UnknownCurrency,
}

impl MoneyError {
    /// The stable refusal code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Empty => "empty_amount",
            Self::Invalid => "invalid_amount",
            Self::TooManyDecimals => "too_many_decimal_places",
            Self::OutOfRange => "amount_out_of_range",
            Self::CurrencyMismatch => "currency_mismatch",
            Self::UnknownCurrency => "unknown_currency",
        }
    }
}

/// An exact amount: signed minor units in one currency, `|minor| <= 10^12`. Deserializing
/// goes through [`Money::new`], so a request or a model's answer cannot carry an amount the
/// constructor would refuse (D-052).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "MoneyRepr")]
pub struct Money {
    minor: i64,
    currency: Currency,
}

/// The wire form of [`Money`], validated on the way in.
#[derive(Deserialize)]
struct MoneyRepr {
    minor: i64,
    currency: Currency,
}

impl TryFrom<MoneyRepr> for Money {
    type Error = MoneyError;

    fn try_from(repr: MoneyRepr) -> Result<Self, MoneyError> {
        Self::new(repr.minor, repr.currency)
    }
}

impl Money {
    /// An amount of `minor` units, refused beyond ±10^12.
    pub const fn new(minor: i64, currency: Currency) -> Result<Self, MoneyError> {
        if minor > MAX_ABS_MINOR || minor < -MAX_ABS_MINOR {
            return Err(MoneyError::OutOfRange);
        }
        Ok(Self { minor, currency })
    }

    /// Zero in `currency`.
    pub const fn zero(currency: Currency) -> Self {
        Self { minor: 0, currency }
    }

    /// Signed minor units.
    pub const fn minor(self) -> i64 {
        self.minor
    }

    /// The currency.
    pub const fn currency(self) -> Currency {
        self.currency
    }

    /// True for zero.
    pub const fn is_zero(self) -> bool {
        self.minor == 0
    }

    /// The negated amount, refused beyond the range (never the case for a value built by
    /// [`Money::new`] or deserialized, since the range is symmetric — but checked, not
    /// assumed).
    pub fn checked_neg(self) -> Result<Self, MoneyError> {
        let minor = self.minor.checked_neg().ok_or(MoneyError::OutOfRange)?;
        Self::new(minor, self.currency)
    }

    /// `self + other`, refused across currencies or beyond the range.
    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        if self.currency != other.currency {
            return Err(MoneyError::CurrencyMismatch);
        }
        let sum = self
            .minor
            .checked_add(other.minor)
            .ok_or(MoneyError::OutOfRange)?;
        Self::new(sum, self.currency)
    }

    /// `self - other`, refused across currencies or beyond the range.
    pub fn checked_sub(self, other: Self) -> Result<Self, MoneyError> {
        self.checked_add(other.checked_neg()?)
    }

    /// Parses a plain decimal string: an optional leading `-`, at least one digit, and
    /// optionally one `.` followed by 1 to `exponent` digits. No `+`, no spaces, no digit
    /// grouping, no exponent notation: anything else is refused (R1 profiles translate
    /// other bank formats into this one before it gets here).
    pub fn parse(text: &str, currency: Currency) -> Result<Self, MoneyError> {
        let bytes = text.as_bytes();
        let (negative, digits) = match bytes.split_first() {
            None => return Err(MoneyError::Empty),
            Some((b'-', rest)) => (true, rest),
            Some(_) => (false, bytes),
        };
        let (int_part, frac_part) = match digits.iter().position(|&b| b == b'.') {
            Some(dot) => {
                let (int_part, rest) = digits.split_at(dot);
                (int_part, rest.get(1..).unwrap_or_default())
            }
            None => (digits, &[][..]),
        };
        let has_point = int_part.len() != digits.len();
        if int_part.is_empty() || (has_point && frac_part.is_empty()) {
            return Err(MoneyError::Invalid);
        }
        let exponent = usize::from(currency.exponent());
        let mut value: i64 = 0;
        for &b in int_part {
            value = push_digit(value, b)?;
        }
        if frac_part.len() > exponent {
            // A non-digit in the fraction is a malformed amount, not a precision problem.
            if frac_part.iter().any(|b| !b.is_ascii_digit()) {
                return Err(MoneyError::Invalid);
            }
            return Err(MoneyError::TooManyDecimals);
        }
        for &b in frac_part {
            value = push_digit(value, b)?;
        }
        let padding = POW10
            .get(exponent.saturating_sub(frac_part.len()))
            .copied()
            .ok_or(MoneyError::OutOfRange)?;
        let value = value.checked_mul(padding).ok_or(MoneyError::OutOfRange)?;
        let value = if negative {
            value.checked_neg().ok_or(MoneyError::OutOfRange)?
        } else {
            value
        };
        Self::new(value, currency)
    }

    /// The plain decimal form [`Money::parse`] reads back exactly: `-1234.50`, `1200`,
    /// `0.001`.
    pub fn to_decimal_string(self) -> String {
        let exponent = usize::from(self.currency.exponent());
        let digits = self.minor.unsigned_abs().to_string();
        let mut out = String::with_capacity(digits.len().saturating_add(3));
        if self.minor < 0 {
            out.push('-');
        }
        if exponent == 0 {
            out.push_str(&digits);
            return out;
        }
        let padded = format!("{digits:0>width$}", width = exponent.saturating_add(1));
        let split = padded.len().saturating_sub(exponent);
        let (int_part, frac_part) = padded.split_at(split);
        out.push_str(int_part);
        out.push('.');
        out.push_str(frac_part);
        out
    }
}

/// `value * 10 + digit`, refusing a non-digit or anything beyond the range. The bound is
/// checked on every digit, so a 20-digit (or 8,000-digit) amount ends in `OutOfRange`
/// without overflowing.
fn push_digit(value: i64, byte: u8) -> Result<i64, MoneyError> {
    if !byte.is_ascii_digit() {
        return Err(MoneyError::Invalid);
    }
    let digit = i64::from(byte.wrapping_sub(b'0'));
    let next = value
        .checked_mul(10)
        .and_then(|v| v.checked_add(digit))
        .ok_or(MoneyError::OutOfRange)?;
    if next > MAX_ABS_MINOR {
        return Err(MoneyError::OutOfRange);
    }
    Ok(next)
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.to_decimal_string(), self.currency)
    }
}

/// A running total in one currency, in `i128` so any number of bounded amounts sums
/// without overflow (whole-dataset reports, D-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MoneySum {
    minor: i128,
    currency: Currency,
}

impl MoneySum {
    /// Zero in `currency`.
    pub const fn zero(currency: Currency) -> Self {
        Self { minor: 0, currency }
    }

    /// Adds one amount, refused across currencies.
    pub fn add(&mut self, amount: Money) -> Result<(), MoneyError> {
        if amount.currency != self.currency {
            return Err(MoneyError::CurrencyMismatch);
        }
        self.minor = self
            .minor
            .checked_add(i128::from(amount.minor))
            .ok_or(MoneyError::OutOfRange)?;
        Ok(())
    }

    /// The total in minor units.
    pub const fn minor(self) -> i128 {
        self.minor
    }

    /// The currency.
    pub const fn currency(self) -> Currency {
        self.currency
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;
    use proptest::prelude::*;

    fn usd(text: &str) -> Result<i64, MoneyError> {
        Money::parse(text, Currency::Usd).map(Money::minor)
    }

    #[test]
    fn plain_decimals_parse_exactly() {
        assert_eq!(usd("0"), Ok(0));
        assert_eq!(usd("12"), Ok(1_200));
        assert_eq!(usd("12.3"), Ok(1_230));
        assert_eq!(usd("12.34"), Ok(1_234));
        assert_eq!(usd("-12.34"), Ok(-1_234));
        assert_eq!(usd("0007.05"), Ok(705));
        assert_eq!(usd("-0.00"), Ok(0));
        assert_eq!(usd("10000000000.00"), Ok(MAX_ABS_MINOR));
        assert_eq!(usd("-10000000000.00"), Ok(-MAX_ABS_MINOR));
        assert_eq!(Money::parse("1200", Currency::Jpy).unwrap().minor(), 1_200);
        assert_eq!(Money::parse("1.5", Currency::Kwd).unwrap().minor(), 1_500);
        assert_eq!(Money::parse("0.001", Currency::Kwd).unwrap().minor(), 1);
    }

    #[test]
    fn anything_else_is_a_named_refusal() {
        assert_eq!(usd(""), Err(MoneyError::Empty));
        for bad in [
            "-", "+1", " 1", "1 ", "1,000.00", "1.", ".5", "-.5", "1..2", "1.2.3", "1e3", "0x10",
            "１２", "--1", "1-", "NaN", "inf", "1_000",
        ] {
            assert_eq!(usd(bad), Err(MoneyError::Invalid), "{bad:?}");
        }
        assert_eq!(usd("1.234"), Err(MoneyError::TooManyDecimals));
        assert_eq!(usd("1.2a4"), Err(MoneyError::Invalid));
        assert_eq!(
            Money::parse("1.0", Currency::Jpy),
            Err(MoneyError::TooManyDecimals)
        );
        assert_eq!(usd("10000000000.01"), Err(MoneyError::OutOfRange));
        // A 20-digit amount (failure case 6) is refused without overflowing.
        assert_eq!(usd("12345678901234567890"), Err(MoneyError::OutOfRange));
        assert_eq!(usd(&"9".repeat(8_000)), Err(MoneyError::OutOfRange));
    }

    #[test]
    fn arithmetic_is_checked_and_single_currency() {
        let a = Money::new(MAX_ABS_MINOR, Currency::Usd).unwrap();
        let one = Money::new(1, Currency::Usd).unwrap();
        assert_eq!(a.checked_add(one), Err(MoneyError::OutOfRange));
        assert_eq!(
            one.checked_add(Money::new(1, Currency::Eur).unwrap()),
            Err(MoneyError::CurrencyMismatch)
        );
        assert_eq!(a.checked_sub(a).unwrap().minor(), 0);
        assert_eq!(
            Money::new(MAX_ABS_MINOR + 1, Currency::Usd),
            Err(MoneyError::OutOfRange)
        );
        let mut sum = MoneySum::zero(Currency::Usd);
        for _ in 0..1_000 {
            sum.add(a).unwrap();
        }
        assert_eq!(sum.minor(), 1_000 * i128::from(MAX_ABS_MINOR));
        assert_eq!(
            sum.add(Money::zero(Currency::Inr)),
            Err(MoneyError::CurrencyMismatch)
        );
    }

    #[test]
    fn currency_codes_are_exact() {
        for c in Currency::ALL {
            assert_eq!(Currency::from_code(c.code()), Ok(c));
        }
        assert_eq!(Currency::from_code("usd"), Err(MoneyError::UnknownCurrency));
        assert_eq!(Currency::from_code("XXX"), Err(MoneyError::UnknownCurrency));
        assert_eq!(
            Currency::from_code(" USD"),
            Err(MoneyError::UnknownCurrency)
        );
    }

    /// M-5 regression: a derived `Deserialize` skipped `Money::new`'s range check, so
    /// `i64::MIN` minor units deserialized, and negating it lost a unit.
    #[test]
    fn deserializing_is_range_checked_like_construction() {
        for minor in [i64::MIN, i64::MAX, MAX_ABS_MINOR + 1, -MAX_ABS_MINOR - 1] {
            let json = format!(r#"{{"minor":{minor},"currency":"Usd"}}"#);
            assert!(
                serde_json::from_str::<Money>(&json).is_err(),
                "{minor} deserialized"
            );
        }
        let edge: Money = serde_json::from_str(&format!(
            r#"{{"minor":{},"currency":"Usd"}}"#,
            -MAX_ABS_MINOR
        ))
        .unwrap();
        assert_eq!(edge.checked_neg().map(Money::minor), Ok(MAX_ABS_MINOR));
        let round = serde_json::to_string(&edge).unwrap();
        assert_eq!(serde_json::from_str::<Money>(&round).unwrap(), edge);
    }

    fn any_currency() -> impl Strategy<Value = Currency> {
        prop::sample::select(Currency::ALL.to_vec())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4_096))]

        /// Formatting then parsing returns the same minor units: no unit is ever lost.
        #[test]
        fn parse_of_format_is_identity(minor in -MAX_ABS_MINOR..=MAX_ABS_MINOR, c in any_currency()) {
            let m = Money::new(minor, c).unwrap();
            prop_assert_eq!(Money::parse(&m.to_decimal_string(), c), Ok(m));
        }

        /// A decimal built from integer and fraction digits parses to exactly
        /// int * 10^e + frac * 10^(e - len), computed independently in i128.
        #[test]
        fn parse_equals_the_exact_integer(
            int_part in 0_u64..=20_000_000_000,
            frac in proptest::collection::vec(0_u8..=9, 0..=3),
            negative: bool,
            c in any_currency(),
        ) {
            let e = usize::from(c.exponent());
            let mut text = String::new();
            if negative { text.push('-'); }
            text.push_str(&int_part.to_string());
            if !frac.is_empty() {
                text.push('.');
                for d in &frac { text.push(char::from(b'0' + d)); }
            }
            let result = Money::parse(&text, c);
            if frac.len() > e {
                prop_assert_eq!(result, Err(MoneyError::TooManyDecimals));
            } else {
                let mut expected = i128::from(int_part);
                for d in &frac { expected = expected * 10 + i128::from(*d); }
                for _ in frac.len()..e { expected *= 10; }
                if negative { expected = -expected; }
                if expected.abs() > i128::from(MAX_ABS_MINOR) {
                    prop_assert_eq!(result, Err(MoneyError::OutOfRange));
                } else {
                    prop_assert_eq!(result.map(|m| i128::from(m.minor())), Ok(expected));
                }
            }
        }

        /// Arbitrary text never panics and is either an exact amount or a named refusal.
        #[test]
        fn arbitrary_text_never_panics(text in ".{0,40}", c in any_currency()) {
            if let Ok(m) = Money::parse(&text, c) {
                prop_assert_eq!(Money::parse(&m.to_decimal_string(), c), Ok(m));
            }
        }

        /// Checked addition either equals the exact i128 sum within range or refuses.
        #[test]
        fn addition_is_exact_or_refused(
            a in -MAX_ABS_MINOR..=MAX_ABS_MINOR,
            b in -MAX_ABS_MINOR..=MAX_ABS_MINOR,
        ) {
            let x = Money::new(a, Currency::Usd).unwrap();
            let y = Money::new(b, Currency::Usd).unwrap();
            let exact = i128::from(a) + i128::from(b);
            match x.checked_add(y) {
                Ok(s) => prop_assert_eq!(i128::from(s.minor()), exact),
                Err(e) => {
                    prop_assert_eq!(e, MoneyError::OutOfRange);
                    prop_assert!(exact.abs() > i128::from(MAX_ABS_MINOR));
                }
            }
        }
    }
}
