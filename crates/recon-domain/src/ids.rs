//! Typed identifiers. Each wraps a UUID; the domain never generates one (that needs a
//! clock and randomness, which are I/O) — the database layer and the server do.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Wraps an existing UUID.
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// The UUID.
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }

            /// The UUID's 16 big-endian bytes (the canonical encoding in hashes).
            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id!(
    /// A firm: the tenant (D-020).
    FirmId
);
id!(
    /// A firm's client business.
    ClientId
);
id!(
    /// A client's bank account.
    AccountId
);
id!(
    /// One uploaded file's import.
    ImportId
);
id!(
    /// One statement line.
    LineId
);
id!(
    /// A firm's user.
    UserId
);
id!(
    /// One decision-log record.
    DecisionId
);
id!(
    /// A document (invoice, bill, receipt, credit note, book entry).
    DocumentId
);
id!(
    /// A match.
    MatchId
);
id!(
    /// A proposal (engine or model).
    ProposalId
);
id!(
    /// A rule (its versions are numbered).
    RuleId
);
