//! The decoder: any chunking decodes the same, and UTF-8 passes through unchanged after a
//! leading mark.
#![no_main]

use libfuzzer_sys::fuzz_target;
use recon_import::FileRefusal;
use recon_import::decode::{Decoder, TextEncoding};

#[path = "common.rs"]
mod common;

fn run(encoding: TextEncoding, input: &[u8], chunk: usize) -> Result<Vec<u8>, FileRefusal> {
    let mut decoder = Decoder::new(encoding, 1 << 20);
    let mut out = Vec::new();
    let mut sink = |piece: &[u8]| {
        out.extend_from_slice(piece);
        Ok(())
    };
    for part in input.chunks(chunk) {
        decoder.push(part, &mut sink)?;
    }
    decoder.finish(&mut sink)?;
    Ok(out)
}

fuzz_target!(|data: &[u8]| {
    let (chunk, input) = common::split(data);
    for encoding in [TextEncoding::Utf8, TextEncoding::Windows1252] {
        let whole = run(encoding, input, input.len().max(1));
        assert_eq!(whole, run(encoding, input, chunk));
        if encoding == TextEncoding::Utf8
            && let Ok(out) = &whole
            && !input.starts_with(&[0xFF, 0xFE])
            && !input.starts_with(&[0xFE, 0xFF])
        {
            let body = input.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(input);
            assert_eq!(out.as_slice(), body);
        }
    }
});
