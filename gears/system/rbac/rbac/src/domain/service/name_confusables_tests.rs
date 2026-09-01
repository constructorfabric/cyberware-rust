//! Unit tests for the confusables fold + collision helper.

#![allow(clippy::expect_used, clippy::panic)]

use super::{fold_to_ascii_skeleton, name_collides_with_builtin};

#[test]
fn fold_passes_pure_ascii_unchanged() {
    assert_eq!(fold_to_ascii_skeleton("Owner"), "Owner");
    assert_eq!(fold_to_ascii_skeleton("owner-2024"), "owner-2024");
}

#[test]
fn fold_replaces_greek_capital_omicron_with_latin_o() {
    // "Οwner" — Greek capital Omicron at position 0.
    let greek_owner = "\u{039F}wner";
    assert_eq!(fold_to_ascii_skeleton(greek_owner), "Owner");
}

#[test]
fn fold_replaces_cyrillic_lookalikes() {
    // "Оwner" — Cyrillic capital O (U+041E) at position 0.
    let cyrillic_owner = "\u{041E}wner";
    assert_eq!(fold_to_ascii_skeleton(cyrillic_owner), "Owner");

    // "Reаder" — Cyrillic small a (U+0430) at position 2.
    let mixed = "Re\u{0430}der";
    assert_eq!(fold_to_ascii_skeleton(mixed), "Reader");
}

#[test]
fn fold_leaves_non_lookalike_characters_alone() {
    // Japanese hiragana — not a Latin lookalike, kept verbatim.
    assert_eq!(fold_to_ascii_skeleton("\u{304a}"), "\u{304a}");
    // Emoji.
    assert_eq!(fold_to_ascii_skeleton("Owner\u{1f980}"), "Owner\u{1f980}");
}

#[test]
fn collide_with_builtin_catches_greek_lookalike() {
    let greek_owner = "\u{039F}wner";
    assert!(
        name_collides_with_builtin(greek_owner, "Owner"),
        "Greek-Omicron '\u{39f}wner' MUST collide with built-in 'Owner'"
    );
}

#[test]
fn collide_with_builtin_catches_cyrillic_lookalike() {
    let cyrillic_owner = "\u{041E}wner";
    assert!(
        name_collides_with_builtin(cyrillic_owner, "Owner"),
        "Cyrillic-O '\u{41e}wner' MUST collide with built-in 'Owner'"
    );
}

#[test]
fn collide_with_builtin_catches_case_variants() {
    assert!(name_collides_with_builtin("OWNER", "Owner"));
    assert!(name_collides_with_builtin("owner", "Owner"));
    assert!(name_collides_with_builtin("oWnEr", "Owner"));
}

#[test]
fn collide_with_builtin_does_not_false_positive_on_distinct_name() {
    assert!(!name_collides_with_builtin("Auditor", "Owner"));
    assert!(!name_collides_with_builtin("Owner-2", "Owner"));
    assert!(!name_collides_with_builtin("Owners", "Owner"));
}

/// Data-driven check that every codepoint the fold table claims to
/// recognise still folds correctly. A dropped arm or renamed constant
/// would silently shrink the effective confusables set; this test
/// catches that. Each row is `(input_codepoint, expected_ascii)`.
#[test]
fn fold_table_recognises_every_known_lookalike_codepoint() {
    let cases: &[(char, char)] = &[
        // Greek uppercase
        ('\u{0391}', 'A'),
        ('\u{0392}', 'B'),
        ('\u{0395}', 'E'),
        ('\u{0396}', 'Z'),
        ('\u{0397}', 'H'),
        ('\u{0399}', 'I'),
        ('\u{039A}', 'K'),
        ('\u{039C}', 'M'),
        ('\u{039D}', 'N'),
        ('\u{039F}', 'O'),
        ('\u{03A1}', 'P'),
        ('\u{03A4}', 'T'),
        ('\u{03A5}', 'Y'),
        ('\u{03A7}', 'X'),
        // Greek lowercase
        ('\u{03BF}', 'o'),
        ('\u{03C1}', 'p'),
        ('\u{03C5}', 'u'),
        ('\u{03C7}', 'x'),
        // Cyrillic uppercase
        ('\u{0410}', 'A'),
        ('\u{0412}', 'B'),
        ('\u{0415}', 'E'),
        ('\u{041A}', 'K'),
        ('\u{041C}', 'M'),
        ('\u{041D}', 'H'),
        ('\u{041E}', 'O'),
        ('\u{0420}', 'P'),
        ('\u{0421}', 'C'),
        ('\u{0422}', 'T'),
        ('\u{0425}', 'X'),
        // Cyrillic lowercase
        ('\u{0430}', 'a'),
        ('\u{0435}', 'e'),
        ('\u{043E}', 'o'),
        ('\u{0440}', 'p'),
        ('\u{0441}', 'c'),
        ('\u{0445}', 'x'),
    ];

    for &(input, expected) in cases {
        let folded = fold_to_ascii_skeleton(&input.to_string());
        assert_eq!(
            folded,
            expected.to_string(),
            "codepoint U+{:04X} should fold to ASCII '{}', got '{}'",
            u32::from(input),
            expected,
            folded,
        );
    }
}

/// Characters that are NOT in the fold table pass through unchanged.
/// Picks plausible "false positive" cases to guard against an
/// over-eager fold.
#[test]
fn fold_table_leaves_unrelated_characters_unchanged() {
    let cases = [
        '\u{00E9}',  // é — Latin-1 supplement, not in the table
        '\u{0394}',  // Δ — Greek Delta, no Latin lookalike
        '\u{0411}',  // Б — Cyrillic Be, no Latin lookalike
        '\u{4E2D}',  // 中 — CJK ideograph
        '\u{1F600}', // 😀 — emoji
        '_',         // ASCII punctuation
        '0',         // ASCII digit
    ];

    for ch in cases {
        let folded = fold_to_ascii_skeleton(&ch.to_string());
        assert_eq!(
            folded,
            ch.to_string(),
            "non-lookalike codepoint U+{:04X} should pass through unchanged",
            u32::from(ch),
        );
    }
}

// ---------------------------------------------------------------------------
// NFKC normalisation pass closes the compatibility-decomposition
// bypass that the single-codepoint fold table alone could not cover.
// ---------------------------------------------------------------------------

#[test]
fn nfkc_folds_math_bold_capital_o_to_ascii() {
    // "𝐎wner" — MATHEMATICAL BOLD CAPITAL O (U+1D40E) at position 0. NFKC
    // decomposes it to plain ASCII 'O', so it must not slip past the fold
    // table and reach storage as-is.
    let math_bold_owner = "\u{1D40E}wner";
    assert_eq!(fold_to_ascii_skeleton(math_bold_owner), "Owner");
    assert!(
        name_collides_with_builtin(math_bold_owner, "Owner"),
        "math-bold 'O' lookalike MUST collide with the built-in Owner role"
    );
}

#[test]
fn nfkc_folds_fullwidth_capital_o_to_ascii() {
    // "Ｏwner" — FULLWIDTH LATIN CAPITAL LETTER O (U+FF2F) at position 0.
    let fullwidth_owner = "\u{FF2F}wner";
    assert_eq!(fold_to_ascii_skeleton(fullwidth_owner), "Owner");
    assert!(name_collides_with_builtin(fullwidth_owner, "Owner"));
}

#[test]
fn nfkc_pass_composes_then_greek_fold_still_applies() {
    // Combined: NFKC normalises the fullwidth 'A', and the Greek fold
    // handles the capital Omicron — both passes compose correctly.
    // "Ａdmіn" is not a built-in, but the fold should still normalise
    // the fullwidth A to ASCII 'A'.
    let mixed = "\u{FF21}dmin"; // FULLWIDTH LATIN CAPITAL LETTER A
    assert_eq!(fold_to_ascii_skeleton(mixed), "Admin");
}
