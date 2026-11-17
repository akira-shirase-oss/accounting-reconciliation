//! The built-in document profile.
#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use recon_domain::ClientId;
use recon_import::{CompiledDocuments, CsvLimits, DocumentContext, DocumentParser};

#[path = "common.rs"]
mod common;

const HEAD: &[u8] = b"kind,number,issue_date,due_date,amount,currency,counterparty,description\n";

fn run(input: &[u8], chunk: usize) -> String {
    let mut parser = DocumentParser::with_profile(
        DocumentContext {
            client: ClientId::from_uuid(uuid::Uuid::from_u128(4)),
        },
        Arc::new(CompiledDocuments::builtin()),
        CsvLimits::default(),
    );
    let mut docs = Vec::new();
    for part in input.chunks(chunk) {
        if let Err(e) = parser.push(part, &mut |d| docs.push(d)) {
            return format!("refused {e:?} after {}", docs.len());
        }
    }
    match parser.finish(&mut |d| docs.push(d)) {
        Ok(s) => format!("{s:?} {docs:?}"),
        Err(e) => format!("refused {e:?} after {}", docs.len()),
    }
}

fuzz_target!(|data: &[u8]| {
    let (chunk, rest) = common::split(data);
    let input = [HEAD, rest].concat();
    assert_eq!(run(&input, input.len().max(1)), run(&input, chunk));
});
