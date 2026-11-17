//! Pure domain of Accounting Reconciliation: exact money over the D-005 table, strict
//! dates, field text rules, the normalized line and its fingerprint (D-008), the line and
//! match state machines (D-006, D-011), balanced matches (D-014), import batches (D-010),
//! documents (D-007) and the hash-chained decision record (D-019).
//!
//! No I/O, no clock, no randomness: `scripts/check-architecture.sh` fails the build if
//! this crate gains an I/O dependency or touches the filesystem, network, environment or
//! time, so every rule here is a cheap, deterministic test.
#![deny(clippy::arithmetic_side_effects)]

pub mod anchor;
pub mod date;
pub mod decision;
pub mod document;
pub mod fingerprint;
pub mod ids;
pub mod import;
pub mod limits;
pub mod line;
pub mod matching;
pub mod money;
pub mod refusal;
pub mod text;

pub use date::Date;
pub use ids::{AccountId, ClientId, DecisionId, DocumentId, FirmId, ImportId, LineId, UserId};
pub use money::{Currency, Money, MoneyError, MoneySum};
pub use refusal::{Field, RefusalCode, RowRefusal};
