//! `LIKE` wildcard escaping for caller-supplied substring filters.
//!
//! `SeaORM`'s `ColumnTrait::contains` does not escape LIKE metacharacters
//! (`%`, `_`, `\`), so `name_contains=admin_%` would silently widen the
//! match. This module produces patterns that match the literal caller
//! input regardless of any metacharacters it contains.
//!
//! Two-step contract:
//! 1. Build the escaped pattern with [`escape_like_literal`] or
//!    [`substring_like_pattern`] — every `%`, `_`, and `\` in the input is
//!    prefixed with `\`.
//! 2. Wrap the pattern with [`escaped_like`] when calling
//!    `Column::like(...)` — this appends `ESCAPE '\'` to the rendered SQL.
//!
//! Why step 2 matters: `Postgres`'s `LIKE` default escape character is `\`
//! (independent of `standard_conforming_strings`, which governs *string
//! literal* parsing, not `LIKE` semantics), so step 2 is a no-op on `PG`.
//! `SQLite` has **no** default `LIKE` escape character — without an
//! explicit `ESCAPE '\'` clause, the `\` we emit is treated as a literal
//! and the `%` / `_` after it stays a wildcard. Skipping step 2 silently
//! breaks `SQLite`.
//!
//! TODO(toolkit-db-like-escape): `external/gears-rust/libs/toolkit-db/
//! src/odata/core.rs` has the same shape (`like_escape` helper + bare
//! `.like(...)`) and inherits the same `SQLite` bug. Fix in a separate PR;
//! it affects every `OData` consumer, not just RBAC.

use sea_orm::sea_query::LikeExpr;

/// Escape `%`, `_`, and `\` in `needle` so it can be embedded inside
/// a `LIKE` pattern without those characters acting as wildcards.
///
/// Order matters: escape `\` first so the subsequent `\%` / `\_` we
/// emit are not themselves re-escaped.
///
/// The returned string MUST be paired with [`escaped_like`] at the
/// `.like(...)` call site — step 2 of the two-step contract above.
#[must_use]
pub fn escape_like_literal(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() + 4);
    for ch in needle.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

/// Build a substring-match `LIKE` pattern that treats `needle`
/// literally (any `%` / `_` in `needle` are escaped). The returned
/// string MUST be wrapped with [`escaped_like`] before being passed to
/// `Column::like(...)`. The `scope_prefix_condition` helper is the
/// remaining consumer of the descendant-`LIKE` shape; substring name
/// matching goes through `toolkit_odata::contains(...)`.
#[allow(dead_code)]
#[must_use]
pub fn substring_like_pattern(needle: &str) -> String {
    format!("%{}%", escape_like_literal(needle))
}

/// Wrap a pre-escaped `LIKE` pattern with an explicit `ESCAPE '\'`
/// clause. Required for correctness on `SQLite` (which has no default
/// escape character) and a no-op on `Postgres` (whose default already is
/// `\`). Use for every `Column::like(...)` call that consumes the
/// output of [`escape_like_literal`] or [`substring_like_pattern`].
#[must_use]
pub fn escaped_like(pattern: String) -> LikeExpr {
    LikeExpr::new(pattern).escape('\\')
}

#[cfg(test)]
mod tests {
    use super::{escape_like_literal, substring_like_pattern};

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(escape_like_literal("admin"), "admin");
    }

    #[test]
    fn percent_is_escaped() {
        assert_eq!(escape_like_literal("admin_%"), r"admin\_\%");
    }

    #[test]
    fn underscore_is_escaped() {
        assert_eq!(escape_like_literal("a_b"), r"a\_b");
    }

    #[test]
    fn backslash_is_escaped_first() {
        assert_eq!(escape_like_literal(r"a\b"), r"a\\b");
    }

    #[test]
    fn substring_pattern_wraps_with_percent() {
        assert_eq!(substring_like_pattern("admin_%"), r"%admin\_\%%");
    }

    #[test]
    fn empty_needle_yields_match_everything_pattern() {
        // Empty needle → `LIKE '%%'`, which matches every row — the same
        // semantics as `.contains("")`.
        assert_eq!(substring_like_pattern(""), "%%");
    }
}
