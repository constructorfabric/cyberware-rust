use super::test_harness::MetricsHarness;
use super::*;
use crate::domain::deny::{build_allow_response, build_deny_response};
use crate::domain::error::PluginError;
use crate::domain::metrics_port::{CacheKind, HierarchyOp, RbacOp, Resolver, ScopeTypeLabel};

#[test]
fn allow_records_duration_only() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let allow: Result<EvaluationResponse, PluginError> = Ok(build_allow_response(vec![]));
    metrics.record_outcome(Duration::from_millis(3), &allow);
    harness.force_flush();

    assert_eq!(
        harness.histogram_count(AUTHZ_EVALUATION_DURATION, &[("decision", "allow")]),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHZ_EVALUATION_DENY, &[]),
        0,
        "allow must not increment deny_total"
    );
}

#[test]
fn scope_mismatch_deny_records_reason() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let deny: Result<EvaluationResponse, PluginError> =
        Ok(build_deny_response(error_codes::SCOPE_MISMATCH_V1, None));
    metrics.record_outcome(Duration::from_millis(1), &deny);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(AUTHZ_EVALUATION_DENY, &[("reason", "scope_mismatch")]),
        1
    );
    assert_eq!(harness.counter_value(AUTHZ_FAIL_CLOSED, &[]), 0);
}

#[test]
fn constraints_unavailable_deny_also_counts_fail_closed() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let deny: Result<EvaluationResponse, PluginError> = Ok(build_deny_response(
        error_codes::CONSTRAINTS_UNAVAILABLE_V1,
        None,
    ));
    metrics.record_outcome(Duration::from_millis(1), &deny);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "constraints_unavailable")]),
        1
    );
}

#[test]
fn rbac_error_records_error_and_fail_closed() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let err: Result<EvaluationResponse, PluginError> = Err(PluginError::RbacUnavailable);
    metrics.record_outcome(Duration::from_millis(2), &err);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(
            AUTHZ_EVALUATION_ERROR,
            &[("error_type", "rbac_unavailable")]
        ),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "rbac_unavailable")]),
        1
    );
    assert_eq!(
        harness.histogram_count(AUTHZ_EVALUATION_DURATION, &[("decision", "error")]),
        1
    );
}

#[test]
fn scope_provenance_internal_has_dedicated_error_and_fail_closed_labels() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let err: Result<EvaluationResponse, PluginError> = Err(PluginError::RbacScopeProvenanceInvalid);
    metrics.record_outcome(Duration::from_millis(1), &err);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(
            AUTHZ_EVALUATION_ERROR,
            &[("error_type", "rbac_scope_provenance_invalid")]
        ),
        1
    );
    assert_eq!(
        harness.counter_value(
            AUTHZ_FAIL_CLOSED,
            &[("reason", "rbac_scope_provenance_invalid")]
        ),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "all_constraints_failed")]),
        0,
        "RBAC provenance drift must not be attributed to constraint compilation"
    );
}

#[test]
fn validation_failure_is_client_error_not_fail_closed() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    // A request-validation failure is a client-fault variant, which carries
    // `invalid_request` and no fail-closed reason on the variant itself.
    let err: Result<EvaluationResponse, PluginError> = Err(PluginError::MissingResourceType);
    metrics.record_outcome(Duration::from_millis(1), &err);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(AUTHZ_EVALUATION_ERROR, &[("error_type", "invalid_request")]),
        1
    );
    assert_eq!(harness.counter_value(AUTHZ_FAIL_CLOSED, &[]), 0);
}

#[test]
fn cache_invariant_internal_error_is_fail_closed() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let err: Result<EvaluationResponse, PluginError> =
        Err(PluginError::internal("cache value type mismatch"));
    metrics.record_outcome(Duration::from_millis(1), &err);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(AUTHZ_EVALUATION_ERROR, &[("error_type", "unexpected")]),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "all_constraints_failed")]),
        1
    );
}

#[test]
fn tenant_not_found_is_fail_closed_but_not_resolver_timeout() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    // A deterministic tenant rejection (deleted/unauthorized).
    let err: Result<EvaluationResponse, PluginError> = Err(PluginError::TenantNotFound);
    metrics.record_outcome(Duration::from_millis(1), &err);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(
            AUTHZ_EVALUATION_ERROR,
            &[("error_type", "tenant_resolver_not_found")]
        ),
        1
    );
    // Still fail-closed (we deny because the scope can't be resolved)...
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "scope_unresolvable")]),
        1
    );
    // ...but it must NOT page on-call as a phantom resolver outage.
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "resolver_timeout")]),
        0,
        "deterministic not-found must not be labelled resolver_timeout"
    );
}

#[test]
fn resource_group_not_found_is_fail_closed_but_not_resolver_timeout() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let err: Result<EvaluationResponse, PluginError> = Err(PluginError::ResourceGroupNotFound);
    metrics.record_outcome(Duration::from_millis(1), &err);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(
            AUTHZ_EVALUATION_ERROR,
            &[("error_type", "rg_resolver_not_found")]
        ),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "scope_unresolvable")]),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "resolver_timeout")]),
        0,
        "deterministic not-found must not be labelled resolver_timeout"
    );
}

#[test]
fn gts_registry_unavailable_is_labelled_distinctly_not_resolver_timeout() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    // A Strict-mode GTS registry outage.
    let err: Result<EvaluationResponse, PluginError> = Err(PluginError::GtsRegistryUnavailable);
    metrics.record_outcome(Duration::from_millis(1), &err);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(
            AUTHZ_EVALUATION_ERROR,
            &[("error_type", "gts_registry_unavailable")]
        ),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "gts_registry_unavailable")]),
        1
    );
    // It must NOT land in the resolver_timeout bucket (that pages on-call for a
    // phantom resolver outage when the registry is what is down).
    assert_eq!(
        harness.counter_value(AUTHZ_FAIL_CLOSED, &[("reason", "resolver_timeout")]),
        0,
        "GTS registry outage must not be labelled resolver_timeout"
    );
}

#[test]
fn cache_hit_ratio_tracks_running_ratio_per_kind() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    // 3 accesses on tenant_subtree: miss, hit, hit → ratio 2/3.
    metrics.record_cache_access(CacheKind::TenantSubtree, false);
    metrics.record_cache_access(CacheKind::TenantSubtree, true);
    metrics.record_cache_access(CacheKind::TenantSubtree, true);
    harness.force_flush();

    let ratio = harness
        .gauge_value(
            AUTHZ_EVALUATION_CACHE_HIT_RATIO,
            &[("cache_type", "tenant_subtree")],
        )
        .expect("cache hit ratio gauge present");
    assert!(
        (ratio - 2.0 / 3.0).abs() < 1e-9,
        "expected 2/3, got {ratio}"
    );
}

#[test]
fn hierarchy_and_rbac_durations_record() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    metrics.record_hierarchy_query(
        Resolver::Tenant,
        HierarchyOp::SubtreeIds,
        Duration::from_millis(2),
    );
    metrics.record_rbac_query(RbacOp::EvaluatePermission, Duration::from_millis(1));
    harness.force_flush();

    // Each histogram is written exactly once above; pinning `== 1` catches a
    // double-emit regression (which `>= 1` would silently let through).
    assert_eq!(
        harness.histogram_count(
            AUTHZ_HIERARCHY_QUERY_DURATION,
            &[("resolver", "tenant"), ("operation", "subtree_ids")]
        ),
        1
    );
    assert_eq!(
        harness.histogram_count(
            AUTHZ_RBAC_QUERY_DURATION,
            &[("operation", "evaluate_permission")]
        ),
        1
    );
}

#[test]
fn scope_type_counter_records_label() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    metrics.record_scope_type(ScopeTypeLabel::TenantSubtree);
    harness.force_flush();

    assert_eq!(
        harness.counter_value(
            AUTHZ_EVALUATION_BY_SCOPE_TYPE,
            &[("scope_type", "tenant_subtree")]
        ),
        1
    );
}

/// Canon-parity trait impl forwarding to the inherent (enum-typed) methods.
/// Components keep their concrete `Arc<AuthZMetrics>` handle; the trait exists
/// so `NoopMetrics` is a drop-in and matches the `account-management` pattern.
/// Lives in this `cfg(test)` companion (no production caller takes
/// `dyn AuthzMetricsPort` yet); move it back beside `AuthZMetrics` when a DI
/// call site materializes.
impl crate::domain::metrics_port::AuthzMetricsPort for AuthZMetrics {
    fn record_outcome(&self, elapsed: Duration, result: &Result<EvaluationResponse, PluginError>) {
        AuthZMetrics::record_outcome(self, elapsed, result);
    }
    fn record_scope_type(&self, scope_type: ScopeTypeLabel) {
        AuthZMetrics::record_scope_type(self, scope_type);
    }
    fn record_cache_access(&self, kind: CacheKind, hit: bool) {
        AuthZMetrics::record_cache_access(self, kind, hit);
    }
    fn record_hierarchy_query(
        &self,
        resolver: Resolver,
        operation: HierarchyOp,
        duration: Duration,
    ) {
        AuthZMetrics::record_hierarchy_query(self, resolver, operation, duration);
    }
    fn record_rbac_query(&self, operation: RbacOp, duration: Duration) {
        AuthZMetrics::record_rbac_query(self, operation, duration);
    }
    fn record_constraint_compilation(&self, scope_type: ScopeTypeLabel, duration: Duration) {
        AuthZMetrics::record_constraint_compilation(self, scope_type, duration);
    }
    fn inc_unsupported_property(&self) {
        AuthZMetrics::inc_unsupported_property(self);
    }
    fn inc_scope_provenance_rejection(&self) {
        AuthZMetrics::inc_scope_provenance_rejection(self);
    }
    fn inc_token_scope_narrowing(&self, operation: NarrowingOp) {
        AuthZMetrics::inc_token_scope_narrowing(self, operation);
    }
    fn inc_barrier_mode_override(&self) {
        AuthZMetrics::inc_barrier_mode_override(self);
    }
    fn inc_capability_negotiation(&self, capabilities: &str) {
        AuthZMetrics::inc_capability_negotiation(self, capabilities);
    }
}

/// An unreadable `tenant_id` claim is a malformed request, so it must label as
/// `invalid_request` and raise no fail-closed reason.
///
/// End-to-end check that the emitted METRIC is the client-fault one, not just
/// the `labels()` pair: a misclassified client fault would land on
/// `Unexpected` + `AllConstraintsFailed`, which pages on-call.
#[test]
fn unreadable_subject_tenant_claim_labels_as_invalid_request() {
    for detail in ["not a string", "not a UUID: invalid length"] {
        let harness = MetricsHarness::new();
        let metrics = harness.metrics();
        let err: Result<EvaluationResponse, PluginError> =
            Err(PluginError::UnreadableSubjectTenant {
                detail: detail.to_owned(),
            });
        metrics.record_outcome(Duration::from_millis(1), &err);
        harness.force_flush();

        assert_eq!(
            harness.counter_value(AUTHZ_EVALUATION_ERROR, &[("error_type", "invalid_request")]),
            1,
            "a malformed tenant claim is a client fault, not a system fault: {detail}"
        );
        assert_eq!(
            harness.counter_value(AUTHZ_FAIL_CLOSED, &[]),
            0,
            "a client fault must not bump fail_closed; that is what pages \
             on-call for a phantom outage: {detail}"
        );
    }
}
