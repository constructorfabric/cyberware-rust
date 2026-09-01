#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::field_reassign_with_default,
    clippy::needless_pass_by_value
)]
use super::*;
use crate::domain::deny::error_codes::{
    CONSTRAINTS_UNAVAILABLE_V1, EXPANSION_INFEASIBLE_V1, INSUFFICIENT_PERMISSIONS_V1,
    INVALID_REQUEST_V1, SCOPE_MISMATCH_V1, UNSUPPORTED_PROPERTY_V1,
};
use crate::test_support::{
    EvaluationRequestBuilder, InMemoryRbacServiceClient, InMemoryResourceGroupClient,
    InMemoryTenantResolverClient,
};
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    DenyReason as RbacDenyReason, EffectivePermission, PermissionRule, PermissionScopeType, Scope,
};
use resource_group_sdk::models::{
    GroupHierarchyWithDepth, ResourceGroupMembership, ResourceGroupWithDepth,
};
use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantRef, TenantStatus};
use uuid::Uuid;

/// Default known types for in-file tests so the GTS type validator
/// (Warn mode by default) doesn't deny on the default request shape.
/// Tests that exercise the validator's failure paths build their own
/// registry with `RecordingTypesRegistry::new()` or `set_unavailable`.
fn default_registry() -> Arc<crate::test_support::RecordingTypesRegistry> {
    use crate::test_support::EvaluationRequestBuilder;
    Arc::new(
        crate::test_support::RecordingTypesRegistry::with_known_types(vec![
            EvaluationRequestBuilder::default()
                .build()
                .subject
                .subject_type
                .as_deref()
                .unwrap(),
            "gts.cf.core.resources.test.v1~",
        ]),
    )
}

/// Build a plugin wired with the supplied RBAC fake plus default
/// tenant / resource-group fakes. Used by tests that don't reach the
/// post-policy materialization step (validation deny, scope deny,
/// policy deny, RBAC error).
fn plugin_with_rbac(rbac: Arc<InMemoryRbacServiceClient>) -> AuthZResolverPlugin {
    plugin_with_resolvers(
        rbac,
        Arc::new(InMemoryTenantResolverClient::default()),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
}

/// Build a plugin wired with caller-supplied RBAC + tenant + RG fakes.
/// Used by tests that reach `materialize_scope` (policy allow path).
fn plugin_with_resolvers(
    rbac: Arc<InMemoryRbacServiceClient>,
    tenant_resolver: Arc<InMemoryTenantResolverClient>,
    resource_group: Arc<InMemoryResourceGroupClient>,
) -> AuthZResolverPlugin {
    let config = Arc::new(AuthZResolverPluginConfig {
        // The trusted-actor bypass is configuration, so a test that exercises
        // it has to ask for it exactly like a deployment would.
        trusted_system_actors: vec![crate::config::TrustedSystemActor {
            subject_type: crate::test_support::trusted_actors::AM_SYSTEM_SUBJECT_TYPE.to_owned(),
            subject_id: crate::test_support::trusted_actors::AM_SYSTEM_ACTOR_UUID,
        }],
        vendor: "cf".to_owned(),
        ..AuthZResolverPluginConfig::default()
    });
    AuthZResolverPlugin::new(
        config,
        rbac,
        tenant_resolver,
        resource_group,
        default_registry(),
    )
}

fn root_tenant() -> TenantInfo {
    TenantInfo {
        id: TenantId(Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_0001)),
        name: "root".to_owned(),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn assert_scope_provenance_error(error: AuthZResolverError) {
    match error {
        AuthZResolverError::Internal(message) => assert_eq!(
            message,
            crate::domain::deny::service_errors::RBAC_SCOPE_PROVENANCE_INVALID
        ),
        other => panic!("expected RBAC scope-provenance Internal error, got {other:?}"),
    }
}

fn scoped_grant(scope: Scope) -> EffectivePermission {
    EffectivePermission::new(
        PermissionRule::new("read", "gts.cf.core.resources.test.v1~"),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "Scoped Reader",
        scope,
        false,
    )
}

/// Build a valid two-assignment aggregate whose tenant and group legs both
/// resolve to empty runtime sets: the tenant root is suspended with no
/// descendants, and the group has no resource memberships.
fn valid_combined_allow_with_empty_materialization() -> (
    Arc<InMemoryRbacServiceClient>,
    Arc<InMemoryTenantResolverClient>,
    Arc<InMemoryResourceGroupClient>,
) {
    let tenant_id = Uuid::from_u128(0xEC01);
    let group_id = Uuid::from_u128(0xEC02);
    let grants = vec![
        scoped_grant(Scope::tenant(tenant_id)),
        scoped_grant(Scope::resource_group(tenant_id, group_id)),
    ];
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        grants,
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: tenant_id,
                },
                PermissionScopeType::GroupSubtree {
                    root_group_ids: vec![group_id],
                },
            ],
        },
    ));
    let tenant = TenantInfo {
        id: TenantId(tenant_id),
        name: "suspended-empty-root".to_owned(),
        status: TenantStatus::Suspended,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    };
    let tenant_resolver = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        tenant.clone(),
    ]));
    tenant_resolver.add_descendants(tenant.id, Vec::new());
    let resource_group = Arc::new(InMemoryResourceGroupClient::with_group_descendants(
        group_id,
        vec![ResourceGroupWithDepth {
            id: group_id,
            code: "gts.cf.core.rg.type.v1~empty.v1~".to_owned(),
            name: "empty-group".to_owned(),
            hierarchy: GroupHierarchyWithDepth {
                parent_id: None,
                tenant_id,
                depth: 0,
            },
            metadata: None,
        }],
    ));

    (rbac, tenant_resolver, resource_group)
}

/// RBAC fake configured to allow against `Global` scope, plus a tenant
/// resolver fake configured with a single root tenant. Sufficient for
/// tests that need `materialize_scope` to succeed without exercising
/// real subtree data.
fn rbac_allowing_with_root_tenant() -> (
    Arc<InMemoryRbacServiceClient>,
    Arc<InMemoryTenantResolverClient>,
) {
    let root = root_tenant();
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    (rbac, tr)
}

#[tokio::test]
async fn evaluate_emits_outcome_metrics_through_wrapper() {
    // End-to-end proof that the trait wrapper records outcome metrics:
    // a third-party token that doesn't cover `delete` denies at the scope
    // step, so `evaluate()` returns Ok(decision=false) and the wrapper
    // must emit deny_total{reason=scope_mismatch} + duration{decision=deny}.
    use crate::infra::metrics::test_harness::MetricsHarness;
    use crate::infra::metrics::{AUTHZ_EVALUATION_DENY, AUTHZ_EVALUATION_DURATION};

    let harness = MetricsHarness::new();
    let metrics = harness.metrics();
    let config = Arc::new(AuthZResolverPluginConfig {
        vendor: "cf".to_owned(),
        ..AuthZResolverPluginConfig::default()
    });
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let plugin = AuthZResolverPlugin::with_metrics(
        config,
        rbac,
        Arc::new(InMemoryTenantResolverClient::default()),
        Arc::new(InMemoryResourceGroupClient::default()),
        default_registry(),
        metrics,
    );

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["read:events".to_owned()])
        .with_action_name("delete")
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("scope deny is Ok(decision=false)");
    assert!(!response.decision, "scope mismatch must deny");

    harness.force_flush();
    assert_eq!(
        harness.counter_value(AUTHZ_EVALUATION_DENY, &[("reason", "scope_mismatch")]),
        1,
        "wrapper must emit deny_total for the scope-mismatch deny"
    );
    assert_eq!(
        harness.histogram_count(AUTHZ_EVALUATION_DURATION, &[("decision", "deny")]),
        1,
        "wrapper must record evaluation duration with decision=deny"
    );
}

#[tokio::test]
async fn forged_global_scope_is_rejected_before_platform_root_materialization() {
    use crate::infra::metrics::test_harness::MetricsHarness;
    use crate::infra::metrics::{
        AUTHZ_EVALUATION_ERROR, AUTHZ_FAIL_CLOSED, AUTHZ_SCOPE_PROVENANCE_REJECTION,
    };

    let tenant_id = Uuid::new_v4();
    let scoped_grant = EffectivePermission::new(
        PermissionRule::new("read", "gts.cf.core.resources.test.v1~"),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "Scoped Reader",
        Scope::tenant(tenant_id),
        false,
    );
    // The aggregate is deliberately forged wider than the only contributing
    // assignment. This reproduces the dangerous producer/consumer boundary:
    // without provenance validation `Global` would resolve the platform root.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![scoped_grant],
        PermissionScopeType::Global,
    ));
    let tenant_resolver = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root_tenant(),
    ]));
    let tenant_resolver_client: Arc<dyn tenant_resolver_sdk::TenantResolverClient> =
        tenant_resolver.clone();
    let harness = MetricsHarness::new();
    let plugin = AuthZResolverPlugin::with_metrics(
        Arc::new(AuthZResolverPluginConfig {
            vendor: "vz".to_owned(),
            ..AuthZResolverPluginConfig::default()
        }),
        rbac,
        tenant_resolver_client,
        Arc::new(InMemoryResourceGroupClient::default()),
        default_registry(),
        harness.metrics(),
    );
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();

    let error = plugin
        .evaluate(request)
        .await
        .expect_err("inconsistent assignment provenance must fail closed");
    assert_scope_provenance_error(error);
    assert_eq!(
        tenant_resolver.call_count(),
        0,
        "a forged Global result must be rejected before querying the platform root"
    );

    harness.force_flush();
    assert_eq!(
        harness.counter_value(AUTHZ_SCOPE_PROVENANCE_REJECTION, &[]),
        1,
        "the fail-closed provenance guard must be operationally visible"
    );
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
}

#[tokio::test]
async fn wildcard_scope_global_emits_tenant_constraint() {
    // Wildcard scope passes; RBAC allow with Global scope; tenant
    // resolver returns root with no descendants. Single-tenant subtree
    // emits Eq (the Eq-if-len==1 rule).
    use crate::domain::constraint_generator::OWNER_TENANT_ID;
    use authz_resolver_sdk::constraints::Predicate;

    let (rbac, tr) = rbac_allowing_with_root_tenant();
    let plugin = plugin_with_resolvers(rbac, tr, Arc::new(InMemoryResourceGroupClient::default()));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin.evaluate(request).await.expect("tenant-scoped allow");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    let predicate = &response.context.constraints[0].predicates[0];
    match predicate {
        Predicate::Eq(eq) => {
            assert_eq!(eq.property, OWNER_TENANT_ID);
        }
        other => panic!("expected Eq predicate for single-tenant, got {other:?}"),
    }
    assert!(response.context.deny_reason.is_none());
}

#[tokio::test]
async fn validation_failure_surfaces_before_scope_check() {
    // Validation runs before scope and policy; the RBAC fake is the
    // default-stub and must never be called for this request. (Absent
    // subject_type is now valid, so trigger the validation failure with a
    // present-but-unrecognized value.)
    let rbac = Arc::new(InMemoryRbacServiceClient::default());
    let plugin = plugin_with_rbac(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_subject_type(Some("bogus-type".to_owned()))
        .build();
    // A malformed request is the caller's fault, so it is a business deny
    // (`invalid_request.v1`), NOT an `Internal` the PEP would retry.
    let response = plugin
        .evaluate(request)
        .await
        .expect("a client fault is Ok(decision=false), not Err");
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

#[tokio::test]
async fn empty_token_scopes_returns_deny_not_stub() {
    // Default builder produces token_scopes: vec![] → scope check denies
    // before policy runs.
    let rbac = Arc::new(InMemoryRbacServiceClient::default());
    let plugin = plugin_with_rbac(Arc::clone(&rbac));
    let response = plugin
        .evaluate(EvaluationRequestBuilder::default().build())
        .await
        .expect("scope-deny is an Ok(decision=false), not an Err");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    assert_eq!(rbac.call_count(), 0, "scope deny must never reach RBAC");
}

#[tokio::test]
async fn scope_mismatch_returns_deny_not_stub() {
    let rbac = Arc::new(InMemoryRbacServiceClient::default());
    let plugin = plugin_with_rbac(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["read:events".to_owned()])
        .with_action_name("delete")
        .build();
    let response = plugin.evaluate(request).await.expect("Ok(deny)");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    assert_eq!(rbac.call_count(), 0, "scope deny must never reach RBAC");
}

#[tokio::test]
async fn scope_match_emits_tenant_constraint() {
    // read:events token + read action → scope passes → policy allow →
    // hierarchy materializes (Global → root tenant) → generation emits
    // a tenant constraint.
    let (rbac, tr) = rbac_allowing_with_root_tenant();
    let plugin = plugin_with_resolvers(rbac, tr, Arc::new(InMemoryResourceGroupClient::default()));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["read:events".to_owned()])
        .with_action_name("read")
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("matching scope + tenant materialization -> allow");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
}

#[tokio::test]
async fn policy_denied_returns_insufficient_permissions() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_denied(
        RbacDenyReason::NoMatchingPermission,
    ));
    let plugin = plugin_with_rbac(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("policy-deny is Ok(decision=false), not Err");

    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn policy_error_returns_service_unavailable() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::internal("simulated"),
    ));
    let plugin = plugin_with_rbac(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .build();
    match plugin.evaluate(request).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "rbac service unavailable");
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn policy_allowed_emits_tenant_constraint() {
    let (rbac, tr) = rbac_allowing_with_root_tenant();
    let plugin = plugin_with_resolvers(
        Arc::clone(&rbac),
        tr,
        Arc::new(InMemoryResourceGroupClient::default()),
    );
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("policy-allow + tenant materialization -> Ok(decision=true)");
    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    assert!(response.context.deny_reason.is_none());
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn hierarchy_resolver_error_returns_service_unavailable() {
    // Policy allows (wildcard scope, Global); tenant resolver is in
    // error mode → materialize_scope fails → ServiceUnavailable.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::with_error("simulated"));
    let plugin = plugin_with_resolvers(rbac, tr, Arc::new(InMemoryResourceGroupClient::default()));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .build();
    match plugin.evaluate(request).await {
        Err(AuthZResolverError::ServiceUnavailable(msg)) => {
            assert_eq!(msg, "tenant resolver unavailable");
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
}

// -- Capability degradation ------------------------------------------

fn membership(group_id: Uuid, resource_id: Uuid) -> ResourceGroupMembership {
    ResourceGroupMembership {
        group_id,
        resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
        resource_id: resource_id.to_string(),
    }
}

/// Configure an RG fake so a single root group `G1` (owned by `owner_tenant_id`)
/// maps to `resources` (flat subtree — no child groups).
///
/// `get_group_descendants` is `depth >= 0`, so its page includes the root group
/// itself; we seed the root at depth 0 carrying its owning tenant so the plugin
/// can build the tenant-paired group constraint (`RESOURCE_GROUP_MODEL.md`
/// "tenant constraint always applies alongside group predicates").
fn rg_with_group_subtree(
    group_id: Uuid,
    owner_tenant_id: Uuid,
    resources: &[Uuid],
) -> Arc<InMemoryResourceGroupClient> {
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    rg.add_group_descendants(
        group_id,
        vec![resource_group_sdk::models::ResourceGroupWithDepth {
            id: group_id,
            code: "gts.cf.core.rg.type.v1~test.v1~".to_owned(),
            name: "g1".to_owned(),
            hierarchy: resource_group_sdk::models::GroupHierarchyWithDepth {
                parent_id: None,
                tenant_id: owner_tenant_id,
                depth: 0,
            },
            metadata: None,
        }],
    );
    rg.add_memberships(resources.iter().map(|r| membership(group_id, *r)).collect());
    rg
}

#[tokio::test]
async fn group_subtree_emits_in_predicate_on_id() {
    use crate::domain::constraint_generator::RESOURCE_ID;
    use authz_resolver_sdk::constraints::Predicate;

    let g1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_00A1);
    let resources = vec![
        Uuid::from_u128(0xA001),
        Uuid::from_u128(0xA002),
        Uuid::from_u128(0xA003),
    ];
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![g1],
        },
    ));
    let group_tenant = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_00A9);
    let plugin = plugin_with_resolvers(
        rbac,
        Arc::new(InMemoryTenantResolverClient::default()),
        rg_with_group_subtree(g1, group_tenant, &resources),
    );
    // owner_tenant_id is now required: the group constraint is tenant-paired.
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["id".to_owned(), "owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("GroupSubtree allow -> Ok(decision=true)");
    assert!(response.decision);
    // One constraint, but now with TWO AND'd predicates: In(id, …) + the
    // group's owning-tenant predicate (defense-in-depth).
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
    // The paired tenant predicate carries the group's owning tenant.
    let tenant_ok = predicates.iter().any(|p| match p {
        Predicate::Eq(e) => {
            e.property == "owner_tenant_id" && e.value == serde_json::json!(group_tenant)
        }
        _ => false,
    });
    assert!(
        tenant_ok,
        "group constraint must pair the group's owning tenant"
    );
}

#[tokio::test]
async fn combined_emits_two_or_d_constraints() {
    use crate::domain::constraint_generator::{OWNER_TENANT_ID, RESOURCE_ID};
    use authz_resolver_sdk::constraints::Predicate;

    let t1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_00B1);
    // The group's OWNING tenant, deliberately different from the tenant-subtree
    // leg's t1: if the two matched, pairing the group side with the request's
    // tenant instead of the group's owner would be indistinguishable here.
    let group_tenant = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_00B2);
    let g1 = Uuid::from_u128(0x_0000_0000_0000_0000_0000_0000_0000_00C1);
    let resources = vec![Uuid::from_u128(0xC101), Uuid::from_u128(0xC102)];

    let root = TenantInfo {
        id: TenantId(t1),
        name: "t1".to_owned(),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    };
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);

    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::TenantSubtree { root_tenant_id: t1 },
                PermissionScopeType::GroupSubtree {
                    root_group_ids: vec![g1],
                },
            ],
        },
    ));
    // The group g1 is owned by `group_tenant`, not by the requested t1 — groups
    // are tenant-scoped, and the group side must carry ITS tenant.
    let plugin = plugin_with_resolvers(
        rbac,
        tr,
        rg_with_group_subtree(g1, group_tenant, &resources),
    );
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
    // First constraint: the OR'd tenant side — Eq(owner_tenant_id, T1).
    match &response.context.constraints[0].predicates[0] {
        Predicate::Eq(eq) => {
            assert_eq!(eq.property, OWNER_TENANT_ID);
            assert_eq!(eq.value, serde_json::json!(t1));
        }
        other => panic!("expected Eq on tenant, got {other:?}"),
    }
    // Second constraint: the tenant-PAIRED group side — In(id, [resources])
    // AND-paired with the group's owning-tenant predicate.
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
        .expect("expected In(id, ...) on the group side");
    assert_eq!(in_pred.values.len(), 2);
    // The VALUE matters: pairing with the request's t1 rather than the group's
    // own tenant is the cross-tenant leak this constraint exists to prevent.
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
async fn tenant_direct_reserved_returns_insufficient_permissions() {
    // Reserved variant — materialize_scope returns Materialization::Denied,
    // constraint generator dispatches to insufficient_permissions deny.
    let t1 = Uuid::from_u128(0xD001);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::TenantDirect { tenant_id: t1 },
    ));
    let plugin = plugin_with_resolvers(
        rbac,
        Arc::new(InMemoryTenantResolverClient::default()),
        Arc::new(InMemoryResourceGroupClient::default()),
    );
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("reserved variant is Ok(decision=false), not Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
}

#[tokio::test]
async fn unsupported_property_denies() {
    // PEP supports only "id"; tenant-scoped allow tries to emit a
    // predicate on "owner_tenant_id" → unsupported_property deny.
    let (rbac, tr) = rbac_allowing_with_root_tenant();
    let plugin = plugin_with_resolvers(rbac, tr, Arc::new(InMemoryResourceGroupClient::default()));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["id".to_owned()])
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
async fn expansion_infeasible_denies() {
    // Configure a tight max_expansion_ids = 2; tenant resolver returns
    // root + 2 descendants (3 total) → over threshold.
    let root_id = Uuid::from_u128(0xE001);
    let root = TenantInfo {
        id: TenantId(root_id),
        name: "root".to_owned(),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    };
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(
        root.id,
        vec![
            TenantRef {
                id: TenantId(Uuid::from_u128(0xE002)),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: Some(TenantId(root_id)),
                self_managed: false,
            },
            TenantRef {
                id: TenantId(Uuid::from_u128(0xE003)),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: Some(TenantId(root_id)),
                self_managed: false,
            },
        ],
    );
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));

    // Construct a tight config inline.
    let mut config = AuthZResolverPluginConfig::default();
    config.vendor = "vz".to_owned();
    config.capability_degradation.max_expansion_ids = 2;
    let plugin = AuthZResolverPlugin::new(
        Arc::new(config),
        rbac,
        tr,
        Arc::new(InMemoryResourceGroupClient::default()),
        default_registry(),
    );

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("expansion_infeasible is Ok(decision=false)");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, EXPANSION_INFEASIBLE_V1);
}

// -- Orchestration and audit -----------------------------------------

#[tokio::test]
async fn require_constraints_false_skips_constraint_generation() {
    // Even with empty supported_properties (which would normally cause an
    // unsupported_property deny), require_constraints=false must skip
    // generate_constraints entirely and return an empty-constraints
    // allow.
    let (rbac, tr) = rbac_allowing_with_root_tenant();
    let plugin = plugin_with_resolvers(rbac, tr, Arc::new(InMemoryResourceGroupClient::default()));
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![]) // empty — would normally fail validation
        .with_require_constraints(false)
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("require_constraints=false -> Ok(decision=true)");
    assert!(response.decision);
    assert!(response.context.constraints.is_empty());
    assert!(response.context.deny_reason.is_none());
}

#[test]
fn empty_materialization_detection_covers_all_eager_scope_shapes() {
    assert_eq!(
        empty_materialization_deny(&Materialization::TenantSubtree {
            tenant_ids: Vec::new(),
        })
        .map(|(code, _)| code),
        Some(INSUFFICIENT_PERMISSIONS_V1)
    );
    assert_eq!(
        empty_materialization_deny(&Materialization::GroupSubtree {
            resource_ids: Vec::new(),
            owner_tenant_ids: vec![Uuid::new_v4()],
        })
        .map(|(code, _)| code),
        Some(INSUFFICIENT_PERMISSIONS_V1)
    );
    assert_eq!(
        empty_materialization_deny(&Materialization::Combined {
            tenant_ids: Vec::new(),
            resource_ids: Vec::new(),
            group_owner_tenant_ids: Vec::new(),
        })
        .map(|(code, _)| code),
        Some(CONSTRAINTS_UNAVAILABLE_V1)
    );
    assert!(
        empty_materialization_deny(&Materialization::TenantSubtree {
            tenant_ids: vec![Uuid::new_v4()],
        })
        .is_none()
    );
    assert!(
        empty_materialization_deny(&Materialization::TenantSubtreePushdown {
            root_tenant_id: Uuid::new_v4(),
            barrier_mode: BarrierMode::Respect,
            status: vec![TenantStatus::Active],
        })
        .is_none()
    );
}

#[tokio::test]
async fn valid_combined_scope_resolving_to_no_ids_denies_in_both_pep_modes() {
    for require_constraints in [false, true] {
        let (rbac, tenant_resolver, resource_group) =
            valid_combined_allow_with_empty_materialization();
        let plugin = plugin_with_resolvers(rbac, tenant_resolver, resource_group);
        let request = EvaluationRequestBuilder::default()
            .with_token_scopes(vec!["*".to_owned()])
            .with_supported_properties(vec!["owner_tenant_id".to_owned(), "id".to_owned()])
            .with_require_constraints(require_constraints)
            .build();

        let response = plugin
            .evaluate(request)
            .await
            .expect("empty materialization is a fail-closed business deny");
        assert!(!response.decision);
        assert_eq!(
            response
                .context
                .deny_reason
                .expect("deny reason")
                .error_code,
            CONSTRAINTS_UNAVAILABLE_V1,
            "empty materialization must deny before require_constraints={require_constraints} can widen it"
        );
    }
}

#[tokio::test]
async fn malformed_empty_active_scope_payloads_fail_before_hierarchy() {
    let scopes = [
        PermissionScopeType::Global,
        PermissionScopeType::TenantSubtree {
            root_tenant_id: Uuid::new_v4(),
        },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![Uuid::new_v4()],
        },
    ];

    for scope_type in scopes {
        let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
            Vec::new(),
            scope_type,
        ));
        let tenant_resolver = Arc::new(InMemoryTenantResolverClient::default());
        let resource_group = Arc::new(InMemoryResourceGroupClient::default());
        let plugin = plugin_with_resolvers(rbac, tenant_resolver.clone(), resource_group.clone());
        let request = EvaluationRequestBuilder::default()
            .with_token_scopes(vec!["*".to_owned()])
            .with_require_constraints(false)
            .build();

        let error = plugin
            .evaluate(request)
            .await
            .expect_err("empty active-scope provenance must fail closed");
        assert_scope_provenance_error(error);
        assert_eq!(tenant_resolver.call_count(), 0);
        assert_eq!(resource_group.call_count(), 0);
    }
}

#[tokio::test]
async fn require_constraints_false_with_empty_combined_fails_closed_before_materialization() {
    // An empty Combined is not a reserved-variant materialization. Without the
    // provenance guard, the decision-only branch would treat it as an allow
    // with no constraints even though no assignment contributed authority.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::Combined { scopes: vec![] },
    ));
    let tenant_resolver = Arc::new(InMemoryTenantResolverClient::default());
    let plugin = plugin_with_resolvers(
        rbac,
        tenant_resolver.clone(),
        Arc::new(InMemoryResourceGroupClient::default()),
    );
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .with_require_constraints(false)
        .build();

    let error = plugin
        .evaluate(request)
        .await
        .expect_err("an empty normal allow must fail closed for decision-only PEPs");
    assert!(matches!(error, AuthZResolverError::Internal(_)));
    assert_eq!(
        tenant_resolver.call_count(),
        0,
        "empty provenance must be rejected before hierarchy materialization"
    );
}

#[tokio::test]
async fn require_constraints_true_with_empty_combined_fails_closed_before_materialization() {
    // Constraint-bearing PEPs must hit the same provenance barrier as
    // decision-only PEPs. Rejecting before materialization keeps malformed
    // empty allows out of hierarchy caches and constraint generation alike.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::Combined { scopes: vec![] },
    ));
    let tenant_resolver = Arc::new(InMemoryTenantResolverClient::default());
    let plugin = plugin_with_resolvers(
        rbac,
        tenant_resolver.clone(),
        Arc::new(InMemoryResourceGroupClient::default()),
    );
    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .with_require_constraints(true)
        .build();

    let error = plugin
        .evaluate(request)
        .await
        .expect_err("an empty normal allow must fail closed before constraint generation");
    assert!(matches!(error, AuthZResolverError::Internal(_)));
    assert_eq!(
        tenant_resolver.call_count(),
        0,
        "empty provenance must be rejected before hierarchy materialization"
    );
}

#[tokio::test]
async fn strict_mode_unknown_resource_type_returns_business_deny() {
    // Strict + Unknown is a business deny — `Ok(decision=false,
    // unknown_resource_type.v1)`, not `Err(Internal)`.
    // Build a plugin with strict mode by overriding the config.
    use crate::config::GtsValidationMode;
    use crate::domain::deny::error_codes::UNKNOWN_RESOURCE_TYPE_V1;

    let (rbac, tr) = rbac_allowing_with_root_tenant();
    // Use an empty types registry — the default subject + resource types
    // are unknown to it.
    let empty_registry = Arc::new(crate::test_support::RecordingTypesRegistry::new());

    let mut config = AuthZResolverPluginConfig::default();
    config.vendor = "vz".to_owned();
    config.gts_validation.mode = GtsValidationMode::Strict;
    let plugin = AuthZResolverPlugin::new(
        Arc::new(config),
        rbac,
        tr,
        Arc::new(InMemoryResourceGroupClient::default()),
        empty_registry,
    );

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("Strict + Unknown is Ok(decision=false), NOT Err");
    assert!(!response.decision);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, UNKNOWN_RESOURCE_TYPE_V1);
}

// ------------ System actor vs the scope gate ------------

/// The in-process AM system actor carries NO token (empty `token_scopes` by
/// construction), so the trusted short-circuit MUST run before the fail-closed
/// empty-scopes deny — otherwise the hard-delete reaper's RG cascade is denied
/// on every attempt and the retention backlog never drains. The sentinel-gated
/// bypass must admit it end-to-end without ever consulting RBAC.
#[tokio::test]
async fn trusted_system_actor_empty_scopes_bypasses_scope_gate() {
    let (rbac, tr) = rbac_allowing_with_root_tenant();
    let rbac_probe = Arc::clone(&rbac);
    let plugin = plugin_with_resolvers(rbac, tr, Arc::new(InMemoryResourceGroupClient::default()));
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(crate::test_support::trusted_actors::AM_SYSTEM_ACTOR_UUID)
        .with_subject_type(Some("am.system".to_owned()))
        .with_subject_tenant_id(Uuid::nil())
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("trusted system actor must be admitted");
    assert!(
        response.decision,
        "got deny: {:?}",
        response.context.deny_reason
    );
    assert_eq!(
        rbac_probe.call_count(),
        0,
        "trusted short-circuit must never reach RBAC"
    );
}

/// Security pin for the bypass: the gate is the unforgeable sentinel
/// UUID, not the `subject_type` string — a forged `am.system` tag on a
/// random subject id is rejected at Step 1 validation (only the true
/// sentinel may carry that type), so it never even reaches the scope
/// gate the real system actor bypasses.
///
/// The rejection is an `invalid_request.v1` deny rather than an `Internal`:
/// the forged tag is a malformed request, and a deny is what the PEP must act
/// on. What the pin is about — the forgery does NOT ride the bypass and RBAC
/// is never consulted — is unchanged.
#[tokio::test]
async fn forged_am_system_type_rejected_before_scope_gate() {
    let rbac = Arc::new(InMemoryRbacServiceClient::default());
    let plugin = plugin_with_rbac(Arc::clone(&rbac));
    let response = plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_subject_type(Some("am.system".to_owned()))
                .build(),
        )
        .await
        .expect("a forged tag is a client fault: Ok(decision=false), not Err");
    assert!(
        !response.decision,
        "a forged am.system tag MUST NOT be allowed"
    );
    let reason = response.context.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, INVALID_REQUEST_V1);
    assert_eq!(
        reason.details.as_deref(),
        Some("unknown subject type: am.system")
    );
    assert_eq!(rbac.call_count(), 0);
}
