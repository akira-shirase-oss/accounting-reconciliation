//! Date formats a profile declares (D-010, D-062). A format names the order of its parts
//! (`ymd`, `dmy`, `mdy`), the separator (`-`, `/`, `.`, a space, or none for `20260301`),
//! numeric or three-letter English month names, and whether day and month are always two
//! digits. Years are always four digits: a two-digit year would be a guess at the century.
//!
//! A profile may leave the order of day and month undeclared. Its dates are then read both
//! ways, and the file is refused whole as `ambiguous_date_format` with a question for the
//! user — which orders every date in the file is consistent with — never a guess, even when
//! only one order fits (`25/03/2026`): the person who knows the bank declares it.

use recon_domain::Date;
use recon_domain::date::DateError;
use serde::{Deserialize, Serialize};

/// The order of a date's parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateOrder {
    /// Year, month, day.
    Ymd,
    /// Day, month, year.
    Dmy,
    /// Month, day, year.
    Mdy,
}

impl DateOrder {
    /// The stored code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ymd => "ymd",
            Self::Dmy => "dmy",
            Self::Mdy => "mdy",
        }
    }
}

/// The separators a format may declare.
pub const SEPARATORS: [char; 4] = ['-', '/', '.', ' '];

/// A date format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DateFormat {
    /// The order of the parts; absent means day-month or month-day, undeclared.
    #[serde(default)]
    pub order: Option<DateOrder>,
    /// The separator, or none (`ymd` only: `20260301`).
    #[serde(default)]
    pub separator: Option<char>,
    /// Months as `Jan`…`Dec` (any letter case).
    #[serde(default)]
    pub month_names: bool,
    /// Day and month always two digits (`01`, never `1`).
    #[serde(default)]
    pub padded: bool,
}

impl DateFormat {
    /// `YYYY-MM-DD`, exactly ten characters (the R0 format).
    pub const ISO: Self = Self {
        order: Some(DateOrder::Ymd),
        separator: Some('-'),
        month_names: false,
        padded: true,
    };

    /// Problems that make the format unusable, as `field: problem` codes.
    pub fn problems(self, field: &'static str, out: &mut Vec<String>) {
        if let Some(sep) = self.separator
            && !SEPARATORS.contains(&sep)
        {
            out.push(format!("{field}.separator: unsupported"));
        }
        match (self.order, self.separator) {
            (None, None) => out.push(format!(
                "{field}.separator: required when the order is undeclared"
            )),
            (Some(DateOrder::Dmy | DateOrder::Mdy), None) => {
                out.push(format!("{field}.separator: required for dmy and mdy"));
            }
            _ => {}
        }
        if self.month_names && self.order.is_none() {
            out.push(format!("{field}.order: required with month names"));
        }
        if self.month_names && self.separator.is_none() {
            out.push(format!("{field}.separator: required with month names"));
        }
    }

    /// Parses `text`.
    pub fn parse(self, text: &str) -> Parsed {
        let Some(order) = self.order else {
            return self.parse_undeclared(text);
        };
        let Some((a, b, c)) = self.parts(text) else {
            return Parsed::Invalid(DateError::Invalid);
        };
        let ymd = match order {
            DateOrder::Ymd => (a, b, c),
            DateOrder::Dmy => (c, b, a),
            DateOrder::Mdy => (c, a, b),
        };
        match self.build(ymd) {
            Ok(d) => Parsed::Date(d),
            Err(e) => Parsed::Invalid(e),
        }
    }

    fn parse_undeclared(self, text: &str) -> Parsed {
        let Some((a, b, year)) = self.parts(text) else {
            return Parsed::Invalid(DateError::Invalid);
        };
        let dmy = self.build((year, b, a));
        let mdy = self.build((year, a, b));
        match (dmy, mdy) {
            (Err(e), Err(_)) => Parsed::Invalid(e),
            (dmy, mdy) => Parsed::Undeclared {
                dmy: dmy.is_ok(),
                mdy: mdy.is_ok(),
            },
        }
    }

    /// Splits into three parts (or the eight digits of `YYYYMMDD`).
    fn parts(self, text: &str) -> Option<(&str, &str, &str)> {
        match self.separator {
            None => {
                if text.len() != 8 || !text.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                Some((text.get(0..4)?, text.get(4..6)?, text.get(6..8)?))
            }
            Some(sep) => {
                let mut it = text.split(sep);
                let parts = (it.next()?, it.next()?, it.next()?);
                it.next().is_none().then_some(parts)
            }
        }
    }

    /// Builds a date from (year, month, day) texts.
    fn build(self, (year, month, day): (&str, &str, &str)) -> Result<Date, DateError> {
        let year = digits(year, 4, 4)?;
        let month = if self.month_names {
            month_name(month)?
        } else {
            digits(month, if self.padded { 2 } else { 1 }, 2)?
        };
        let day = digits(day, if self.padded { 2 } else { 1 }, 2)?;
        Date::new(
            u16::try_from(year).map_err(|_| DateError::Invalid)?,
            u8::try_from(month).map_err(|_| DateError::Invalid)?,
            u8::try_from(day).map_err(|_| DateError::Invalid)?,
        )
    }
}

/// What parsing one date found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    /// A date.
    Date(Date),
    /// Not a date in the format.
    Invalid(DateError),
    /// The order is undeclared: whether the text is a real day read as day-month and as
    /// month-day (at least one is).
    Undeclared {
        /// Valid as day-month-year.
        dmy: bool,
        /// Valid as month-day-year.
        mdy: bool,
    },
}

/// The question an `ambiguous_date_format` refusal puts to the user: the orders every date
/// in the file is consistent with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DateQuestion {
    /// Every date reads as day-month-year.
    pub dmy: bool,
    /// Every date reads as month-day-year.
    pub mdy: bool,
}

impl DateQuestion {
    /// The orders to offer, in a fixed order.
    pub fn options(self) -> Vec<&'static str> {
        [(self.dmy, "dmy"), (self.mdy, "mdy")]
            .into_iter()
            .filter_map(|(ok, code)| ok.then_some(code))
            .collect()
    }
}

fn digits(text: &str, min: usize, max: usize) -> Result<u32, DateError> {
    if !(min..=max).contains(&text.len()) || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DateError::Invalid);
    }
    let mut value: u32 = 0;
    for b in text.bytes() {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u32::from(b.wrapping_sub(b'0'))))
            .ok_or(DateError::Invalid)?;
    }
    Ok(value)
}

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

fn month_name(text: &str) -> Result<u32, DateError> {
    if text.len() != 3 || !text.is_ascii() {
        return Err(DateError::Invalid);
    }
    let upper = text.to_ascii_uppercase();
    MONTHS
        .iter()
        .position(|m| *m == upper)
        .and_then(|i| u32::try_from(i).ok())
        .map(|i| i.saturating_add(1))
        .ok_or(DateError::Invalid)
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

    fn fmt(order: Option<DateOrder>, separator: Option<char>) -> DateFormat {
        DateFormat {
            order,
            separator,
            month_names: false,
            padded: false,
        }
    }

    fn date(text: &str) -> Date {
        Date::parse_iso(text).unwrap()
    }

    #[test]
    fn declared_orders_read_their_parts() {
        let dmy = fmt(Some(DateOrder::Dmy), Some('/'));
        assert_eq!(dmy.parse("01/03/2026"), Parsed::Date(date("2026-03-01")));
        assert_eq!(dmy.parse("1/3/2026"), Parsed::Date(date("2026-03-01")));
        assert_eq!(dmy.parse("31/12/2026"), Parsed::Date(date("2026-12-31")));
        assert_eq!(dmy.parse("12/31/2026"), Parsed::Invalid(DateError::Invalid));
        let mdy = fmt(Some(DateOrder::Mdy), Some('-'));
        assert_eq!(mdy.parse("12-31-2026"), Parsed::Date(date("2026-12-31")));
        let compact = fmt(Some(DateOrder::Ymd), None);
        assert_eq!(compact.parse("20260301"), Parsed::Date(date("2026-03-01")));
        assert_eq!(
            compact.parse("2026031"),
            Parsed::Invalid(DateError::Invalid)
        );
        let named = DateFormat {
            month_names: true,
            ..fmt(Some(DateOrder::Dmy), Some('-'))
        };
        assert_eq!(named.parse("01-Mar-2026"), Parsed::Date(date("2026-03-01")));
        assert_eq!(named.parse("1-mar-2026"), Parsed::Date(date("2026-03-01")));
        assert_eq!(
            named.parse("01-Mrz-2026"),
            Parsed::Invalid(DateError::Invalid)
        );
        assert_eq!(
            dmy.parse("01/03/1989"),
            Parsed::Invalid(DateError::OutOfRange)
        );
    }

    #[test]
    fn strictness_two_digit_years_padding_and_extra_parts_are_refused() {
        let dmy = fmt(Some(DateOrder::Dmy), Some('/'));
        for bad in [
            "01/03/26",
            "01/03/02026",
            "001/03/2026",
            "01/03/2026/1",
            "01-03-2026",
            " 01/03/2026",
            "01/03/2026 ",
            "o1/03/2026",
            "29/02/2026",
            "",
            "/",
            "١/03/2026",
        ] {
            assert!(matches!(dmy.parse(bad), Parsed::Invalid(_)), "{bad:?}");
        }
        assert_eq!(
            DateFormat::ISO.parse("2026-3-01"),
            Parsed::Invalid(DateError::Invalid)
        );
        assert_eq!(
            DateFormat::ISO.parse("2026-03-01"),
            Parsed::Date(date("2026-03-01"))
        );
    }

    #[test]
    fn an_undeclared_order_reports_which_readings_are_real_days() {
        let open = fmt(None, Some('/'));
        assert_eq!(
            open.parse("01/02/2026"),
            Parsed::Undeclared {
                dmy: true,
                mdy: true
            }
        );
        assert_eq!(
            open.parse("25/03/2026"),
            Parsed::Undeclared {
                dmy: true,
                mdy: false
            }
        );
        assert_eq!(
            open.parse("03/25/2026"),
            Parsed::Undeclared {
                dmy: false,
                mdy: true
            }
        );
        assert_eq!(
            open.parse("25/25/2026"),
            Parsed::Invalid(DateError::Invalid)
        );
        let q = DateQuestion {
            dmy: true,
            mdy: false,
        };
        assert_eq!(q.options(), ["dmy"]);
    }

    #[test]
    fn unusable_formats_are_named() {
        let mut problems = vec![];
        fmt(None, None).problems("date_format", &mut problems);
        fmt(Some(DateOrder::Dmy), None).problems("date_format", &mut problems);
        fmt(Some(DateOrder::Ymd), Some('x')).problems("date_format", &mut problems);
        DateFormat {
            month_names: true,
            ..fmt(None, Some('-'))
        }
        .problems("date_format", &mut problems);
        assert_eq!(problems.len(), 4, "{problems:?}");
        let mut none = vec![];
        DateFormat::ISO.problems("date_format", &mut none);
        assert_eq!(none, Vec::<String>::new());
    }

    proptest! {
        #[test]
        fn every_day_round_trips_through_every_declared_order(
            year in 1990_u16..=2100, month in 1_u8..=12, day in 1_u8..=28,
        ) {
            let d = Date::new(year, month, day).unwrap();
            let dmy = fmt(Some(DateOrder::Dmy), Some('.'));
            prop_assert_eq!(dmy.parse(&format!("{day}.{month}.{year}")), Parsed::Date(d));
            let mdy = fmt(Some(DateOrder::Mdy), Some('/'));
            prop_assert_eq!(mdy.parse(&format!("{month:02}/{day:02}/{year}")), Parsed::Date(d));
        }

        #[test]
        fn arbitrary_text_never_panics(text in "\\PC{0,16}") {
            for f in [fmt(None, Some('/')), fmt(Some(DateOrder::Ymd), None), DateFormat::ISO] {
                let _ = f.parse(&text);
            }
        }
    }
}
