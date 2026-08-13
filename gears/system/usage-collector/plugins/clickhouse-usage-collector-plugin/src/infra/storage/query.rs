//! Injection-safe `OData` → `ClickHouse` SQL translation foundation.
//!
//! Pure (no DB) logic that turns a validated `toolkit_odata` filter AST into a
//! parameterised `ClickHouse` `WHERE` fragment plus an ordered list of binds.
//! Every SQL identifier is drawn from a closed allowlist
//! ([`translate::record_column`] / [`translate::usage_type_column`]); every
//! value is bound (`?`), never interpolated.

pub mod aggregate;
pub mod bind;
pub mod keyset;
pub mod translate;

/// Hard upper bound on the page size either list path will request from
/// `ClickHouse` in a single `fetch_all`, regardless of the caller's `$top`.
///
/// Defense-in-depth backstop: the usage-collector core gateway already rejects
/// `$top > 1000` with `400 InvalidArgument`. The value is kept in lock-step with
/// that cap so this clamp is never reached in normal operation.
pub const MAX_PAGE_SIZE: u64 = 1_000;

/// Default page size when the caller omits `$top`.
pub const DEFAULT_PAGE_SIZE: u64 = 100;

/// Resolve the effective `LIMIT` for a list query: the caller's `$top`
/// (`requested`) when present, else `default_page_size`, clamped to
/// `[1, MAX_PAGE_SIZE]`.
///
/// The lower bound of 1 prevents a `$top=0` from driving `LIMIT 0+1 = 1`
/// then `truncate(0)` — which would `None`-fail `rows.last()` on a non-empty
/// table and 500 the list path.
#[must_use]
pub fn effective_page_size(requested: Option<u64>, default_page_size: u64) -> u64 {
    requested
        .unwrap_or(default_page_size)
        .clamp(1, MAX_PAGE_SIZE)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "query_tests.rs"]
mod query_tests;
