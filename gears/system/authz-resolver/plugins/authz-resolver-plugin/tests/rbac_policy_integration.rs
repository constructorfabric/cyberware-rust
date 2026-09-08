#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! End-to-end RBAC-policy-integration tests through the full toolkit init
//! → `ClientHub` resolve → evaluate path.
//!
//! Covers: insufficient permissions, RBAC infrastructure errors, recovery
//! after error, RBAC allow → falls through to still-stubbed post-policy
//! step. The `scope_deny_never_calls_rbac` case lives here too (alongside its
//! duplicate in `scope_enforcement.rs`) so the RBAC-not-called assertion is
//! visible in the file dedicated to policy evaluation.

mod common;

use std::sync::Arc;

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::{EvaluationRequestBuilder, RbacScript};
use authz_resolver_sdk::AuthZResolverError;
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{DenyReason as RbacDenyReason, PermissionScopeType};
use toolkit::Gear;

use common::InMemoryRbacServiceClient;

/// Constructor Fabric error code asserted for every policy-deny path. Mirrors
/// the crate-internal `error_codes::INSUFFICIENT_PERMISSIONS_V1`.
const INSUFFICIENT_PERMISSIONS_V1: &str =
    "gts.cf.core.errors.err.v1~cf.authz.errors.insufficient_permissions.v1";

const SCOPE_MISMATCH_V1: &str = "gts.cf.core.errors.err.v1~cf.authz.errors.scope_mismatch.v1";

/// Wire up the plugin with a caller-supplied RBAC fake and return the
/// resolved client plus the rbac handle for assertions. The tenant
/// resolver is pre-configured with a root tenant so `Global`-scoped
/// Allow paths complete materialization without per-test boilerplate.
async fn init_and_resolve(
    rbac: Arc<InMemoryRbacServiceClient>,
) -> (
    Arc<dyn authz_resolver_sdk::AuthZResolverPluginClient>,
    Arc<InMemoryRbacServiceClient>,
) {
    use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantStatus};
    use uuid::Uuid;
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
    let (ctx, hub, _registry, rbac, _tr, _rg) =
        common::build_ctx_with(common::MissingDependency::None, rbac, tr, rg);
    AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect("init should succeed");
    (common::resolve_plugin(&hub), rbac)
}

fn wildcard_scope_request() -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .build()
}

/// Wildcard request that also declares `owner_tenant_id` as a supported
/// property — required by tests that reach the constraint generator.
fn wildcard_request_with_tenant_property() -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build()
}

#[tokio::test]
async fn rbac_denied_returns_insufficient_permissions() {
    let (plugin, rbac) = init_and_resolve(Arc::new(InMemoryRbacServiceClient::with_denied(
        RbacDenyReason::NoMatchingPermission,
    )))
    .await;

    let response = plugin
        .evaluate(wildcard_scope_request())
        .await
        .expect("policy-deny is Ok(decision=false), not Err");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn rbac_internal_error_returns_service_unavailable() {
    let (plugin, rbac) = init_and_resolve(Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::internal("simulated"),
    )))
    .await;

    match plugin.evaluate(wildcard_scope_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "rbac service unavailable");
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn rbac_dependency_unavailable_returns_service_unavailable() {
    let (plugin, rbac) = init_and_resolve(Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::dependency_unavailable("tenant-resolver"),
    )))
    .await;

    match plugin.evaluate(wildcard_scope_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "rbac service unavailable");
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn rbac_validation_error_also_collapses_to_service_unavailable() {
    // Every RbacServiceError variant maps to ServiceUnavailable.
    // The categorical info is in tracing; the user-facing message is stable.
    let (plugin, rbac) = init_and_resolve(Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::validation("simulated"),
    )))
    .await;

    match plugin.evaluate(wildcard_scope_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "rbac service unavailable");
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn rbac_allowed_falls_through_to_post_policy_stub() {
    let (plugin, rbac) = init_and_resolve(Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    )))
    .await;

    // Global-scoped allow → Ok(decision=true) with a tenant constraint.
    let response = plugin
        .evaluate(wildcard_request_with_tenant_property())
        .await
        .expect("Global-scoped allow -> Ok(decision=true)");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn forwards_subject_action_and_resource_to_rbac_unaltered() {
    // Pins the plugin's OUTBOUND contract: the request it sends to the RBAC
    // service must carry the subject id, the mapped principal type, the short
    // operation from `action.name`, and the concrete `resource.type` — all
    // unaltered. The fake captures the request; without this, only the
    // response side was ever asserted.
    use rbac_sdk::models::PrincipalType;
    let (plugin, rbac) = init_and_resolve(Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    )))
    .await;

    let subject_id = uuid::Uuid::from_u128(0xCAFE);
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(subject_id)
        .with_action_name("read")
        .with_resource_type("gts.cf.core.resources.test.v1~")
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin.evaluate(request).await.expect("allow");
    assert!(response.decision);

    let sent = rbac
        .last_evaluate_permission_request()
        .expect("RBAC must have been queried on the allow path");
    assert_eq!(
        sent.subject_id,
        subject_id.to_string(),
        "subject id must be forwarded unaltered"
    );
    assert_eq!(
        sent.principal_type,
        PrincipalType::User,
        "default subject type must map to PrincipalType::User"
    );
    assert_eq!(
        sent.operation, "read",
        "action.name must be forwarded as the operation"
    );
    assert_eq!(
        sent.resource_type, "gts.cf.core.resources.test.v1~",
        "resource.type must be forwarded unaltered"
    );
}

#[tokio::test]
async fn e_11_rbac_recovery_after_error_no_caching() {
    // Build the fake in Error mode, then flip to Allowed after the first
    // call. The plugin must NOT cache the prior error — the second call
    // sees the live RBAC state.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::internal("transient"),
    ));
    let (plugin, rbac) = init_and_resolve(rbac).await;

    // 1st call: RBAC errors → ServiceUnavailable.
    match plugin.evaluate(wildcard_scope_request()).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "rbac service unavailable");
        }
        other => panic!("first call must surface RBAC error, got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 1);

    // Flip the script — RBAC is back.
    rbac.set_script(RbacScript::Allowed {
        grants: vec![],
        scope_type: PermissionScopeType::Global,
    });

    // 2nd call: RBAC allowed → tenant materialization → Ok(decision=true).
    // The plugin reached RBAC live; no stale denial.
    let response = plugin
        .evaluate(wildcard_request_with_tenant_property())
        .await
        .expect("after recovery: tenant allow -> Ok(decision=true)");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    assert_eq!(rbac.call_count(), 2);
}

#[tokio::test]
async fn scope_deny_never_calls_rbac() {
    // The "scope deny short-circuits before policy" contract: even with
    // RBAC available and willing to allow, a scope-denying request must
    // never reach RBAC.
    let (plugin, rbac) = init_and_resolve(Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    )))
    .await;

    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["read:events".to_owned()])
                .with_action_name("delete")
                .build(),
        )
        .await
        .expect("scope-deny is Ok(decision=false), not Err");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    assert_eq!(rbac.call_count(), 0, "scope-deny must never call RBAC");
}
