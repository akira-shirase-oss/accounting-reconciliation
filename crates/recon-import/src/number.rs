//! Number formats a profile declares (D-062): the decimal mark (`.` or `,`), an optional
//! grouping mark with Western (`1,234,567.89`) or Indian (`12,34,567.89`) grouping, and the
//! negative forms the bank uses — a leading minus, parentheses (`(125.00)`) or a trailing
//! minus (`125.00-`).
//!
//! A field is rewritten into the plain decimal form [`Money::parse`] reads (an optional `-`,
//! digits, one `.` and the fraction), so there is still one exact parser and no float
//! anywhere. Grouping is checked group by group: a number either has no grouping marks at
//! all or every group has the declared size; `1,23,456` under Western grouping or `123,456`
//! under Indian grouping is `invalid_amount`, never read as some other number.

use recon_domain::refusal::RefusalCode;
use recon_domain::{Currency, Money};
use serde::{Deserialize, Serialize};

/// How digits are grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupStyle {
    /// Groups of three: `1,234,567`.
    #[default]
    Western,
    /// The last group of three, then groups of two: `12,34,567`.
    Indian,
}

/// A negative form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Negative {
    /// `-125.00`.
    Minus,
    /// `(125.00)`.
    Parentheses,
    /// `125.00-`.
    TrailingMinus,
}

/// The grouping marks a format may declare.
pub const GROUPING_MARKS: [char; 6] = [',', '.', ' ', '\'', '\u{00A0}', '\u{202F}'];

/// A number format.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberFormat {
    /// The decimal mark, `.` or `,`.
    pub decimal: char,
    /// The grouping mark, if any.
    #[serde(default)]
    pub grouping: Option<char>,
    /// How groups are sized.
    #[serde(default)]
    pub group_style: GroupStyle,
    /// The negative forms in use (none: every amount is a magnitude).
    #[serde(default)]
    pub negatives: Vec<Negative>,
    /// A leading `+` is allowed.
    #[serde(default)]
    pub plus_sign: bool,
}

impl NumberFormat {
    /// The R0 format: `.` decimals, no grouping, a leading minus.
    pub fn plain() -> Self {
        Self {
            decimal: '.',
            grouping: None,
            group_style: GroupStyle::Western,
            negatives: vec![Negative::Minus],
            plus_sign: false,
        }
    }

    /// Problems that make the format unusable, as `field: problem` codes.
    pub fn problems(&self, field: &'static str, out: &mut Vec<String>) {
        if !matches!(self.decimal, '.' | ',') {
            out.push(format!("{field}.decimal: must be . or ,"));
        }
        if let Some(g) = self.grouping {
            if !GROUPING_MARKS.contains(&g) {
                out.push(format!("{field}.grouping: unsupported"));
            }
            if g == self.decimal {
                out.push(format!("{field}.grouping: equals the decimal mark"));
            }
        }
        let mut seen = self.negatives.clone();
        seen.sort_by_key(|n| *n as u8);
        seen.dedup();
        if seen.len() != self.negatives.len() {
            out.push(format!("{field}.negatives: repeated"));
        }
    }

    fn allows(&self, negative: Negative) -> bool {
        self.negatives.contains(&negative)
    }

    /// Parses `text` as a signed amount in `currency`.
    pub fn parse(&self, text: &str, currency: Currency) -> Result<Money, RefusalCode> {
        if text.is_empty() {
            return Err(RefusalCode::MissingRequiredField);
        }
        let (negative, core) = self.sign(text)?;
        let (int_part, frac_part) = match core.split_once(self.decimal) {
            Some((i, f)) => (i, Some(f)),
            None => (core, None),
        };
        let digits = self.ungroup(int_part)?;
        let mut plain = String::with_capacity(core.len().saturating_add(2));
        if negative {
            plain.push('-');
        }
        plain.push_str(&digits);
        if let Some(f) = frac_part {
            plain.push('.');
            plain.push_str(f);
        }
        Money::parse(&plain, currency).map_err(RefusalCode::from)
    }

    /// Splits off the sign: `(negative, the unsigned text)`.
    fn sign<'t>(&self, text: &'t str) -> Result<(bool, &'t str), RefusalCode> {
        if let Some(inner) = text.strip_prefix('(') {
            let inner = inner
                .strip_suffix(')')
                .filter(|_| self.allows(Negative::Parentheses))
                .ok_or(RefusalCode::InvalidAmount)?;
            return Ok((true, inner));
        }
        if let Some(rest) = text.strip_prefix('-') {
            return if self.allows(Negative::Minus) {
                Ok((true, rest))
            } else {
                Err(RefusalCode::InvalidAmount)
            };
        }
        if let Some(rest) = text.strip_prefix('+') {
            return if self.plus_sign {
                Ok((false, rest))
            } else {
                Err(RefusalCode::InvalidAmount)
            };
        }
        if let Some(rest) = text.strip_suffix('-') {
            return if self.allows(Negative::TrailingMinus) {
                Ok((true, rest))
            } else {
                Err(RefusalCode::InvalidAmount)
            };
        }
        Ok((false, text))
    }

    /// The integer digits without grouping marks, checked group by group.
    fn ungroup(&self, int_part: &str) -> Result<String, RefusalCode> {
        let Some(mark) = self.grouping.filter(|g| int_part.contains(*g)) else {
            return Ok(int_part.to_owned());
        };
        let groups: Vec<&str> = int_part.split(mark).collect();
        let last = groups.len().saturating_sub(1);
        for (i, g) in groups.iter().enumerate() {
            if g.is_empty() || !g.bytes().all(|b| b.is_ascii_digit()) {
                return Err(RefusalCode::InvalidAmount);
            }
            // There are at least two groups (the mark is in the text): the first holds 1 to
            // 3 digits (1 to 2 in Indian grouping), the last exactly 3, the ones between 3
            // (2 in Indian grouping).
            let len = g.len();
            let ok = if i == 0 {
                match self.group_style {
                    GroupStyle::Western => (1..=3).contains(&len),
                    GroupStyle::Indian => (1..=2).contains(&len),
                }
            } else if i == last {
                len == 3
            } else {
                match self.group_style {
                    GroupStyle::Western => len == 3,
                    GroupStyle::Indian => len == 2,
                }
            };
            if !ok {
                return Err(RefusalCode::InvalidAmount);
            }
        }
        Ok(groups.concat())
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
    use RefusalCode as C;
    use proptest::prelude::*;

    fn format(decimal: char, grouping: Option<char>, style: GroupStyle) -> NumberFormat {
        NumberFormat {
            decimal,
            grouping,
            group_style: style,
            negatives: vec![
                Negative::Minus,
                Negative::Parentheses,
                Negative::TrailingMinus,
            ],
            plus_sign: false,
        }
    }

    fn minor(f: &NumberFormat, text: &str) -> Result<i64, RefusalCode> {
        f.parse(text, Currency::Inr).map(Money::minor)
    }

    #[test]
    fn western_and_indian_grouping_are_checked_group_by_group() {
        let western = format('.', Some(','), GroupStyle::Western);
        assert_eq!(minor(&western, "1,234,567.89"), Ok(123_456_789));
        assert_eq!(minor(&western, "1234567.89"), Ok(123_456_789));
        assert_eq!(minor(&western, "999"), Ok(99_900));
        for bad in [
            "1,23,456.00",
            "1234,567.00",
            ",123.00",
            "1,,234.00",
            "12,34",
            "1,234,56",
        ] {
            assert_eq!(minor(&western, bad), Err(C::InvalidAmount), "{bad}");
        }
        let indian = format('.', Some(','), GroupStyle::Indian);
        assert_eq!(minor(&indian, "12,34,567.89"), Ok(123_456_789));
        assert_eq!(minor(&indian, "1,23,45,678.00"), Ok(1_234_567_800));
        assert_eq!(minor(&indian, "12,345.00"), Ok(1_234_500));
        assert_eq!(minor(&indian, "1,234"), Ok(123_400));
        for bad in ["123,456.00", "1,234,567.00", "1,2,345.00", "12,34,56"] {
            assert_eq!(minor(&indian, bad), Err(C::InvalidAmount), "{bad}");
        }
    }

    #[test]
    fn decimal_commas_and_other_marks() {
        let euro = format(',', Some('.'), GroupStyle::Western);
        assert_eq!(minor(&euro, "1.234,50"), Ok(123_450));
        assert_eq!(minor(&euro, "-0,05"), Ok(-5));
        assert_eq!(minor(&euro, "1,234.50"), Err(C::InvalidAmount));
        let swiss = format('.', Some('\''), GroupStyle::Western);
        assert_eq!(minor(&swiss, "1'234.50"), Ok(123_450));
        let spaced = format(',', Some('\u{00A0}'), GroupStyle::Western);
        assert_eq!(minor(&spaced, "1\u{00A0}234,50"), Ok(123_450));
    }

    #[test]
    fn negative_forms_only_as_declared() {
        let all = format('.', None, GroupStyle::Western);
        assert_eq!(minor(&all, "(125.00)"), Ok(-12_500));
        assert_eq!(minor(&all, "125.00-"), Ok(-12_500));
        assert_eq!(minor(&all, "-125.00"), Ok(-12_500));
        for bad in [
            "(-125.00)",
            "-(125.00)",
            "(125.00",
            "125.00)",
            "--1",
            "1--",
            "+1",
            "- 1",
        ] {
            assert_eq!(minor(&all, bad), Err(C::InvalidAmount), "{bad}");
        }
        let plain = NumberFormat::plain();
        assert_eq!(minor(&plain, "-12.5"), Ok(-1_250));
        assert_eq!(minor(&plain, "(12.50)"), Err(C::InvalidAmount));
        assert_eq!(minor(&plain, "12.50-"), Err(C::InvalidAmount));
        let magnitudes = NumberFormat {
            negatives: vec![],
            plus_sign: true,
            ..NumberFormat::plain()
        };
        assert_eq!(minor(&magnitudes, "-1"), Err(C::InvalidAmount));
        assert_eq!(minor(&magnitudes, "+1"), Ok(100));
    }

    #[test]
    fn money_rules_still_hold_after_the_rewrite() {
        let f = format('.', Some(','), GroupStyle::Western);
        assert_eq!(minor(&f, "1.234"), Err(C::TooManyDecimalPlaces));
        assert_eq!(
            minor(&f, "12,345,678,901,234,567.00"),
            Err(C::AmountOutOfRange)
        );
        assert_eq!(minor(&f, ""), Err(C::MissingRequiredField));
        assert_eq!(minor(&f, "1e3"), Err(C::InvalidAmount));
        assert_eq!(minor(&f, "1."), Err(C::InvalidAmount));
        assert_eq!(minor(&f, "₹100"), Err(C::InvalidAmount));
        assert_eq!(f.parse("1,000", Currency::Jpy).map(Money::minor), Ok(1_000));
        assert_eq!(f.parse("1.5", Currency::Jpy), Err(C::TooManyDecimalPlaces));
    }

    #[test]
    fn unusable_formats_are_named() {
        let mut problems = vec![];
        format(';', None, GroupStyle::Western).problems("n", &mut problems);
        format('.', Some('.'), GroupStyle::Western).problems("n", &mut problems);
        format('.', Some('x'), GroupStyle::Western).problems("n", &mut problems);
        NumberFormat {
            negatives: vec![Negative::Minus, Negative::Minus],
            ..NumberFormat::plain()
        }
        .problems("n", &mut problems);
        assert_eq!(problems.len(), 4, "{problems:?}");
    }

    proptest! {
        /// Writing any amount in a grouped format and reading it back is exact.
        #[test]
        fn grouped_amounts_round_trip(minor_units in -1_000_000_000_000_i64..=1_000_000_000_000, indian in any::<bool>()) {
            let style = if indian { GroupStyle::Indian } else { GroupStyle::Western };
            let f = format('.', Some(','), style);
            let digits = (minor_units.unsigned_abs() / 100).to_string();
            let mut groups: Vec<String> = vec![];
            let mut rest = digits.as_str();
            let mut first = true;
            while !rest.is_empty() {
                let size = if first || style == GroupStyle::Western { 3 } else { 2 };
                let cut = rest.len().saturating_sub(size);
                groups.insert(0, rest[cut..].to_owned());
                rest = &rest[..cut];
                first = false;
            }
            let text = format!("{}.{:02}", groups.join(","), minor_units.unsigned_abs() % 100);
            let text = if minor_units < 0 { format!("({text})") } else { text };
            prop_assert_eq!(minor(&f, &text), Ok(minor_units));
        }

        #[test]
        fn arbitrary_text_never_panics(text in "\\PC{0,24}") {
            let _ = format(',', Some('.'), GroupStyle::Indian).parse(&text, Currency::Kwd);
        }
    }
}
