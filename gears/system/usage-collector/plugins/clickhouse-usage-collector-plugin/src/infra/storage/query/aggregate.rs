//! Aggregation SQL builder for `ClickHouse` — inject-safe SELECT-expression
//! builders for the pushed-down `aggregate` query.
//!
//! Adapted from the reference plugin's `aggregate.rs` with `ClickHouse`-specific
//! differences:
//!
//! - `SUM(value)` returns `Decimal128(9)` natively (no `::numeric` cast needed).
//! - `COUNT(*)` returns `UInt64`; for uniform decoding as
//!   `Option<bigdecimal::BigDecimal>` the caller must handle the JSON type.
//! - `metadata['key']` (map subscript) replaces `metadata ->> $key`.
//! - `toString(tenant_id)` converts the `UUID` column to `String` for grouping.
//! - The grouped result is capped at `MAX_AGGREGATION_BUCKETS + 1` rows via a
//!   server-side `LIMIT` when `dim_count > 0`.
//!
//! All identifiers come from closed enum allowlists; the only caller-derived
//! value (a [`AggregationDimension::Metadata`] key) is bound via `ctx` (`?`).

use usage_collector_sdk::{AggregationDimension, AggregationOp, MAX_AGGREGATION_BUCKETS};

use super::bind::SqlBind;
use super::translate::SqlCtx;

/// SQL aggregate expression for an [`AggregationOp`].
///
/// `ClickHouse` returns the correct numeric type natively; `AVG` is rounded to
/// 6 fractional digits to cap the scale of a non-terminating quotient
/// (DESIGN.md §3.6 Aggregated Query).
#[must_use]
pub fn agg_select_expr(op: AggregationOp) -> &'static str {
    match op {
        AggregationOp::Sum => "SUM(value)",
        AggregationOp::Count => "COUNT(*)",
        AggregationOp::Min => "MIN(value)",
        AggregationOp::Max => "MAX(value)",
        AggregationOp::Avg => "ROUND(AVG(value), 6)",
    }
}

/// `corrects_id`-partition `WHERE` clause for an [`AggregationOp`], or `None`.
///
/// Per plugin-spi.md §Method 3:
/// - `SUM` nets across all active rows (compensations carry a signed `value`) →
///   **no** partition (`None`).
/// - All other ops (`COUNT`, `MIN`, `MAX`, `AVG`) restrict to `corrects_id IS NULL`
///   rows — compensations adjust `SUM`, they are not events.
#[must_use]
pub fn corrects_id_partition_clause(op: AggregationOp) -> Option<&'static str> {
    match op {
        AggregationOp::Sum => None,
        AggregationOp::Count | AggregationOp::Min | AggregationOp::Max | AggregationOp::Avg => {
            Some("corrects_id IS NULL")
        }
    }
}

/// SQL `String`-returning expression for a group [`AggregationDimension`].
///
/// - `TenantId`: `toString(tenant_id)` (UUID → String in `ClickHouse`).
/// - `Metadata(key)`: `metadata[?]` (map subscript; key is bound via `ctx`).
/// - All other identity columns are emitted directly.
pub fn dimension_select_expr(dim: &AggregationDimension, ctx: &mut SqlCtx) -> String {
    match dim {
        AggregationDimension::TenantId => "toString(tenant_id)".to_owned(),
        AggregationDimension::ResourceId => "resource_id".to_owned(),
        AggregationDimension::ResourceType => "resource_type".to_owned(),
        AggregationDimension::SubjectId => "subject_id".to_owned(),
        AggregationDimension::SubjectType => "subject_type".to_owned(),
        AggregationDimension::Metadata(key) => {
            ctx.push(SqlBind::Str(key.as_str().to_owned()));
            "metadata[?]".to_owned()
        }
    }
}

/// `LIMIT` clause bounding the aggregate's distinct-group cardinality.
///
/// When `dim_count > 0` a `GROUP BY` is present; cap to
/// `MAX_AGGREGATION_BUCKETS + 1` so the gateway can detect an over-cap result
/// and return `400`. When `dim_count == 0` there is no grouping and exactly
/// one row is produced — no cap needed.
#[must_use]
pub fn aggregate_limit_clause(dim_count: usize) -> String {
    if dim_count == 0 {
        String::new()
    } else {
        format!(" LIMIT {}", MAX_AGGREGATION_BUCKETS + 1)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "aggregate_tests.rs"]
mod aggregate_tests;
