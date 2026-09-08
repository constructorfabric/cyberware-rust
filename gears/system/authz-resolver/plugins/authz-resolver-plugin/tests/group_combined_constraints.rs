#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! End-to-end tests for group + combined constraints,
//! `unsupported_property.v1`, `expansion_infeasible.v1`, and reserved-variant
//! denies (`TenantDirect`, `ExplicitGroups`, Combined-with-reserved-inner).

mod common;

use std::sync::Arc;

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::EvaluationRequestBuilder;
use authz_resolver_sdk::constraints::Predicate;
use rbac_sdk::models::PermissionScopeType;
use resource_group_sdk::models::ResourceGroupMembership;
use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantRef, TenantStatus};
use toolkit::Gear;
use uuid::Uuid;

use common::{
    InMemoryRbacServiceClient, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
};

const OWNER_TENANT_ID: &str = "owner_tenant_id";
const RESOURCE_ID: &str = "id";
const INSUFFICIENT_PERMISSIONS_V1: &str =
    "gts.cf.core.errors.err.v1~cf.authz.errors.insufficient_permissions.v1";
const UNSUPPORTED_PROPERTY_V1: &str =
    "gts.cf.core.errors.err.v1~cf.authz.errors.unsupported_property.v1";
const EXPANSION_INFEASIBLE_V1: &str =
    "gts.cf.core.errors.err.v1~cf.authz.errors.expansion_infeasible.v1";

fn tenant_info(id: u128) -> TenantInfo {
    TenantInfo {
        id: TenantId(Uuid::from_u128(id)),
        name: format!("tenant-{id:x}"),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn tenant_ref(id: u128, parent: Uuid) -> TenantRef {
    TenantRef {
        id: TenantId(Uuid::from_u128(id)),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: Some(TenantId(parent)),
        self_managed: false,
    }
}

fn membership(group_id: Uuid, resource_id: Uuid) -> ResourceGroupMembership {
    ResourceGroupMembership {
        group_id,
        resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
        resource_id: resource_id.to_string(),
    }
}

/// A depth-0 group (the root itself) carrying its owning tenant. `get_group_descendants`
/// is `depth >= 0`, so a real RG returns the root in the page; seeding it here lets the
/// plugin capture the owning tenant for the tenant-paired group constraint.
fn group_at_depth0(
    group_id: Uuid,
    tenant_id: Uuid,
) -> resource_group_sdk::models::ResourceGroupWithDepth {
    resource_group_sdk::models::ResourceGroupWithDepth {
        id: group_id,
        code: "gts.cf.core.rg.type.v1~test.v1~".to_owned(),
        name: format!("group-{group_id:x}"),
        hierarchy: resource_group_sdk::models::GroupHierarchyWithDepth {
            parent_id: None,
            tenant_id,
            depth: 0,
        },
        metadata: None,
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

async fn init_plugin_with_max(
    rbac: Arc<InMemoryRbacServiceClient>,
    tr: Arc<InMemoryTenantResolverClient>,
    rg: Arc<InMemoryResourceGroupClient>,
    max_expansion_ids: usize,
) -> Arc<dyn authz_resolver_sdk::AuthZResolverPluginClient> {
    let (ctx, hub, _registry, _rbac, _tr, _rg) = common::build_ctx_with_config(
        rbac,
        tr,
        rg,
        common::CtxOverrides {
            max_expansion_ids: Some(max_expansion_ids),
            ..Default::default()
        },
    );
    AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect("init should succeed");
    common::resolve_plugin(&hub)
}

#[tokio::test]
async fn t_05_group_subtree_emits_in_on_id() {
    let g1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_0501);
    let resources = vec![
        Uuid::from_u128(0x5101),
        Uuid::from_u128(0x5102),
        Uuid::from_u128(0x5103),
    ];

    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![g1],
        },
    ));
    let group_tenant = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_0509);
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    // get_group_descendants is depth>=0 — seed the root group itself (depth 0)
    // with its owning tenant so the plugin can tenant-pair the group constraint.
    rg.add_group_descendants(g1, vec![group_at_depth0(g1, group_tenant)]);
    rg.add_memberships(resources.iter().map(|r| membership(g1, *r)).collect());

    let plugin = init_plugin(rbac, tr, rg).await;
    // owner_tenant_id is now mandatory for group scopes (tenant-paired constraint).
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![RESOURCE_ID.to_owned(), OWNER_TENANT_ID.to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("GroupSubtree allow -> Ok(decision=true)");

    assert!(response.decision);
    // One constraint with two AND'd predicates: In(id, …) + owner_tenant_id.
    assert_eq!(response.context.constraints.len(), 1);
    let predicates = &response.context.constraints[0].predicates;
    assert_eq!(
        predicates.len(),
        2,
        "group constraint must be tenant-paired"
    );
    let in_pred = predicates
        .iter()
        .find_map(|p| match p {
            Predicate::In(ip) if ip.property == RESOURCE_ID => Some(ip),
            _ => None,
        })
        .expect("expected In(id, ...)");
    assert_eq!(in_pred.values.len(), 3);
    for r in &resources {
        assert!(in_pred.values.contains(&serde_json::json!(r)));
    }
    let tenant_paired = predicates.iter().any(|p| matches!(p,
        Predicate::Eq(e) if e.property == OWNER_TENANT_ID && e.value == serde_json::json!(group_tenant)));
    assert!(
        tenant_paired,
        "group constraint must pair the group's owning tenant"
    );
}

#[tokio::test]
async fn t_07_combined_tenant_and_group_emits_two_or_d_constraints() {
    let root_id = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_0701);
    let root = tenant_info(0x701);
    let descendants = vec![tenant_ref(0x702, Uuid::from_u128(0x701))];
    let g1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_0711);
    let resources = [Uuid::from_u128(0x7101), Uuid::from_u128(0x7102)];

    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: root_id,
                },
                PermissionScopeType::GroupSubtree {
                    root_group_ids: vec![g1],
                },
            ],
        },
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    // Group g1 is owned by the DESCENDANT tenant 0x702, not by the granted root
    // (groups are tenant-scoped). Owning it at the root would make this test
    // blind: the group side would then pair the same tenant the tenant-subtree
    // leg carries, so pairing with the request's tenants instead of the group's
    // owner — the cross-tenant leak — would look identical.
    let group_tenant = Uuid::from_u128(0x702);
    rg.add_group_descendants(g1, vec![group_at_depth0(g1, group_tenant)]);
    rg.add_memberships(resources.iter().map(|r| membership(g1, *r)).collect());

    let plugin = init_plugin(rbac, tr, rg).await;
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![OWNER_TENANT_ID.to_owned(), RESOURCE_ID.to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("Combined allow -> Ok(decision=true)");

    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 2);
    // First constraint: the OR'd tenant side — In(owner_tenant_id, [root, descendant]).
    match &response.context.constraints[0].predicates[0] {
        Predicate::In(in_pred) => {
            assert_eq!(in_pred.property, OWNER_TENANT_ID);
            assert_eq!(in_pred.values.len(), 2);
        }
        other => panic!("expected In on tenant, got {other:?}"),
    }
    // Second constraint: the tenant-PAIRED group side — In(id, [resources]) AND owner_tenant_id.
    let group_predicates = &response.context.constraints[1].predicates;
    assert_eq!(
        group_predicates.len(),
        2,
        "group side must be tenant-paired"
    );
    let in_pred = group_predicates
        .iter()
        .find_map(|p| match p {
            Predicate::In(ip) if ip.property == RESOURCE_ID => Some(ip),
            _ => None,
        })
        .expect("expected In(id, ...) on group side");
    assert_eq!(in_pred.values.len(), 2);
    // Assert the VALUE, not just the property: the group side must carry the
    // group's owning tenant alone, never the request's tenant list.
    let tenant_paired = group_predicates.iter().any(|p| match p {
        Predicate::Eq(e) => {
            e.property == OWNER_TENANT_ID && e.value == serde_json::json!(group_tenant)
        }
        Predicate::In(i) => {
            i.property == OWNER_TENANT_ID && i.values == vec![serde_json::json!(group_tenant)]
        }
        _ => false,
    });
    assert!(
        tenant_paired,
        "group side must AND-pair owner_tenant_id = the GROUP's tenant \
         (got {group_predicates:?})"
    );
}

#[tokio::test]
async fn t_12_unsupported_property_returns_unsupported_property_deny() {
    // Tenant-scoped allow but PEP declares supported_properties = ["id"]
    // only — the generated predicate uses owner_tenant_id which isn't
    // declared, so the plugin denies with unsupported_property.v1.
    let root_id = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_1201);
    let root = tenant_info(0x1201);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(rbac, tr, Arc::new(InMemoryResourceGroupClient::default())).await;

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![RESOURCE_ID.to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("unsupported_property is Ok(decision=false)");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, UNSUPPORTED_PROPERTY_V1);
}

#[tokio::test]
async fn t_14_expansion_infeasible_returns_deny() {
    // Tight config: max_expansion_ids = 2; tenant resolver returns root +
    // 2 descendants = 3 tenants → strictly greater than 2 → deny.
    let root_id = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_1401);
    let root = tenant_info(0x1401);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(
        root.id,
        vec![
            tenant_ref(0x1402, Uuid::from_u128(0x1401)),
            tenant_ref(0x1403, Uuid::from_u128(0x1401)),
        ],
    );
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));

    let plugin = init_plugin_with_max(
        rbac,
        tr,
        Arc::new(InMemoryResourceGroupClient::default()),
        2,
    )
    .await;

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![OWNER_TENANT_ID.to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("expansion_infeasible is Ok(decision=false)");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, EXPANSION_INFEASIBLE_V1);
}

#[tokio::test]
async fn tenant_direct_reserved_returns_insufficient_permissions_deny() {
    let t1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_2001);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::TenantDirect { tenant_id: t1 },
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![OWNER_TENANT_ID.to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("reserved variant is Ok(decision=false), not Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
    // Sole reserved scope → deny before any tenant-resolver call (fail-closed).
    assert_eq!(
        tr.call_count(),
        0,
        "reserved TenantDirect must deny before querying the tenant resolver"
    );
}

#[tokio::test]
async fn explicit_groups_reserved_returns_insufficient_permissions_deny() {
    let g1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_2101);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::ExplicitGroups {
            group_ids: vec![g1],
        },
    ));
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    let plugin = init_plugin(
        rbac,
        Arc::new(InMemoryTenantResolverClient::default()),
        Arc::clone(&rg),
    )
    .await;

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![OWNER_TENANT_ID.to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("reserved variant is Ok(decision=false), not Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
    // RG resolver MUST NOT be invoked for the reserved ExplicitGroups path —
    // materialize_scope short-circuits to Denied before any RG call.
    assert_eq!(
        rg.call_count(),
        0,
        "ExplicitGroups must not invoke the RG resolver"
    );
}

#[tokio::test]
async fn combined_with_reserved_inner_variant_denies() {
    // Combined of [TenantSubtree(T1), TenantDirect(T2)] — the second inner
    // is reserved. The whole Combined materialization short-circuits to
    // Denied (fail-closed); the legitimate sub-scope's tenant_ids are
    // NOT carried in the response.
    let t1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_3001);
    let t2 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_3002);
    let root = tenant_info(0x3001);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);

    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::TenantSubtree { root_tenant_id: t1 },
                PermissionScopeType::TenantDirect { tenant_id: t2 },
            ],
        },
    ));
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![OWNER_TENANT_ID.to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("Combined with reserved inner is Ok(decision=false), not Err");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
    assert!(response.context.constraints.is_empty());
    // The reserved inner variant makes the whole Combined short-circuit to
    // Denied during materialization — the legitimate TenantSubtree(T1) sub-scope
    // must NOT trigger a tenant-resolver query (fail-closed, no partial work).
    assert_eq!(
        tr.call_count(),
        0,
        "Combined with a reserved inner variant must deny before querying the tenant resolver"
    );
}
