//! The recorded-response adapter (D-016): CI, drills and load runs never touch a
//! provider. It replays registered answers by prompt version — an unregistered
//! prompt is [`ProposeError::Unrecorded`], never a guess — and keeps every request
//! it was asked, so a test can assert what would have been sent (no account
//! fields, text only in `data` fields, the bounded shape).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{Answer, BoxFuture, ProposeError, ProposeRequest, Proposer};

/// A proposer that replays recorded answers. Keys are prompt versions — one
/// fixture per prompt, so a version bump is a deliberate fixture change.
#[derive(Debug, Default)]
pub struct RecordedProposer {
    answers: HashMap<String, String>,
    requests: Mutex<Vec<ProposeRequest>>,
}

impl RecordedProposer {
    /// An empty adapter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `answer_json` (the answer schema's text) for `prompt_version`.
    #[must_use]
    pub fn with_answer(mut self, prompt_version: &str, answer_json: &str) -> Self {
        self.answers
            .insert(prompt_version.to_owned(), answer_json.to_owned());
        self
    }

    /// Every request the adapter was asked, in order — what the wire would carry.
    /// An empty log on a poisoned mutex: a poisoned test adapter degrades, it
    /// does not panic the suite.
    pub fn requests(&self) -> Vec<ProposeRequest> {
        self.requests
            .lock()
            .map(|log| log.clone())
            .unwrap_or_default()
    }
}

impl Proposer for RecordedProposer {
    fn propose<'a>(
        &'a self,
        request: &'a ProposeRequest,
    ) -> BoxFuture<'a, Result<Answer, ProposeError>> {
        if let Ok(mut log) = self.requests.lock() {
            log.push(request.clone());
        }
        let out = self
            .answers
            .get(&request.prompt_version)
            .map_or(Err(ProposeError::Unrecorded), |raw| {
                Answer::parse(raw).map_err(|_| ProposeError::Malformed)
            });
        Box::pin(async move { out })
    }
}
