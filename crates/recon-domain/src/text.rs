//! Text from files is data (invariant 7). This module decides which characters a field may
//! hold at all and how narrations and counterparties are normalized for matching and
//! fingerprints (`normalization v1`).
//!
//! Normalization is deliberately small and independent of Unicode table versions: a
//! toolchain upgrade must never change a fingerprint. It collapses runs of space and the
//! no-break spaces into one space, trims, and upper-cases ASCII letters only.

/// The normalization version recorded with fingerprints.
pub const NORMALIZATION_VERSION: u8 = 1;

/// Characters refused in every text field: Unicode control characters (category Cc,
/// which includes NUL, tab, CR and LF); the byte-order mark; the bidirectional embedding,
/// override and isolate controls and the bidi marks (LRM, RLM, ALM) — a narration that
/// reorders itself on screen is an attack on the reviewer, not data; and the invisible
/// format characters (zero-width spaces and joiners, the soft hyphen, the Mongolian vowel
/// separator, word joiner and invisible operators, the deprecated format controls) and the
/// line and paragraph separators, which would make visually identical narrations different
/// lines (D-054).
pub const fn is_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
        )
}

const fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\u{00A0}' | '\u{2007}' | '\u{202F}')
}

/// Normalizes a narration or counterparty (`normalization v1`).
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if is_space(c) {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_collapses_trims_and_uppercases_ascii_only() {
        assert_eq!(
            normalize("  neft  Acme\u{00A0}\u{00A0}Ltd  "),
            "NEFT ACME LTD"
        );
        assert_eq!(normalize("café Ä"), "CAFé Ä");
        assert_eq!(normalize("   "), "");
        assert_eq!(normalize("a"), "A");
    }

    /// L-4 regression: invisible characters made visually identical narrations different
    /// lines, and bidi marks still reordered text on screen.
    #[test]
    fn zero_widths_bidi_marks_and_separators_are_forbidden() {
        for c in [
            '\u{00AD}', '\u{061C}', '\u{180E}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}',
            '\u{200F}', '\u{2028}', '\u{2029}', '\u{2060}', '\u{2064}', '\u{206A}', '\u{206F}',
        ] {
            assert!(is_forbidden(c), "U+{:04X}", u32::from(c));
        }
    }

    #[test]
    fn controls_boms_and_bidi_are_forbidden() {
        for c in [
            '\0', '\t', '\r', '\n', '\u{7f}', '\u{85}', '\u{FEFF}', '\u{202E}', '\u{2066}',
        ] {
            assert!(is_forbidden(c), "{c:?}");
        }
        for c in ['a', ' ', '€', '₹', '\u{00A0}', 'é', '='] {
            assert!(!is_forbidden(c), "{c:?}");
        }
    }
}
