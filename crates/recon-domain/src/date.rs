//! Calendar dates as statements carry them: no time, no zone. Parsed strictly from
//! `YYYY-MM-DD`; any other layout is a profile's job to declare (R1), never a guess.

use std::fmt;

/// The earliest year a statement date may carry.
pub const MIN_YEAR: u16 = 1990;
/// The latest year a statement date may carry.
pub const MAX_YEAR: u16 = 2100;

/// Why a date was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DateError {
    /// Not `YYYY-MM-DD` with ASCII digits, or not a real calendar day.
    #[error("invalid_date")]
    Invalid,
    /// A real day outside 1990–2100.
    #[error("date_out_of_range")]
    OutOfRange,
}

impl DateError {
    /// The stable refusal code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_date",
            Self::OutOfRange => "date_out_of_range",
        }
    }
}

/// A valid calendar day between [`MIN_YEAR`] and [`MAX_YEAR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

/// True for Gregorian leap years.
pub const fn is_leap_year(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// Days in `month` (1–12) of `year`; 0 for a month outside 1–12.
pub const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

impl Date {
    /// A date, refused when it is not a real day or is out of range.
    pub const fn new(year: u16, month: u8, day: u8) -> Result<Self, DateError> {
        if month < 1 || month > 12 || day < 1 || day > days_in_month(year, month) {
            return Err(DateError::Invalid);
        }
        if year < MIN_YEAR || year > MAX_YEAR {
            return Err(DateError::OutOfRange);
        }
        Ok(Self { year, month, day })
    }

    /// Parses exactly `YYYY-MM-DD` (ten ASCII bytes).
    pub fn parse_iso(text: &str) -> Result<Self, DateError> {
        let b = text.as_bytes();
        if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
            return Err(DateError::Invalid);
        }
        let year = digits(b.get(0..4))?;
        let month = digits(b.get(5..7))?;
        let day = digits(b.get(8..10))?;
        let year = u16::try_from(year).map_err(|_| DateError::Invalid)?;
        let month = u8::try_from(month).map_err(|_| DateError::Invalid)?;
        let day = u8::try_from(day).map_err(|_| DateError::Invalid)?;
        Self::new(year, month, day)
    }

    /// The year.
    pub const fn year(self) -> u16 {
        self.year
    }

    /// The month, 1–12.
    pub const fn month(self) -> u8 {
        self.month
    }

    /// The day of the month, 1–31.
    pub const fn day(self) -> u8 {
        self.day
    }

    /// `YYYY-MM-DD`.
    pub fn to_iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// The next calendar day, if it is still in range.
    pub fn succ(self) -> Option<Self> {
        if self.day < days_in_month(self.year, self.month) {
            return Self::new(self.year, self.month, self.day.saturating_add(1)).ok();
        }
        if self.month < 12 {
            return Self::new(self.year, self.month.saturating_add(1), 1).ok();
        }
        Self::new(self.year.saturating_add(1), 1, 1).ok()
    }
}

fn digits(bytes: Option<&[u8]>) -> Result<u32, DateError> {
    let bytes = bytes.ok_or(DateError::Invalid)?;
    let mut value: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return Err(DateError::Invalid);
        }
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u32::from(b.wrapping_sub(b'0'))))
            .ok_or(DateError::Invalid)?;
    }
    Ok(value)
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
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

    #[test]
    fn strict_iso_dates() {
        assert_eq!(
            Date::parse_iso("2026-02-28").unwrap().to_iso(),
            "2026-02-28"
        );
        assert_eq!(Date::parse_iso("2024-02-29").unwrap().day(), 29);
        for bad in [
            "2026-02-29",
            "2100-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-1-01",
            "26-01-01",
            "2026/01/01",
            "01/02/2026",
            "2026-01-01 ",
            "２０２６-01-01",
            "2026-01-0a",
            "",
        ] {
            assert_eq!(Date::parse_iso(bad), Err(DateError::Invalid), "{bad:?}");
        }
        assert_eq!(Date::parse_iso("1989-12-31"), Err(DateError::OutOfRange));
        assert_eq!(Date::parse_iso("2101-01-01"), Err(DateError::OutOfRange));
        assert_eq!(Date::parse_iso("2000-02-29").unwrap().year(), 2000);
    }

    #[test]
    fn succ_walks_the_calendar() {
        let mut d = Date::new(2023, 12, 30).unwrap();
        let mut seen = vec![];
        for _ in 0..4 {
            d = d.succ().unwrap();
            seen.push(d.to_iso());
        }
        assert_eq!(
            seen,
            ["2023-12-31", "2024-01-01", "2024-01-02", "2024-01-03"]
        );
        assert_eq!(Date::new(2100, 12, 31).unwrap().succ(), None);
    }

    proptest! {
        /// Every day of every month in range round-trips; one past the end is refused.
        #[test]
        fn calendar_round_trips(year in MIN_YEAR..=MAX_YEAR, month in 1_u8..=12) {
            let last = days_in_month(year, month);
            for day in 1..=last {
                let d = Date::new(year, month, day).unwrap();
                prop_assert_eq!(Date::parse_iso(&d.to_iso()), Ok(d));
            }
            prop_assert_eq!(Date::new(year, month, last + 1), Err(DateError::Invalid));
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".{0,14}") {
            let _ = Date::parse_iso(&text);
        }
    }
}
