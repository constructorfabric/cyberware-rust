//! Test fakes for scope-validator unit tests.
//!
//! Both fakes are `pub(crate)` and gated `#[cfg(test)]` because they
//! depend on `pub(crate)` types invisible to integration tests.
//! `FakeTenantResolverClient` is duplicated in
//! `tests/common/scope_fakes.rs` (implementing only `pub` traits) so
//! integration tests can use it without a `pub(crate)` bypass — the
//! duplication is intentional.
//!
//! Methods `ScopeValidator` never calls are `unimplemented!()` so a
//! future accidental call panics immediately rather than silently
//! succeeding with wrong data.

// All items are test-only — suppress "never used" warnings centrally.
#![allow(unknown_lints, de0309_must_have_domain_model)]
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tenant_resolver_sdk::TenantResolverClient;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{
    BarrierMode, GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions,
    GetDescendantsResponse, GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef,
    TenantStatus,
};
use toolkit_odata::{ODataQuery, Page, PageInfo};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::rg_port::RbacRgRead;

// ---------------------------------------------------------------------------
// FakeTenantResolverClient
// ---------------------------------------------------------------------------

/// HashMap-backed `dyn TenantResolverClient` for scope-validator unit
/// tests. The backing maps are immutable after construction so the fake
/// is `Send + Sync` without any mutex; counters use `AtomicUsize`.
// Test fake — not a domain entity. The `domain/` location is forced by
// the `pub(crate)` trait it implements.
pub struct FakeTenantResolverClient {
    /// Map from tenant UUID to its parent UUID (`None` = root of the tree).
    parents: HashMap<Uuid, Option<Uuid>>,
    /// Platform root tenant, when a test seeds one. `None` keeps
    /// `get_root_tenant` at `unimplemented!()` so an unexpected call
    /// still fails loudly for the validator tests that must never make
    /// it; the display-name hydrator does call it, for root-scoped rows.
    root_tenant: Option<Uuid>,
    /// Tenants with `self_managed = true`. When `BarrierMode::Respect`
    /// is passed in, the fake honours real-client barrier semantics so
    /// the validator's `BarrierMode::Ignore` choice is observable.
    barrier_tenants: HashSet<Uuid>,
    /// One-shot override: if set, the next `get_tenant` call returns
    /// this error instead of the normal happy / not-found path.
    fail_get_tenant_with: Mutex<Option<TenantResolverError>>,
    /// `subject_id` from the last `SecurityContext` passed to any
    /// method — tests use this to verify the context is forwarded
    /// verbatim.
    pub(crate) last_ctx_subject_id: Arc<Mutex<Option<Uuid>>>,
    /// Number of times `get_tenant` was called.
    pub(crate) get_tenant_calls: Arc<AtomicUsize>,
    /// Number of times `get_ancestors` was called.
    pub(crate) get_ancestors_calls: Arc<AtomicUsize>,
    /// Number of times `is_ancestor` was called.
    pub(crate) is_ancestor_calls: Arc<AtomicUsize>,
}

impl FakeTenantResolverClient {
    /// Seed a single linear chain `[root, T1, T2, …]` where `chain[i]`'s
    /// parent is `chain[i-1]` and `chain[0]` has no parent.
    pub(crate) fn with_chain(chain: &[Uuid]) -> Self {
        let mut parents = HashMap::with_capacity(chain.len());
        for (i, &id) in chain.iter().enumerate() {
            let parent = if i == 0 { None } else { Some(chain[i - 1]) };
            // `or_insert`, not `insert`: a uuid repeated in `chain` would
            // otherwise end up its own parent, and the ancestor walk would
            // never terminate. Callers do repeat ids — several helpers build
            // the chain from every seeded row's tenant.
            parents.entry(id).or_insert(parent);
        }
        Self::from_parts(parents, HashSet::new())
    }

    /// Seed multiple branches that share a common root UUID at
    /// `branches[i][0]`. First assignment wins so shared roots keep
    /// their `None` parent.
    pub(crate) fn with_disjoint_subtrees(branches: &[&[Uuid]]) -> Self {
        let total: usize = branches.iter().map(|b| b.len()).sum();
        let mut parents: HashMap<Uuid, Option<Uuid>> = HashMap::with_capacity(total);
        for branch in branches {
            for (i, &id) in branch.iter().enumerate() {
                let parent = if i == 0 { None } else { Some(branch[i - 1]) };
                parents.entry(id).or_insert(parent);
            }
        }
        Self::from_parts(parents, HashSet::new())
    }

    /// Mark `barrier_tenants` as `self_managed = true`. The fake's
    /// `get_ancestors` and `is_ancestor` honour `BarrierMode`, so the
    /// validator's `BarrierMode::Ignore` choice is observable to tests.
    pub(crate) fn with_self_managed(mut self, barrier_tenants: &[Uuid]) -> Self {
        self.barrier_tenants = barrier_tenants.iter().copied().collect();
        self
    }

    /// Arm the fake so its next `get_tenant` returns `err` — used to
    /// verify a non-`TenantNotFound` upstream error propagates as
    /// `ScopeError::Upstream`, not `ScopeNotFound`.
    pub(crate) fn with_tenant_failure(self, err: TenantResolverError) -> Self {
        *self.fail_get_tenant_with.lock().expect("lock poisoned") = Some(err);
        self
    }

    /// Seed the platform root tenant returned by `get_root_tenant`.
    #[allow(dead_code)]
    pub(crate) fn with_root_tenant(mut self, root: Uuid) -> Self {
        self.root_tenant = Some(root);
        self
    }

    fn from_parts(parents: HashMap<Uuid, Option<Uuid>>, barrier_tenants: HashSet<Uuid>) -> Self {
        Self {
            parents,
            root_tenant: None,
            barrier_tenants,
            fail_get_tenant_with: Mutex::new(None),
            last_ctx_subject_id: Arc::new(Mutex::new(None)),
            get_tenant_calls: Arc::new(AtomicUsize::new(0)),
            get_ancestors_calls: Arc::new(AtomicUsize::new(0)),
            is_ancestor_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Sum of all three tracked call counters.
    pub(crate) fn total_calls(&self) -> usize {
        self.get_tenant_calls.load(Ordering::SeqCst)
            + self.get_ancestors_calls.load(Ordering::SeqCst)
            + self.is_ancestor_calls.load(Ordering::SeqCst)
    }

    /// Construct a synthetic `TenantInfo` for a known tenant UUID.
    fn make_info(&self, id: Uuid) -> TenantInfo {
        TenantInfo {
            id: TenantId(id),
            name: format!("fake-tenant-{id}"),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: self.parents.get(&id).and_then(|p| *p).map(TenantId),
            self_managed: self.barrier_tenants.contains(&id),
        }
    }

    /// Construct a synthetic `TenantRef` for a known tenant UUID.
    fn make_ref(&self, id: Uuid) -> TenantRef {
        TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: self.parents.get(&id).and_then(|p| *p).map(TenantId),
            self_managed: self.barrier_tenants.contains(&id),
        }
    }
}

#[async_trait]
impl TenantResolverClient for FakeTenantResolverClient {
    async fn get_tenant(
        &self,
        ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.get_tenant_calls.fetch_add(1, Ordering::SeqCst);
        *self.last_ctx_subject_id.lock().expect("lock poisoned") = Some(ctx.subject_id());
        if let Some(err) = self
            .fail_get_tenant_with
            .lock()
            .expect("lock poisoned")
            .take()
        {
            return Err(err);
        }
        if self.parents.contains_key(&id.0) {
            Ok(self.make_info(id.0))
        } else {
            Err(TenantResolverError::TenantNotFound { tenant_id: id })
        }
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        match self.root_tenant {
            Some(root) => Ok(self.make_info(root)),
            // `ScopeValidator` never calls this; an unexpected call in a
            // validator test is a wiring bug, not a data condition.
            None => unimplemented!(
                "FakeTenantResolverClient: get_root_tenant called without \
                 with_root_tenant(...) - seed a root tenant first"
            ),
        }
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        unimplemented!("FakeTenantResolverClient: get_tenants is not called by ScopeValidator")
    }

    /// Ancestor chain from direct parent to root, honouring
    /// `BarrierMode`. `Respect`: starting tenant `self_managed` →
    /// empty; chain tenant `self_managed` → include and stop.
    /// `Ignore`: walk to tree root.
    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        self.get_ancestors_calls.fetch_add(1, Ordering::SeqCst);
        if !self.parents.contains_key(&id.0) {
            return Err(TenantResolverError::TenantNotFound { tenant_id: id });
        }
        let tenant_ref = self.make_ref(id.0);

        if options.barrier_mode == BarrierMode::Respect && self.barrier_tenants.contains(&id.0) {
            return Ok(GetAncestorsResponse {
                tenant: tenant_ref,
                ancestors: vec![],
            });
        }

        // Walk the parent chain in parent-to-root order.
        let mut ancestors: Vec<TenantRef> = Vec::new();
        let mut current = id.0;
        while let Some(parent_id) = self.parents.get(&current).and_then(|p| *p) {
            let parent_is_barrier = self.barrier_tenants.contains(&parent_id);
            ancestors.push(self.make_ref(parent_id));
            if options.barrier_mode == BarrierMode::Respect && parent_is_barrier {
                // Include the barrier tenant but stop above it.
                break;
            }
            current = parent_id;
        }

        Ok(GetAncestorsResponse {
            tenant: tenant_ref,
            ancestors,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        unimplemented!("FakeTenantResolverClient: get_descendants is not called by ScopeValidator")
    }

    /// Whether `ancestor_id` is in the parent chain of `descendant_id`.
    /// Returns `false` for `ancestor_id == descendant_id` (matching
    /// upstream); `ScopeValidator::is_ancestor` short-circuits the self
    /// case before delegating, so the divergence is never exposed.
    ///
    /// An endpoint this fake has never been seeded with is `TenantNotFound`,
    /// NOT `Ok(false)`. That is the real contract — the plugin AM ships
    /// probes both endpoints and errors when either is absent — and the
    /// difference is load-bearing: `assignable_scopes` may still name a
    /// tenant that has since been deleted, and the code under test has to
    /// distinguish "no such tenant" from "not an ancestor".
    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor_id: TenantId,
        descendant_id: TenantId,
        options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        self.is_ancestor_calls.fetch_add(1, Ordering::SeqCst);

        for endpoint in [ancestor_id, descendant_id] {
            if !self.parents.contains_key(&endpoint.0) {
                return Err(TenantResolverError::TenantNotFound {
                    tenant_id: endpoint,
                });
            }
        }

        if options.barrier_mode == BarrierMode::Respect
            && self.barrier_tenants.contains(&descendant_id.0)
        {
            return Ok(false);
        }

        // Walk from descendant toward root.
        let mut current = descendant_id.0;
        while let Some(parent_id) = self.parents.get(&current).and_then(|p| *p) {
            if parent_id == ancestor_id.0 {
                return Ok(true);
            }
            if options.barrier_mode == BarrierMode::Respect
                && self.barrier_tenants.contains(&parent_id)
            {
                return Ok(false);
            }
            current = parent_id;
        }
        Ok(false)
    }
}

/// HashMap-backed `dyn RbacRgRead` for scope-validator unit tests.
/// Seeded via [`FakeRbacRgRead::with_group`]; `list_memberships`
/// returns a configurable script of pages.
// Test fake — not a domain entity.
pub struct FakeRbacRgRead {
    /// Map from resource-group UUID to its owner-tenant UUID.
    groups: HashMap<Uuid, Uuid>,
    /// Map from resource-group UUID to its display name. Separate from
    /// `groups` so a test can seed a group that exists but has no name
    /// seeded (the shape a display-name test needs to distinguish
    /// "unknown group" from "group without a resolvable name").
    group_names: HashMap<Uuid, String>,
    /// Number of times `get_group` was invoked.
    pub(crate) get_group_calls: Arc<AtomicUsize>,
    /// When set, `get_group` fails with `RbacRgReadError::Upstream`
    /// carrying this message instead of consulting `groups`.
    ///
    /// Exists because a `NotFound`-only fake left the
    /// `Upstream -> ServiceUnavailable` mapping in
    /// `validate_group_principal` unreachable: an outage in the
    /// resource-group gear would have been indistinguishable, to a test,
    /// from "that group does not exist" — which is a 404 the caller can act
    /// on rather than a 503 they should retry.
    group_upstream_failure: Option<String>,
    /// Number of times `group_names` was invoked — one call per page,
    /// never one per row.
    pub(crate) group_names_calls: Arc<AtomicUsize>,
    /// Number of times `list_memberships` was invoked.
    pub(crate) list_memberships_calls: Arc<AtomicUsize>,
    /// Scripted pages returned by `list_memberships` in order. The
    /// last page has `next_cursor = None`; an empty vector returns one
    /// empty page on every call.
    membership_pages: Arc<Mutex<Vec<Vec<Uuid>>>>,
}

impl Default for FakeRbacRgRead {
    fn default() -> Self {
        Self {
            groups: HashMap::new(),
            group_names: HashMap::new(),
            get_group_calls: Arc::new(AtomicUsize::new(0)),
            group_upstream_failure: None,
            group_names_calls: Arc::new(AtomicUsize::new(0)),
            list_memberships_calls: Arc::new(AtomicUsize::new(0)),
            membership_pages: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeRbacRgRead {
    /// Seed one (`group_id` → `owner_tenant_id`) entry; chainable.
    /// Make `get_group` fail with `RbacRgReadError::Upstream(msg)`.
    #[allow(dead_code)]
    pub(crate) fn with_group_upstream_failure(mut self, msg: impl Into<String>) -> Self {
        self.group_upstream_failure = Some(msg.into());
        self
    }

    pub(crate) fn with_group(mut self, group_id: Uuid, owner_tenant_id: Uuid) -> Self {
        self.groups.insert(group_id, owner_tenant_id);
        self
    }

    /// Seed one (`group_id` → display name) entry; chainable. Implies
    /// nothing about `get_group` existence — seed both when a test needs
    /// a group that both exists and has a name.
    #[allow(dead_code)]
    pub(crate) fn with_group_name(mut self, group_id: Uuid, name: &str) -> Self {
        self.group_names.insert(group_id, name.to_owned());
        self
    }

    /// Configure the scripted membership pages for `list_memberships`.
    // The integration consumers live in `tests/postgres_permission_evaluator.rs`
    // and use their own copy in `tests/common/scope_fakes.rs`; this one stays so
    // adjacent unit tests can script membership pages.
    #[allow(dead_code)]
    pub(crate) fn with_membership_pages(self, pages: Vec<Vec<Uuid>>) -> Self {
        if let Ok(mut guard) = self.membership_pages.lock() {
            *guard = pages;
        }
        self
    }
}

#[async_trait]
impl RbacRgRead for FakeRbacRgRead {
    /// Returns the RBAC-owned projection of the seeded group; only the
    /// `tenant_id` field is meaningful for validator tests. Constructing
    /// the RBAC-owned [`RbacRgGroup`] directly (rather than synthesising
    /// the upstream `ResourceGroup`) keeps the test surface aligned
    /// with the port-isolation invariant.
    async fn get_group(
        &self,
        _ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<crate::domain::rg_port::RbacRgGroup, crate::domain::rg_port::RbacRgReadError> {
        self.get_group_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(msg) = self.group_upstream_failure.as_ref() {
            return Err(crate::domain::rg_port::RbacRgReadError::Upstream(
                msg.clone().into(),
            ));
        }
        match self.groups.get(&id).copied() {
            Some(tenant_id) => Ok(crate::domain::rg_port::RbacRgGroup {
                id,
                tenant_id,
                name: self.group_names.get(&id).cloned().unwrap_or_default(),
            }),
            None => Err(crate::domain::rg_port::RbacRgReadError::NotFound),
        }
    }

    /// Answers from the seeded name table in one call, mirroring the
    /// production adapter's batched `id in (...)` listing. Ids with no
    /// seeded name are absent from the map — never an error.
    async fn group_names(
        &self,
        _ctx: &SecurityContext,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, crate::domain::rg_port::RbacRgReadError> {
        self.group_names_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ids
            .iter()
            .filter_map(|id| self.group_names.get(id).map(|n| (*id, n.clone())))
            .collect())
    }

    /// Returns the next scripted page. The page index is carried in the
    /// cursor's `k[0]` slot (matching the production wire shape — the
    /// DB-side paginator emits one `k` entry per order field, and the
    /// RBAC evaluator round-trips the token via `CursorV1::decode`).
    /// Absent cursor uses the per-call counter so successive calls
    /// advance through the script.
    async fn list_memberships(
        &self,
        _ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<
        Page<crate::domain::rg_port::RbacRgMembership>,
        crate::domain::rg_port::RbacRgReadError,
    > {
        let call_count = self.list_memberships_calls.fetch_add(1, Ordering::SeqCst);
        let pages = self
            .membership_pages
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let empty_page = || Page {
            items: Vec::new(),
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 100,
            },
        };

        if pages.is_empty() {
            return Ok(empty_page());
        }

        let index = query
            .cursor
            .as_ref()
            .and_then(|c| c.k.first())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(call_count);

        let page = pages.get(index).cloned().unwrap_or_default();
        let items: Vec<crate::domain::rg_port::RbacRgMembership> = page
            .iter()
            .map(|gid| crate::domain::rg_port::RbacRgMembership { group_id: *gid })
            .collect();
        let next_cursor = if index + 1 < pages.len() {
            let cursor = toolkit_odata::CursorV1 {
                k: vec![(index + 1).to_string()],
                o: toolkit_odata::SortDir::Asc,
                s: "+group_id".to_owned(),
                f: None,
                d: "fwd".to_owned(),
            };
            Some(cursor.encode().expect("FakeRbacRgRead: CursorV1::encode"))
        } else {
            None
        };
        Ok(Page {
            items,
            page_info: PageInfo {
                next_cursor,
                prev_cursor: None,
                limit: 100,
            },
        })
    }
}

/// A `DBProvider` over an empty in-memory `SQLite` database, with no
/// migrations applied.
///
/// Exists because the services now take their connection source as a
/// constructor argument: a unit test that stubs every repository still has
/// to hand them *something*. The stubs ignore the executor, so this
/// provider is never queried — but `conn()` must succeed, which an
/// unmigrated in-memory database satisfies.
///
/// A test that actually reads or writes rows wants
/// `tests/common::fresh_sqlite_provider` (migrated) or a Postgres fixture
/// instead; this one would fail on the first real statement, which is the
/// intended signal that a stub was expected and a query happened.
///
/// # Panics
///
/// If the in-memory database cannot be opened, which would mean the `SQLite`
/// driver is broken rather than anything about the code under test.
pub async fn stub_db_provider() -> toolkit_db::DBProvider<toolkit_db::DbError> {
    let db = toolkit_db::connect_db("sqlite::memory:", toolkit_db::ConnectOpts::default())
        .await
        .expect("opening an in-memory SQLite database must succeed");
    toolkit_db::DBProvider::new(db)
}
