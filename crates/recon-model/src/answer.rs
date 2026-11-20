//! The answer schema (D-016): chosen candidate ids with allocations, or none; an
//! optional residual code and amount; a reason of at most 500 characters. Parsing
//! proves only that the bytes are this schema — whether the answer is *allowed* is
//! [`crate::validate`]'s separate question.

use recon_domain::{Currency, DocumentId};
use serde::Deserialize;

/// One chosen document and the amount allocated to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Allocation {
    /// The candidate's id — must be in the request's set.
    pub document: DocumentId,
    /// Minor units allocated, positive.
    pub amount_minor: i64,
    /// Must equal the line's currency.
    pub currency: Currency,
}

/// The part of the difference a named code explains.
#[derive(Debug, Clone, PartialEq)]
pub struct ResidualAnswer {
    /// The code as the model wrote it; [`crate::validate`] resolves it against
    /// the named set.
    pub code: String,
    /// Minor units, positive.
    pub amount_minor: i64,
    /// Must equal the line's currency.
    pub currency: Currency,
}

/// A parsed answer: either a proposal or a deliberate "none of these".
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The model proposes: allocations, an optional residual and a reason.
    Proposal {
        /// The chosen candidates with what each covers.
        allocations: Vec<Allocation>,
        /// An optional named residual that completes the balance.
        residual: Option<ResidualAnswer>,
        /// Why, in at most 500 characters (bounded in [`crate::validate`]).
        reason: String,
    },
    /// The model declines: nothing in the candidate set is convincing.
    Declined {
        /// An optional explanation.
        reason: Option<String>,
    },
}

impl Answer {
    /// Parses the schema — strict: unknown fields and unknown kinds are refused.
    /// A currency string must be one of the D-005 table's ISO codes; anything
    /// else is malformed here, and a *different* known currency is
    /// [`crate::Refusal::CurrencyMismatch`] later.
    pub fn parse(text: &str) -> Result<Self, AnswerError> {
        let wire: Wire = serde_json::from_str(text).map_err(|e| AnswerError(e.to_string()))?;
        Ok(match wire {
            Wire::None { reason } => Self::Declined { reason },
            Wire::Proposal {
                allocations,
                residual,
                reason,
            } => Self::Proposal {
                allocations: allocations
                    .into_iter()
                    .map(|a| Allocation {
                        document: a.document,
                        amount_minor: a.amount_minor,
                        currency: a.currency,
                    })
                    .collect(),
                residual: residual.map(|r| ResidualAnswer {
                    code: r.code,
                    amount_minor: r.amount_minor,
                    currency: r.currency,
                }),
                reason,
            },
        })
    }
}

/// The bytes are not the answer schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed_answer: {0}")]
pub struct AnswerError(String);

fn currency<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Currency, D::Error> {
    let code = String::deserialize(d)?;
    Currency::from_code(&code).map_err(serde::de::Error::custom)
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Wire {
    Proposal {
        allocations: Vec<WireAllocation>,
        residual: Option<WireResidual>,
        reason: String,
    },
    None {
        reason: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAllocation {
    document: DocumentId,
    amount_minor: i64,
    #[serde(deserialize_with = "currency")]
    currency: Currency,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireResidual {
    code: String,
    amount_minor: i64,
    #[serde(deserialize_with = "currency")]
    currency: Currency,
}
