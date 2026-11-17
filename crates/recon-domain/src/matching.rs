//! Match records (used from R2 on; fixed here so the invariants are types from day one).
//! A [`BalancedMatch`] cannot be built unless its line allocations equal its document
//! allocations plus named residuals exactly, in one currency (invariant 3, D-014).

use serde::{Deserialize, Serialize};

use crate::ids::{DocumentId, LineId};
use crate::money::{Currency, Money, MoneySum};

/// Most items on either side of one match: a hard bound above D-013's search limit of 5,
/// for manual matches.
pub const MAX_ITEMS_PER_SIDE: usize = 64;

/// The deterministic tiers (D-011). Only T1 may commit without a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tier {
    /// Exact amount and reference, a unique candidate: may auto-commit.
    T1,
    /// Exact amount, unique by amount in the window, counterparty match: a proposal.
    T2,
    /// An exact combination (D-013): a proposal.
    T3,
    /// Balances only with a residual (D-014): a proposal needing four-eyes.
    T4,
}

impl Tier {
    /// True only for T1.
    pub const fn may_auto_commit(self) -> bool {
        matches!(self, Self::T1)
    }
}

/// Named residuals (D-014).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidualCode {
    /// A bank fee taken from the payment.
    BankFee,
    /// Tax withheld at source.
    TaxWithheld,
    /// At most one minor unit per document.
    Rounding,
}

impl ResidualCode {
    /// The stable code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::BankFee => "bank_fee",
            Self::TaxWithheld => "tax_withheld",
            Self::Rounding => "rounding",
        }
    }
}

/// Part of a line's amount allocated to a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineAllocation {
    /// The line.
    pub line: LineId,
    /// The allocated amount (the line's sign).
    pub amount: Money,
}

/// Part of a document's amount allocated to a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentAllocation {
    /// The document.
    pub document: DocumentId,
    /// The allocated amount (normalized to the bank's sign, D-007).
    pub amount: Money,
}

/// A named residual: the part of the difference a code explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Residual {
    /// Why.
    pub code: ResidualCode,
    /// How much (signed).
    pub amount: Money,
}

/// Why a match was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MatchRefusal {
    /// No line allocation.
    #[error("no_lines")]
    NoLines,
    /// No document allocation.
    #[error("no_documents")]
    NoDocuments,
    /// More than [`MAX_ITEMS_PER_SIDE`] on one side.
    #[error("too_many_items")]
    TooManyItems,
    /// An allocation or residual of zero.
    #[error("zero_allocation")]
    ZeroAllocation,
    /// The same line twice.
    #[error("duplicate_line")]
    DuplicateLine,
    /// The same document twice.
    #[error("duplicate_document")]
    DuplicateDocument,
    /// More than one currency (no FX matching in v1).
    #[error("cross_currency")]
    CrossCurrency,
    /// The same residual code twice.
    #[error("duplicate_residual")]
    DuplicateResidual,
    /// A rounding residual larger than one minor unit per document.
    #[error("rounding_too_large")]
    RoundingTooLarge,
    /// Lines ≠ documents + residuals.
    #[error("unbalanced")]
    Unbalanced,
}

impl MatchRefusal {
    /// The stable code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoLines => "no_lines",
            Self::NoDocuments => "no_documents",
            Self::TooManyItems => "too_many_items",
            Self::ZeroAllocation => "zero_allocation",
            Self::DuplicateLine => "duplicate_line",
            Self::DuplicateDocument => "duplicate_document",
            Self::CrossCurrency => "cross_currency",
            Self::DuplicateResidual => "duplicate_residual",
            Self::RoundingTooLarge => "rounding_too_large",
            Self::Unbalanced => "unbalanced",
        }
    }
}

/// A match whose sides balance exactly. Its fields are private: the only way to hold one
/// is through [`BalancedMatch::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BalancedMatch {
    currency: Currency,
    lines: Vec<LineAllocation>,
    documents: Vec<DocumentAllocation>,
    residuals: Vec<Residual>,
}

impl BalancedMatch {
    /// Builds a match, refused unless every allocation is non-zero, unique and in one
    /// currency, and `sum(lines) == sum(documents) + sum(residuals)` exactly.
    pub fn new(
        lines: Vec<LineAllocation>,
        documents: Vec<DocumentAllocation>,
        residuals: Vec<Residual>,
    ) -> Result<Self, MatchRefusal> {
        let first = lines.first().ok_or(MatchRefusal::NoLines)?;
        if documents.is_empty() {
            return Err(MatchRefusal::NoDocuments);
        }
        if lines.len() > MAX_ITEMS_PER_SIDE || documents.len() > MAX_ITEMS_PER_SIDE {
            return Err(MatchRefusal::TooManyItems);
        }
        let currency = first.amount.currency();
        let amounts = lines
            .iter()
            .map(|l| l.amount)
            .chain(documents.iter().map(|d| d.amount))
            .chain(residuals.iter().map(|r| r.amount));
        for amount in amounts {
            if amount.currency() != currency {
                return Err(MatchRefusal::CrossCurrency);
            }
            if amount.is_zero() {
                return Err(MatchRefusal::ZeroAllocation);
            }
        }
        if !all_unique(lines.iter().map(|l| l.line)) {
            return Err(MatchRefusal::DuplicateLine);
        }
        if !all_unique(documents.iter().map(|d| d.document)) {
            return Err(MatchRefusal::DuplicateDocument);
        }
        if !all_unique(residuals.iter().map(|r| r.code)) {
            return Err(MatchRefusal::DuplicateResidual);
        }
        if let Some(rounding) = residuals.iter().find(|r| r.code == ResidualCode::Rounding) {
            let limit = u64::try_from(documents.len()).unwrap_or(u64::MAX);
            if rounding.amount.minor().unsigned_abs() > limit {
                return Err(MatchRefusal::RoundingTooLarge);
            }
        }
        let mut line_total = MoneySum::zero(currency);
        let mut other_total = MoneySum::zero(currency);
        for l in &lines {
            line_total
                .add(l.amount)
                .map_err(|_| MatchRefusal::CrossCurrency)?;
        }
        for amount in documents
            .iter()
            .map(|d| d.amount)
            .chain(residuals.iter().map(|r| r.amount))
        {
            other_total
                .add(amount)
                .map_err(|_| MatchRefusal::CrossCurrency)?;
        }
        if line_total != other_total {
            return Err(MatchRefusal::Unbalanced);
        }
        Ok(Self {
            currency,
            lines,
            documents,
            residuals,
        })
    }

    /// The match's currency.
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// The line allocations.
    pub fn lines(&self) -> &[LineAllocation] {
        &self.lines
    }

    /// The document allocations.
    pub fn documents(&self) -> &[DocumentAllocation] {
        &self.documents
    }

    /// The residuals; any residual makes the match a four-eyes proposal (D-014).
    pub fn residuals(&self) -> &[Residual] {
        &self.residuals
    }

    /// True when a residual is present, so the match always needs a second person.
    pub fn needs_four_eyes(&self) -> bool {
        !self.residuals.is_empty()
    }
}

fn all_unique<T: Eq + std::hash::Hash>(items: impl Iterator<Item = T>) -> bool {
    let mut seen = std::collections::HashSet::new();
    items.into_iter().all(|item| seen.insert(item))
}

/// A match's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    /// Waiting for a person (or a T1 rule).
    Proposed,
    /// Committed.
    Committed,
    /// Rejected by a person. Final.
    Rejected,
    /// Suspended because a source changed (D-009).
    Suspended,
    /// Reversed by a later decision. Final.
    Reversed,
}

impl MatchStatus {
    /// Checks a status change; anything outside the lifecycle is `invalid_match_transition`.
    pub const fn can_become(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Proposed, Self::Committed | Self::Rejected)
                | (Self::Committed | Self::Suspended, Self::Reversed)
                | (Self::Committed, Self::Suspended)
                | (Self::Suspended, Self::Committed)
        )
    }
}

/// The deterministic score's breakdown (D-012, `scoring v1`): weights 40/30/15/10/5. It is
/// a ranking aid, never a probability, and never combined with a model's claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ScoreRepr")]
pub struct ScoreBreakdown {
    /// 0 or 40.
    pub exact_amount: u8,
    /// 0 or 30.
    pub exact_reference: u8,
    /// 0 or 15.
    pub counterparty: u8,
    /// 0 to 10.
    pub date_proximity: u8,
    /// 0 or 5.
    pub uniqueness: u8,
}

/// The wire form of [`ScoreBreakdown`], validated on the way in (D-052).
#[derive(Deserialize)]
struct ScoreRepr {
    exact_amount: u8,
    exact_reference: u8,
    counterparty: u8,
    date_proximity: u8,
    uniqueness: u8,
}

/// A score whose components are not `scoring-v1`'s weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid_score")]
pub struct InvalidScore;

impl TryFrom<ScoreRepr> for ScoreBreakdown {
    type Error = InvalidScore;

    fn try_from(r: ScoreRepr) -> Result<Self, InvalidScore> {
        let flag = |value: u8, weight: u8| match value {
            0 => Ok(false),
            v if v == weight => Ok(true),
            _ => Err(InvalidScore),
        };
        Self::new(
            flag(r.exact_amount, 40)?,
            flag(r.exact_reference, 30)?,
            flag(r.counterparty, 15)?,
            r.date_proximity,
            flag(r.uniqueness, 5)?,
        )
        .ok_or(InvalidScore)
    }
}

impl ScoreBreakdown {
    /// The scoring version recorded with every proposal.
    pub const VERSION: &'static str = "scoring-v1";

    /// Builds a breakdown, refused (None) when a component exceeds its weight.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "one flag per scoring component of D-012"
    )]
    pub const fn new(
        exact_amount: bool,
        exact_reference: bool,
        counterparty: bool,
        date_proximity: u8,
        unique: bool,
    ) -> Option<Self> {
        if date_proximity > 10 {
            return None;
        }
        Some(Self {
            exact_amount: if exact_amount { 40 } else { 0 },
            exact_reference: if exact_reference { 30 } else { 0 },
            counterparty: if counterparty { 15 } else { 0 },
            date_proximity,
            uniqueness: if unique { 5 } else { 0 },
        })
    }

    /// The total, 0–100.
    pub const fn total(self) -> u8 {
        self.exact_amount
            .saturating_add(self.exact_reference)
            .saturating_add(self.counterparty)
            .saturating_add(self.date_proximity)
            .saturating_add(self.uniqueness)
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
    use uuid::Uuid;

    fn usd(minor: i64) -> Money {
        Money::new(minor, Currency::Usd).unwrap()
    }
    fn line(n: u128, minor: i64) -> LineAllocation {
        LineAllocation {
            line: LineId::from_uuid(Uuid::from_u128(n)),
            amount: usd(minor),
        }
    }
    fn doc(n: u128, minor: i64) -> DocumentAllocation {
        DocumentAllocation {
            document: DocumentId::from_uuid(Uuid::from_u128(n)),
            amount: usd(minor),
        }
    }

    #[test]
    fn one_payment_three_invoices_less_a_bank_fee_balances_with_a_named_residual() {
        let fee = Residual {
            code: ResidualCode::BankFee,
            amount: usd(-1_500),
        };
        let m = BalancedMatch::new(
            vec![line(1, 298_500)],
            vec![doc(10, 100_000), doc(11, 120_000), doc(12, 80_000)],
            vec![fee],
        )
        .unwrap();
        assert!(m.needs_four_eyes());
        // Without the residual the same allocations are refused, never absorbed.
        assert_eq!(
            BalancedMatch::new(
                vec![line(1, 298_500)],
                vec![doc(10, 100_000), doc(11, 120_000), doc(12, 80_000)],
                vec![]
            ),
            Err(MatchRefusal::Unbalanced)
        );
    }

    #[test]
    fn structural_refusals_are_named() {
        assert_eq!(
            BalancedMatch::new(vec![], vec![doc(1, 5)], vec![]),
            Err(MatchRefusal::NoLines)
        );
        assert_eq!(
            BalancedMatch::new(vec![line(1, 5)], vec![], vec![]),
            Err(MatchRefusal::NoDocuments)
        );
        assert_eq!(
            BalancedMatch::new(vec![line(1, 5), line(1, 5)], vec![doc(2, 10)], vec![]),
            Err(MatchRefusal::DuplicateLine)
        );
        assert_eq!(
            BalancedMatch::new(vec![line(1, 10)], vec![doc(2, 5), doc(2, 5)], vec![]),
            Err(MatchRefusal::DuplicateDocument)
        );
        assert_eq!(
            BalancedMatch::new(vec![line(1, 0)], vec![doc(2, 0)], vec![]),
            Err(MatchRefusal::ZeroAllocation)
        );
        let eur = DocumentAllocation {
            document: DocumentId::from_uuid(Uuid::from_u128(3)),
            amount: Money::new(5, Currency::Eur).unwrap(),
        };
        assert_eq!(
            BalancedMatch::new(vec![line(1, 5)], vec![eur], vec![]),
            Err(MatchRefusal::CrossCurrency)
        );
        let rounding = Residual {
            code: ResidualCode::Rounding,
            amount: usd(2),
        };
        assert_eq!(
            BalancedMatch::new(vec![line(1, 12)], vec![doc(2, 10)], vec![rounding]),
            Err(MatchRefusal::RoundingTooLarge)
        );
        let many: Vec<_> = (0..65).map(|n| line(n, 1)).collect();
        assert_eq!(
            BalancedMatch::new(many, vec![doc(999, 65)], vec![]),
            Err(MatchRefusal::TooManyItems)
        );
    }

    #[test]
    fn match_lifecycle_and_score() {
        assert!(MatchStatus::Proposed.can_become(MatchStatus::Committed));
        assert!(!MatchStatus::Rejected.can_become(MatchStatus::Committed));
        assert!(!MatchStatus::Reversed.can_become(MatchStatus::Committed));
        assert!(!MatchStatus::Proposed.can_become(MatchStatus::Suspended));
        assert_eq!(
            ScoreBreakdown::new(true, true, true, 10, true)
                .unwrap()
                .total(),
            100
        );
        assert_eq!(ScoreBreakdown::new(false, false, false, 11, false), None);
        assert!(Tier::T1.may_auto_commit() && !Tier::T2.may_auto_commit());
    }

    /// M-5: allocations and scores deserialized from a request or a model's answer carry
    /// the same checks as the constructors.
    #[test]
    fn deserialized_allocations_and_scores_are_checked() {
        let line = format!(
            r#"{{"line":"{}","amount":{{"minor":{},"currency":"Usd"}}}}"#,
            Uuid::from_u128(1),
            i64::MIN
        );
        assert!(serde_json::from_str::<LineAllocation>(&line).is_err());
        let ok = r#"{"exact_amount":40,"exact_reference":0,"counterparty":15,"date_proximity":7,"uniqueness":5}"#;
        assert_eq!(
            serde_json::from_str::<ScoreBreakdown>(ok).unwrap().total(),
            67
        );
        for bad in [
            r#"{"exact_amount":200,"exact_reference":0,"counterparty":0,"date_proximity":0,"uniqueness":0}"#,
            r#"{"exact_amount":40,"exact_reference":29,"counterparty":0,"date_proximity":0,"uniqueness":0}"#,
            r#"{"exact_amount":0,"exact_reference":0,"counterparty":0,"date_proximity":11,"uniqueness":0}"#,
        ] {
            assert!(
                serde_json::from_str::<ScoreBreakdown>(bad).is_err(),
                "{bad}"
            );
        }
    }

    proptest! {
        /// Any set of allocations builds a match exactly when it balances (one currency,
        /// non-zero, unique): a match that does not balance cannot be built.
        #[test]
        fn a_match_builds_if_and_only_if_it_balances(
            lines in proptest::collection::vec(-1_000_000_i64..1_000_000, 1..6),
            docs in proptest::collection::vec(-1_000_000_i64..1_000_000, 1..6),
            fee in proptest::option::of(-10_000_i64..10_000),
            force_balance: bool,
        ) {
            let mut docs = docs;
            if force_balance {
                let lt: i64 = lines.iter().sum();
                let dt: i64 = docs.iter().sum::<i64>() + fee.unwrap_or(0);
                let last = docs.len() - 1;
                docs[last] += lt - dt;
            }
            let la: Vec<_> = lines.iter().enumerate().map(|(i, m)| line(i as u128, *m)).collect();
            let da: Vec<_> = docs.iter().enumerate().map(|(i, m)| doc(100 + i as u128, *m)).collect();
            let ra: Vec<_> = fee.into_iter().map(|m| Residual { code: ResidualCode::BankFee, amount: usd(m) }).collect();
            let has_zero = lines.contains(&0) || docs.contains(&0) || fee == Some(0);
            let balanced = lines.iter().map(|&m| i128::from(m)).sum::<i128>()
                == docs.iter().map(|&m| i128::from(m)).sum::<i128>() + i128::from(fee.unwrap_or(0));
            let result = BalancedMatch::new(la, da, ra);
            if has_zero {
                prop_assert_eq!(result, Err(MatchRefusal::ZeroAllocation));
            } else if balanced {
                prop_assert!(result.is_ok());
            } else {
                prop_assert_eq!(result, Err(MatchRefusal::Unbalanced));
            }
        }
    }
}
