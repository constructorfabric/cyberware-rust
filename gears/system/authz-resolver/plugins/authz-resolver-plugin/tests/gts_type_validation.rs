#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! End-to-end tests for GTS type validation through the full
//! toolkit init → `ClientHub` resolve → evaluate path.
//!
//! Covers the three modes, the cache-first lookup with TTL semantics,
//! mode-dependent fallback during outage, and the fail-fast
//! subject-then-resource ordering.

mod common;

use std::sync::Arc;

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::EvaluationRequestBuilder;
use authz_resolver_plugin::test_support::request_builder::{
    DEFAULT_RESOURCE_TYPE, DEFAULT_SUBJECT_TYPE,
};
use authz_resolver_sdk::AuthZResolverError;
use rbac_sdk::models::PermissionScopeType;
use toolkit::Gear;

use common::{
    InMemoryRbacServiceClient, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
    RecordingTypesRegistry,
};

/// Default fakes — registry knows nothing, tenant resolver / RG / RBAC at
/// defaults. Tests script behavior on top.
fn default_fakes() -> (
    Arc<InMemoryRbacServiceClient>,
    Arc<InMemoryTenantResolverClient>,
    Arc<InMemoryResourceGroupClient>,
) {
    (
        Arc::new(InMemoryRbacServiceClient::with_allowed(
            vec![],
            PermissionScopeType::Global,
        )),
        Arc::new(InMemoryTenantResolverClient::default()),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
}

async fn init_plugin_with_mode(
    rbac: Arc<InMemoryRbacServiceClient>,
    tr: Arc<InMemoryTenantResolverClient>,
    rg: Arc<InMemoryResourceGroupClient>,
    mode: &str,
) -> (
    Arc<dyn authz_resolver_sdk::AuthZResolverPluginClient>,
    Arc<RecordingTypesRegistry>,
) {
    let (ctx, hub, registry, _rbac, _tr, _rg) = common::build_ctx_with_config(
        rbac,
        tr,
        rg,
        common::CtxOverrides {
            gts_validation_mode: Some(mode.to_owned()),
            ..Default::default()
        },
    );
    AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect("init should succeed");
    (common::resolve_plugin(&hub), registry)
}

fn wildcard_request() -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build()
}

#[tokio::test]
async fn t_31_warn_mode_unknown_subject_type_allows() {
    // Explicit Warn mode (it is no longer the default), registry doesn't know
    // the default subject / resource types. The GTS validator emits a warn log and lets the
    // request through. Downstream may still fail (tenant resolver has
    // no root configured), but the failure must NOT be an
    // "unknown gts type" Internal from the validator.
    let (rbac, tr, rg) = default_fakes();
    let (plugin, _registry) = init_plugin_with_mode(rbac, tr, rg, "warn").await;

    // Warn mode must let the unknown types through the validator. With the
    // default fakes (RBAC allow Global + empty tenant resolver) the request
    // then reaches Global materialization and fails at `get_root_tenant` —
    // an outcome only reachable if validation did NOT block.
    match plugin.evaluate(wildcard_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => assert_eq!(
            msg, "tenant resolver unavailable",
            "warn mode must pass GTS validation and reach materialization"
        ),
        other => panic!(
            "warn mode should allow the unknown type through to materialization, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn t_32_strict_mode_unknown_subject_type_returns_business_deny() {
    // The default subject type passes the foundation's request-shape
    // validator, but the registry has not been primed — so the GTS
    // validator's lookup returns Unknown. Strict mode now surfaces
    // a business deny with `unknown_resource_type.v1` (was `Err(Internal)`).
    const UNKNOWN_RESOURCE_TYPE_V1: &str =
        "gts.cf.core.errors.err.v1~cf.authz.errors.unknown_resource_type.v1";

    let (rbac, tr, rg) = default_fakes();
    let (plugin, _registry) = init_plugin_with_mode(rbac, tr, rg, "strict").await;

    let response = plugin
        .evaluate(wildcard_request())
        .await
        .expect("Strict + Unknown is Ok(decision=false), NOT Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, UNKNOWN_RESOURCE_TYPE_V1);
    let details = reason.details.expect("details populated");
    assert!(
        details.contains(DEFAULT_SUBJECT_TYPE),
        "details must name the offending type: {details}"
    );
}

#[tokio::test]
async fn t_33_strict_mode_registry_unavailable_yields_service_unavailable() {
    let (rbac, tr, rg) = default_fakes();
    let (plugin, registry) = init_plugin_with_mode(rbac, tr, rg, "strict").await;
    registry.set_unavailable(true);

    match plugin.evaluate(wildcard_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "gts schema registry unavailable");
        }
        other => panic!("expected Err(ServiceUnavailable), got {other:?}"),
    }
}

#[tokio::test]
async fn t_34_off_mode_skips_registry_entirely() {
    let (rbac, tr, rg) = default_fakes();
    let (plugin, registry) = init_plugin_with_mode(rbac, tr, rg, "off").await;

    // No types primed; default request has subject + resource types that
    // the registry has never seen. Off mode must skip the registry call
    // entirely — no "unknown gts type" Internal, and the registry's
    // call count remains zero.
    // Off mode skips the registry AND must not block: the request proceeds to
    // Global materialization and fails at the (empty) tenant resolver.
    match plugin.evaluate(wildcard_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => assert_eq!(
            msg, "tenant resolver unavailable",
            "off mode must skip validation and proceed to materialization"
        ),
        other => panic!("off mode should proceed past validation, got {other:?}"),
    }
    assert_eq!(
        registry.get_type_schema_call_count(),
        0,
        "Off mode must not invoke get_type_schema"
    );
}

#[tokio::test]
async fn t_35_cache_hit_survives_registry_outage() {
    // Strict mode, prime the registry with the default subject + resource
    // types, evaluate once (warms the cache), then take the registry down
    // and evaluate again. The second call must still pass the validator
    // (cached entries serve through the outage).
    let (rbac, tr, rg) = default_fakes();
    let (plugin, registry) = init_plugin_with_mode(rbac, tr, rg, "strict").await;
    common::register_default_types(&registry);

    // Warm — registry called for subject + resource.
    _ = plugin.evaluate(wildcard_request()).await;
    let warm_calls = registry.get_type_schema_call_count();
    assert_eq!(
        warm_calls, 2,
        "warm call must hit the registry exactly twice (subject + resource), \
         got {warm_calls}"
    );

    // Take registry down. Validator should serve cached entries.
    registry.set_unavailable(true);

    // The validator passes; downstream may still fail (tenant resolver),
    // but the failure must NOT be the GTS-validator's ServiceUnavailable.
    // The validator serves cached entries, so the only failure is the
    // downstream tenant resolver — never the validator's own outage error.
    match plugin.evaluate(wildcard_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => assert_eq!(
            msg, "tenant resolver unavailable",
            "cached entries must serve through the outage - the validator must \
             not surface its own ServiceUnavailable"
        ),
        other => panic!(
            "expected downstream tenant-resolver error after cached validation, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn t_36_uncached_type_during_outage_surfaces_service_unavailable() {
    // Strict mode + outage + uncached type → ServiceUnavailable surfaces
    // The cache is never primed, so the first evaluate() is a
    // plain cache miss that collides with the registry outage and must surface
    // the validator's ServiceUnavailable.
    let (rbac, tr, rg) = default_fakes();
    let (plugin, registry) = init_plugin_with_mode(rbac, tr, rg, "strict").await;

    // Don't prime the cache. Trip the outage immediately. The first
    // evaluate() call hits cache miss → registry unavailable → fallback.
    registry.set_unavailable(true);
    match plugin.evaluate(wildcard_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "gts schema registry unavailable");
        }
        other => panic!("expected ServiceUnavailable from validator, got {other:?}"),
    }
}

#[tokio::test]
async fn t_37_fail_fast_invalid_subject_short_circuits_resource() {
    // Strict mode, default subject NOT in the known set, default resource
    // IS in the known set. The subject lookup fails first — fail-fast
    // prevents the resource lookup. The deny is now a business
    // deny with `unknown_resource_type.v1` (was `Err(Internal)`).
    const UNKNOWN_RESOURCE_TYPE_V1: &str =
        "gts.cf.core.errors.err.v1~cf.authz.errors.unknown_resource_type.v1";

    let (rbac, tr, rg) = default_fakes();
    let (plugin, registry) = init_plugin_with_mode(rbac, tr, rg, "strict").await;
    // Prime only the resource type — the subject lookup must miss.
    registry.add_known_type(DEFAULT_RESOURCE_TYPE);

    let response = plugin
        .evaluate(wildcard_request())
        .await
        .expect("subject failure is Ok(decision=false), not Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, UNKNOWN_RESOURCE_TYPE_V1);
    assert!(
        reason
            .details
            .as_deref()
            .is_some_and(|d| d.contains(DEFAULT_SUBJECT_TYPE)),
        "details must name the unknown subject type"
    );
    // Only the subject was queried — the resource lookup short-circuited.
    assert_eq!(
        registry.get_type_schema_call_count(),
        1,
        "subject failure must short-circuit before resource lookup"
    );
}

// Sanity check that `DEFAULT_SUBJECT_TYPE` is reachable for use as a
// fallback in tests that prime only that type.
#[allow(dead_code)]
const _SUBJECT_REACHABLE: &str = DEFAULT_SUBJECT_TYPE;
