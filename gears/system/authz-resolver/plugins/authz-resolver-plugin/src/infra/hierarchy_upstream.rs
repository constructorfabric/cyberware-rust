//! `SdkHierarchyUpstream` — the [`HierarchyUpstream`] adapter over the tenant
//! resolver and resource-group SDK clients.
//!
//! Everything the domain must not know about hierarchy I/O lives here:
//! `toolkit_odata` filter construction, `CursorV1` decoding, page draining with
//! safety caps, `IN`-list chunking, SDK error mapping, and the round-trip
//! timing metric. The domain side sees only plain ids and [`PluginError`] —
//! see `crate::domain::hierarchy_upstream` for why the seam is here.
//!
//! No caching happens in this module. The caller owns the cache, so every
//! method here is an unconditional upstream read; the round-trip metric this
//! module records is therefore a true miss count.
//!
//! ## Downstream identity (why `SecurityContext::anonymous()`)
//!
//! Every resolver call below passes `SecurityContext::anonymous()`. This is
//! deliberate, not a missing-context bug:
//!
//! * **Full-tree visibility is required.** The roots the PDP expands
//!   (`root_tenant_id`, `root_group_ids`) come from the *trusted RBAC
//!   evaluation result*, not from the caller. A subject-scoped context would
//!   hide ancestors/descendants the subject cannot see directly and yield wrong
//!   allow/deny decisions — the materialization is a pure expansion of an
//!   already-authorized root.
//! * **The request carries no trusted tenant identity.** The subject's home
//!   tenant lives only in caller-asserted `subject.properties["tenant_id"]`
//!   (see `audit_emitter`), so building a context from it would feed a
//!   PEP-controlled tenant id into the resolver's access control.
//! * **Anonymous is safe in-process.** Tenant/RG resolver calls go through
//!   `ClientHub` in the same process — a trusted toolkit boundary with no
//!   network in between.
//!
//! This mirrors the Constructor Fabric reference `tr-authz-plugin`.
//!
//! TODO(cyberware-rust#1597): once S2S auth + gRPC/mTLS transport land, replace
//! `anonymous()` with an S2S-issued **service** context identifying this caller
//! as the plugin (NOT the subject) — anonymous is unsafe across a network
//! boundary, where there is no cryptographic identity on the wire.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use authz_resolver_sdk::models::BarrierMode as SdkBarrierMode;
use resource_group_sdk::ResourceGroupError;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
use resource_group_sdk::models::{ResourceGroupMembership, ResourceGroupWithDepth};
use tenant_resolver_sdk::TenantResolverError;
use tenant_resolver_sdk::api::TenantResolverClient;
use tenant_resolver_sdk::models::{
    BarrierMode as TrBarrierMode, GetDescendantsOptions, TenantId, TenantStatus,
};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_odata::ast::{Expr, Value};
use toolkit_odata::{CursorV1, ODataQuery};
use toolkit_security::SecurityContext;
use tracing::warn;
use uuid::Uuid;

use crate::domain::error::PluginError;
use crate::domain::hierarchy_cache::TenantMetadata;
use crate::domain::hierarchy_upstream::{GroupSubtreeFetch, HierarchyUpstream, TenantSubtreeFetch};
use crate::domain::metrics_port::{HierarchyOp, Resolver};
use crate::infra::metrics::AuthZMetrics;

/// [`HierarchyUpstream`] over the real SDK clients.
pub(crate) struct SdkHierarchyUpstream {
    tenant_resolver: Arc<dyn TenantResolverClient>,
    resource_group: Arc<dyn ResourceGroupReadHierarchy>,
    metrics: Arc<AuthZMetrics>,
}

impl SdkHierarchyUpstream {
    pub(crate) fn new(
        tenant_resolver: Arc<dyn TenantResolverClient>,
        resource_group: Arc<dyn ResourceGroupReadHierarchy>,
        metrics: Arc<AuthZMetrics>,
    ) -> Self {
        Self {
            tenant_resolver,
            resource_group,
            metrics,
        }
    }
}

#[async_trait]
impl HierarchyUpstream for SdkHierarchyUpstream {
    async fn root_tenant_id(&self) -> Result<Uuid, PluginError> {
        let ctx = SecurityContext::anonymous();
        self.tenant_resolver
            .get_root_tenant(&ctx)
            .await
            .map(|t| t.id.0)
            .inspect_err(|err| warn!(error = ?err, "tenant resolver call failed"))
            .map_err(|err| map_tenant_resolver_error(&err))
    }

    async fn tenant_subtree(
        &self,
        root_tenant_id: Uuid,
        barrier_mode: SdkBarrierMode,
        descendant_status: Vec<TenantStatus>,
    ) -> Result<TenantSubtreeFetch, PluginError> {
        let ctx = SecurityContext::anonymous();
        let options = GetDescendantsOptions {
            status: descendant_status,
            barrier_mode: project_barrier_mode(barrier_mode),
            max_depth: None,
        };
        // Time the actual resolver round-trip (design section 3.13
        // authz_hierarchy_query_duration_milliseconds{resolver,operation}).
        let started = Instant::now();
        let result = self
            .tenant_resolver
            .get_descendants(&ctx, TenantId(root_tenant_id), &options)
            .await;
        self.metrics.record_hierarchy_query(
            Resolver::Tenant,
            HierarchyOp::SubtreeIds,
            started.elapsed(),
        );
        let response = result
            .inspect_err(|err| warn!(error = ?err, "tenant resolver call failed"))
            .map_err(|err| map_tenant_resolver_error(&err))?;
        // The root is reported with its status and NOT folded into the id
        // list: the `[Active]` root clamp is the caller's decision, and it
        // needs the status to make it.
        Ok(TenantSubtreeFetch {
            root_id: response.tenant.id.0,
            root_status: response.tenant.status,
            descendant_ids: response.descendants.iter().map(|t| t.id.0).collect(),
        })
    }

    async fn tenant_metadata(&self, tenant_id: Uuid) -> Result<TenantMetadata, PluginError> {
        let ctx = SecurityContext::anonymous();
        let info = self
            .tenant_resolver
            .get_tenant(&ctx, TenantId(tenant_id))
            .await
            .inspect_err(|err| warn!(error = ?err, "tenant resolver call failed"))
            .map_err(|err| map_tenant_resolver_error(&err))?;
        Ok(TenantMetadata {
            id: info.id.0,
            status: info.status,
            self_managed: info.self_managed,
            parent_id: info.parent_id.map(|t| t.0),
        })
    }

    async fn group_subtree(
        &self,
        root_group_ids: &[Uuid],
    ) -> Result<GroupSubtreeFetch, PluginError> {
        let ctx = SecurityContext::anonymous();
        // Time the combined RG round-trips (design section 3.13
        // authz_hierarchy_query_duration_milliseconds{resolver=rg,...}).
        let started = Instant::now();

        // 1. Build the full subtree group set (roots + descendants) and collect
        //    every group's owning tenant. `get_group_descendants` is
        //    `depth >= 0`, so `page.items` includes the root group itself —
        //    reading `hierarchy.tenant_id` off the items covers every group in
        //    the subtree, roots included.
        let mut subtree: Vec<Uuid> = root_group_ids.to_vec();
        let mut owner_tenant_ids: Vec<Uuid> = Vec::new();
        for root_id in root_group_ids {
            // Drain ALL pages — reading only the first page silently truncated
            // the subtree (and its owning tenants) for any group with more
            // descendants than one page.
            let descendants =
                drain_group_descendants(self.resource_group.as_ref(), &ctx, *root_id).await?;
            for group in &descendants {
                subtree.push(group.id);
                owner_tenant_ids.push(group.hierarchy.tenant_id);
            }
        }
        subtree.sort();
        subtree.dedup();
        owner_tenant_ids.sort();
        owner_tenant_ids.dedup();

        // 2. Fetch memberships filtered to that subtree (ALL pages), chunking
        //    the `IN (...)` list.
        //
        // `subtree` is bounded by `MAX_PAGINATED_ITEMS` (100_000), and a single
        // `IN` that wide exceeds driver bind-parameter limits and produces a
        // pathological plan — the same hazard the RBAC repositories chunk
        // against at `GROUP_PRINCIPALS_CHUNK` / `ROLE_ID_CHUNK` (both 500).
        // Chunks are disjoint group-id sets, so the per-chunk resource ids merge
        // by union; `parse_resource_ids` already sorts and dedups within a
        // chunk, and the final dedup covers a resource that belongs to groups in
        // two different chunks.
        let mut resource_ids: Vec<Uuid> = Vec::new();
        for chunk in subtree.chunks(GROUP_ID_IN_CHUNK) {
            let query = ODataQuery::new().with_filter(build_group_id_in_filter(chunk));
            let memberships = drain_memberships(self.resource_group.as_ref(), &ctx, query).await?;
            resource_ids.extend(parse_resource_ids(&memberships));
        }
        resource_ids.sort();
        resource_ids.dedup();

        self.metrics.record_hierarchy_query(
            Resolver::Rg,
            HierarchyOp::GroupSubtree,
            started.elapsed(),
        );
        Ok(GroupSubtreeFetch {
            resource_ids,
            owner_tenant_ids,
        })
    }

    async fn group_member_resource_ids(
        &self,
        group_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, PluginError> {
        let ctx = SecurityContext::anonymous();
        let query = ODataQuery::new().with_filter(build_group_id_in_filter(group_ids));
        let memberships = drain_memberships(self.resource_group.as_ref(), &ctx, query).await?;
        Ok(parse_resource_ids(&memberships))
    }

    async fn group_owner_tenant_id(&self, group_id: Uuid) -> Result<Uuid, PluginError> {
        let ctx = SecurityContext::anonymous();
        let group = self
            .resource_group
            .get_group(&ctx, group_id)
            .await
            .inspect_err(|err| warn!(error = ?err, "resource group call failed"))
            .map_err(|err| map_resource_group_error(&err))?;
        Ok(group.hierarchy.tenant_id)
    }
}

fn project_barrier_mode(sdk: SdkBarrierMode) -> TrBarrierMode {
    match sdk {
        SdkBarrierMode::Respect => TrBarrierMode::Respect,
        SdkBarrierMode::Ignore => TrBarrierMode::Ignore,
    }
}

/// Map a tenant-resolver failure to a [`PluginError`], distinguishing a
/// DETERMINISTIC rejection from a transient outage.
///
/// `TenantNotFound` and `Unauthorized` are deterministic — the granted scope's
/// tenant cannot be resolved (deleted/never existed) or the caller is not
/// allowed to see it, and neither self-heals on retry. (The TR SDK overloads
/// `TenantNotFound` to mean "not found" OR "unauthorized" — built-in plugins
/// return it for both — so the classification holds whichever it actually is.)
/// Both stay fail-closed, but [`PluginError::TenantNotFound`] carries a
/// `scope_unresolvable` reason rather than a `resolver_timeout`, so a stale
/// grant naming a deleted tenant does not page on-call for a phantom outage.
/// Every other variant is a genuine transient resolver outage.
///
/// The retryable/non-retryable split is still invisible to the *caller*: both
/// project onto the SDK's `ServiceUnavailable` (see `domain::error`), which
/// needs an SDK change to fix. What is fixed here is the observability side.
fn map_tenant_resolver_error(err: &TenantResolverError) -> PluginError {
    match err {
        TenantResolverError::TenantNotFound { .. } | TenantResolverError::Unauthorized => {
            PluginError::TenantNotFound
        }
        _ => PluginError::TenantResolverUnavailable,
    }
}

/// Resource-group analogue of [`map_tenant_resolver_error`]. The PEP→PDP call
/// surfaces a canonical envelope; project it to the typed `ResourceGroupError`
/// for dispatch. `NotFound` and `PermissionDenied` are deterministic rejections
/// (neither is a transient outage); everything else (outage / internal / the
/// write-path conflict variants these read-only call sites never hit) falls
/// through to the transient label.
fn map_resource_group_error(err: &CanonicalError) -> PluginError {
    match ResourceGroupError::from(err.clone()) {
        ResourceGroupError::NotFound { .. } | ResourceGroupError::PermissionDenied { .. } => {
            PluginError::ResourceGroupNotFound
        }
        _ => PluginError::ResourceGroupUnavailable,
    }
}

/// Safety backstop for paginated resolver reads. Draining the cursor fixes the
/// silent first-page-only truncation, but an unbounded subtree could still
/// allocate without limit. A result this large is pathological — the constraint
/// generator's `max_expansion_ids` business limit (default `10_000`) would reject
/// it as `EXPANSION_INFEASIBLE_V1` long before this — so we cap accumulation far
/// above that and fail CLOSED (deny via a system error) rather than truncate the
/// allow-set or exhaust memory.
const MAX_PAGINATED_ITEMS: usize = 100_000;

/// Upper bound on pages drained per cursor walk. [`MAX_PAGINATED_ITEMS`] bounds
/// what a well-behaved upstream can return; this bounds what a misbehaving one
/// can cost. A page that carries a cursor but serves no items never moves the
/// item counter, so that cap alone cannot end the walk — and the walk spins
/// while holding the hierarchy cache's single-flight lease, so every waiter on
/// the same key spins with it. Derived from the resolver's 200-row page maximum
/// so a legitimate full drain of [`MAX_PAGINATED_ITEMS`] still fits.
const MAX_PAGINATED_PAGES: usize = 500;

/// Rows requested per drained page. The resource-group repositories resolve an
/// absent `$top` to their *default* page size (25), not their maximum, so a
/// drain that does not ask explicitly pays 8x the round trips and silently caps
/// a walk at `MAX_PAGINATED_PAGES * 25` — well under [`MAX_PAGINATED_ITEMS`].
/// Asking for the documented maximum is what makes the item cap reachable and
/// the assert below true.
const DRAIN_PAGE_SIZE: usize = 200;

/// Keeps the two caps from drifting: the page budget must still admit a full
/// [`MAX_PAGINATED_ITEMS`] drain at the page size the drains actually request,
/// so raising the item cap without the page cap — or shrinking the page size —
/// is a compile error rather than a silently unreachable limit.
const _: () = assert!(MAX_PAGINATED_PAGES * DRAIN_PAGE_SIZE >= MAX_PAGINATED_ITEMS);

/// Group ids per `IN (...)` when fetching memberships for a resolved group
/// subtree. Matches the RBAC repositories' `GROUP_PRINCIPALS_CHUNK` /
/// `ROLE_ID_CHUNK`, which chunk at the same width and for the same reason:
/// the id list is upstream-influenced, and an unbounded `IN` risks the
/// driver's bind-parameter limit and a bad plan.
const GROUP_ID_IN_CHUNK: usize = 500;

/// Decide whether a cursor walk advances, ends, or must fail closed.
///
/// `Ok(None)` is the one normal ending. Everything else is fail closed: these
/// drains build an allow-set, so a walk that cannot be trusted to terminate
/// must deny rather than return the rows it happened to collect. A cursor that
/// does not change is the cheapest such signal — it is caught on the second
/// page, before the page budget runs out.
fn next_page_cursor(
    next_cursor: Option<String>,
    previous: &mut Option<String>,
) -> Result<Option<CursorV1>, PluginError> {
    let Some(token) = next_cursor else {
        return Ok(None);
    };
    if previous.as_deref() == Some(token.as_str()) {
        warn!("resolver returned a non-advancing cursor; failing closed");
        return Err(PluginError::internal(
            "resolver returned a non-advancing cursor",
        ));
    }
    let cursor = CursorV1::decode(&token)
        .map_err(|e| PluginError::internal(format!("invalid resolver cursor: {e}")))?;
    *previous = Some(token);
    Ok(Some(cursor))
}

/// Drain every page of `get_group_descendants` for `group_id`, following the
/// cursor to exhaustion. Mirrors the resolver-side `drain_hierarchy_pages`
/// idiom. Without this the caller saw only the first page (server default ~25,
/// max 200), silently truncating the materialized allow-set. Fails closed if
/// the accumulated set exceeds [`MAX_PAGINATED_ITEMS`], if the walk outlives
/// [`MAX_PAGINATED_PAGES`], or if the cursor stops advancing.
async fn drain_group_descendants(
    rg: &dyn ResourceGroupReadHierarchy,
    ctx: &SecurityContext,
    group_id: Uuid,
) -> Result<Vec<ResourceGroupWithDepth>, PluginError> {
    let mut all: Vec<ResourceGroupWithDepth> = Vec::new();
    let mut query = ODataQuery::new().with_limit(DRAIN_PAGE_SIZE as u64);
    let mut previous_cursor: Option<String> = None;
    // Bounded: falling out of the loop means the page budget ran out with a
    // cursor still outstanding, which is its own fail-closed exit below.
    for _ in 0..MAX_PAGINATED_PAGES {
        let page = rg
            .get_group_descendants(ctx, group_id, &query)
            .await
            .inspect_err(|err| warn!(error = ?err, "resource group call failed"))
            .map_err(|err| map_resource_group_error(&err))?;
        all.extend(page.items);
        if all.len() > MAX_PAGINATED_ITEMS {
            warn!(
                group_id = %group_id,
                accumulated = all.len(),
                cap = MAX_PAGINATED_ITEMS,
                "group descendants exceeded pagination safety cap; failing closed"
            );
            return Err(PluginError::internal(
                "group descendant set exceeded pagination safety cap",
            ));
        }
        match next_page_cursor(page.page_info.next_cursor, &mut previous_cursor)? {
            Some(cursor) => query = query.with_cursor(cursor),
            None => return Ok(all),
        }
    }
    warn!(
        group_id = %group_id,
        cap = MAX_PAGINATED_PAGES,
        "group descendants exceeded pagination page cap; failing closed"
    );
    Err(PluginError::internal(
        "group descendant set exceeded pagination page cap",
    ))
}

/// Drain every page of `list_memberships` for `base_query`, following the cursor
/// to exhaustion. Same first-page-only truncation risk as
/// [`drain_group_descendants`], and the same three fail-closed bounds.
async fn drain_memberships(
    rg: &dyn ResourceGroupReadHierarchy,
    ctx: &SecurityContext,
    base_query: ODataQuery,
) -> Result<Vec<ResourceGroupMembership>, PluginError> {
    let mut all: Vec<ResourceGroupMembership> = Vec::new();
    let mut query = base_query.with_limit(DRAIN_PAGE_SIZE as u64);
    let mut previous_cursor: Option<String> = None;
    for _ in 0..MAX_PAGINATED_PAGES {
        let page = rg
            .list_memberships(ctx, &query)
            .await
            .inspect_err(|err| warn!(error = ?err, "resource group call failed"))
            .map_err(|err| map_resource_group_error(&err))?;
        all.extend(page.items);
        if all.len() > MAX_PAGINATED_ITEMS {
            warn!(
                accumulated = all.len(),
                cap = MAX_PAGINATED_ITEMS,
                "memberships exceeded pagination safety cap; failing closed"
            );
            return Err(PluginError::internal(
                "membership set exceeded pagination safety cap",
            ));
        }
        match next_page_cursor(page.page_info.next_cursor, &mut previous_cursor)? {
            Some(cursor) => query = query.with_cursor(cursor),
            None => return Ok(all),
        }
    }
    warn!(
        cap = MAX_PAGINATED_PAGES,
        "memberships exceeded pagination page cap; failing closed"
    );
    Err(PluginError::internal(
        "membership set exceeded pagination page cap",
    ))
}

fn build_group_id_in_filter(group_ids: &[Uuid]) -> Expr {
    Expr::In(
        Box::new(Expr::Identifier("group_id".to_owned())),
        group_ids
            .iter()
            .map(|id| Expr::Value(Value::Uuid(*id)))
            .collect(),
    )
}

fn parse_resource_ids(
    memberships: &[resource_group_sdk::models::ResourceGroupMembership],
) -> Vec<Uuid> {
    let mut dropped = 0_usize;
    let mut first_dropped: Option<&str> = None;
    let mut ids: Vec<Uuid> = memberships
        .iter()
        .filter_map(|m| {
            if let Ok(id) = Uuid::parse_str(&m.resource_id) {
                Some(id)
            } else {
                // Drop the row (fail-safe: it can't be authorized) but track
                // an aggregate count so a malformed-batch flood emits one
                // summary line instead of N per-row warns.
                dropped += 1;
                if first_dropped.is_none() {
                    first_dropped = Some(m.resource_id.as_str());
                }
                None
            }
        })
        .collect();
    if dropped > 0 {
        warn!(
            dropped_count = dropped,
            first_dropped = first_dropped.unwrap_or(""),
            "RG membership batch contained non-UUID resource_ids; dropped them"
        );
    }
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
#[path = "hierarchy_upstream_tests.rs"]
mod tests;
