//! Accounting Reconciliation's command line (the `recon` binary): the checks an accountant
//! runs before an upload ([`check`]), and the seeded data generator the tests, drills and
//! load runs share ([`generate`]).

pub mod check;
pub mod generate;
