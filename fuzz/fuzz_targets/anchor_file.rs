//! An anchor file is external input (the operator's `--anchors FILE`): each line
//! either parses into an `Anchor` or is refused, and a parsed anchor verifies or
//! is refused by name — never a panic on any bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use recon_domain::anchor::Anchor;

const KEY: [u8; 32] = [7u8; 32];

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    for line in text.lines() {
        if let Ok(anchor) = serde_json::from_str::<Anchor>(line) {
            let _ = anchor.verify(&KEY);
        }
    }
});
