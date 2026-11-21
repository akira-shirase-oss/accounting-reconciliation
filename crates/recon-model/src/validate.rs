//! The validator (D-016, `END_GOAL` failure case 3): an answer is refused — with a
//! named code — when it names anything outside the candidate set, another client
//! or firm's document (the set is the tenant boundary; there is no legitimate way
//! to name one), a different currency, or allocations that do not balance. A
//! balanced answer the *engine* would have scored differently is none of the
//! port's business: it comes out a [`Validated::Proposal`] and stays a proposal —
//! only a person may commit it (non-negotiable 2).

use std::collections::HashSet;
use std::fmt;

use recon_domain::{
    DocumentId, Money,
    matching::{
        BalancedMatch, DocumentAllocation, LineAllocation, MatchRefusal, Residual, ResidualCode,
    },
};

use crate::{Answer, ProposeRequest};

/// The most characters a reason may carry (D-016).
const MAX_REASON_CHARS: usize = 500;

/// Why an answer was refused. Each code is stable — the decision log records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A document that is not in the request's candidate set — including a real
    /// document of another tenant, which the set is the boundary against.
    CandidateNotInSet,
    /// An allocation or residual in a currency other than the line's.
    CurrencyMismatch,
    /// Allocations plus residual do not equal the line's absolute amount.
    Unbalanced,
    /// An allocation larger than the document's open amount.
    AllocationExceedsOpen,
    /// An allocation of zero or less.
    NonPositiveAllocation,
    /// A reason past the bound.
    ReasonTooLong,
    /// A residual code outside the named set (D-014).
    UnknownResidualCode,
    /// A residual of zero or less — residuals are positive magnitudes on the
    /// wire; the code says which side they explain (D-014, S2-5).
    NonPositiveResidual,
    /// A `rounding` residual larger than one minor unit per document — the
    /// same bound [`BalancedMatch::new`] applies everywhere else (D-014, S2-5).
    RoundingTooLarge,
    /// One document named twice.
    DuplicateCandidate,
    /// A proposal with nothing allocated.
    NoChoices,
}

impl Refusal {
    /// The stable machine code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::CandidateNotInSet => "candidate_not_in_set",
            Self::CurrencyMismatch => "currency_mismatch",
            Self::Unbalanced => "unbalanced",
            Self::AllocationExceedsOpen => "allocation_exceeds_open",
            Self::NonPositiveAllocation => "nonpositive_allocation",
            Self::ReasonTooLong => "reason_too_long",
            Self::UnknownResidualCode => "unknown_residual_code",
            Self::NonPositiveResidual => "nonpositive_residual",
            Self::RoundingTooLarge => "rounding_too_large",
            Self::DuplicateCandidate => "duplicate_candidate",
            Self::NoChoices => "no_choices",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for Refusal {}

/// Which way a checked answer went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalKind {
    /// It is a proposal.
    Proposal,
    /// The model declined.
    Declined,
}

/// An answer the validator allowed. A proposal is *input* to a person — the only
/// thing it can become upstream is a proposal row; committing it is a person's
/// recorded act, never the port's.
#[derive(Debug, Clone, PartialEq)]
pub enum Validated {
    /// The model declined the candidate set.
    Declined,
    /// A schema-clean, balanced, in-set proposal.
    Proposal(ValidatedProposal),
}

impl Validated {
    /// Its kind.
    pub const fn kind(&self) -> ProposalKind {
        match self {
            Self::Declined => ProposalKind::Declined,
            Self::Proposal(_) => ProposalKind::Proposal,
        }
    }
}

/// The validated payload: domain types, ready to become a proposal row.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedProposal {
    /// Each document and what it covers (the line's currency, positive, within open).
    pub allocations: Vec<DocumentAllocation>,
    /// The named residual that completed the balance, if any.
    pub residual: Option<Residual>,
    /// `true` whenever the proposal carries a residual — the same
    /// `BalancedMatch::needs_four_eyes` rule the engine applies (D-014).
    pub needs_four_eyes: bool,
    /// The model's reason, within the bound.
    pub reason: String,
}

fn residual_code(code: &str) -> Option<ResidualCode> {
    match code {
        "bank_fee" => Some(ResidualCode::BankFee),
        "tax_withheld" => Some(ResidualCode::TaxWithheld),
        "rounding" => Some(ResidualCode::Rounding),
        _ => None,
    }
}

/// Checks `answer` against `request`. The candidate set is the whole universe:
/// ids are checked, currencies must equal the line's, every allocation must be
/// positive and inside the document's open amount, and allocations plus residual
/// must balance the line's absolute amount exactly.
pub fn validate(request: &ProposeRequest, answer: &Answer) -> Result<Validated, Refusal> {
    let Answer::Proposal {
        allocations,
        residual,
        reason,
    } = answer
    else {
        return Ok(Validated::Declined);
    };

    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(Refusal::ReasonTooLong);
    }
    if allocations.is_empty() {
        return Err(Refusal::NoChoices);
    }

    let currency = request.currency();
    let line_abs = request.line.amount.minor().unsigned_abs().cast_signed();

    let mut seen: HashSet<DocumentId> = HashSet::with_capacity(allocations.len());
    let mut covered: i64 = 0;
    let mut out = Vec::with_capacity(allocations.len());
    for allocation in allocations {
        if !seen.insert(allocation.document) {
            return Err(Refusal::DuplicateCandidate);
        }
        let candidate = request
            .candidates
            .iter()
            .find(|c| c.id == allocation.document)
            .ok_or(Refusal::CandidateNotInSet)?;
        if allocation.currency != currency {
            return Err(Refusal::CurrencyMismatch);
        }
        if allocation.amount_minor <= 0 {
            return Err(Refusal::NonPositiveAllocation);
        }
        if allocation.amount_minor > candidate.open.minor() {
            return Err(Refusal::AllocationExceedsOpen);
        }
        covered = covered
            .checked_add(allocation.amount_minor)
            .ok_or(Refusal::Unbalanced)?;
        out.push(DocumentAllocation {
            document: allocation.document,
            amount: Money::new(allocation.amount_minor, currency)
                .map_err(|_| Refusal::Unbalanced)?,
        });
    }

    let residual = match residual {
        None => None,
        Some(r) => {
            if r.currency != currency {
                return Err(Refusal::CurrencyMismatch);
            }
            let code = residual_code(&r.code).ok_or(Refusal::UnknownResidualCode)?;
            if r.amount_minor <= 0 {
                return Err(Refusal::NonPositiveResidual);
            }
            covered = covered
                .checked_add(r.amount_minor)
                .ok_or(Refusal::Unbalanced)?;
            Some(Residual {
                code,
                amount: Money::new(r.amount_minor, currency).map_err(|_| Refusal::Unbalanced)?,
            })
        }
    };

    if covered != line_abs {
        return Err(Refusal::Unbalanced);
    }

    // The same balance law the engine applies (D-014, S2-5): build the proposed
    // match through `BalancedMatch::new` so its typed refusals — a rounding
    // residual past one minor unit per document, a zero allocation — are this
    // port's refusals too, not just the write path's.
    let residuals: Vec<Residual> = residual.into_iter().collect();
    let needs_four_eyes = !residuals.is_empty();
    BalancedMatch::new(
        vec![LineAllocation {
            line: request.line.id,
            amount: Money::new(line_abs, currency).map_err(|_| Refusal::Unbalanced)?,
        }],
        out.clone(),
        residuals.clone(),
    )
    .map_err(|e| match e {
        MatchRefusal::RoundingTooLarge => Refusal::RoundingTooLarge,
        _ => Refusal::Unbalanced,
    })?;

    Ok(Validated::Proposal(ValidatedProposal {
        allocations: out,
        residual: residuals.first().copied(),
        needs_four_eyes,
        reason: reason.clone(),
    }))
}
