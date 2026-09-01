#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::field_reassign_with_default,
    clippy::similar_names
)]
//! End-to-end tenant-constraint-generation tests. Verifies that tenant-scoped
//! allow paths produce real `Ok(decision=true)` responses with the right
//! predicate shape (`Eq` for direct/RootOnly and single-tenant subtrees,
//! `In` for multi-tenant subtrees). Group and Combined scopes now also
//! produce real responses (group `In(id, ...)`, combined OR'd constraints).

mod common;

use std::sync::Arc;

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::EvaluationRequestBuilder;
use authz_resolver_sdk::constraints::Predicate;
use authz_resolver_sdk::models::{BarrierMode, Capability, TenantContext, TenantMode};
use rbac_sdk::models::PermissionScopeType;
use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantRef, TenantStatus};
use toolkit::Gear;
use uuid::Uuid;

use common::{
    InMemoryRbacServiceClient, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
};

const OWNER_TENANT_ID: &str = "owner_tenant_id";

fn tenant_info(id: u128, status: TenantStatus) -> TenantInfo {
    TenantInfo {
        id: TenantId(Uuid::from_u128(id)),
        name: format!("tenant-{id:x}"),
        status,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn tenant_ref(id: u128, status: TenantStatus) -> TenantRef {
    TenantRef {
        id: TenantId(Uuid::from_u128(id)),
        status,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

/// A depth-0 group (the root) carrying its owning tenant. `get_group_descendants`
/// is `depth >= 0` and returns the root group itself; seeding it lets the plugin
/// build the tenant-paired group constraint (`RESOURCE_GROUP_MODEL.md`).
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

/// A `TenantRef` with an explicit parent and self-managed flag — lets the
/// resolver fake's barrier logic prune self-managed subtrees in Respect mode.
fn tenant_ref_full(
    id: u128,
    status: TenantStatus,
    parent: Option<u128>,
    self_managed: bool,
) -> TenantRef {
    TenantRef {
        id: TenantId(Uuid::from_u128(id)),
        status,
        tenant_type: None,
        parent_id: parent.map(|p| TenantId(Uuid::from_u128(p))),
        self_managed,
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

fn single_predicate(response: &authz_resolver_sdk::EvaluationResponse) -> &Predicate {
    assert_eq!(response.context.constraints.len(), 1);
    assert_eq!(response.context.constraints[0].predicates.len(), 1);
    &response.context.constraints[0].predicates[0]
}

fn assert_eq_predicate(predicate: &Predicate, expected_value: Uuid) {
    match predicate {
        Predicate::Eq(eq) => {
            assert_eq!(eq.property, OWNER_TENANT_ID);
            assert_eq!(eq.value, serde_json::json!(expected_value));
        }
        other => panic!("expected Predicate::Eq, got {other:?}"),
    }
}

fn assert_in_predicate(predicate: &Predicate, expected_values: &[Uuid]) {
    match predicate {
        Predicate::In(in_pred) => {
            assert_eq!(in_pred.property, OWNER_TENANT_ID);
            assert_eq!(
                in_pred.values.len(),
                expected_values.len(),
                "predicate values length mismatch"
            );
            for v in expected_values {
                assert!(
                    in_pred.values.contains(&serde_json::json!(v)),
                    "values must contain {v}"
                );
            }
        }
        other => panic!("expected Predicate::In, got {other:?}"),
    }
}

fn wildcard_request() -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build()
}

fn wildcard_request_with_mode(mode: TenantMode) -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .with_tenant_context(Some(TenantContext {
            mode,
            root_id: None,
            barrier_mode: BarrierMode::Respect,
            tenant_status: None,
        }))
        .build()
}

// ---------- Scope × mode matrix ----------

#[tokio::test]
async fn t_01_global_subtree_emits_in_with_all_tenants() {
    let root = tenant_info(0x100, TenantStatus::Active);
    let descendants = vec![
        tenant_ref(0x101, TenantStatus::Active),
        tenant_ref(0x102, TenantStatus::Active),
    ];
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let plugin = init_plugin(rbac, tr, Arc::new(InMemoryResourceGroupClient::default())).await;

    let response = plugin
        .evaluate(wildcard_request())
        .await
        .expect("Global + Subtree allow");
    assert!(response.decision);
    let predicate = single_predicate(&response);
    assert_in_predicate(
        predicate,
        &[
            Uuid::from_u128(0x100),
            Uuid::from_u128(0x101),
            Uuid::from_u128(0x102),
        ],
    );
}

#[tokio::test]
async fn t_02_global_root_only_emits_eq_with_one_resolver_call() {
    let root = tenant_info(0x200, TenantStatus::Active);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let response = plugin
        .evaluate(wildcard_request_with_mode(TenantMode::RootOnly))
        .await
        .expect("Global + RootOnly allow");
    assert!(response.decision);
    assert_eq_predicate(single_predicate(&response), Uuid::from_u128(0x200));
    // Only get_root_tenant; get_descendants must NOT be called.
    assert_eq!(
        tr.call_count(),
        1,
        "Global + RootOnly should make exactly one tenant-resolver call (get_root_tenant)"
    );
}

#[tokio::test]
async fn t_03_tenant_subtree_emits_in_with_descendants() {
    let root_id = Uuid::from_u128(0x300);
    let root = tenant_info(0x300, TenantStatus::Active);
    let descendants = vec![
        tenant_ref(0x301, TenantStatus::Active),
        tenant_ref(0x302, TenantStatus::Active),
    ];
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(rbac, tr, Arc::new(InMemoryResourceGroupClient::default())).await;

    let response = plugin
        .evaluate(wildcard_request())
        .await
        .expect("TenantSubtree + Subtree allow");
    assert!(response.decision);
    assert_in_predicate(
        single_predicate(&response),
        &[
            Uuid::from_u128(0x300),
            Uuid::from_u128(0x301),
            Uuid::from_u128(0x302),
        ],
    );
}

#[tokio::test]
async fn tenant_subtree_with_capability_pushes_down_in_tenant_subtree() {
    // End-to-end (#12): RBAC grants TenantSubtree(root) and the PEP advertises
    // `Capability::TenantHierarchy`. The plugin must return decision=true with
    // an `InTenantSubtree` push-down predicate and NOT expand the subtree via
    // the tenant resolver (zero resolver calls — the whole win).
    let root_id = Uuid::from_u128(0x500);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        tenant_info(0x500, TenantStatus::Active),
    ]));
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(
        rbac,
        tr.clone(),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![OWNER_TENANT_ID.to_owned()])
        .with_capabilities(vec![Capability::TenantHierarchy])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("TenantSubtree + TenantHierarchy push-down allow");

    assert!(response.decision);
    match single_predicate(&response) {
        Predicate::InTenantSubtree(p) => {
            assert_eq!(p.property, OWNER_TENANT_ID);
            assert_eq!(p.root_tenant_id, serde_json::json!(root_id));
        }
        other => panic!("expected InTenantSubtree push-down, got {other:?}"),
    }
    assert_eq!(
        tr.call_count(),
        0,
        "push-down must not expand the subtree via the tenant resolver"
    );
}

#[tokio::test]
async fn t_04_tenant_subtree_root_only_emits_eq_no_resolver_call() {
    let root_id = Uuid::from_u128(0x400);
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let response = plugin
        .evaluate(wildcard_request_with_mode(TenantMode::RootOnly))
        .await
        .expect("TenantSubtree + RootOnly allow");
    assert!(response.decision);
    assert_eq_predicate(single_predicate(&response), root_id);
    assert_eq!(
        tr.call_count(),
        0,
        "TenantSubtree + RootOnly must NOT call tenant resolver (RootOnly short-circuits)"
    );
}

// ---------- Barrier-aware ----------

#[tokio::test]
async fn t_20_barrier_respect_excludes_self_managed_subtree() {
    // root → A (self_managed barrier) → A1. The plugin passes
    // barrier_mode=Respect (the default) to the resolver, and the fake now
    // prunes A and everything beneath it — so the subtree is [root] only.
    let root_id = Uuid::from_u128(0x500);
    let root = tenant_info(0x500, TenantStatus::Active);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(
        root.id,
        vec![
            tenant_ref_full(0x501, TenantStatus::Active, Some(0x500), true), // A: barrier
            tenant_ref_full(0x502, TenantStatus::Active, Some(0x501), false), // A1: behind A
        ],
    );
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(rbac, tr, Arc::new(InMemoryResourceGroupClient::default())).await;

    // wildcard_request() omits tenant_context → defaults to mode=Subtree,
    // barrier_mode=Respect.
    let response = plugin
        .evaluate(wildcard_request())
        .await
        .expect("barrier-respect allow");
    assert!(response.decision);
    // Only root survives the barrier → single-tenant Eq.
    assert_eq_predicate(single_predicate(&response), root_id);
}

#[tokio::test]
async fn t_21_barrier_ignore_includes_self_managed_subtree() {
    // Same root → A (self_managed) → A1 tree as t_20, but barrier_mode=Ignore
    // makes the resolver fake traverse through the self-managed barrier, so
    // the full subtree is returned.
    let root_id = Uuid::from_u128(0x600);
    let root = tenant_info(0x600, TenantStatus::Active);
    let descendants = vec![
        tenant_ref_full(0x601, TenantStatus::Active, Some(0x600), true), // A: barrier
        tenant_ref_full(0x602, TenantStatus::Active, Some(0x601), false), // A1: behind A
    ];
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(rbac, tr, Arc::new(InMemoryResourceGroupClient::default())).await;

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .with_tenant_context(Some(TenantContext {
            mode: TenantMode::Subtree,
            root_id: None,
            barrier_mode: BarrierMode::Ignore,
            tenant_status: None,
        }))
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("barrier-ignore allow");
    assert!(response.decision);
    assert_in_predicate(
        single_predicate(&response),
        &[
            Uuid::from_u128(0x600),
            Uuid::from_u128(0x601),
            Uuid::from_u128(0x602),
        ],
    );
}

// ---------- tenant_status filtering ----------

#[tokio::test]
async fn t_22_default_status_no_filter_includes_suspended_descendants() {
    // tenant_status absent → NO descendant status filter: the
    // resolver receives an empty status list and a Suspended descendant is
    // INCLUDED. Descendant status is a business concern AM enforces itself, not
    // an authz-scope clamp. The subtree has an Active child (0x701) and a
    // Suspended child (0x702); both appear in the constraint.
    let root_id = Uuid::from_u128(0x700);
    let active_child = Uuid::from_u128(0x701);
    let suspended_child = Uuid::from_u128(0x702);
    let root = tenant_info(0x700, TenantStatus::Active);
    let descendants = vec![
        tenant_ref(0x701, TenantStatus::Active),
        tenant_ref(0x702, TenantStatus::Suspended),
    ];
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let response = plugin
        .evaluate(wildcard_request())
        .await
        .expect("default status allow");
    assert!(response.decision);

    // Default → NO status filter: the resolver receives an empty status list.
    let captured = tr
        .last_get_descendants_request()
        .expect("resolver was called");
    assert!(
        captured.options.status.is_empty(),
        "default tenant_status must be no-filter (empty), got {:?}",
        captured.options.status
    );
    // Root + BOTH children (Active and Suspended) appear — descendants are not
    // status-clamped; the caller filters status per-op.
    assert_in_predicate(
        single_predicate(&response),
        &[root_id, active_child, suspended_child],
    );
}

#[tokio::test]
async fn t_23_explicit_status_includes_suspended_tenants() {
    let root_id = Uuid::from_u128(0x800);
    let root = tenant_info(0x800, TenantStatus::Active);
    let descendants = vec![
        tenant_ref(0x801, TenantStatus::Active),
        tenant_ref(0x802, TenantStatus::Suspended),
    ];
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
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
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .with_tenant_context(Some(TenantContext {
            mode: TenantMode::Subtree,
            root_id: None,
            barrier_mode: BarrierMode::Respect,
            tenant_status: Some(vec!["active".to_owned(), "suspended".to_owned()]),
        }))
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("explicit status allow");
    assert!(response.decision);
    assert_in_predicate(
        single_predicate(&response),
        &[
            Uuid::from_u128(0x800),
            Uuid::from_u128(0x801),
            Uuid::from_u128(0x802),
        ],
    );
    let captured = tr.last_get_descendants_request().expect("resolver called");
    assert!(captured.options.status.contains(&TenantStatus::Active));
    assert!(captured.options.status.contains(&TenantStatus::Suspended));
}

#[tokio::test]
async fn suspended_root_excluded_from_active_subtree() {
    // SECURITY (§3.6): a granted root that is Suspended must NOT appear in the
    // materialized subtree. The resolver returns the root regardless of status
    // (it only status-filters descendants), so the plugin applies the [Active]
    // root clamp itself. Descendants are unfiltered but here happen to
    // be active, so only the two active descendants survive.
    let root_id = Uuid::from_u128(0x880);
    let root = tenant_info(0x880, TenantStatus::Suspended);
    let descendants = vec![
        tenant_ref(0x881, TenantStatus::Active),
        tenant_ref(0x882, TenantStatus::Active),
    ];
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: root_id,
        },
    ));
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;
    // Default tenant_status → [Active]; no explicit tenant_context.
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("active-descendant subtree allow");
    assert!(response.decision);
    // Only the two active descendants — the suspended root 0x880 is excluded.
    assert_in_predicate(
        single_predicate(&response),
        &[Uuid::from_u128(0x881), Uuid::from_u128(0x882)],
    );
}

#[tokio::test]
async fn suspended_root_with_no_active_descendants_denies() {
    // The grant resolves to zero active tenants (suspended root, no matching
    // descendants) → fail-closed deny rather than an empty-In "allow".
    let root_id = Uuid::from_u128(0x890);
    let root = tenant_info(0x890, TenantStatus::Suspended);
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
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("evaluate returns Ok(business-deny)");
    assert!(!response.decision, "empty subtree must fail closed (deny)");
    let code = response
        .context
        .deny_reason
        .expect("deny carries a reason")
        .error_code;
    assert!(
        code.contains("insufficient_permissions"),
        "expected insufficient_permissions deny, got {code}"
    );
}

// ---------- Group / Combined now produce real responses ----------

#[tokio::test]
async fn group_subtree_emits_in_constraint_on_id() {
    // GroupSubtree produces a single `In(id, [resources])` constraint.
    let group_id = Uuid::from_u128(0x900);
    let resource_id = Uuid::from_u128(0x901);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![group_id],
        },
    ));
    let group_tenant = Uuid::from_u128(0x909);
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    // depth>=0: seed the root group itself with its owning tenant.
    rg.add_group_descendants(group_id, vec![group_at_depth0(group_id, group_tenant)]);
    rg.add_memberships(vec![resource_group_sdk::models::ResourceGroupMembership {
        group_id,
        resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
        resource_id: resource_id.to_string(),
    }]);

    let plugin = init_plugin(rbac, tr, rg).await;
    // Group constraints are now tenant-paired → owner_tenant_id required.
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["id".to_owned(), "owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("GroupSubtree allow -> Ok(decision=true)");
    assert!(response.decision);
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
            Predicate::In(ip) if ip.property == "id" => Some(ip),
            _ => None,
        })
        .expect("expected In(id, ...)");
    assert_eq!(in_pred.values.len(), 1);
    assert!(in_pred.values.contains(&serde_json::json!(resource_id)));
    assert!(
        predicates.iter().any(|p| matches!(p,
            Predicate::Eq(e) if e.property == OWNER_TENANT_ID && e.value == serde_json::json!(group_tenant))),
        "group constraint must pair the group's owning tenant"
    );
}

#[tokio::test]
async fn combined_emits_or_d_constraints() {
    // Combined of (TenantSubtree, GroupSubtree) → two OR'd constraints.
    let root_id = Uuid::from_u128(0xA00);
    let root = tenant_info(0xA00, TenantStatus::Active);
    let descendants = vec![tenant_ref(0xA01, TenantStatus::Active)];
    let group_id = Uuid::from_u128(0xA10);
    let resource_id = Uuid::from_u128(0xA11);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: root_id,
                },
                PermissionScopeType::GroupSubtree {
                    root_group_ids: vec![group_id],
                },
            ],
        },
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, descendants);
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    // Group is owned by the root tenant (groups are tenant-scoped).
    rg.add_group_descendants(group_id, vec![group_at_depth0(group_id, root_id)]);
    rg.add_memberships(vec![resource_group_sdk::models::ResourceGroupMembership {
        group_id,
        resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
        resource_id: resource_id.to_string(),
    }]);

    let plugin = init_plugin(rbac, tr, rg).await;
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned(), "id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("Combined allow -> Ok(decision=true)");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 2);
    // First constraint: tenant — In(owner_tenant_id, [root, A01]) (2 tenants → In)
    match &response.context.constraints[0].predicates[0] {
        Predicate::In(in_pred) => {
            assert_eq!(in_pred.property, OWNER_TENANT_ID);
            assert_eq!(in_pred.values.len(), 2);
        }
        other => panic!("expected In on tenant, got {other:?}"),
    }
    // Second constraint: group — In(id, [resource])
    match &response.context.constraints[1].predicates[0] {
        Predicate::In(in_pred) => {
            assert_eq!(in_pred.property, "id");
            assert_eq!(in_pred.values.len(), 1);
        }
        other => panic!("expected In on id, got {other:?}"),
    }
}

// ---------- Multi-tenant isolation ----------

#[tokio::test]
async fn cross_tenant_isolation_constraints_exclude_other_tenant() {
    // Headline PDP guarantee. Two INDEPENDENT subtrees exist in the resolver:
    // A→A1 and B→B1, each with its own per-root descendant list. A subject
    // scoped to tenant-A's subtree must (a) cause the plugin to query A's root
    // specifically, and (b) receive constraints covering only A + A1 — B's
    // subtree must never be consulted or leak in. Because the fake returns
    // per-root descendants, a bug that queried the wrong root or unioned
    // subtrees would surface B's ids here and fail the test.
    let tenant_a = Uuid::from_u128(0xA00);
    let tenant_a_child = Uuid::from_u128(0xA01);
    let root_b = Uuid::from_u128(0xB00);
    let tenant_b_child = Uuid::from_u128(0xB01);

    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        tenant_info(0xA00, TenantStatus::Active),
        tenant_info(0xB00, TenantStatus::Active),
    ]));
    tr.add_descendants(
        TenantId(tenant_a),
        vec![tenant_ref(0xA01, TenantStatus::Active)],
    );
    // B has its own non-empty subtree the fake WILL return when B's root is
    // queried — so "B absent" is a real property, not a structural certainty.
    tr.add_descendants(
        TenantId(root_b),
        vec![tenant_ref(0xB01, TenantStatus::Active)],
    );

    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: tenant_a,
        },
    ));
    let plugin = init_plugin(
        rbac,
        Arc::clone(&tr),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let response = plugin
        .evaluate(wildcard_request())
        .await
        .expect("tenant-A subtree allow");
    assert!(response.decision);

    // (a) The plugin scoped the hierarchy query to A's root (from the RBAC
    // grant) — not B's. This is the teeth: it proves isolation by construction.
    let captured = tr
        .last_get_descendants_request()
        .expect("plugin must query the tenant subtree");
    assert_eq!(
        captured.id,
        TenantId(tenant_a),
        "plugin must query tenant-A's root, not another tenant's"
    );

    // (b) Constraints are exactly A + A1, and neither B nor B1 leaks in.
    let predicate = single_predicate(&response);
    assert_in_predicate(predicate, &[tenant_a, tenant_a_child]);
    match predicate {
        Predicate::In(in_pred) => {
            for leaked in [root_b, tenant_b_child] {
                assert!(
                    !in_pred.values.contains(&serde_json::json!(leaked)),
                    "tenant-B subtree id {leaked} leaked into tenant-A constraints"
                );
            }
        }
        other => panic!("expected Predicate::In, got {other:?}"),
    }
}
