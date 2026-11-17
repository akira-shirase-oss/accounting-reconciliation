//! Shared by every target: the chunk size comes from the first byte, so libFuzzer explores
//! chunk boundaries as well as content.

/// Splits `data` into (chunk size 1..=64, the input).
pub fn split(data: &[u8]) -> (usize, &[u8]) {
    match data.split_first() {
        Some((first, rest)) => (usize::from(*first % 64) + 1, rest),
        None => (1, data),
    }
}
