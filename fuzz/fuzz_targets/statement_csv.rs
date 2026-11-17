//! The built-in `r0-generic-csv` statement parser (with its header, so rows are reached).
#![no_main]

use libfuzzer_sys::fuzz_target;
use recon_domain::{AccountId, Currency};
use recon_import::{AccountContext, StatementParser};

#[path = "common.rs"]
mod common;

const HEAD: &[u8] =
    b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";

fn run(input: &[u8], chunk: usize) -> String {
    let mut parser = StatementParser::new(AccountContext {
        account: AccountId::from_uuid(uuid::Uuid::from_u128(1)),
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
            assert!(lines.len() <= s.rows_read as usize);
            format!("{s:?} {lines:?}")
        }
        Err(e) => format!("refused {e:?} after {}", lines.len()),
    }
}

fuzz_target!(|data: &[u8]| {
    let (chunk, rest) = common::split(data);
    let input = [HEAD, rest].concat();
    assert_eq!(run(&input, input.len().max(1)), run(&input, chunk));
});
