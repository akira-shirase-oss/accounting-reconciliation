//! Parsers from file bytes to normalized statement lines (ARCHITECTURE: `recon-import`).
//! Pure and bounded: bytes arrive in chunks from whoever owns the I/O (the server streams
//! an upload into [`profile::StatementParser`]); nothing here reads a file, a socket or a
//! clock — `scripts/check-architecture.sh` enforces it.
//!
//! R0 has one fixed profile (`r0-generic-csv` v1, D-040); R1 adds declared profiles, OFX and
//! document imports.
#![deny(clippy::arithmetic_side_effects)]

pub mod csv;
pub mod profile;

pub use csv::{CsvLimits, FileRefusal};
pub use profile::{AccountContext, ParseSummary, StatementParser};
