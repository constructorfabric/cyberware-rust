//! Scope-validator test fakes for integration tests.
//!
//! Parallel implementation of `src/domain/model/scope_fakes.rs`. The two copies
//! are intentionally duplicated; do not deduplicate.
//! `FakeRbacRgRead` is defined here because `rbac::domain::rg_port::RbacRgRead`
//! must be reachable from this integration-test binary.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rbac::domain::rg_port::RbacRgRead;
use toolkit_odata::{ODataQuery, Page, PageInfo};

use async_trait::async_trait;
use tenant_resolver_sdk::TenantResolverClient;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{
    BarrierMode, GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions,
    GetDescendantsResponse, GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef,
    TenantStatus,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// HashMap-backed `dyn TenantResolverClient` for scope-validator integration
/// tests. See `src/domain/model/scope_fakes.rs` for the unit-test version.
pub struct FakeTenantResolverClient {
    parents: HashMap<Uuid, Option<Uuid>>,
    barrier_tenants: HashSet<Uuid>,
    pub get_tenant_calls: Arc<AtomicUsize>,
    pub get_ancestors_calls: Arc<AtomicUsize>,
    pub is_ancestor_calls: Arc<AtomicUsize>,
}

impl FakeTenantResolverClient {
    /// Seed a linear chain `[root, T1, T2, …]`.
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

    /// Seed multiple branches sharing `branches[i][0]` as the common root.
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

    /// Mark `barrier_tenants` as `self_managed = true`.
    pub(crate) fn with_self_managed(mut self, barrier_tenants: &[Uuid]) -> Self {
        self.barrier_tenants = barrier_tenants.iter().copied().collect();
        self
    }

    fn from_parts(parents: HashMap<Uuid, Option<Uuid>>, barrier_tenants: HashSet<Uuid>) -> Self {
        Self {
            parents,
            barrier_tenants,
            get_tenant_calls: Arc::new(AtomicUsize::new(0)),
            get_ancestors_calls: Arc::new(AtomicUsize::new(0)),
            is_ancestor_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Sum of all three call counters.
    pub(crate) fn total_calls(&self) -> usize {
        self.get_tenant_calls.load(Ordering::SeqCst)
            + self.get_ancestors_calls.load(Ordering::SeqCst)
            + self.is_ancestor_calls.load(Ordering::SeqCst)
    }

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
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.get_tenant_calls.fetch_add(1, Ordering::SeqCst);
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
        unimplemented!("FakeTenantResolverClient: get_root_tenant not called by ScopeValidator")
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        unimplemented!("FakeTenantResolverClient: get_tenants not called by ScopeValidator")
    }

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
        let mut ancestors: Vec<TenantRef> = Vec::new();
        let mut current = id.0;
        while let Some(parent_id) = self.parents.get(&current).and_then(|p| *p) {
            let parent_is_barrier = self.barrier_tenants.contains(&parent_id);
            ancestors.push(self.make_ref(parent_id));
            if options.barrier_mode == BarrierMode::Respect && parent_is_barrier {
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
        unimplemented!("FakeTenantResolverClient: get_descendants not called by ScopeValidator")
    }

    /// An endpoint this fake has never been seeded with is `TenantNotFound`,
    /// NOT `Ok(false)` — mirrors the plugin AM ships, which probes both
    /// endpoints and errors when either is absent. The distinction matters:
    /// `assignable_scopes` may name a tenant that has since been deleted.
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

/// HashMap-backed `dyn RbacRgRead` for scope-validator integration tests.
/// Mirrors `src/domain/scope_fakes::FakeRbacRgRead` — duplication is
/// intentional.
pub struct FakeRbacRgRead {
    /// Map from resource-group UUID to its owner-tenant UUID.
    groups: HashMap<Uuid, Uuid>,
    /// Number of times `get_group` was called.
    pub get_group_calls: Arc<AtomicUsize>,
    /// Number of times `list_memberships` was called. Evaluator tests assert
    /// on this counter to verify the membership lookup short-circuits
    /// (`include_group_roles = false`, non-User principal).
    pub list_memberships_calls: Arc<AtomicUsize>,
    /// Scripted pages returned by `list_memberships` in call order; the
    /// last element returns `next_cursor = None`. An empty vector returns
    /// one empty page on every call.
    membership_pages: Arc<Mutex<Vec<Vec<Uuid>>>>,
}

impl Default for FakeRbacRgRead {
    fn default() -> Self {
        Self {
            groups: HashMap::new(),
            get_group_calls: Arc::new(AtomicUsize::new(0)),
            list_memberships_calls: Arc::new(AtomicUsize::new(0)),
            membership_pages: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeRbacRgRead {
    /// Seed one (`group_id` → `owner_tenant_id`) entry; builder-chained.
    pub(crate) fn with_group(mut self, group_id: Uuid, owner_tenant_id: Uuid) -> Self {
        self.groups.insert(group_id, owner_tenant_id);
        self
    }

    /// Configure scripted membership pages for `list_memberships`.
    pub(crate) fn with_membership_pages(self, pages: Vec<Vec<Uuid>>) -> Self {
        if let Ok(mut guard) = self.membership_pages.lock() {
            *guard = pages;
        }
        self
    }
}

#[async_trait::async_trait]
impl RbacRgRead for FakeRbacRgRead {
    async fn get_group(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<rbac::domain::rg_port::RbacRgGroup, rbac::domain::rg_port::RbacRgReadError> {
        self.get_group_calls.fetch_add(1, Ordering::SeqCst);
        match self.groups.get(&id).copied() {
            Some(tenant_id) => Ok(rbac::domain::rg_port::RbacRgGroup {
                id,
                tenant_id,
                // Integration tests here assert scope validation, not
                // display names; a seeded group has no name to render.
                name: String::new(),
            }),
            None => Err(rbac::domain::rg_port::RbacRgReadError::NotFound),
        }
    }

    /// No seeded names — display-name resolution is not what these
    /// integration tests exercise, and an absent name is a legal answer.
    async fn group_names(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, rbac::domain::rg_port::RbacRgReadError>
    {
        Ok(std::collections::HashMap::new())
    }

    /// Returns the next scripted page. The page index is carried in the
    /// cursor's `k[0]` slot (matching the production wire shape — the
    /// DB-side paginator emits one `k` entry per order field, and the
    /// RBAC evaluator round-trips the token via `CursorV1::decode`). If
    /// the cursor is absent the fake returns the page indexed by
    /// `call_count`. An empty page-script returns one empty page on
    /// every call.
    async fn list_memberships(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<rbac::domain::rg_port::RbacRgMembership>, rbac::domain::rg_port::RbacRgReadError>
    {
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
        let items: Vec<rbac::domain::rg_port::RbacRgMembership> = page
            .iter()
            .map(|gid| rbac::domain::rg_port::RbacRgMembership { group_id: *gid })
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
