//! Authz-ordering regression tests for [`super::RoleAssignmentService`].
//!
//! Mirrors the role-definition tests: an unauthorized caller MUST NOT
//! distinguish "row exists" from "row missing", and on `delete` MUST
//! NOT see `StaleEtag` for a wrong `If-Match`.

#![allow(clippy::panic, clippy::expect_used)]

use std::sync::Arc;
use toolkit_db::secure::DBRunner;

use chrono::SubsecRound;
use rbac_sdk::models::{PrincipalType, Scope};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    CreateRoleAssignmentRequest, ListRoleAssignmentsRequest, RoleAssignmentService,
    assignable_scopes_admit,
};
use crate::domain::error::DomainError;
use crate::domain::etag::{Etag, etag_for};
use crate::domain::model::RoleAssignmentModel;
use crate::domain::model::RoleDefinitionModel;
use crate::domain::policy_enforcer::ReadableScopes;
use crate::domain::policy_enforcer_mock::{MockPolicyEnforcer, ReadableScopesPred};
use crate::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository, SubjectAssignmentsQuery, VisibilityFilter,
};
use crate::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionPatch, RoleDefinitionRepository, RoleDefinitionVisibility,
};
use crate::domain::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
use crate::domain::scope_validator::ScopeValidator;
use toolkit_odata::{ODataQuery, Page};

fn ctx() -> SecurityContext {
    SecurityContext::anonymous()
}

fn sample_assignment(tenant: Uuid) -> RoleAssignmentModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    RoleAssignmentModel {
        id: Uuid::now_v7(),
        role_definition_id: Uuid::now_v7(),
        principal_id: "alice".to_owned(),
        principal_type: PrincipalType::User,
        scope: Scope::tenant(tenant),
        created_at: now,
        updated_at: now,
        created_by: "tester".to_owned(),
        // These fixtures exercise authz ordering and list projection, not
        // display names; an unrecorded author identity is the legacy shape.
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

struct StubAssignmentRepo {
    seeded: Option<RoleAssignmentModel>,
    /// Rows returned by `list_with_filters_and_cursor`. Parallel to
    /// `seeded` so the single-row `find_by_id` semantics stay intact while
    /// the list tests can seed multi-tenant fixtures.
    seeded_list_rows: Vec<RoleAssignmentModel>,
}

stub_impl! {
    impl RoleAssignmentRepository => for StubAssignmentRepo,
    stub_label = "StubAssignmentRepo",
    methods = [
        async fn create(_new: NewRoleAssignment) -> Result<RoleAssignmentModel, DomainError>;
        async fn get_subject_assignments(_query: SubjectAssignmentsQuery)
            -> Result<Vec<RoleAssignmentModel>, DomainError>;
        async fn delete(_id: Uuid) -> Result<bool, DomainError>;
        async fn count_by_role(_visibility: VisibilityFilter, _ids: &[Uuid])
            -> Result<std::collections::HashMap<Uuid, u64>, DomainError>;
    ],
    custom = {
        async fn find_by_id<C: DBRunner>(&self, _db: &C, id: Uuid) -> Result<Option<RoleAssignmentModel>, DomainError> {
            match &self.seeded {
                Some(row) if row.id == id => Ok(Some(row.clone())),
                _ => Ok(None),
            }
        }

        // Deliberately naive: filter only on `VisibilityFilter`. The
        // cross-scope tests assert on the projection layer; cursor /
        // user-supplied-filter narrowing is out of scope.
        async fn list<C: DBRunner>(
            &self,
            _db: &C,
            visibility: VisibilityFilter,
            _query: &ODataQuery,
        ) -> Result<Page<RoleAssignmentModel>, DomainError> {
            let items: Vec<RoleAssignmentModel> = self
                .seeded_list_rows
                .iter()
                .filter(|m| match &visibility {
                    VisibilityFilter::None => false,
                    VisibilityFilter::Unrestricted => true,
                    VisibilityFilter::Subtrees(prefixes) => {
                        let path = m.scope.path();
                        prefixes.iter().any(|p| {
                            path == *p || path.starts_with(&format!("{p}/"))
                        })
                    }
                })
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
    }
}

/// Role-definition repo used by `create_role_assignment`; the `delete`
/// path never reads it, so every method just panics.
struct PanicOnCallRoleDefRepo;

stub_impl! {
    impl RoleDefinitionRepository => for PanicOnCallRoleDefRepo,
    stub_label = "PanicOnCallRoleDefRepo",
    methods = [
        async fn create(_new: NewRoleDefinition)
            -> Result<RoleDefinitionModel, DomainError>;
        async fn find_by_id(_id: Uuid)
            -> Result<Option<RoleDefinitionModel>, DomainError>;
        async fn find_by_ids(_ids: &[Uuid])
            -> Result<Vec<RoleDefinitionModel>, DomainError>;
        async fn list(_visibility: RoleDefinitionVisibility, _query: &ODataQuery)
            -> Result<Page<RoleDefinitionModel>, DomainError>;
        async fn update(_id: Uuid, _patch: RoleDefinitionPatch, _expected_etag: &Etag)
            -> Result<RoleDefinitionModel, DomainError>;
        async fn delete(_id: Uuid, _expected_etag: &Etag) -> Result<(), DomainError>;
        async fn count_by_type(_visibility: RoleDefinitionVisibility)
            -> Result<crate::domain::role_definition_repo::RoleTypeCounts, DomainError>;
        async fn count_assignments_for_role(_id: Uuid) -> Result<u64, DomainError>;
    ]
}

async fn build_service(
    seeded: Option<RoleAssignmentModel>,
    policy: Arc<MockPolicyEnforcer>,
) -> RoleAssignmentService<StubAssignmentRepo, PanicOnCallRoleDefRepo> {
    let tenant_chain: Vec<Uuid> = seeded
        .as_ref()
        .and_then(|r| r.scope.tenant_id())
        .into_iter()
        .collect();
    let tenant_resolver = Arc::new(FakeTenantResolverClient::with_chain(&tenant_chain))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(FakeRbacRgRead::default()) as Arc<dyn crate::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg.clone()));
    let repo = Arc::new(StubAssignmentRepo {
        seeded,
        seeded_list_rows: Vec::new(),
    });
    let role_repo = Arc::new(PanicOnCallRoleDefRepo);
    RoleAssignmentService::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        repo,
        role_repo,
        policy,
        scope_validator,
        rg,
    )
}

/// List-test helper: seeds `seeded_list_rows` for the stubbed
/// repo's `list_with_filters_and_cursor` impl.
async fn build_list_service(
    seeded_list_rows: Vec<RoleAssignmentModel>,
    policy: Arc<MockPolicyEnforcer>,
) -> RoleAssignmentService<StubAssignmentRepo, PanicOnCallRoleDefRepo> {
    build_list_service_with_roles(seeded_list_rows, policy, Arc::new(PanicOnCallRoleDefRepo)).await
}

/// [`build_list_service`] with the role-definition repo supplied.
///
/// The hydrator's repo type is the service's own `RDR` — the two are one
/// generic parameter — so a test that seeds role names has to build the
/// service over the same seeded repo. The list path never calls it, so a
/// seeded repo is as inert here as the panic stub.
async fn build_list_service_with_roles<
    RDR: crate::domain::role_definition_repo::RoleDefinitionRepository,
>(
    seeded_list_rows: Vec<RoleAssignmentModel>,
    policy: Arc<MockPolicyEnforcer>,
    role_repo: Arc<RDR>,
) -> RoleAssignmentService<StubAssignmentRepo, RDR> {
    let tenants: Vec<Uuid> = seeded_list_rows
        .iter()
        .filter_map(|r| r.scope.tenant_id())
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
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg.clone()));
    let repo = Arc::new(StubAssignmentRepo {
        seeded: None,
        seeded_list_rows,
    });
    RoleAssignmentService::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        repo,
        role_repo,
        policy,
        scope_validator,
        rg,
    )
}

/// Build a default `list` request — empty `OData` query, rooted at `/`.
/// Tests override `context_scope` and supply their own `ODataQuery`
/// when they want to exercise filter behaviour.
fn list_request() -> ListRoleAssignmentsRequest {
    ListRoleAssignmentsRequest {
        context_scope: Scope::Root,
        query: ODataQuery::new(),
    }
}

/// Build an assignment row anchored at `scope_path`. Used by the
/// tests to seed multi-tenant fixtures.
fn assignment_at(scope_path: &str) -> RoleAssignmentModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    let scope = Scope::parse(scope_path).expect("scope_path MUST parse");
    RoleAssignmentModel {
        id: Uuid::now_v7(),
        role_definition_id: Uuid::now_v7(),
        principal_id: "alice".to_owned(),
        principal_type: PrincipalType::User,
        scope,
        created_at: now,
        updated_at: now,
        created_by: "tester".to_owned(),
        // These fixtures exercise authz ordering and list projection, not
        // display names; an unrecorded author identity is the legacy shape.
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

// ---------------------------------------------------------------------------
// delete: existing row vs missing row, unauthorized caller
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_delete_returns_not_found_identical_to_missing() {
    let tenant = Uuid::now_v7();
    let row = sample_assignment(tenant);
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
        matches!(err_existing, DomainError::RoleAssignmentNotFound { id } if id == real_id),
        "expected RoleAssignmentNotFound for denied caller, got {err_existing:?}"
    );
    assert!(
        matches!(err_missing, DomainError::RoleAssignmentNotFound { id } if id == fake_id),
        "expected RoleAssignmentNotFound for missing row, got {err_missing:?}"
    );
}

/// An unauthorized caller sending a stale `If-Match` MUST
/// see `RoleAssignmentNotFound`, not `StaleEtag`. Pre-fix the `ETag`
/// compare ran before enforce and leaked existence.
#[tokio::test]
async fn unauthorized_delete_cannot_probe_stale_etag() {
    let tenant = Uuid::now_v7();
    let row = sample_assignment(tenant);
    let real_id = row.id;

    let svc = build_service(Some(row), Arc::new(MockPolicyEnforcer::deny_all())).await;

    // Wrong If-Match.
    let stale = etag_for(chrono::Utc::now().trunc_subsecs(6), Uuid::now_v7());

    let err = svc
        .delete(&ctx(), real_id, Some(stale))
        .await
        .expect_err("denied caller MUST NOT succeed");

    assert!(
        matches!(err, DomainError::RoleAssignmentNotFound { id } if id == real_id),
        "stale If-Match on a denied caller MUST surface as RoleAssignmentNotFound; \
         got {err:?} (would leak existence if surfaced as StaleEtag)"
    );
}

// ---------------------------------------------------------------------------
// Write-path guard: empty principal_id MUST be rejected at create.
// ---------------------------------------------------------------------------
//
// The guard sits at the top of `create`, before the authz check and any
// repository call. A passing test confirms an attacker cannot plant a
// row with `principal_id = ""` that the evaluator's read-path guard
// would otherwise have to catch. Both layers exist; either one alone
// would close the attack chain, but together they form defence in
// depth.

fn empty_principal_id_request(principal_type: PrincipalType) -> CreateRoleAssignmentRequest {
    CreateRoleAssignmentRequest {
        role_definition_id: Uuid::now_v7(),
        principal_id: String::new(),
        principal_type,
        scope: rbac_sdk::models::Scope::root(),
    }
}

#[tokio::test]
async fn create_rejects_empty_principal_id_for_user() {
    // The guard fires before authz, so deny_all is fine — it would
    // never be consulted. The PanicOnCallRoleDefRepo doubles as a
    // canary: if the guard regresses past the role-definition lookup,
    // the test panics with a clear message.
    let svc = build_service(None, Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err = svc
        .create(&ctx(), empty_principal_id_request(PrincipalType::User))
        .await
        .expect_err("empty principal_id MUST be rejected for User");

    match err {
        DomainError::Validation { detail } => {
            assert!(
                detail.contains("principal_id") && detail.contains("non-empty"),
                "validation message should name `principal_id` and `non-empty`, got: {detail}"
            );
        }
        other => panic!(
            "expected DomainError::Validation, got {other:?} \
             (regression: empty principal_id MUST not reach the repo)"
        ),
    }
}

#[tokio::test]
async fn create_rejects_empty_principal_id_for_service_principal() {
    let svc = build_service(None, Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err = svc
        .create(
            &ctx(),
            empty_principal_id_request(PrincipalType::ServicePrincipal),
        )
        .await
        .expect_err("empty principal_id MUST be rejected for ServicePrincipal");

    match err {
        DomainError::Validation { detail } => {
            assert!(
                detail.contains("principal_id") && detail.contains("non-empty"),
                "validation message should name `principal_id` and `non-empty`, got: {detail}"
            );
        }
        other => panic!(
            "expected DomainError::Validation, got {other:?} \
             (regression: empty principal_id MUST not reach the repo, branch=USER)"
        ),
    }
}

/// `Group` is also covered by the stricter UUID-parse check below the
/// empty guard, but a redundant case keeps the contract loud: the
/// empty-string rejection fires for *every* principal type, before the
/// type-specific validation.
#[tokio::test]
async fn create_rejects_empty_principal_id_for_group() {
    let svc = build_service(None, Arc::new(MockPolicyEnforcer::deny_all())).await;

    let err = svc
        .create(&ctx(), empty_principal_id_request(PrincipalType::Group))
        .await
        .expect_err("empty principal_id MUST be rejected for Group");

    match err {
        DomainError::Validation { detail } => {
            assert!(
                detail.contains("principal_id") && detail.contains("non-empty"),
                "Group's empty-string rejection should match the User / \
                 ServicePrincipal message (the contract is principal-type-agnostic), \
                 got: {detail}"
            );
        }
        other => panic!(
            "expected DomainError::Validation, got {other:?} \
             (regression: empty principal_id MUST not reach the repo, branch=GROUP)"
        ),
    }
}

// ---------------------------------------------------------------------------
// List — cross-scope leakage on the projection layer
// ---------------------------------------------------------------------------
//
// Complements the repo-level `visibility_filter_narrows_to_subtrees`
// postgres test by exercising the *service-layer* projection from
// `ReadableScopes` to `VisibilityFilter`. Every test seeds rows in two
// tenants and asserts the unauthorised tenant's rows are absent.

/// `Unrestricted` projection MUST surface every assignment across every
/// tenant.
#[tokio::test]
async fn unrestricted_caller_sees_all_assignments_across_tenants() {
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let row_t1 = assignment_at(&format!("/tenants/{t1}"));
    let row_t2 = assignment_at(&format!("/tenants/{t2}"));
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
        ids.contains(&row_t1.id) && ids.contains(&row_t2.id),
        "both tenants' assignments MUST surface under Unrestricted; got ids={ids:?}"
    );
}

/// Core cross-scope assertion at the service layer for assignments:
/// a caller authorised for T1 MUST NOT see T2's assignments.
#[tokio::test]
async fn subtrees_caller_sees_only_listed_tenants_assignments() {
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let row_t1 = assignment_at(&format!("/tenants/{t1}"));
    let row_t2 = assignment_at(&format!("/tenants/{t2}"));
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
        "T1 assignment MUST surface for a caller authorised under T1; got ids={ids:?}"
    );
    assert!(
        !ids.contains(&row_t2.id),
        "T2 assignment MUST NOT surface for a caller authorised only under T1 \
         (cross-scope leakage); got ids={ids:?}"
    );
}

/// `ReadableScopes::None` → empty page. Role assignments have no
/// built-ins concept; the list MUST be empty.
#[tokio::test]
async fn none_visibility_returns_empty_page() {
    let t1 = Uuid::now_v7();
    let row = assignment_at(&format!("/tenants/{t1}"));
    // Default mock with an empty readable-scopes table → `ReadableScopes::None`.
    let policy = Arc::new(MockPolicyEnforcer::default());
    let svc = build_list_service(vec![row], policy).await;

    let page = svc
        .list(&ctx(), list_request())
        .await
        .expect("None-visibility list MUST succeed");

    assert!(
        page.items.is_empty(),
        "None visibility MUST yield an empty page (no built-ins to fall back to); \
         got ids={:?}",
        page.items.iter().map(|r| r.id).collect::<Vec<_>>()
    );
}

/// The list endpoint MUST NOT return 403. The service comment at
/// `service.rs` says: a `Denied` from `readable_scopes` would be
/// surprising — surface as Internal so operators see the upstream
/// failure. `MockPolicyEnforcer::readable_scopes` cannot return Denied,
/// so this test exercises the closed-posture default (`None`) under a
/// `deny_all` policy and confirms list returns an empty page, not a
/// `403`. The behavioural contract (no 403 on list) is what matters.
#[tokio::test]
async fn list_never_surfaces_authorization_denied() {
    let row = assignment_at("/tenants/00000000-0000-7000-8000-000000000001");
    let policy = Arc::new(MockPolicyEnforcer::deny_all());
    let svc = build_list_service(vec![row], policy).await;

    let result = svc.list(&ctx(), list_request()).await;

    match result {
        Ok(page) => assert!(
            page.items.is_empty(),
            "deny_all caller MUST see an empty page on list, NOT 403; got {} items",
            page.items.len()
        ),
        Err(DomainError::AuthorizationDenied { .. }) => panic!(
            "list MUST NOT return 403; a denial from the policy layer \
             MUST surface as either empty page or Internal"
        ),
        Err(other) => panic!(
            "list returned an unexpected error variant: {other:?} \
             (expected Ok(empty) under deny_all)"
        ),
    }
}

// ---------------------------------------------------------------------------
// `assignable_scopes_admit`: the descendant rule `create` enforces.
// A role may be assigned only at a scope its
// `assignable_scopes` cover (the exact scope or a descendant of one).
// A regression that admitted an out-of-scope assignment would let a
// role be bound where it was never meant to apply.
//
// Descent between tenants is real hierarchy, not string shape, so the
// check consults the tenant resolver and these tests hand it a fake
// seeded with the tenants under test. `admit` below is the shared
// harness: the `branches` the caller passes decide which tenants exist
// and how they are related.
// ---------------------------------------------------------------------------

/// Evaluate `assignable_scopes_admit` against a tenant hierarchy made of
/// `branches` (each a root-to-leaf chain; branches sharing a first
/// element share that parent).
async fn admit(branches: &[&[Uuid]], assignable: &[Scope], scope: &Scope) -> bool {
    try_admit(branches, assignable, scope)
        .await
        .expect("assignable_scopes_admit must not fail against seeded fakes")
}

/// [`admit`] without the `expect`, for the cases that are about whether
/// the check errors at all.
async fn try_admit(
    branches: &[&[Uuid]],
    assignable: &[Scope],
    scope: &Scope,
) -> Result<bool, DomainError> {
    let tenant_resolver = Arc::new(FakeTenantResolverClient::with_disjoint_subtrees(branches))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(FakeRbacRgRead::default()) as Arc<dyn crate::domain::rg_port::RbacRgRead>;
    let validator = ScopeValidator::new(tenant_resolver, rg);
    assignable_scopes_admit(&validator, &ctx(), assignable, scope).await
}

/// A resource-group assignable scope covers that resource group and
/// nothing else — not a tenant below the one it lives in, and not a
/// resource group inside such a tenant.
///
/// This is the case the tenant-hierarchy fallback is most likely to get
/// wrong, because `ScopeValidator::is_ancestor` has to resolve two
/// scopes down to tenant ids to ask the resolver anything at all. If it
/// does that for an RG scope, `/tenants/P/resourceGroups/G` quietly
/// becomes `/tenants/P` and covers the entire subtree under P — the
/// exact opposite of what an operator asked for by naming one group.
/// Every scope here is cross-tenant on purpose: the same-tenant shapes
/// are answered structurally and never reach the resolver.
#[tokio::test]
async fn assignable_scopes_admit_rg_does_not_cover_a_descendant_tenant() {
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let group = Uuid::now_v7();
    assert!(
        !admit(
            &[&[parent, child]],
            &[Scope::resource_group(parent, group)],
            &Scope::tenant(child)
        )
        .await,
        "an RG{{parent,G}} assignable scope MUST NOT admit a whole child tenant"
    );
    assert!(
        !admit(
            &[&[parent, child]],
            &[Scope::resource_group(parent, group)],
            &Scope::resource_group(child, Uuid::now_v7())
        )
        .await,
        "nor an RG inside that child tenant"
    );
}

/// An assignable scope whose tenant no longer exists admits nothing, and
/// does not stop the entries after it from being considered.
///
/// Nothing prunes `assignable_scopes` when a tenant is deleted, so a
/// stale entry is an ordinary state for a long-lived role. Treating it
/// as a hard error would fail assignments that a later entry allows, and
/// would make the outcome depend on the order the list is stored in —
/// so both orderings are asserted.
#[tokio::test]
async fn assignable_scopes_admit_skips_an_entry_whose_tenant_is_gone() {
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let deleted = Uuid::now_v7(); // never seeded: the resolver 404s on it
    let live = Scope::tenant(parent);
    let stale = Scope::tenant(deleted);

    for (order, assignable) in [
        ("stale first", vec![stale.clone(), live.clone()]),
        ("stale last", vec![live.clone(), stale.clone()]),
    ] {
        let outcome = try_admit(&[&[parent, child]], &assignable, &Scope::tenant(child)).await;
        assert!(
            matches!(outcome, Ok(true)),
            "a live parent entry MUST still admit the child, {order}; got {outcome:?}"
        );
    }

    let alone = try_admit(&[&[parent, child]], &[stale], &Scope::tenant(child)).await;
    assert!(
        matches!(alone, Ok(false)),
        "a stale entry on its own admits nothing — and is still not an error; got {alone:?}"
    );
}

#[tokio::test]
async fn assignable_scopes_admit_exact_tenant_match() {
    let t = Uuid::now_v7();
    assert!(admit(&[&[t]], &[Scope::tenant(t)], &Scope::tenant(t)).await);
}

#[tokio::test]
async fn assignable_scopes_admit_tenant_covers_rg_underneath() {
    let t = Uuid::now_v7();
    assert!(
        admit(
            &[&[t]],
            &[Scope::tenant(t)],
            &Scope::resource_group(t, Uuid::now_v7())
        )
        .await,
        "a Tenant{{T}} assignable scope MUST admit an RG{{T,_}} assignment underneath it"
    );
}

/// The rule the design states: a tenant assignable scope covers the
/// whole subtree below it, so a role assignable at the parent may be
/// assigned at a child tenant.
#[tokio::test]
async fn assignable_scopes_admit_tenant_covers_child_tenant() {
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let grandchild = Uuid::now_v7();
    assert!(
        admit(
            &[&[parent, child, grandchild]],
            &[Scope::tenant(parent)],
            &Scope::tenant(child)
        )
        .await,
        "a Tenant{{parent}} assignable scope MUST admit an assignment at a child tenant"
    );
    assert!(
        admit(
            &[&[parent, child, grandchild]],
            &[Scope::tenant(parent)],
            &Scope::tenant(grandchild)
        )
        .await,
        "descent is transitive: a grandchild is inside the parent's subtree too"
    );
    assert!(
        admit(
            &[&[parent, child, grandchild]],
            &[Scope::tenant(parent)],
            &Scope::resource_group(child, Uuid::now_v7())
        )
        .await,
        "an RG inside a descendant tenant is inside the parent's subtree as well"
    );
    assert!(
        !admit(
            &[&[parent, child]],
            &[Scope::tenant(child)],
            &Scope::tenant(parent)
        )
        .await,
        "descent is one-way: a child assignable scope MUST NOT admit the parent"
    );
}

#[tokio::test]
async fn assignable_scopes_admit_root_covers_everything() {
    let t = Uuid::now_v7();
    assert!(admit(&[&[t]], &[Scope::root()], &Scope::root()).await);
    assert!(admit(&[&[t]], &[Scope::root()], &Scope::tenant(t)).await);
    assert!(
        admit(
            &[&[t]],
            &[Scope::root()],
            &Scope::resource_group(t, Uuid::now_v7())
        )
        .await
    );
}

#[tokio::test]
async fn assignable_scopes_admit_rejects_other_tenant() {
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    // Disjoint subtrees: neither tenant is an ancestor of the other, so
    // the rejection is about unrelatedness, not about string shape.
    assert!(
        !admit(&[&[t1], &[t2]], &[Scope::tenant(t1)], &Scope::tenant(t2)).await,
        "a Tenant{{T1}} assignable scope MUST NOT admit a Tenant{{T2}} assignment"
    );
    assert!(
        !admit(
            &[&[t1], &[t2]],
            &[Scope::tenant(t1)],
            &Scope::resource_group(t2, Uuid::now_v7())
        )
        .await,
        "a Tenant{{T1}} assignable scope MUST NOT admit an RG{{T2,_}} assignment"
    );
}

#[tokio::test]
async fn assignable_scopes_admit_rg_covers_only_exact_rg() {
    let t = Uuid::now_v7();
    let g1 = Uuid::now_v7();
    // Exact RG is admitted...
    assert!(
        admit(
            &[&[t]],
            &[Scope::resource_group(t, g1)],
            &Scope::resource_group(t, g1)
        )
        .await
    );
    // ...but not a sibling RG in the same tenant...
    assert!(
        !admit(
            &[&[t]],
            &[Scope::resource_group(t, g1)],
            &Scope::resource_group(t, Uuid::now_v7())
        )
        .await,
        "an RG{{T,G1}} assignable scope MUST NOT admit a sibling RG{{T,G2}} assignment"
    );
    // ...nor the whole tenant above it.
    assert!(
        !admit(&[&[t]], &[Scope::resource_group(t, g1)], &Scope::tenant(t)).await,
        "an RG{{T,G1}} assignable scope MUST NOT admit a whole-Tenant{{T}} assignment"
    );
}

#[tokio::test]
async fn assignable_scopes_admit_empty_list_admits_nothing() {
    let t = Uuid::now_v7();
    assert!(
        !admit(&[&[t]], &[], &Scope::tenant(t)).await,
        "an empty assignable_scopes list MUST admit no assignment"
    );
}

// ---------------------------------------------------------------------------
// list_with_names: the role name rides the caller's own visibility
// ---------------------------------------------------------------------------
//
// `role_definition_name` is resolved from RBAC's own table, and that table
// is not readable in full by everyone: the catalog answers 404 for another
// tenant's custom role. An ancestor admin can grant such a role at a
// descendant scope, and the descendant's admin can read the resulting
// assignment row — so the name must be narrowed by the reader's own
// role-definition visibility, and a failure to work out that visibility
// must cost the name, never the page.

/// `RoleDefinitionRepository` that answers the one batched read the
/// hydrator makes and nothing else. Every other method is unreachable from
/// a read path, so reaching one is a bug worth a panic rather than a
/// plausible empty answer.
struct SeededRoleDefRepo {
    rows: Vec<RoleDefinitionModel>,
}

#[async_trait::async_trait]
impl RoleDefinitionRepository for SeededRoleDefRepo {
    async fn find_by_ids<C: DBRunner>(
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
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError> {
        panic!("SeededRoleDefRepo: hydration writes nothing");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError> {
        panic!("SeededRoleDefRepo: a per-row lookup is what the batch avoids");
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
        _query: &ODataQuery,
    ) -> Result<Page<RoleDefinitionModel>, DomainError> {
        panic!("SeededRoleDefRepo: hydration resolves by id, never by listing");
    }
    async fn update<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _patch: RoleDefinitionPatch,
        _expected_etag: &Etag,
    ) -> Result<RoleDefinitionModel, DomainError> {
        panic!("SeededRoleDefRepo: hydration is a read path");
    }
    async fn delete<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _expected_etag: &Etag,
    ) -> Result<(), DomainError> {
        panic!("SeededRoleDefRepo: hydration is a read path");
    }
    async fn count_assignments_for_role<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<u64, DomainError> {
        panic!("SeededRoleDefRepo: hydration counts nothing");
    }
    async fn count_by_type<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
    ) -> Result<crate::domain::role_definition_repo::RoleTypeCounts, DomainError> {
        panic!("SeededRoleDefRepo: hydration summarises nothing");
    }
}

/// A custom role definition owned by `owner`.
fn custom_role(id: Uuid, name: &str, owner: Uuid) -> RoleDefinitionModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    RoleDefinitionModel {
        id,
        name: name.to_owned(),
        description: None,
        is_built_in: false,
        permissions: Vec::new(),
        not_permissions: Vec::new(),
        assignable_scopes: vec![Scope::tenant(owner)],
        owner_tenant_id: Some(owner),
        created_at: now,
        updated_at: now,
        created_by: "tester".to_owned(),
    }
}

/// A list service with display-name hydration wired over seeded role
/// definitions. The principal/group/author readers are inert: these tests
/// are about the role name and about what happens to the page around it.
async fn build_named_list_service(
    seeded_list_rows: Vec<RoleAssignmentModel>,
    roles: Vec<RoleDefinitionModel>,
    policy: Arc<MockPolicyEnforcer>,
) -> RoleAssignmentService<StubAssignmentRepo, SeededRoleDefRepo> {
    let roles_repo = Arc::new(SeededRoleDefRepo { rows: roles });
    let svc =
        build_list_service_with_roles(seeded_list_rows, policy, Arc::clone(&roles_repo)).await;
    let hydrator = crate::domain::role_assignment::PrincipalNameHydrator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(crate::domain::principal_name_reader_mock::FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles_repo),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(crate::domain::metrics::NoopMetrics),
    );
    svc.with_hydrator(Arc::new(hydrator))
}

/// The happy path the narrowing must not break: a caller who can read
/// `T1` sees the name of `T1`'s custom role, and does *not* see the name
/// of the role owned by the tenant above them — while both rows are
/// served.
#[tokio::test]
async fn list_with_names_narrows_role_names_to_the_callers_visibility() {
    let t1 = Uuid::now_v7();
    let ancestor = Uuid::now_v7();
    let own_role = Uuid::now_v7();
    let ancestor_role = Uuid::now_v7();
    let mut row_own = assignment_at(&format!("/tenants/{t1}"));
    row_own.role_definition_id = own_role;
    let mut row_ancestor = assignment_at(&format!("/tenants/{t1}"));
    row_ancestor.role_definition_id = ancestor_role;
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Subtrees(vec![format!("/tenants/{t1}")]),
    )]));
    let svc = build_named_list_service(
        vec![row_own.clone(), row_ancestor.clone()],
        vec![
            custom_role(own_role, "My Custom Role", t1),
            custom_role(ancestor_role, "Ancestor Secret Role", ancestor),
        ],
        policy,
    )
    .await;

    let page = svc
        .list_with_names(&ctx(), list_request())
        .await
        .expect("list_with_names MUST succeed");

    assert_eq!(page.items.len(), 2, "both rows are served");
    let named: Vec<Option<&str>> = page
        .items
        .iter()
        .map(|item| item.role_definition_name.as_deref())
        .collect();
    assert!(
        named.contains(&Some("My Custom Role")),
        "the caller's own tenant's role is named; got {named:?}"
    );
    assert!(
        !named.contains(&Some("Ancestor Secret Role")),
        "the name of a role the catalog answers 404 for MUST NOT leak; got {named:?}"
    );
}

/// A failure to derive the caller's role-definition visibility costs the
/// role name and nothing else: the page, its rows and its cursor are the
/// ones `list` produced, and the read does not become an error.
///
/// The failure is injected the way it can actually happen — an
/// unparseable prefix out of `readable_scopes`, which the derivation
/// refuses to silently drop.
#[tokio::test]
async fn list_with_names_degrades_when_role_visibility_cannot_be_derived() {
    let t1 = Uuid::now_v7();
    let role = Uuid::now_v7();
    let mut row = assignment_at(&format!("/tenants/{t1}"));
    row.role_definition_id = role;
    let policy = Arc::new(MockPolicyEnforcer::default().with_readable_scopes(vec![
        // The assignment list itself stays fully visible …
        (
            ReadableScopesPred {
                target_type: Some(crate::domain::resource_types::ROLE_ASSIGNMENT.to_owned()),
                ..ReadableScopesPred::default()
            },
            ReadableScopes::Unrestricted,
        ),
        // … while the role-definition derivation gets a prefix it cannot
        // parse, which surfaces as an internal error rather than a silent
        // "built-ins only".
        (
            ReadableScopesPred {
                target_type: Some(crate::domain::resource_types::ROLE_DEFINITION.to_owned()),
                ..ReadableScopesPred::default()
            },
            ReadableScopes::Subtrees(vec!["not-a-scope".to_owned()]),
        ),
    ]));
    let svc = build_named_list_service(
        vec![row.clone()],
        vec![custom_role(role, "Tenant Administrator", t1)],
        policy,
    )
    .await;

    let page = svc
        .list_with_names(&ctx(), list_request())
        .await
        .expect("a failed visibility derivation MUST NOT fail the read");

    assert_eq!(page.items.len(), 1, "the row is still served");
    assert_eq!(page.items[0].model.id, row.id);
    assert!(
        page.items[0].role_definition_name.is_none(),
        "with no visibility to apply, no role name may be served"
    );
}

/// The envelope check costs ONE tenant-resolver round-trip, however long
/// `assignable_scopes` is.
///
/// This is the property the chain lookup exists for. Asking
/// `is_ancestor` per entry instead would make a role-assignment create
/// issue one RPC per entry, on a path that runs for every create — so
/// the count is pinned rather than left to be re-derived from the code.
/// Every entry here is a cross-tenant tenant scope, the only shape that
/// cannot be settled structurally, and none of them admits the target.
#[tokio::test]
async fn assignable_scopes_admit_costs_one_round_trip_regardless_of_list_length() {
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let strangers: Vec<Uuid> = (0..10).map(|_| Uuid::now_v7()).collect();

    let mut branches: Vec<Vec<Uuid>> = vec![vec![parent, child]];
    branches.extend(strangers.iter().map(|s| vec![*s]));
    let branch_refs: Vec<&[Uuid]> = branches.iter().map(Vec::as_slice).collect();

    let tenant_fake = Arc::new(FakeTenantResolverClient::with_disjoint_subtrees(
        &branch_refs,
    ));
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        Arc::new(FakeRbacRgRead::default()) as Arc<dyn crate::domain::rg_port::RbacRgRead>,
    );

    let assignable: Vec<Scope> = strangers.iter().map(|s| Scope::tenant(*s)).collect();
    let admitted = assignable_scopes_admit(&validator, &ctx(), &assignable, &Scope::tenant(child))
        .await
        .expect("seeded fakes must not fail");

    assert!(
        !admitted,
        "none of the unrelated tenants may admit the child scope"
    );
    assert_eq!(
        tenant_fake.total_calls(),
        1,
        "ten assignable scopes must still cost exactly one resolver call, not ten"
    );
}

// ---------------------------------------------------------------------------
// enforcer outage: `AuthorizationError::Internal` MUST surface as a 500
//
// `Denied` is deliberately collapsed into `RoleAssignmentNotFound` so a
// caller cannot probe existence. `Internal` must NOT take that path: an
// unreachable enforcer is not evidence that the row is absent, and
// reporting 404 hides the outage from the caller and from operators.
// Until `Decision::Internal` existed the mock could not produce this
// error at all, so every arm below was unreachable from a test.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_enforcer_internal_surfaces_as_internal_not_not_found() {
    let tenant = Uuid::now_v7();
    let row = sample_assignment(tenant);
    let real_id = row.id;

    let svc = build_service(
        Some(row),
        Arc::new(MockPolicyEnforcer::internal_all("enforcer unreachable")),
    )
    .await;

    let err = svc
        .get(&ctx(), real_id)
        .await
        .expect_err("an enforcer outage MUST NOT be reported as success");

    assert!(
        matches!(err, DomainError::Internal { ref diagnostic, .. } if diagnostic == "enforcer unreachable"),
        "expected Internal carrying the enforcer diagnostic, got {err:?}"
    );
}

#[tokio::test]
async fn delete_enforcer_internal_surfaces_as_internal_not_not_found() {
    let tenant = Uuid::now_v7();
    let row = sample_assignment(tenant);
    let real_id = row.id;
    let if_match = etag_for(row.updated_at, row.id);

    let svc = build_service(
        Some(row),
        Arc::new(MockPolicyEnforcer::internal_all("enforcer unreachable")),
    )
    .await;

    let err = svc
        .delete(&ctx(), real_id, Some(if_match))
        .await
        .expect_err("an enforcer outage MUST NOT be reported as success");

    assert!(
        matches!(err, DomainError::Internal { ref diagnostic, .. } if diagnostic == "enforcer unreachable"),
        "expected Internal carrying the enforcer diagnostic, got {err:?}"
    );
}

/// The counterpart to `list_never_surfaces_authorization_denied`: that
/// test pins "no 403 on list", this one pins that the no-403 rule is not
/// implemented by swallowing an outage. A `readable_scopes` failure MUST
/// be an error, not an empty page — an empty page renders as "you may
/// read nothing" and is indistinguishable from a legitimate result.
#[tokio::test]
async fn list_readable_scopes_internal_surfaces_as_internal_not_empty_page() {
    let row = assignment_at("/tenants/00000000-0000-7000-8000-000000000001");
    let policy = Arc::new(
        MockPolicyEnforcer::allow_all()
            .with_readable_scopes_failure("tenant resolver disconnected"),
    );
    let svc = build_list_service(vec![row], policy).await;

    let err = svc
        .list(&ctx(), list_request())
        .await
        .expect_err("a readable_scopes outage MUST NOT render as an empty page");

    assert!(
        matches!(err, DomainError::Internal { ref diagnostic, .. } if diagnostic == "tenant resolver disconnected"),
        "expected Internal carrying the readable_scopes diagnostic, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// `validate_group_principal`'s two unreachable error branches.
//
// `create` reaches it only after the empty-id guard, authz, the
// role-definition lookup and the assignable-scope check, so neither the
// root-scope rejection nor the `Upstream -> ServiceUnavailable` mapping had
// a test: `PanicOnCallRoleDefRepo` panics at the lookup, and the RG fake
// could only answer `NotFound`. The second gap is the one that matters — a
// resource-group outage was indistinguishable, to a test, from "that group
// does not exist", i.e. a 503 the caller should retry reported as a 404
// they would act on.
// ---------------------------------------------------------------------------

/// Role-definition repo that answers the create path's single `find_by_id`.
struct CreatePathRoleDefRepo {
    row: RoleDefinitionModel,
}

stub_impl! {
    impl RoleDefinitionRepository => for CreatePathRoleDefRepo,
    stub_label = "CreatePathRoleDefRepo",
    methods = [
        async fn create(_new: NewRoleDefinition)
            -> Result<RoleDefinitionModel, DomainError>;
        async fn find_by_ids(_ids: &[Uuid])
            -> Result<Vec<RoleDefinitionModel>, DomainError>;
        async fn list(_visibility: RoleDefinitionVisibility, _query: &ODataQuery)
            -> Result<Page<RoleDefinitionModel>, DomainError>;
        async fn update(_id: Uuid, _patch: RoleDefinitionPatch, _expected_etag: &Etag)
            -> Result<RoleDefinitionModel, DomainError>;
        async fn delete(_id: Uuid, _expected_etag: &Etag) -> Result<(), DomainError>;
        async fn count_by_type(_visibility: RoleDefinitionVisibility)
            -> Result<crate::domain::role_definition_repo::RoleTypeCounts, DomainError>;
        async fn count_assignments_for_role(_id: Uuid) -> Result<u64, DomainError>;
    ],
    custom = {
        async fn find_by_id<C: toolkit_db::secure::DBRunner>(
            &self,
            _db: &C,
            _id: Uuid,
        ) -> Result<Option<RoleDefinitionModel>, DomainError> {
            Ok(Some(self.row.clone()))
        }
    }
}

/// Build a service whose create path can reach `validate_group_principal`:
/// an allow-all policy, a role definition that exists and is assignable at
/// `assignable`, and the supplied RG fake.
async fn build_create_service(
    role: RoleDefinitionModel,
    tenant_chain: &[Uuid],
    rg: Arc<FakeRbacRgRead>,
) -> RoleAssignmentService<StubAssignmentRepo, CreatePathRoleDefRepo> {
    let tenant_resolver = Arc::new(FakeTenantResolverClient::with_chain(tenant_chain))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg_read: Arc<dyn crate::domain::rg_port::RbacRgRead> = rg;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, Arc::clone(&rg_read)));
    let repo = Arc::new(StubAssignmentRepo {
        seeded: None,
        seeded_list_rows: Vec::new(),
    });
    RoleAssignmentService::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        repo,
        Arc::new(CreatePathRoleDefRepo { row: role }),
        Arc::new(MockPolicyEnforcer::allow_all()),
        scope_validator,
        rg_read,
    )
}

#[tokio::test]
async fn create_group_principal_at_root_scope_is_forbidden() {
    let role_id = Uuid::now_v7();
    let mut role = custom_role(role_id, "RootAssignable", Uuid::now_v7());
    role.assignable_scopes = vec![rbac_sdk::models::Scope::root()];
    role.owner_tenant_id = None;
    let svc = build_create_service(role, &[], Arc::new(FakeRbacRgRead::default())).await;

    let err = svc
        .create(
            &ctx(),
            CreateRoleAssignmentRequest {
                role_definition_id: role_id,
                principal_id: Uuid::now_v7().to_string(),
                principal_type: PrincipalType::Group,
                scope: rbac_sdk::models::Scope::root(),
            },
        )
        .await
        .expect_err("a Group principal at root scope MUST be refused");

    assert!(
        matches!(err, DomainError::GroupPrincipalRootScopeForbidden),
        "expected GroupPrincipalRootScopeForbidden, got {err:?}"
    );
}

#[tokio::test]
async fn create_group_principal_maps_a_resource_group_outage_to_service_unavailable() {
    let tenant = Uuid::now_v7();
    let role_id = Uuid::now_v7();
    let group_id = Uuid::now_v7();
    let rg = Arc::new(
        FakeRbacRgRead::default()
            .with_group(group_id, tenant)
            .with_group_upstream_failure("resource-group gate timed out"),
    );
    let svc = build_create_service(custom_role(role_id, "Auditor", tenant), &[tenant], rg).await;

    let err = svc
        .create(
            &ctx(),
            CreateRoleAssignmentRequest {
                role_definition_id: role_id,
                principal_id: group_id.to_string(),
                principal_type: PrincipalType::Group,
                scope: rbac_sdk::models::Scope::tenant(tenant),
            },
        )
        .await
        .expect_err("an RG outage MUST NOT be reported as success");

    assert!(
        matches!(err, DomainError::ServiceUnavailable { .. }),
        "a resource-group outage MUST surface as ServiceUnavailable (retryable), \
         not GroupPrincipalNotFound (a 404 the caller would act on); got {err:?}"
    );
}
