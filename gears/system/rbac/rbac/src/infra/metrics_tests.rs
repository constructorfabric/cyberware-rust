//! Unit tests for the parent module, kept out of line: an inline
//! `#[cfg(test)]` block of this size is a lint error in this workspace.

use super::test_harness::MetricsHarness;
use super::*;

#[test]
fn dependency_query_emits_duration_and_health() {
    let h = MetricsHarness::new();
    let m = h.metrics();
    m.dependency_query(
        Dependency::TenantResolver,
        DependencyOp::GetAncestors,
        DependencyOutcome::Success,
        0.003,
    );
    h.force_flush();
    assert_eq!(
        h.histogram_count(
            "rbac_dependency_query_duration_milliseconds",
            &[
                ("dependency", "tenant_resolver"),
                ("operation", "get_ancestors")
            ]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "rbac_dependency_health_total",
            &[
                ("dependency", "tenant_resolver"),
                ("operation", "get_ancestors"),
                ("outcome", "success")
            ]
        ),
        1
    );
}

/// The display-name counter carries exactly `kind` + `outcome` and
/// nothing identifying, and a zero count emits nothing at all.
#[test]
fn principal_name_counter_carries_only_categorical_labels() {
    let h = MetricsHarness::new();
    let m = h.metrics();
    m.principal_name_resolve(NameKind::User, NameOutcome::Degraded, 1);
    m.principal_name_resolve(NameKind::Group, NameOutcome::Resolved, 3);
    m.principal_name_resolve(NameKind::Author, NameOutcome::Resolved, 0);
    h.force_flush();
    assert_eq!(
        h.counter_value(
            "rbac_principal_name_resolve_total",
            &[("kind", "user"), ("outcome", "degraded")]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "rbac_principal_name_resolve_total",
            &[("kind", "group"), ("outcome", "resolved")]
        ),
        3
    );
    // `attributes_match` requires an exact attribute-set match, so
    // this also proves no third label (a tenant or an id) rode along.
    assert_eq!(
        h.counter_value(
            "rbac_principal_name_resolve_total",
            &[("kind", "author"), ("outcome", "resolved")]
        ),
        0,
        "a zero count must not emit a data point"
    );
}

#[test]
fn permission_counters_carry_labels() {
    let h = MetricsHarness::new();
    let m = h.metrics();
    m.permission_deny(EvalDenyReason::NotPermissionExclusion);
    m.permission_allow_scope_type(EvalScopeType::Global);
    m.permission_eval_error(EvalErrorType::Internal);
    m.permission_eval_duration(EvalResult::Allow, 0.01);
    h.force_flush();
    assert_eq!(
        h.counter_value(
            "rbac_permission_deny_total",
            &[("reason", "not_permission_exclusion")]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "rbac_permission_eval_by_scope_type_total",
            &[("scope_type", "global")]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "rbac_permission_eval_error_total",
            &[("error_type", "internal")]
        ),
        1
    );
    assert_eq!(
        h.histogram_count(
            "rbac_permission_eval_duration_milliseconds",
            &[("result", "allow")]
        ),
        1
    );
}
