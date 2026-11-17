//! Line identity (D-008). A line's *tuple* is (account, booking date, amount, currency,
//! normalized narration, bank reference when present); its *fingerprint* adds the
//! occurrence index — how many earlier rows of the same file share the tuple — so two
//! genuine identical transactions on one day are two lines, while the same transaction in
//! two files is one.
//!
//! Every field is length-prefixed (`u32` big-endian) or fixed-width, so no two different
//! tuples share bytes. The import flip computes the fingerprint from the tuple hash in SQL
//! (`recon-db`'s migration); a test there proves both definitions agree.

use sha2::{Digest, Sha256};

use crate::date::Date;
use crate::ids::AccountId;
use crate::money::Money;

/// The fingerprint definition's version, part of every hashed byte string.
pub const FINGERPRINT_VERSION: &str = "v1";

const TUPLE_DOMAIN: &[u8] = b"recon-tuple-v1";
const FINGERPRINT_DOMAIN: &[u8] = b"recon-fp-v1";

/// SHA-256 of a line's tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TupleHash(pub [u8; 32]);

/// SHA-256 of a tuple hash and an occurrence index: a line's identity in its account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

/// The fields of a line that identify it.
#[derive(Debug, Clone, Copy)]
pub struct LineTuple<'a> {
    /// The account.
    pub account: AccountId,
    /// The booking date.
    pub booking_date: Date,
    /// The signed amount and its currency.
    pub amount: Money,
    /// The narration after `normalization v1`.
    pub narration_norm: &'a str,
    /// The bank's reference, when the file carries one.
    pub reference: Option<&'a str>,
}

fn push_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
}

/// The tuple hash.
pub fn tuple_hash(tuple: &LineTuple<'_>) -> TupleHash {
    let mut h = Sha256::new();
    h.update(TUPLE_DOMAIN);
    h.update(tuple.account.as_bytes());
    h.update(tuple.booking_date.to_iso().as_bytes());
    h.update(tuple.amount.minor().to_be_bytes());
    h.update(tuple.amount.currency().code().as_bytes());
    push_len_prefixed(&mut h, tuple.narration_norm.as_bytes());
    match tuple.reference {
        None => h.update([0_u8]),
        Some(r) => {
            h.update([1_u8]);
            push_len_prefixed(&mut h, r.as_bytes());
        }
    }
    TupleHash(h.finalize().into())
}

/// The fingerprint of the `occurrence`-th row (0-based) of a file with this tuple.
pub fn fingerprint(tuple: &TupleHash, occurrence: u32) -> Fingerprint {
    let mut h = Sha256::new();
    h.update(FINGERPRINT_DOMAIN);
    h.update(tuple.0);
    h.update(occurrence.to_be_bytes());
    Fingerprint(h.finalize().into())
}

/// The digest of an import's lines: SHA-256 over their fingerprints in row order. The
/// import's completion decision carries it, so the lines behind a decision can be checked.
pub fn lines_digest<'a>(fingerprints: impl IntoIterator<Item = &'a Fingerprint>) -> [u8; 32] {
    let mut h = Sha256::new();
    for f in fingerprints {
        h.update(f.0);
    }
    h.finalize().into()
}

/// Assigns occurrence indexes to tuples in file order (the reference the SQL flip must
/// equal): the n-th repeat of a tuple gets index n.
pub fn occurrences(tuples: &[TupleHash]) -> Vec<u32> {
    let mut seen = std::collections::HashMap::<TupleHash, u32>::with_capacity(tuples.len());
    tuples
        .iter()
        .map(|t| {
            let n = seen.entry(*t).or_insert(0);
            let current = *n;
            *n = n.saturating_add(1);
            current
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;
    use crate::money::Currency;
    use uuid::Uuid;

    fn tuple<'a>(narration: &'a str, reference: Option<&'a str>) -> LineTuple<'a> {
        LineTuple {
            account: AccountId::from_uuid(Uuid::from_u128(7)),
            booking_date: Date::new(2026, 3, 1).unwrap(),
            amount: Money::new(-12_500, Currency::Usd).unwrap(),
            narration_norm: narration,
            reference,
        }
    }

    #[test]
    fn identical_twins_get_distinct_fingerprints_and_files_agree() {
        let t = tuple_hash(&tuple("ACME", None));
        let occ = occurrences(&[t, t, t]);
        assert_eq!(occ, [0, 1, 2]);
        let fps: Vec<_> = occ.iter().map(|o| fingerprint(&t, *o)).collect();
        assert_ne!(fps[0], fps[1]);
        assert_ne!(fps[1], fps[2]);
        // The same transaction seen first in another file has the same identity.
        assert_eq!(fingerprint(&t, 0), fps[0]);
    }

    #[test]
    fn field_boundaries_and_absent_references_are_unambiguous() {
        let a = tuple_hash(&tuple("AB", Some("C")));
        let b = tuple_hash(&tuple("A", Some("BC")));
        let c = tuple_hash(&tuple("ABC", None));
        let d = tuple_hash(&tuple("ABC", Some("")));
        assert_ne!(a, b);
        assert_ne!(c, d);
        let mut other_account = tuple("AB", Some("C"));
        other_account.account = AccountId::from_uuid(Uuid::from_u128(8));
        assert_ne!(tuple_hash(&other_account), a);
    }

    #[test]
    fn the_definition_is_pinned() {
        // A change to the definition changes every stored fingerprint. These values were
        // computed independently (Python hashlib over the documented byte layout).
        let t = tuple_hash(&tuple("ACME LTD", Some("INV-1")));
        assert_eq!(
            hex::encode(t.0),
            "7eee13dacb2e4bc211e3a7e92645ffb8b911a278d96bd6652ff37453d1753c94"
        );
        assert_eq!(
            hex::encode(fingerprint(&t, 0).0),
            "eaaff4f03ef7558c1c2a5fb6cd6c07f7173a19e16b9ca4985603215a25733594"
        );
        assert_eq!(
            hex::encode(lines_digest(std::iter::empty())),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
