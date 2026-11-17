//! Head anchors (D-049): a client's chain head, dated and signed, kept outside the database.
//!
//! Inside the database, whoever can act as the schema's owner (or a superuser) can disable
//! the log's guards and rewrite a chain's tail consistently, head included; no check made
//! against the database alone can catch that. An anchor records `(firm, client, seq, head
//! hash, when)` under an HMAC-SHA256 with a key only the operator holds, in a file the
//! server cannot write. Verification against anchors then requires the record at every
//! anchored sequence to exist with the anchored hash.
//!
//! Signed bytes, v1: `"recon-anchor-v1" firm client seq:i64 head:32 at:i64` (UUIDs as 16
//! bytes, integers big-endian, `at` in microseconds since the Unix epoch).

use hmac::{Hmac, KeyInit as _, Mac as _};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::ids::{ClientId, FirmId};

const DOMAIN: &[u8] = b"recon-anchor-v1";

/// The shortest key accepted, in bytes.
pub const MIN_KEY_BYTES: usize = 32;

/// One anchored head, as one line of an anchor file (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// The firm.
    pub firm: Uuid,
    /// The client.
    pub client: Uuid,
    /// The head's sequence number.
    pub seq: i64,
    /// The head's hash, hex.
    pub head: String,
    /// When it was read (the database clock), microseconds since the Unix epoch.
    pub at: i64,
    /// HMAC-SHA256 over the v1 bytes, hex.
    pub mac: String,
}

/// Why an anchor is not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AnchorError {
    /// The key is shorter than [`MIN_KEY_BYTES`].
    #[error("anchor_key_too_short")]
    KeyTooShort,
    /// The head is not 32 bytes of hex.
    #[error("anchor_malformed")]
    Malformed,
    /// The MAC does not match: the anchor was altered, or another key signed it.
    #[error("anchor_signature_mismatch")]
    BadSignature,
}

fn signed_bytes(firm: FirmId, client: ClientId, seq: i64, head: &[u8; 32], at: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(DOMAIN.len().saturating_add(80));
    out.extend_from_slice(DOMAIN);
    out.extend_from_slice(firm.as_bytes());
    out.extend_from_slice(client.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(head);
    out.extend_from_slice(&at.to_be_bytes());
    out
}

fn mac(key: &[u8], bytes: &[u8]) -> Result<Hmac<Sha256>, AnchorError> {
    if key.len() < MIN_KEY_BYTES {
        return Err(AnchorError::KeyTooShort);
    }
    let mut m = Hmac::<Sha256>::new_from_slice(key).map_err(|_| AnchorError::KeyTooShort)?;
    m.update(bytes);
    Ok(m)
}

impl Anchor {
    /// Signs a head.
    pub fn sign(
        key: &[u8],
        firm: FirmId,
        client: ClientId,
        seq: i64,
        head: [u8; 32],
        at: i64,
    ) -> Result<Self, AnchorError> {
        let tag = mac(key, &signed_bytes(firm, client, seq, &head, at))?
            .finalize()
            .into_bytes();
        Ok(Self {
            firm: firm.as_uuid(),
            client: client.as_uuid(),
            seq,
            head: hex::encode(head),
            at,
            mac: hex::encode(tag),
        })
    }

    /// Checks the signature and returns the anchored `(seq, head)`.
    pub fn verify(&self, key: &[u8]) -> Result<(i64, [u8; 32]), AnchorError> {
        let head: [u8; 32] = hex::decode(&self.head)
            .ok()
            .and_then(|h| h.try_into().ok())
            .ok_or(AnchorError::Malformed)?;
        let tag = hex::decode(&self.mac).map_err(|_| AnchorError::Malformed)?;
        let bytes = signed_bytes(
            FirmId::from_uuid(self.firm),
            ClientId::from_uuid(self.client),
            self.seq,
            &head,
            self.at,
        );
        mac(key, &bytes)?
            .verify_slice(&tag)
            .map_err(|_| AnchorError::BadSignature)?;
        Ok((self.seq, head))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn a_signed_anchor_verifies_and_any_change_or_other_key_fails() {
        let key = [7_u8; 32];
        let firm = FirmId::from_uuid(Uuid::from_u128(1));
        let client = ClientId::from_uuid(Uuid::from_u128(2));
        let a = Anchor::sign(&key, firm, client, 42, [9; 32], 1_700_000_000_000_000).unwrap();
        assert_eq!(a.verify(&key), Ok((42, [9; 32])));
        assert_eq!(a.verify(&[8_u8; 32]), Err(AnchorError::BadSignature));
        for tampered in [
            Anchor {
                seq: 41,
                ..a.clone()
            },
            Anchor {
                at: a.at + 1,
                ..a.clone()
            },
            Anchor {
                client: Uuid::from_u128(3),
                ..a.clone()
            },
            Anchor {
                head: hex::encode([8_u8; 32]),
                ..a.clone()
            },
        ] {
            assert_eq!(tampered.verify(&key), Err(AnchorError::BadSignature));
        }
        assert_eq!(
            Anchor {
                head: "zz".into(),
                ..a.clone()
            }
            .verify(&key),
            Err(AnchorError::Malformed)
        );
        assert_eq!(
            Anchor::sign(&[1; 31], firm, client, 1, [0; 32], 0),
            Err(AnchorError::KeyTooShort)
        );
    }
}
