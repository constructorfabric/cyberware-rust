#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::config::CacheConfig;
use crate::domain::clock::StubClock;
use crate::domain::deny::error_codes::INSUFFICIENT_PERMISSIONS_V1;
use tenant_resolver_sdk::api::TenantResolverClient;

use crate::test_support::{
    EvaluationRequestBuilder, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
};
use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantRef};

use crate::infra::hierarchy_upstream::SdkHierarchyUpstream;
use crate::infra::metrics::AuthZMetrics;

fn client_with_tenants(tenants: Vec<TenantInfo>) -> HierarchyClient {
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(tenants));
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let cache = Arc::new(HierarchyCache::new(
        &CacheConfig::default(),
        Arc::new(StubClock::new()) as Arc<dyn crate::domain::clock::Clock>,
        Arc::new(AuthZMetrics::from_global()),
    ));
    HierarchyClient::new(
        Arc::new(SdkHierarchyUpstream::new(
            tr,
            rg,
            Arc::new(AuthZMetrics::from_global()),
        )),
        cache,
    )
}

fn root_tenant(id: u128) -> TenantInfo {
    TenantInfo {
        id: TenantId(Uuid::from_u128(id)),
        name: format!("t-{id:x}"),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn wildcard_request() -> EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .build()
}

#[test]
fn all_invalid_tenant_status_falls_back_to_none() {
    use authz_resolver_sdk::models::TenantContext;
    // Every supplied status is unrecognized → `resolve_tenant_context`
    // returns `None`, so `get_tenant_subtree_ids` applies the documented
    // `[Active]` default rather than an empty (= "all statuses") filter
    // that would wrongly include suspended/deleted tenants.
    let request = EvaluationRequestBuilder::default()
        .with_tenant_context(Some(TenantContext {
            tenant_status: Some(vec!["bogus".to_owned(), "nonsense".to_owned()]),
            ..Default::default()
        }))
        .build();
    let (_barrier, status, _mode) = resolve_tenant_context(&request);
    assert_eq!(
        status, None,
        "all-invalid tenant_status must fall back to None ([Active] default)"
    );
}

#[test]
fn empty_tenant_status_list_falls_back_to_none() {
    use authz_resolver_sdk::models::TenantContext;
    // A non-empty request that yields an empty parsed set must behave the
    // same as all-invalid: fall back to the `[Active]` default.
    let request = EvaluationRequestBuilder::default()
        .with_tenant_context(Some(TenantContext {
            tenant_status: Some(vec![]),
            ..Default::default()
        }))
        .build();
    let (_barrier, status, _mode) = resolve_tenant_context(&request);
    assert_eq!(
        status, None,
        "empty tenant_status list must fall back to None ([Active] default)"
    );
}

#[test]
fn mixed_tenant_status_keeps_only_recognized_values() {
    use authz_resolver_sdk::models::TenantContext;
    // Recognized values are kept; unrecognized ones are dropped (not a
    // fall-back), so the filter stays meaningful instead of widening.
    let request = EvaluationRequestBuilder::default()
        .with_tenant_context(Some(TenantContext {
            tenant_status: Some(vec!["active".to_owned(), "bogus".to_owned()]),
            ..Default::default()
        }))
        .build();
    let (_barrier, status, _mode) = resolve_tenant_context(&request);
    assert_eq!(
        status,
        Some(vec![TenantStatus::Active]),
        "unrecognized values are dropped while recognized ones are kept"
    );
}

#[tokio::test]
async fn reserved_tenant_direct_materializes_as_denied() {
    let client = client_with_tenants(vec![]);
    let request = wildcard_request();
    let result = client
        .materialize_scope(
            &PermissionScopeType::TenantDirect {
                tenant_id: Uuid::from_u128(1),
            },
            &request,
        )
        .await
        .expect("reserved variant maps to Ok(Denied), not Err");
    match result {
        Materialization::Denied {
            error_code,
            details,
        } => {
            assert_eq!(error_code, INSUFFICIENT_PERMISSIONS_V1);
            let details = details.expect("details populated");
            assert!(
                details.contains("TenantDirect"),
                "details should name the variant, got {details:?}"
            );
        }
        other => panic!("expected Materialization::Denied, got {other:?}"),
    }
}

#[tokio::test]
async fn reserved_explicit_groups_materializes_as_denied() {
    let client = client_with_tenants(vec![]);
    let request = wildcard_request();
    let result = client
        .materialize_scope(
            &PermissionScopeType::ExplicitGroups {
                group_ids: vec![Uuid::from_u128(1)],
            },
            &request,
        )
        .await
        .expect("reserved variant maps to Ok(Denied), not Err");
    match result {
        Materialization::Denied {
            error_code,
            details,
        } => {
            assert_eq!(error_code, INSUFFICIENT_PERMISSIONS_V1);
            assert!(
                details
                    .expect("details populated")
                    .contains("ExplicitGroups"),
            );
        }
        other => panic!("expected Materialization::Denied, got {other:?}"),
    }
}

#[tokio::test]
async fn combined_with_inner_reserved_short_circuits_to_denied() {
    // Combined of [TenantSubtree(T1), TenantDirect(T2)] — the second
    // inner scope is reserved; the whole Combined materialization must
    // be Denied (fail-closed, no partial constraints).
    let t1 = Uuid::from_u128(0xA);
    let root = root_tenant(0xA);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let cache = Arc::new(HierarchyCache::new(
        &CacheConfig::default(),
        Arc::new(StubClock::new()) as Arc<dyn crate::domain::clock::Clock>,
        Arc::new(AuthZMetrics::from_global()),
    ));
    let client = HierarchyClient::new(
        Arc::new(SdkHierarchyUpstream::new(
            tr,
            rg,
            Arc::new(AuthZMetrics::from_global()),
        )),
        cache,
    );

    let scope = PermissionScopeType::Combined {
        scopes: vec![
            PermissionScopeType::TenantSubtree { root_tenant_id: t1 },
            PermissionScopeType::TenantDirect {
                tenant_id: Uuid::from_u128(0xB),
            },
        ],
    };
    let result = client
        .materialize_scope(&scope, &wildcard_request())
        .await
        .expect("inner reserved propagates as Ok(Denied)");
    assert!(
        matches!(result, Materialization::Denied { error_code, .. } if error_code == INSUFFICIENT_PERMISSIONS_V1),
        "expected Materialization::Denied, got {result:?}"
    );
}

#[tokio::test]
async fn combined_with_nested_reserved_at_depth_two_short_circuits() {
    // Reserved variant at depth ≥ 2:
    //   Combined { scopes: [ Combined { scopes: [TenantDirect(T2)] }, TenantSubtree(T1) ] }
    // Pins that `first_reserved_variant` actually recurses into nested
    // `Combined` (a non-recursive scan would miss this and let the legitimate
    // TenantSubtree(T1) leg call the tenant resolver before discovering the
    // reserved inner).
    let t1 = Uuid::from_u128(0xA);
    let root = root_tenant(0xA);
    let tr_concrete = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr_concrete.add_descendants(root.id, vec![]);
    // Coerce to the dyn-Arc HierarchyClient::new wants, keep `tr_concrete` for
    // the call_count assertion afterward (Arc::clone is type-preserving, so
    // the coercion has to happen at a type-ascribed let binding).
    let tr: Arc<dyn TenantResolverClient> = tr_concrete.clone();
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let cache = Arc::new(HierarchyCache::new(
        &CacheConfig::default(),
        Arc::new(StubClock::new()) as Arc<dyn crate::domain::clock::Clock>,
        Arc::new(AuthZMetrics::from_global()),
    ));
    let client = HierarchyClient::new(
        Arc::new(SdkHierarchyUpstream::new(
            tr,
            rg,
            Arc::new(AuthZMetrics::from_global()),
        )),
        cache,
    );

    let scope = PermissionScopeType::Combined {
        scopes: vec![
            PermissionScopeType::Combined {
                scopes: vec![PermissionScopeType::TenantDirect {
                    tenant_id: Uuid::from_u128(0xB),
                }],
            },
            PermissionScopeType::TenantSubtree { root_tenant_id: t1 },
        ],
    };
    let result = client
        .materialize_scope(&scope, &wildcard_request())
        .await
        .expect("nested reserved propagates as Ok(Denied)");
    assert!(
        matches!(result, Materialization::Denied { error_code, .. } if error_code == INSUFFICIENT_PERMISSIONS_V1),
        "expected Materialization::Denied, got {result:?}"
    );
    assert_eq!(
        tr_concrete.call_count(),
        0,
        "nested reserved variant must deny before any tenant-resolver call"
    );
}

#[tokio::test]
async fn combined_with_explicit_groups_inner_short_circuits() {
    // Reserved-via-ExplicitGroups (not TenantDirect) at depth 1:
    //   Combined { scopes: [TenantSubtree(T1), ExplicitGroups(...)] }
    // Pins that `first_reserved_variant` flags `ExplicitGroups`, not only
    // `TenantDirect`. A regression that dropped ExplicitGroups from the
    // reserved set would let the TenantSubtree leg call the resolver here.
    let t1 = Uuid::from_u128(0xC);
    let root = root_tenant(0xC);
    let tr_concrete = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr_concrete.add_descendants(root.id, vec![]);
    let tr: Arc<dyn TenantResolverClient> = tr_concrete.clone();
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let cache = Arc::new(HierarchyCache::new(
        &CacheConfig::default(),
        Arc::new(StubClock::new()) as Arc<dyn crate::domain::clock::Clock>,
        Arc::new(AuthZMetrics::from_global()),
    ));
    let client = HierarchyClient::new(
        Arc::new(SdkHierarchyUpstream::new(
            tr,
            rg,
            Arc::new(AuthZMetrics::from_global()),
        )),
        cache,
    );

    let scope = PermissionScopeType::Combined {
        scopes: vec![
            PermissionScopeType::TenantSubtree { root_tenant_id: t1 },
            PermissionScopeType::ExplicitGroups {
                group_ids: vec![Uuid::from_u128(0xD)],
            },
        ],
    };
    let result = client
        .materialize_scope(&scope, &wildcard_request())
        .await
        .expect("ExplicitGroups inner propagates as Ok(Denied)");
    assert!(
        matches!(result, Materialization::Denied { error_code, .. } if error_code == INSUFFICIENT_PERMISSIONS_V1),
        "expected Materialization::Denied, got {result:?}"
    );
    assert_eq!(
        tr_concrete.call_count(),
        0,
        "Combined with ExplicitGroups inner must deny before any tenant-resolver call"
    );
}

#[tokio::test]
async fn combined_with_only_valid_inner_scopes_unions_ids() {
    // Combined of [TenantSubtree(T1), TenantSubtree(T2)] — both valid;
    // result is Combined { tenant_ids: union, resource_ids: [] }.
    let t1 = Uuid::from_u128(0x10);
    let t2 = Uuid::from_u128(0x20);
    let root1 = root_tenant(0x10);
    let root2 = root_tenant(0x20);
    let tr_concrete = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root1.clone(),
        root2.clone(),
    ]));
    tr_concrete.add_descendants(root1.id, vec![]);
    tr_concrete.add_descendants(root2.id, vec![]);
    let tr: Arc<dyn TenantResolverClient> = tr_concrete.clone();
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let cache = Arc::new(HierarchyCache::new(
        &CacheConfig::default(),
        Arc::new(StubClock::new()) as Arc<dyn crate::domain::clock::Clock>,
        Arc::new(AuthZMetrics::from_global()),
    ));
    let client = HierarchyClient::new(
        Arc::new(SdkHierarchyUpstream::new(
            tr,
            rg,
            Arc::new(AuthZMetrics::from_global()),
        )),
        cache,
    );

    let scope = PermissionScopeType::Combined {
        scopes: vec![
            PermissionScopeType::TenantSubtree { root_tenant_id: t1 },
            PermissionScopeType::TenantSubtree { root_tenant_id: t2 },
        ],
    };
    let result = client
        .materialize_scope(&scope, &wildcard_request())
        .await
        .expect("valid inner scopes union as Combined");
    match result {
        Materialization::Combined {
            tenant_ids,
            resource_ids,
            group_owner_tenant_ids,
        } => {
            assert!(tenant_ids.contains(&t1));
            assert!(tenant_ids.contains(&t2));
            assert!(resource_ids.is_empty());
            // No group sub-scope here, so the group-owning-tenant set is empty.
            assert!(group_owner_tenant_ids.is_empty());
        }
        other => panic!("expected Materialization::Combined, got {other:?}"),
    }
    // Two distinct roots → exactly two resolver calls. Pins against an
    // N+1 regression that would re-query the same root or query extra roots.
    assert_eq!(
        tr_concrete.call_count(),
        2,
        "two distinct roots must produce exactly two resolver calls"
    );
}

// -- TenantSubtreePushdown branching (capability-driven, #12) ---------

fn pushdown_request() -> EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_capabilities(vec![Capability::TenantHierarchy])
        .build()
}

#[tokio::test]
async fn tenant_subtree_with_capability_emits_pushdown_without_resolver() {
    // Empty fake + zero resolver calls: a push-down takes the root from the
    // grant scope and never touches the resolver (the whole win — no RPC, no
    // expansion, no cache).
    let tr_concrete = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![]));
    let tr: Arc<dyn TenantResolverClient> = tr_concrete.clone();
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let cache = Arc::new(HierarchyCache::new(
        &CacheConfig::default(),
        Arc::new(StubClock::new()) as Arc<dyn crate::domain::clock::Clock>,
        Arc::new(AuthZMetrics::from_global()),
    ));
    let client = HierarchyClient::new(
        Arc::new(SdkHierarchyUpstream::new(
            tr,
            rg,
            Arc::new(AuthZMetrics::from_global()),
        )),
        cache,
    );

    let root = Uuid::from_u128(0x7E);
    let result = client
        .materialize_scope(
            &PermissionScopeType::TenantSubtree {
                root_tenant_id: root,
            },
            &pushdown_request(),
        )
        .await
        .expect("push-down materialization");
    match result {
        Materialization::TenantSubtreePushdown {
            root_tenant_id,
            barrier_mode,
            status,
        } => {
            assert_eq!(root_tenant_id, root);
            assert_eq!(barrier_mode, SdkBarrierMode::Respect);
            // None status → empty (no status filter = all statuses). Tenant
            // status is a business concern AM enforces itself, not an
            // authz-scope clamp.
            assert!(
                status.is_empty(),
                "None status must default to no filter, got {status:?}"
            );
        }
        other => panic!("expected TenantSubtreePushdown, got {other:?}"),
    }
    assert_eq!(
        tr_concrete.call_count(),
        0,
        "push-down must not call the tenant resolver"
    );
}

#[tokio::test]
async fn tenant_subtree_without_capability_stays_eager() {
    let client = client_with_tenants(vec![root_tenant(1)]);
    let result = client
        .materialize_scope(
            &PermissionScopeType::TenantSubtree {
                root_tenant_id: Uuid::from_u128(1),
            },
            &wildcard_request(), // no capabilities advertised
        )
        .await
        .expect("eager materialization");
    assert!(
        matches!(result, Materialization::TenantSubtree { .. }),
        "no TenantHierarchy capability → eager expansion, got {result:?}"
    );
}

#[tokio::test]
async fn root_only_mode_ignores_capability_and_emits_tenant_direct() {
    use authz_resolver_sdk::models::{TenantContext, TenantMode};
    let root = Uuid::from_u128(9);
    let request = EvaluationRequestBuilder::default()
        .with_capabilities(vec![Capability::TenantHierarchy])
        .with_tenant_context(Some(TenantContext {
            mode: TenantMode::RootOnly,
            ..Default::default()
        }))
        .build();
    let client = client_with_tenants(vec![]);
    let result = client
        .materialize_scope(
            &PermissionScopeType::TenantSubtree {
                root_tenant_id: root,
            },
            &request,
        )
        .await
        .expect("root-only materialization");
    match result {
        Materialization::TenantDirect { tenant_id } => assert_eq!(tenant_id, root),
        other => panic!("RootOnly must emit TenantDirect even with capability, got {other:?}"),
    }
}

#[tokio::test]
async fn combined_stays_eager_even_with_capability() {
    // Combined aggregates concrete ID lists, so inner tenant scopes must
    // expand eagerly regardless of the advertised capability.
    let root1 = root_tenant(0x10);
    let root2 = root_tenant(0x20);
    let tr_concrete = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root1.clone(),
        root2.clone(),
    ]));
    tr_concrete.add_descendants(root1.id, vec![]);
    tr_concrete.add_descendants(root2.id, vec![]);
    let tr: Arc<dyn TenantResolverClient> = tr_concrete;
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let cache = Arc::new(HierarchyCache::new(
        &CacheConfig::default(),
        Arc::new(StubClock::new()) as Arc<dyn crate::domain::clock::Clock>,
        Arc::new(AuthZMetrics::from_global()),
    ));
    let client = HierarchyClient::new(
        Arc::new(SdkHierarchyUpstream::new(
            tr,
            rg,
            Arc::new(AuthZMetrics::from_global()),
        )),
        cache,
    );

    let scope = PermissionScopeType::Combined {
        scopes: vec![
            PermissionScopeType::TenantSubtree {
                root_tenant_id: root1.id.0,
            },
            PermissionScopeType::TenantSubtree {
                root_tenant_id: root2.id.0,
            },
        ],
    };
    let result = client
        .materialize_scope(&scope, &pushdown_request())
        .await
        .expect("combined materialization");
    assert!(
        matches!(result, Materialization::Combined { .. }),
        "Combined must stay eager (no push-down inside), got {result:?}"
    );
}

// Suppress unused-import warnings for the dummy TenantRef import used
// only when manually constructing descendants in other tests.
#[allow(dead_code)]
fn _touch_tenant_ref() -> Option<TenantRef> {
    None
}
