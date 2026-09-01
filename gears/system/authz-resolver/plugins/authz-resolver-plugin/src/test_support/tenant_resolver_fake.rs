//! Scriptable in-memory `TenantResolverClient` fake.
//!
//! Mirrors the RBAC fake's shape: `Default` is loud-stub, `with_*`
//! constructors configure the script, accessors expose call counts and
//! the last-captured request for assertions.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tenant_resolver_sdk::api::TenantResolverClient;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{
    BarrierMode, GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions,
    GetDescendantsResponse, GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef,
};
use toolkit_security::SecurityContext;

/// Configuration the fake follows for `get_tenant` / `get_descendants` calls.
#[derive(Debug, Clone, Default)]
struct Script {
    /// Tenants the fake knows about, keyed by id.
    tenants: HashMap<TenantId, TenantInfo>,
    /// Per-parent descendant lists for `get_descendants`.
    descendants: HashMap<TenantId, Vec<TenantRef>>,
    /// When `Some(message)`, every method returns
    /// `TenantResolverError::Internal(message.clone())`.
    error: Option<String>,
}

/// Captured `get_descendants` invocation — used by tests that need to
/// assert the resolved options the plugin sent (default `tenant_status`,
/// `barrier_mode`, etc.).
#[derive(Debug, Clone)]
pub struct CapturedGetDescendantsRequest {
    pub id: TenantId,
    pub options: GetDescendantsOptions,
}

pub struct InMemoryTenantResolverClient {
    script: Mutex<Script>,
    call_count: AtomicUsize,
    root_call_count: AtomicUsize,
    last_get_descendants: Mutex<Option<CapturedGetDescendantsRequest>>,
}

impl Default for InMemoryTenantResolverClient {
    fn default() -> Self {
        Self {
            script: Mutex::new(Script::default()),
            call_count: AtomicUsize::new(0),
            root_call_count: AtomicUsize::new(0),
            last_get_descendants: Mutex::new(None),
        }
    }
}

impl InMemoryTenantResolverClient {
    /// Pre-populate the fake with tenants. `get_tenant` matches by id;
    /// `get_root_tenant` returns the tenant whose `parent_id` is `None`;
    /// `get_tenants` filters by the supplied id list.
    #[must_use]
    pub fn with_tenants(tenants: Vec<TenantInfo>) -> Self {
        let fake = Self::default();
        fake.add_tenants(tenants);
        fake
    }

    /// Layer additional tenants onto an existing fake.
    pub fn add_tenants(&self, tenants: Vec<TenantInfo>) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for t in tenants {
            script.tenants.insert(t.id, t);
        }
    }

    /// Configure `get_descendants(parent_id, ...)` to return the supplied
    /// descendant list (wrapped in `GetDescendantsResponse { tenant, descendants }`
    /// with `tenant` derived from the configured tenants map).
    #[must_use]
    pub fn with_descendants(parent_id: TenantId, descendants: Vec<TenantRef>) -> Self {
        let fake = Self::default();
        fake.add_descendants(parent_id, descendants);
        fake
    }

    /// Layer additional descendant scripts onto an existing fake.
    pub fn add_descendants(&self, parent_id: TenantId, descendants: Vec<TenantRef>) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.descendants.insert(parent_id, descendants);
    }

    /// Configure the fake to always fail with `TenantResolverError::Internal(message)`.
    /// Used by recovery / error-path tests.
    #[must_use]
    pub fn with_error(message: impl Into<String>) -> Self {
        let fake = Self::default();
        fake.set_error(message);
        fake
    }

    /// Switch the fake into / out of error mode at runtime. `Some(msg)`
    /// makes every method fail; `None` clears the error.
    pub fn set_error(&self, message: impl Into<String>) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.error = Some(message.into());
    }

    /// Clear the error mode set by `with_error` / `set_error`.
    pub fn clear_error(&self) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.error = None;
    }

    /// Total calls across every implemented trait method.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Number of platform-root lookups. Scoped-provenance tests assert this
    /// stays zero so a tenant or resource-group assignment cannot silently
    /// enter Global materialization and later be narrowed by coincidence.
    #[must_use]
    pub fn root_call_count(&self) -> usize {
        self.root_call_count.load(Ordering::SeqCst)
    }

    /// Last `get_descendants` invocation the fake observed (cloned out of
    /// the mutex). Tests assert on the resolved `GetDescendantsOptions`
    /// (default `tenant_status = [Active]`, `barrier_mode`, etc.).
    #[must_use]
    pub fn last_get_descendants_request(&self) -> Option<CapturedGetDescendantsRequest> {
        match self.last_get_descendants.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn maybe_error(&self) -> Option<TenantResolverError> {
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script
            .error
            .as_ref()
            .map(|m| TenantResolverError::Internal(m.clone()))
    }
}

#[async_trait]
impl TenantResolverClient for InMemoryTenantResolverClient {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script
            .tenants
            .get(&id)
            .cloned()
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.root_call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script
            .tenants
            .values()
            .find(|t| t.parent_id.is_none())
            .cloned()
            .ok_or_else(|| {
                TenantResolverError::Internal(
                    "no root tenant configured in InMemoryTenantResolverClient".to_owned(),
                )
            })
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        Ok(ids
            .iter()
            .filter_map(|id| script.tenants.get(id).cloned())
            .collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let tenant = script
            .tenants
            .get(&id)
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })?;
        let to_ref = |tenant: &TenantInfo| TenantRef {
            id: tenant.id,
            status: tenant.status,
            tenant_type: tenant.tenant_type.clone(),
            parent_id: tenant.parent_id,
            self_managed: tenant.self_managed,
        };

        // The SDK contract orders ancestors from direct parent to root. Cap
        // traversal at the configured tenant count so malformed cyclic test
        // data fails loudly instead of hanging the test process.
        let mut ancestors = Vec::new();
        let mut parent_id = tenant.parent_id;
        while let Some(current_id) = parent_id {
            if ancestors.len() >= script.tenants.len() {
                return Err(TenantResolverError::Internal(
                    "cycle in InMemoryTenantResolverClient tenant hierarchy".to_owned(),
                ));
            }
            let parent =
                script
                    .tenants
                    .get(&current_id)
                    .ok_or(TenantResolverError::TenantNotFound {
                        tenant_id: current_id,
                    })?;
            ancestors.push(to_ref(parent));
            parent_id = parent.parent_id;
        }

        Ok(GetAncestorsResponse {
            tenant: to_ref(tenant),
            ancestors,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        match self.last_get_descendants.lock() {
            Ok(mut g) => {
                *g = Some(CapturedGetDescendantsRequest {
                    id,
                    options: options.clone(),
                });
            }
            Err(p) => {
                *p.into_inner() = Some(CapturedGetDescendantsRequest {
                    id,
                    options: options.clone(),
                });
            }
        }
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let tenant_info = script
            .tenants
            .get(&id)
            .cloned()
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })?;
        let all = script.descendants.get(&id).cloned().unwrap_or_default();

        // Model the resolver's documented filtering so integration tests
        // exercise real behavior instead of pre-baked lists:
        //  - `status`: keep only descendants whose status is requested
        //    (empty list = all statuses);
        //  - `barrier_mode = Respect`: stop at self-managed boundaries — a
        //    self-managed tenant and everything beneath it is excluded;
        //    `Ignore` traverses through them.
        let by_id: HashMap<TenantId, TenantRef> = all.iter().map(|t| (t.id, t.clone())).collect();
        let respect = matches!(options.barrier_mode, BarrierMode::Respect);
        let descendants: Vec<TenantRef> = all
            .iter()
            .filter(|d| options.status.is_empty() || options.status.contains(&d.status))
            .filter(|d| !respect || !behind_barrier(d, &by_id, id))
            .cloned()
            .collect();

        Ok(GetDescendantsResponse {
            tenant: TenantRef::from(&tenant_info),
            descendants,
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor_id: TenantId,
        _descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        unreachable!(
            "InMemoryTenantResolverClient::is_ancestor - \
             not used by foundation/scope/policy/hierarchy stories; extend the fake when needed"
        )
    }
}

/// `true` if `d` is at or beyond a self-managed barrier relative to the
/// subtree root: either `d` is itself self-managed, or one of its ancestors
/// within the descendant set is. Used to model `barrier_mode = Respect`.
fn behind_barrier(d: &TenantRef, by_id: &HashMap<TenantId, TenantRef>, root: TenantId) -> bool {
    if d.self_managed {
        return true;
    }
    let mut cursor = d.parent_id;
    while let Some(parent_id) = cursor {
        if parent_id == root {
            return false; // reached the subtree root without crossing a barrier
        }
        match by_id.get(&parent_id) {
            Some(parent) if parent.self_managed => return true,
            Some(parent) => cursor = parent.parent_id,
            None => return false, // parent outside the set (e.g. the root) — no barrier
        }
    }
    false
}
