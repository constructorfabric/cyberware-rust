//! [`HierarchyUpstream`] — the port the materialization logic reads hierarchy
//! through.
//!
//! # Why the split
//!
//! This trait is the seam between the domain decision (turn a
//! `PermissionScopeType` into a `Materialization`, applying the barrier, status
//! and fail-closed rules) and the upstream adapter (`toolkit_odata` filter
//! construction, `CursorV1` decoding, page draining with safety caps, `IN`-list
//! chunking, and tenant/resource-group SDK error mapping). Everything on this
//! side of it is stated in domain terms — plain ids, statuses and
//! [`PluginError`] — with no SDK type, no query, and no cursor. The adapter
//! lives in `crate::infra::hierarchy_upstream`.
//!
//! # What deliberately stays in the domain
//!
//! The port returns raw upstream FACTS, not decisions. The `[Active]` clamp on
//! a granted root, the fail-closed check for resources with no owning tenant,
//! and the group-ownership comparison are all security policy (design §3.6),
//! so they stay with the materialization logic that owns them — the adapter
//! reports what upstream said and nothing more.

use async_trait::async_trait;
use authz_resolver_sdk::models::BarrierMode as SdkBarrierMode;
use tenant_resolver_sdk::models::TenantStatus;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::PluginError;
use crate::domain::hierarchy_cache::TenantMetadata;

/// One tenant subtree as upstream reports it, before any authz clamp.
///
/// The root is carried separately WITH its status because the resolver returns
/// the root regardless of status and the `[Active]` root clamp is the caller's
/// decision to make — folding the root into the id list here would silently
/// discard the only signal that clamp needs.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TenantSubtreeFetch {
    /// The granted root's id.
    pub(crate) root_id: Uuid,
    /// The granted root's lifecycle status.
    pub(crate) root_status: TenantStatus,
    /// Descendant ids, root excluded, already filtered by the requested
    /// status list (an empty list means every status).
    pub(crate) descendant_ids: Vec<Uuid>,
}

/// One group subtree as upstream reports it, before any authz clamp.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupSubtreeFetch {
    /// Distinct resource ids that belong to any group in the subtree.
    pub(crate) resource_ids: Vec<Uuid>,
    /// Distinct owning tenants of the groups in the subtree.
    pub(crate) owner_tenant_ids: Vec<Uuid>,
}

/// Read-only access to the tenant and resource-group hierarchies.
///
/// Every method is a single logical read that may cost several upstream
/// round-trips; the adapter owns the paging, and the caller owns the caching.
#[async_trait]
pub(crate) trait HierarchyUpstream: Send + Sync + 'static {
    /// The platform root tenant's id.
    async fn root_tenant_id(&self) -> Result<Uuid, PluginError>;

    /// The root plus its descendants.
    ///
    /// `descendant_status` filters descendants only; an empty list means every
    /// status. The root is returned regardless of its status — see
    /// [`TenantSubtreeFetch`].
    async fn tenant_subtree(
        &self,
        root_tenant_id: Uuid,
        barrier_mode: SdkBarrierMode,
        descendant_status: Vec<TenantStatus>,
    ) -> Result<TenantSubtreeFetch, PluginError>;

    /// The four documented fields of one tenant.
    async fn tenant_metadata(&self, tenant_id: Uuid) -> Result<TenantMetadata, PluginError>;

    /// Every resource reachable through the subtrees rooted at `root_group_ids`,
    /// plus the distinct owning tenants of the groups in those subtrees.
    async fn group_subtree(
        &self,
        root_group_ids: &[Uuid],
    ) -> Result<GroupSubtreeFetch, PluginError>;

    /// Resources of the supplied groups only — no descendant traversal.
    async fn group_member_resource_ids(&self, group_ids: &[Uuid])
    -> Result<Vec<Uuid>, PluginError>;

    /// The tenant that owns `group_id`.
    async fn group_owner_tenant_id(&self, group_id: Uuid) -> Result<Uuid, PluginError>;
}
