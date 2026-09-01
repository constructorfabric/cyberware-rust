#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::unnecessary_wraps
)]
//! Vertical assignment-scope tests through the production RBAC evaluator,
//! production RBAC local client, plugin policy adapter, hierarchy materializer,
//! and constraint generator.
//!
//! Unlike the plugin's scripted-RBAC tests, these tests seed typed role
//! assignment and role definition domain rows. They exercise the production
//! evaluator and local-client boundaries, but deliberately replace the SQL
//! candidate query with [`SeededAssignmentRepo`]; `PostgreSQL` narrowing and row
//! projection are covered by the RBAC repository integration suite.

#![allow(unknown_lints, de0901_gts_string_pattern)]
mod common;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::{
    EvaluationRequestBuilder, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
};
use authz_resolver_sdk::constraints::Predicate;
use authz_resolver_sdk::models::{BarrierMode, TenantContext, TenantMode};
use chrono::Utc;
use rbac::api::service::local_client::RbacServiceLocalClient;
use rbac::domain::DomainError;
use rbac::domain::etag::Etag;
use rbac::domain::metrics::NoopMetrics;
use rbac::domain::model::{RoleAssignmentModel, RoleDefinitionModel};
use rbac::domain::permission_evaluator::PermissionEvaluator;
use rbac::domain::rg_port::{RbacRgGroup, RbacRgMembership, RbacRgRead, RbacRgReadError};
use rbac::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository, SubjectAssignmentsQuery, VisibilityFilter,
};
use rbac::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionPatch, RoleDefinitionRepository, RoleDefinitionVisibility,
    RoleTypeCounts,
};
use rbac_sdk::RbacServiceClientV1;
use rbac_sdk::models::{PermissionRule, PrincipalType, Scope};
use resource_group_sdk::models::{
    GroupHierarchyWithDepth, ResourceGroupMembership, ResourceGroupWithDepth,
};
use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantStatus};
use toolkit::Gear;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const RESOURCE_TYPE: &str = "gts.cf.core.resources.test.v1~";
const SERVICE_PRINCIPAL_TYPE: &str = "gts.cf.core.security.subject_service_principal.v1~";
const OWNER_TENANT_ID: &str = "owner_tenant_id";
const RESOURCE_ID: &str = "id";

/// Read-only assignment repository that applies the evaluator's production
/// candidate-query contract to a small set of pre-seeded domain rows.
struct SeededAssignmentRepo {
    rows: Vec<RoleAssignmentModel>,
}

#[async_trait]
impl RoleAssignmentRepository for SeededAssignmentRepo {
    async fn create<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError> {
        panic!("vertical scope test does not create assignments through the repository")
    }

    async fn count_by_role<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError> {
        // Decorates the role-definitions read API; an authorization decision
        // never counts rows, so reaching this from the evaluator would mean
        // the permission path had grown a dependency it must not have.
        panic!("a permission evaluation must not count assignments")
    }

    async fn find_by_id<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError> {
        Ok(self.rows.iter().find(|row| row.id == id).cloned())
    }

    async fn list<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _query: &ODataQuery,
    ) -> Result<Page<RoleAssignmentModel>, DomainError> {
        panic!("vertical scope test does not list assignments")
    }

    async fn get_subject_assignments<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError> {
        let rows =
            self.rows
                .iter()
                .filter(|row| {
                    let direct_match = query.user_principal.as_ref().is_some_and(
                        |(principal_type, principal_id)| {
                            row.principal_type == *principal_type
                                && row.principal_id == *principal_id
                        },
                    );
                    let group_match = row.principal_type == PrincipalType::Group
                        && query
                            .group_principals
                            .iter()
                            .any(|group| group == &row.principal_id);
                    if !direct_match && !group_match {
                        return false;
                    }
                    if query.all_scopes {
                        return true;
                    }
                    let path = row.scope.path();
                    query.ancestor_scopes.contains(&path)
                        || query
                            .context_tenant_rg_prefix
                            .strip_suffix('%')
                            .is_some_and(|prefix| path.starts_with(prefix))
                })
                .cloned()
                .collect();
        Ok(rows)
    }

    async fn delete<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<bool, DomainError> {
        panic!("vertical scope test does not delete assignments")
    }
}

/// Read-only role-definition repository backing the evaluator's batched
/// projection from assignments to effective permissions.
struct SeededDefinitionRepo {
    rows: Vec<RoleDefinitionModel>,
}

#[async_trait]
impl RoleDefinitionRepository for SeededDefinitionRepo {
    async fn create<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError> {
        panic!("vertical scope test does not create role definitions")
    }

    async fn count_by_type<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
    ) -> Result<RoleTypeCounts, DomainError> {
        // Serves the catalogue summary endpoint. Like the assignment count
        // above, it has no place on an authorization path.
        panic!("a permission evaluation must not summarise the role catalogue")
    }

    async fn find_by_id<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError> {
        Ok(self.rows.iter().find(|row| row.id == id).cloned())
    }

    async fn find_by_ids<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        Ok(self
            .rows
            .iter()
            .filter(|row| ids.contains(&row.id))
            .cloned()
            .collect())
    }

    async fn list<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
        _query: &ODataQuery,
    ) -> Result<Page<RoleDefinitionModel>, DomainError> {
        panic!("vertical scope test does not list role definitions")
    }

    async fn update<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _patch: RoleDefinitionPatch,
        _expected_etag: &Etag,
    ) -> Result<RoleDefinitionModel, DomainError> {
        panic!("vertical scope test does not update role definitions")
    }

    async fn delete<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _expected_etag: &Etag,
    ) -> Result<(), DomainError> {
        panic!("vertical scope test does not delete role definitions")
    }

    async fn count_assignments_for_role<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<u64, DomainError> {
        Ok(0)
    }
}

/// The service-principal test subject does not require group-membership
/// expansion, so reaching either RBAC RG method would indicate an evaluator
/// regression rather than test data that should be invented here.
struct NoopRbacRgRead;

#[async_trait]
impl RbacRgRead for NoopRbacRgRead {
    async fn get_group(
        &self,
        _ctx: &SecurityContext,
        _id: Uuid,
    ) -> Result<RbacRgGroup, RbacRgReadError> {
        panic!("permission evaluation must not validate assignment creation scope")
    }

    /// Display-name resolution belongs to the role-assignment read path,
    /// never to permission evaluation.
    async fn group_names(
        &self,
        _ctx: &SecurityContext,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, RbacRgReadError> {
        panic!("permission evaluation must not resolve group display names")
    }

    async fn list_memberships(
        &self,
        _ctx: &SecurityContext,
        _query: &ODataQuery,
    ) -> Result<Page<RbacRgMembership>, RbacRgReadError> {
        panic!("service-principal permission evaluation must not resolve group principals")
    }
}

fn tenant(id: Uuid, parent_id: Option<Uuid>) -> TenantInfo {
    TenantInfo {
        id: TenantId(id),
        name: format!("tenant-{id}"),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: parent_id.map(TenantId),
        self_managed: false,
    }
}

/// Construct the real RBAC producer and local SDK adapter from one persisted
/// assignment shape, then initialize the plugin around that adapter.
async fn init_vertical_plugin(
    subject_id: Uuid,
    assignment_scope: Scope,
    tenant_resolver: Arc<InMemoryTenantResolverClient>,
    resource_group: Arc<InMemoryResourceGroupClient>,
) -> Arc<dyn authz_resolver_sdk::AuthZResolverPluginClient> {
    let role_id = Uuid::now_v7();
    let now = Utc::now();
    let assignment_repo = Arc::new(SeededAssignmentRepo {
        rows: vec![RoleAssignmentModel {
            id: Uuid::now_v7(),
            role_definition_id: role_id,
            principal_id: subject_id.to_string(),
            principal_type: PrincipalType::ServicePrincipal,
            scope: assignment_scope,
            created_at: now,
            updated_at: now,
            created_by: "vertical-test".to_owned(),
            // Scope provenance does not involve the author identity.
            created_by_type: None,
            created_by_tenant_id: None,
        }],
    });
    let definition_repo = Arc::new(SeededDefinitionRepo {
        rows: vec![RoleDefinitionModel {
            id: role_id,
            name: "Scoped Reader".to_owned(),
            description: None,
            is_built_in: true,
            permissions: vec![PermissionRule::new("read", RESOURCE_TYPE)],
            not_permissions: Vec::new(),
            assignable_scopes: vec![Scope::root()],
            owner_tenant_id: None,
            created_at: now,
            updated_at: now,
            created_by: "vertical-test".to_owned(),
        }],
    });
    let tr_for_rbac: Arc<dyn tenant_resolver_sdk::TenantResolverClient> = tenant_resolver.clone();
    // The stub repos ignore the executor, but the evaluator still needs a
    // connection source: an unmigrated in-memory database is never queried.
    let db = toolkit_db::connect_db("sqlite::memory:", toolkit_db::ConnectOpts::default())
        .await
        .expect("in-memory sqlite must open");
    let provider: toolkit_db::DBProvider<toolkit_db::DbError> = toolkit_db::DBProvider::new(db);
    let evaluator = Arc::new(PermissionEvaluator::new(
        provider,
        assignment_repo,
        definition_repo,
        tr_for_rbac,
        Arc::new(NoopRbacRgRead),
        Arc::new(NoopMetrics),
    ));
    let rbac: Arc<dyn RbacServiceClientV1> = Arc::new(RbacServiceLocalClient::new(evaluator));
    let (ctx, hub, registry, _tenant_resolver, _resource_group) =
        common::build_ctx_with_rbac_client(rbac, tenant_resolver, resource_group);
    // `gts_validation.mode` defaults to `strict`, and this file uses its own
    // subject / resource type ids rather than the builder defaults, so they
    // have to be Known for the vertical RBAC path to be reachable at all.
    registry.add_known_types(vec![SERVICE_PRINCIPAL_TYPE, RESOURCE_TYPE]);
    AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect("plugin must initialize around the real RBAC local client");
    common::resolve_plugin(&hub)
}

fn request(subject_id: Uuid, tenant_id: Uuid) -> authz_resolver_sdk::EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_subject_id(subject_id)
        .with_subject_type(Some(SERVICE_PRINCIPAL_TYPE.to_owned()))
        .with_subject_tenant_id(tenant_id)
        .with_action_name("read")
        .with_resource_type(RESOURCE_TYPE)
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec![OWNER_TENANT_ID.to_owned(), RESOURCE_ID.to_owned()])
        .with_tenant_context(Some(TenantContext {
            mode: TenantMode::Subtree,
            root_id: Some(tenant_id),
            barrier_mode: BarrierMode::Respect,
            tenant_status: None,
        }))
        .build()
}

#[tokio::test]
async fn tenant_assignment_reaches_plugin_as_only_its_tenant_constraint() {
    let root_id = Uuid::from_u128(1);
    let tenant_id = Uuid::from_u128(2);
    let subject_id = Uuid::from_u128(4);
    let tenant_resolver = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        tenant(root_id, None),
        tenant(tenant_id, Some(root_id)),
    ]));
    tenant_resolver.add_descendants(TenantId(tenant_id), Vec::new());
    let plugin = init_vertical_plugin(
        subject_id,
        Scope::tenant(tenant_id),
        tenant_resolver.clone(),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
    .await;

    let response = plugin
        .evaluate(request(subject_id, tenant_id))
        .await
        .expect("tenant-scoped evaluation must complete");

    assert!(response.decision);
    assert_eq!(
        tenant_resolver.root_call_count(),
        0,
        "tenant assignment must never enter platform-root materialization"
    );
    assert_eq!(response.context.constraints.len(), 1);
    assert_eq!(response.context.constraints[0].predicates.len(), 1);
    match &response.context.constraints[0].predicates[0] {
        Predicate::Eq(predicate) => {
            assert_eq!(predicate.property, OWNER_TENANT_ID);
            assert_eq!(predicate.value, serde_json::json!(tenant_id));
        }
        other => panic!("expected exact tenant Eq constraint, got {other:?}"),
    }
}

#[tokio::test]
async fn resource_group_assignment_reaches_plugin_as_group_and_owner_constraints() {
    let root_id = Uuid::from_u128(10);
    let tenant_id = Uuid::from_u128(11);
    let subject_id = Uuid::from_u128(12);
    let group_id = Uuid::from_u128(13);
    let member_id = Uuid::from_u128(14);
    let tenant_resolver = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        tenant(root_id, None),
        tenant(tenant_id, Some(root_id)),
    ]));
    let resource_group = Arc::new(InMemoryResourceGroupClient::with_group_descendants(
        group_id,
        vec![ResourceGroupWithDepth {
            id: group_id,
            code: "gts.cf.core.rg.type.v1~test.v1~".to_owned(),
            name: "assigned-group".to_owned(),
            hierarchy: GroupHierarchyWithDepth {
                parent_id: None,
                tenant_id,
                depth: 0,
            },
            metadata: None,
        }],
    ));
    resource_group.add_memberships(vec![ResourceGroupMembership {
        group_id,
        resource_type: RESOURCE_TYPE.to_owned(),
        resource_id: member_id.to_string(),
    }]);
    let plugin = init_vertical_plugin(
        subject_id,
        Scope::resource_group(tenant_id, group_id),
        tenant_resolver.clone(),
        resource_group,
    )
    .await;

    let response = plugin
        .evaluate(request(subject_id, tenant_id))
        .await
        .expect("resource-group-scoped evaluation must complete");

    assert!(response.decision);
    assert_eq!(
        tenant_resolver.root_call_count(),
        0,
        "resource-group assignment must never enter platform-root materialization"
    );
    assert_eq!(response.context.constraints.len(), 1);
    let predicates = &response.context.constraints[0].predicates;
    assert_eq!(predicates.len(), 2);
    match &predicates[0] {
        Predicate::In(predicate) => {
            assert_eq!(predicate.property, RESOURCE_ID);
            assert_eq!(predicate.values, vec![serde_json::json!(member_id)]);
        }
        other => panic!("expected resource-id In constraint, got {other:?}"),
    }
    match &predicates[1] {
        Predicate::Eq(predicate) => {
            assert_eq!(predicate.property, OWNER_TENANT_ID);
            assert_eq!(predicate.value, serde_json::json!(tenant_id));
        }
        other => panic!("expected owning-tenant Eq constraint, got {other:?}"),
    }
}
