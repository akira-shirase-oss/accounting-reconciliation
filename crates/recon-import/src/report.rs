//! Refusal reports an accountant can act on (D-068): for every refused row, where it is
//! (the line of the file, or the transaction of an OFX statement), which column (by the
//! profile's own header name), what is wrong and how to fix it, with the profile's date and
//! number formats spelled out; for a refused file, the same for the file. The API returns
//! the report with every refusal and `recon import check` prints it before an upload. It
//! names columns and formats, never the file's content.

use std::fmt::Write as _;

use serde::Serialize;

use recon_domain::Field;
use recon_domain::refusal::{RefusalCode, RowRefusal};

use crate::csv::FileRefusal;
use crate::dates::{DateFormat, DateOrder};
use crate::documents::CompiledDocuments;
use crate::number::{GroupStyle, Negative, NumberFormat};
use crate::profile::{AmountColumns, CompiledStatement};

/// The profile a file was read with, as a report needs it.
#[derive(Debug, Clone, Copy)]
pub enum ProfileView<'a> {
    /// A CSV statement profile.
    Statement(&'a CompiledStatement),
    /// A CSV document profile.
    Documents(&'a CompiledDocuments),
    /// The OFX profile.
    Ofx,
    /// A profile that can no longer be read (columns are named by field).
    Unknown,
}

/// An owned profile for a report (what a server keeps across an upload).
#[derive(Debug, Clone)]
pub enum ProfileRef {
    /// A CSV statement profile.
    Statement(std::sync::Arc<CompiledStatement>),
    /// A CSV document profile.
    Documents(std::sync::Arc<CompiledDocuments>),
    /// The OFX profile.
    Ofx,
    /// Unknown.
    Unknown,
}

impl ProfileRef {
    /// The view a report reads.
    pub fn view(&self) -> ProfileView<'_> {
        match self {
            Self::Statement(p) => ProfileView::Statement(p),
            Self::Documents(p) => ProfileView::Documents(p),
            Self::Ofx => ProfileView::Ofx,
            Self::Unknown => ProfileView::Unknown,
        }
    }
}

impl ProfileView<'_> {
    fn header(&self) -> Option<&[String]> {
        match self {
            Self::Statement(p) => Some(&p.profile().header),
            Self::Documents(p) => Some(&p.profile().header),
            Self::Ofx | Self::Unknown => None,
        }
    }

    fn date_format(&self) -> Option<DateFormat> {
        match self {
            Self::Statement(p) => Some(p.profile().date_format),
            Self::Documents(p) => Some(p.profile().date_format),
            Self::Ofx | Self::Unknown => None,
        }
    }

    fn number_format(&self) -> Option<&NumberFormat> {
        match self {
            Self::Statement(p) => Some(&p.profile().number_format),
            Self::Documents(p) => Some(&p.profile().number_format),
            Self::Ofx | Self::Unknown => None,
        }
    }

    const fn is_ofx(&self) -> bool {
        matches!(self, Self::Ofx)
    }

    /// The profile's column (or OFX element) for a field.
    fn column(&self, field: Field) -> Option<String> {
        let amount = |a: &AmountColumns| match a {
            AmountColumns::Signed(c) => c.clone(),
            AmountColumns::Split {
                money_out,
                money_in,
            } => format!("{money_out} / {money_in}"),
        };
        let split = |a: &AmountColumns, out: bool| match a {
            AmountColumns::Split {
                money_out,
                money_in,
            } => Some(if out { money_out } else { money_in }.clone()),
            AmountColumns::Signed(c) => Some(c.clone()),
        };
        match self {
            Self::Statement(p) => {
                let c = &p.profile().columns;
                match field {
                    Field::BookingDate => Some(c.booking_date.clone()),
                    Field::ValueDate => c.value_date.clone(),
                    Field::Amount => Some(amount(&c.amount)),
                    Field::MoneyOut => split(&c.amount, true),
                    Field::MoneyIn => split(&c.amount, false),
                    Field::Currency => c.currency.clone(),
                    Field::Narration => Some(c.narration.clone()),
                    Field::Reference => c.reference.clone(),
                    Field::Counterparty => c.counterparty.clone(),
                    Field::BalanceAfter => c.balance_after.clone(),
                    _ => None,
                }
            }
            Self::Documents(p) => {
                let c = &p.profile().columns;
                match field {
                    Field::Kind => c.kind.clone(),
                    Field::Number => c.number.clone(),
                    Field::IssueDate => Some(c.issue_date.clone()),
                    Field::DueDate => c.due_date.clone(),
                    Field::Amount => Some(amount(&c.amount)),
                    Field::MoneyOut => split(&c.amount, true),
                    Field::MoneyIn => split(&c.amount, false),
                    Field::Currency => c.currency.clone(),
                    Field::Counterparty => c.counterparty.clone(),
                    Field::Description => c.description.clone(),
                    _ => None,
                }
            }
            Self::Ofx => Some(
                match field {
                    Field::BookingDate => "DTPOSTED",
                    Field::ValueDate => "DTAVAIL",
                    Field::Amount => "TRNAMT",
                    Field::Reference => "FITID",
                    Field::Narration => "NAME / MEMO",
                    Field::Counterparty => "NAME",
                    Field::Currency => "CURDEF",
                    _ => return None,
                }
                .to_owned(),
            ),
            Self::Unknown => None,
        }
    }
}

/// How a date format reads, as a pattern: `DD/MM/YYYY`, `D-Mon-YYYY`, `YYYYMMDD`.
pub fn describe_date(f: DateFormat) -> String {
    let sep = f.separator.map(String::from).unwrap_or_default();
    let (d, m) = if f.padded { ("DD", "MM") } else { ("D", "M") };
    let m = if f.month_names { "Mon" } else { m };
    let parts = |order: DateOrder| match order {
        DateOrder::Ymd => ["YYYY", m, d],
        DateOrder::Dmy => [d, m, "YYYY"],
        DateOrder::Mdy => [m, d, "YYYY"],
    };
    match f.order {
        Some(order) => {
            let p = parts(order);
            let pattern = p.join(&sep);
            if f.padded || f.month_names {
                pattern
            } else {
                format!("{pattern} (day and month in one or two digits)")
            }
        }
        None => format!(
            "{} or {} (the order is not declared)",
            parts(DateOrder::Dmy).join(&sep),
            parts(DateOrder::Mdy).join(&sep)
        ),
    }
}

/// How a number format reads, by example: `like 12,34,567.89; negatives as (1.00)`.
pub fn describe_number(f: &NumberFormat) -> String {
    let group = |digits: &str| -> String {
        let Some(mark) = f.grouping else {
            return digits.to_owned();
        };
        let (head, last) = digits.split_at(digits.len().saturating_sub(3));
        let mut groups = vec![last.to_owned()];
        let mut rest = head;
        let size = match f.group_style {
            GroupStyle::Western => 3,
            GroupStyle::Indian => 2,
        };
        while !rest.is_empty() {
            let cut = rest.len().saturating_sub(size);
            groups.insert(0, rest.get(cut..).unwrap_or_default().to_owned());
            rest = rest.get(..cut).unwrap_or_default();
        }
        groups.join(&mark.to_string())
    };
    let example = format!("{}{}89", group("1234567"), f.decimal);
    let one = format!("1{}00", f.decimal);
    let negatives: Vec<String> = f
        .negatives
        .iter()
        .map(|n| match n {
            Negative::Minus => format!("-{one}"),
            Negative::Parentheses => format!("({one})"),
            Negative::TrailingMinus => format!("{one}-"),
        })
        .collect();
    let signs = if negatives.is_empty() {
        "no negative sign (magnitudes)".to_owned()
    } else {
        format!("negatives as {}", negatives.join(" or "))
    };
    format!("like {example}, no currency symbol; {signs}")
}

/// One problem in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// The record number (the header is 1), or the OFX transaction's position.
    pub row: Option<u32>,
    /// Plain words for where: `line 14`, `transaction 3`, `the whole file`.
    pub location: String,
    /// The profile's column (or OFX element) at fault, when one is.
    pub column: Option<String>,
    /// The stable code.
    pub code: &'static str,
    /// What is wrong.
    pub problem: String,
    /// How to fix it.
    pub fix: String,
}

/// A refusal report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// One sentence: what happened and what to do.
    pub summary: String,
    /// The problems, in file order (at most the first 1,000 refused rows).
    pub problems: Vec<Problem>,
    /// Refused rows counted but not listed.
    pub unlisted: u32,
}

fn location(row: Option<u32>, view: &ProfileView<'_>) -> String {
    match row {
        Some(r) if view.is_ofx() => format!("transaction {r}"),
        Some(1) => "line 1 (the header)".to_owned(),
        Some(r) => format!("line {r}"),
        None => "the whole file".to_owned(),
    }
}

/// The problem and fix for `code`, with the profile's formats where they help.
fn explain(code: RefusalCode, view: &ProfileView<'_>) -> (String, String) {
    let (problem, fix) = code.explain();
    let mut problem = problem.to_owned();
    let mut fix = fix.to_owned();
    match code {
        RefusalCode::InvalidDate | RefusalCode::DateOutOfRange => {
            if let Some(f) = view.date_format() {
                let _ = write!(problem, " The profile reads dates as {}.", describe_date(f));
            } else if view.is_ofx() {
                problem.push_str(" OFX dates are YYYYMMDD, optionally with a time.");
            }
        }
        RefusalCode::InvalidAmount
        | RefusalCode::InvalidBalance
        | RefusalCode::UnexpectedSign
        | RefusalCode::TooManyDecimalPlaces => {
            if let Some(f) = view.number_format() {
                let _ = write!(
                    problem,
                    " The profile reads amounts {}.",
                    describe_number(f)
                );
            }
        }
        RefusalCode::HeaderMismatch | RefusalCode::FieldCountMismatch => {
            if let Some(h) = view.header() {
                let _ = write!(
                    fix,
                    " The profile's header has {} columns: {}.",
                    h.len(),
                    h.join(",")
                );
            }
        }
        _ => {}
    }
    (problem, fix)
}

/// `1204` as `1,204`.
fn grouped(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len().saturating_add(4));
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && digits.len().saturating_sub(i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The report for a file whose rows were refused.
pub fn rows_report(
    rows_read: u32,
    rows_refused: u32,
    refusals: &[RowRefusal],
    view: &ProfileView<'_>,
) -> Report {
    let problems: Vec<Problem> = refusals
        .iter()
        .map(|r| {
            let (problem, fix) = explain(r.code, view);
            Problem {
                row: Some(r.row),
                location: location(Some(r.row), view),
                column: r
                    .field
                    .and_then(|f| view.column(f).or_else(|| Some(f.label().to_owned()))),
                code: r.code.as_str(),
                problem,
                fix,
            }
        })
        .collect();
    let listed = u32::try_from(problems.len()).unwrap_or(u32::MAX);
    let unlisted = rows_refused.saturating_sub(listed);
    let (verb, them) = if rows_refused == 1 {
        ("has", "it")
    } else {
        ("have", "them")
    };
    let mut summary = format!(
        "Nothing was imported: {} of {} rows {verb} a problem. Fix {them} in the file and upload the whole file again; a file is imported whole or not at all.",
        grouped(rows_refused),
        grouped(rows_read),
    );
    if unlisted > 0 {
        let _ = write!(summary, " The first {listed} are listed.");
    }
    Report {
        summary,
        problems,
        unlisted,
    }
}

/// The report for a file refused whole.
pub fn file_report(refusal: &FileRefusal, view: &ProfileView<'_>) -> Report {
    let (mut problem, mut fix) = explain(refusal.code, view);
    if let Some(q) = refusal.question {
        let options = q.options();
        let named: Vec<String> = options
            .iter()
            .map(|o| match *o {
                "dmy" => "day-month-year (\"dmy\")".to_owned(),
                _ => "month-day-year (\"mdy\")".to_owned(),
            })
            .collect();
        if named.is_empty() {
            problem.push_str(" No reading fits every date in the file.");
        } else {
            let _ = write!(
                problem,
                " Every date in the file reads as {}.",
                named.join(" and as ")
            );
        }
        if let [only] = options.as_slice() {
            fix = format!(
                "Create a new version of the profile with \"order\": \"{only}\" in date_format (the only order every date fits), then upload the file again."
            );
        }
    }
    let summary =
        "Nothing was imported: the whole file was refused. Fix the problem below and upload it again."
            .to_owned();
    Report {
        summary,
        problems: vec![Problem {
            row: refusal.row,
            location: location(refusal.row, view),
            column: None,
            code: refusal.code.as_str(),
            problem,
            fix,
        }],
        unlisted: 0,
    }
}

impl Report {
    /// The report as text for a terminal, headed by the file's name and the profile it was
    /// read with.
    pub fn to_text(&self, file: &str, profile: &str) -> String {
        let mut out = format!("{file}: refused (read with {profile}).\n{}\n", self.summary);
        for p in &self.problems {
            let column = p
                .column
                .as_ref()
                .map(|c| format!(", column \"{c}\""))
                .unwrap_or_default();
            let _ = write!(
                out,
                "\n  {}{column} [{}]\n    What is wrong: {}\n    How to fix:    {}\n",
                p.location, p.code, p.problem, p.fix
            );
        }
        if self.unlisted > 0 {
            let _ = write!(
                out,
                "\n  …and {} more refused rows, not listed.\n",
                self.unlisted
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;
    use crate::dates::DateQuestion;
    use crate::profile::StatementProfile;

    #[test]
    fn formats_are_described_as_an_accountant_reads_them() {
        assert_eq!(describe_date(DateFormat::ISO), "YYYY-MM-DD");
        let dmy = DateFormat {
            order: Some(DateOrder::Dmy),
            separator: Some('/'),
            month_names: false,
            padded: false,
        };
        assert_eq!(
            describe_date(dmy),
            "D/M/YYYY (day and month in one or two digits)"
        );
        assert_eq!(
            describe_date(DateFormat { order: None, ..dmy }),
            "D/M/YYYY or M/D/YYYY (the order is not declared)"
        );
        let indian = NumberFormat {
            decimal: '.',
            grouping: Some(','),
            group_style: GroupStyle::Indian,
            negatives: vec![Negative::Parentheses],
            plus_sign: false,
        };
        assert_eq!(
            describe_number(&indian),
            "like 12,34,567.89, no currency symbol; negatives as (1.00)"
        );
        assert_eq!(
            describe_number(&NumberFormat::plain()),
            "like 1234567.89, no currency symbol; negatives as -1.00"
        );
    }

    #[test]
    fn a_row_report_names_line_column_problem_and_fix() {
        let p = StatementProfile::r0().compile().unwrap();
        let view = ProfileView::Statement(&p);
        let refusals = [
            RowRefusal::new(14, RefusalCode::InvalidDate).at(Field::ValueDate),
            RowRefusal::new(20, RefusalCode::FieldCountMismatch),
        ];
        let r = rows_report(1204, 2, &refusals, &view);
        assert!(
            r.summary
                .starts_with("Nothing was imported: 2 of 1,204 rows have a problem"),
            "{}",
            r.summary
        );
        assert_eq!(r.problems[0].location, "line 14");
        assert_eq!(r.problems[0].column.as_deref(), Some("value_date"));
        assert!(r.problems[0].problem.contains("YYYY-MM-DD"));
        assert!(
            r.problems[1]
                .fix
                .contains("8 columns: booking_date,value_date")
        );
        assert_eq!(r.unlisted, 0);
        let text = r.to_text("march.csv", "r0-generic-csv");
        assert!(
            text.contains("line 14, column \"value_date\" [invalid_date]"),
            "{text}"
        );
        assert!(text.contains("How to fix:"));
    }

    #[test]
    fn an_ambiguous_date_report_asks_the_question_with_its_answer() {
        let p = StatementProfile::r0().compile().unwrap();
        let refusal = FileRefusal {
            code: RefusalCode::AmbiguousDateFormat,
            row: Some(2),
            question: Some(DateQuestion {
                dmy: true,
                mdy: false,
            }),
        };
        let r = file_report(&refusal, &ProfileView::Statement(&p));
        assert!(r.problems[0].problem.contains("day-month-year (\"dmy\")"));
        assert!(
            r.problems[0].fix.contains("\"order\": \"dmy\""),
            "{}",
            r.problems[0].fix
        );
        assert_eq!(r.problems[0].location, "line 2");
        let ofx = file_report(
            &FileRefusal::new(RefusalCode::DeclarationRefused, None),
            &ProfileView::Ofx,
        );
        assert_eq!(ofx.problems[0].location, "the whole file");
    }
}
