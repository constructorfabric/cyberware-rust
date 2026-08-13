//! Order-by rendering, keyset (tuple-comparison) predicates, and cursor
//! encode/decode for keyset pagination — adapted for `ClickHouse`.
//!
//! Structurally identical to the reference plugin's `keyset.rs`; the only
//! dialect difference is the placeholder shape: `ClickHouse` uses `?`
//! (positional) rather than `$N`. The column allowlists and cursor API remain
//! identical.
//!
//! ## Verified `toolkit-odata` cursor / order API
//!
//! - `ODataOrderBy(pub Vec<OrderKey>)`; `OrderKey { field: String, dir: SortDir }`.
//! - `CursorV1 { k: Vec<String>, o: SortDir, s: String, f: Option<String>, d: String }`.
//! - `CursorV1::encode(&self) -> serde_json::Result<String>` (base64url).
//! - `CursorV1::decode(token: &str) -> Result<CursorV1, toolkit_odata::Error>`.

use std::str::FromStr;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use toolkit_odata::filter::FieldKind;
use toolkit_odata::{CursorV1, ODataOrderBy, SortDir};

use super::bind::SqlBind;
use super::translate::SqlCtx;

/// Reject any cursor whose direction is not forward (`"fwd"`).
///
/// `FINAL`-qualified reads are ordered consistently, but only forward cursors
/// are minted in v1. A `"bwd"` cursor would be silently walked forward since
/// the keyset operator is derived from the sort direction, not `cursor.d`.
/// Reject it fail-closed until backward paging is implemented.
///
/// # Errors
///
/// Returns an error string when `cursor.d` is anything other than `"fwd"`.
pub fn ensure_forward_cursor(cursor: &CursorV1) -> Result<(), String> {
    if cursor.d == "fwd" {
        Ok(())
    } else {
        Err(format!(
            "unsupported cursor direction `{}`: only forward paging is supported",
            cursor.d
        ))
    }
}

/// Render an `ORDER BY` column list from an `ODataOrderBy`, resolving each
/// field through `col`.
///
/// # Errors
///
/// Returns an error string when the order is empty or a field is not on the
/// allowlist.
pub fn render_order_by(
    order: &ODataOrderBy,
    col: impl Fn(&str) -> Option<&'static str>,
) -> Result<String, String> {
    if order.is_empty() {
        return Err("order must not be empty".to_owned());
    }
    let parts = order
        .0
        .iter()
        .map(|key| {
            let column = col(&key.field)
                .ok_or_else(|| format!("order field not allowlisted: {}", key.field))?;
            let dir = match key.dir {
                SortDir::Asc => "ASC",
                SortDir::Desc => "DESC",
            };
            Ok(format!("{column} {dir}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(parts.join(", "))
}

/// Build a keyset predicate as a row-value tuple comparison.
///
/// For an all-ascending order: `(c1, c2, …) > (?, ?, …)`.
/// For an all-descending order: `(c1, c2, …) < (?, ?, …)`.
/// Mixed directions are unsupported (v1 limitation).
///
/// # Errors
///
/// Returns an error string when `order_pairs` is empty, its length differs from
/// `cursor_keys`, a field is nullable (not keyset-safe), a field is not on the
/// allowlist, directions are mixed, or a cursor key cannot be parsed.
pub fn keyset_predicate(
    order_pairs: &[(&str, bool)],
    cursor_keys: &[String],
    col: impl Fn(&str) -> Option<&'static str>,
    kind: impl Fn(&str) -> Option<FieldKind>,
    keyset_safe: impl Fn(&str) -> bool,
    ctx: &mut SqlCtx,
) -> Result<String, String> {
    if order_pairs.is_empty() {
        return Err("keyset order must not be empty".to_owned());
    }
    if order_pairs.len() != cursor_keys.len() {
        return Err(format!(
            "cursor key count {} does not match order arity {}",
            cursor_keys.len(),
            order_pairs.len()
        ));
    }

    let all_asc = order_pairs.iter().all(|(_, asc)| *asc);
    let all_desc = order_pairs.iter().all(|(_, asc)| !*asc);
    let cmp = if all_asc {
        ">"
    } else if all_desc {
        "<"
    } else {
        return Err("mixed-direction keyset orders are unsupported in v1".to_owned());
    };

    let mut columns = Vec::with_capacity(order_pairs.len());
    let mut placeholders = Vec::with_capacity(order_pairs.len());
    for ((field, _), raw) in order_pairs.iter().zip(cursor_keys.iter()) {
        if !keyset_safe(field) {
            return Err(format!(
                "keyset field is nullable and cannot be a keyset ordering key: {field}"
            ));
        }
        let column = col(field).ok_or_else(|| format!("keyset field not allowlisted: {field}"))?;
        let field_kind =
            kind(field).ok_or_else(|| format!("keyset field has no known kind: {field}"))?;
        let bind = cursor_key_to_bind(field_kind, raw)?;
        placeholders.push(bind.placeholder());
        ctx.push(bind);
        columns.push(column);
    }

    Ok(format!(
        "({}) {cmp} ({})",
        columns.join(", "),
        placeholders.join(", ")
    ))
}

/// Parse a raw cursor key string into a typed [`SqlBind`] for `ClickHouse`.
///
/// `DateTimeUtc` keys are parsed as RFC 3339 strings and converted to
/// `DateTime64Micros` (`i64` epoch-microseconds), matching the `i64` storage
/// type of `DateTime64(6)` columns.
///
/// # Errors
///
/// Returns an error string when the value cannot be parsed for its kind, or
/// when the kind is not supported as a keyset column.
pub fn cursor_key_to_bind(kind: FieldKind, raw: &str) -> Result<SqlBind, String> {
    match kind {
        FieldKind::DateTimeUtc => {
            let dt = OffsetDateTime::parse(raw, &Rfc3339)
                .map_err(|e| format!("invalid datetime cursor key `{raw}`: {e}"))?;
            // Convert nanoseconds to microseconds via Euclidean division (equiv.
            // to truncating division for positive timestamps; avoids the
            // clippy::integer_division lint on i128 /).
            let nanos = dt.unix_timestamp_nanos();
            let micros = nanos.div_euclid(1_000);
            #[allow(
                clippy::cast_possible_truncation,
                reason = "practical timestamps fit in i64"
            )]
            Ok(SqlBind::DateTime64Micros(micros as i64))
        }
        FieldKind::Uuid => Uuid::from_str(raw)
            .map(SqlBind::Uuid)
            .map_err(|e| format!("invalid uuid cursor key `{raw}`: {e}")),
        FieldKind::String => Ok(SqlBind::Str(raw.to_owned())),
        other => Err(format!(
            "cursor key kind `{other}` is not supported as a keyset column"
        )),
    }
}

/// Build and encode the forward cursor for the next page.
///
/// # Errors
///
/// Returns an error string when the order is empty, its arity differs from
/// `last_row_keys`, or serialisation fails.
pub fn encode_next_cursor(
    order: &ODataOrderBy,
    last_row_keys: &[String],
    filter_hash: Option<&str>,
) -> Result<String, String> {
    if order.is_empty() {
        return Err("cursor order must not be empty".to_owned());
    }
    if order.0.len() != last_row_keys.len() {
        return Err(format!(
            "row key count {} does not match order arity {}",
            last_row_keys.len(),
            order.0.len()
        ));
    }
    let primary_dir = order.0.first().map_or(SortDir::Asc, |k| k.dir);
    let cursor = CursorV1 {
        k: last_row_keys.to_vec(),
        o: primary_dir,
        s: order.to_signed_tokens(),
        f: filter_hash.map(str::to_owned),
        d: "fwd".to_owned(),
    };
    cursor
        .encode()
        .map_err(|e| format!("cursor encode failed: {e}"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "keyset_tests.rs"]
mod keyset_tests;
