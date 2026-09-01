//! Confusables-aware comparison between a user-supplied role name and a
//! canonical ASCII built-in name.
//!
//! ## Why this exists
//!
//! A bare `eq_ignore_ascii_case` check lets a caller bypass the built-in
//! roster by substituting non-ASCII characters that look identical to a
//! Latin letter — e.g. `"Οwner"` with Greek capital Omicron (U+039F)
//! instead of Latin O (U+004F). [`fold_to_ascii_skeleton`] maps the most
//! common Greek and Cyrillic visual lookalikes to their Latin
//! counterpart; [`name_collides_with_builtin`] composes the fold with a
//! case-insensitive ASCII compare.
//!
//! This is a conservative subset of UTS #39 — only the codepoints likely
//! to appear in an impersonation attempt against the built-in roster.
//! For full UTS #39 coverage, swap the body of `fold_to_ascii_skeleton`
//! for a call to `unicode_security::skeleton`.

/// Map a single character to its visual ASCII counterpart, if any.
/// Returns `None` when there is no Latin lookalike.
#[allow(clippy::match_same_arms)]
fn fold_char_to_ascii(ch: char) -> Option<char> {
    match ch {
        // Greek uppercase letters that visually match a Latin uppercase letter.
        '\u{0391}' => Some('A'), // Α GREEK CAPITAL LETTER ALPHA
        '\u{0392}' => Some('B'), // Β GREEK CAPITAL LETTER BETA
        '\u{0395}' => Some('E'), // Ε GREEK CAPITAL LETTER EPSILON
        '\u{0396}' => Some('Z'), // Ζ GREEK CAPITAL LETTER ZETA
        '\u{0397}' => Some('H'), // Η GREEK CAPITAL LETTER ETA
        '\u{0399}' => Some('I'), // Ι GREEK CAPITAL LETTER IOTA
        '\u{039A}' => Some('K'), // Κ GREEK CAPITAL LETTER KAPPA
        '\u{039C}' => Some('M'), // Μ GREEK CAPITAL LETTER MU
        '\u{039D}' => Some('N'), // Ν GREEK CAPITAL LETTER NU
        '\u{039F}' => Some('O'), // Ο GREEK CAPITAL LETTER OMICRON
        '\u{03A1}' => Some('P'), // Ρ GREEK CAPITAL LETTER RHO
        '\u{03A4}' => Some('T'), // Τ GREEK CAPITAL LETTER TAU
        '\u{03A5}' => Some('Y'), // Υ GREEK CAPITAL LETTER UPSILON
        '\u{03A7}' => Some('X'), // Χ GREEK CAPITAL LETTER CHI
        // Greek lowercase letters that visually match a Latin lowercase letter.
        '\u{03BF}' => Some('o'), // ο GREEK SMALL LETTER OMICRON
        '\u{03C1}' => Some('p'), // ρ GREEK SMALL LETTER RHO
        '\u{03C5}' => Some('u'), // υ (loose; less reliable)
        '\u{03C7}' => Some('x'), // χ GREEK SMALL LETTER CHI
        // Cyrillic uppercase letters that visually match a Latin uppercase letter.
        '\u{0410}' => Some('A'), // А CYRILLIC CAPITAL LETTER A
        '\u{0412}' => Some('B'), // В CYRILLIC CAPITAL LETTER VE
        '\u{0415}' => Some('E'), // Е CYRILLIC CAPITAL LETTER IE
        '\u{041A}' => Some('K'), // К CYRILLIC CAPITAL LETTER KA
        '\u{041C}' => Some('M'), // М CYRILLIC CAPITAL LETTER EM
        '\u{041D}' => Some('H'), // Н CYRILLIC CAPITAL LETTER EN
        '\u{041E}' => Some('O'), // О CYRILLIC CAPITAL LETTER O
        '\u{0420}' => Some('P'), // Р CYRILLIC CAPITAL LETTER ER
        '\u{0421}' => Some('C'), // С CYRILLIC CAPITAL LETTER ES
        '\u{0422}' => Some('T'), // Т CYRILLIC CAPITAL LETTER TE
        '\u{0425}' => Some('X'), // Х CYRILLIC CAPITAL LETTER HA
        // Cyrillic lowercase letters that visually match a Latin lowercase letter.
        '\u{0430}' => Some('a'), // а CYRILLIC SMALL LETTER A
        '\u{0435}' => Some('e'), // е CYRILLIC SMALL LETTER IE
        '\u{043E}' => Some('o'), // о CYRILLIC SMALL LETTER O
        '\u{0440}' => Some('p'), // р CYRILLIC SMALL LETTER ER
        '\u{0441}' => Some('c'), // с CYRILLIC SMALL LETTER ES
        '\u{0445}' => Some('x'), // х CYRILLIC SMALL LETTER HA
        _ => None,
    }
}

/// Fold `input` to its ASCII visual skeleton in two passes:
///
/// 1. **NFKC normalisation** (`unicode_normalization::UnicodeNormalization::nfkc`).
///    Collapses compatibility-decomposed lookalikes — math-bold
///    `\u{1D40E}` MATHEMATICAL BOLD CAPITAL O, fullwidth `\u{FF2F}`
///    LATIN CAPITAL LETTER O, etc. — onto plain ASCII so the
///    per-codepoint fold table below sees normalised input. Without
///    this pass the fold table never matches math-bold variants and
///    `name_collides_with_builtin` silently lets `"𝐎wner"` through.
/// 2. **Greek / Cyrillic single-codepoint fold.** [`fold_char_to_ascii`]
///    handles the residual non-decomposable lookalikes (Greek capital
///    Omicron, Cyrillic capital O, etc.) that NFKC does not touch.
///
/// Still a conservative subset of UTS #39 — for full coverage swap
/// both passes for a call to `unicode_security::skeleton`.
pub fn fold_to_ascii_skeleton(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    input
        .nfkc()
        .map(|ch| fold_char_to_ascii(ch).unwrap_or(ch))
        .collect()
}

/// `true` iff `input`'s ASCII visual skeleton collides with `builtin`
/// (case-insensitively). `builtin` is expected to be pure ASCII — the
/// canonical built-in role names are.
pub fn name_collides_with_builtin(input: &str, builtin: &str) -> bool {
    fold_to_ascii_skeleton(input).eq_ignore_ascii_case(builtin)
}

#[cfg(test)]
#[path = "name_confusables_tests.rs"]
mod name_confusables_tests;
