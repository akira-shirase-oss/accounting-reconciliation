//! Parsers from file bytes to normalized statement lines and documents (ARCHITECTURE:
//! `recon-import`). Pure and bounded: bytes arrive in chunks from whoever owns the I/O (the
//! server streams an upload into a parser); nothing here reads a file, a socket or a clock —
//! `scripts/check-architecture.sh` enforces it.
//!
//! The stages: [`decode`] (the file's encoding to UTF-8), [`csv`] (strict RFC 4180 records),
//! then a profile — [`profile`] for CSV statements (D-040, D-062), [`ofx`] for OFX 1.x SGML
//! and 2.x XML statements (D-063), [`documents`] for invoices, bills, receipts, credit notes
//! and book entries (D-064). Dates and amounts are read through the declared [`dates`] and
//! [`number`] formats, never guessed.
#![deny(clippy::arithmetic_side_effects)]

pub mod csv;
pub mod dates;
pub mod decode;
pub mod number;
pub mod profile;

pub use csv::{CsvLimits, FileRefusal};
pub use profile::{
    AccountContext, CompiledStatement, ParseSummary, StatementParser, StatementProfile,
};
