#![allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "tests: a panic is the failure report"
)]

use super::*;
use proptest::prelude::*;
use recon_domain::{DocumentId, LineId};
use uuid::Uuid;

fn inr(minor: i64) -> Money {
    Money::new(minor, Currency::Inr).unwrap()
}

fn date(day: u8) -> Date {
    Date::new(2026, 3, day).unwrap()
}

fn line(n: u128, day: u8, minor: i64, narration: &str, reference: Option<&str>) -> LineInput {
    LineInput {
        id: LineId::from_uuid(Uuid::from_u128(n)),
        booking_date: date(day),
        amount: inr(minor),
        narration_norm: normalize(narration),
        reference: reference.map(str::to_owned),
        counterparty_norm: None,
    }
}

fn invoice(n: u128, day: u8, minor: i64, number: &str, counterparty: &str) -> DocumentInput {
    DocumentInput {
        id: DocumentId::from_uuid(Uuid::from_u128(1_000 + n)),
        kind: DocumentKind::SalesInvoice,
        number_norm: Some(normalize(number)),
        issue_date: date(day),
        due_date: None,
        open_amount: inr(minor),
        counterparty_norm: Some(normalize(counterparty)),
    }
}

fn outcome(run: &Run, n: u128) -> &Outcome {
    &run.outcomes[&LineId::from_uuid(Uuid::from_u128(n))]
}

fn tier_of(o: &Outcome) -> Option<(Tier, bool)> {
    match o {
        Outcome::Matched { tier, commit, .. } => Some((*tier, *commit)),
        Outcome::Exception { .. } => None,
    }
}

fn kind_of(o: &Outcome) -> Option<ExceptionKind> {
    match o {
        Outcome::Exception { kind, .. } => Some(*kind),
        Outcome::Matched { .. } => None,
    }
}

#[test]
fn t1_needs_amount_reference_and_uniqueness_and_may_commit_only_if_allowed() {
    let docs = [
        invoice(1, 1, 118_000, "INV-117", "Granite Builders"),
        invoice(2, 2, 118_000, "INV-118", "Deccan Spices"),
    ];
    let lines = [line(1, 4, 118_000, "NEFT CR GRANITE INV-117", None)];
    let run1 = run(&lines, &docs, &Settings::default());
    assert_eq!(tier_of(outcome(&run1, 1)), Some((Tier::T1, true)));
    if let Outcome::Matched { score, .. } = outcome(&run1, 1) {
        assert_eq!((score.exact_amount, score.exact_reference), (40, 30));
    }
    let off = Settings {
        auto_commit_t1: false,
        ..Settings::default()
    };
    assert_eq!(
        tier_of(outcome(&run(&lines, &docs, &off), 1)),
        Some((Tier::T1, false)),
        "with auto-commit off, T1 is a proposal"
    );
    // A reference inside a slash-separated narration counts too.
    let slashed = [line(2, 4, 118_000, "UPI/DECCAN/INV-118/X", None)];
    assert_eq!(
        tier_of(outcome(&run(&slashed, &docs, &Settings::default()), 2)),
        Some((Tier::T1, true))
    );
}

#[test]
fn two_equal_invoices_without_a_reference_are_never_guessed() {
    let docs = [
        invoice(1, 1, 50_000, "INV-1", "Acme"),
        invoice(2, 1, 50_000, "INV-2", "Acme"),
    ];
    let lines = [line(1, 3, 50_000, "NEFT ACME", None)];
    let r = run(&lines, &docs, &Settings::default());
    assert_eq!(
        kind_of(outcome(&r, 1)),
        Some(ExceptionKind::MultipleCandidates)
    );
}

#[test]
fn t2_needs_the_counterparty_otherwise_the_narration_is_needed() {
    let docs = [invoice(1, 1, 75_000, "INV-9", "Indigo Print")];
    let named = [line(1, 5, 75_000, "CHQ DEP INDIGO PRINT", None)];
    assert_eq!(
        tier_of(outcome(&run(&named, &docs, &Settings::default()), 1)),
        Some((Tier::T2, false))
    );
    let anonymous = [line(2, 5, 75_000, "CASH DEPOSIT", None)];
    assert_eq!(
        kind_of(outcome(&run(&anonymous, &docs, &Settings::default()), 2)),
        Some(ExceptionKind::NarrationNeeded)
    );
}

#[test]
fn a_reference_with_another_amount_is_an_amount_mismatch() {
    let docs = [invoice(1, 1, 75_000, "INV-9", "Indigo Print")];
    let lines = [line(1, 5, 70_000, "PART PAYMENT INV-9", None)];
    assert_eq!(
        kind_of(outcome(&run(&lines, &docs, &Settings::default()), 1)),
        Some(ExceptionKind::AmountMismatch)
    );
}

#[test]
fn an_exact_combination_is_a_t3_proposal() {
    let docs = [
        invoice(1, 1, 30_000, "INV-1", "Acme"),
        invoice(2, 2, 20_000, "INV-2", "Acme"),
        invoice(3, 3, 45_000, "INV-3", "Acme"),
    ];
    let lines = [line(1, 6, 50_000, "NEFT ACME", None)];
    let r = run(&lines, &docs, &Settings::default());
    let Outcome::Matched {
        tier,
        matched,
        commit,
        ..
    } = outcome(&r, 1)
    else {
        panic!("{:?}", outcome(&r, 1));
    };
    assert_eq!((*tier, *commit), (Tier::T3, false));
    assert_eq!(matched.documents().len(), 2);
}

/// The ROADMAP's gate case: one payment for three invoices less a bank fee is a many-to-one
/// proposal with the residual flagged, never committed automatically.
#[test]
fn three_invoices_less_a_bank_fee_is_a_four_eyes_proposal() {
    let docs = [
        invoice(1, 1, 30_000, "INV-1", "Acme"),
        invoice(2, 2, 20_000, "INV-2", "Acme"),
        invoice(3, 3, 45_000, "INV-3", "Acme"),
    ];
    let lines = [line(1, 6, 94_850, "NEFT ACME", None)];
    // Tolerances are zero until the firm sets them: no residual is ever absorbed.
    assert_eq!(
        kind_of(outcome(&run(&lines, &docs, &Settings::default()), 1)),
        Some(ExceptionKind::AmountMismatch)
    );
    let mut settings = Settings::default();
    settings.tolerances.insert(
        Currency::Inr,
        Tolerances {
            bank_fee: 500,
            ..Tolerances::default()
        },
    );
    let r = run(&lines, &docs, &settings);
    let Outcome::Matched {
        tier,
        matched,
        commit,
        ..
    } = outcome(&r, 1)
    else {
        panic!("{:?}", outcome(&r, 1));
    };
    assert_eq!((*tier, *commit), (Tier::T4, false));
    assert_eq!(matched.documents().len(), 3);
    assert_eq!(matched.residuals()[0].code, ResidualCode::BankFee);
    assert_eq!(matched.residuals()[0].amount, inr(-150));
    assert!(matched.needs_four_eyes());
}

#[test]
fn a_document_two_lines_would_take_holds_both_whatever_their_order() {
    let docs = [invoice(1, 1, 10_000, "INV-1", "Acme")];
    let lines = [
        line(1, 2, 10_000, "NEFT ACME INV-1", None),
        line(2, 3, 10_000, "NEFT ACME INV-1", None),
    ];
    let r = run(&lines, &docs, &Settings::default());
    for n in [1, 2] {
        assert_eq!(
            kind_of(outcome(&r, n)),
            Some(ExceptionKind::MultipleCandidates)
        );
    }
}

#[test]
fn windows_currencies_and_directions() {
    let docs = [invoice(1, 1, 10_000, "INV-1", "Acme")];
    // 7 days before the issue date is in; 8 is out; due date + 90 is the far end.
    let on = |month: u8, day: u8| {
        [LineInput {
            booking_date: Date::new(2026, month, day).unwrap(),
            ..line(1, 1, 10_000, "INV-1", None)
        }]
    };
    assert_eq!(
        kind_of(outcome(&run(&on(2, 21), &docs, &Settings::default()), 1)),
        Some(ExceptionKind::NoCandidate),
        "8 days before the issue date is out"
    );
    assert_eq!(
        tier_of(outcome(&run(&on(2, 22), &docs, &Settings::default()), 1)),
        Some((Tier::T1, true)),
        "7 days before is in"
    );
    assert_eq!(
        tier_of(outcome(&run(&on(5, 30), &docs, &Settings::default()), 1)),
        Some((Tier::T1, true)),
        "the due date (here the issue date) + 90 days is in"
    );
    assert_eq!(
        kind_of(outcome(&run(&on(5, 31), &docs, &Settings::default()), 1)),
        Some(ExceptionKind::NoCandidate)
    );
    let usd_line = [LineInput {
        amount: Money::new(10_000, Currency::Usd).unwrap(),
        ..line(2, 2, 10_000, "INV-1 ACME", None)
    }];
    assert_eq!(
        kind_of(outcome(&run(&usd_line, &docs, &Settings::default()), 2)),
        Some(ExceptionKind::CrossCurrency)
    );
    let payment = [line(3, 2, -10_000, "INV-1 ACME", None)];
    assert_eq!(
        kind_of(outcome(&run(&payment, &docs, &Settings::default()), 3)),
        Some(ExceptionKind::NoCandidate),
        "a payment never matches a sales invoice"
    );
}

#[test]
fn the_worst_case_search_ends_in_its_budget_and_says_so() {
    // 40 invoices of one counterparty, every amount a little over a fifth of the payment:
    // no exact combination exists, and the search cannot prune its way out quickly.
    let docs: Vec<DocumentInput> = (0..40_u32)
        .map(|i| {
            invoice(
                u128::from(i),
                1,
                20_011 + i64::from(i) * 7,
                &format!("X-{i}"),
                "Acme",
            )
        })
        .collect();
    let lines = [line(1, 2, 100_000, "NEFT ACME", None)];
    let r = run(&lines, &docs, &Settings::default());
    assert_eq!(
        kind_of(outcome(&r, 1)),
        Some(ExceptionKind::SearchBudgetExhausted)
    );
    if let Outcome::Exception { candidates, .. } = outcome(&r, 1) {
        assert_eq!(candidates.len(), MAX_LISTED_CANDIDATES);
    }
}

#[test]
fn day_numbers_count_real_days() {
    assert_eq!(
        day_number(Date::new(2026, 3, 1).unwrap()) - day_number(Date::new(2026, 2, 28).unwrap()),
        1
    );
    assert_eq!(
        day_number(Date::new(2024, 3, 1).unwrap()) - day_number(Date::new(2024, 2, 28).unwrap()),
        2
    );
    assert_eq!(
        day_number(Date::new(2027, 1, 1).unwrap()) - day_number(Date::new(2026, 1, 1).unwrap()),
        365
    );
    assert_eq!(day_number(Date::new(2000, 1, 1).unwrap()), 10_957);
}

/// A seeded mix of lines and documents for the properties.
fn scenario(seed: u64, n: u32) -> (Vec<LineInput>, Vec<DocumentInput>) {
    let mut x = seed;
    let mut next = |m: u64| {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (x >> 33) % m
    };
    let cps = ["ACME", "BLUEFIN", "COBALT"];
    let mut lines = vec![];
    let mut docs = vec![];
    for i in 0..n {
        let amount = i64::try_from(next(5) * 1_000 + 1_000).unwrap();
        let cp = cps[usize::try_from(next(3)).unwrap()];
        let day = u8::try_from(next(20) + 1).unwrap();
        docs.push(invoice(
            u128::from(i),
            day,
            amount,
            &format!("INV-{}", next(8)),
            cp,
        ));
        let reference = if next(2) == 0 {
            format!("INV-{}", next(8))
        } else {
            String::new()
        };
        lines.push(line(
            u128::from(i),
            day + 2,
            if next(4) == 0 { amount * 2 } else { amount },
            &format!("NEFT {cp} {reference}"),
            None,
        ));
    }
    (lines, docs)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Order independence: any order of the same lines and documents gives the same
    /// outcomes and digest.
    #[test]
    fn any_order_gives_the_same_run(seed in any::<u64>(), n in 1_u32..40, rotate in 0_usize..40) {
        let (lines, docs) = scenario(seed, n);
        let base = run(&lines, &docs, &Settings::default());
        let mut l2 = lines.clone();
        let mut d2 = docs.clone();
        l2.reverse();
        let k = rotate % d2.len();
        d2.rotate_left(k);
        let again = run(&l2, &d2, &Settings::default());
        prop_assert_eq!(&base, &again);
        prop_assert_eq!(base.digest, run(&lines, &docs, &Settings::default()).digest);
    }

    /// Every match balances (it exists only as a BalancedMatch), no document is allocated
    /// twice or beyond its open amount, and only T1 ever commits.
    #[test]
    fn matches_balance_never_overlap_and_only_t1_commits(seed in any::<u64>(), n in 1_u32..40) {
        let (lines, docs) = scenario(seed, n);
        let r = run(&lines, &docs, &Settings::default());
        let mut used = BTreeSet::new();
        for o in r.outcomes.values() {
            if let Outcome::Matched { tier, matched, commit, .. } = o {
                prop_assert!(!*commit || *tier == Tier::T1);
                for d in matched.documents() {
                    prop_assert!(used.insert(d.document), "document allocated twice");
                    let doc = docs.iter().find(|x| x.id == d.document).unwrap();
                    prop_assert!(d.amount.minor().abs() <= doc.open_amount.minor().abs());
                }
            }
        }
        prop_assert_eq!(r.outcomes.len(), lines.len(), "every line has exactly one outcome");
    }
}
