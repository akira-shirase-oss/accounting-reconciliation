//! What a caller sends a proposer: the line, the candidate set the engine
//! computed and their evidence fields — structured data only, so the answer can be
//! checked against the set it was given (D-016). Free text travels in [`Data`]
//! wrappers: delimited on the wire, never instruction. The type carries no account
//! field at all — an account number cannot be sent because there is nowhere to put
//! it (D-016's "account numbers are never sent").

use recon_domain::document::DocumentKind;
use recon_domain::{Currency, Date, DocumentId, LineId, Money};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};

/// Text a model may read but must never act on as instruction: narrations,
/// counterparty names, references and file excerpts. Serializes as `{"data": …}` —
/// the delimiter is on the wire, not a convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Data<T>(pub T);

impl<T: Serialize> Serialize for Data<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("data", &self.0)?;
        map.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Data<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire<T> {
            data: T,
        }
        Ok(Self(Wire::deserialize(deserializer)?.data))
    }
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's serialize_with contract takes the field by reference"
)]
fn date_iso<S: Serializer>(date: &Date, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&date.to_string())
}

/// A bank line's facts. `narration` and `counterparty` are data a model reads;
/// nothing here is an instruction to it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineFacts {
    /// The line under review.
    pub id: LineId,
    /// The booking date.
    #[serde(serialize_with = "date_iso")]
    pub booking_date: Date,
    /// The signed amount (bank's view) and its currency — the answer must balance it.
    pub amount: Money,
    /// The narration, delimited as data.
    pub narration: Data<String>,
    /// The counterparty as the bank printed it, delimited as data.
    pub counterparty: Option<Data<String>>,
}

/// One candidate document, exactly as the engine ranked it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    /// The document — the only name the answer may give it back by.
    pub id: DocumentId,
    /// The document's amount.
    pub amount: Money,
    /// What remains unallocated on it; an allocation beyond this is refused.
    pub open: Money,
    /// The document's date.
    #[serde(serialize_with = "date_iso")]
    pub date: Date,
    /// The document's kind.
    pub kind: DocumentKind,
    /// Its reference text (invoice number and the like), delimited as data.
    pub reference: Data<String>,
    /// Its counterparty name, delimited as data.
    pub counterparty: Option<Data<String>>,
    /// An excerpt of the document's parsed text, delimited as data.
    pub excerpt: Option<Data<String>>,
}

/// One proposal request: the line and the candidate set, with the prompt's
/// version so every proposal records what asked for it (D-016).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProposeRequest {
    /// The prompt template's version, recorded on every proposal the answer becomes.
    pub prompt_version: String,
    /// The line to explain.
    pub line: LineFacts,
    /// The closed set the answer may choose from — anything else is refused.
    pub candidates: Vec<Candidate>,
}

impl ProposeRequest {
    /// The currency every allocation and residual must name.
    pub fn currency(&self) -> Currency {
        self.line.amount.currency()
    }
}
