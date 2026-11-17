//! The OFX fixtures under `fixtures/` (D-063, D-066): two well-formed statements, one per
//! format, and the hostile files, each refused with its code before a line is handed on.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "integration tests: a panic is the failure report"
)]

use std::path::PathBuf;

use recon_domain::refusal::RefusalCode;
use recon_domain::{AccountId, Currency};
use recon_import::{AccountContext, FileRefusal, OfxParser, ParseSummary};
use uuid::Uuid;

fn fixture(path: &str) -> Vec<u8> {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(path);
    std::fs::read(&file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()))
}

fn parse(
    path: &str,
) -> (
    Result<ParseSummary, FileRefusal>,
    Vec<recon_domain::import::NewLine>,
) {
    let bytes = fixture(path);
    let mut parser = OfxParser::new(AccountContext {
        account: AccountId::from_uuid(Uuid::from_u128(0x0F)),
        currency: Currency::Usd,
    });
    let mut lines = vec![];
    let mut push = |l| lines.push(l);
    for chunk in bytes.chunks(97) {
        if let Err(e) = parser.push(chunk, &mut push) {
            return (Err(e), lines);
        }
    }
    let summary = parser.finish(&mut push);
    (summary, lines)
}

#[test]
fn the_sgml_and_xml_statements_read_whole() {
    let (summary, lines) = parse("ofx/sgml-1x.ofx");
    let summary = summary.unwrap();
    assert!(summary.is_clean(), "{:?}", summary.refusals);
    assert_eq!((summary.rows_read, lines.len()), (3, 3));
    assert_eq!(lines[0].narration_raw, "Fixture Café Supplies INV 77");
    assert_eq!(lines[0].amount.minor(), -12_550);
    assert_eq!(
        lines[1]
            .value_date
            .map(recon_domain::Date::to_iso)
            .as_deref(),
        Some("2026-03-06")
    );
    assert_eq!(lines[2].counterparty_raw, None);

    let (summary, lines) = parse("ofx/xml-2x.ofx");
    let summary = summary.unwrap();
    assert!(summary.is_clean(), "{:?}", summary.refusals);
    let narrations: Vec<_> = lines.iter().map(|l| l.narration_raw.as_str()).collect();
    assert_eq!(
        narrations,
        [
            "Fixture Phone & Data",
            "Fixture Refunds Order 12",
            "Card fee"
        ]
    );
    assert_eq!(
        lines[1].counterparty_raw.as_deref(),
        Some("Fixture Refunds")
    );
    let references: Vec<_> = lines.iter().map(|l| l.reference.as_deref()).collect();
    assert_eq!(
        references,
        [Some("XMLFIX01"), Some("XMLFIX02"), Some("XMLFIX03")]
    );
}

#[test]
fn every_hostile_ofx_fixture_is_refused_with_its_code() {
    use RefusalCode as C;
    for (path, code) in [
        (
            "hostile-files/ofx-billion-laughs.ofx",
            C::DeclarationRefused,
        ),
        (
            "hostile-files/ofx-external-entity.ofx",
            C::DeclarationRefused,
        ),
        ("hostile-files/ofx-doctype.ofx", C::DeclarationRefused),
        ("hostile-files/ofx-comment.ofx", C::DeclarationRefused),
        ("hostile-files/ofx-unknown-entity.ofx", C::EntityRefused),
        ("hostile-files/ofx-deep-nesting.ofx", C::MalformedOfx),
        (
            "hostile-files/ofx-two-statements.ofx",
            C::MultipleStatements,
        ),
        ("hostile-files/ofx-truncated.ofx", C::MalformedOfx),
    ] {
        let (outcome, lines) = parse(path);
        let refusal = outcome.expect_err(path);
        assert_eq!((refusal.code, refusal.row), (code, None), "{path}");
        if code != C::MultipleStatements && code != C::MalformedOfx {
            assert!(lines.is_empty(), "{path}: a line was handed on");
        }
    }
}
