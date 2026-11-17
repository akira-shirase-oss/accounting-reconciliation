//! The OFX 1.x and 2.x parser: entities are never expanded, every outcome is a line or a
//! named refusal, and chunking is invisible.
#![no_main]

use libfuzzer_sys::fuzz_target;
use recon_domain::{AccountId, Currency};
use recon_import::{AccountContext, OfxParser};

#[path = "common.rs"]
mod common;

fn run(input: &[u8], chunk: usize) -> String {
    let mut parser = OfxParser::new(AccountContext {
        account: AccountId::from_uuid(uuid::Uuid::from_u128(3)),
        currency: Currency::Usd,
    });
    let mut lines = Vec::new();
    for part in input.chunks(chunk) {
        if let Err(e) = parser.push(part, &mut |l| lines.push(l)) {
            return format!("refused {e:?} after {}", lines.len());
        }
    }
    match parser.finish(&mut |l| lines.push(l)) {
        Ok(s) => {
            assert!(s.rows_refused <= s.rows_read);
            format!("{s:?} {lines:?}")
        }
        Err(e) => format!("refused {e:?} after {}", lines.len()),
    }
}

fuzz_target!(|data: &[u8]| {
    let (chunk, input) = common::split(data);
    assert_eq!(run(input, input.len().max(1)), run(input, chunk));
});
