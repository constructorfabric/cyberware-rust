#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! End-to-end scope-enforcement test through the full toolkit init →
//! `ClientHub` resolve → evaluate path.
//!
//! Asserts the spec's four scope outcomes (wildcard passes, empty denies,
//! third-party mismatch denies, third-party match passes) against a real
//! `AuthZResolverPluginGear::init()` that registers the plugin in
//! `ClientHub` and reaches it through `dyn AuthZResolverPluginClient`.
//!
//! The scope-deny paths assert `rbac.call_count() == 0`
//! explicitly (the "scope deny never calls RBAC" contract).
//! The pass-through paths configure the RBAC fake to `Allowed` so the
//! post-policy stub is reached.

mod common;

use std::sync::Arc;

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::EvaluationRequestBuilder;
use rbac_sdk::models::PermissionScopeType;
use toolkit::Gear;

use common::InMemoryRbacServiceClient;

/// The GTS error code asserted for every scope-deny path. Mirrors the
/// crate-internal `error_codes::SCOPE_MISMATCH_V1` constant.
const SCOPE_MISMATCH_V1: &str = "gts.cf.core.errors.err.v1~cf.authz.errors.scope_mismatch.v1";

/// Wire up the plugin with a caller-supplied RBAC fake and return the
/// resolved `dyn AuthZResolverPluginClient` plus the rbac handle for
/// assertions. The tenant resolver is pre-configured with a single root
/// tenant so `Global`-scoped Allow paths complete materialization
/// without setup boilerplate in every test.
async fn init_and_resolve(
    rbac: Arc<InMemoryRbacServiceClient>,
) -> (
    Arc<dyn authz_resolver_sdk::AuthZResolverPluginClient>,
    Arc<common::InMemoryRbacServiceClient>,
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

fn rbac_allowing() -> Arc<InMemoryRbacServiceClient> {
    Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ))
}

fn rbac_default() -> Arc<InMemoryRbacServiceClient> {
    Arc::new(InMemoryRbacServiceClient::default())
}

#[tokio::test]
async fn wildcard_scope_passes_check_and_reaches_stub() {
    let (plugin, rbac) = init_and_resolve(rbac_allowing()).await;
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["*".to_owned()])
                .with_supported_properties(vec!["owner_tenant_id".to_owned()])
                .build(),
        )
        .await;

    // Global-scoped allow → Ok(decision=true) with a tenant constraint.
    let response = response.expect("wildcard + Global allow -> Ok(decision=true)");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    assert_eq!(
        rbac.call_count(),
        1,
        "wildcard scope passes -> RBAC IS called"
    );
}

#[tokio::test]
async fn empty_token_scopes_deny_via_full_pipeline() {
    let (plugin, rbac) = init_and_resolve(rbac_default()).await;
    let response = plugin
        .evaluate(EvaluationRequestBuilder::default().build())
        .await
        .expect("scope-deny is Ok(decision=false), not Err");

    assert!(!response.decision);
    let reason = response
        .context
        .deny_reason
        .expect("deny_reason populated on scope deny");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    assert_eq!(rbac.call_count(), 0, "scope deny must never call RBAC");
}

#[tokio::test]
async fn third_party_scope_mismatch_denies() {
    let (plugin, rbac) = init_and_resolve(rbac_default()).await;
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
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    let details = reason.details.expect("details populated");
    assert!(
        details.contains("delete") && details.contains("write"),
        "details should name op + class: {details}"
    );
    assert_eq!(rbac.call_count(), 0, "scope deny must never call RBAC");
}

#[tokio::test]
async fn third_party_scope_match_falls_through_to_stub() {
    let (plugin, rbac) = init_and_resolve(rbac_allowing()).await;
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["read:events".to_owned()])
                .with_action_name("read")
                .with_supported_properties(vec!["owner_tenant_id".to_owned()])
                .build(),
        )
        .await;

    // Scope class `read` matches `read:events`. Scope passes → policy
    // allow → tenant materialization → Ok(decision=true) with one constraint.
    let response = response.expect("matching scope + tenant allow -> Ok");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn read_token_allowed_for_get_and_list() {
    // get/list are mapped to the `read` scope class in the default config, so
    // a read-only token satisfies them. Without those entries they fall back to
    // `write`, and a read-only token is denied its own GET/LIST.
    for op in ["get", "list"] {
        let (plugin, _rbac) = init_and_resolve(rbac_allowing()).await;
        let response = plugin
            .evaluate(
                EvaluationRequestBuilder::default()
                    .with_token_scopes(vec!["read:events".to_owned()])
                    .with_action_name(op)
                    .with_supported_properties(vec!["owner_tenant_id".to_owned()])
                    .build(),
            )
            .await
            .unwrap_or_else(|e| panic!("read token + {op} should pass scope check: {e:?}"));
        assert!(
            response.decision,
            "read-only token must be allowed to '{op}' (read-style op)"
        );
    }
}

#[tokio::test]
async fn read_scope_caller_allowed_for_read_only_adapter_level_operation() {
    // Adapter-level data-plane operations are declared by adapter manifests, so
    // their ids never appear in `operation_to_scope`. Falling back to the
    // `write` class would deny a read-only operator operation to a caller
    // holding only read-class scopes, one step *above* the RBAC matcher and
    // therefore invisible to any reasoning about roles.
    //
    // The caller here is deliberately **scoped**, not first-party: a
    // first-party client carries the wildcard scope, short-circuits step 3
    // entirely, and would pass this test no matter what the map says. Only a
    // narrow token exercises the operation-to-scope path at all.
    let (plugin, rbac) = init_and_resolve(rbac_allowing()).await;
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["read:adapters".to_owned()])
                .with_action_name("list-access-keys")
                .with_supported_properties(vec!["owner_tenant_id".to_owned()])
                .build(),
        )
        .await
        .expect("read-scope caller + read-only adapter-level operation -> Ok");

    assert!(
        response.decision,
        "a read-scope caller must reach RBAC for a read-only adapter-level operation"
    );
    assert_eq!(
        rbac.call_count(),
        1,
        "the scope check must pass the request through to RBAC, not decide it"
    );
}

#[tokio::test]
async fn read_scope_caller_denied_a_mutating_adapter_level_operation() {
    // The other half of the pair: relaxing the read-only case must not relax
    // the mutating one. `delete` at the id's boundary keeps the `write` class,
    // so the same read-scope caller is still stopped before RBAC.
    let (plugin, rbac) = init_and_resolve(rbac_allowing()).await;
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["read:adapters".to_owned()])
                .with_action_name("delete-access-key")
                .build(),
        )
        .await
        .expect("scope-deny is Ok(decision=false), not Err");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    assert_eq!(rbac.call_count(), 0, "scope deny must never call RBAC");
}

#[tokio::test]
async fn unmapped_operation_falls_back_to_default_unmapped_scope() {
    let (plugin, rbac) = init_and_resolve(rbac_default()).await;
    // `some_new_operation` is not in the default `operation_to_scope` map,
    // so the scope class falls back to `default_unmapped_scope` = "write".
    // The read:events token cannot satisfy "write" → deny.
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["read:events".to_owned()])
                .with_action_name("some_new_operation")
                .build(),
        )
        .await
        .expect("scope-deny is Ok");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    let details = reason.details.expect("details populated");
    assert!(
        details.contains("some_new_operation") && details.contains("write"),
        "details should name the unmapped op and the fallback class: {details}"
    );
    assert_eq!(rbac.call_count(), 0, "scope deny must never call RBAC");
}
