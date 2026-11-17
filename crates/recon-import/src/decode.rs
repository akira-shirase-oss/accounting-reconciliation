//! File bytes to UTF-8 text, streamed (D-010, D-062). Accepted: UTF-8 (a byte-order mark
//! allowed), UTF-16 in either byte order when a byte-order mark says so, and Windows-1252
//! only when the profile declares it. A UTF-32 mark, or a mark that contradicts the
//! declared encoding, is `unsupported_encoding`. The size bound counts the file's own
//! bytes, before decoding.
//!
//! Decoding never guesses and never substitutes: an unpaired UTF-16 surrogate, an odd final
//! byte, or one of the five bytes Windows-1252 leaves undefined reaches the reader as the
//! byte `0xFF`, which is never valid UTF-8, so its record is refused as `invalid_utf8` with
//! its row number instead of being silently replaced. Memory is one 16 KiB block of output
//! (UTF-8 passes through without a copy).

use recon_domain::refusal::RefusalCode;
use serde::{Deserialize, Serialize};

use crate::csv::FileRefusal;

/// The encoding a profile declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextEncoding {
    /// UTF-8; a UTF-16 file is read when its byte-order mark says so.
    #[default]
    Utf8,
    /// Windows-1252 (no byte-order mark may appear).
    Windows1252,
}

/// What the first bytes decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Undecided,
    Utf8,
    Utf16 { big_endian: bool },
    Windows1252,
}

/// A byte no UTF-8 decoder accepts: marks undecodable input for the reader to refuse.
const INVALID: u8 = 0xFF;
/// Input decoded per block.
const BLOCK: usize = 16 * 1024;

/// Windows-1252's 0x80–0x9F, as Unicode scalar values (0 for the five undefined bytes).
const CP1252_HIGH: [u32; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0, 0x017D, 0, 0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC,
    0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

/// The streaming decoder.
#[derive(Debug)]
pub struct Decoder {
    declared: TextEncoding,
    mode: Mode,
    /// Up to four leading bytes held until the byte-order mark is decided.
    head: Vec<u8>,
    /// A UTF-16 byte waiting for its pair.
    odd: Option<u8>,
    /// A UTF-16 high surrogate waiting for its low half.
    high: Option<u16>,
    raw_bytes: u64,
    max_raw_bytes: u64,
    out: Vec<u8>,
}

impl Decoder {
    /// A decoder for `declared`, refusing files over `max_raw_bytes`.
    pub fn new(declared: TextEncoding, max_raw_bytes: u64) -> Self {
        Self {
            declared,
            mode: Mode::Undecided,
            head: Vec::with_capacity(4),
            odd: None,
            high: None,
            raw_bytes: 0,
            max_raw_bytes,
            out: Vec::new(),
        }
    }

    /// The file's bytes read so far.
    pub const fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }

    /// Decodes `chunk`, handing UTF-8 text to `sink` in pieces of any size.
    pub fn push<F>(&mut self, chunk: &[u8], sink: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(&[u8]) -> Result<(), FileRefusal>,
    {
        let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        self.raw_bytes = self.raw_bytes.saturating_add(len);
        if self.raw_bytes > self.max_raw_bytes {
            return Err(FileRefusal::new(RefusalCode::FileTooLarge, None));
        }
        let mut rest = chunk;
        if self.mode == Mode::Undecided {
            let take = rest.len().min(4_usize.saturating_sub(self.head.len()));
            let (head, tail) = rest.split_at(take);
            self.head.extend_from_slice(head);
            rest = tail;
            if self.head.len() < 4 {
                return Ok(());
            }
            self.decide_and_flush_head(sink)?;
        }
        self.decode(rest, sink)
    }

    /// Ends the file: decides a mark on a file shorter than four bytes, and refuses a
    /// dangling UTF-16 half through the reader.
    pub fn finish<F>(&mut self, sink: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(&[u8]) -> Result<(), FileRefusal>,
    {
        if self.mode == Mode::Undecided {
            self.decide_and_flush_head(sink)?;
        }
        if self.odd.take().is_some() || self.high.take().is_some() {
            sink(&[INVALID])?;
        }
        Ok(())
    }

    fn decide_and_flush_head<F>(&mut self, sink: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(&[u8]) -> Result<(), FileRefusal>,
    {
        let head = std::mem::take(&mut self.head);
        let unsupported = || Err(FileRefusal::new(RefusalCode::UnsupportedEncoding, None));
        let (mode, skip) = if head.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
            || head.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
        {
            return unsupported();
        } else if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
            (Mode::Utf8, 3)
        } else if head.starts_with(&[0xFF, 0xFE]) {
            (Mode::Utf16 { big_endian: false }, 2)
        } else if head.starts_with(&[0xFE, 0xFF]) {
            (Mode::Utf16 { big_endian: true }, 2)
        } else {
            let mode = match self.declared {
                TextEncoding::Utf8 => Mode::Utf8,
                TextEncoding::Windows1252 => Mode::Windows1252,
            };
            (mode, 0)
        };
        if skip > 0 && self.declared == TextEncoding::Windows1252 {
            // A byte-order mark contradicts a declared single-byte encoding.
            return unsupported();
        }
        self.mode = mode;
        let body = head.get(skip..).unwrap_or_default().to_vec();
        self.decode(&body, sink)
    }

    fn decode<F>(&mut self, bytes: &[u8], sink: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(&[u8]) -> Result<(), FileRefusal>,
    {
        if bytes.is_empty() {
            return Ok(());
        }
        match self.mode {
            Mode::Undecided | Mode::Utf8 => sink(bytes),
            Mode::Windows1252 => {
                for block in bytes.chunks(BLOCK) {
                    self.out.clear();
                    for &b in block {
                        push_cp1252(&mut self.out, b);
                    }
                    sink(&self.out)?;
                }
                Ok(())
            }
            Mode::Utf16 { big_endian } => {
                for block in bytes.chunks(BLOCK) {
                    self.out.clear();
                    for &b in block {
                        let Some(first) = self.odd.take() else {
                            self.odd = Some(b);
                            continue;
                        };
                        let unit = if big_endian {
                            u16::from_be_bytes([first, b])
                        } else {
                            u16::from_le_bytes([first, b])
                        };
                        self.unit(unit);
                    }
                    let out = std::mem::take(&mut self.out);
                    let result = sink(&out);
                    self.out = out;
                    result?;
                }
                Ok(())
            }
        }
    }

    fn unit(&mut self, unit: u16) {
        let is_high = (0xD800..=0xDBFF).contains(&unit);
        let is_low = (0xDC00..=0xDFFF).contains(&unit);
        if let Some(high) = self.high.take() {
            if is_low {
                let scalar = 0x1_0000_u32
                    .saturating_add((u32::from(high) & 0x3FF) << 10)
                    .saturating_add(u32::from(unit) & 0x3FF);
                push_scalar(&mut self.out, scalar);
                return;
            }
            // The high half had no low half.
            self.out.push(INVALID);
        }
        if is_high {
            self.high = Some(unit);
        } else if is_low {
            self.out.push(INVALID);
        } else {
            push_scalar(&mut self.out, u32::from(unit));
        }
    }
}

fn push_scalar(out: &mut Vec<u8>, scalar: u32) {
    match char::from_u32(scalar) {
        Some(c) => {
            let mut buf = [0_u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        None => out.push(INVALID),
    }
}

fn push_cp1252(out: &mut Vec<u8>, b: u8) {
    match b {
        0x00..=0x7F => out.push(b),
        0x80..=0x9F => {
            let scalar = CP1252_HIGH
                .get(usize::from(b.wrapping_sub(0x80)))
                .copied()
                .unwrap_or(0);
            if scalar == 0 {
                out.push(INVALID);
            } else {
                push_scalar(out, scalar);
            }
        }
        // 0xA0–0xFF are Latin-1, the same code points.
        _ => push_scalar(out, u32::from(b)),
    }
}

/// Decodes a whole buffer (tests and the OFX header).
pub fn decode_all(encoding: TextEncoding, bytes: &[u8]) -> Result<Vec<u8>, FileRefusal> {
    let mut decoder = Decoder::new(encoding, u64::MAX);
    let mut out = Vec::with_capacity(bytes.len());
    let mut sink = |piece: &[u8]| {
        out.extend_from_slice(piece);
        Ok(())
    };
    decoder.push(bytes, &mut sink)?;
    decoder.finish(&mut sink)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;
    use proptest::prelude::*;

    fn chunked(encoding: TextEncoding, bytes: &[u8], chunk: usize) -> Result<Vec<u8>, FileRefusal> {
        let mut decoder = Decoder::new(encoding, 1_000_000);
        let mut out = vec![];
        let mut sink = |piece: &[u8]| {
            out.extend_from_slice(piece);
            Ok(())
        };
        for part in bytes.chunks(chunk.max(1)) {
            decoder.push(part, &mut sink)?;
        }
        decoder.finish(&mut sink)?;
        Ok(out)
    }

    fn utf16(text: &str, big_endian: bool) -> Vec<u8> {
        let mut out = if big_endian {
            vec![0xFE, 0xFF]
        } else {
            vec![0xFF, 0xFE]
        };
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            });
        }
        out
    }

    #[test]
    fn utf8_passes_through_and_its_mark_is_dropped() {
        let enc = TextEncoding::Utf8;
        assert_eq!(
            chunked(enc, "a,é\n".as_bytes(), 1).unwrap(),
            "a,é\n".as_bytes()
        );
        assert_eq!(chunked(enc, b"\xEF\xBB\xBFa\n", 2).unwrap(), b"a\n");
        assert_eq!(chunked(enc, b"ab", 1).unwrap(), b"ab");
        assert_eq!(chunked(enc, b"", 1).unwrap(), b"");
    }

    #[test]
    fn utf16_is_read_by_its_mark_in_either_order() {
        let text = "date,amount\n2026-03-01,₹ 𝄞\n";
        for be in [false, true] {
            for chunk in [1, 3, 7, 4096] {
                assert_eq!(
                    chunked(TextEncoding::Utf8, &utf16(text, be), chunk).unwrap(),
                    text.as_bytes(),
                    "big endian {be}, chunk {chunk}"
                );
            }
        }
    }

    #[test]
    fn broken_utf16_reaches_the_reader_as_invalid_bytes() {
        // An unpaired high surrogate, a lone low surrogate, an odd final byte.
        let mut lone_high = vec![0xFF, 0xFE];
        lone_high.extend_from_slice(&0xD800_u16.to_le_bytes());
        lone_high.extend_from_slice(&u16::from(b'a').to_le_bytes());
        assert_eq!(
            chunked(TextEncoding::Utf8, &lone_high, 1).unwrap(),
            b"\xFFa"
        );
        let mut lone_low = vec![0xFF, 0xFE];
        lone_low.extend_from_slice(&0xDC00_u16.to_le_bytes());
        assert_eq!(chunked(TextEncoding::Utf8, &lone_low, 2).unwrap(), b"\xFF");
        assert_eq!(
            chunked(TextEncoding::Utf8, &[0xFF, 0xFE, b'a', 0, b'b'], 1).unwrap(),
            b"a\xFF"
        );
    }

    #[test]
    fn windows_1252_only_when_declared() {
        let bytes = b"Caf\xE9 \x80 \x93x\x94\n";
        assert_eq!(
            chunked(TextEncoding::Windows1252, bytes, 2).unwrap(),
            "Café € “x”\n".as_bytes()
        );
        // The undefined bytes are refused through the reader, never replaced.
        assert_eq!(
            chunked(TextEncoding::Windows1252, b"a\x81b", 8).unwrap(),
            b"a\xFFb"
        );
        // Undeclared, the same bytes are not UTF-8: the reader refuses them.
        assert_eq!(chunked(TextEncoding::Utf8, bytes, 3).unwrap(), bytes);
    }

    #[test]
    fn marks_that_are_not_supported_or_contradict_the_profile_are_refused() {
        let refused = Err(FileRefusal::new(RefusalCode::UnsupportedEncoding, None));
        for bytes in [
            &[0x00, 0x00, 0xFE, 0xFF, 0, 0, 0, b'a'][..],
            &[0xFF, 0xFE, 0x00, 0x00, b'a', 0, 0, 0],
        ] {
            assert_eq!(chunked(TextEncoding::Utf8, bytes, 1), refused);
        }
        for bytes in [&b"\xEF\xBB\xBFa"[..], b"\xFF\xFEa\0", b"\xFE\xFF\0a"] {
            assert_eq!(chunked(TextEncoding::Windows1252, bytes, 1), refused);
        }
        // Decided at the end for a file shorter than four bytes.
        assert_eq!(
            chunked(TextEncoding::Windows1252, b"\xEF\xBB\xBF", 1),
            refused
        );
    }

    #[test]
    fn the_size_bound_counts_the_files_own_bytes() {
        let mut decoder = Decoder::new(TextEncoding::Utf8, 10);
        let mut sink = |_: &[u8]| Ok(());
        decoder.push(b"0123456789", &mut sink).unwrap();
        assert_eq!(
            decoder.push(b"x", &mut sink),
            Err(FileRefusal::new(RefusalCode::FileTooLarge, None))
        );
        assert_eq!(decoder.raw_bytes(), 11);
    }

    proptest! {
        #[test]
        fn any_chunking_decodes_the_same(
            bytes in proptest::collection::vec(any::<u8>(), 0..300),
            chunk in 1_usize..9,
            declared in prop::sample::select(vec![TextEncoding::Utf8, TextEncoding::Windows1252]),
        ) {
            prop_assert_eq!(chunked(declared, &bytes, usize::MAX), chunked(declared, &bytes, chunk));
        }

        #[test]
        fn utf16_round_trips_any_text(text in "\\PC{0,40}", be in any::<bool>()) {
            prop_assert_eq!(chunked(TextEncoding::Utf8, &utf16(&text, be), 5).unwrap(), text.as_bytes());
        }
    }
}
