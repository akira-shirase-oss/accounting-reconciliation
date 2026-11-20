//! The model port (D-016). A model only proposes: it answers a request that carries
//! the line, the candidate set the engine computed and their evidence fields as
//! structured data — file text delimited as [`Data`], never in the instructions;
//! account numbers are never sent — and its answer passes the [`validate`]
//! function before it can become a proposal row. Nothing here commits.
//!
//! The port is one trait, [`Proposer`], with three adapters: the recorded-response
//! adapter here (CI, drills and load runs), and the OpenAI-compatible and loopback
//! adapters that arrive with the egress plumbing (D-024).

mod answer;
mod port;
mod recorded;
mod request;
mod validate;

pub use answer::{Allocation, Answer, AnswerError, ResidualAnswer};
pub use port::{BoxFuture, ProposeError, Proposer};
pub use recorded::RecordedProposer;
pub use request::{Candidate, Data, LineFacts, ProposeRequest};
pub use validate::{ProposalKind, Refusal, Validated, ValidatedProposal, validate};
