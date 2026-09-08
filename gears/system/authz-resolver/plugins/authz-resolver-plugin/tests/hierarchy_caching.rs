#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::items_after_statements
)]
//! End-to-end hierarchy-caching tests through the full toolkit init →
//! `ClientHub` resolve → evaluate path.
//!
//! Covers the four hierarchy materialization shapes from the design:
//! `TenantSubtree`, `GroupSubtree`, `Combined`, and the reserved-variant
//! deny path. Plus a singleflight-under-concurrent-load assertion.

mod common;

use std::sync::Arc;

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::EvaluationRequestBuilder;
use authz_resolver_sdk::AuthZResolverError;
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::PermissionScopeType;
use resource_group_sdk::models::ResourceGroupMembership;
use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantRef, TenantStatus};
use toolkit::Gear;
use uuid::Uuid;

use common::{
    InMemoryRbacServiceClient, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
};

fn root_tenant_with_id(id: u128) -> TenantInfo {
    TenantInfo {
        id: TenantId(Uuid::from_u128(id)),
        name: format!("tenant-{id:x}"),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn child_tenant_ref(id: u128) -> TenantRef {
    TenantRef {
        id: TenantId(Uuid::from_u128(id)),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

async fn init_plugin(
    rbac: Arc<InMemoryRbacServiceClient>,
    tr: Arc<InMemoryTenantResolverClient>,
    rg: Arc<InMemoryResourceGroupClient>,
) -> Arc<dyn authz_resolver_sdk::AuthZResolverPluginClient> {
    let (ctx, hub, _registry, _rbac, _tr, _rg) =
        common::build_ctx_with(common::MissingDependency::None, rbac, tr, rg);
    AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect("init should succeed");
    common::resolve_plugin(&hub)
}

fn wildcard_request() -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .build()
}

fn wildcard_request_with_tenant_property() -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build()
}

fn wildcard_request_with_resource_id_property() -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        // Group scopes now emit a tenant-paired constraint, so the PEP must
        // also support owner_tenant_id (RESOURCE_GROUP_MODEL.md invariant).
        .with_supported_properties(vec!["id".to_owned(), "owner_tenant_id".to_owned()])
        .build()
}

#[tokio::test]
async fn i_01_tenant_subtree_cache_hit_through_evaluate() {
    // RBAC keeps allowing TenantSubtree; tenant resolver scripted with
    // root → [A, B] descendants. Two evaluations of the same request
    // share one tenant-resolver round-trip.
    let root_id = Uuid::from_u128(0x1000);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let root = root_tenant_with_id(0x1000);
    let descendants = vec![child_tenant_ref(0x1001), child_tenant_ref(0x1002)];
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let plugin = init_plugin(Arc::clone(&rbac), Arc::clone(&tr), rg).await;

    // First evaluation: cache miss → tenant resolver invoked. The response
    // is Ok(decision=true) with an In(owner_tenant_id, [root, A, B])
    // predicate.
    let r1 = plugin
        .evaluate(wildcard_request_with_tenant_property())
        .await
        .expect("tenant subtree allow -> Ok(decision=true)");
    assert!(r1.decision);
    assert_eq!(r1.context.constraints.len(), 1);
    let after_first = tr.call_count();
    assert_eq!(
        after_first, 1,
        "first call must invoke the tenant resolver exactly once (no N+1), \
         got call_count={after_first}"
    );

    // Second evaluation: cache hit → no additional resolver call.
    let r2 = plugin
        .evaluate(wildcard_request_with_tenant_property())
        .await
        .expect("second call also allow");
    assert!(r2.decision);
    assert_eq!(r2.context.constraints.len(), 1);
    assert_eq!(
        tr.call_count(),
        after_first,
        "second call must hit cache and NOT increment tenant resolver call_count"
    );
}

#[tokio::test]
async fn i_05_singleflight_under_concurrent_load() {
    // Ten concurrent evaluates with the same TenantSubtree scope; the
    // tenant resolver fake must be called exactly once thanks to
    // singleflight coalescing.
    let root_id = Uuid::from_u128(0x2000);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let root = root_tenant_with_id(0x2000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let plugin = init_plugin(rbac, Arc::clone(&tr), rg).await;

    let barrier = Arc::new(tokio::sync::Barrier::new(10));
    let mut handles = Vec::new();
    for _ in 0..10 {
        let plugin = Arc::clone(&plugin);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            plugin
                .evaluate(wildcard_request_with_tenant_property())
                .await
        }));
    }
    for h in handles {
        let result = h.await.unwrap();
        // Tenant subtree allow → Ok(decision=true) with a constraint.
        // Every task gets the same shape.
        let response = result.expect("tenant subtree allow");
        assert!(response.decision);
        assert_eq!(response.context.constraints.len(), 1);
    }

    assert_eq!(
        tr.call_count(),
        1,
        "singleflight must coalesce 10 concurrent evaluates into one tenant resolver call"
    );
}

#[tokio::test]
async fn i_02_group_subtree_materialization_through_evaluate() {
    // GroupSubtree scope → RG fake's get_group_descendants + list_memberships
    // are both called.
    let rg1 = Uuid::from_u128(0xA001);
    let rg1a = Uuid::from_u128(0xA01A);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg1],
        },
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    // RG1's descendant is RG1a; RG1a has two memberships.
    rg.add_group_descendants(
        rg1,
        vec![resource_group_sdk::models::ResourceGroupWithDepth {
            id: rg1a,
            code: "gts.cf.core.rg.type.v1~test.v1~".to_owned(),
            name: "rg1a".to_owned(),
            hierarchy: resource_group_sdk::models::GroupHierarchyWithDepth {
                parent_id: Some(rg1),
                tenant_id: Uuid::from_u128(1),
                depth: 1,
            },
            metadata: None,
        }],
    );
    rg.add_memberships(vec![
        ResourceGroupMembership {
            group_id: rg1a,
            resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
            resource_id: Uuid::from_u128(0xB001).to_string(),
        },
        ResourceGroupMembership {
            group_id: rg1a,
            resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
            resource_id: Uuid::from_u128(0xB002).to_string(),
        },
    ]);

    let plugin = init_plugin(rbac, tr, Arc::clone(&rg)).await;
    let response = plugin
        .evaluate(wildcard_request_with_resource_id_property())
        .await
        .expect("GroupSubtree allow -> Ok(decision=true)");

    use authz_resolver_sdk::constraints::Predicate;
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    match &response.context.constraints[0].predicates[0] {
        Predicate::In(in_pred) => {
            assert_eq!(in_pred.property, "id");
            assert_eq!(in_pred.values.len(), 2);
        }
        other => panic!("expected In(id, [...]), got {other:?}"),
    }

    // Exactly two RG calls for a single root group: one
    // get_group_descendants(RG1), one list_memberships(group_id in (RG1, RG1a)).
    // Pinned (not `>= 2`) so an N+1 over the subtree would fail here.
    assert_eq!(
        rg.call_count(),
        2,
        "expected exactly descendants + memberships (no N+1), got {}",
        rg.call_count()
    );
}

#[tokio::test]
async fn reserved_scope_variant_returns_insufficient_permissions_deny() {
    // TenantDirect is reserved (per design §3.6; v1 producers don't emit it).
    // The plugin's defensive deny path surfaces it as a business deny with
    // `insufficient_permissions.v1` — NOT an Err(Internal).
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::TenantDirect {
            tenant_id: Uuid::from_u128(0x3001),
        },
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let plugin = init_plugin(rbac, Arc::clone(&tr), rg).await;
    // supported_properties is set defensively though the Denied arm skips
    // the validation step.
    let response = plugin
        .evaluate(wildcard_request_with_tenant_property())
        .await
        .expect("reserved variant is Ok(decision=false), not Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(
        reason.error_code,
        "gts.cf.core.errors.err.v1~cf.authz.errors.insufficient_permissions.v1"
    );
    // Fail-closed teeth: the reserved variant must short-circuit to deny
    // BEFORE any tenant-resolver call — a regression that resolved first
    // (fail-open hazard) would bump this count.
    assert_eq!(
        tr.call_count(),
        0,
        "reserved scope variant must deny before querying the tenant resolver"
    );
}

#[tokio::test]
async fn hierarchy_resolver_error_propagates_as_service_unavailable() {
    // Tenant resolver in error mode → materialize_scope fails →
    // ServiceUnavailable surfaces all the way to the gateway.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: Uuid::from_u128(0x4001),
        },
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::with_error("simulated outage"));
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let plugin = init_plugin(rbac, tr, rg).await;
    let result = plugin.evaluate(wildcard_request()).await;
    match result {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "tenant resolver unavailable");
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn rbac_error_short_circuits_before_hierarchy() {
    // RBAC fails → ServiceUnavailable; hierarchy resolvers must NEVER be
    // called.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::internal("simulated"),
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let plugin = init_plugin(rbac, Arc::clone(&tr), Arc::clone(&rg)).await;
    let result = plugin.evaluate(wildcard_request()).await;
    assert!(matches!(
        result,
        Err(AuthZResolverError::ServiceUnavailable(_))
    ));
    assert_eq!(
        tr.call_count(),
        0,
        "RBAC error must short-circuit before hierarchy"
    );
    assert_eq!(rg.call_count(), 0);
}
