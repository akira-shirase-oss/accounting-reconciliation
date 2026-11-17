//! Import batches (D-008, D-010): a file identified by its SHA-256, staged, then complete,
//! refused or discarded — never partly visible. The normalized row a parser produces is
//! [`NewLine`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::date::Date;
use crate::fingerprint::TupleHash;
use crate::money::Money;

/// The R0 import profile's name (D-040): one fixed CSV layout.
pub const PROFILE_R0: &str = "r0-generic-csv";
/// Its version, recorded on every import and decision (D-035).
pub const PROFILE_R0_VERSION: i32 = 1;

/// SHA-256 of an uploaded file's bytes: the file's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileHash(pub [u8; 32]);

impl fmt::Display for FileHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// An import's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    /// Rows are being staged; nothing is visible.
    Staging,
    /// The lines are visible, all at once. Final.
    Complete,
    /// At least one row (or the file) was refused; nothing was persisted. Final.
    Refused,
    /// Abandoned (a crash, a lost upload) or a repeat of an already complete file. Final.
    Discarded,
}

impl ImportStatus {
    /// The stored name.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Complete => "complete",
            Self::Refused => "refused",
            Self::Discarded => "discarded",
        }
    }

    /// Parses a stored name.
    pub fn from_code(code: &str) -> Option<Self> {
        [
            Self::Staging,
            Self::Complete,
            Self::Refused,
            Self::Discarded,
        ]
        .into_iter()
        .find(|s| s.code() == code)
    }

    /// Only `staging` moves, and only to a final state.
    pub const fn can_become(self, to: Self) -> bool {
        matches!(
            (self, to),
            (
                Self::Staging,
                Self::Complete | Self::Refused | Self::Discarded
            )
        )
    }
}

/// Why an import was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscardReason {
    /// The same file is already complete for the account; the earlier import answers.
    DuplicateFile,
    /// Its staging lease expired (the process died or stalled).
    LeaseExpired,
    /// The upload failed before the file was complete.
    UploadFailed,
}

impl DiscardReason {
    /// The stored code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::DuplicateFile => "duplicate_file",
            Self::LeaseExpired => "lease_expired",
            Self::UploadFailed => "upload_failed",
        }
    }
}

/// One validated, normalized statement row, ready to stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLine {
    /// The record number in the file (the header is row 1).
    pub row: u32,
    /// Booking date.
    pub booking_date: Date,
    /// Value date, when the file has one.
    pub value_date: Option<Date>,
    /// Signed amount in the account's currency.
    pub amount: Money,
    /// The narration as the bank wrote it.
    pub narration_raw: String,
    /// The narration after `normalization v1`.
    pub narration_norm: String,
    /// The bank reference, when present.
    pub reference: Option<String>,
    /// The counterparty as written, when present.
    pub counterparty_raw: Option<String>,
    /// The counterparty normalized, when present.
    pub counterparty_norm: Option<String>,
    /// The running balance after the row, when the file has one.
    pub balance_after: Option<Money>,
    /// The tuple hash (D-008); the occurrence index is assigned at the flip.
    pub tuple_hash: TupleHash,
}

/// Row accounting for a finished import: every row read is a new line, a duplicate of an
/// existing line, or refused (invariant 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImportCounts {
    /// Data rows read (the header is not counted).
    pub rows_read: u32,
    /// New lines.
    pub lines_created: u32,
    /// Rows whose fingerprint an existing line of the account already has.
    pub duplicates: u32,
    /// Refused rows.
    pub rows_refused: u32,
}

impl ImportCounts {
    /// True when every row is accounted for exactly once.
    pub fn is_complete_accounting(self) -> bool {
        u64::from(self.lines_created)
            .checked_add(u64::from(self.duplicates))
            .and_then(|s| s.checked_add(u64::from(self.rows_refused)))
            == Some(u64::from(self.rows_read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_staging_moves_and_counts_balance() {
        use ImportStatus as S;
        for from in [S::Staging, S::Complete, S::Refused, S::Discarded] {
            for to in [S::Staging, S::Complete, S::Refused, S::Discarded] {
                assert_eq!(from.can_become(to), from == S::Staging && to != S::Staging);
            }
            assert_eq!(S::from_code(from.code()), Some(from));
        }
        let counts = ImportCounts {
            rows_read: 10,
            lines_created: 7,
            duplicates: 2,
            rows_refused: 1,
        };
        assert!(counts.is_complete_accounting());
        assert!(
            !ImportCounts {
                rows_read: 11,
                ..counts
            }
            .is_complete_accounting()
        );
        assert!(
            !ImportCounts {
                lines_created: 8,
                ..counts
            }
            .is_complete_accounting()
        );
    }
}
