//! Who a signed-in person is and what their role may do (D-018, D-021). The matrix is
//! the one place "may this role" is answered; the server enforces it on every route.

use serde::{Deserialize, Serialize};

/// A firm user's role (D-018). Roles are cumulative: each includes the powers below it —
/// a small firm's reviewer also prepares and every role reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Reads.
    Viewer,
    /// Imports, resolves exceptions directly, approves ordinary proposals under the
    /// firm's threshold.
    Preparer,
    /// Any approval including the second eye, rules, period close.
    Reviewer,
    /// Firm settings, users, model use, reopening periods.
    Owner,
}

impl Role {
    /// Every role, weakest first.
    pub const ALL: [Self; 4] = [Self::Viewer, Self::Preparer, Self::Reviewer, Self::Owner];

    /// The stored name.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Preparer => "preparer",
            Self::Reviewer => "reviewer",
            Self::Owner => "owner",
        }
    }

    /// The role named `code`, or `None`.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.code() == code)
    }
}

/// What a route needs, in domain terms: every protected handler names its `Act` and the
/// matrix below answers. Amount- and match-dependent conditions (the four-eyes threshold,
/// a preparer's T2/T3 bound) are checked by the caller on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Act {
    /// Read the firm's own data.
    Read,
    /// Upload statements and documents; manage import profiles; accept or reject a held
    /// revision; start a matching job.
    Import,
    /// Decide a line directly: exclude it or place a manual-match proposal.
    Resolve,
    /// Approve or reject an ordinary proposal at or under the firm's threshold.
    ApproveProposal,
    /// The second eye on a four-eyes match, and any approval above the threshold.
    ApproveFourEyes,
    /// Acknowledge an exception in place — reviewer-level, it is what a close
    /// accepts (D-006).
    AcknowledgeException,
    /// Approve or create a rule version (D-015).
    ManageRules,
    /// Close a period.
    ClosePeriod,
    /// Reopen a closed period (owner only, with a reason).
    ReopenPeriod,
    /// Firm settings and the model-use decision.
    FirmSettings,
    /// Create, assign and disable users.
    ManageUsers,
}

/// `true` when `role` may `act`. Cumulative: each role holds the powers below it.
pub const fn may(role: Role, act: Act) -> bool {
    use Act as A;
    match role {
        Role::Viewer => matches!(act, A::Read),
        Role::Preparer => matches!(act, A::Read | A::Import | A::Resolve | A::ApproveProposal),
        Role::Reviewer => matches!(
            act,
            A::Read
                | A::Import
                | A::Resolve
                | A::ApproveProposal
                | A::ApproveFourEyes
                | A::AcknowledgeException
                | A::ManageRules
                | A::ClosePeriod
        ),
        Role::Owner => true,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::missing_panics_doc,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;

    /// The matrix is exactly cumulative: every role holds the powers below it, nothing
    /// more, and only the owner reopens periods or manages the firm.
    #[test]
    fn the_matrix_is_cumulative_and_owner_only_at_the_top() {
        for &weaker in &Role::ALL {
            for &stronger in &Role::ALL {
                if stronger > weaker {
                    for act in [
                        Act::Read,
                        Act::Import,
                        Act::Resolve,
                        Act::ApproveProposal,
                        Act::ApproveFourEyes,
                        Act::AcknowledgeException,
                        Act::ManageRules,
                        Act::ClosePeriod,
                    ] {
                        assert!(
                            may(stronger, act) || !may(weaker, act),
                            "{weaker:?} has {act:?} but {stronger:?} lacks it"
                        );
                    }
                }
            }
        }
        assert!(!may(Role::Reviewer, Act::ReopenPeriod));
        assert!(!may(Role::Reviewer, Act::FirmSettings));
        assert!(!may(Role::Reviewer, Act::ManageUsers));
        assert!(!may(Role::Preparer, Act::ApproveFourEyes));
        assert!(!may(Role::Viewer, Act::ApproveProposal));
        assert!(may(Role::Owner, Act::ReopenPeriod));
    }

    /// Codes round-trip and are the schema's CHECK list.
    #[test]
    fn role_codes_round_trip() {
        for role in Role::ALL {
            assert_eq!(Role::from_code(role.code()), Some(role));
        }
        assert_eq!(Role::from_code("admin"), None);
    }
}
