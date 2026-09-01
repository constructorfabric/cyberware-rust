//! Tests for [`super::PrincipalNameHydrator`].
//!
//! Two things are being pinned here, and the second is the one that rots
//! silently: that names land on the right rows, and that the *number of
//! reads* is a function of the page's distinct principals and roles rather
//! than of its row count. A per-row implementation would satisfy every
//! name assertion below while making one Keycloak membership drain and one
//! role-definition query per row, so the call counters are load-bearing.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use toolkit_db::secure::DBRunner;

use async_trait::async_trait;
use chrono::SubsecRound;
use parking_lot::Mutex;
use rbac_sdk::models::{PermissionRule, PrincipalType, Scope};
use toolkit_odata::{ODataQuery, Page as ODataPage};
use toolkit_security::SecurityContext;
use uuid::{Uuid, uuid};

use super::PrincipalNameHydrator;
use crate::config::PrincipalNamesConfig;
use crate::domain::error::DomainError;
use crate::domain::etag::Etag;
use crate::domain::model::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
use crate::domain::model::{RoleAssignmentModel, RoleDefinitionModel};
use crate::domain::ports::metrics::{NameKind, NameOutcome, PrincipalNameMetricsPort};
use crate::domain::ports::principal_name_reader::PrincipalNameError;
use crate::domain::ports::principal_name_reader_mock::FakePrincipalNameReader;
use crate::domain::role_assignment::HydratedRoleAssignment;
use crate::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionPatch, RoleDefinitionRepository, RoleDefinitionVisibility,
};

const T1: Uuid = uuid!("11111111-1111-1111-1111-111111111111");
const T2: Uuid = uuid!("22222222-2222-2222-2222-222222222222");
const ROOT: Uuid = uuid!("00000000-0000-0000-0000-0000000000ff");
const G1: Uuid = uuid!("aaaaaaaa-0000-0000-0000-000000000001");

fn ctx() -> SecurityContext {
    SecurityContext::anonymous()
}

/// Records every `(kind, outcome, count)` sample so a test can assert the
/// counter is categorical and counts principals, not calls.
#[derive(Default)]
struct RecordingMetrics {
    samples: Mutex<Vec<(NameKind, NameOutcome, u64)>>,
}

impl RecordingMetrics {
    fn count(&self, kind: NameKind, outcome: NameOutcome) -> u64 {
        self.samples
            .lock()
            .iter()
            .filter(|(k, o, _)| *k == kind && *o == outcome)
            .map(|(_, _, c)| *c)
            .sum()
    }
}

impl PrincipalNameMetricsPort for RecordingMetrics {
    fn principal_name_resolve(&self, kind: NameKind, outcome: NameOutcome, count: u64) {
        self.samples.lock().push((kind, outcome, count));
    }
}

/// Row template. Callers override only the fields their case is about.
fn row(principal_id: &str, principal_type: PrincipalType, scope: Scope) -> RoleAssignmentModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    RoleAssignmentModel {
        id: Uuid::now_v7(),
        role_definition_id: Uuid::now_v7(),
        principal_id: principal_id.to_owned(),
        principal_type,
        scope,
        created_at: now,
        updated_at: now,
        created_by: "author-1".to_owned(),
        // No recorded author identity by default — the legacy shape, and
        // the one every test that is not about authors wants.
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

fn user_row(principal_id: &str, tenant: Uuid) -> RoleAssignmentModel {
    row(principal_id, PrincipalType::User, Scope::tenant(tenant))
}

fn root_user_row(principal_id: &str) -> RoleAssignmentModel {
    row(principal_id, PrincipalType::User, Scope::root())
}

fn sp_row(principal_id: &str, tenant: Uuid) -> RoleAssignmentModel {
    row(
        principal_id,
        PrincipalType::ServicePrincipal,
        Scope::tenant(tenant),
    )
}

fn group_row(group_id: Uuid, tenant: Uuid) -> RoleAssignmentModel {
    row(
        &group_id.to_string(),
        PrincipalType::Group,
        Scope::tenant(tenant),
    )
}

/// A user row whose author identity is spelled out: `author` is the stored
/// `created_by` subject id, and `identity` is the `(kind, home tenant)` pair
/// `create` stamps from the caller's `SecurityContext`. `None` reproduces a
/// row written before those columns existed.
fn user_row_with_author(
    principal_id: &str,
    tenant: Uuid,
    author: &str,
    identity: Option<(PrincipalType, Uuid)>,
) -> RoleAssignmentModel {
    let mut model = user_row(principal_id, tenant);
    model.created_by = author.to_owned();
    model.created_by_type = identity.map(|(kind, _)| kind);
    model.created_by_tenant_id = identity.map(|(_, home_tenant)| home_tenant);
    model
}

/// The role-definition visibility of a caller who may see every role.
/// The default for tests that are not about visibility, so they read as
/// "role names resolve" rather than as an authorization exercise.
fn all_roles() -> RoleDefinitionVisibility {
    RoleDefinitionVisibility::All
}

fn names(out: &[HydratedRoleAssignment]) -> Vec<Option<&str>> {
    out.iter()
        .map(|h| h.principal_name.as_deref())
        .collect::<Vec<_>>()
}

/// `RoleDefinitionRepository` answering `find_by_ids` from a seeded
/// table of rows, counting the calls, and optionally failing.
///
/// The call counter is the load-bearing part: role names come from RBAC's
/// own table, and the cheap-looking mistake is one `find_by_id` per row.
/// Only the batched reads may ever be reached from a read path, so every
/// other method panics rather than returning a plausible empty answer.
///
/// It deliberately does **not** override `find_by_ids_visible`: the trait's
/// default filters `find_by_ids` through the same `visibility_admits`
/// predicate the storage layer lowers into SQL, so these tests exercise the
/// real narrowing rule rather than a fake's imitation of it.
#[derive(Default)]
struct FakeRoleDefinitionRepo {
    rows: HashMap<Uuid, RoleDefinitionModel>,
    calls: AtomicUsize,
    /// Id set of each `find_by_ids` call, in call order — so a test can
    /// assert that many rows sharing a role produced one batch containing
    /// that role once.
    seen_ids: Mutex<Vec<Vec<Uuid>>>,
    /// When set, `find_by_ids` fails instead of answering — the shape a
    /// database hiccup takes on the read path.
    fails: bool,
    /// When set, the read yields to the runtime before answering.
    ///
    /// `timeout_at` polls its future once *before* it checks the deadline,
    /// so a fake that is ready on the first poll succeeds even on an
    /// expired budget — and a test built on one cannot tell whether this
    /// phase was reached in time or merely reached at all. A real database
    /// read returns `Pending` first; yielding reproduces that, which is
    /// what makes the phase-ordering test below able to fail.
    yields_first: bool,
}

impl FakeRoleDefinitionRepo {
    /// A custom role owned by `T1` — the default shape, since most tests
    /// are about batching rather than about who may see the row.
    fn with_role(self, id: Uuid, name: &str) -> Self {
        self.with_custom_role(id, name, T1)
    }

    /// A custom role owned by an explicit tenant. Custom roles are the
    /// ones the catalog hides from other tenants, so the owner is what
    /// makes a visibility test mean anything.
    fn with_custom_role(mut self, id: Uuid, name: &str, owner: Uuid) -> Self {
        let mut model = role_def(id, name);
        model.owner_tenant_id = Some(owner);
        self.rows.insert(id, model);
        self
    }

    /// A platform built-in. Visible to every authenticated caller,
    /// whatever their readable scopes.
    fn with_builtin_role(mut self, id: Uuid, name: &str) -> Self {
        let mut model = role_def(id, name);
        model.is_built_in = true;
        model.owner_tenant_id = None;
        self.rows.insert(id, model);
        self
    }

    fn failing(mut self) -> Self {
        self.fails = true;
        self
    }

    fn yielding(mut self) -> Self {
        self.yields_first = true;
        self
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Ids handed to call `index`, sorted so the assertion does not depend
    /// on `HashSet` iteration order.
    fn sorted_ids(&self, index: usize) -> Vec<Uuid> {
        let mut ids = self.seen_ids.lock()[index].clone();
        ids.sort();
        ids
    }
}

#[async_trait]
impl RoleDefinitionRepository for FakeRoleDefinitionRepo {
    async fn find_by_ids<C: DBRunner>(
        &self,
        _db: &C,
        ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        if self.yields_first {
            tokio::task::yield_now().await;
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_ids.lock().push(ids.to_vec());
        if self.fails {
            return Err(DomainError::internal("role_definitions read failed"));
        }
        Ok(ids
            .iter()
            .filter_map(|id| self.rows.get(id).cloned())
            .collect())
    }
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError> {
        panic!("the hydrator reads role definitions in batches and writes none");
    }
    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError> {
        panic!("a per-row role lookup is exactly what the batched read must avoid");
    }
    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
        _query: &ODataQuery,
    ) -> Result<ODataPage<RoleDefinitionModel>, DomainError> {
        panic!("the hydrator resolves names by id, never by listing");
    }
    async fn update<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _patch: RoleDefinitionPatch,
        _expected_etag: &Etag,
    ) -> Result<RoleDefinitionModel, DomainError> {
        panic!("hydration is a read path");
    }
    async fn delete<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _expected_etag: &Etag,
    ) -> Result<(), DomainError> {
        panic!("hydration is a read path");
    }
    async fn count_assignments_for_role<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<u64, DomainError> {
        panic!("hydration does not count assignments");
    }
    async fn count_by_type<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: RoleDefinitionVisibility,
    ) -> Result<crate::domain::role_definition_repo::RoleTypeCounts, DomainError> {
        panic!("hydration does not summarise the catalog");
    }
}

/// Minimal role-definition row: only `id` and `name` matter to hydration.
fn role_def(id: Uuid, name: &str) -> RoleDefinitionModel {
    let now = chrono::Utc::now().trunc_subsecs(6);
    RoleDefinitionModel {
        id,
        name: name.to_owned(),
        description: None,
        is_built_in: false,
        permissions: vec![PermissionRule::new("read", "gts.cf.core.rbac.role.v1~")],
        not_permissions: Vec::new(),
        assignable_scopes: vec![Scope::tenant(T1)],
        owner_tenant_id: Some(T1),
        created_at: now,
        updated_at: now,
        created_by: "tester".to_owned(),
    }
}

/// Hydrator over the supplied user reader; RG and tenant-resolver fakes
/// are the caller's, so a test can seed group names or the root tenant.
/// Role names are out of scope for these callers, hence the empty role
/// repo: an unseeded id resolves to no name, exactly as a deleted
/// definition would.
async fn hydrator_with(
    users: Arc<FakePrincipalNameReader>,
    rg: Arc<FakeRbacRgRead>,
    tenants: Arc<FakeTenantResolverClient>,
    metrics: Arc<RecordingMetrics>,
) -> PrincipalNameHydrator<FakeRoleDefinitionRepo> {
    hydrator_with_roles(
        users,
        rg,
        Arc::new(FakeRoleDefinitionRepo::default()),
        tenants,
        metrics,
    )
    .await
}

/// Full-control builder, for the tests that are about role names.
async fn hydrator_with_roles(
    users: Arc<FakePrincipalNameReader>,
    rg: Arc<FakeRbacRgRead>,
    roles: Arc<FakeRoleDefinitionRepo>,
    tenants: Arc<FakeTenantResolverClient>,
    metrics: Arc<RecordingMetrics>,
) -> PrincipalNameHydrator<FakeRoleDefinitionRepo> {
    PrincipalNameHydrator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        users,
        rg,
        roles,
        tenants,
        metrics,
    )
}

/// The common case: no groups, no root-scoped rows, no metrics assertions.
async fn hydrator(
    users: Arc<FakePrincipalNameReader>,
) -> PrincipalNameHydrator<FakeRoleDefinitionRepo> {
    hydrator_with(
        users,
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await
}

/// One reader call per distinct tenant on the page, never one per row.
#[tokio::test]
async fn users_are_resolved_with_one_call_per_tenant() {
    let users = Arc::new(
        FakePrincipalNameReader::default()
            .with_name(T1, "u1", "Ada")
            .with_name(T1, "u2", "Alan")
            .with_name(T2, "u3", "Grace"),
    );
    let h = hydrator(Arc::clone(&users)).await;
    let rows = vec![user_row("u1", T1), user_row("u2", T1), user_row("u3", T2)];

    let out = h.hydrate(&ctx(), rows, Some(all_roles())).await;

    assert_eq!(names(&out), vec![Some("Ada"), Some("Alan"), Some("Grace")]);
    assert_eq!(users.call_count(), 2, "one call per tenant, not per row");
}

/// Rows in one tenant collapse into a single call carrying both ids —
/// the property that makes a 100-row page cost one upstream read.
#[tokio::test]
async fn one_tenants_ids_arrive_in_a_single_call() {
    let users = Arc::new(
        FakePrincipalNameReader::default()
            .with_name(T1, "u1", "Ada")
            .with_name(T1, "u2", "Alan"),
    );
    let h = hydrator(Arc::clone(&users)).await;

    let _ = h
        .hydrate(
            &ctx(),
            vec![user_row("u1", T1), user_row("u2", T1)],
            Some(all_roles()),
        )
        .await;

    let seen = users.seen_ids.lock().clone();
    assert_eq!(seen.len(), 1);
    let mut ids = seen[0].clone();
    ids.sort();
    assert_eq!(ids, vec!["u1".to_owned(), "u2".to_owned()]);
}

/// A root-scoped row carries no tenant, so the lookup goes to the
/// platform root tenant.
#[tokio::test]
async fn root_scoped_row_resolves_against_the_root_tenant() {
    let users =
        Arc::new(FakePrincipalNameReader::default().with_name(ROOT, "admin", "Platform Admin"));
    let h = hydrator_with(
        Arc::clone(&users),
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(FakeTenantResolverClient::with_chain(&[ROOT]).with_root_tenant(ROOT)),
        Arc::new(RecordingMetrics::default()),
    )
    .await;

    let out = h
        .hydrate(&ctx(), vec![root_user_row("admin")], Some(all_roles()))
        .await;

    assert_eq!(names(&out), vec![Some("Platform Admin")]);
    assert_eq!(users.seen_tenants.lock().as_slice(), &[ROOT]);
}

/// A page without a root-scoped row must not consult the tenant resolver
/// at all — the fake's `get_root_tenant` is `unimplemented!()` without a
/// seeded root, so an unnecessary call would panic here.
#[tokio::test]
async fn tenant_scoped_page_does_not_resolve_the_root_tenant() {
    let users = Arc::new(FakePrincipalNameReader::default().with_name(T1, "u1", "Ada"));
    let h = hydrator(users).await;

    let out = h
        .hydrate(&ctx(), vec![user_row("u1", T1)], Some(all_roles()))
        .await;

    assert_eq!(names(&out), vec![Some("Ada")]);
}

/// Unresolvable ids and non-user kinds degrade to no name, and never
/// fail. A `ServicePrincipal` must not even reach the user reader: there
/// is no `subject_id -> client_id` lookup on the platform, so asking
/// would be a wasted round trip on every page.
#[tokio::test]
async fn unresolved_and_non_user_kinds_degrade() {
    let users = Arc::new(FakePrincipalNameReader::default());
    let h = hydrator(Arc::clone(&users)).await;
    let rows = vec![user_row("missing", T1), sp_row("sp-subject", T1)];

    let out = h.hydrate(&ctx(), rows, Some(all_roles())).await;

    assert_eq!(names(&out), vec![None, None]);
    assert_eq!(users.seen_tenants.lock().len(), 1);
    let asked = users.seen_ids.lock().clone();
    assert_eq!(
        asked[0],
        vec!["missing".to_owned()],
        "the service-principal id must not be asked about"
    );
}

/// Upstream failure yields the full page with no names — same rows, same
/// order, no error.
#[tokio::test]
async fn upstream_failure_yields_rows_without_names() {
    let users = Arc::new(FakePrincipalNameReader::default().failing(
        PrincipalNameError::Unavailable {
            detail: "kc down".to_owned(),
        },
    ));
    let h = hydrator(users).await;

    let out = h
        .hydrate(
            &ctx(),
            vec![user_row("u1", T1), user_row("u2", T2)],
            Some(all_roles()),
        )
        .await;

    assert_eq!(out.len(), 2);
    assert_eq!(names(&out), vec![None, None]);
}

/// A denied read is degradation, not failure: the caller may list role
/// assignments but not read users, and still gets every row.
#[tokio::test]
async fn denied_user_read_degrades() {
    let users = Arc::new(FakePrincipalNameReader::default().failing(PrincipalNameError::Denied));
    let h = hydrator(users).await;

    let out = h
        .hydrate(&ctx(), vec![user_row("u1", T1)], Some(all_roles()))
        .await;

    assert_eq!(names(&out), vec![None]);
}

/// Group principals resolve through the RG port, in one call for the
/// whole page, with no user lookup at all.
#[tokio::test]
async fn group_principals_resolve_through_the_rg_port() {
    let users = Arc::new(FakePrincipalNameReader::default());
    let rg = Arc::new(FakeRbacRgRead::default().with_group_name(G1, "Engineering"));
    let h = hydrator_with(
        Arc::clone(&users),
        Arc::clone(&rg),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;

    // Two rows for the same group: one listing, one name.
    let out = h
        .hydrate(
            &ctx(),
            vec![group_row(G1, T1), group_row(G1, T2)],
            Some(all_roles()),
        )
        .await;

    assert_eq!(names(&out), vec![Some("Engineering"), Some("Engineering")]);
    assert_eq!(rg.group_names_calls.load(Ordering::SeqCst), 1);
    assert_eq!(users.call_count(), 0, "a group page needs no user read");
}

/// A `Group` principal id that is not UUID-shaped (only possible for a
/// row written before group ids were validated) degrades instead of
/// poisoning the batch.
#[tokio::test]
async fn non_uuid_group_id_degrades() {
    let rg = Arc::new(FakeRbacRgRead::default().with_group_name(G1, "Engineering"));
    let h = hydrator_with(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::clone(&rg),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;
    let bad = row("not-a-uuid", PrincipalType::Group, Scope::tenant(T1));

    let out = h.hydrate(&ctx(), vec![bad], Some(all_roles())).await;

    assert_eq!(names(&out), vec![None]);
    assert_eq!(
        rg.group_names_calls.load(Ordering::SeqCst),
        0,
        "nothing resolvable, so no listing"
    );
}

/// An empty page touches nothing.
#[tokio::test]
async fn empty_page_makes_no_calls() {
    let users = Arc::new(FakePrincipalNameReader::default());
    let rg = Arc::new(FakeRbacRgRead::default());
    let h = hydrator_with(
        Arc::clone(&users),
        Arc::clone(&rg),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;

    let out = h.hydrate(&ctx(), Vec::new(), Some(all_roles())).await;

    assert!(out.is_empty());
    assert_eq!(users.call_count(), 0);
    assert_eq!(rg.group_names_calls.load(Ordering::SeqCst), 0);
}

/// Outcomes are counted once per principal on the page, by kind. A
/// service principal counts as `unsupported` — a permanent platform gap
/// — rather than `degraded`, so a dashboard does not read it as an
/// outage.
#[tokio::test]
async fn outcomes_are_counted_per_principal_by_kind() {
    let users = Arc::new(FakePrincipalNameReader::default().with_name(T1, "u1", "Ada"));
    let rg = Arc::new(FakeRbacRgRead::default().with_group_name(G1, "Engineering"));
    let metrics = Arc::new(RecordingMetrics::default());
    let h = hydrator_with(
        users,
        rg,
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::clone(&metrics),
    )
    .await;

    let _ = h
        .hydrate(
            &ctx(),
            vec![
                user_row("u1", T1),
                user_row("missing", T1),
                group_row(G1, T1),
                sp_row("sp-subject", T1),
            ],
            Some(all_roles()),
        )
        .await;

    assert_eq!(metrics.count(NameKind::User, NameOutcome::Resolved), 1);
    assert_eq!(metrics.count(NameKind::User, NameOutcome::Degraded), 1);
    assert_eq!(metrics.count(NameKind::Group, NameOutcome::Resolved), 1);
    assert_eq!(
        metrics.count(NameKind::Other, NameOutcome::Unsupported),
        1,
        "a service principal is `other`/`unsupported`, never a failed user lookup"
    );
    // None of these rows records an author identity (the `row()` template
    // leaves it unset), so every row's author is "nothing can name this".
    assert_eq!(metrics.count(NameKind::Author, NameOutcome::Unsupported), 4);
}

/// Author names ride the same reader as holders, keyed by the author
/// identity the row recorded at create time; a row with no recorded author
/// kind degrades to no name.
#[tokio::test]
async fn author_names_use_the_stored_author_identity() {
    let users = Arc::new(FakePrincipalNameReader::default().with_name(T1, "author-1", "Grace"));
    let h = hydrator(Arc::clone(&users)).await;
    let recorded = user_row_with_author("u1", T1, "author-1", Some((PrincipalType::User, T1)));
    let legacy = user_row_with_author("u2", T1, "platform-bootstrap", None);

    let out = h
        .hydrate(&ctx(), vec![recorded, legacy], Some(all_roles()))
        .await;

    assert_eq!(out[0].created_by_name.as_deref(), Some("Grace"));
    assert!(
        out[1].created_by_name.is_none(),
        "a row with no recorded author identity MUST stay unnamed"
    );
    // Holders and author share the tenant, so they share the one call: the
    // author is not a second round trip per page.
    assert_eq!(users.call_count(), 1);
}

/// The author is looked up in the tenant the *row* recorded, never in the
/// row's scope tenant. A partner admin granting a role inside a child
/// tenant is the ordinary case, and reusing the scope tenant would ask the
/// wrong tenant and name nobody.
#[tokio::test]
async fn author_is_resolved_in_its_own_tenant_not_the_rows_scope() {
    let users = Arc::new(FakePrincipalNameReader::default().with_name(T2, "author-1", "Grace"));
    let h = hydrator(Arc::clone(&users)).await;
    let row = user_row_with_author("u1", T1, "author-1", Some((PrincipalType::User, T2)));

    let out = h.hydrate(&ctx(), vec![row], Some(all_roles())).await;

    assert_eq!(out[0].created_by_name.as_deref(), Some("Grace"));
    let mut seen = users.seen_tenants.lock().clone();
    seen.sort();
    let mut expected = vec![T1, T2];
    expected.sort();
    assert_eq!(seen, expected, "one call per distinct lookup tenant");
}

/// A machine author is a *recorded* identity that still cannot be named:
/// nothing on the platform maps a service-principal subject id back to its
/// client, so the author must not even reach the user reader.
#[tokio::test]
async fn service_principal_author_is_never_asked_about() {
    let users =
        Arc::new(FakePrincipalNameReader::default().with_name(T1, "sp-author", "must not be used"));
    let h = hydrator(Arc::clone(&users)).await;
    let row = user_row_with_author(
        "u1",
        T1,
        "sp-author",
        Some((PrincipalType::ServicePrincipal, T1)),
    );

    let out = h.hydrate(&ctx(), vec![row], Some(all_roles())).await;

    assert!(out[0].created_by_name.is_none());
    let asked = users.seen_ids.lock().clone();
    assert_eq!(
        asked[0],
        vec!["u1".to_owned()],
        "a service-principal author MUST NOT reach the user reader"
    );
}

// ---------------------------------------------------------------------------
// Role-definition names
// ---------------------------------------------------------------------------
//
// The role name is the one name on the row that comes from RBAC's own
// table. That makes it cheap, not exempt: the same batching and the same
// degradation are asserted here as for the two upstream-backed names,
// because a consumer that had to special-case one of the three would have
// learned nothing from the other two.

/// The name of the granted role reaches every row of a page.
#[tokio::test]
async fn role_definition_names_are_resolved_for_a_page() {
    let admin = Uuid::now_v7();
    let auditor = Uuid::now_v7();
    let roles = Arc::new(
        FakeRoleDefinitionRepo::default()
            .with_role(admin, "Tenant Administrator")
            .with_role(auditor, "Auditor"),
    );
    let h = hydrator_with_roles(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;
    let mut first = user_row("u1", T1);
    first.role_definition_id = admin;
    let mut second = user_row("u2", T1);
    second.role_definition_id = auditor;

    let out = h
        .hydrate(&ctx(), vec![first, second], Some(all_roles()))
        .await;

    assert_eq!(
        out.iter()
            .map(|row| row.role_definition_name.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("Tenant Administrator"), Some("Auditor")]
    );
    assert_eq!(roles.calls(), 1, "one batched read for the whole page");
}

/// An authorization grid shows the same handful of roles on every row, so
/// the read must cost one query carrying each *distinct* role id once — not
/// one query per row, and not one id per row inside the query.
#[tokio::test]
async fn many_rows_sharing_a_role_cost_one_batched_read() {
    let shared = Uuid::now_v7();
    let roles =
        Arc::new(FakeRoleDefinitionRepo::default().with_role(shared, "Tenant Administrator"));
    let h = hydrator_with_roles(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;
    let rows: Vec<RoleAssignmentModel> = (0..25)
        .map(|i| {
            let mut row = user_row(&format!("u{i}"), T1);
            row.role_definition_id = shared;
            row
        })
        .collect();

    let out = h.hydrate(&ctx(), rows, Some(all_roles())).await;

    assert_eq!(out.len(), 25);
    assert!(
        out.iter()
            .all(|row| row.role_definition_name.as_deref() == Some("Tenant Administrator"))
    );
    assert_eq!(roles.calls(), 1, "25 rows, one read");
    assert_eq!(
        roles.sorted_ids(0),
        vec![shared],
        "the batch MUST carry the distinct role id once, not once per row"
    );
}

/// A role id with no row — a definition deleted inside the FK-restrict race
/// window — degrades to no name, and leaves the rest of the page named.
#[tokio::test]
async fn an_unknown_role_id_degrades_without_affecting_the_page() {
    let known = Uuid::now_v7();
    let vanished = Uuid::now_v7();
    let roles = Arc::new(FakeRoleDefinitionRepo::default().with_role(known, "Auditor"));
    let metrics = Arc::new(RecordingMetrics::default());
    let h = hydrator_with_roles(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::clone(&metrics),
    )
    .await;
    let mut named = user_row("u1", T1);
    named.role_definition_id = known;
    let mut unnamed = user_row("u2", T1);
    unnamed.role_definition_id = vanished;

    let out = h
        .hydrate(&ctx(), vec![named, unnamed], Some(all_roles()))
        .await;

    assert_eq!(out[0].role_definition_name.as_deref(), Some("Auditor"));
    assert!(out[1].role_definition_name.is_none());
    assert_eq!(
        metrics.count(NameKind::RoleDefinition, NameOutcome::Resolved),
        1
    );
    assert_eq!(
        metrics.count(NameKind::RoleDefinition, NameOutcome::Degraded),
        1
    );
}

/// A failing role read leaves the page exactly as it was: same rows, same
/// order, no error, no names. Same contract as an upstream outage, which is
/// the point — hydration can never be the reason a read fails.
#[tokio::test]
async fn a_failing_role_read_leaves_the_page_intact() {
    let role = Uuid::now_v7();
    let roles = Arc::new(
        FakeRoleDefinitionRepo::default()
            .with_role(role, "Tenant Administrator")
            .failing(),
    );
    let users = Arc::new(FakePrincipalNameReader::default().with_name(T1, "u1", "Ada"));
    let metrics = Arc::new(RecordingMetrics::default());
    let h = hydrator_with_roles(
        users,
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::clone(&metrics),
    )
    .await;
    let mut first = user_row("u1", T1);
    first.role_definition_id = role;
    let mut second = user_row("u2", T1);
    second.role_definition_id = role;

    let out = h
        .hydrate(&ctx(), vec![first, second], Some(all_roles()))
        .await;

    assert_eq!(out.len(), 2, "the page keeps every row");
    assert!(out.iter().all(|row| row.role_definition_name.is_none()));
    // The principal name is unaffected: the two resolutions are independent,
    // so a database hiccup must not blank out a name that did resolve.
    assert_eq!(names(&out), vec![Some("Ada"), None]);
    assert_eq!(
        metrics.count(NameKind::RoleDefinition, NameOutcome::Degraded),
        2
    );
}

/// An empty page still touches nothing — including the local role read,
/// which is cheap but not free.
#[tokio::test]
async fn empty_page_makes_no_role_read() {
    let roles = Arc::new(FakeRoleDefinitionRepo::default());
    let h = hydrator_with_roles(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;

    assert!(
        h.hydrate(&ctx(), Vec::new(), Some(all_roles()))
            .await
            .is_empty()
    );
    assert_eq!(roles.calls(), 0);
}

// ---------------------------------------------------------------------------
// A name is never an empty string
// ---------------------------------------------------------------------------
//
// A blank name is worse than no name: `"principal_name": "   "` renders as
// an empty cell that reads as a bug, while an *absent* field renders as the
// id the row still carries. Each reader drops blanks at its own end; the
// three tests below go through the hydrator's merge step, which is the
// backstop that catches a source which did not.

/// A user name that arrives blank is treated as unresolved — and counted
/// as degraded, because from the reader's point of view nothing resolved.
#[tokio::test]
async fn a_blank_user_name_is_absent_not_empty() {
    let users = Arc::new(FakePrincipalNameReader::default().with_name(T1, "u1", "   "));
    let metrics = Arc::new(RecordingMetrics::default());
    let h = hydrator_with(
        users,
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::clone(&metrics),
    )
    .await;

    let out = h
        .hydrate(&ctx(), vec![user_row("u1", T1)], Some(all_roles()))
        .await;

    assert_eq!(
        names(&out),
        vec![None],
        "a blank name MUST NOT reach the wire"
    );
    assert_eq!(metrics.count(NameKind::User, NameOutcome::Degraded), 1);
}

/// Same rule for a group whose upstream name is blank. The RG fake hands
/// its seeded value back verbatim, so this exercises the merge-step gate
/// rather than the adapter's own.
#[tokio::test]
async fn a_blank_group_name_is_absent_not_empty() {
    let rg = Arc::new(FakeRbacRgRead::default().with_group_name(G1, " \t "));
    let h = hydrator_with(
        Arc::new(FakePrincipalNameReader::default()),
        rg,
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;

    let out = h
        .hydrate(&ctx(), vec![group_row(G1, T1)], Some(all_roles()))
        .await;

    assert_eq!(names(&out), vec![None]);
}

/// And for a role definition whose stored name is blank — the one name
/// that comes from RBAC's own table, so the only one where a blank could
/// be blamed on someone else's data.
#[tokio::test]
async fn a_blank_role_definition_name_is_absent_not_empty() {
    let role = Uuid::now_v7();
    let roles = Arc::new(FakeRoleDefinitionRepo::default().with_role(role, "   "));
    let h = hydrator_with_roles(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;
    let mut row = user_row("u1", T1);
    row.role_definition_id = role;

    let out = h.hydrate(&ctx(), vec![row], Some(all_roles())).await;

    assert!(out[0].role_definition_name.is_none());
}

// ---------------------------------------------------------------------------
// Role names stay behind the role-definition visibility gate
// ---------------------------------------------------------------------------
//
// `GET /rbac/v1/role-definitions/{id}` answers 404 for another tenant's
// custom role, deliberately, to avoid disclosing that it exists. An
// ancestor admin may grant such a role at a descendant scope, and the
// descendant's admin may read the resulting assignment row — so the row's
// `role_definition_name` must not become the back door to the string the
// 404 withholds. The row itself is still served: the gate narrows the
// decoration, never the page.

/// A caller who may only see `T1`'s custom roles gets the row granting
/// `T2`'s custom role — with no name on it.
#[tokio::test]
async fn another_tenants_custom_role_is_served_without_a_name() {
    let mine = Uuid::now_v7();
    let theirs = Uuid::now_v7();
    let roles = Arc::new(
        FakeRoleDefinitionRepo::default()
            .with_custom_role(mine, "My Custom Role", T1)
            .with_custom_role(theirs, "Their Secret Role", T2),
    );
    let h = hydrator_with_roles(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;
    let mut own = user_row("u1", T1);
    own.role_definition_id = mine;
    let mut granted_from_above = user_row("u2", T1);
    granted_from_above.role_definition_id = theirs;

    let out = h
        .hydrate(
            &ctx(),
            vec![own, granted_from_above],
            Some(RoleDefinitionVisibility::CustomForTenantsWithBuiltins(
                vec![T1],
            )),
        )
        .await;

    assert_eq!(out.len(), 2, "both rows are still served");
    assert_eq!(
        out[0].role_definition_name.as_deref(),
        Some("My Custom Role")
    );
    assert!(
        out[1].role_definition_name.is_none(),
        "the name of a role this caller cannot fetch MUST NOT be disclosed"
    );
}

/// Built-in roles are visible to every authenticated caller, so the
/// narrowing must not blank out the names the whole catalog shares.
#[tokio::test]
async fn builtin_role_names_resolve_for_a_tenant_scoped_caller() {
    let builtin = Uuid::now_v7();
    let roles = Arc::new(FakeRoleDefinitionRepo::default().with_builtin_role(builtin, "Owner"));
    let h = hydrator_with_roles(
        Arc::new(FakePrincipalNameReader::default()),
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .await;
    let mut row = user_row("u1", T1);
    row.role_definition_id = builtin;

    let out = h
        .hydrate(
            &ctx(),
            vec![row],
            // A caller whose only custom-role visibility is another
            // tenant's: built-ins are still theirs to read.
            Some(RoleDefinitionVisibility::CustomForTenantsWithBuiltins(
                vec![T2],
            )),
        )
        .await;

    assert_eq!(out[0].role_definition_name.as_deref(), Some("Owner"));
}

/// A caller whose visibility could not be derived at all gets the page,
/// with role ids and no role names — never an error, and never a fallback
/// to the unnarrowed read.
#[tokio::test]
async fn undeterminable_visibility_degrades_only_the_role_name() {
    let role = Uuid::now_v7();
    let roles = Arc::new(FakeRoleDefinitionRepo::default().with_role(role, "Tenant Administrator"));
    let users = Arc::new(FakePrincipalNameReader::default().with_name(T1, "u1", "Ada"));
    let metrics = Arc::new(RecordingMetrics::default());
    let h = hydrator_with_roles(
        users,
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::clone(&metrics),
    )
    .await;
    let mut row = user_row("u1", T1);
    row.role_definition_id = role;

    let out = h.hydrate(&ctx(), vec![row], None).await;

    assert_eq!(out.len(), 1, "the row is still served");
    assert!(out[0].role_definition_name.is_none());
    assert_eq!(
        names(&out),
        vec![Some("Ada")],
        "the principal name is unaffected - the three names are independent"
    );
    assert_eq!(
        roles.calls(),
        0,
        "with no visibility to apply, the table MUST NOT be read at all"
    );
    assert_eq!(
        metrics.count(NameKind::RoleDefinition, NameOutcome::Degraded),
        1
    );
}

// ---------------------------------------------------------------------------
// Per-request bounds: the deadline and the tenant fan-out budget
// ---------------------------------------------------------------------------
//
// The per-tenant budgets bound one tenant. Nothing bounds how many tenants
// a page spans — that is chosen by whoever wrote the assignments — so
// without the two bounds below a root-scope listing across many tenants
// does not degrade, it hangs: every tenant costs a full Keycloak
// membership drain, sequentially, with no deadline.

/// A reader that answers one tenant instantly and stalls forever on any
/// other. Stalling rather than sleeping a fixed time: the assertion is
/// that the deadline cuts the request short, and a sleep long enough to be
/// unambiguous would otherwise be the test's runtime.
struct OneFastTenantReader {
    fast_tenant: Uuid,
    names: HashMap<String, String>,
    calls: AtomicUsize,
}

#[async_trait]
impl crate::domain::ports::principal_name_reader::PrincipalNameReader for OneFastTenantReader {
    async fn user_names(
        &self,
        _ctx: &SecurityContext,
        tenant_id: Uuid,
        ids: &[String],
    ) -> Result<HashMap<String, String>, PrincipalNameError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if tenant_id != self.fast_tenant {
            // Longer than any deadline a test sets; `timeout_at` drops
            // this future, so the test does not actually wait.
            tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        }
        Ok(ids
            .iter()
            .filter_map(|id| self.names.get(id).map(|n| (id.clone(), n.clone())))
            .collect())
    }
}

/// Limits with a short deadline and a generous tenant budget.
fn limits_with(
    resolve_timeout_ms: u64,
    max_lookup_tenants_per_request: u32,
) -> PrincipalNamesConfig {
    PrincipalNamesConfig {
        max_lookup_tenants_per_request,
        resolve_timeout_ms,
        ..PrincipalNamesConfig::default()
    }
}

/// On the deadline the page is served with the names that resolved before
/// it — not with no names, and not with an error. The fast tenant is
/// visited first because it carries more ids, which is the ordering the
/// hydrator commits to.
#[tokio::test]
async fn the_resolve_deadline_serves_a_partially_named_page() {
    let users = Arc::new(OneFastTenantReader {
        fast_tenant: T1,
        names: HashMap::from([
            ("u1".to_owned(), "Ada".to_owned()),
            ("u2".to_owned(), "Alan".to_owned()),
            ("u3".to_owned(), "Grace".to_owned()),
        ]),
        calls: AtomicUsize::new(0),
    });
    let h = PrincipalNameHydrator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::clone(&users)
            as Arc<dyn crate::domain::ports::principal_name_reader::PrincipalNameReader>,
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(FakeRoleDefinitionRepo::default()),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .with_limits(&limits_with(60, 8));

    let out = h
        .hydrate(
            &ctx(),
            vec![user_row("u1", T1), user_row("u2", T1), user_row("u3", T2)],
            Some(all_roles()),
        )
        .await;

    assert_eq!(out.len(), 3, "every row is served");
    assert_eq!(
        names(&out),
        vec![Some("Ada"), Some("Alan"), None],
        "the tenant that answered before the deadline keeps its names"
    );
    assert_eq!(
        users.calls.load(Ordering::SeqCst),
        2,
        "the stalled tenant is attempted once, then the loop stops"
    );
}

/// The local name resolves even when an upstream eats the whole budget.
///
/// Role names come from this gear's own table, which is the reason the
/// module claims they stay resolvable when every upstream is down. That
/// claim only holds if they are resolved *first*: every phase shares one
/// deadline, so a phase that runs after a stalled Keycloak inherits an
/// expired budget and returns nothing.
///
/// The role repository yields before answering, which is what makes this
/// test able to fail: `timeout_at` polls its future once before it checks
/// the deadline, so a fake that answers on the first poll would succeed on
/// an expired budget and the ordering regression would pass unnoticed.
#[tokio::test]
async fn a_stalled_user_reader_does_not_cost_the_page_its_role_names() {
    let role_id = Uuid::now_v7();
    let users = Arc::new(OneFastTenantReader {
        fast_tenant: T1,
        names: HashMap::from([("u1".to_owned(), "Ada".to_owned())]),
        calls: AtomicUsize::new(0),
    });
    let roles = Arc::new(
        FakeRoleDefinitionRepo::default()
            .with_role(role_id, "Tenant Administrator")
            .yielding(),
    );
    let h = PrincipalNameHydrator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::clone(&users)
            as Arc<dyn crate::domain::ports::principal_name_reader::PrincipalNameReader>,
        Arc::new(FakeRbacRgRead::default()),
        Arc::clone(&roles),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .with_limits(&limits_with(60, 8));
    let mut stalled = user_row("u3", T2);
    stalled.role_definition_id = role_id;
    let mut served = user_row("u1", T1);
    served.role_definition_id = role_id;

    let out = h
        .hydrate(&ctx(), vec![served, stalled], Some(all_roles()))
        .await;

    assert_eq!(out.len(), 2, "every row is served");
    assert_eq!(
        out.iter()
            .map(|row| row.role_definition_name.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("Tenant Administrator"), Some("Tenant Administrator")],
        "the local role read runs before the upstream phases, so a stalled \
         user reader cannot take its budget"
    );
    assert_eq!(
        names(&out),
        vec![Some("Ada"), None],
        "the stalled tenant still loses only its own principal name"
    );
}

/// The fan-out budget caps how many tenants one request may visit, so the
/// per-tenant budgets cannot multiply without limit. The tenants that fit
/// are the ones naming the most rows.
#[tokio::test]
async fn the_tenant_fanout_budget_caps_upstream_calls() {
    let tenants: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
    let mut seeded = FakePrincipalNameReader::default();
    let mut rows = Vec::new();
    for (i, tenant) in tenants.iter().enumerate() {
        // Tenant 0 carries three principals, tenant 1 two, the rest one:
        // the budget must spend itself on the two busiest.
        let count = match i {
            0 => 3,
            1 => 2,
            _ => 1,
        };
        for n in 0..count {
            let id = format!("t{i}-u{n}");
            seeded = seeded.with_name(*tenant, &id, &format!("Name {i}-{n}"));
            rows.push(user_row(&id, *tenant));
        }
    }
    let users = Arc::new(seeded);
    let h = PrincipalNameHydrator::new(
        crate::domain::model::scope_fakes::stub_db_provider().await,
        Arc::clone(&users)
            as Arc<dyn crate::domain::ports::principal_name_reader::PrincipalNameReader>,
        Arc::new(FakeRbacRgRead::default()),
        Arc::new(FakeRoleDefinitionRepo::default()),
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(RecordingMetrics::default()),
    )
    .with_limits(&limits_with(60_000, 2));

    let out = h.hydrate(&ctx(), rows, Some(all_roles())).await;

    assert_eq!(out.len(), 8, "every row is served");
    assert_eq!(
        users.call_count(),
        2,
        "five tenants on the page, two visited - the budget, not the page, decides"
    );
    let named = out.iter().filter(|r| r.principal_name.is_some()).count();
    assert_eq!(
        named, 5,
        "the two busiest tenants (3 + 2 principals) are the ones resolved"
    );
}
