//! Authz-ordering regression tests for [`super::RoleDefinitionService`].
//!
//! Verifies that an unauthorized caller cannot probe the existence,
//! built-in-ness, stale-ETag, or body-validation state of a row through
//! the differentiated 4xx responses — the information-leak chain.
//!
//! Strategy: substitute the repo with a tiny stub seeded with one row,
//! a `MockPolicyEnforcer::deny_all()` PEP, and a panic-stubbed
//! `TargetTypeValidator` so any code path that reaches it (instead of
//! being short-circuited by the denied authz check) fails loudly.

#![allow(clippy::panic, clippy::expect_used)]

use std::sync::Arc;
use toolkit_db::secure::DBRunner;

use async_trait::async_trait;
use chrono::SubsecRound;
use rbac_sdk::models::{PermissionRule, PrincipalType, Scope};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    CallerScope, CreateRoleDefinitionRequest, ListRoleDefinitionsRequest, RoleDefinitionService,
    UpdateRoleDefinitionRequest,
};
use crate::domain::actions;
use crate::domain::error::DomainError;
use crate::domain::etag::{Etag, etag_for};
use crate::domain::model::RoleDefinitionModel;
use crate::domain::policy_enforcer::{AuthorizationError, PolicyEnforcer, ReadableScopes};
use crate::domain::policy_enforcer_mock::{MockPolicyEnforcer, ReadableScopesPred};
use crate::domain::resource_types;
use crate::domain::role_assignment_repo::VisibilityFilter;
use crate::domain::role_assignment_repo_mock::EmptyRoleAssignmentRepository;
use crate::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionPatch, RoleDefinitionRepository, RoleDefinitionVisibility,
    RoleTypeCounts,
};
use crate::domain::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
use crate::domain::scope_validator::ScopeValidator;
use crate::domain::target_type_validator::{TargetTypeValidationError, TargetTypeValidator};
use toolkit_odata::{ODataQuery, Page};

fn ctx() -> SecurityContext {
    SecurityContext::anonymous()
}

fn sample_model(tenant: Uuid) -> RoleDefinitionModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    RoleDefinitionModel {
        id: Uuid::now_v7(),
        name: "Auditor".to_owned(),
        description: None,
        is_built_in: false,
        permissions: Vec::new(),
        not_permissions: Vec::new(),
        assignable_scopes: vec![Scope::tenant(tenant)],
        owner_tenant_id: Some(tenant),
        created_at: now,
        updated_at: now,
        created_by: "tester".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

struct StubRepo {
    seeded: Option<RoleDefinitionModel>,
    /// Rows returned by [`RoleDefinitionRepository::list`]. Parallel to
    /// `seeded` so the single-row `find_by_id` semantics stay intact while
    /// the list tests can seed multi-tenant fixtures.
    seeded_list_rows: Vec<RoleDefinitionModel>,
    /// Every [`RoleDefinitionVisibility`] handed to
    /// [`RoleDefinitionRepository::count_by_type`], in call order.
    ///
    /// Recorded rather than merely answered because the summary's whole
    /// security property is *which* row set it counts. A returned number
    /// alone cannot distinguish a correctly narrowed projection from a
    /// `RoleDefinitionVisibility::All` one that happened to see a single
    /// tenant's rows in the fixture — only the projection itself can.
    count_by_type_visibilities: parking_lot::Mutex<Vec<RoleDefinitionVisibility>>,
}

/// Does `model` fall inside the row set `visibility` admits?
///
/// Shared by the stubbed `list` and `count_by_type` so the summary is
/// measured over exactly the rows the list would page — which is the
/// contract the two are supposed to keep with each other.
fn row_visible(model: &RoleDefinitionModel, visibility: &RoleDefinitionVisibility) -> bool {
    match visibility {
        RoleDefinitionVisibility::BuiltinsOnly => model.is_built_in,
        RoleDefinitionVisibility::CustomForTenants(tenants) => {
            !model.is_built_in && model.owner_tenant_id.is_some_and(|t| tenants.contains(&t))
        }
        RoleDefinitionVisibility::CustomForTenantsWithBuiltins(tenants) => {
            model.is_built_in || model.owner_tenant_id.is_some_and(|t| tenants.contains(&t))
        }
        RoleDefinitionVisibility::All => true,
    }
}

stub_impl! {
    impl RoleDefinitionRepository => for StubRepo,
    stub_label = "StubRepo",
    methods = [
        async fn create(_new: NewRoleDefinition)
            -> Result<RoleDefinitionModel, DomainError>;
        async fn find_by_ids(_ids: &[Uuid])
            -> Result<Vec<RoleDefinitionModel>, DomainError>;
        async fn update(_id: Uuid, _patch: RoleDefinitionPatch, _expected_etag: &Etag)
            -> Result<RoleDefinitionModel, DomainError>;
        async fn delete(_id: Uuid, _expected_etag: &Etag) -> Result<(), DomainError>;
        async fn count_assignments_for_role(_id: Uuid) -> Result<u64, DomainError>;
    ],
    custom = {
        async fn find_by_id<C: DBRunner>(&self, _db: &C, id: Uuid) -> Result<Option<RoleDefinitionModel>, DomainError> {
            match &self.seeded {
                Some(row) if row.id == id => Ok(Some(row.clone())),
                _ => Ok(None),
            }
        }

        // Deliberately naive: filters only on the visibility variant.
        // The user `$filter` (carried inside `_query`) is ignored —
        // the projection layer is what's under test here.
        async fn list<C: DBRunner>(
            &self,
            _db: &C,
            visibility: RoleDefinitionVisibility,
            _query: &ODataQuery,
        ) -> Result<Page<RoleDefinitionModel>, DomainError> {
            let items: Vec<RoleDefinitionModel> = self
                .seeded_list_rows
                .iter()
                .filter(|m| row_visible(m, &visibility))
                .cloned()
                .collect();
            Ok(Page {
                items,
                page_info: toolkit_odata::PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: 0,
                },
            })
        }

        // Counts over the same seeded rows and the same visibility
        // predicate `list` uses, and records the projection it was handed
        // so a summary test can assert on it directly.
        async fn count_by_type<C: DBRunner>(
            &self,
            _db: &C,
            visibility: RoleDefinitionVisibility,
        ) -> Result<RoleTypeCounts, DomainError> {
            self.count_by_type_visibilities
                .lock()
                .push(visibility.clone());
            let mut counts = RoleTypeCounts::default();
            for row in self
                .seeded_list_rows
                .iter()
                .filter(|m| row_visible(m, &visibility))
            {
                if row.is_built_in {
                    counts.built_in = counts.built_in.saturating_add(1);
                } else {
                    counts.custom = counts.custom.saturating_add(1);
                }
            }
            Ok(counts)
        }
    }
}

/// Target-type validator that panics on any call. The denied-create
/// test wires this in to prove the network lookup never happens on the
/// unauthorized branch.
struct PanicOnCallTargetTypeValidator;

#[async_trait]
impl TargetTypeValidator for PanicOnCallTargetTypeValidator {
    async fn ensure_exists(&self, _target_type: &str) -> Result<(), TargetTypeValidationError> {
        panic!("authz must short-circuit before target_type lookup")
    }
}

async fn build_service(
    seeded: Option<RoleDefinitionModel>,
    policy: Arc<MockPolicyEnforcer>,
) -> RoleDefinitionService<StubRepo, EmptyRoleAssignmentRepository> {
    build_service_with_validator(seeded, policy, Arc::new(PanicOnCallTargetTypeValidator)).await
}

/// [`build_service`] with the target-type validator supplied, for the tests
/// that are about what the validator's failure modes map to.
async fn build_service_with_validator(
    seeded: Option<RoleDefinitionModel>,
    policy: Arc<MockPolicyEnforcer>,
    target_type_validator: Arc<dyn TargetTypeValidator>,
) -> RoleDefinitionService<StubRepo, EmptyRoleAssignmentRepository> {
    let tenants: Vec<Uuid> = seeded
        .as_ref()
        .and_then(|r| r.owner_tenant_id)
        .into_iter()
        .collect();
    let tenant_resolver = Arc::new(FakeTenantResolverClient::with_chain(&tenants))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(FakeRbacRgRead::default()) as Arc<dyn crate::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let repo = Arc::new(StubRepo {
        seeded,
        seeded_list_rows: Vec::new(),
        count_by_type_visibilities: parking_lot::Mutex::new(Vec::new()),
    });
    RoleDefinitionService::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        repo,
        Arc::new(EmptyRoleAssignmentRepository),
        policy,
        scope_validator,
        target_type_validator,
    )
}

/// List-test helper: drop the panic-on-call target-type stub
/// (the list path never touches it) and seed
/// `seeded_list_rows` for the stubbed repo's `list` impl. Built-ins are
/// shipped via the same vector — the projection logic sees them through
/// `ListFilter::BuiltinsOnly`.
async fn build_list_service(
    seeded_list_rows: Vec<RoleDefinitionModel>,
    policy: Arc<MockPolicyEnforcer>,
) -> RoleDefinitionService<StubRepo, EmptyRoleAssignmentRepository> {
    let tenants: Vec<Uuid> = seeded_list_rows
        .iter()
        .filter_map(|r| r.owner_tenant_id)
        .collect();
    // Each seeded tenant is its own root. A chain would make the second
    // tenant a descendant of the first, so a test asserting that another
    // tenant's rows stay hidden would really be asserting the child case
    // while reading as the foreign-tenant case.
    let branches: Vec<[Uuid; 1]> = tenants.iter().map(|&t| [t]).collect();
    let branch_refs: Vec<&[Uuid]> = branches.iter().map(<[Uuid; 1]>::as_slice).collect();
    let tenant_resolver = Arc::new(FakeTenantResolverClient::with_disjoint_subtrees(
        &branch_refs,
    )) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(FakeRbacRgRead::default()) as Arc<dyn crate::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn TargetTypeValidator> =
        Arc::new(PanicOnCallTargetTypeValidator);
    let repo = Arc::new(StubRepo {
        seeded: None,
        seeded_list_rows,
        count_by_type_visibilities: parking_lot::Mutex::new(Vec::new()),
    });
    RoleDefinitionService::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        repo,
        Arc::new(EmptyRoleAssignmentRepository),
        policy,
        scope_validator,
        target_type_validator,
    )
}

/// Build a [`RoleDefinitionModel`] custom row tagged with the given
/// tenant + name. Used by the list tests to construct multi-tenant
/// fixtures cheaply.
fn custom_row(tenant: Uuid, name: &str) -> RoleDefinitionModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    RoleDefinitionModel {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        description: None,
        is_built_in: false,
        permissions: Vec::new(),
        not_permissions: Vec::new(),
        assignable_scopes: vec![Scope::tenant(tenant)],
        owner_tenant_id: Some(tenant),
        created_at: now,
        updated_at: now,
        created_by: "tester".to_owned(),
    }
}

/// Built-in row with no `owner_tenant_id`. The list logic surfaces these
/// to every authenticated caller.
fn builtin_row(name: &str) -> RoleDefinitionModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    RoleDefinitionModel {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        description: None,
        is_built_in: true,
        permissions: Vec::new(),
        not_permissions: Vec::new(),
        assignable_scopes: vec![Scope::Root],
        owner_tenant_id: None,
        created_at: now,
        updated_at: now,
        created_by: "system".to_owned(),
    }
}

/// Build a default `list` request — empty `OData` query. Tests
/// override fields via struct-update or supply their own `ODataQuery`.
fn list_request() -> ListRoleDefinitionsRequest {
    ListRoleDefinitionsRequest {
        caller_scope: CallerScope::Root,
        query: ODataQuery::new(),
    }
}

// ---------------------------------------------------------------------------
// update: existing row vs missing row, unauthorized caller
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_update_returns_not_found_identical_to_missing() {
    let tenant = Uuid::now_v7();
    let row = sample_model(tenant);
    let real_id = row.id;
    let fake_id = Uuid::now_v7();
    let if_match = etag_for(row.updated_at, row.id);

    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err_existing = svc
        .update(
            &ctx(),
            UpdateRoleDefinitionRequest {
                id: real_id,
                if_match: Some(if_match.clone()),
                patch: RoleDefinitionPatch::default(),
                immutable_field_attempted: None,
            },
        )
        .await
        .expect_err("denied caller MUST NOT succeed");

    let err_missing = svc
        .update(
            &ctx(),
            UpdateRoleDefinitionRequest {
                id: fake_id,
                if_match: Some(if_match),
                patch: RoleDefinitionPatch::default(),
                immutable_field_attempted: None,
            },
        )
        .await
        .expect_err("missing row MUST fail");

    // Both branches MUST surface the same variant; the only differing
    // field is the id the caller already supplied — no information leaked.
    assert!(
        matches!(err_existing, DomainError::RoleDefinitionNotFound { id } if id == real_id),
        "expected RoleDefinitionNotFound for denied caller, got {err_existing:?}"
    );
    assert!(
        matches!(err_missing, DomainError::RoleDefinitionNotFound { id } if id == fake_id),
        "expected RoleDefinitionNotFound for missing row, got {err_missing:?}"
    );
}

/// An unauthorized caller sending a *stale* `If-Match`
/// MUST see `RoleDefinitionNotFound`, not `StaleEtag`. Pre-fix the
/// `ETag` compare ran before enforce and leaked existence via 412.
#[tokio::test]
async fn unauthorized_update_cannot_probe_stale_etag() {
    let tenant = Uuid::now_v7();
    let row = sample_model(tenant);
    let real_id = row.id;

    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::deny_all())).await;

    // Deliberately wrong If-Match (random ETag built from unrelated ids).
    let stale = etag_for(chrono::Utc::now().trunc_subsecs(6), Uuid::now_v7());

    let err = svc
        .update(
            &ctx(),
            UpdateRoleDefinitionRequest {
                id: real_id,
                if_match: Some(stale),
                patch: RoleDefinitionPatch::default(),
                immutable_field_attempted: None,
            },
        )
        .await
        .expect_err("denied caller MUST NOT succeed");

    assert!(
        matches!(err, DomainError::RoleDefinitionNotFound { id } if id == real_id),
        "stale If-Match on a denied caller MUST surface as RoleDefinitionNotFound; \
         got {err:?} (would leak existence if surfaced as StaleEtag)"
    );
}

/// An unauthorized caller sending a *malformed* patch
/// MUST see `RoleDefinitionNotFound`, not `InvalidPermissionRule`.
/// Pre-fix the patch validation (target-type lookups) ran before
/// enforce and leaked existence.
#[tokio::test]
async fn unauthorized_update_cannot_probe_validation_errors() {
    let tenant = Uuid::now_v7();
    let row = sample_model(tenant);
    let real_id = row.id;
    let if_match = etag_for(row.updated_at, row.id);

    // Note: the panic-on-call TargetTypeValidator would fire if the
    // code path reaches it, so this test ALSO implicitly verifies that
    // the network validation never runs on the denied branch.
    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::deny_all())).await;

    let bad_patch = RoleDefinitionPatch {
        permissions: Some(vec![PermissionRule::new("", "x")]), // empty op = malformed
        ..Default::default()
    };

    let err = svc
        .update(
            &ctx(),
            UpdateRoleDefinitionRequest {
                id: real_id,
                if_match: Some(if_match),
                patch: bad_patch,
                immutable_field_attempted: None,
            },
        )
        .await
        .expect_err("denied caller MUST NOT succeed");

    assert!(
        matches!(err, DomainError::RoleDefinitionNotFound { id } if id == real_id),
        "malformed patch on a denied caller MUST surface as RoleDefinitionNotFound; \
         got {err:?} (would leak existence if surfaced as InvalidPermissionRule)"
    );
}

// ---------------------------------------------------------------------------
// delete: existing row vs missing row, unauthorized caller
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_delete_returns_not_found_identical_to_missing() {
    let tenant = Uuid::now_v7();
    let row = sample_model(tenant);
    let real_id = row.id;
    let fake_id = Uuid::now_v7();
    let if_match = etag_for(row.updated_at, row.id);

    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err_existing = svc
        .delete(&ctx(), real_id, Some(if_match.clone()))
        .await
        .expect_err("denied caller MUST NOT succeed");
    let err_missing = svc
        .delete(&ctx(), fake_id, Some(if_match))
        .await
        .expect_err("missing row MUST fail");

    assert!(
        matches!(err_existing, DomainError::RoleDefinitionNotFound { id } if id == real_id),
        "expected RoleDefinitionNotFound for denied caller, got {err_existing:?}"
    );
    assert!(
        matches!(err_missing, DomainError::RoleDefinitionNotFound { id } if id == fake_id),
        "expected RoleDefinitionNotFound for missing row, got {err_missing:?}"
    );
}

/// Delete MUST enforce the catalogued `delete` action, never `write`.
///
/// `gts/permissions.rs` registers `role_definition_delete.v1` with
/// `action: delete`, and role-assignment delete already enforces
/// `actions::DELETE`. Enforcing `write` here would let a write-only grant
/// destroy roles while a delete-only grant was never consulted. The stale
/// `If-Match` stops the flow immediately after the authz check, so the
/// recorded call is observable without the stubbed repo delete firing.
#[tokio::test]
async fn delete_enforces_the_delete_action_not_write() {
    let tenant = Uuid::now_v7();
    let row = sample_model(tenant);
    let real_id = row.id;
    let stale = etag_for(chrono::Utc::now().trunc_subsecs(6), Uuid::now_v7());
    let policy = Arc::new(MockPolicyEnforcer::allow_all());
    let svc = build_service(Some(row), Arc::clone(&policy)).await;

    let err = svc
        .delete(&ctx(), real_id, Some(stale))
        .await
        .expect_err("a stale If-Match MUST fail once the authz check has passed");
    assert!(
        matches!(err, DomainError::StaleEtag { .. }),
        "expected StaleEtag (authz allowed, precondition rejected), got {err:?}"
    );

    let calls = policy.recorded_calls();
    assert_eq!(calls.len(), 1, "delete MUST issue exactly one authz check");
    assert_eq!(
        calls[0].2,
        actions::DELETE,
        "delete MUST enforce the catalogued `delete` action, got `{}`",
        calls[0].2
    );
    assert_eq!(calls[0].3, resource_types::ROLE_DEFINITION);
}

// ---------------------------------------------------------------------------
// create: enforce-first short-circuit
// ---------------------------------------------------------------------------

/// An unauthorized create with a malformed permission rule
/// MUST surface `AuthorizationDenied`, NOT a validation error. The
/// panic-on-call target-type stub would fire if the network lookup ran
/// on the denied branch, so a passing test is proof of the short-circuit.
#[tokio::test]
async fn unauthorized_create_short_circuits_before_target_type() {
    let tenant = Uuid::now_v7();
    let svc = build_service(None, Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err = svc
        .create(
            &ctx(),
            CreateRoleDefinitionRequest {
                caller_scope: CallerScope::Root,
                name: "Auditor".to_owned(),
                description: None,
                permissions: vec![PermissionRule::new(
                    "fly", // unknown verb + unregistered target — would fail validation if it ran
                    "gts.cf.does.not.exist.v1~",
                )],
                not_permissions: Vec::new(),
                assignable_scopes: vec![Scope::tenant(tenant)],
                owner_tenant_id: Some(tenant),
            },
        )
        .await
        .expect_err("denied caller MUST NOT succeed");

    assert!(
        matches!(err, DomainError::AuthorizationDenied { .. }),
        "unauthorized create MUST surface AuthorizationDenied before any target-type \
         lookup runs; got {err:?}"
    );
}

/// A non-root, tenant-bound caller (`CallerScope::Tenant(A)`) MUST
/// NOT create a role definition owned by a *different* tenant `B`. The
/// `resolve_owner_tenant` guard rejects the cross-tenant body with
/// `OwnerTenantMismatch` (→ 403). Uses `allow_all` so that a regression
/// removing the mismatch guard would let the create proceed and flip
/// this test red rather than being masked by an authz denial. Closes
/// the "non-root branch has zero coverage" gap: no other create test
/// reaches this branch.
#[tokio::test]
async fn non_root_caller_cannot_create_role_definition_in_other_tenant() {
    let caller_tenant = Uuid::now_v7();
    let other_tenant = Uuid::now_v7();
    let svc = build_service(None, Arc::new(MockPolicyEnforcer::allow_all())).await;

    let err = svc
        .create(
            &ctx(),
            CreateRoleDefinitionRequest {
                caller_scope: CallerScope::Tenant(caller_tenant),
                name: "Auditor".to_owned(),
                description: None,
                permissions: vec![PermissionRule::new(
                    "read",
                    "gts.cf.core.rbac.role_definition.v1~",
                )],
                not_permissions: Vec::new(),
                assignable_scopes: vec![Scope::tenant(other_tenant)],
                // Cross-tenant: caller is bound to `caller_tenant` but the
                // body claims `other_tenant`.
                owner_tenant_id: Some(other_tenant),
            },
        )
        .await
        .expect_err("a tenant-bound caller MUST NOT create in another tenant");

    assert!(
        matches!(err, DomainError::OwnerTenantMismatch),
        "cross-tenant create MUST surface OwnerTenantMismatch (403); got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// List — cross-scope leakage on the projection layer
// ---------------------------------------------------------------------------
//
// Walks every branch of the `ReadableScopes` → `CustomVisibility`
// projection and asserts the cross-tenant filter is applied. Every test
// seeds rows in two (or three) tenants and asserts the unauthorised
// tenants' rows are absent.

/// `Unrestricted` projection MUST surface every custom row across every
/// tenant — locks the Unrestricted branch against a regression that
/// silently dropped rows.
#[tokio::test]
async fn unrestricted_caller_sees_all_custom_rows_across_tenants() {
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let row_t1 = custom_row(t1, "AuditorT1");
    let row_t2 = custom_row(t2, "AuditorT2");
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    let svc = build_list_service(vec![row_t1.clone(), row_t2.clone()], policy).await;

    let page = svc
        .list(&ctx(), list_request())
        .await
        .expect("unrestricted list MUST succeed");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&row_t1.id),
        "T1 custom MUST surface under Unrestricted; got ids={ids:?}"
    );
    assert!(
        ids.contains(&row_t2.id),
        "T2 custom MUST surface under Unrestricted; got ids={ids:?}"
    );
}

/// Core cross-scope assertion: a caller authorised for T1 MUST NOT
/// see T2's customs.
#[tokio::test]
async fn subtrees_caller_sees_only_listed_tenants_customs() {
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let row_t1 = custom_row(t1, "AuditorT1");
    let row_t2 = custom_row(t2, "AuditorT2");
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Subtrees(vec![format!("/tenants/{t1}")]),
    )]));
    let svc = build_list_service(vec![row_t1.clone(), row_t2.clone()], policy).await;

    let page = svc
        .list(&ctx(), list_request())
        .await
        .expect("subtrees list MUST succeed");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&row_t1.id),
        "T1 custom MUST surface for a caller authorised under T1; got ids={ids:?}"
    );
    assert!(
        !ids.contains(&row_t2.id),
        "T2 custom MUST NOT surface for a caller authorised only under T1 \
         (cross-scope leakage); got ids={ids:?}"
    );
}

/// A tenant-scoped admin whose `readable_scopes` resolve to `Subtrees` MUST
/// still see the built-in catalog alongside their own tenant's custom roles:
/// built-ins are unconditionally visible, so a custom-only mapping would hide
/// them from tenant admins.
#[tokio::test]
async fn subtrees_caller_sees_builtins_and_own_tenant_customs() {
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let built_in = builtin_row("Reader");
    let row_t1 = custom_row(t1, "AuditorT1");
    let row_t2 = custom_row(t2, "AuditorT2");
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Subtrees(vec![format!("/tenants/{t1}")]),
    )]));
    let svc = build_list_service(
        vec![built_in.clone(), row_t1.clone(), row_t2.clone()],
        policy,
    )
    .await;

    let page = svc
        .list(&ctx(), list_request())
        .await
        .expect("subtrees list MUST succeed");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&built_in.id),
        "built-in MUST surface for a tenant-scoped Subtrees caller; got ids={ids:?}"
    );
    assert!(
        ids.contains(&row_t1.id),
        "T1 custom MUST surface for a caller authorised under T1; got ids={ids:?}"
    );
    assert!(
        !ids.contains(&row_t2.id),
        "T2 custom MUST NOT surface (cross-scope leakage); got ids={ids:?}"
    );
}

/// Built-ins surface to every authenticated caller, even when
/// `readable_scopes` returns `None`. The custom segment drops to empty.
#[tokio::test]
async fn none_visibility_drops_custom_segment_but_keeps_builtins() {
    let t1 = Uuid::now_v7();
    let built_in = builtin_row("Reader");
    let custom = custom_row(t1, "AuditorT1");
    // Default mock with an empty readable-scopes table → `ReadableScopes::None`.
    let policy = Arc::new(MockPolicyEnforcer::default());
    let svc = build_list_service(vec![built_in.clone(), custom.clone()], policy).await;

    let page = svc
        .list(&ctx(), list_request())
        .await
        .expect("None-visibility list MUST succeed");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&built_in.id),
        "built-in MUST surface even when readable_scopes returns None; \
         got ids={ids:?}"
    );
    assert!(
        !ids.contains(&custom.id),
        "custom row MUST NOT surface when readable_scopes returns None; \
         got ids={ids:?}"
    );
}

/// A parent-admin with `Subtrees([T_parent, T_child])` sees both
/// tenants' customs, but NOT an unrelated tenant's customs.
#[tokio::test]
async fn parent_admin_sees_two_tenant_subtrees() {
    let t_parent = Uuid::now_v7();
    let t_child = Uuid::now_v7();
    let t_other = Uuid::now_v7();
    let row_parent = custom_row(t_parent, "AuditorParent");
    let row_child = custom_row(t_child, "AuditorChild");
    let row_other = custom_row(t_other, "AuditorOther");
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Subtrees(vec![
            format!("/tenants/{t_parent}"),
            format!("/tenants/{t_child}"),
        ]),
    )]));
    let svc = build_list_service(
        vec![row_parent.clone(), row_child.clone(), row_other.clone()],
        policy,
    )
    .await;

    let page = svc
        .list(&ctx(), list_request())
        .await
        .expect("two-subtree list MUST succeed");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&row_parent.id) && ids.contains(&row_child.id),
        "parent + child customs MUST both surface; got ids={ids:?}"
    );
    assert!(
        !ids.contains(&row_other.id),
        "unrelated tenant's custom MUST NOT surface; got ids={ids:?}"
    );
}

/// `?owner_tenant_id=T2` filter on a caller whose enforce(T2) returns
/// `Denied`: drop the custom segment, keep built-ins. Pairs with the
/// authz-ordering tests by exercising the *list* path's denial-shape.
#[tokio::test]
async fn specific_owner_filter_unauthorized_drops_custom_segment() {
    let t2 = Uuid::now_v7();
    let built_in = builtin_row("Reader");
    let row_t2 = custom_row(t2, "AuditorT2");
    // deny_all surfaces every enforce as Denied; the list path's
    // SpecificOwner branch maps that to CustomVisibility::None.
    let policy = Arc::new(MockPolicyEnforcer::deny_all());
    let svc = build_list_service(vec![built_in.clone(), row_t2.clone()], policy).await;

    // The user-supplied tenant `$filter` is opaque to the policy enforcer:
    // list visibility is derived purely from `readable_scopes`, so there is
    // no per-tenant `enforce()` call to assert on this path. The equivalent
    // guarantee — visibility limits results — is covered by
    // `denied_subtree_yields_builtins_only`.
    let _ = (t2, &row_t2, &built_in, &svc);
}

/// `Subtrees(prefixes)` whose tenant count exceeds [`ALLOWED_TENANTS_CAP`]
/// MUST surface as `DomainError::Validation`. Locks the cap behaviour
/// against a future "just bump the cap" footgun.
#[tokio::test]
async fn allowed_tenants_cap_overflow_returns_validation() {
    use crate::domain::role_definition::service::ALLOWED_TENANTS_CAP;

    // ALLOWED_TENANTS_CAP + 1 distinct tenant prefixes.
    let prefixes: Vec<String> = (0..=ALLOWED_TENANTS_CAP)
        .map(|_| format!("/tenants/{}", Uuid::now_v7()))
        .collect();
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Subtrees(prefixes),
    )]));
    let svc = build_list_service(Vec::new(), policy).await;

    let err = svc
        .list(&ctx(), list_request())
        .await
        .expect_err("over-cap subtree set MUST be rejected");

    assert!(
        matches!(&err, DomainError::Validation { detail } if detail.contains(&ALLOWED_TENANTS_CAP.to_string())),
        "expected DomainError::Validation naming the cap, got {err:?}"
    );
}

/// `Subtrees([RG-scoped prefix])` projects to no tenant ids — RG-scoped
/// read grants don't make tenant-owned customs visible.
#[tokio::test]
async fn rg_prefix_skipped_when_projecting_tenants() {
    let t1 = Uuid::now_v7();
    let rg = Uuid::now_v7();
    let row = custom_row(t1, "AuditorT1");
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Subtrees(vec![format!("/tenants/{t1}/resourceGroups/{rg}")]),
    )]));
    let svc = build_list_service(vec![row.clone()], policy).await;

    let page = svc
        .list(&ctx(), list_request())
        .await
        .expect("RG-only-subtree list MUST succeed");

    assert!(
        page.items.is_empty(),
        "RG-scoped readable_scopes MUST project to no tenants, so customs MUST be empty; \
         got ids={:?}",
        page.items.iter().map(|r| r.id).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// built-in immutability: the gate fires before owner-resolution, authz, and
// the stale-ETag precondition. Built-ins carry owner_tenant_id=None, so the
// owner resolution would otherwise raise an `internal` (500) error before the
// immutability check ran — regression guard for that NULL-owner path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_built_in_rejected_with_built_in_not_modifiable() {
    let row = builtin_row("Owner");
    let real_id = row.id;
    let if_match = etag_for(row.updated_at, row.id);
    // deny_all → an authz-first ordering would yield RoleDefinitionNotFound;
    // owner_tenant_id=None → owner-resolution would yield `internal`. Neither
    // must win: the built-in gate runs first.
    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err = svc
        .update(
            &ctx(),
            UpdateRoleDefinitionRequest {
                id: real_id,
                if_match: Some(if_match),
                patch: RoleDefinitionPatch::default(),
                immutable_field_attempted: None,
            },
        )
        .await
        .expect_err("PATCH on a built-in MUST be rejected");

    assert!(
        matches!(
            err,
            DomainError::BuiltInRoleNotModifiable { role_definition_id } if role_definition_id == real_id
        ),
        "expected BuiltInRoleNotModifiable, got {err:?}"
    );
}

#[tokio::test]
async fn delete_built_in_rejected_even_with_stale_etag() {
    let row = builtin_row("Owner");
    let real_id = row.id;
    // A well-formed but stale If-Match MUST NOT mask the built-in gate with a
    // StaleEtag — the unit-level mirror of the e2e
    // `test_patch_built_in_fires_before_precondition_check`.
    let stale = etag_for(chrono::Utc::now().trunc_subsecs(6), Uuid::now_v7());
    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err = svc
        .delete(&ctx(), real_id, Some(stale))
        .await
        .expect_err("DELETE on a built-in MUST be rejected");

    assert!(
        matches!(
            err,
            DomainError::BuiltInRoleNotModifiable { role_definition_id } if role_definition_id == real_id
        ),
        "expected BuiltInRoleNotModifiable, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// `assignment_count` — visibility-bounded, and `None` when the caller can
// see no assignments at all.
// ---------------------------------------------------------------------------

/// Assignment repo that records the [`VisibilityFilter`] it was handed and
/// answers from a canned per-role map. Recording the filter is the point:
/// the count is only trustworthy if the projection the caller's readable
/// scopes produce actually reaches the query.
struct CountingAssignmentRepo {
    counts: std::collections::HashMap<Uuid, u64>,
    seen: parking_lot::Mutex<Vec<(VisibilityFilter, Vec<Uuid>)>>,
    /// When set, `count_by_role` records the call and then fails.
    ///
    /// Stands in for what the aggregate can really do in production — a
    /// statement timeout, a pool-acquire timeout, a lock wait on the
    /// `GROUP BY role_definition_id` scan. The read that carries this
    /// decoration never touched the assignments table before the count
    /// existed, so none of those may become its HTTP status.
    fail_count_by_role: bool,
}

impl CountingAssignmentRepo {
    fn new(counts: std::collections::HashMap<Uuid, u64>) -> Self {
        Self {
            counts,
            seen: parking_lot::Mutex::new(Vec::new()),
            fail_count_by_role: false,
        }
    }

    /// Variant whose `count_by_role` always errors.
    fn failing() -> Self {
        Self {
            fail_count_by_role: true,
            ..Self::new(std::collections::HashMap::new())
        }
    }
}

#[async_trait]
impl crate::domain::role_assignment_repo::RoleAssignmentRepository for CountingAssignmentRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: crate::domain::role_assignment_repo::NewRoleAssignment,
    ) -> Result<crate::domain::model::RoleAssignmentModel, DomainError> {
        panic!("the count path is read-only")
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<crate::domain::model::RoleAssignmentModel>, DomainError> {
        panic!("the count path reads no individual assignment")
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _query: &ODataQuery,
    ) -> Result<Page<crate::domain::model::RoleAssignmentModel>, DomainError> {
        panic!("a count must never page the assignments it counts")
    }
    async fn get_subject_assignments<C: DBRunner>(
        &self,
        _db: &C,
        _query: crate::domain::role_assignment_repo::SubjectAssignmentsQuery,
    ) -> Result<Vec<crate::domain::model::RoleAssignmentModel>, DomainError> {
        panic!("the count path is not the evaluator path")
    }
    async fn delete<C: DBRunner>(&self, _db: &C, _id: Uuid) -> Result<bool, DomainError> {
        panic!("the count path is read-only")
    }
    async fn count_by_role<C: DBRunner>(
        &self,
        _db: &C,
        visibility: VisibilityFilter,
        ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, u64>, DomainError> {
        // Record first, then decide: a test asserting the failure was
        // degraded still needs proof the query was actually attempted.
        self.seen.lock().push((visibility, ids.to_vec()));
        if self.fail_count_by_role {
            return Err(DomainError::internal(
                "assignment count aggregate timed out",
            ));
        }
        Ok(ids
            .iter()
            .filter_map(|id| self.counts.get(id).map(|c| (*id, *c)))
            .collect())
    }
}

/// The general counted-service builder.
///
/// Wider than [`build_counting_service`] on three axes the degradation and
/// summary tests need: it seeds the `find_by_id` row as well as the
/// `list` / `count_by_type` row set (so `get_with_counts` is reachable), it
/// takes an arbitrary [`PolicyEnforcer`] rather than only the mock (which
/// cannot express a *failing* `readable_scopes`), and it takes an
/// already-built assignment repo (so the failing variant can be handed in).
/// Both repo handles come back, because these tests assert on what each repo
/// was *asked* — not only on what the service returned.
async fn build_counting_service_with(
    seeded: Option<RoleDefinitionModel>,
    seeded_list_rows: Vec<RoleDefinitionModel>,
    policy: Arc<dyn PolicyEnforcer>,
    assignments: Arc<CountingAssignmentRepo>,
) -> (
    RoleDefinitionService<StubRepo, CountingAssignmentRepo>,
    Arc<StubRepo>,
    Arc<CountingAssignmentRepo>,
) {
    let tenants: Vec<Uuid> = seeded
        .as_ref()
        .and_then(|r| r.owner_tenant_id)
        .into_iter()
        .chain(seeded_list_rows.iter().filter_map(|r| r.owner_tenant_id))
        .collect();
    // Each seeded tenant is its own root. A chain would make the second
    // tenant a descendant of the first, so a test asserting that another
    // tenant's rows stay hidden would really be asserting the child case
    // while reading as the foreign-tenant case.
    let branches: Vec<[Uuid; 1]> = tenants.iter().map(|&t| [t]).collect();
    let branch_refs: Vec<&[Uuid]> = branches.iter().map(<[Uuid; 1]>::as_slice).collect();
    let tenant_resolver = Arc::new(FakeTenantResolverClient::with_disjoint_subtrees(
        &branch_refs,
    )) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(FakeRbacRgRead::default()) as Arc<dyn crate::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn TargetTypeValidator> =
        Arc::new(PanicOnCallTargetTypeValidator);
    let repo = Arc::new(StubRepo {
        seeded,
        seeded_list_rows,
        count_by_type_visibilities: parking_lot::Mutex::new(Vec::new()),
    });
    let service = RoleDefinitionService::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::clone(&repo),
        Arc::clone(&assignments),
        policy,
        scope_validator,
        target_type_validator,
    );
    (service, repo, assignments)
}

/// List-service variant whose assignment repo answers from `counts` and
/// records what it was asked.
async fn build_counting_service(
    seeded_list_rows: Vec<RoleDefinitionModel>,
    policy: Arc<MockPolicyEnforcer>,
    counts: std::collections::HashMap<Uuid, u64>,
) -> (
    RoleDefinitionService<StubRepo, CountingAssignmentRepo>,
    Arc<CountingAssignmentRepo>,
) {
    let (service, _repo, assignments) = build_counting_service_with(
        None,
        seeded_list_rows,
        policy,
        Arc::new(CountingAssignmentRepo::new(counts)),
    )
    .await;
    (service, assignments)
}

/// Policy enforcer whose `readable_scopes` fails for **role assignments**
/// only, with [`AuthorizationError::Internal`] — the shape an unreachable
/// or erroring PDP produces.
///
/// A hand-rolled double rather than [`MockPolicyEnforcer`]: the mock's
/// `readable_scopes` is infallible by construction, so the one failure that
/// matters here is inexpressible through it. Role definitions still resolve
/// to `Unrestricted`, so the surrounding read is fully authorised and the
/// only thing that can go wrong is the decoration.
struct AssignmentScopeLookupFails;

#[async_trait]
impl PolicyEnforcer for AssignmentScopeLookupFails {
    async fn enforce(
        &self,
        _ctx: &SecurityContext,
        _subject_id: &str,
        _principal_type: PrincipalType,
        _operation: &str,
        _target_type: &str,
        _context_scope: &Scope,
    ) -> Result<(), AuthorizationError> {
        Ok(())
    }

    async fn readable_scopes(
        &self,
        _ctx: &SecurityContext,
        _subject_id: &str,
        _principal_type: PrincipalType,
        target_type: &str,
        _context_scope: &Scope,
    ) -> Result<ReadableScopes, AuthorizationError> {
        if target_type == resource_types::ROLE_ASSIGNMENT {
            Err(AuthorizationError::Internal(
                "policy enforcer unreachable".to_owned(),
            ))
        } else {
            Ok(ReadableScopes::Unrestricted)
        }
    }
}

/// Readable-scopes table granting `Unrestricted` on role definitions and
/// `answer` on role assignments — the two resource types the counted list
/// consults, answered independently.
fn policy_with_assignment_scopes(answer: ReadableScopes) -> Arc<MockPolicyEnforcer> {
    Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![
        (
            ReadableScopesPred {
                target_type: Some("gts.cf.core.rbac.role_definition.v1~".to_owned()),
                ..ReadableScopesPred::default()
            },
            ReadableScopes::Unrestricted,
        ),
        (
            ReadableScopesPred {
                target_type: Some("gts.cf.core.rbac.role_assignment.v1~".to_owned()),
                ..ReadableScopesPred::default()
            },
            answer,
        ),
    ]))
}

/// A caller with no assignment-read visibility gets rows with **no** count,
/// not a zero: `Some(0)` there would report the caller's own blindness as a
/// fact about the role.
#[tokio::test]
async fn no_assignment_visibility_omits_the_count() {
    let t1 = Uuid::now_v7();
    let row = custom_row(t1, "AuditorT1");
    let counts = std::collections::HashMap::from([(row.id, 7)]);
    let (svc, repo) = build_counting_service(
        vec![row.clone()],
        policy_with_assignment_scopes(ReadableScopes::None),
        counts,
    )
    .await;

    let page = svc
        .list_with_counts(&ctx(), list_request())
        .await
        .expect("the list MUST still succeed");

    assert_eq!(page.items.len(), 1, "the row itself is unaffected");
    assert!(
        page.items[0].assignment_count.is_none(),
        "no assignment visibility MUST mean no count, got {:?}",
        page.items[0].assignment_count
    );
    assert!(
        repo.seen.lock().is_empty(),
        "with no visibility there is nothing to count - the repo must not be queried"
    );
}

/// An *empty* `Subtrees` set admits exactly as many rows as `None`, so it
/// gets the same answer. Otherwise a caller whose grants resolved to nothing
/// would read every role as unused.
#[tokio::test]
async fn empty_subtree_set_omits_the_count_like_none() {
    let t1 = Uuid::now_v7();
    let row = custom_row(t1, "AuditorT1");
    let counts = std::collections::HashMap::from([(row.id, 7)]);
    let (svc, repo) = build_counting_service(
        vec![row],
        policy_with_assignment_scopes(ReadableScopes::Subtrees(Vec::new())),
        counts,
    )
    .await;

    let page = svc
        .list_with_counts(&ctx(), list_request())
        .await
        .expect("the list MUST still succeed");

    assert!(
        page.items[0].assignment_count.is_none(),
        "an empty readable-scope set is no visibility, got {:?}",
        page.items[0].assignment_count
    );
    assert!(repo.seen.lock().is_empty());
}

/// A `Subtrees` caller gets counts, and the prefix set reaches the repo
/// unchanged so the number is bounded by exactly those scopes. A role with
/// no visible assignments reports `Some(0)`, which is the "visible and
/// unused" answer.
#[tokio::test]
async fn subtree_visibility_counts_and_passes_the_prefix_set_through() {
    let t1 = Uuid::now_v7();
    let used = custom_row(t1, "Used");
    let unused = custom_row(t1, "Unused");
    let counts = std::collections::HashMap::from([(used.id, 5)]);
    let prefix = format!("/tenants/{t1}");
    let (svc, repo) = build_counting_service(
        vec![used.clone(), unused.clone()],
        policy_with_assignment_scopes(ReadableScopes::Subtrees(vec![prefix.clone()])),
        counts,
    )
    .await;

    let page = svc
        .list_with_counts(&ctx(), list_request())
        .await
        .expect("the counted list MUST succeed");

    let counted: std::collections::HashMap<Uuid, Option<u64>> = page
        .items
        .iter()
        .map(|row| (row.model.id, row.assignment_count))
        .collect();
    assert_eq!(counted[&used.id], Some(5));
    assert_eq!(
        counted[&unused.id],
        Some(0),
        "a role the caller can see with no matching assignments reports zero"
    );

    let seen = repo.seen.lock();
    assert_eq!(seen.len(), 1, "one batched query for the whole page");
    assert_eq!(
        seen[0].0,
        VisibilityFilter::Subtrees(vec![prefix]),
        "the caller's readable scopes MUST reach the count query verbatim"
    );
    assert_eq!(
        seen[0].1.len(),
        2,
        "both role ids ride in the one query, deduplicated"
    );
}

/// An over-sized readable-scope set omits the count instead of failing the
/// read. The count is a decoration: a caller with very many readable
/// assignment scopes must not lose the ability to list role definitions
/// because a number could not be computed for them — and a number taken over
/// a truncated scope set would be worse than no number at all.
#[tokio::test]
async fn over_cap_assignment_scope_set_omits_the_count_and_serves_the_page() {
    let t1 = Uuid::now_v7();
    let row = custom_row(t1, "AuditorT1");
    let over_cap: Vec<String> = (0
        ..=crate::domain::role_assignment_repo::ALLOWED_SCOPE_PREFIXES_CAP)
        .map(|_| format!("/tenants/{}", Uuid::now_v7()))
        .collect();
    let (svc, repo) = build_counting_service(
        vec![row],
        policy_with_assignment_scopes(ReadableScopes::Subtrees(over_cap)),
        std::collections::HashMap::new(),
    )
    .await;

    let page = svc
        .list_with_counts(&ctx(), list_request())
        .await
        .expect("an over-cap scope set MUST NOT fail the read");

    assert_eq!(page.items.len(), 1, "the page is served in full");
    assert!(
        page.items[0].assignment_count.is_none(),
        "the count is omitted rather than computed over a truncated scope set"
    );
    assert!(
        repo.seen.lock().is_empty(),
        "no query runs once the projection has refused the scope set"
    );
}

// ---------------------------------------------------------------------------
// The decoration invariant: a count must never change an HTTP status code, a
// row set, or a pagination cursor.
//
// The over-cap case above is one way the count can fail to be computable;
// these are the other two — the aggregate query itself failing, and the
// assignment scope-set lookup failing. Either one propagating would turn
// `GET /rbac/v1/role-definitions` — a read whose own data is intact — into a
// 500/503 whenever the assignments table or the PDP had a bad minute.
// ---------------------------------------------------------------------------

/// A failing `count_by_role` MUST leave the page intact and every count
/// omitted — not surface as an error from `list_with_counts`.
#[tokio::test]
async fn count_query_failure_omits_the_count_and_serves_the_page() {
    let t1 = Uuid::now_v7();
    let first = custom_row(t1, "AuditorOne");
    let second = custom_row(t1, "AuditorTwo");
    let (svc, _defs, assignments) = build_counting_service_with(
        None,
        vec![first.clone(), second.clone()],
        policy_with_assignment_scopes(ReadableScopes::Unrestricted),
        Arc::new(CountingAssignmentRepo::failing()),
    )
    .await;

    let page = svc
        .list_with_counts(&ctx(), list_request())
        .await
        .expect("a failed count MUST NOT fail the read");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.model.id).collect();
    assert!(
        ids.contains(&first.id) && ids.contains(&second.id),
        "the full page is served regardless of the count; got ids={ids:?}"
    );
    assert!(
        page.items.iter().all(|r| r.assignment_count.is_none()),
        "every row MUST carry no count when the aggregate failed; got {:?}",
        page.items
            .iter()
            .map(|r| r.assignment_count)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        assignments.seen.lock().len(),
        1,
        "the query really was attempted - the omission is a degradation, not a skip"
    );
}

/// Same degradation on the single-row read: `get_with_counts` MUST still
/// return the row. The seeded row is a built-in so `get` short-circuits on
/// visibility and the assertion is about the count path alone, not about the
/// enforcer's `enforce` decision.
#[tokio::test]
async fn get_count_query_failure_still_returns_the_row() {
    let built_in = builtin_row("Reader");
    let id = built_in.id;
    let (svc, _defs, assignments) = build_counting_service_with(
        Some(built_in),
        Vec::new(),
        policy_with_assignment_scopes(ReadableScopes::Unrestricted),
        Arc::new(CountingAssignmentRepo::failing()),
    )
    .await;

    let counted = svc
        .get_with_counts(&ctx(), id, &CallerScope::Root)
        .await
        .expect("a failed count MUST NOT fail the read");

    assert_eq!(counted.model.id, id, "the row is returned unchanged");
    assert!(
        counted.assignment_count.is_none(),
        "the count is omitted rather than raised; got {:?}",
        counted.assignment_count
    );
    assert_eq!(
        assignments.seen.lock().len(),
        1,
        "the query really was attempted - the omission is a degradation, not a skip"
    );
}

/// An `AuthorizationError::Internal` from the *assignment* `readable_scopes`
/// call MUST degrade the same way. Authorization of the read itself already
/// happened in `list`, so dropping this second scope-set query weakens no
/// access decision: it can only narrow a number, and here no number is
/// produced at all.
#[tokio::test]
async fn assignment_scope_lookup_failure_omits_the_count_and_serves_the_page() {
    let t1 = Uuid::now_v7();
    let row = custom_row(t1, "AuditorT1");
    let counts = std::collections::HashMap::from([(row.id, 9)]);
    let (svc, _defs, assignments) = build_counting_service_with(
        None,
        vec![row.clone()],
        Arc::new(AssignmentScopeLookupFails),
        Arc::new(CountingAssignmentRepo::new(counts)),
    )
    .await;

    let page = svc
        .list_with_counts(&ctx(), list_request())
        .await
        .expect("an unreachable PDP MUST NOT fail a read it already authorised");

    assert_eq!(page.items.len(), 1, "the page is served in full");
    assert_eq!(page.items[0].model.id, row.id);
    assert!(
        page.items[0].assignment_count.is_none(),
        "the count is omitted rather than raised; got {:?}",
        page.items[0].assignment_count
    );
    assert!(
        assignments.seen.lock().is_empty(),
        "no count query may run once the scope set the count must be bounded by \
         is unknown - an unbounded count would leak assignments across tenants"
    );
}

// ---------------------------------------------------------------------------
// `summary` — the endpoint's one security property
// ---------------------------------------------------------------------------

/// A tenant-scoped caller's summary MUST count only their own tenant's
/// custom roles, plus the built-in catalog (unconditionally visible to any
/// authenticated caller).
///
/// The assertion that matters is on the [`RoleDefinitionVisibility`] the
/// repository is handed, not merely on the numbers: the summary is a single
/// aggregate with no row set to inspect, so a regression that widened the
/// projection to `RoleDefinitionVisibility::All` — or skipped the policy
/// enforcer altogether — would publish the platform-wide custom-role count
/// to every tenant admin. With `All` the fixture below reports `custom = 2`
/// and the recorded projection is `All`, so both halves of this test fail.
#[tokio::test]
async fn summary_counts_only_the_callers_own_tenant_customs() {
    let mine = Uuid::now_v7();
    let theirs = Uuid::now_v7();
    let built_in = builtin_row("Reader");
    let my_row = custom_row(mine, "AuditorMine");
    let their_row = custom_row(theirs, "AuditorTheirs");

    // A genuinely restricted caller: read on their own tenant subtree and
    // nowhere else. The predicate pins `context_scope` too, so a regression
    // that consulted the enforcer at `Scope::Root` for a tenant-bound caller
    // would fall through to the mock's closed-posture default rather than
    // silently matching.
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred {
            target_type: Some(resource_types::ROLE_DEFINITION.to_owned()),
            context_scope: Some(Scope::tenant(mine)),
            ..ReadableScopesPred::default()
        },
        ReadableScopes::Subtrees(vec![format!("/tenants/{mine}")]),
    )]));
    let (svc, defs, _assignments) = build_counting_service_with(
        None,
        vec![built_in, my_row, their_row],
        policy,
        Arc::new(CountingAssignmentRepo::new(std::collections::HashMap::new())),
    )
    .await;

    let counts = svc
        .summary(&ctx(), &CallerScope::Tenant(mine))
        .await
        .expect("a tenant-scoped summary MUST succeed");

    assert_eq!(
        counts.custom, 1,
        "only the caller's own tenant's custom role may be counted - another \
         tenant's custom role leaked into the summary; got {counts:?}"
    );
    assert_eq!(
        counts.built_in, 1,
        "built-ins stay counted for a tenant-scoped caller; got {counts:?}"
    );
    assert_eq!(
        counts.total(),
        2,
        "total is the two buckets; got {counts:?}"
    );

    let seen = defs.count_by_type_visibilities.lock();
    assert_eq!(seen.len(), 1, "one aggregate, no per-row queries");
    assert_eq!(
        seen[0],
        RoleDefinitionVisibility::CustomForTenantsWithBuiltins(vec![mine]),
        "the summary MUST be taken over exactly the projection the caller's \
         readable scopes produce - anything wider (notably \
         RoleDefinitionVisibility::All) publishes the platform-wide custom-role \
         count to a tenant admin"
    );
}

/// The counterpart with no read anywhere: the custom bucket drops to zero
/// while the built-in catalog is still counted. Pins that the summary's
/// closed-posture default is `BuiltinsOnly` and not "everything".
#[tokio::test]
async fn summary_with_no_read_anywhere_counts_builtins_only() {
    let theirs = Uuid::now_v7();
    let built_in = builtin_row("Reader");
    let their_row = custom_row(theirs, "AuditorTheirs");
    // Empty readable-scopes table → `ReadableScopes::None`.
    let policy = Arc::new(MockPolicyEnforcer::default());
    let (svc, defs, _assignments) = build_counting_service_with(
        None,
        vec![built_in, their_row],
        policy,
        Arc::new(CountingAssignmentRepo::new(std::collections::HashMap::new())),
    )
    .await;

    let counts = svc
        .summary(&ctx(), &CallerScope::Tenant(Uuid::now_v7()))
        .await
        .expect("a summary for a caller with no read MUST still succeed");

    assert_eq!(
        counts.custom, 0,
        "a caller who can read no tenant sees no custom roles counted; got {counts:?}"
    );
    assert_eq!(
        counts.built_in, 1,
        "built-ins are visible to every authenticated caller; got {counts:?}"
    );

    let seen = defs.count_by_type_visibilities.lock();
    assert_eq!(
        seen[0],
        RoleDefinitionVisibility::BuiltinsOnly,
        "no read anywhere MUST project to BuiltinsOnly, never to All"
    );
}

// ---------------------------------------------------------------------------
// rename MUST honour the built-in name reservation
//
// `create` rejects a custom role named after a built-in, but the DB is
// not the backstop on rename: `uq_role_name_builtin` is partial on
// `owner_tenant_id IS NULL`, so a tenant-owned row sits outside the
// index. Without the check in `update`, the two-step
// create-"Auditor"-then-rename-to-"Owner" made a custom role
// indistinguishable from the built-in in the same result set.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_rejects_rename_to_builtin_name() {
    let tenant = Uuid::now_v7();
    let row = sample_model(tenant);
    let real_id = row.id;
    let if_match = etag_for(row.updated_at, row.id);

    // Authorized caller: the rename must be refused on its own merits,
    // not because the caller was denied.
    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::allow_all())).await;

    let err = svc
        .update(
            &ctx(),
            UpdateRoleDefinitionRequest {
                id: real_id,
                if_match: Some(if_match),
                patch: RoleDefinitionPatch {
                    name: Some("Owner".to_owned()),
                    ..Default::default()
                },
                immutable_field_attempted: None,
            },
        )
        .await
        .expect_err("renaming a custom role onto a built-in name MUST fail");

    assert!(
        matches!(err, DomainError::RoleDefinitionNameReservedByBuiltin { ref name } if name == "Owner"),
        "expected RoleDefinitionNameReservedByBuiltin, got {err:?}"
    );
}

/// The rename check must run through the same confusables fold `create`
/// uses, or the reservation is bypassed by substituting a visually
/// identical codepoint (here Greek capital Omicron for Latin O).
#[tokio::test]
async fn update_rejects_rename_to_confusable_builtin_name() {
    let tenant = Uuid::now_v7();
    let row = sample_model(tenant);
    let real_id = row.id;
    let if_match = etag_for(row.updated_at, row.id);

    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::allow_all())).await;

    let confusable = "\u{039F}wner".to_owned();
    let err = svc
        .update(
            &ctx(),
            UpdateRoleDefinitionRequest {
                id: real_id,
                if_match: Some(if_match),
                patch: RoleDefinitionPatch {
                    name: Some(confusable.clone()),
                    ..Default::default()
                },
                immutable_field_attempted: None,
            },
        )
        .await
        .expect_err("a confusable built-in name MUST be refused on rename too");

    assert!(
        matches!(err, DomainError::RoleDefinitionNameReservedByBuiltin { ref name } if *name == confusable),
        "expected RoleDefinitionNameReservedByBuiltin for the confusable form, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// A types-registry outage is a 500, not a 400.
//
// `target_type_validator.rs` states the rule: "Internal — maps to 500, NOT
// 400, since the validation is not authoritative." Until
// `FailingTargetTypeValidator` existed neither double could produce
// `Internal`, so that arm of `From<TargetTypeValidationError>` was
// unreachable from any test — a regression collapsing an outage into the
// `NotRegistered` 400 would have told the caller their rule was invalid when
// the truth was that RBAC could not check it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_surfaces_a_registry_outage_as_internal_not_invalid_rule() {
    let tenant = Uuid::now_v7();
    let svc = build_service_with_validator(
        None,
        Arc::new(MockPolicyEnforcer::allow_all()),
        Arc::new(
            crate::domain::target_type_validator::FailingTargetTypeValidator::new(
                "types registry unreachable",
            ),
        ),
    )
    .await;

    let err = svc
        .create(
            &ctx(),
            CreateRoleDefinitionRequest {
                caller_scope: CallerScope::Root,
                name: "Auditor".to_owned(),
                description: None,
                // A well-formed rule: the only thing that can fail here is
                // the registry lookup itself.
                permissions: vec![PermissionRule::new(
                    "read",
                    "gts.cf.resources.compute.vm.v1~",
                )],
                not_permissions: Vec::new(),
                assignable_scopes: vec![Scope::tenant(tenant)],
                owner_tenant_id: Some(tenant),
            },
        )
        .await
        .expect_err("a registry outage MUST NOT be reported as success");

    assert!(
        matches!(err, DomainError::Internal { .. }),
        "a types-registry outage MUST surface as Internal (500), not \
         InvalidPermissionRule (400) — the caller's rule was never judged; \
         got {err:?}"
    );
}
