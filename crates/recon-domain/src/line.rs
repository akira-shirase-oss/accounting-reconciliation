//! The statement line's state machine (D-006). A line is in exactly one state; every change
//! is a [`Transition`] with a [`Cause`], refused by name when the table below does not
//! allow it. Only a T1 rule auto-commit or a person's approval reaches `matched` (D-011,
//! invariant 2); a model never commits or excludes anything (D-016).

use serde::{Deserialize, Serialize};

/// Why a line is excluded from matching. Fixed at R0; R3's review API may add codes with
/// a decision entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExcludeReason {
    /// A transfer between the client's own accounts.
    InternalTransfer,
    /// Not the client's business (a personal payment, for example).
    NonBusiness,
    /// A payment and its reversal, both on the statement.
    ReversedPair,
    /// Outside what this reconciliation covers, with a reviewer's note.
    OutOfScope,
}

impl ExcludeReason {
    /// Every reason.
    pub const ALL: [Self; 4] = [
        Self::InternalTransfer,
        Self::NonBusiness,
        Self::ReversedPair,
        Self::OutOfScope,
    ];

    /// The stable code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::InternalTransfer => "internal_transfer",
            Self::NonBusiness => "non_business",
            Self::ReversedPair => "reversed_pair",
            Self::OutOfScope => "out_of_scope",
        }
    }
}

/// A typed exception (D-011; failure case 9 in `END_GOAL.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExceptionKind {
    /// No open document qualifies.
    NoCandidate,
    /// More than one qualifies and nothing decides between them.
    MultipleCandidates,
    /// A candidate exists but the amounts differ.
    AmountMismatch,
    /// The only candidates are in another currency (no FX matching in v1).
    CrossCurrency,
    /// The combination search hit its node budget (D-013).
    SearchBudgetExhausted,
    /// The statement was revised under a matched line (D-009).
    SourceChanged,
    /// The line looks like a duplicate of another.
    DuplicateSuspect,
    /// Only the narration could decide; a person (or the model, if enabled) must read it.
    NarrationNeeded,
    /// The model provider failed or timed out.
    ModelUnavailable,
    /// The job's model budget is spent.
    ModelBudgetExhausted,
}

impl ExceptionKind {
    /// Every kind.
    pub const ALL: [Self; 10] = [
        Self::NoCandidate,
        Self::MultipleCandidates,
        Self::AmountMismatch,
        Self::CrossCurrency,
        Self::SearchBudgetExhausted,
        Self::SourceChanged,
        Self::DuplicateSuspect,
        Self::NarrationNeeded,
        Self::ModelUnavailable,
        Self::ModelBudgetExhausted,
    ];

    /// The stable code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoCandidate => "no_candidate",
            Self::MultipleCandidates => "multiple_candidates",
            Self::AmountMismatch => "amount_mismatch",
            Self::CrossCurrency => "cross_currency",
            Self::SearchBudgetExhausted => "search_budget_exhausted",
            Self::SourceChanged => "source_changed",
            Self::DuplicateSuspect => "duplicate_suspect",
            Self::NarrationNeeded => "narration_needed",
            Self::ModelUnavailable => "model_unavailable",
            Self::ModelBudgetExhausted => "model_budget_exhausted",
        }
    }
}

/// A line's state (D-006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", content = "code", rename_all = "snake_case")]
pub enum LineState {
    /// Imported, not yet looked at by the engine or a person.
    Unreviewed,
    /// A proposal waits for a person.
    Proposed,
    /// Committed to a match.
    Matched,
    /// Excluded with a reason.
    Excluded(ExcludeReason),
    /// A typed exception.
    Exception(ExceptionKind),
    /// Replaced through an accepted revision (D-009). Final.
    Superseded,
}

impl LineState {
    /// The state's stored name (the `status` column).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unreviewed => "unreviewed",
            Self::Proposed => "proposed",
            Self::Matched => "matched",
            Self::Excluded(_) => "excluded",
            Self::Exception(_) => "exception",
            Self::Superseded => "superseded",
        }
    }

    /// Every state, each exclusion reason and exception kind included.
    pub fn all() -> Vec<Self> {
        let mut all = vec![
            Self::Unreviewed,
            Self::Proposed,
            Self::Matched,
            Self::Superseded,
        ];
        all.extend(ExcludeReason::ALL.map(Self::Excluded));
        all.extend(ExceptionKind::ALL.map(Self::Exception));
        all
    }

    /// True when a period may close over this line (D-006): matched, excluded with a
    /// reason, an exception (its acknowledgement is checked separately) or superseded.
    pub const fn is_settled(self) -> bool {
        !matches!(self, Self::Unreviewed | Self::Proposed)
    }
}

/// Who or what is changing a line, as recorded in the decision log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// The R0 operator token (replaced by `person` once sessions exist, R3).
    Operator,
    /// A signed-in person.
    Person,
    /// An approved, versioned rule.
    Rule,
    /// A person approving a proposal.
    Approval,
    /// The deterministic engine.
    Engine,
    /// The model, through the port (proposals only).
    Model,
    /// The system itself (a sweeper, a deadline).
    System,
}

impl ActorKind {
    /// Every kind.
    pub const ALL: [Self; 7] = [
        Self::Operator,
        Self::Person,
        Self::Rule,
        Self::Approval,
        Self::Engine,
        Self::Model,
        Self::System,
    ];

    /// The stored name.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Person => "person",
            Self::Rule => "rule",
            Self::Approval => "approval",
            Self::Engine => "engine",
            Self::Model => "model",
            Self::System => "system",
        }
    }
}

/// Why a line changes state. Each cause has exactly one actor kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The engine proposed a match (T2–T4) or re-evaluated the line.
    EngineProposal,
    /// The engine typed the line as an exception.
    EngineException,
    /// A T1 rule committed the match automatically (the only automatic commit).
    AutoCommit,
    /// An approved rule excluded the line (only when its owner allowed automatic exclusion).
    RuleExclusion,
    /// The model proposed a candidate (validated).
    ModelProposal,
    /// A person approved a proposal.
    Approval,
    /// A person acted directly: rejected, excluded, reopened, or proposed a manual match.
    PersonAction,
    /// A person accepted a statement revision (D-009).
    RevisionAccepted,
    /// The system recorded a failure (the model unavailable or over budget).
    SystemException,
}

impl Cause {
    /// Every cause.
    pub const ALL: [Self; 9] = [
        Self::EngineProposal,
        Self::EngineException,
        Self::AutoCommit,
        Self::RuleExclusion,
        Self::ModelProposal,
        Self::Approval,
        Self::PersonAction,
        Self::RevisionAccepted,
        Self::SystemException,
    ];

    /// The actor kind the decision records.
    pub const fn actor_kind(self) -> ActorKind {
        match self {
            Self::EngineProposal | Self::EngineException => ActorKind::Engine,
            Self::AutoCommit | Self::RuleExclusion => ActorKind::Rule,
            Self::ModelProposal => ActorKind::Model,
            Self::Approval => ActorKind::Approval,
            Self::PersonAction | Self::RevisionAccepted => ActorKind::Person,
            Self::SystemException => ActorKind::System,
        }
    }
}

/// A refused transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransitionRefusal {
    /// The target equals the current state.
    #[error("no_op_transition")]
    NoOp,
    /// A superseded line never changes again.
    #[error("line_superseded")]
    Superseded,
    /// The model may only propose (D-016).
    #[error("model_cannot_commit")]
    ModelCannotCommit,
    /// The pair of states is never connected.
    #[error("invalid_transition")]
    Invalid,
    /// The pair is connected, but not by this cause.
    #[error("cause_not_allowed")]
    CauseNotAllowed,
}

impl TransitionRefusal {
    /// The stable code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoOp => "no_op_transition",
            Self::Superseded => "line_superseded",
            Self::ModelCannotCommit => "model_cannot_commit",
            Self::Invalid => "invalid_transition",
            Self::CauseNotAllowed => "cause_not_allowed",
        }
    }
}

/// The causes allowed between two states, or `None` when the states are never connected.
///
/// The whole table: read it as "from → to: causes".
#[expect(
    clippy::match_same_arms,
    reason = "the table reads row by row; merging rows would hide them"
)]
fn allowed(from: LineState, to: LineState) -> Option<&'static [Cause]> {
    use Cause as C;
    use LineState as S;
    Some(match (from, to) {
        (S::Unreviewed, S::Proposed) => &[C::EngineProposal, C::ModelProposal, C::PersonAction],
        (S::Unreviewed, S::Matched) => &[C::AutoCommit],
        (S::Unreviewed | S::Exception(_), S::Excluded(_)) => &[C::RuleExclusion, C::PersonAction],
        (S::Unreviewed, S::Exception(_)) => &[C::EngineException, C::SystemException],
        (S::Proposed, S::Matched) => &[C::Approval],
        (S::Proposed, S::Unreviewed) => &[C::EngineProposal, C::PersonAction],
        (S::Proposed, S::Exception(_)) => {
            &[C::EngineException, C::SystemException, C::PersonAction]
        }
        (S::Proposed, S::Excluded(_)) => &[C::PersonAction],
        (S::Exception(_), S::Proposed) => &[C::EngineProposal, C::ModelProposal, C::PersonAction],
        (S::Exception(_), S::Unreviewed) => &[C::EngineProposal, C::PersonAction],
        (S::Exception(a), S::Exception(b)) if a != b => &[C::EngineException, C::SystemException],
        (S::Matched, S::Exception(ExceptionKind::SourceChanged)) => &[C::RevisionAccepted],
        (S::Matched | S::Excluded(_), S::Unreviewed) => &[C::PersonAction],
        (S::Unreviewed | S::Proposed | S::Excluded(_) | S::Exception(_), S::Superseded) => {
            &[C::RevisionAccepted]
        }
        _ => return None,
    })
}

/// One line state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    /// The state before.
    pub from: LineState,
    /// The state after.
    pub to: LineState,
    /// Why.
    pub cause: Cause,
}

impl Transition {
    /// Checks `from → to` under `cause` against the table.
    pub fn new(from: LineState, to: LineState, cause: Cause) -> Result<Self, TransitionRefusal> {
        if from == to {
            return Err(TransitionRefusal::NoOp);
        }
        if from == LineState::Superseded {
            return Err(TransitionRefusal::Superseded);
        }
        if cause == Cause::ModelProposal && !matches!(to, LineState::Proposed) {
            return Err(TransitionRefusal::ModelCannotCommit);
        }
        let causes = allowed(from, to).ok_or(TransitionRefusal::Invalid)?;
        if causes.contains(&cause) {
            Ok(Self { from, to, cause })
        } else {
            Err(TransitionRefusal::CauseNotAllowed)
        }
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

    /// Every (from, to, cause) triple is either in the table or a named refusal, and the
    /// commitments of invariant 2 hold over the whole space.
    #[test]
    fn every_transition_is_allowed_by_the_table_or_refused_by_name() {
        let states = LineState::all();
        let mut allowed_count = 0;
        for &from in &states {
            for &to in &states {
                for cause in Cause::ALL {
                    match Transition::new(from, to, cause) {
                        Ok(t) => {
                            allowed_count += 1;
                            assert!(allowed(from, to).unwrap().contains(&cause));
                            assert_eq!((t.from, t.to, t.cause), (from, to, cause));
                            if to == LineState::Matched {
                                assert!(
                                    matches!(cause, Cause::AutoCommit | Cause::Approval),
                                    "{from:?} -> matched by {cause:?}"
                                );
                            }
                            if cause == Cause::AutoCommit {
                                assert_eq!((from, to), (LineState::Unreviewed, LineState::Matched));
                            }
                            if cause.actor_kind() == ActorKind::Model {
                                assert_eq!(to, LineState::Proposed);
                            }
                        }
                        Err(refusal) => {
                            let expected = if from == to {
                                TransitionRefusal::NoOp
                            } else if from == LineState::Superseded {
                                TransitionRefusal::Superseded
                            } else if cause == Cause::ModelProposal && to != LineState::Proposed {
                                TransitionRefusal::ModelCannotCommit
                            } else if allowed(from, to).is_none() {
                                TransitionRefusal::Invalid
                            } else {
                                TransitionRefusal::CauseNotAllowed
                            };
                            assert_eq!(refusal, expected, "{from:?} -> {to:?} by {cause:?}");
                        }
                    }
                }
            }
        }
        assert!(allowed_count > 0);
    }

    #[test]
    fn the_model_never_commits_or_excludes() {
        for from in LineState::all() {
            assert!(Transition::new(from, LineState::Matched, Cause::ModelProposal).is_err());
            for reason in ExcludeReason::ALL {
                assert!(
                    Transition::new(from, LineState::Excluded(reason), Cause::ModelProposal)
                        .is_err()
                );
            }
        }
        assert!(
            Transition::new(
                LineState::Exception(ExceptionKind::NarrationNeeded),
                LineState::Proposed,
                Cause::ModelProposal
            )
            .is_ok()
        );
    }

    #[test]
    fn a_matched_line_only_leaves_through_a_person_or_a_revision() {
        let from = LineState::Matched;
        let exits: Vec<_> = LineState::all()
            .into_iter()
            .flat_map(|to| Cause::ALL.map(move |c| (to, c)))
            .filter(|&(to, c)| Transition::new(from, to, c).is_ok())
            .collect();
        assert_eq!(
            exits,
            [
                (LineState::Unreviewed, Cause::PersonAction),
                (
                    LineState::Exception(ExceptionKind::SourceChanged),
                    Cause::RevisionAccepted
                ),
            ]
        );
    }

    #[test]
    fn names_and_codes_are_stable() {
        assert_eq!(
            LineState::Excluded(ExcludeReason::NonBusiness).name(),
            "excluded"
        );
        assert_eq!(
            ExceptionKind::SearchBudgetExhausted.code(),
            "search_budget_exhausted"
        );
        assert!(!LineState::Unreviewed.is_settled());
        assert!(LineState::Matched.is_settled());
        let kinds: std::collections::HashSet<_> = ActorKind::ALL.map(ActorKind::code).into();
        assert_eq!(kinds.len(), ActorKind::ALL.len());
    }
}
