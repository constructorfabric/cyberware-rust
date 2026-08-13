//! Injection-safe filter translation: a validated `FilterNode<F>` becomes a
//! parameterized `ClickHouse` `WHERE` fragment plus an ordered bind list.
//!
//! Identifiers come only from the closed allowlists ([`record_column`] /
//! [`usage_type_column`]); values are always bound via `?` placeholders via
//! [`crate::infra::storage::query::bind::odata_value_to_bind`].
//!
//! ## `ClickHouse` vs `PostgreSQL` dialect differences
//!
//! - Placeholders: `ClickHouse` uses positional `?` (not `$N`); there is no
//!   numbered parameter index in the query fragment — the bind order is tracked
//!   by `SqlCtx::binds` alone.
//! - Metadata push-down: `metadata['key']` (map subscript) replaces `metadata
//!   ->> $key`.
//! - `DateTime64(6)` comparisons use `i64` epoch-microseconds bound as
//!   `DateTime64Micros` and converted by `fromUnixTimestamp64Micro(?)`.
//! - String comparisons for `status` / `kind` are straightforward (`String`
//!   bind).

use toolkit_odata::filter::{FilterField, FilterNode, FilterOp};

pub use super::bind::{SqlBind, bind_one, odata_value_to_bind};
pub use toolkit_odata::filter::ODataValue;

/// Closed allowlist mapping a `usage_records` filter-field name to its column.
///
/// The identity map is the security boundary — only these nine identifiers can
/// ever reach the SQL string. `gts_id` is intentionally absent: it is a typed
/// parameter on the SPI, not a `$filter` field.
#[must_use]
pub fn record_column(field_name: &str) -> Option<&'static str> {
    match field_name {
        "id" => Some("id"),
        "created_at" => Some("created_at"),
        "tenant_id" => Some("tenant_id"),
        "resource_id" => Some("resource_id"),
        "resource_type" => Some("resource_type"),
        "subject_id" => Some("subject_id"),
        "subject_type" => Some("subject_type"),
        "corrects_id" => Some("corrects_id"),
        "status" => Some("status"),
        _ => None,
    }
}

/// Closed allowlist mapping a `usage_type_catalog` filter-field name to its column.
#[must_use]
pub fn usage_type_column(field_name: &str) -> Option<&'static str> {
    match field_name {
        "gts_id" => Some("gts_id"),
        "kind" => Some("kind"),
        _ => None,
    }
}

/// Bind accumulator for a single `ClickHouse` SQL statement.
///
/// `ClickHouse` uses positional `?` placeholders — there is no `$N` index.
/// `binds` holds the values in the order they must be applied via `bind_one`,
/// matching the left-to-right `?` occurrence in the assembled query.
///
/// Callers that need to precede the filter binds with fixed binds (e.g. a
/// `gts_id` bound as the first `?`) should push those first, then pass `ctx`
/// to the translate functions.
pub struct SqlCtx {
    /// Accumulated binds in placeholder order.
    ///
    /// `pub(crate)` so the stores and keyset helper can accumulate binds in the
    /// same ordered context; not exposed to external consumers.
    pub(crate) binds: Vec<SqlBind>,
}

impl SqlCtx {
    /// Create an empty context.
    #[must_use]
    pub fn new() -> Self {
        Self { binds: Vec::new() }
    }

    /// Append a bind. Called by translate functions for every `?` emitted.
    pub(crate) fn push(&mut self, b: SqlBind) {
        self.binds.push(b);
    }
}

impl Default for SqlCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a comparison [`FilterOp`] to its SQL operator string.
///
/// # Errors
///
/// Returns an error string for non-comparison operators (`In` / `Contains` /
/// `StartsWith` / `EndsWith` / `And` / `Or`): those are handled structurally
/// by the translators.
fn op_sql(op: FilterOp) -> Result<&'static str, String> {
    match op {
        FilterOp::Eq => Ok("="),
        FilterOp::Ne => Ok("<>"),
        FilterOp::Gt => Ok(">"),
        FilterOp::Ge => Ok(">="),
        FilterOp::Lt => Ok("<"),
        FilterOp::Le => Ok("<="),
        other => Err(format!("unsupported operator: {other:?}")),
    }
}

/// Translate a `usage_records` filter node into a parameterized `ClickHouse`
/// `WHERE` fragment, pushing each value onto `ctx` as a bind.
///
/// Identifiers resolve through [`record_column`]; an unmapped field is an error
/// (never interpolated). Values resolve through [`odata_value_to_bind`].
///
/// # Errors
///
/// Returns an error string when a field is not on the allowlist, an operator is
/// unsupported, a composite carries a non-`And`/`Or` operator, or a value
/// cannot be converted to a bind.
pub fn translate_record_filter<F: FilterField>(
    node: &FilterNode<F>,
    ctx: &mut SqlCtx,
) -> Result<String, String> {
    translate_filter(node, ctx, record_column)
}

/// Translate a `usage_type_catalog` filter node. Identical to
/// [`translate_record_filter`] but resolves identifiers through
/// [`usage_type_column`].
///
/// # Errors
///
/// Same conditions as [`translate_record_filter`].
pub fn translate_usage_type_filter<F: FilterField>(
    node: &FilterNode<F>,
    ctx: &mut SqlCtx,
) -> Result<String, String> {
    translate_filter(node, ctx, usage_type_column)
}

/// Shared recursive walker parameterised over the column allowlist.
fn translate_filter<F: FilterField>(
    node: &FilterNode<F>,
    ctx: &mut SqlCtx,
    col: fn(&str) -> Option<&'static str>,
) -> Result<String, String> {
    match node {
        FilterNode::Binary { field, op, value } => {
            let column = col(field.name())
                .ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            let operator = op_sql(*op)?;
            let bind = odata_value_to_bind(value)?;
            let placeholder = bind.placeholder();
            ctx.push(bind);
            Ok(format!("{column} {operator} {placeholder}"))
        }
        FilterNode::InList { field, values } => {
            let column = col(field.name())
                .ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            if values.is_empty() {
                return Err("IN list must not be empty".to_owned());
            }
            let placeholders = values
                .iter()
                .map(|v| {
                    odata_value_to_bind(v).map(|b| {
                        let placeholder = b.placeholder();
                        ctx.push(b);
                        placeholder
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(format!("{column} IN ({})", placeholders.join(", ")))
        }
        FilterNode::Composite { op, children } => {
            let joiner = match op {
                FilterOp::And => " AND ",
                FilterOp::Or => " OR ",
                other => return Err(format!("invalid composite operator: {other:?}")),
            };
            let parts = children
                .iter()
                .map(|child| translate_filter(child, ctx, col))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", parts.join(joiner)))
        }
        FilterNode::Not(inner) => Ok(format!("NOT ({})", translate_filter(inner, ctx, col)?)),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "translate_tests.rs"]
mod translate_tests;
