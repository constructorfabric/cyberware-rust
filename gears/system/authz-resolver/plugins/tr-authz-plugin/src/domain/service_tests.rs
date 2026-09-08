// Created: 2026-04-16 by Constructor Tech
// Updated: 2026-04-29 by Constructor Tech

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::models::{Action, Resource, Subject};
use authz_resolver_sdk::{
    EvaluationRequest, EvaluationRequestContext, EvaluationResponse, Predicate, TenantContext,
};
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::service::Service;

// -- Mock Tenant Resolver --

struct MockTenantResolver {
    descendants: Vec<TenantRef>,
    root: Option<TenantRef>,
}

impl MockTenantResolver {
    fn with_tenants(root_id: Uuid, descendant_ids: Vec<Uuid>) -> Self {
        let root = TenantRef {
            id: TenantId(root_id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        };
        let descendants = descendant_ids
            .into_iter()
            .map(|id| TenantRef {
                id: TenantId(id),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: Some(TenantId(root_id)),
                self_managed: false,
            })
            .collect();
        Self {
            descendants,
            root: Some(root),
        }
    }

    fn empty() -> Self {
        Self {
            descendants: vec![],
            root: None,
        }
    }
}

#[async_trait]
impl TenantResolverClient for MockTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        if self.root.as_ref().is_some_and(|r| r.id == id) {
            Ok(TenantInfo {
                id,
                name: format!("T-{}", id.0),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: None,
                self_managed: false,
            })
        } else {
            Err(TenantResolverError::TenantNotFound { tenant_id: id })
        }
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        unimplemented!("not used by tr-authz-plugin")
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(vec![])
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        unimplemented!("not used by tr-authz-plugin")
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        match &self.root {
            Some(root) if root.id == id => Ok(GetDescendantsResponse {
                tenant: root.clone(),
                descendants: self.descendants.clone(),
            }),
            _ => Err(TenantResolverError::TenantNotFound { tenant_id: id }),
        }
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor_id: TenantId,
        _descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        unimplemented!("not used by tr-authz-plugin")
    }
}

fn make_request(tenant_id: Uuid) -> EvaluationRequest {
    // `subject.properties["tenant_id"] = tenant_id` mirrors the PEP-injected
    // home tenant for a caller whose subject tenant equals `root_id`. That
    // matches the original intent of these legacy tests (subject is allowed
    // to see its own tenant's subtree — R6-style).
    let mut subject_props = std::collections::HashMap::default();
    subject_props.insert(
        "tenant_id".to_owned(),
        serde_json::Value::String(tenant_id.to_string()),
    );
    EvaluationRequest {
        subject: Subject {
            id: Uuid::now_v7(),
            subject_type: None,
            properties: subject_props,
        },
        action: Action {
            name: "list".to_owned(),
        },
        resource: Resource {
            resource_type: gts_id!("cf.test.authz.resource.v1~").to_owned(),
            id: None,
            properties: std::collections::HashMap::default(),
        },
        context: EvaluationRequestContext {
            tenant_context: Some(TenantContext {
                root_id: Some(tenant_id),
                ..Default::default()
            }),
            token_scopes: vec![],
            require_constraints: true,
            capabilities: vec![],
            supported_properties: vec![],
            bearer_token: None,
        },
    }
}

fn make_request_no_tenant() -> EvaluationRequest {
    EvaluationRequest {
        subject: Subject {
            id: Uuid::now_v7(),
            subject_type: None,
            properties: std::collections::HashMap::default(),
        },
        action: Action {
            name: "list".to_owned(),
        },
        resource: Resource {
            resource_type: gts_id!("cf.test.authz.resource.v1~").to_owned(),
            id: None,
            properties: std::collections::HashMap::default(),
        },
        context: EvaluationRequestContext {
            tenant_context: None,
            token_scopes: vec![],
            require_constraints: false,
            capabilities: vec![],
            supported_properties: vec![],
            bearer_token: None,
        },
    }
}

// -- Tests --

#[tokio::test]
async fn tenant_subtree_resolved_to_in_predicate() {
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let mock = MockTenantResolver::with_tenants(t1, vec![t2]);
    let svc = Service::new(Arc::new(mock));
    let resp = svc.evaluate(&make_request(t1)).await;

    assert!(resp.decision);
    assert_eq!(resp.context.constraints.len(), 1);
    let preds = &resp.context.constraints[0].predicates;
    assert_eq!(preds.len(), 1);
    assert!(
        matches!(&preds[0], Predicate::In(p) if p.property == "owner_tenant_id"),
        "expected In(owner_tenant_id), got: {preds:?}"
    );
    // Should have both t1 (root) and t2 (descendant)
    if let Predicate::In(p) = &preds[0] {
        assert_eq!(p.values.len(), 2, "root + 1 descendant");
    }
}

#[tokio::test]
async fn barrier_handled_by_tr_only_visible_returned() {
    // Barrier filtering is delegated to TR (BarrierMode::Respect): the mock
    // simulates the post-filter view — TR has already dropped t_barrier and
    // t_behind, so they never reach the authz plugin. The plugin must trust
    // that view and return exactly what TR exposed.
    let t1 = Uuid::now_v7();
    let t_normal = Uuid::now_v7();
    let t_barrier = Uuid::now_v7(); // absent — would-be barrier tenant
    let t_behind = Uuid::now_v7(); // absent — would-be descendant of t_barrier
    // Mock: TR already filtered out t_barrier + t_behind
    let mock = MockTenantResolver::with_tenants(t1, vec![t_normal]);
    let svc = Service::new(Arc::new(mock));
    let resp = svc.evaluate(&make_request(t1)).await;

    assert!(resp.decision);
    let preds = &resp.context.constraints[0].predicates;
    if let Predicate::In(p) = &preds[0] {
        let got: std::collections::HashSet<_> = p.values.iter().map(parse_uuid_value).collect();
        let expected: std::collections::HashSet<_> = [t1, t_normal].into_iter().collect();
        assert_eq!(
            got, expected,
            "predicate must contain exactly the tenants TR returned",
        );
        assert!(
            !got.contains(&t_barrier),
            "barrier tenant must be excluded (TR dropped it)",
        );
        assert!(
            !got.contains(&t_behind),
            "behind-barrier tenant must be excluded (TR dropped it)",
        );
    } else {
        panic!("expected In predicate");
    }
}

#[tokio::test]
async fn no_tenant_in_request_denies() {
    let mock = MockTenantResolver::empty();
    let svc = Service::new(Arc::new(mock));
    let resp = svc.evaluate(&make_request_no_tenant()).await;
    assert!(!resp.decision, "no tenant -> deny");
}

#[tokio::test]
async fn nil_tenant_denies() {
    let mock = MockTenantResolver::empty();
    let svc = Service::new(Arc::new(mock));
    let resp = svc.evaluate(&make_request(Uuid::default())).await;
    assert!(!resp.decision, "nil tenant -> deny");
}

#[tokio::test]
async fn tenant_not_found_denies() {
    let mock = MockTenantResolver::empty();
    let svc = Service::new(Arc::new(mock));
    let resp = svc.evaluate(&make_request(Uuid::now_v7())).await;
    assert!(!resp.decision, "tenant not found -> deny (fail-closed)");
}

#[tokio::test]
async fn tr_error_denies() {
    struct FailingTr;

    #[async_trait]
    impl TenantResolverClient for FailingTr {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::Internal(format!("fail {id}")))
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::Internal("fail".to_owned()))
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Err(TenantResolverError::Internal("fail".to_owned()))
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            Err(TenantResolverError::Internal("fail".to_owned()))
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Err(TenantResolverError::Internal("fail".to_owned()))
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _a: TenantId,
            _d: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Err(TenantResolverError::Internal("fail".to_owned()))
        }
    }

    let svc = Service::new(Arc::new(FailingTr));
    let resp = svc.evaluate(&make_request(Uuid::now_v7())).await;
    assert!(!resp.decision, "TR error -> deny (fail-closed)");
}

#[tokio::test]
async fn group_predicates_from_request_properties() {
    let t1 = Uuid::now_v7();
    let mock = MockTenantResolver::with_tenants(t1, vec![]);
    let svc = Service::new(Arc::new(mock));

    let g1 = Uuid::now_v7();
    let g2 = Uuid::now_v7();
    let mut req = make_request(t1);
    req.context.capabilities = vec![
        authz_resolver_sdk::Capability::GroupMembership,
        authz_resolver_sdk::Capability::GroupHierarchy,
    ];
    req.resource.properties.insert(
        "group_ids".to_owned(),
        serde_json::json!([g1.to_string(), g2.to_string()]),
    );
    req.resource.properties.insert(
        "ancestor_group_ids".to_owned(),
        serde_json::json!([g1.to_string()]),
    );

    let resp = svc.evaluate(&req).await;
    assert!(resp.decision);

    let preds = &resp.context.constraints[0].predicates;
    assert_eq!(preds.len(), 3, "In + InGroup + InGroupSubtree");
    assert!(matches!(&preds[0], Predicate::In(_)));
    assert!(matches!(&preds[1], Predicate::InGroup(_)));
    assert!(matches!(&preds[2], Predicate::InGroupSubtree(_)));
}

#[tokio::test]
async fn empty_group_selector_is_denied_instead_of_widened() {
    let tenant_id = Uuid::now_v7();
    let mock = MockTenantResolver::with_tenants(tenant_id, vec![]);
    let svc = Service::new(Arc::new(mock));

    for property in ["group_ids", "ancestor_group_ids"] {
        let mut request = make_request(tenant_id);
        request.context.capabilities = vec![
            authz_resolver_sdk::Capability::GroupMembership,
            authz_resolver_sdk::Capability::GroupHierarchy,
        ];
        request
            .resource
            .properties
            .insert(property.to_owned(), serde_json::json!([]));

        let response = svc.evaluate(&request).await;
        assert!(
            !response.decision,
            "an empty {property} selector must match nothing, not tenant-wide scope"
        );
    }
}

#[tokio::test]
async fn unadvertised_group_predicate_is_denied() {
    let tenant_id = Uuid::now_v7();
    let mock = MockTenantResolver::with_tenants(tenant_id, vec![]);
    let svc = Service::new(Arc::new(mock));
    let mut request = make_request(tenant_id);
    request.resource.properties.insert(
        "group_ids".to_owned(),
        serde_json::json!([Uuid::now_v7().to_string()]),
    );

    let response = svc.evaluate(&request).await;

    assert!(
        !response.decision,
        "the PDP must not emit an unadvertised native group predicate"
    );
}

// ──────────────────────────────────────────────────────────────────────
// R1–R8 decision matrix tests
// ──────────────────────────────────────────────────────────────────────
//
// Hierarchy used by the R-tests (as in aviator5's review comment on PR #1550):
//
//     r (root)
//     └── t1 (partner)
//         └── t2 (partner)
//             ├── t3 (customer)
//             └── t4 (customer)
//
// `HierarchyMock` implements `is_ancestor` and `get_descendants` against this
// shape; `get_tenant` / `get_ancestors` / `get_root_tenant` / `get_tenants`
// are unused by `tr-authz-plugin::evaluate`.

// `HierarchyMock`, `FailingOnDescendants`, and `EmptyTr` (in `client_tests`)
// previously implemented `TenantResolverClient` three times with overlapping
// skeletons. The configurable mock now lives in
// `crate::domain::test_support::MockTr` and is instantiated via the constructors
// `with_hierarchy` / `failing_descendants` / `empty`.
use crate::domain::test_support::{MockTr, setup_svc};

/// Wrapper around `svc.evaluate(&build_r_request(...)).await` to collapse the
/// 8-line call into a single line per test. Same arguments as `build_r_request`.
async fn evaluate_r(
    svc: &Service,
    subject_tid: Uuid,
    action: &str,
    resource_id: Option<Uuid>,
    owner_tenant_id: Option<Uuid>,
    root_id: Option<Uuid>,
    mode: Option<authz_resolver_sdk::TenantMode>,
) -> EvaluationResponse {
    svc.evaluate(&build_r_request(
        subject_tid,
        action,
        resource_id,
        owner_tenant_id,
        root_id,
        mode,
    ))
    .await
}

/// Generic request builder for R-rule tests.
fn build_r_request(
    subject_tid: Uuid,
    action: &str,
    resource_id: Option<Uuid>,
    owner_tenant_id: Option<Uuid>,
    root_id: Option<Uuid>,
    mode: Option<authz_resolver_sdk::TenantMode>,
) -> EvaluationRequest {
    let mut subject_props = std::collections::HashMap::default();
    subject_props.insert(
        "tenant_id".to_owned(),
        serde_json::Value::String(subject_tid.to_string()),
    );
    let mut resource_props = std::collections::HashMap::default();
    if let Some(owner) = owner_tenant_id {
        resource_props.insert(
            "owner_tenant_id".to_owned(),
            serde_json::Value::String(owner.to_string()),
        );
    }
    let tenant_context = if root_id.is_some() || mode.is_some() {
        Some(TenantContext {
            root_id,
            mode: mode.unwrap_or_default(),
            ..Default::default()
        })
    } else {
        None
    };
    EvaluationRequest {
        subject: Subject {
            id: Uuid::now_v7(),
            subject_type: None,
            properties: subject_props,
        },
        action: Action {
            name: action.to_owned(),
        },
        resource: Resource {
            resource_type: gts_id!("cf.test.authz.resource.v1~").to_owned(),
            id: resource_id,
            properties: resource_props,
        },
        context: EvaluationRequestContext {
            tenant_context,
            token_scopes: vec![],
            require_constraints: true,
            capabilities: vec![],
            supported_properties: vec![],
            bearer_token: None,
        },
    }
}

/// Assert that the response is `allow` with a single `In(owner_tenant_id, …)`
/// predicate whose values equal `expected` as a set.
///
/// UUID parsing is strict: a non-string predicate value or a malformed UUID
/// fails the test (rather than being silently dropped) — otherwise tests
/// could pass with bad predicate payloads.
fn assert_allow_in(resp: &EvaluationResponse, expected: &[Uuid]) {
    assert!(resp.decision, "expected allow, got deny");
    let preds = &resp.context.constraints[0].predicates;
    assert_eq!(preds.len(), 1);
    let Predicate::In(p) = &preds[0] else {
        panic!("expected In predicate");
    };
    let got: std::collections::HashSet<_> = p.values.iter().map(parse_uuid_value).collect();
    let want: std::collections::HashSet<_> = expected.iter().copied().collect();
    assert_eq!(got, want, "predicate values mismatch");
}

/// Strict UUID parser for test predicate values. Panics with a descriptive
/// message on non-string values or invalid UUIDs.
fn parse_uuid_value(v: &serde_json::Value) -> Uuid {
    let s = v
        .as_str()
        .unwrap_or_else(|| panic!("expected predicate value to be a string, got: {v:?}"));
    Uuid::parse_str(s).unwrap_or_else(|e| panic!("invalid UUID in predicate values: {s:?} ({e})"))
}

// ── R1: single, root_id, root_only ─────────────────────────────────────
#[tokio::test]
async fn r1_partner_reads_customer_task_root_only() {
    let (svc, [_r, t1, t2, _t3, _t4]) = setup_svc();
    let resp = evaluate_r(
        &svc,
        t1,
        "get",
        Some(Uuid::now_v7()),
        Some(t2),
        Some(t2),
        Some(authz_resolver_sdk::TenantMode::RootOnly),
    )
    .await;
    assert_allow_in(&resp, &[t2]);
}

#[tokio::test]
async fn r1_deny_when_owner_differs_from_root_id() {
    let (svc, [_r, t1, t2, t3, _t4]) = setup_svc();
    // owner_tenant_id=t3 but root_id=t2 → mismatch → deny.
    let resp = evaluate_r(
        &svc,
        t1,
        "get",
        Some(Uuid::now_v7()),
        Some(t3),
        Some(t2),
        Some(authz_resolver_sdk::TenantMode::RootOnly),
    )
    .await;
    assert!(!resp.decision);
}

// ── R2: single, root_id, subtree (default) ─────────────────────────────
#[tokio::test]
async fn r2_partner_reads_task_from_customer_subtree() {
    let (svc, [_r, t1, t2, t3, _t4]) = setup_svc();
    let resp = evaluate_r(
        &svc,
        t1,
        "get",
        Some(Uuid::now_v7()),
        Some(t3),
        Some(t2),
        None, // default: Subtree
    )
    .await;
    assert_allow_in(&resp, &[t3]);
}

#[tokio::test]
async fn r2_deny_when_owner_outside_root_subtree() {
    let (svc, [_r, t1, _t2, _t3, _t4]) = setup_svc();
    // subject=t1, root_id=t1, but owner=non-existent → not in t1's subtree.
    let alien = Uuid::now_v7();
    let resp = evaluate_r(
        &svc,
        t1,
        "get",
        Some(Uuid::now_v7()),
        Some(alien),
        Some(t1),
        None,
    )
    .await;
    assert!(!resp.decision);
}

// ── R3: single, no root_id, root_only ─────────────────────────────────
#[tokio::test]
async fn r3_user_reads_own_task_root_only() {
    let (svc, [_r, _t1, _t2, t3, _t4]) = setup_svc();
    let resp = evaluate_r(
        &svc,
        t3,
        "get",
        Some(Uuid::now_v7()),
        Some(t3),
        None,
        Some(authz_resolver_sdk::TenantMode::RootOnly),
    )
    .await;
    assert_allow_in(&resp, &[t3]);
}

#[tokio::test]
async fn r3_deny_when_owner_differs_from_subject() {
    let (svc, [_r, _t1, _t2, t3, t4]) = setup_svc();
    let resp = evaluate_r(
        &svc,
        t3,
        "get",
        Some(Uuid::now_v7()),
        Some(t4),
        None,
        Some(authz_resolver_sdk::TenantMode::RootOnly),
    )
    .await;
    assert!(!resp.decision);
}

// ── R4: single, no root_id, subtree (default) ─────────────────────────
#[tokio::test]
async fn r4_partner_reads_task_from_own_subtree() {
    let (svc, [_r, t1, _t2, t3, _t4]) = setup_svc();
    let resp = evaluate_r(&svc, t1, "get", Some(Uuid::now_v7()), Some(t3), None, None).await;
    assert_allow_in(&resp, &[t3]);
}

#[tokio::test]
async fn r4_allow_when_owner_equals_subject_reflexive() {
    let (svc, [_r, t1, _t2, _t3, _t4]) = setup_svc();
    // Reflexive: owner == subject — `is_in_subtree(subject, owner)` short-circuits.
    let resp = evaluate_r(&svc, t1, "get", Some(Uuid::now_v7()), Some(t1), None, None).await;
    assert_allow_in(&resp, &[t1]);
}

#[tokio::test]
async fn r4_deny_when_owner_outside_subject_subtree() {
    let (svc, [_r, _t1, _t2, t3, t4]) = setup_svc();
    // subject=t3, owner=t4 → siblings, neither is ancestor of the other.
    let resp = evaluate_r(&svc, t3, "get", Some(Uuid::now_v7()), Some(t4), None, None).await;
    assert!(!resp.decision);
}

// ── R5: list, root_id, root_only ──────────────────────────────────────
#[tokio::test]
async fn r5_partner_lists_customer_tasks_root_only() {
    let (svc, [_r, t1, t2, _t3, _t4]) = setup_svc();
    let resp = evaluate_r(
        &svc,
        t1,
        "list",
        None,
        None,
        Some(t2),
        Some(authz_resolver_sdk::TenantMode::RootOnly),
    )
    .await;
    assert_allow_in(&resp, &[t2]);
}

#[tokio::test]
async fn r5_deny_when_subject_cannot_see_root_id() {
    let (svc, [_r, _t1, _t2, t3, t4]) = setup_svc();
    // subject=t3 (customer), root_id=t4 (sibling) → subject not an ancestor of t4.
    let resp = evaluate_r(
        &svc,
        t3,
        "list",
        None,
        None,
        Some(t4),
        Some(authz_resolver_sdk::TenantMode::RootOnly),
    )
    .await;
    assert!(!resp.decision);
}

// ── R6: list, root_id, subtree ────────────────────────────────────────
#[tokio::test]
async fn r6_partner_lists_customer_subtree() {
    let (svc, [_r, t1, t2, t3, t4]) = setup_svc();
    let resp = evaluate_r(&svc, t1, "list", None, None, Some(t2), None).await;
    assert_allow_in(&resp, &[t2, t3, t4]);
}

#[tokio::test]
async fn r6_deny_when_subject_cannot_see_root_id() {
    let (svc, [_r, _t1, _t2, t3, t4]) = setup_svc();
    // subject=t3 (customer), root_id=t4 (sibling) → subject not an ancestor of t4 → deny.
    let resp = evaluate_r(&svc, t3, "list", None, None, Some(t4), None).await;
    assert!(!resp.decision);
}

// ── R7: list, no root_id, root_only ───────────────────────────────────
#[tokio::test]
async fn r7_user_lists_own_tasks_root_only() {
    let (svc, [_r, _t1, _t2, t3, _t4]) = setup_svc();
    let resp = evaluate_r(
        &svc,
        t3,
        "list",
        None,
        None,
        None,
        Some(authz_resolver_sdk::TenantMode::RootOnly),
    )
    .await;
    assert_allow_in(&resp, &[t3]);
}

// ── R8: list, no root_id, subtree ─────────────────────────────────────
#[tokio::test]
async fn r8_partner_lists_own_subtree() {
    let (svc, [_r, t1, t2, t3, t4]) = setup_svc();
    let resp = evaluate_r(&svc, t1, "list", None, None, None, None).await;
    assert_allow_in(&resp, &[t1, t2, t3, t4]);
}

#[tokio::test]
async fn r8_deny_on_tr_error() {
    // Dedicated R8 fail-path: `get_descendants(subject)` TR failure → deny.
    let svc = Service::new(Arc::new(MockTr::failing_descendants()));
    let resp = evaluate_r(&svc, Uuid::now_v7(), "list", None, None, None, None).await;
    assert!(!resp.decision, "R8: TR error on get_descendants -> deny");
}

// ── Fail-closed: single-resource missing owner_tenant_id ───────────────
#[tokio::test]
async fn single_resource_missing_owner_tenant_id_denies() {
    let (svc, [_r, t1, _t2, _t3, _t4]) = setup_svc();
    let resp = evaluate_r(
        &svc,
        t1,
        "get",
        Some(Uuid::now_v7()),
        None, // missing owner_tenant_id in properties
        None,
        None,
    )
    .await;
    assert!(!resp.decision);
}
