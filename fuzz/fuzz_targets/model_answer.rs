//! A model's answer is external input (D-016): any bytes either parse into the
//! answer schema or are refused, and a parsed answer against a fixed request is
//! either validated or refused with a named code — never a panic, never a hang.
#![no_main]

use libfuzzer_sys::fuzz_target;
use recon_domain::document::DocumentKind;
use recon_domain::{Currency, Date, DocumentId, LineId, Money};
use recon_model::{Candidate, Data, LineFacts, ProposeRequest, validate};
use uuid::Uuid;

fn request() -> ProposeRequest {
    ProposeRequest {
        prompt_version: "fuzz".to_owned(),
        line: LineFacts {
            id: LineId::from_uuid(Uuid::from_u128(1)),
            booking_date: Date::new(2026, 4, 2).unwrap(),
            amount: Money::new(-4215, Currency::Usd).unwrap(),
            narration: Data("n".to_owned()),
            counterparty: None,
        },
        candidates: vec![Candidate {
            id: DocumentId::from_uuid(Uuid::from_u128(2)),
            amount: Money::new(4215, Currency::Usd).unwrap(),
            open: Money::new(4215, Currency::Usd).unwrap(),
            date: Date::new(2026, 4, 1).unwrap(),
            kind: DocumentKind::SalesInvoice,
            reference: Data("INV-1".to_owned()),
            counterparty: None,
            excerpt: None,
        }],
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(answer) = recon_model::Answer::parse(text) {
        let _ = validate(&request(), &answer);
    }
});
