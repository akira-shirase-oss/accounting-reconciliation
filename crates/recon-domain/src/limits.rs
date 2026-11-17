//! Every bound on an import (D-010). Changing one needs a measurement and a decision.

/// Largest upload: 50 MiB.
pub const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;
/// Most data rows in one file (the header is not counted).
pub const MAX_ROWS: u32 = 2_000_000;
/// Longest physical record, terminator included: 64 KiB.
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// Most fields in one record.
pub const MAX_FIELDS: usize = 256;
/// Longest field: 8 KiB.
pub const MAX_FIELD_BYTES: usize = 8 * 1024;
/// Most row errors an import report lists (the count covers them all).
pub const MAX_REPORTED_ROW_ERRORS: usize = 1_000;
/// Longest a parse may take, in seconds (enforced by the caller, which owns the clock).
pub const PARSE_DEADLINE_SECS: u64 = 60;
