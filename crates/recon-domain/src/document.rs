//! Documents (D-007): what statement lines are reconciled against — sales invoices,
//! purchase bills, receipts, credit notes and book entries exported from the books — in
//! the same normalized shape: signed minor units as the bank would see them (money in
//! positive, money out negative), dates, raw and normalized text, the source file and row.
//!
//! A document's identity (`document fingerprint v1`, D-064) is SHA-256 over its client,
//! kind, normalized number, issue date, amount, currency and normalized counterparty, plus
//! an occurrence index like a line's, so the same export imported twice adds nothing.
//! A number may name only one document per (client, kind, counterparty): a second, different
//! document with it is `document_number_conflict`, never a silent second invoice.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::date::Date;
use crate::fingerprint::TupleHash;
use crate::ids::ClientId;
use crate::import::StagedRow;
use crate::money::Money;

const DOCUMENT_TUPLE_DOMAIN: &[u8] = b"recon-document-tuple-v1";

/// A document's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// An invoice the client issued: money in.
    SalesInvoice,
    /// A bill the client received: money out.
    PurchaseBill,
    /// A receipt for money the client received: money in.
    Receipt,
    /// A credit note the client issued to a customer: money out.
    CreditNote,
    /// A cash-book or bank-book entry from the books, signed as the bank would see it.
    BookEntry,
}

impl DocumentKind {
    /// Every kind.
    pub const ALL: [Self; 5] = [
        Self::SalesInvoice,
        Self::PurchaseBill,
        Self::Receipt,
        Self::CreditNote,
        Self::BookEntry,
    ];

    /// The stored code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::SalesInvoice => "sales_invoice",
            Self::PurchaseBill => "purchase_bill",
            Self::Receipt => "receipt",
            Self::CreditNote => "credit_note",
            Self::BookEntry => "book_entry",
        }
    }

    /// Parses an exact stored code.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.code() == code)
    }

    /// The sign a magnitude of this kind takes as the bank sees it, when a profile declares
    /// amounts by kind: `1` money in, `-1` money out, `None` when the kind has no fixed
    /// direction (a book entry is either, so its amount must be signed in the file).
    pub const fn sign(self) -> Option<i8> {
        match self {
            Self::SalesInvoice | Self::Receipt => Some(1),
            Self::PurchaseBill | Self::CreditNote => Some(-1),
            Self::BookEntry => None,
        }
    }

    /// True when a document of this kind must carry a number.
    pub const fn requires_number(self) -> bool {
        matches!(
            self,
            Self::SalesInvoice | Self::PurchaseBill | Self::CreditNote
        )
    }
}

/// The fields of a document that identify it.
#[derive(Debug, Clone, Copy)]
pub struct DocumentTuple<'a> {
    /// The client.
    pub client: ClientId,
    /// The kind.
    pub kind: DocumentKind,
    /// The number after `normalization v1`, when the document has one.
    pub number_norm: Option<&'a str>,
    /// The issue date.
    pub issue_date: Date,
    /// The signed amount and its currency.
    pub amount: Money,
    /// The counterparty after `normalization v1`, when present.
    pub counterparty_norm: Option<&'a str>,
}

fn push_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
}

fn push_optional(hasher: &mut Sha256, text: Option<&str>) {
    match text {
        None => hasher.update([0_u8]),
        Some(t) => {
            hasher.update([1_u8]);
            push_len_prefixed(hasher, t.as_bytes());
        }
    }
}

/// The document tuple hash (`document fingerprint v1`); the fingerprint adds the occurrence
/// index exactly as for lines ([`crate::fingerprint::fingerprint`]).
pub fn document_tuple_hash(tuple: &DocumentTuple<'_>) -> TupleHash {
    let mut h = Sha256::new();
    h.update(DOCUMENT_TUPLE_DOMAIN);
    h.update(tuple.client.as_bytes());
    push_len_prefixed(&mut h, tuple.kind.code().as_bytes());
    push_optional(&mut h, tuple.number_norm);
    h.update(tuple.issue_date.to_iso().as_bytes());
    h.update(tuple.amount.minor().to_be_bytes());
    h.update(tuple.amount.currency().code().as_bytes());
    push_optional(&mut h, tuple.counterparty_norm);
    TupleHash(h.finalize().into())
}

/// One validated, normalized document row, ready to stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDocument {
    /// The record number in the file (the header is row 1).
    pub row: u32,
    /// The kind.
    pub kind: DocumentKind,
    /// The number as written, when present.
    pub number_raw: Option<String>,
    /// The number normalized, when present.
    pub number_norm: Option<String>,
    /// The issue date.
    pub issue_date: Date,
    /// The due date, when present.
    pub due_date: Option<Date>,
    /// The signed amount as the bank would see it.
    pub amount: Money,
    /// The counterparty as written, when present.
    pub counterparty_raw: Option<String>,
    /// The counterparty normalized, when present.
    pub counterparty_norm: Option<String>,
    /// A description or narration, when present.
    pub description: Option<String>,
    /// The tuple hash; the occurrence index is assigned at the flip.
    pub tuple_hash: TupleHash,
}

impl StagedRow for NewDocument {
    fn row(&self) -> u32 {
        self.row
    }

    fn heap_bytes(&self) -> usize {
        [
            self.number_raw.as_ref().map(String::len),
            self.number_norm.as_ref().map(String::len),
            self.counterparty_raw.as_ref().map(String::len),
            self.counterparty_norm.as_ref().map(String::len),
            self.description.as_ref().map(String::len),
        ]
        .into_iter()
        .flatten()
        .fold(192_usize, usize::saturating_add)
    }
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

    fn tuple<'a>(number: Option<&'a str>, counterparty: Option<&'a str>) -> DocumentTuple<'a> {
        DocumentTuple {
            client: ClientId::from_uuid(Uuid::from_u128(5)),
            kind: DocumentKind::SalesInvoice,
            number_norm: number,
            issue_date: Date::new(2026, 3, 1).unwrap(),
            amount: Money::new(12_500, Currency::Inr).unwrap(),
            counterparty_norm: counterparty,
        }
    }

    #[test]
    fn kinds_round_trip_and_carry_their_sign() {
        for kind in DocumentKind::ALL {
            assert_eq!(DocumentKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(DocumentKind::from_code("invoice"), None);
        assert_eq!(DocumentKind::SalesInvoice.sign(), Some(1));
        assert_eq!(DocumentKind::PurchaseBill.sign(), Some(-1));
        assert_eq!(DocumentKind::BookEntry.sign(), None);
        assert!(DocumentKind::CreditNote.requires_number());
        assert!(!DocumentKind::Receipt.requires_number());
    }

    #[test]
    fn the_document_tuple_separates_its_fields_and_is_pinned() {
        let a = document_tuple_hash(&tuple(Some("INV1"), Some("ACME")));
        assert_ne!(a, document_tuple_hash(&tuple(Some("INV1ACME"), None)));
        assert_ne!(a, document_tuple_hash(&tuple(None, Some("INV1ACME"))));
        assert_ne!(
            document_tuple_hash(&tuple(Some(""), None)),
            document_tuple_hash(&tuple(None, None))
        );
        let mut bill = tuple(Some("INV1"), Some("ACME"));
        bill.kind = DocumentKind::PurchaseBill;
        assert_ne!(a, document_tuple_hash(&bill));
        // Computed independently (Python hashlib over the documented byte layout).
        assert_eq!(
            hex::encode(a.0),
            "0aaf3ad3fc1149b76ff9b9eab6b1d9fb23d3afa22923dd2e2821065fa37c96a0"
        );
    }
}
