#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! End-to-end module init test — happy path.
//!
//! Verifies the full `Gear::init()` sequence: config deserialization,
//! `ClientHub` dependency resolution, types-registry registration, and
//! `ClientHub` registration of `dyn AuthZResolverPluginClient`. Then
//! invokes `evaluate()` through the resolved trait object to confirm the
//! gateway-visible call path is in place.

mod common;

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::EvaluationRequestBuilder;
use toolkit::Gear;

#[tokio::test]
async fn happy_path_init_registers_plugin_and_records_gts_registration() {
    // RBAC must be scripted to Allow so the post-scope policy step
    // does not return the default-stub error before reaching the post-policy
    // assertion below.
    use rbac_sdk::models::PermissionScopeType;
    use std::sync::Arc;
    use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantStatus};
    use uuid::Uuid;
    let rbac = Arc::new(common::InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let root = TenantInfo {
        id: TenantId(Uuid::from_u128(1)),
        name: "root".to_owned(),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    };
    let tr = Arc::new(common::InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(common::InMemoryResourceGroupClient::default());
    let (ctx, hub, registry, _rbac, _tr, _rg) =
        common::build_ctx_with(common::MissingDependency::None, rbac, tr, rg);

    let module = AuthZResolverPluginGear;
    module
        .init(&ctx)
        .await
        .expect("init should succeed when every dependency is registered");

    // Types-registry recorded exactly one batch with one entity carrying the
    // plugin's GTS instance id and configured vendor/priority.
    let calls = registry.calls();
    assert_eq!(calls.len(), 1, "exactly one register batch expected");
    let payload = &calls[0][0];
    assert_eq!(payload.get("vendor").and_then(|v| v.as_str()), Some("cf"));
    assert_eq!(
        payload.get("priority").and_then(serde_json::Value::as_i64),
        Some(100)
    );
    let registered_id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .expect("registered payload should carry an id");
    assert!(
        registered_id.contains("authz_resolver.plugin.v1"),
        "instance id should contain the plugin type segment: {registered_id}"
    );

    // The gateway-visible trait object is reachable via ClientHub. With
    // a wildcard `token_scopes`, scope enforcement passes and
    // evaluation reaches the still-stubbed post-scope step.
    let plugin = common::resolve_plugin(&hub);
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["*".to_owned()])
                .with_supported_properties(vec!["owner_tenant_id".to_owned()])
                .build(),
        )
        .await;

    // Global-scoped allow → Ok(decision=true) with a single-tenant Eq
    // predicate on owner_tenant_id carrying the root tenant id.
    let response = response.expect("Global-scoped allow returns Ok(decision=true)");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    assert!(response.context.deny_reason.is_none());
}

const INVALID_REQUEST_V1: &str = "gts.cf.core.errors.err.v1~cf.authz.errors.invalid_request.v1";

#[tokio::test]
async fn evaluate_validation_failure_surfaces_through_registered_plugin() {
    // Validation runs before scope and policy. The default RBAC fake stays
    // unscripted — it must never be called for a validation-deny request.
    let (ctx, hub, _registry, rbac, _tr, _rg) = common::build_ctx(common::MissingDependency::None);
    AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect("init should succeed");

    let plugin = common::resolve_plugin(&hub);
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_subject_type(Some("bogus-type".to_owned()))
                .build(),
        )
        .await;

    // A malformed request reaches the PEP as a business deny, not as a
    // 500-class `Internal` it would retry.
    let response = response.expect("a client fault is Ok(decision=false), not Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, INVALID_REQUEST_V1);
    assert_eq!(
        reason.details.as_deref(),
        Some("unknown subject type: bogus-type")
    );
    assert_eq!(
        rbac.call_count(),
        0,
        "validation deny must never reach RBAC"
    );
}
