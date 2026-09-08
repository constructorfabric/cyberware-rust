// Created: 2026-04-16 by Constructor Tech
// @cpt-dod:cpt-cf-resource-group-dod-testing-rest-api:p1
#![allow(clippy::expect_used)]
//! Full-chain integration test with a real (`SQLite` in-memory) database.
//!
//! Verifies the complete `AuthZ` -> `PolicyEnforcer` -> `GroupService` -> `AccessScope`
//! -> `SecureORM` -> SQL WHERE `tenant_id` IN (...) -> filtered results path.
//!
//! Two tenants each create groups; listing groups through the `AuthZ`-scoped
//! `GroupService` returns only the requesting tenant's data.

mod common;

use std::sync::Arc;
use toolkit_gts::GTS_ID_PREFIX;

use async_trait::async_trait;
use uuid::Uuid;

use authz_resolver_sdk::{
    AuthZResolverApi, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
    PolicyEnforcer,
    constraints::{Constraint, InGroupPredicate, InPredicate, Predicate},
};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::{DBProvider, DbError};
use toolkit_odata::ODataQuery;
use toolkit_security::{PlatformSecurityContext, pep_properties};

use resource_group::domain::group_service::{GroupService, QueryProfile};
use resource_group::infra::storage::group_repo::GroupRepository;
use resource_group::infra::storage::membership_repo::MembershipRepository;
use resource_group::infra::storage::type_repo::TypeRepository;

use common::{make_ctx, test_db};

// ── Mock AuthZ: tenant-scoping (like static-authz-plugin) ───────────────

struct TenantScopingAuthZ;

#[async_trait]
impl AuthZResolverApi for TenantScopingAuthZ {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant_id = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("subject must have tenant_id");

        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [tenant_id],
                    ))],
                }],
                deny_reason: None,
            },
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build a `GroupService` with the file-local `TenantScopingAuthZ` mock.
///
/// Differs from `common::make_group_service` only by the `AuthZ` implementation:
/// `common` uses `AllowAllAuthZ`, while these tests need explicit tenant
/// scoping via `In(OWNER_TENANT_ID)` to exercise the `AccessScope` path.
fn make_group_service(
    db: Arc<DBProvider<DbError>>,
) -> GroupService<GroupRepository, TypeRepository> {
    let authz: Arc<dyn AuthZResolverApi> = Arc::new(TenantScopingAuthZ);
    let enforcer = PolicyEnforcer::new(authz);
    GroupService::new(
        db,
        QueryProfile::default(),
        enforcer,
        Arc::new(GroupRepository),
        Arc::new(TypeRepository),
        common::make_types_registry(),
    )
}

/// Same wiring, but every metadata validation fails: the registry answers
/// `Internal` rather than `NotFound`, so `validate_metadata_via_gts` returns
/// an error instead of skipping. Used to check what a cross-tenant caller can
/// learn from *which* error comes back.
fn make_group_service_with_unavailable_registry(
    db: Arc<DBProvider<DbError>>,
) -> GroupService<GroupRepository, TypeRepository> {
    let authz: Arc<dyn AuthZResolverApi> = Arc::new(TenantScopingAuthZ);
    let enforcer = PolicyEnforcer::new(authz);
    GroupService::new(
        db,
        QueryProfile::default(),
        enforcer,
        Arc::new(GroupRepository),
        Arc::new(TypeRepository),
        common::make_unavailable_types_registry(),
    )
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Full chain: two tenants create groups, each tenant sees only its own.
///
/// Flow per tenant:
///   `SecurityContext{tenant=T}` -> `GroupService.list_groups(&ctx, &query)`
///     -> `PolicyEnforcer.access_scope()` -> `AccessScope{owner_tenant_id IN (T)}`
///     -> `GroupRepository.list_groups(&conn, &scope, &query)`
///       -> `SecureORM` `.scope_with(&scope)` -> SQL WHERE `tenant_id` IN ('T')
///     -> only T's groups returned
// Scenario: L2-Tenant-01 - Tenant isolation for list_groups
#[tokio::test]
async fn tenant_isolation_list_groups() {
    let db = test_db().await;
    let type_svc = common::make_type_service(db.clone());
    let group_svc = make_group_service(db.clone());

    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let ctx_a = make_ctx(tenant_a);
    let ctx_b = make_ctx(tenant_b);

    // Create a type (types are not tenant-scoped)
    let type_code = common::create_root_type(&type_svc, "dbiso").await.code;

    // Tenant A creates 2 groups
    let ga1 = common::create_root_group(
        &group_svc,
        &ctx_a,
        &type_code,
        "Tenant A - Group 1",
        tenant_a,
    )
    .await;
    let ga2 = common::create_root_group(
        &group_svc,
        &ctx_a,
        &type_code,
        "Tenant A - Group 2",
        tenant_a,
    )
    .await;

    // Tenant B creates 1 group
    let gb1 = common::create_root_group(
        &group_svc,
        &ctx_b,
        &type_code,
        "Tenant B - Group 1",
        tenant_b,
    )
    .await;

    let query = ODataQuery::default();

    // ── Tenant A lists groups: should see only A's groups ──
    let page_a = group_svc
        .list_groups(&ctx_a, &query)
        .await
        .expect("list groups for tenant A");

    let ids_a: Vec<Uuid> = page_a.items.iter().map(|g| g.id).collect();
    assert!(ids_a.contains(&ga1.id), "Tenant A should see group A1");
    assert!(ids_a.contains(&ga2.id), "Tenant A should see group A2");
    assert!(!ids_a.contains(&gb1.id), "Tenant A must NOT see group B1");
    assert_eq!(
        ids_a.len(),
        2,
        "Tenant A should see exactly 2 groups, got: {ids_a:?}"
    );

    // ── Tenant B lists groups: should see only B's groups ──
    let page_b = group_svc
        .list_groups(&ctx_b, &query)
        .await
        .expect("list groups for tenant B");

    let ids_b: Vec<Uuid> = page_b.items.iter().map(|g| g.id).collect();
    assert!(ids_b.contains(&gb1.id), "Tenant B should see group B1");
    assert!(!ids_b.contains(&ga1.id), "Tenant B must NOT see group A1");
    assert!(!ids_b.contains(&ga2.id), "Tenant B must NOT see group A2");
    assert_eq!(
        ids_b.len(),
        1,
        "Tenant B should see exactly 1 group, got: {ids_b:?}"
    );
}

/// Full chain: `get_group` with wrong tenant returns not-found.
// Scenario: L2-Tenant-02 - Cross-tenant get_group invisible
#[tokio::test]
async fn tenant_isolation_get_group_cross_tenant_invisible() {
    let db = test_db().await;
    let type_svc = common::make_type_service(db.clone());
    let group_svc = make_group_service(db.clone());

    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let ctx_a = make_ctx(tenant_a);
    let ctx_b = make_ctx(tenant_b);

    // Create type
    let type_code = common::create_root_type(&type_svc, "xget").await.code;

    // Tenant A creates a group
    let ga =
        common::create_root_group(&group_svc, &ctx_a, &type_code, "A's secret group", tenant_a)
            .await;

    // Tenant B tries to get tenant A's group → should fail
    let result = group_svc.get_group(&ctx_b, ga.id).await;
    assert!(
        result.is_err(),
        "Tenant B should not be able to get tenant A's group"
    );
}

/// Full chain: `get_group_descendants` respects tenant scope.
// Scenario: L2-Tenant-03 - Hierarchy queries are tenant-scoped
#[tokio::test]
async fn tenant_isolation_hierarchy_scoped() {
    let db = test_db().await;
    let type_svc = common::make_type_service(db.clone());
    let group_svc = make_group_service(db.clone());

    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let ctx_a = make_ctx(tenant_a);
    let ctx_b = make_ctx(tenant_b);

    // Create parent and child types
    let parent_type = common::create_root_type(&type_svc, "hierp").await.code;
    let child_type = common::create_child_type(&type_svc, "hierc", &[&parent_type], &[])
        .await
        .code;

    // Tenant A: parent + child
    let parent =
        common::create_root_group(&group_svc, &ctx_a, &parent_type, "A Parent", tenant_a).await;
    let _child = common::create_child_group(
        &group_svc,
        &ctx_a,
        &child_type,
        parent.id,
        "A Child",
        tenant_a,
    )
    .await;

    // Tenant B: unrelated group (same parent type, different tenant)
    let _b_group =
        common::create_root_group(&group_svc, &ctx_b, &parent_type, "B Unrelated", tenant_b).await;

    // Tenant A lists hierarchy from parent — should NOT include B's group
    let query = ODataQuery::default();
    let hier = group_svc
        .get_group_descendants(&ctx_a, parent.id, &query)
        .await
        .expect("list hierarchy for tenant A");

    let hier_names: Vec<&str> = hier.items.iter().map(|g| g.name.as_str()).collect();
    assert!(
        hier_names.contains(&"A Parent"),
        "hierarchy should contain parent"
    );
    assert!(
        hier_names.contains(&"A Child"),
        "hierarchy should contain child"
    );
    assert!(
        !hier_names.iter().any(|n| n.contains("B Unrelated")),
        "hierarchy must NOT contain tenant B's group, got: {hier_names:?}"
    );

    // Reverse direction: tenant B must not be able to peek into tenant A's
    // subtree. The access-scope layer enforces isolation by returning an
    // empty result rather than leaking rows (so existence of the parent is
    // not disclosed). Either an explicit error or an empty page is an
    // acceptable form of denial; leakage of A's groups is not.
    let cross_tenant = group_svc
        .get_group_descendants(&ctx_b, parent.id, &query)
        .await;
    if let Ok(page) = cross_tenant {
        // The success path of cross-tenant hierarchy reads is access-scope's
        // "deny via empty page" pattern (existence is not disclosed). Anything
        // else -- including B's own groups bleeding through because the service
        // ignored the requested root -- is a regression. Asserting strict
        // emptiness is stronger than the previous "no A names leaked" check.
        assert!(
            page.items.is_empty(),
            "cross-tenant hierarchy lookup must return an empty page, got: {:?}",
            page.items
                .iter()
                .map(|g| (g.id, g.name.clone()))
                .collect::<Vec<_>>(),
        );
    }
    // Err path: explicit denial is equally acceptable.
}

/// Full chain: `update_group` with wrong tenant returns not-found.
// Scenario: L2-Tenant-04 - Cross-tenant update blocked
#[tokio::test]
async fn tenant_isolation_update_cross_tenant_blocked() {
    let db = test_db().await;
    let type_svc = common::make_type_service(db.clone());
    let group_svc = make_group_service(db.clone());

    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let ctx_a = make_ctx(tenant_a);
    let ctx_b = make_ctx(tenant_b);

    let type_code = common::create_root_type(&type_svc, "xupd").await.code;

    // Tenant A creates a group
    let ga = common::create_root_group(&group_svc, &ctx_a, &type_code, "A's group", tenant_a).await;

    // Tenant B tries to update tenant A's group → should fail
    let result = group_svc
        .update_group(
            &ctx_b,
            ga.id,
            resource_group_sdk::UpdateGroupRequest {
                name: "Hijacked!".to_owned(),
                parent_id: None,
                metadata: None,
            },
        )
        .await;
    assert!(
        result.is_err(),
        "Tenant B should not be able to update tenant A's group"
    );

    // `is_err()` alone does not catch a partial write followed by an error —
    // re-read the row as tenant A and verify the original state is untouched.
    let after = group_svc
        .get_group(&ctx_a, ga.id)
        .await
        .expect("tenant A must still see the original group");
    assert_eq!(after.name, "A's group", "name must not have been hijacked");
    assert_eq!(after.metadata, None, "metadata must remain None");
    assert_eq!(
        after.hierarchy.parent_id, None,
        "parent_id must remain None"
    );
}

/// A cross-tenant `update_group` must not become an existence oracle.
///
/// `update_group` reads the row before it opens its transaction, and that
/// read builds `system_scope()` -- it answers for a group in any tenant. If
/// metadata validation ran on the strength of that read alone, a caller in
/// tenant B would get a *validation* error for a group that exists in
/// tenant A and a *not-found* for an id that exists nowhere: two different
/// answers, which is exactly how one learns that the foreign group is real.
/// The scoped read has to come first, so both cases answer the same way.
#[tokio::test]
async fn tenant_isolation_update_cross_tenant_is_not_an_existence_oracle() {
    let db = test_db().await;
    let type_svc = common::make_type_service(db.clone());
    let group_svc = make_group_service(db.clone());
    // Every metadata validation on this one fails, so the only thing that can
    // keep the two cases indistinguishable is the order of the two reads.
    let oracle_svc = make_group_service_with_unavailable_registry(db.clone());

    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let ctx_a = make_ctx(tenant_a);
    let ctx_b = make_ctx(tenant_b);

    let type_code = common::create_root_type(&type_svc, "xoracle").await.code;
    let ga = common::create_root_group(&group_svc, &ctx_a, &type_code, "A's group", tenant_a).await;

    let req = || resource_group_sdk::UpdateGroupRequest {
        name: "Probe".to_owned(),
        parent_id: None,
        metadata: Some(serde_json::json!({"probe": true})),
    };

    let existing = oracle_svc
        .update_group(&ctx_b, ga.id, req())
        .await
        .expect_err("tenant B must not update tenant A's group");
    let unknown = oracle_svc
        .update_group(&ctx_b, Uuid::now_v7(), req())
        .await
        .expect_err("an unknown id must not update anything either");

    assert!(
        matches!(
            existing,
            resource_group::domain::error::DomainError::GroupNotFound { .. }
        ),
        "a group in another tenant must answer not-found, not a metadata \
         validation error that proves it exists: {existing}"
    );
    assert!(
        matches!(
            unknown,
            resource_group::domain::error::DomainError::GroupNotFound { .. }
        ),
        "an unknown id must answer not-found: {unknown}"
    );
}

/// Full chain: `delete_group` with wrong tenant returns not-found.
// Scenario: L2-Tenant-05 - Cross-tenant delete blocked
#[tokio::test]
async fn tenant_isolation_delete_cross_tenant_blocked() {
    let db = test_db().await;
    let type_svc = common::make_type_service(db.clone());
    let group_svc = make_group_service(db.clone());

    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let ctx_a = make_ctx(tenant_a);
    let ctx_b = make_ctx(tenant_b);

    let type_code = common::create_root_type(&type_svc, "xdel").await.code;

    // Tenant A creates a group
    let ga = common::create_root_group(
        &group_svc,
        &ctx_a,
        &type_code,
        "A's group to delete",
        tenant_a,
    )
    .await;

    // Tenant B tries to delete tenant A's group → should fail
    let result = group_svc.delete_group(&ctx_b, ga.id, false).await;
    assert!(
        result.is_err(),
        "Tenant B should not be able to delete tenant A's group"
    );

    // Tenant A can still see and delete their own group
    let own = group_svc.get_group(&ctx_a, ga.id).await;
    assert!(own.is_ok(), "Tenant A should still see their group");

    let del = group_svc.delete_group(&ctx_a, ga.id, false).await;
    assert!(
        del.is_ok(),
        "Tenant A should be able to delete their own group"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 2: Group-based predicate tests (InGroup / InGroupSubtree)
// ═══════════════════════════════════════════════════════════════════════

/// Mock `AuthZ` that returns tenant scoping + `InGroup` predicate.
/// Simulates S14 scenario: user has access to specific group IDs.
struct GroupScopingAuthZ {
    /// Groups the subject has access to (injected per test).
    allowed_group_ids: Vec<Uuid>,
}

#[async_trait]
impl AuthZResolverApi for GroupScopingAuthZ {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant_id = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("subject must have tenant_id");

        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![
                        // Tenant scoping (always present)
                        Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            [tenant_id],
                        )),
                        // Group membership scoping
                        Predicate::InGroup(InGroupPredicate::new(
                            pep_properties::RESOURCE_ID,
                            self.allowed_group_ids.clone(),
                        )),
                    ],
                }],
                deny_reason: None,
            },
        })
    }
}

/// Phase 2 test: `RG_GROUP_RESOURCE` carries no RG member-handle type
/// mapping (groups nest through `parent_id`/`resource_group_closure`, not
/// membership rows, and `gts.cf.core.rg.group.v1~` can never be registered
/// in `gts_type`), so `PolicyEnforcer` suppresses the configured
/// `GroupMembership` capability per request and a PDP that returns a native
/// `InGroup` predicate anyway is rejected fail-closed with the typed
/// unadvertised-capability error — never silently compiled into a
/// zero-row membership subquery.
// Scenario: L2-Tenant-06 - InGroup predicate against groups fails closed (S14)
#[tokio::test]
async fn group_based_in_group_predicate_against_groups_fails_closed() {
    let group_a = Uuid::now_v7();
    let group_b = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();

    let authz: Arc<dyn AuthZResolverApi> = Arc::new(GroupScopingAuthZ {
        allowed_group_ids: vec![group_a, group_b],
    });
    let enforcer = PolicyEnforcer::new(authz)
        .with_capabilities(vec![authz_resolver_sdk::Capability::GroupMembership]);
    let ctx = make_ctx(tenant_id);

    let error = enforcer
        .access_scope(
            &ctx,
            &resource_group::domain::group_service::RG_GROUP_RESOURCE,
            "list",
            None,
        )
        .await
        .expect_err("an InGroup predicate against the unmapped group resource must fail closed");

    let authz_resolver_sdk::pep::EnforcerError::CompileFailed(
        authz_resolver_sdk::pep::ConstraintCompileError::UnadvertisedCapabilities {
            predicate,
            missing,
        },
    ) = error
    else {
        panic!("expected typed unadvertised-capability error, got: {error:?}");
    };
    assert_eq!(predicate, "InGroup");
    assert_eq!(missing, vec!["group_membership"]);
}

/// Phase 2 test: memberships seeded into DB, verify that group-based
/// membership data is correctly stored and accessible.
///
/// This verifies the data layer works with the membership table that
/// `InGroup` subqueries reference. The actual subquery SQL execution
/// is validated by the `SecureORM` cond.rs unit tests.
// Scenario: L2-Membership-01 - Membership data correctly stored
#[tokio::test]
async fn group_based_membership_data_correctly_stored() {
    let db = test_db().await;
    let type_svc = common::make_type_service(db.clone());
    let group_svc = make_group_service(db.clone());

    let tenant = Uuid::now_v7();
    let ctx = make_ctx(tenant);

    // Create types: project (root, allows "task" members) and task
    let project_type = format!(
        "{GTS_ID_PREFIX}cf.core.rg.type.v1~x.test.proj.i{}.v1~",
        Uuid::now_v7().as_simple()
    );
    let task_type = format!(
        "{GTS_ID_PREFIX}cf.core.rg.type.v1~x.test.task.i{}.v1~",
        Uuid::now_v7().as_simple()
    );

    // Create task type first (project references it in allowed_membership_types)
    type_svc
        .create_type_unscoped(resource_group_sdk::CreateTypeRequest {
            code: task_type.clone(),
            can_be_root: true,
            allowed_parent_types: vec![],
            allowed_membership_types: vec![],
            metadata_schema: None,
        })
        .await
        .expect("create task type");

    type_svc
        .create_type_unscoped(resource_group_sdk::CreateTypeRequest {
            code: project_type.clone(),
            can_be_root: true,
            allowed_parent_types: vec![],
            allowed_membership_types: vec![task_type.clone()],
            metadata_schema: None,
        })
        .await
        .expect("create project type");

    // Create ProjectA and ProjectB
    let project_a = group_svc
        .create_group(
            &ctx,
            resource_group_sdk::CreateGroupRequest::new(
                project_type.clone(),
                "ProjectA".to_owned(),
            ),
            tenant,
        )
        .await
        .expect("create ProjectA");

    let project_b = group_svc
        .create_group(
            &ctx,
            resource_group_sdk::CreateGroupRequest::new(project_type, "ProjectB".to_owned()),
            tenant,
        )
        .await
        .expect("create ProjectB");

    // Add memberships via MembershipService (with PolicyEnforcer)
    let authz: Arc<dyn AuthZResolverApi> = Arc::new(TenantScopingAuthZ);
    let enforcer = PolicyEnforcer::new(authz);
    let membership_svc = resource_group::domain::membership_service::MembershipService::new(
        db.clone(),
        enforcer,
        Arc::new(GroupRepository),
        Arc::new(TypeRepository),
        Arc::new(MembershipRepository),
    );

    // task-001, task-002 → ProjectA
    membership_svc
        .add_membership(&ctx, project_a.id, &task_type, "task-001")
        .await
        .expect("add task-001 to ProjectA");
    membership_svc
        .add_membership(&ctx, project_a.id, &task_type, "task-002")
        .await
        .expect("add task-002 to ProjectA");

    // task-003 → ProjectB
    membership_svc
        .add_membership(&ctx, project_b.id, &task_type, "task-003")
        .await
        .expect("add task-003 to ProjectB");

    // List memberships for ProjectA
    let query = ODataQuery::default();
    let all = membership_svc
        .list_memberships(&ctx, &query)
        .await
        .expect("list memberships");

    let project_a_members: Vec<&str> = all
        .items
        .iter()
        .filter(|m| m.group_id == project_a.id)
        .map(|m| m.resource_id.as_str())
        .collect();

    assert!(project_a_members.contains(&"task-001"));
    assert!(project_a_members.contains(&"task-002"));
    assert!(!project_a_members.contains(&"task-003"));

    let members_of_b: Vec<&str> = all
        .items
        .iter()
        .filter(|m| m.group_id == project_b.id)
        .map(|m| m.resource_id.as_str())
        .collect();

    assert!(members_of_b.contains(&"task-003"));
    assert!(!members_of_b.contains(&"task-001"));
}
