//! Failure case 6 at the parser (D-066): every fixture in `fixtures/hostile-files/` is
//! refused with its code and row, and the generated large inputs — a 100 MiB line, 10^6
//! columns, 50 MiB of bad rows, 50 MiB of OFX — end within 2 s each. The same fixtures run
//! through the real server in `recon-server`'s `hostile` drill, which also bounds its memory
//! and checks nothing is persisted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests: a panic is the failure report"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use recon_domain::refusal::RefusalCode as C;
use recon_domain::{AccountId, Currency};
use recon_import::dates::DateFormat;
use recon_import::{
    AccountContext, CsvLimits, FileRefusal, OfxParser, ParseSummary, StatementParser,
    StatementProfile,
};

/// The bound failure case 6 sets on parsing a 50 MiB input.
const PARSE_BOUND: Duration = Duration::from_secs(2);

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../fixtures/hostile-files/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn ctx() -> AccountContext {
    AccountContext {
        account: AccountId::from_uuid(uuid::Uuid::from_u128(11)),
        currency: Currency::Usd,
    }
}

/// Any parser, driven over chunks; lines are counted, never kept.
trait Parse {
    fn push_chunk(&mut self, chunk: &[u8], lines: &mut usize) -> Result<(), FileRefusal>;
    fn end(self, lines: &mut usize) -> Result<ParseSummary, FileRefusal>;
}

impl Parse for StatementParser {
    fn push_chunk(&mut self, chunk: &[u8], lines: &mut usize) -> Result<(), FileRefusal> {
        self.push(chunk, &mut |_| *lines += 1)
    }
    fn end(self, lines: &mut usize) -> Result<ParseSummary, FileRefusal> {
        self.finish(&mut |_| *lines += 1)
    }
}

impl Parse for OfxParser {
    fn push_chunk(&mut self, chunk: &[u8], lines: &mut usize) -> Result<(), FileRefusal> {
        self.push(chunk, &mut |_| *lines += 1)
    }
    fn end(self, lines: &mut usize) -> Result<ParseSummary, FileRefusal> {
        self.finish(&mut |_| *lines += 1)
    }
}

/// Feeds `chunks` (an iterator, so a 100 MiB input is never held whole) and reports the
/// outcome, the lines handed on and the time taken.
fn drive<P: Parse>(
    mut parser: P,
    chunks: impl IntoIterator<Item = Vec<u8>>,
) -> (Result<ParseSummary, FileRefusal>, usize, Duration) {
    let started = Instant::now();
    let mut lines = 0;
    for chunk in chunks {
        if let Err(e) = parser.push_chunk(&chunk, &mut lines) {
            return (Err(e), lines, started.elapsed());
        }
    }
    let outcome = parser.end(&mut lines);
    (outcome, lines, started.elapsed())
}

fn whole(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes.chunks(7).map(<[u8]>::to_vec).collect()
}

/// How a fixture must be refused.
enum Expect {
    /// The whole file, with this code and row.
    File(C, Option<u32>),
    /// These rows, with these codes (and nothing handed on after the first).
    Rows(&'static [(u32, C)]),
}

fn check(name: &str, outcome: &Result<ParseSummary, FileRefusal>, expect: &Expect) {
    match (outcome, expect) {
        (Err(refusal), Expect::File(code, row)) => {
            assert_eq!((refusal.code, refusal.row), (*code, *row), "{name}");
        }
        (Ok(summary), Expect::Rows(rows)) => {
            let found: Vec<(u32, C)> = summary.refusals.iter().map(|r| (r.row, r.code)).collect();
            assert_eq!(found, rows.to_vec(), "{name}");
            assert!(!summary.is_clean(), "{name} must be refused");
        }
        (other, _) => panic!("{name}: unexpected outcome {other:?}"),
    }
}

/// The table `fixtures/hostile-files/README.md` describes.
const CSV_CASES: [(&str, Expect); 11] = [
    (
        "csv-truncated-row.csv",
        Expect::Rows(&[(3, C::UnterminatedQuote)]),
    ),
    (
        "csv-wrong-encoding.csv",
        Expect::Rows(&[(2, C::InvalidUtf8)]),
    ),
    (
        "csv-ambiguous-date.csv",
        Expect::Rows(&[(2, C::InvalidDate)]),
    ),
    (
        "csv-twenty-digit-amount.csv",
        Expect::Rows(&[(2, C::AmountOutOfRange)]),
    ),
    (
        "csv-formula-cell.csv",
        Expect::Rows(&[(2, C::FormulaCell), (3, C::FormulaCell)]),
    ),
    ("csv-utf32.csv", Expect::File(C::UnsupportedEncoding, None)),
    (
        "csv-bare-cr.csv",
        Expect::Rows(&[(2, C::BareCarriageReturn)]),
    ),
    (
        "csv-balance-discontinuity.csv",
        Expect::Rows(&[(3, C::BalanceDiscontinuity)]),
    ),
    (
        "csv-nul-byte.csv",
        Expect::Rows(&[(2, C::ForbiddenCharacter)]),
    ),
    (
        "csv-bidi-override.csv",
        Expect::Rows(&[(2, C::ForbiddenCharacter)]),
    ),
    (
        "csv-header-mismatch.csv",
        Expect::File(C::HeaderMismatch, Some(1)),
    ),
];

const OFX_CASES: [(&str, C); 8] = [
    ("ofx-billion-laughs.ofx", C::DeclarationRefused),
    ("ofx-external-entity.ofx", C::DeclarationRefused),
    ("ofx-doctype.ofx", C::DeclarationRefused),
    ("ofx-comment.ofx", C::DeclarationRefused),
    ("ofx-unknown-entity.ofx", C::EntityRefused),
    ("ofx-deep-nesting.ofx", C::MalformedOfx),
    ("ofx-two-statements.ofx", C::MultipleStatements),
    ("ofx-truncated.ofx", C::MalformedOfx),
];

#[test]
fn every_hostile_fixture_is_refused_with_its_code_and_row() {
    for (name, expect) in &CSV_CASES {
        let (outcome, lines, _) = drive(StatementParser::new(ctx()), whole(&fixture(name)));
        check(name, &outcome, expect);
        // Only rows before the first refusal are ever handed on.
        assert!(lines <= 1, "{name}: {lines} lines handed on");
    }
    for (name, code) in &OFX_CASES {
        let (outcome, lines, _) = drive(OfxParser::new(ctx()), whole(&fixture(name)));
        check(name, &outcome, &Expect::File(*code, None));
        // A second statement is found after the first one's rows: the file is still
        // refused whole, and the server stages nothing from it (the hostile drill).
        assert!(lines <= 1, "{name}: {lines} lines handed on");
    }
}

/// The ambiguous-date fixture under a profile that leaves the order open: the question,
/// not a guess.
#[test]
fn the_ambiguous_date_fixture_is_a_question() {
    let mut open = StatementProfile::r0();
    open.date_format = DateFormat {
        order: None,
        separator: Some('/'),
        month_names: false,
        padded: false,
    };
    let parser = StatementParser::with_profile(
        ctx(),
        Arc::new(open.compile().unwrap()),
        CsvLimits::default(),
    );
    let (outcome, lines, _) = drive(parser, whole(&fixture("csv-ambiguous-date.csv")));
    let refusal = outcome.unwrap_err();
    assert_eq!(
        (refusal.code, refusal.row),
        (C::AmbiguousDateFormat, Some(2))
    );
    assert_eq!(refusal.question.unwrap().options(), ["dmy", "mdy"]);
    assert_eq!(lines, 0);
}

const HEAD: &[u8] =
    b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";

/// `n` chunks of `chunk` after `first`.
fn stream(first: &[u8], chunk: Vec<u8>, n: usize) -> impl Iterator<Item = Vec<u8>> {
    std::iter::once(first.to_vec()).chain(std::iter::repeat_n(chunk, n))
}

#[test]
fn a_100_mib_line_stops_at_the_upload_bound_in_time() {
    // 100 MiB without a terminator, in 64 KiB chunks: refused once 50 MiB have been read.
    let (outcome, lines, took) = drive(
        StatementParser::new(ctx()),
        stream(HEAD, vec![b'a'; 64 * 1024], 1_600),
    );
    assert_eq!(
        outcome.unwrap_err(),
        FileRefusal::new(C::FileTooLarge, None)
    );
    assert_eq!(lines, 0);
    assert!(took < PARSE_BOUND, "50 MiB of one line took {took:?}");
}

#[test]
fn a_million_columns_is_one_refused_row() {
    let mut row = b"a,".repeat(1_000_000);
    row.push(b'\n');
    let (outcome, _, took) = drive(StatementParser::new(ctx()), [HEAD.to_vec(), row]);
    check(
        "10^6 columns",
        &outcome,
        &Expect::Rows(&[(2, C::TooManyFields)]),
    );
    assert!(took < PARSE_BOUND, "{took:?}");
}

#[test]
fn fifty_mib_of_bad_rows_is_counted_reported_and_ends_in_time() {
    // About 800,000 rows, each with an amount in exponent notation, then one good row.
    let bad = b"2026-03-01,2026-03-01,1e3,USD,HOSTILE ROW WITH A LONG NARRATION,REF,CP,\n";
    let per_chunk = (64 * 1024) / bad.len();
    let chunks = (50 * 1024 * 1024) / (per_chunk * bad.len());
    let chunk = bad.repeat(per_chunk);
    let (outcome, lines, took) = drive(StatementParser::new(ctx()), stream(HEAD, chunk, chunks));
    let summary = outcome.unwrap();
    let rows = u32::try_from(per_chunk * chunks).unwrap();
    assert_eq!((summary.rows_read, summary.rows_refused), (rows, rows));
    assert_eq!(
        summary.refusals.len(),
        1_000,
        "the report lists 1,000 and counts all"
    );
    assert_eq!(lines, 0);
    assert!(summary.file_bytes > 49 * 1024 * 1024);
    assert!(took < PARSE_BOUND, "50 MiB of bad rows took {took:?}");
}

#[test]
fn fifty_mib_of_hostile_ofx_ends_in_time() {
    let head = b"OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nENCODING:USASCII\nCHARSET:1252\n\n<OFX><BANKMSGSRSV1><STMTTRNRS><STMTRS><CURDEF>USD<BANKTRANLIST><STMTTRN><NAME>";
    // One value of 50 MiB of entity references: bounded at 8 KiB, nothing expanded.
    let (outcome, lines, took) = drive(
        OfxParser::new(ctx()),
        stream(head, b"&amp;".repeat(13_107), 800),
    );
    assert!(outcome.is_err() || !outcome.as_ref().unwrap().is_clean());
    assert_eq!(lines, 0);
    assert!(took < PARSE_BOUND, "50 MiB of OFX took {took:?}");
}
