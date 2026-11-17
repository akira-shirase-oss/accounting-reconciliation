//! Import batches (D-008, D-010): a file identified by its SHA-256, staged, then complete,
//! refused, discarded or held as a revision (D-009) — never partly visible. The normalized
//! rows a parser produces are [`NewLine`] (statements) and
//! [`NewDocument`](crate::document::NewDocument) (documents).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::date::Date;
use crate::fingerprint::TupleHash;
use crate::money::Money;

/// The built-in statement profile's name (D-040): the R0 CSV layout, the default.
pub const PROFILE_R0: &str = "r0-generic-csv";
/// Its version, recorded on every import and decision (D-035). Version 2 (R1) adds balance
/// continuity and the formula-cell refusal to version 1 (D-061).
pub const PROFILE_R0_VERSION: i32 = 2;
/// The built-in OFX statement profile (D-063).
pub const PROFILE_OFX: &str = "ofx";
/// Its version.
pub const PROFILE_OFX_VERSION: i32 = 1;
/// The built-in document profile (D-064).
pub const PROFILE_DOCUMENTS: &str = "r1-generic-documents";
/// Its version.
pub const PROFILE_DOCUMENTS_VERSION: i32 = 1;

/// What an import brings in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    /// A bank statement into one account: statement lines.
    Statement,
    /// Documents of one client: invoices, bills, receipts, credit notes, book entries.
    Documents,
}

impl ImportKind {
    /// The stored name.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Statement => "statement",
            Self::Documents => "documents",
        }
    }
}

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
    /// Abandoned (a crash, a lost upload), a repeat of an already complete file, or a
    /// rejected revision. Final.
    Discarded,
    /// A statement that disagrees with stored lines in the range they share: held, with
    /// every difference listed, until a person accepts or rejects it (D-009). Nothing of it
    /// is visible.
    Revision,
}

impl ImportStatus {
    /// The stored name.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Complete => "complete",
            Self::Refused => "refused",
            Self::Discarded => "discarded",
            Self::Revision => "revision",
        }
    }

    /// Every status.
    pub const ALL: [Self; 5] = [
        Self::Staging,
        Self::Complete,
        Self::Refused,
        Self::Discarded,
        Self::Revision,
    ];

    /// Parses a stored name.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.code() == code)
    }

    /// `staging` moves to any other state; a held `revision` only to `complete` (accepted)
    /// or `discarded` (rejected); the rest are final.
    pub const fn can_become(self, to: Self) -> bool {
        matches!(
            (self, to),
            (
                Self::Staging,
                Self::Complete | Self::Refused | Self::Discarded | Self::Revision
            ) | (Self::Revision, Self::Complete | Self::Discarded)
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
    /// Another flip held the account; the file may be uploaded again (D-053).
    AccountBusy,
    /// A person rejected the revision this import was held as (D-009).
    RevisionRejected,
}

impl DiscardReason {
    /// The stored code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::DuplicateFile => "duplicate_file",
            Self::LeaseExpired => "lease_expired",
            Self::UploadFailed => "upload_failed",
            Self::AccountBusy => "account_busy",
            Self::RevisionRejected => "revision_rejected",
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

impl NewLine {
    /// Bytes this row holds in memory, approximately: its text fields plus a fixed overhead.
    /// Staging batches and the server's staging budget are counted in these (D-050).
    pub fn heap_bytes(&self) -> usize {
        let text = [
            Some(self.narration_raw.len()),
            Some(self.narration_norm.len()),
            self.reference.as_ref().map(String::len),
            self.counterparty_raw.as_ref().map(String::len),
            self.counterparty_norm.as_ref().map(String::len),
        ];
        text.into_iter()
            .flatten()
            .fold(192_usize, usize::saturating_add)
    }
}

/// A parsed row on its way to staging: its record number and the bytes it holds.
pub trait StagedRow {
    /// The record number in the file.
    fn row(&self) -> u32;
    /// Bytes held in memory, approximately (D-050).
    fn heap_bytes(&self) -> usize;
}

impl StagedRow for NewLine {
    fn row(&self) -> u32 {
        self.row
    }

    fn heap_bytes(&self) -> usize {
        Self::heap_bytes(self)
    }
}

/// How one row of a revision differs from the stored lines it overlaps (D-009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionChange {
    /// A row in the shared range that no stored line has.
    Added,
    /// A stored line in the shared range the new file does not have.
    Removed,
    /// A stored line and a new row that are the same transaction (one each with the same
    /// booking date and reference, or narration when neither has a reference) but differ.
    Changed,
}

impl RevisionChange {
    /// The stored code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Changed => "changed",
        }
    }
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
    fn only_staging_and_a_held_revision_move_and_counts_balance() {
        use ImportStatus as S;
        for from in S::ALL {
            for to in S::ALL {
                let allowed = (from == S::Staging && to != S::Staging)
                    || (from == S::Revision && matches!(to, S::Complete | S::Discarded));
                assert_eq!(from.can_become(to), allowed, "{from:?} -> {to:?}");
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
