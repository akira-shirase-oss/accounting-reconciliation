//! The decision log's records and their hash chain (D-019, D-042). One chain per client:
//! record *n* stores record *n−1*'s hash and its own SHA-256 over canonical bytes, so an
//! edit made around the database's append-only guards breaks every later link.
//!
//! The database computes `seq`, `prev_hash`, `decided_at` and `record_hash` in a trigger
//! (`recon-db`'s migration, `recon.decision_canonical`); [`DecisionRecord::canonical_bytes`]
//! is the same definition in Rust, used by `audit verify`. A test in `recon-db` proves the
//! two agree byte for byte.
//!
//! Canonical bytes, v1 (`lp(x)` = `u32` big-endian length, then the bytes; `opt(x)` = `0x00`
//! for absent or `0x01` then `lp(x)`; UUIDs as their 16 bytes; integers big-endian):
//!
//! ```text
//! "recon-decision-v1" firm client seq:i64 id prev_hash:32
//! lp(subject_kind) subject_id opt(from_state) lp(to_state)
//! lp(actor_kind) lp(actor_ref) lp(reason_code) lp(evidence) lp(engine_version)
//! decided_at:i64 (microseconds since the Unix epoch)
//! ```

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::ids::{ClientId, DecisionId, FirmId};

const RECORD_DOMAIN: &[u8] = b"recon-decision-v1";
const GENESIS_DOMAIN: &[u8] = b"recon-chain-v1";

/// The longest evidence a decision may carry, in bytes (the database checks it too).
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

/// What a decision is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// An import batch.
    Import,
    /// A statement line.
    Line,
    /// A match.
    Match,
    /// A rule version.
    Rule,
    /// A period.
    Period,
}

impl SubjectKind {
    /// The stored name.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Line => "line",
            Self::Match => "match",
            Self::Rule => "rule",
            Self::Period => "period",
        }
    }
}

/// True for a valid reason code: `^[a-z][a-z0-9_]{0,63}$`.
pub fn is_reason_code(code: &str) -> bool {
    let b = code.as_bytes();
    matches!(b.first(), Some(c) if c.is_ascii_lowercase())
        && b.len() <= 64
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// A stored decision, exactly as the log holds it (strings as stored, so verification
/// hashes what is there, tampered or not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    /// The firm.
    pub firm: FirmId,
    /// The client whose chain this is.
    pub client: ClientId,
    /// Position in the chain, from 1.
    pub seq: i64,
    /// The record's id.
    pub id: DecisionId,
    /// The previous record's hash (the genesis hash for seq 1).
    pub prev_hash: [u8; 32],
    /// This record's hash.
    pub record_hash: [u8; 32],
    /// `import`, `line`, `match`, `rule` or `period`.
    pub subject_kind: String,
    /// The subject's id.
    pub subject_id: Uuid,
    /// The state before (absent for a creation).
    pub from_state: Option<String>,
    /// The state after.
    pub to_state: String,
    /// The actor kind.
    pub actor_kind: String,
    /// Who exactly: a user id, a rule and version, an import, `operator`.
    pub actor_ref: String,
    /// Why, as a code.
    pub reason_code: String,
    /// Evidence bytes (canonical JSON; encrypted per client from R3 on, D-019).
    pub evidence: Vec<u8>,
    /// Engine, profile and scoring versions in force (D-035).
    pub engine_version: String,
    /// The database clock, microseconds since the Unix epoch.
    pub decided_at_micros: i64,
}

fn lp(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

impl DecisionRecord {
    /// The canonical bytes (v1) the record hash covers.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            256_usize
                .saturating_add(self.evidence.len())
                .saturating_add(self.actor_ref.len()),
        );
        out.extend_from_slice(RECORD_DOMAIN);
        out.extend_from_slice(self.firm.as_bytes());
        out.extend_from_slice(self.client.as_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(self.id.as_bytes());
        out.extend_from_slice(&self.prev_hash);
        lp(&mut out, self.subject_kind.as_bytes());
        out.extend_from_slice(self.subject_id.as_bytes());
        match &self.from_state {
            None => out.push(0),
            Some(s) => {
                out.push(1);
                lp(&mut out, s.as_bytes());
            }
        }
        lp(&mut out, self.to_state.as_bytes());
        lp(&mut out, self.actor_kind.as_bytes());
        lp(&mut out, self.actor_ref.as_bytes());
        lp(&mut out, self.reason_code.as_bytes());
        lp(&mut out, &self.evidence);
        lp(&mut out, self.engine_version.as_bytes());
        out.extend_from_slice(&self.decided_at_micros.to_be_bytes());
        out
    }

    /// SHA-256 of the canonical bytes: what `record_hash` must equal.
    pub fn computed_hash(&self) -> [u8; 32] {
        Sha256::digest(self.canonical_bytes()).into()
    }
}

/// The hash a client's chain starts from.
pub fn genesis_hash(firm: FirmId, client: ClientId) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(GENESIS_DOMAIN);
    h.update(firm.as_bytes());
    h.update(client.as_bytes());
    h.finalize().into()
}

/// How a chain is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakKind {
    /// The record belongs to another firm or client.
    WrongChain,
    /// The sequence skips or repeats a number.
    SequenceGap,
    /// `prev_hash` is not the previous record's hash.
    PreviousHashMismatch,
    /// `record_hash` is not the hash of the record's canonical bytes.
    RecordHashMismatch,
    /// The chain verifies, but the stored head (sequence or hash) is not its last record:
    /// records were removed from the end, or the head was rewritten.
    HeadMismatch,
}

/// The first broken link: `recon audit verify` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("decision chain broken at seq {seq}: {kind:?}")]
pub struct ChainBreak {
    /// The first record that does not verify.
    pub seq: i64,
    /// Why.
    pub kind: BreakKind,
}

/// The verified head of a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainHead {
    /// The last verified sequence number (0 for an empty chain).
    pub seq: i64,
    /// The last verified hash (the genesis hash for an empty chain).
    pub hash: [u8; 32],
}

/// Verifies one client's chain record by record, in `seq` order, in constant memory.
#[derive(Debug, Clone)]
pub struct ChainVerifier {
    firm: FirmId,
    client: ClientId,
    head: ChainHead,
}

impl ChainVerifier {
    /// Starts at the genesis hash.
    pub fn new(firm: FirmId, client: ClientId) -> Self {
        Self {
            firm,
            client,
            head: ChainHead {
                seq: 0,
                hash: genesis_hash(firm, client),
            },
        }
    }

    /// Checks the next record.
    pub fn push(&mut self, record: &DecisionRecord) -> Result<(), ChainBreak> {
        let fail = |kind| ChainBreak {
            seq: record.seq,
            kind,
        };
        if record.firm != self.firm || record.client != self.client {
            return Err(fail(BreakKind::WrongChain));
        }
        if Some(record.seq) != self.head.seq.checked_add(1) {
            return Err(fail(BreakKind::SequenceGap));
        }
        if record.prev_hash != self.head.hash {
            return Err(fail(BreakKind::PreviousHashMismatch));
        }
        if record.computed_hash() != record.record_hash {
            return Err(fail(BreakKind::RecordHashMismatch));
        }
        self.head = ChainHead {
            seq: record.seq,
            hash: record.record_hash,
        };
        Ok(())
    }

    /// The head verified so far.
    pub const fn head(&self) -> ChainHead {
        self.head
    }
}

/// The evidence of an import's completion decision (canonical JSON in field order). It
/// carries counts and digests only — no narration, amount or file name (D-025).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportEvidence {
    /// The import.
    pub import: Uuid,
    /// The account.
    pub account: Uuid,
    /// SHA-256 of the file, hex.
    pub file_hash: String,
    /// The profile.
    pub profile: String,
    /// The profile's version.
    pub profile_version: i32,
    /// The fingerprint definition's version.
    pub fingerprint_version: String,
    /// Data rows read.
    pub rows_read: u32,
    /// New lines.
    pub lines_created: u32,
    /// Rows already present as lines.
    pub duplicates: u32,
    /// SHA-256 over every row's fingerprint in row order, hex.
    pub lines_digest: String,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;

    fn chain(n: i64) -> (FirmId, ClientId, Vec<DecisionRecord>) {
        let firm = FirmId::from_uuid(Uuid::from_u128(1));
        let client = ClientId::from_uuid(Uuid::from_u128(2));
        let mut prev = genesis_hash(firm, client);
        let mut out = vec![];
        for seq in 1..=n {
            let mut r = DecisionRecord {
                firm,
                client,
                seq,
                id: DecisionId::from_uuid(Uuid::from_u128(100 + u128::try_from(seq).unwrap())),
                prev_hash: prev,
                record_hash: [0; 32],
                subject_kind: "import".into(),
                subject_id: Uuid::from_u128(500),
                from_state: (seq > 1).then(|| "staging".into()),
                to_state: "complete".into(),
                actor_kind: "operator".into(),
                actor_ref: "operator".into(),
                reason_code: "import_completed".into(),
                evidence: format!("{{\"n\":{seq}}}").into_bytes(),
                engine_version: "recon 0.1.0".into(),
                decided_at_micros: 1_700_000_000_000_000 + seq,
            };
            r.record_hash = r.computed_hash();
            prev = r.record_hash;
            out.push(r);
        }
        (firm, client, out)
    }

    fn verify(
        firm: FirmId,
        client: ClientId,
        records: &[DecisionRecord],
    ) -> Result<ChainHead, ChainBreak> {
        let mut v = ChainVerifier::new(firm, client);
        for r in records {
            v.push(r)?;
        }
        Ok(v.head())
    }

    #[test]
    fn an_intact_chain_verifies_to_its_head() {
        let (firm, client, records) = chain(5);
        let head = verify(firm, client, &records).unwrap();
        assert_eq!(head.seq, 5);
        assert_eq!(head.hash, records[4].record_hash);
        assert_eq!(
            verify(firm, client, &[]).unwrap().hash,
            genesis_hash(firm, client)
        );
    }

    #[test]
    fn every_kind_of_tampering_names_the_first_broken_link() {
        let (firm, client, records) = chain(5);
        let mut edited = records.clone();
        edited[2].reason_code = "approved".into();
        assert_eq!(
            verify(firm, client, &edited),
            Err(ChainBreak {
                seq: 3,
                kind: BreakKind::RecordHashMismatch
            })
        );

        // Re-hashing the edited record moves the break to the next link.
        edited[2].record_hash = edited[2].computed_hash();
        assert_eq!(
            verify(firm, client, &edited),
            Err(ChainBreak {
                seq: 4,
                kind: BreakKind::PreviousHashMismatch
            })
        );

        let mut missing = records.clone();
        missing.remove(1);
        assert_eq!(
            verify(firm, client, &missing),
            Err(ChainBreak {
                seq: 3,
                kind: BreakKind::SequenceGap
            })
        );

        let other = ClientId::from_uuid(Uuid::from_u128(3));
        assert_eq!(
            verify(firm, other, &records),
            Err(ChainBreak {
                seq: 1,
                kind: BreakKind::WrongChain
            })
        );

        let mut evidence = records;
        evidence[0].evidence.push(b' ');
        assert_eq!(verify(firm, client, &evidence).unwrap_err().seq, 1);
    }

    #[test]
    fn absent_and_empty_from_state_hash_differently() {
        let (_, _, records) = chain(1);
        let mut a = records[0].clone();
        a.from_state = None;
        let mut b = a.clone();
        b.from_state = Some(String::new());
        assert_ne!(a.computed_hash(), b.computed_hash());
    }

    #[test]
    fn reason_codes() {
        assert!(is_reason_code("import_completed"));
        assert!(is_reason_code("t1_auto_commit"));
        for bad in ["", "Import", "1abc", "a-b", "a b", &"a".repeat(65)] {
            assert!(!is_reason_code(bad), "{bad:?}");
        }
    }
}
