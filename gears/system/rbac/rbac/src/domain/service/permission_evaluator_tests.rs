//! Tests for [`super::permission_evaluator`].
//!
//! Two invariants carry their own fixtures:
//!
//! * Every known `rbac_sdk::models::Scope` variant must reach a
//!   non-fallthrough arm of [`super::assignment_applies`]. A new variant added
//!   to `Scope` makes [`every_known_scope_variant`] non-exhaustive, so the
//!   fixture stops compiling — the canonical signal that the evaluator needs an
//!   explicit arm for it.
//! * An empty `subject_id` MUST be rejected as `RbacServiceError::Validation`
//!   before any I/O. A `debug_assert!` would be a no-op in release, so the
//!   guard returns `Err` unconditionally and is asserted here as a typed error.

#![allow(clippy::panic)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use toolkit_db::secure::DBRunner;

use async_trait::async_trait;
use chrono::Utc;
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    DenyReason, PermissionResult, PermissionRule, PermissionScopeType, PrincipalType, Scope,
};
use uuid::{Uuid, uuid};

use super::{PermissionEvaluator, UNKNOWN_SCOPE_VARIANTS_TEST_COUNT, assignment_applies};
use crate::domain::error::DomainError;
use crate::domain::model::{RoleAssignmentModel, RoleDefinitionModel};
use crate::domain::ports::metrics::NoopMetrics;
use crate::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository, SubjectAssignmentsQuery, VisibilityFilter,
};
use crate::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionPatch, RoleDefinitionRepository, RoleDefinitionVisibility,
    RoleTypeCounts,
};
use toolkit_odata::{ODataQuery, Page as ODataPage};

/// Exhaustive constructor for every known `Scope` variant.
///
/// `Scope` is `#[non_exhaustive]` in `rbac-sdk`, so a wildcard match
/// here would be required by the compiler — defeating the
/// future-proofing the fixture wants. Instead we enumerate every
/// variant in a real `match` block that the compiler MUST refuse if a
/// new variant lands. The construction lines below then have to grow
/// in lock-step.
fn every_known_scope_variant() -> Vec<Scope> {
    // Compile-time guard. Run an exhaustive `match` on a sentinel; the
    // `#[non_exhaustive]` attribute forces a wildcard arm for callers
    // outside `rbac-sdk`, but `unreachable_patterns` keeps us honest
    // about the same-crate enumeration: every known variant gets a
    // dedicated arm. When a new variant is added without updating the
    // construction below, the wildcard arm fires the `unreachable!`
    // panic at test-runtime — a deliberate alarm, not a soft skip.
    fn check_exhaustive(s: &Scope) {
        // The three known arms intentionally carry identical bodies —
        // they exist so the compiler walks every variant in turn.
        // Collapsing them with `|` would defeat the
        // unknown-variant-catches-the-wildcard guarantee the test
        // depends on.
        #[allow(clippy::wildcard_enum_match_arm, clippy::match_same_arms)]
        match s {
            Scope::Root => {}
            Scope::Tenant { .. } => {}
            Scope::ResourceGroup { .. } => {}
            // `Scope` is `#[non_exhaustive]`; the wildcard catches any
            // future variant the fixture has not been taught about and
            // turns the omission into a loud test failure.
            other => panic!(
                "every_known_scope_variant: unhandled Scope variant {other:?}; \
                 add it to the constructor list and add a matching arm to \
                 `assignment_applies`."
            ),
        }
    }

    let cases = vec![
        Scope::Root,
        Scope::Tenant {
            tenant_id: uuid!("11111111-1111-1111-1111-111111111111"),
        },
        Scope::ResourceGroup {
            tenant_id: uuid!("22222222-2222-2222-2222-222222222222"),
            group_id: uuid!("33333333-3333-3333-3333-333333333333"),
        },
    ];
    for c in &cases {
        check_exhaustive(c);
    }
    cases
}

// ---------------------------------------------------------------------------
// Empty subject_id MUST be rejected at the read-path entry.
// ---------------------------------------------------------------------------

/// Panicking repo stubs. The read-path guards are the first thing each
/// function does, so no repo method should fire in these tests.
/// Hitting any of them is a regression in the guard's position.
struct PanicRoleAssignmentRepo;

#[async_trait]
impl RoleAssignmentRepository for PanicRoleAssignmentRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError> {
        unreachable!("the read-path guard fired? then no write should reach the repo");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError> {
        unreachable!("the read-path guard MUST fire before find_by_id");
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleAssignmentModel>, DomainError> {
        unreachable!("the read-path guard MUST fire before list");
    }
    async fn get_subject_assignments<C: DBRunner>(
        &self,
        _db: &C,
        _query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError> {
        unreachable!(
            "the read-path guard MUST fire before get_subject_assignments \u{2014} \
             an empty subject_id would otherwise reach SQL `WHERE principal_id = ''`"
        );
    }
    async fn delete<C: DBRunner>(&self, _db: &C, _id: Uuid) -> Result<bool, DomainError> {
        unreachable!("the read-path guard MUST fire before delete");
    }
    async fn count_by_role<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, u64>, DomainError> {
        unreachable!("the read-path guard MUST fire before count_by_role");
    }
}

/// `RoleAssignmentRepository` whose `get_subject_assignments` returns no
/// assignments (`Ok(vec![])`); every write/other read method panics. Used
/// by the metrics tests to drive the "no visible roles → deny" path
/// without seeding any data.
struct EmptyRoleAssignmentRepo;

#[async_trait]
impl RoleAssignmentRepository for EmptyRoleAssignmentRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError> {
        unreachable!("metrics test only exercises the read path");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError> {
        unreachable!("metrics test only exercises the read path");
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleAssignmentModel>, DomainError> {
        unreachable!("metrics test only exercises the read path");
    }
    async fn get_subject_assignments<C: DBRunner>(
        &self,
        _db: &C,
        _query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError> {
        Ok(Vec::new())
    }
    async fn delete<C: DBRunner>(&self, _db: &C, _id: Uuid) -> Result<bool, DomainError> {
        unreachable!("metrics test only exercises the read path");
    }
    async fn count_by_role<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, u64>, DomainError> {
        unreachable!("metrics test does not count assignments per role");
    }
}

struct PanicRoleDefinitionRepo;

#[async_trait]
impl RoleDefinitionRepository for PanicRoleDefinitionRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError> {
        unreachable!("the read-path guard MUST fire before role_def create");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError> {
        unreachable!("the read-path guard MUST fire before find_by_id");
    }
    async fn find_by_ids<C: DBRunner>(
        &self,
        _db: &C,
        _ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        unreachable!("the read-path guard MUST fire before find_by_ids");
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleDefinitionModel>, DomainError> {
        unreachable!("the read-path guard MUST fire before role_def list");
    }
    async fn update<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _patch: RoleDefinitionPatch,
        _expected_etag: &crate::domain::etag::Etag,
    ) -> Result<RoleDefinitionModel, DomainError> {
        unreachable!("the read-path guard MUST fire before role_def update");
    }
    async fn delete<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _expected_etag: &crate::domain::etag::Etag,
    ) -> Result<(), DomainError> {
        unreachable!("the read-path guard MUST fire before role_def delete");
    }
    async fn count_assignments_for_role<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<u64, DomainError> {
        unreachable!("the read-path guard MUST fire before count_assignments_for_role");
    }
    async fn count_by_type<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
    ) -> Result<RoleTypeCounts, DomainError> {
        unreachable!("the read-path guard MUST fire before count_by_type");
    }
}

/// Build a `PermissionEvaluator` whose dependencies all panic if
/// reached. The read-path guards fire before any of them, so a
/// passing test confirms the guard is at the right position.
async fn build_evaluator_with_panic_deps()
-> PermissionEvaluator<PanicRoleAssignmentRepo, PanicRoleDefinitionRepo> {
    use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};

    PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(PanicRoleAssignmentRepo),
        Arc::new(PanicRoleDefinitionRepo),
        Arc::new(FakeTenantResolverClient::with_chain(&[Uuid::nil()])),
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(NoopMetrics),
    )
}

#[tokio::test]
async fn evaluate_permission_rejects_empty_subject_id() {
    let evaluator = build_evaluator_with_panic_deps().await;
    let ctx = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &ctx,
            "", // empty subject_id — the guard target
            PrincipalType::User,
            "read",
            "gts.cf.example.resource.v1~",
            &Scope::Root,
        )
        .await;
    match result {
        Err(RbacServiceError::Validation { message }) => {
            assert!(
                message.contains("subject_id"),
                "validation message should name `subject_id`, got: {message}"
            );
        }
        other => panic!(
            "expected RbacServiceError::Validation, got {other:?} \
             (regression: empty subject_id MUST not reach the repo)"
        ),
    }
}

#[tokio::test]
async fn get_subject_roles_rejects_empty_subject_id() {
    let evaluator = build_evaluator_with_panic_deps().await;
    let ctx = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .get_subject_roles(
            &ctx,
            "", // empty subject_id — the guard target
            PrincipalType::User,
            Uuid::nil(),
            true,
        )
        .await;
    match result {
        Err(RbacServiceError::Validation { message }) => {
            assert!(
                message.contains("subject_id"),
                "validation message should name `subject_id`, got: {message}"
            );
        }
        other => panic!(
            "expected RbacServiceError::Validation, got {other:?} \
             (regression: empty subject_id MUST not reach the repo)"
        ),
    }
}

// ---------------------------------------------------------------------------
// Scope-variant exhaustiveness.
// ---------------------------------------------------------------------------

#[test]
fn assignment_applies_handles_every_known_scope_variant_without_fallthrough() {
    // Capture the unknown-variant counter before the test exercises
    // the function; any increment over this run signals a variant
    // landed in the fallthrough arm.
    let before = UNKNOWN_SCOPE_VARIANTS_TEST_COUNT.load(Ordering::Relaxed);

    // Probe each known variant against a representative request scope.
    // The request side is incidental — `assignment_applies` only
    // matches on `grant_scope`, so a single tenant-shaped request is
    // sufficient to drive every grant variant through the function.
    let probe_request = Scope::Tenant {
        tenant_id: Uuid::nil(),
    };
    for grant in every_known_scope_variant() {
        let _discarded = assignment_applies(&grant, &probe_request, &[]);
    }

    let after = UNKNOWN_SCOPE_VARIANTS_TEST_COUNT.load(Ordering::Relaxed);
    assert_eq!(
        after, before,
        "assignment_applies hit the silent-deny fallthrough for at least one known \
         Scope variant; add an explicit match arm so the request reaches a \
         deterministic decision (regression guard)."
    );
}

// ---------------------------------------------------------------------------
// Assignment_applies enforces tenant match + Root-requires-Root grant.
// ---------------------------------------------------------------------------

/// `Scope::Tenant{T1}` grant MUST NOT apply to a `Scope::Tenant{T2}` request.
/// `assignment_applies` compares the tenant identity itself rather than
/// trusting `get_subject_roles` to have narrowed the candidate set — that
/// invariant lives in SQL and is not enforced locally.
#[test]
fn assignment_applies_denies_cross_tenant_tenant_grant() {
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let grant = Scope::Tenant { tenant_id: t1 };
    let request = Scope::Tenant { tenant_id: t2 };
    assert!(
        !assignment_applies(&grant, &request, &[]),
        "Tenant{{T1}} grant MUST NOT apply to Tenant{{T2}} request (cross-tenant escalation guard, no ancestor relationship)"
    );
}

/// `Scope::Tenant{T}` grant MUST NOT apply to a `Scope::Root` request.
/// The caller-tenant fallback in `evaluate_permission` may pass the
/// caller's home tenant as the lookup key for a root evaluation; the
/// per-grant matcher must still reject the tenant grant so a root
/// authorisation requires an explicit root-scoped grant.
#[test]
fn assignment_applies_denies_tenant_grant_for_root_request() {
    let grant = Scope::Tenant {
        tenant_id: Uuid::new_v4(),
    };
    let request = Scope::Root;
    assert!(
        !assignment_applies(&grant, &request, &[]),
        "Tenant{{T}} grant MUST NOT apply to Root request (root needs a root grant)"
    );
}

/// Positive: a `Tenant{T}` grant DOES apply to an `RG{T, G}` request —
/// any RG under the granted tenant is in scope.
#[test]
fn assignment_applies_accepts_tenant_grant_for_rg_request_in_same_tenant() {
    let t = Uuid::new_v4();
    let grant = Scope::Tenant { tenant_id: t };
    let request = Scope::ResourceGroup {
        tenant_id: t,
        group_id: Uuid::new_v4(),
    };
    assert!(
        assignment_applies(&grant, &request, &[]),
        "Tenant{{T}} grant MUST apply to RG{{T, _}} request (same tenant)"
    );
}

/// Negative companion: a `Tenant{T1}` grant does NOT apply to an
/// `RG{T2, _}` request (cross-tenant escalation guard for RG paths)
/// when the two tenants are NOT in an ancestor relationship — the
/// `ancestor_tenants` slice is empty.
#[test]
fn assignment_applies_denies_tenant_grant_for_rg_request_in_other_tenant() {
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let grant = Scope::Tenant { tenant_id: t1 };
    let request = Scope::ResourceGroup {
        tenant_id: t2,
        group_id: Uuid::new_v4(),
    };
    assert!(
        !assignment_applies(&grant, &request, &[]),
        "Tenant{{T1}} grant MUST NOT apply to RG{{T2, _}} request when T1 is not an ancestor of T2"
    );
}

/// Unconditional downward scope inheritance: a `Tenant{parent}`
/// grant MUST apply to a `Tenant{child}` request when `parent` is in
/// the ancestor chain of `child`. The ancestor set is computed by
/// `evaluate_permission` from `tenant_resolver.get_ancestors` and
/// passed in as a slice. This is the read/authorize-parity guarantee
/// the `get_subject_roles_sets_is_inherited_correctly` postgres test
/// already pins on the read side.
#[test]
fn assignment_applies_accepts_tenant_grant_when_grant_tenant_is_request_ancestor() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let grant = Scope::Tenant { tenant_id: parent };
    let request = Scope::Tenant { tenant_id: child };
    assert!(
        assignment_applies(&grant, &request, &[parent]),
        "Tenant{{parent}} grant MUST apply to Tenant{{child}} when parent is in the ancestor chain"
    );
}

/// Companion to `assignment_applies_accepts_tenant_grant_when_grant_tenant_is_request_ancestor`:
/// a `Tenant{parent}` grant MUST also apply
/// to an `RG{child, _}` request when `parent` is an ancestor of
/// `child` — inheritance flows through to RGs under the descendant
/// tenant.
#[test]
fn assignment_applies_accepts_tenant_grant_for_rg_in_descendant_tenant() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let grant = Scope::Tenant { tenant_id: parent };
    let request = Scope::ResourceGroup {
        tenant_id: child,
        group_id: Uuid::new_v4(),
    };
    assert!(
        assignment_applies(&grant, &request, &[parent]),
        "Tenant{{parent}} grant MUST apply to RG under a descendant tenant"
    );
}

/// Even with a non-empty ancestor set, an ancestor-tenant grant MUST
/// NOT satisfy a `Root` request — the narrower-than-root invariant
/// still holds. Only a `Root` grant authorises a `Root` request.
#[test]
fn assignment_applies_denies_ancestor_tenant_grant_for_root_request() {
    let parent = Uuid::new_v4();
    let grant = Scope::Tenant { tenant_id: parent };
    let request = Scope::Root;
    assert!(
        !assignment_applies(&grant, &request, &[parent]),
        "Tenant{{parent}} grant MUST NOT apply to Root request even when listed as an ancestor"
    );
}

// ---------------------------------------------------------------------------
// `assignment_applies` for `ResourceGroup` *grants*. Where the `Tenant`-grant
// cases above cover inheritance, these pin the RG-grant arm: the per-RG
// narrowing the SQL candidate query intentionally skips, which is what closes
// the cross-RG escalation bypass. The arm matches ONLY the exact same RG (same
// tenant AND same group).
// ---------------------------------------------------------------------------

/// Positive: an `RG{T, G}` grant applies to the exact same `RG{T, G}`
/// request.
#[test]
fn assignment_applies_accepts_rg_grant_for_exact_same_rg() {
    let t = Uuid::new_v4();
    let g = Uuid::new_v4();
    let grant = Scope::ResourceGroup {
        tenant_id: t,
        group_id: g,
    };
    let request = Scope::ResourceGroup {
        tenant_id: t,
        group_id: g,
    };
    assert!(
        assignment_applies(&grant, &request, &[]),
        "RG{{T, G}} grant MUST apply to the exact same RG{{T, G}} request"
    );
}

/// The headline cross-RG escalation guard: an `RG{T, G1}` grant MUST
/// NOT apply to a *sibling* `RG{T, G2}` request in the same tenant. A
/// regression letting this return `true` is the exact privilege
/// escalation `assignment_applies` exists to prevent.
#[test]
fn assignment_applies_denies_rg_grant_for_sibling_rg_in_same_tenant() {
    let t = Uuid::new_v4();
    let grant = Scope::ResourceGroup {
        tenant_id: t,
        group_id: Uuid::new_v4(),
    };
    let request = Scope::ResourceGroup {
        tenant_id: t,
        group_id: Uuid::new_v4(),
    };
    assert!(
        !assignment_applies(&grant, &request, &[]),
        "RG{{T, G1}} grant MUST NOT apply to sibling RG{{T, G2}} request (cross-RG escalation guard)"
    );
}

/// An `RG{T1, G}` grant MUST NOT apply to a request for the same group
/// id under a *different* tenant `RG{T2, G}` — group ids are only
/// meaningful within their owning tenant.
#[test]
fn assignment_applies_denies_rg_grant_for_same_group_in_other_tenant() {
    let g = Uuid::new_v4();
    let grant = Scope::ResourceGroup {
        tenant_id: Uuid::new_v4(),
        group_id: g,
    };
    let request = Scope::ResourceGroup {
        tenant_id: Uuid::new_v4(),
        group_id: g,
    };
    assert!(
        !assignment_applies(&grant, &request, &[]),
        "RG{{T1, G}} grant MUST NOT apply to RG{{T2, G}} request (cross-tenant, same group id)"
    );
}

/// An `RG{T, G}` grant DOES apply to a whole-`Tenant{T}` request from the
/// group's OWN tenant — that is how a hint-less collection read (one that
/// evaluates at the caller's tenant scope) gets authorized; the grant's
/// `GroupSubtree`
/// classification then narrows visibility to the group's members, so this
/// is NOT tenant-wide widening. It still MUST NOT apply to a different
/// tenant's request, nor to `Root`.
#[test]
fn assignment_applies_rg_grant_for_own_tenant_request_but_not_root() {
    let t = Uuid::new_v4();
    let grant = Scope::ResourceGroup {
        tenant_id: t,
        group_id: Uuid::new_v4(),
    };
    assert!(
        assignment_applies(&grant, &Scope::Tenant { tenant_id: t }, &[]),
        "RG{{T, G}} grant MUST apply to its own Tenant{{T}} request"
    );
    assert!(
        !assignment_applies(
            &grant,
            &Scope::Tenant {
                tenant_id: Uuid::new_v4()
            },
            &[]
        ),
        "RG{{T, G}} grant MUST NOT apply to a different tenant's request"
    );
    assert!(
        !assignment_applies(&grant, &Scope::Root, &[]),
        "RG{{T, G}} grant MUST NOT apply to a Root request"
    );
}

// ---------------------------------------------------------------------------
// Malformed upstream cursor MUST surface as RbacServiceError::Internal.
// ---------------------------------------------------------------------------

/// `RbacRgRead` fake whose `list_memberships` returns a `next_cursor` that is
/// NOT a valid `CursorV1::encode()` token, driving the decode-failure path
/// inside `resolve_group_memberships`. The token must stay opaque: a hand-built
/// `CursorV1` literal would decode cleanly and never reach that path.
struct MalformedCursorRgRead;

#[async_trait]
impl crate::domain::rg_port::RbacRgRead for MalformedCursorRgRead {
    async fn get_group(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<crate::domain::rg_port::RbacRgGroup, crate::domain::rg_port::RbacRgReadError> {
        unreachable!(
            "the cursor-decode test MUST NOT reach get_group ({id}); \
             the malformed next_cursor fails before scope-validation runs"
        );
    }

    /// Display-name reads are not part of permission evaluation.
    async fn group_names(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, crate::domain::rg_port::RbacRgReadError>
    {
        unreachable!("the cursor-decode test MUST NOT reach group_names");
    }

    async fn list_memberships(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: &toolkit_odata::ODataQuery,
    ) -> Result<
        toolkit_odata::Page<crate::domain::rg_port::RbacRgMembership>,
        crate::domain::rg_port::RbacRgReadError,
    > {
        Ok(toolkit_odata::Page {
            items: Vec::new(),
            page_info: toolkit_odata::PageInfo {
                next_cursor: Some("not-a-base64url-token".to_owned()),
                prev_cursor: None,
                limit: 100,
            },
        })
    }
}

#[tokio::test]
async fn get_subject_roles_surfaces_internal_when_upstream_cursor_is_malformed() {
    use crate::domain::model::scope_fakes::FakeTenantResolverClient;

    let evaluator = PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(PanicRoleAssignmentRepo),
        Arc::new(PanicRoleDefinitionRepo),
        Arc::new(FakeTenantResolverClient::with_chain(&[Uuid::nil()])),
        Arc::new(MalformedCursorRgRead),
        Arc::new(NoopMetrics),
    );
    let ctx = toolkit_security::SecurityContext::anonymous();

    let result = evaluator
        .get_subject_roles(
            &ctx,
            "alice",
            // User + include_group_roles=true is the only path that
            // reaches `resolve_group_memberships`.
            PrincipalType::User,
            Uuid::nil(),
            true,
        )
        .await;

    match result {
        Err(RbacServiceError::Internal { message }) => {
            assert!(
                message.contains("next_cursor"),
                "internal error message should name `next_cursor` to ease triage, got: {message}"
            );
        }
        other => panic!(
            "expected RbacServiceError::Internal naming `next_cursor`, got {other:?} \
             (regression: a hand-built CursorV1 literal would silently accept the \
             upstream's opaque token instead of decoding it)"
        ),
    }
}

// ---------------------------------------------------------------------------
// `resolve_group_memberships` MUST deduplicate group ids across pages
// so the downstream `SubjectAssignmentsQuery::group_principals` does not
// carry duplicates. Regression to the O(n²) `Vec::contains` form OR to
// a no-dedup variant that ships duplicates would surface here.
// ---------------------------------------------------------------------------

/// Capturing `RoleAssignmentRepository` that records the
/// `SubjectAssignmentsQuery` passed to `get_subject_assignments` and
/// returns an empty assignment set. All other methods panic — this
/// test only drives the membership-dedup path.
struct CapturingAssignmentRepo {
    captured: Arc<Mutex<Option<SubjectAssignmentsQuery>>>,
}

#[async_trait]
impl RoleAssignmentRepository for CapturingAssignmentRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleAssignmentModel>, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn get_subject_assignments<C: DBRunner>(
        &self,
        _db: &C,
        query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError> {
        *self
            .captured
            .lock()
            .expect("CapturingAssignmentRepo: mutex poisoned") = Some(query);
        Ok(Vec::new())
    }
    async fn delete<C: DBRunner>(&self, _db: &C, _id: Uuid) -> Result<bool, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn count_by_role<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, u64>, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
}

/// `RoleDefinitionRepository` that returns an empty batch for
/// `find_by_ids`. Needed because `get_subject_roles` always calls
/// `find_by_ids` (even with an empty id list) on the batched
/// path; a panicking stub would fire here even though the test does
/// not care about role-definition lookup.
struct EmptyFindByIdsRoleDefinitionRepo;

#[async_trait]
impl RoleDefinitionRepository for EmptyFindByIdsRoleDefinitionRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError> {
        unreachable!("this test only exercises find_by_ids");
    }
    async fn find_by_ids<C: DBRunner>(
        &self,
        _db: &C,
        _ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        Ok(Vec::new())
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleDefinitionModel>, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn update<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _patch: RoleDefinitionPatch,
        _expected_etag: &crate::domain::etag::Etag,
    ) -> Result<RoleDefinitionModel, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn delete<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _expected_etag: &crate::domain::etag::Etag,
    ) -> Result<(), DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn count_assignments_for_role<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<u64, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
    async fn count_by_type<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
    ) -> Result<RoleTypeCounts, DomainError> {
        unreachable!("this test only exercises the subject-roles read path");
    }
}

#[tokio::test]
async fn resolve_group_memberships_deduplicates_across_pages() {
    use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};

    // Two pages that both contain the same `group_id`. Add a unique
    // id on page 2 so we also confirm dedup keeps non-duplicates.
    let dup = uuid!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let other = uuid!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let rg = FakeRbacRgRead::default().with_membership_pages(vec![vec![dup], vec![dup, other]]);

    let captured: Arc<Mutex<Option<SubjectAssignmentsQuery>>> = Arc::new(Mutex::new(None));
    let evaluator = PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(CapturingAssignmentRepo {
            captured: Arc::clone(&captured),
        }),
        Arc::new(EmptyFindByIdsRoleDefinitionRepo),
        Arc::new(FakeTenantResolverClient::with_chain(&[Uuid::nil()])),
        Arc::new(rg),
        Arc::new(NoopMetrics),
    );
    let ctx = toolkit_security::SecurityContext::anonymous();
    let subject_roles = evaluator
        .get_subject_roles(
            &ctx,
            "alice",
            // User + include_group_roles=true is the only path that
            // reaches `resolve_group_memberships`.
            PrincipalType::User,
            Uuid::nil(),
            true,
        )
        .await
        .expect("get_subject_roles must succeed with capturing/empty repos");
    assert!(
        subject_roles.is_empty(),
        "capturing repo returns no assignments, so no roles should surface"
    );

    let query = captured
        .lock()
        .expect("captured mutex poisoned")
        .take()
        .expect("get_subject_assignments was not called");
    let mut principals = query.group_principals;
    principals.sort();
    let mut expected = vec![dup.to_string(), other.to_string()];
    expected.sort();
    assert_eq!(
        principals, expected,
        "duplicate group_id across pages MUST be deduped before reaching SubjectAssignmentsQuery"
    );
}

// ---------------------------------------------------------------------------
// `evaluate_permission` decision logic. The allow / deny union, the
// per-RG narrowing, and not-permission precedence are otherwise only
// exercised by the `#[ignore]`d Postgres suite. These drive the full
// path at the domain layer with seeded stub repos (the SQL candidate
// query is irrelevant here — the stub returns a fixed candidate set so
// the in-memory `assignment_applies` + matcher path is what's tested).
// ---------------------------------------------------------------------------

const T3_TENANT: Uuid = uuid!("cccccccc-cccc-cccc-cccc-cccccccccccc");
const T3_RESOURCE: &str = "gts.cf.example.thing.v1~";

/// Returns a fixed candidate set from `get_subject_assignments`.
struct SeededAssignmentRepo {
    assignments: Vec<RoleAssignmentModel>,
}

#[async_trait]
impl RoleAssignmentRepository for SeededAssignmentRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError> {
        unreachable!("T3 exercises only the read path");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError> {
        unreachable!("T3 exercises only the read path");
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleAssignmentModel>, DomainError> {
        unreachable!("T3 exercises only the read path");
    }
    async fn get_subject_assignments<C: DBRunner>(
        &self,
        _db: &C,
        _query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError> {
        Ok(self.assignments.clone())
    }
    async fn delete<C: DBRunner>(&self, _db: &C, _id: Uuid) -> Result<bool, DomainError> {
        unreachable!("T3 exercises only the read path");
    }
    async fn count_by_role<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, u64>, DomainError> {
        unreachable!("T3 exercises only the read path");
    }
}

/// Returns seeded role definitions from `find_by_ids`.
struct SeededDefinitionRepo {
    defs: Vec<RoleDefinitionModel>,
}

#[async_trait]
impl RoleDefinitionRepository for SeededDefinitionRepo {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError> {
        unreachable!("T3 exercises only find_by_ids");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError> {
        unreachable!("T3 exercises only find_by_ids");
    }
    async fn find_by_ids<C: DBRunner>(
        &self,
        _db: &C,
        ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        Ok(self
            .defs
            .iter()
            .filter(|d| ids.contains(&d.id))
            .cloned()
            .collect())
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleDefinitionModel>, DomainError> {
        unreachable!("T3 exercises only find_by_ids");
    }
    async fn update<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _patch: RoleDefinitionPatch,
        _expected_etag: &crate::domain::etag::Etag,
    ) -> Result<RoleDefinitionModel, DomainError> {
        unreachable!("T3 exercises only find_by_ids");
    }
    async fn delete<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _expected_etag: &crate::domain::etag::Etag,
    ) -> Result<(), DomainError> {
        unreachable!("T3 exercises only find_by_ids");
    }
    async fn count_assignments_for_role<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<u64, DomainError> {
        unreachable!("T3 exercises only find_by_ids");
    }
    async fn count_by_type<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
    ) -> Result<RoleTypeCounts, DomainError> {
        unreachable!("T3 exercises only find_by_ids");
    }
}

fn t3_role_def(
    id: Uuid,
    permissions: Vec<PermissionRule>,
    not_permissions: Vec<PermissionRule>,
) -> RoleDefinitionModel {
    RoleDefinitionModel {
        id,
        name: "T3 Role".to_owned(),
        description: None,
        is_built_in: false,
        permissions,
        not_permissions,
        assignable_scopes: vec![Scope::tenant(T3_TENANT)],
        owner_tenant_id: Some(T3_TENANT),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        created_by: "tester".to_owned(),
    }
}

fn t3_assignment(role_definition_id: Uuid, scope: Scope) -> RoleAssignmentModel {
    RoleAssignmentModel {
        id: Uuid::now_v7(),
        role_definition_id,
        principal_id: "alice".to_owned(),
        principal_type: PrincipalType::User,
        scope,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        created_by: "tester".to_owned(),
        // The evaluator never reads the author identity — permission
        // evaluation is about the principal holding the role.
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

async fn t3_evaluator(
    assignments: Vec<RoleAssignmentModel>,
    defs: Vec<RoleDefinitionModel>,
) -> PermissionEvaluator<SeededAssignmentRepo, SeededDefinitionRepo> {
    use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
    PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(SeededAssignmentRepo { assignments }),
        Arc::new(SeededDefinitionRepo { defs }),
        Arc::new(FakeTenantResolverClient::with_chain(&[T3_TENANT])),
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(NoopMetrics),
    )
}

async fn t3_evaluate(
    evaluator: &PermissionEvaluator<SeededAssignmentRepo, SeededDefinitionRepo>,
    context_scope: &Scope,
) -> PermissionResult {
    let ctx = toolkit_security::SecurityContext::anonymous();
    evaluator
        .evaluate_permission(
            &ctx,
            "alice",
            PrincipalType::User,
            "read",
            T3_RESOURCE,
            context_scope,
        )
        .await
        .expect("evaluate_permission must not error with seeded repos")
}

#[tokio::test]
async fn evaluate_permission_allows_when_tenant_grant_covers_rg_request() {
    let role_id = Uuid::now_v7();
    let def = t3_role_def(
        role_id,
        vec![PermissionRule::new("read", T3_RESOURCE)],
        vec![],
    );
    let assignment = t3_assignment(role_id, Scope::tenant(T3_TENANT));
    let evaluator = t3_evaluator(vec![assignment], vec![def]).await;

    let request = Scope::resource_group(T3_TENANT, Uuid::new_v4());
    let PermissionResult::Allowed(granted) = t3_evaluate(&evaluator, &request).await else {
        panic!("expected Allowed: a Tenant{{T}} grant MUST cover an RG{{T,_}} request");
    };
    assert_eq!(
        granted.grants.len(),
        1,
        "the Tenant{{T}} grant MUST contribute exactly one effective permission for an RG{{T,_}} request"
    );
    assert_eq!(
        granted.scope_type,
        PermissionScopeType::TenantSubtree {
            root_tenant_id: T3_TENANT,
        },
        "the compiled scope MUST come from the tenant assignment, never the request's shape"
    );
}

/// A hint-less collection read is evaluated at the caller's tenant, but an
/// assignment at `RG{T,G}` must still compile to `GroupSubtree(G)`. Merely
/// asserting `Allowed` misses the tenant-isolation contract: a widened
/// `TenantSubtree` or `Global` result also allows and leaks unrelated rows.
#[tokio::test]
async fn evaluate_permission_compiles_rg_assignment_for_tenant_collection_as_group_subtree() {
    let role_id = Uuid::now_v7();
    let group_id = Uuid::new_v4();
    let def = t3_role_def(
        role_id,
        vec![PermissionRule::new("read", T3_RESOURCE)],
        vec![],
    );
    let assignment = t3_assignment(role_id, Scope::resource_group(T3_TENANT, group_id));
    let evaluator = t3_evaluator(vec![assignment], vec![def]).await;

    let PermissionResult::Allowed(granted) =
        t3_evaluate(&evaluator, &Scope::tenant(T3_TENANT)).await
    else {
        panic!("an RG{{T,G}} grant must authorize its own tenant collection read");
    };
    assert_eq!(granted.grants.len(), 1);
    assert_eq!(
        granted.scope_type,
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![group_id],
        },
        "the assignment bound must survive collection evaluation; Global/TenantSubtree would leak"
    );
}

/// Performance invariant: one `evaluate_permission` call MUST resolve the
/// ancestor chain exactly once. Fetching it in both `get_subject_roles`
/// (`build_ancestor_scopes`) and the `assignment_applies` narrowing would
/// double the most expensive dependency call on the hottest path.
#[tokio::test]
async fn evaluate_permission_fetches_ancestors_once_per_call() {
    use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
    use std::sync::atomic::Ordering;

    let role_id = Uuid::now_v7();
    let def = t3_role_def(
        role_id,
        vec![PermissionRule::new("read", T3_RESOURCE)],
        vec![],
    );
    let assignment = t3_assignment(role_id, Scope::tenant(T3_TENANT));

    // Hold the concrete fake so we can read its get_ancestors call counter
    // after the evaluation; the chain is `[T3_TENANT]` (root).
    let resolver = Arc::new(FakeTenantResolverClient::with_chain(&[T3_TENANT]));
    let ancestors_calls = Arc::clone(&resolver.get_ancestors_calls);

    let evaluator = PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(SeededAssignmentRepo {
            assignments: vec![assignment],
        }),
        Arc::new(SeededDefinitionRepo { defs: vec![def] }),
        resolver,
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(NoopMetrics),
    );

    let ctx = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &ctx,
            "alice",
            PrincipalType::User,
            "read",
            T3_RESOURCE,
            &Scope::resource_group(T3_TENANT, Uuid::new_v4()),
        )
        .await
        .expect("evaluate_permission must not error with seeded repos");

    assert!(
        matches!(result, PermissionResult::Allowed(_)),
        "sanity: a Tenant{{T}} grant must allow an RG{{T,_}} request so the \
         second (narrowing) ancestor path is reached"
    );
    assert_eq!(
        ancestors_calls.load(Ordering::SeqCst),
        1,
        "evaluate_permission must resolve the ancestor chain exactly once per call"
    );
}

/// The per-RG narrowing INSIDE `evaluate_permission`: a grant scoped to
/// `RG{T, G1}` is in the tenant-wide candidate set `get_subject_roles`
/// returns, but `evaluate_permission` MUST drop it for an `RG{T, G2}`
/// request via `assignment_applies` — yielding `NoMatchingPermission`,
/// not a cross-RG grant. This is the headline escalation guard exercised
/// end-to-end (not just on `assignment_applies` in isolation).
#[tokio::test]
async fn evaluate_permission_drops_sibling_rg_grant() {
    let role_id = Uuid::now_v7();
    let def = t3_role_def(
        role_id,
        vec![PermissionRule::new("read", T3_RESOURCE)],
        vec![],
    );
    let assignment = t3_assignment(role_id, Scope::resource_group(T3_TENANT, Uuid::new_v4()));
    let evaluator = t3_evaluator(vec![assignment], vec![def]).await;

    // A *different* RG in the same tenant.
    let request = Scope::resource_group(T3_TENANT, Uuid::new_v4());
    let PermissionResult::Denied(denied) = t3_evaluate(&evaluator, &request).await else {
        panic!("sibling-RG escalation: an RG{{T,G1}} grant MUST NOT satisfy an RG{{T,G2}} request");
    };
    assert_eq!(
        denied.reason,
        DenyReason::NoMatchingPermission,
        "a sibling-RG grant MUST be dropped, not honoured"
    );
}

/// not-permission precedence: a role listing the operation in BOTH
/// `permissions` and `not_permissions` is excluded, denying via
/// `NotPermissionExclusion` rather than granting.
#[tokio::test]
async fn evaluate_permission_denies_via_not_permission_exclusion() {
    let role_id = Uuid::now_v7();
    let def = t3_role_def(
        role_id,
        vec![PermissionRule::new("read", T3_RESOURCE)],
        vec![PermissionRule::new("read", T3_RESOURCE)],
    );
    let assignment = t3_assignment(role_id, Scope::tenant(T3_TENANT));
    let evaluator = t3_evaluator(vec![assignment], vec![def]).await;

    let request = Scope::resource_group(T3_TENANT, Uuid::new_v4());
    let PermissionResult::Denied(denied) = t3_evaluate(&evaluator, &request).await else {
        panic!("a not_permissions exclusion MUST deny");
    };
    assert_eq!(
        denied.reason,
        DenyReason::NotPermissionExclusion,
        "a not_permissions match MUST take precedence and deny via exclusion"
    );
}

// ---------------------------------------------------------------------------
// `subject_id` MUST NOT be logged on the evaluator's debug entry
// events. The fix removed the `subject_id` field (a principal-identity
// leak into log sinks on the hottest path); this captures the evaluator's
// own `tracing` events and fails if the field — or the raw identifier —
// reappears. Uses a minimal hand-rolled subscriber so no
// `tracing-subscriber` dependency is needed.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CapturedFields {
    names: Vec<String>,
    values: Vec<String>,
}

struct FieldVisitor<'a>(&'a mut CapturedFields);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.names.push(field.name().to_owned());
        self.0.values.push(format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.names.push(field.name().to_owned());
        self.0.values.push(value.to_owned());
    }
}

struct CaptureSubscriber {
    captured: Arc<Mutex<CapturedFields>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _meta: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        // Only the evaluator's own entry events are under test.
        if event
            .metadata()
            .target()
            .starts_with("rbac::permission_evaluator")
        {
            let mut captured = self.captured.lock().expect("capture mutex poisoned");
            let mut visitor = FieldVisitor(&mut captured);
            event.record(&mut visitor);
        }
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[tokio::test]
async fn evaluator_debug_events_do_not_log_subject_id() {
    const SECRET_SUBJECT: &str = "SECRET-SUBJECT-IDENTIFIER-9f3c";
    let captured = Arc::new(Mutex::new(CapturedFields::default()));
    let evaluator = t3_evaluator(vec![], vec![]).await;
    let ctx = toolkit_security::SecurityContext::anonymous();

    {
        let subscriber = CaptureSubscriber {
            captured: Arc::clone(&captured),
        };
        // `#[tokio::test]` runs on a current-thread runtime, so the
        // thread-local default covers the awaited evaluator calls.
        let _guard = tracing::subscriber::set_default(subscriber);
        let _discarded = evaluator
            .get_subject_roles(&ctx, SECRET_SUBJECT, PrincipalType::User, T3_TENANT, true)
            .await;
        let _discarded = evaluator
            .evaluate_permission(
                &ctx,
                SECRET_SUBJECT,
                PrincipalType::User,
                "read",
                T3_RESOURCE,
                &Scope::tenant(T3_TENANT),
            )
            .await;
    }

    let captured = captured.lock().expect("capture mutex poisoned");
    // Sanity: the entry events fired, so the test is not vacuous.
    assert!(
        captured.names.iter().any(|n| n == "operation"),
        "expected to capture the evaluator entry events' `operation` field; \
         got fields: {:?}",
        captured.names
    );
    assert!(
        !captured.names.iter().any(|n| n == "subject_id"),
        "evaluator debug events MUST NOT carry a `subject_id` field (T4); \
         fields seen: {:?}",
        captured.names
    );
    assert!(
        !captured.values.iter().any(|v| v.contains(SECRET_SUBJECT)),
        "the subject identifier MUST NOT appear in any evaluator log field value (T4)"
    );
}

// ---------------------------------------------------------------------------
// Metrics — evaluate_permission records latency + result + deny / scope_type
// / error labels via the injected PermissionMetricsPort.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_permission_records_deny_metric_on_no_roles() {
    use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
    use crate::infra::metrics::test_harness::MetricsHarness;
    use std::sync::Arc;

    let harness = MetricsHarness::new();
    let evaluator = PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(EmptyRoleAssignmentRepo), // returns no assignments
        // `get_subject_roles` always calls `find_by_ids` (even with an empty
        // id list) on the batched path, so a panicking role-def repo would
        // fire here — use the empty-batch fake instead.
        Arc::new(EmptyFindByIdsRoleDefinitionRepo),
        Arc::new(FakeTenantResolverClient::with_chain(&[Uuid::nil()])),
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(harness.metrics()),
    );
    let ctx = toolkit_security::SecurityContext::anonymous();
    let _discarded = evaluator
        .evaluate_permission(
            &ctx,
            "subject-1",
            PrincipalType::User,
            "read",
            "gts.cf.example.resource.v1~",
            &Scope::Root,
        )
        .await;
    harness.force_flush();
    assert_eq!(
        harness.counter_value(
            "rbac_permission_deny_total",
            &[("reason", "no_matching_permission")]
        ),
        1,
        "no visible roles must record a no_matching_permission deny"
    );
    assert_eq!(
        harness.histogram_count(
            "rbac_permission_eval_duration_milliseconds",
            &[("result", "deny")]
        ),
        1,
        "every evaluate_permission call records one duration sample"
    );
}

// ---------------------------------------------------------------------------
// Metrics — get_subject_roles records latency with an include_group_roles
// label.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_subject_roles_records_duration_with_group_label() {
    use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
    use crate::infra::metrics::test_harness::MetricsHarness;
    use std::sync::Arc;

    let harness = MetricsHarness::new();
    let evaluator = PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(EmptyRoleAssignmentRepo),
        // `get_subject_roles` always calls `find_by_ids` (even with an empty
        // id list), so use the empty-batch fake rather than a panicking one.
        Arc::new(EmptyFindByIdsRoleDefinitionRepo),
        Arc::new(FakeTenantResolverClient::with_chain(&[Uuid::nil()])),
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(harness.metrics()),
    );
    let ctx = toolkit_security::SecurityContext::anonymous();
    let _discarded = evaluator
        .get_subject_roles(&ctx, "subject-1", PrincipalType::User, Uuid::nil(), false)
        .await;
    harness.force_flush();
    assert_eq!(
        harness.histogram_count(
            "rbac_subject_roles_duration_milliseconds",
            &[("include_group_roles", "false")]
        ),
        1
    );
}

// ---------------------------------------------------------------------------
// Metrics — tenant-resolver dependency calls record latency + outcome via
// the injected PermissionMetricsPort.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_ancestors_records_dependency_success() {
    use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
    use crate::infra::metrics::test_harness::MetricsHarness;
    use std::sync::Arc;

    let harness = MetricsHarness::new();
    let tenant = Uuid::new_v4();
    let evaluator = PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(EmptyRoleAssignmentRepo),
        // `get_subject_roles` always calls `find_by_ids` (even with an empty
        // id list), so use the empty-batch fake rather than a panicking one.
        Arc::new(EmptyFindByIdsRoleDefinitionRepo),
        Arc::new(FakeTenantResolverClient::with_chain(&[tenant])),
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(harness.metrics()),
    );
    let ctx = toolkit_security::SecurityContext::anonymous();
    // A tenant-scoped evaluate drives fetch_ancestor_tenant_ids.
    let _discarded = evaluator
        .evaluate_permission(
            &ctx,
            "subject-1",
            PrincipalType::User,
            "read",
            "gts.cf.example.resource.v1~",
            &Scope::Tenant { tenant_id: tenant },
        )
        .await;
    harness.force_flush();
    assert!(
        harness.counter_value(
            "rbac_dependency_health_total",
            &[
                ("dependency", "tenant_resolver"),
                ("operation", "get_ancestors"),
                ("outcome", "success")
            ]
        ) >= 1,
        "a tenant-scoped evaluate must record at least one successful get_ancestors"
    );
    assert!(
        harness.histogram_count(
            "rbac_dependency_query_duration_milliseconds",
            &[
                ("dependency", "tenant_resolver"),
                ("operation", "get_ancestors")
            ]
        ) >= 1
    );
}

// ---------------------------------------------------------------------------
// A membership walk that cannot terminate MUST fail closed.
//
// `resolve_group_memberships` runs inside an authorisation decision, so an
// upstream that keeps handing out cursors must not spin the request forever
// while the accumulated id set grows without bound. The walk carries the same
// two bounds the PDP's `hierarchy_client` uses: a page budget, and a
// non-advancing-cursor check that trips on page two.
// ---------------------------------------------------------------------------

/// Encode a valid `CursorV1` carrying `index` in the `k[0]` slot —
/// the wire shape `FakeRbacRgRead` and the DB-side paginator both emit.
/// The token must be a real `encode()` product: a hand-built literal
/// fails `CursorV1::decode` and would divert to the malformed-cursor
/// path instead of the one under test.
fn page_cursor_token(index: usize) -> String {
    toolkit_odata::CursorV1 {
        k: vec![index.to_string()],
        o: toolkit_odata::SortDir::Asc,
        s: "+group_id".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    }
    .encode()
    .expect("CursorV1::encode")
}

/// `list_memberships` always answers with the SAME valid cursor, so the
/// walk is handed a token it has already followed.
struct StuckCursorRgRead;

#[async_trait]
impl crate::domain::rg_port::RbacRgRead for StuckCursorRgRead {
    async fn get_group(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<crate::domain::rg_port::RbacRgGroup, crate::domain::rg_port::RbacRgReadError> {
        unreachable!("the non-advancing-cursor test MUST NOT reach get_group ({id})");
    }

    async fn group_names(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, crate::domain::rg_port::RbacRgReadError>
    {
        unreachable!("the non-advancing-cursor test MUST NOT reach group_names");
    }

    async fn list_memberships(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: &toolkit_odata::ODataQuery,
    ) -> Result<
        toolkit_odata::Page<crate::domain::rg_port::RbacRgMembership>,
        crate::domain::rg_port::RbacRgReadError,
    > {
        Ok(toolkit_odata::Page {
            items: vec![crate::domain::rg_port::RbacRgMembership {
                group_id: uuid!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            }],
            page_info: toolkit_odata::PageInfo {
                next_cursor: Some(page_cursor_token(1)),
                prev_cursor: None,
                limit: 100,
            },
        })
    }
}

/// `list_memberships` always answers with a FRESH, strictly advancing
/// cursor and never reports the end of the collection. The
/// non-advancing check cannot catch this one — only the page budget can.
struct EndlessCursorRgRead {
    page: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl crate::domain::rg_port::RbacRgRead for EndlessCursorRgRead {
    async fn get_group(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<crate::domain::rg_port::RbacRgGroup, crate::domain::rg_port::RbacRgReadError> {
        unreachable!("the page-cap test MUST NOT reach get_group ({id})");
    }

    async fn group_names(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, crate::domain::rg_port::RbacRgReadError>
    {
        unreachable!("the page-cap test MUST NOT reach group_names");
    }

    async fn list_memberships(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: &toolkit_odata::ODataQuery,
    ) -> Result<
        toolkit_odata::Page<crate::domain::rg_port::RbacRgMembership>,
        crate::domain::rg_port::RbacRgReadError,
    > {
        let n = self
            .page
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);
        // One item per page, so the item cap is never the reason the
        // walk ends — this isolates the page budget.
        Ok(toolkit_odata::Page {
            items: vec![crate::domain::rg_port::RbacRgMembership {
                group_id: uuid!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            }],
            page_info: toolkit_odata::PageInfo {
                next_cursor: Some(page_cursor_token(n)),
                prev_cursor: None,
                limit: 100,
            },
        })
    }
}

/// Drive `get_subject_roles` down the membership-resolution path with
/// `rg`, returning whatever it produced.
async fn subject_roles_with_rg(
    rg: Arc<dyn crate::domain::rg_port::RbacRgRead>,
) -> Result<Vec<rbac_sdk::models::SubjectRole>, RbacServiceError> {
    use crate::domain::model::scope_fakes::FakeTenantResolverClient;

    let evaluator = PermissionEvaluator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::new(PanicRoleAssignmentRepo),
        Arc::new(PanicRoleDefinitionRepo),
        Arc::new(FakeTenantResolverClient::with_chain(&[Uuid::nil()])),
        rg,
        Arc::new(NoopMetrics),
    );
    let ctx = toolkit_security::SecurityContext::anonymous();
    evaluator
        // User + include_group_roles=true is the only path that reaches
        // `resolve_group_memberships`.
        .get_subject_roles(&ctx, "alice", PrincipalType::User, Uuid::nil(), true)
        .await
}

#[tokio::test]
async fn get_subject_roles_fails_closed_on_non_advancing_cursor() {
    let result = subject_roles_with_rg(Arc::new(StuckCursorRgRead)).await;

    match result {
        Err(RbacServiceError::Internal { message }) => assert!(
            message.contains("non-advancing"),
            "internal error should name the non-advancing cursor to ease triage, got: {message}"
        ),
        other => panic!("a repeated cursor MUST fail closed, not loop or succeed; got {other:?}"),
    }
}

#[tokio::test]
async fn get_subject_roles_fails_closed_when_the_page_budget_runs_out() {
    let rg = Arc::new(EndlessCursorRgRead {
        page: std::sync::atomic::AtomicUsize::new(0),
    });
    let result = subject_roles_with_rg(rg).await;

    match result {
        Err(RbacServiceError::Internal { message }) => assert!(
            message.contains("page cap"),
            "internal error should name the page cap to ease triage, got: {message}"
        ),
        other => panic!(
            "an endlessly paginating upstream MUST fail closed once the page \
             budget is spent, not loop forever; got {other:?}"
        ),
    }
}
