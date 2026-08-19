use usage_collector_sdk::{AggregationDimension, AggregationOp, MAX_AGGREGATION_BUCKETS};

use super::{
    agg_select_expr, aggregate_limit_clause, corrects_id_partition_clause, dimension_select_expr,
};
use crate::infra::storage::query::bind::SqlBind;

// ── corrects_id_partition_clause ─────────────────────────────────────────────

#[test]
fn sum_has_no_partition_clause() {
    assert_eq!(corrects_id_partition_clause(AggregationOp::Sum), None);
}

#[test]
fn count_has_corrects_id_is_null_clause() {
    assert_eq!(
        corrects_id_partition_clause(AggregationOp::Count),
        Some("corrects_id IS NULL")
    );
}

#[test]
fn min_max_avg_have_corrects_id_partition() {
    for op in [AggregationOp::Min, AggregationOp::Max, AggregationOp::Avg] {
        assert_eq!(
            corrects_id_partition_clause(op),
            Some("corrects_id IS NULL"),
            "op = {op:?}"
        );
    }
}

// ── aggregate_limit_clause ────────────────────────────────────────────────────

#[test]
fn dim_count_zero_yields_empty_limit() {
    assert_eq!(aggregate_limit_clause(0), "");
}

#[test]
fn dim_count_one_yields_max_plus_one_limit() {
    let clause = aggregate_limit_clause(1);
    assert_eq!(clause, format!(" LIMIT {}", MAX_AGGREGATION_BUCKETS + 1));
}

#[test]
fn dim_count_three_yields_max_plus_one_limit() {
    let clause = aggregate_limit_clause(3);
    assert!(clause.contains(&(MAX_AGGREGATION_BUCKETS + 1).to_string()));
}

// ── agg_select_expr ───────────────────────────────────────────────────────────

#[test]
fn sum_expr_contains_sum_value() {
    assert_eq!(agg_select_expr(AggregationOp::Sum), "SUM(value)");
}

#[test]
fn count_expr_contains_count_star() {
    assert_eq!(agg_select_expr(AggregationOp::Count), "COUNT(*)");
}

#[test]
fn avg_expr_rounds_to_6_places() {
    assert_eq!(agg_select_expr(AggregationOp::Avg), "ROUND(AVG(value), 6)");
}

#[test]
fn min_and_max_exprs_aggregate_value() {
    assert_eq!(agg_select_expr(AggregationOp::Min), "MIN(value)");
    assert_eq!(agg_select_expr(AggregationOp::Max), "MAX(value)");
}

// ── dimension_select_expr ─────────────────────────────────────────────────────

#[test]
fn tenant_id_dimension_uses_to_string() {
    let (expr, bind) = dimension_select_expr(&AggregationDimension::TenantId);
    assert_eq!(expr, "toString(tenant_id)");
    assert!(bind.is_none());
}

/// The identity columns are emitted verbatim and bind nothing — they come from
/// a closed enum, so there is no caller-derived text in the SQL.
#[test]
fn identity_dimensions_are_emitted_verbatim_without_binds() {
    for (dim, expected) in [
        (AggregationDimension::ResourceId, "resource_id"),
        (AggregationDimension::ResourceType, "resource_type"),
        (AggregationDimension::SubjectId, "subject_id"),
        (AggregationDimension::SubjectType, "subject_type"),
    ] {
        let (expr, bind) = dimension_select_expr(&dim);
        assert_eq!(expr, expected, "dim = {dim:?}");
        assert!(bind.is_none(), "dim = {dim:?} binds nothing");
    }
}

#[test]
fn metadata_dimension_returns_key_bind_and_map_subscript() {
    use usage_collector_sdk::MetadataKey;
    let key = MetadataKey::new("region").unwrap();
    let (expr, bind) = dimension_select_expr(&AggregationDimension::Metadata(key));
    assert_eq!(expr, "metadata[?]");
    assert!(matches!(bind, Some(SqlBind::Str(s)) if s == "region"));
}
