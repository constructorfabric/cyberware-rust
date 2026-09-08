//! Resource-group read port for the RBAC module.
//!
//! `RbacRgRead` is the narrow, RBAC-facing read contract the rest of
//! this crate consumes for resource-group lookups. It exposes only the
//! two methods RBAC needs (`get_group`, `list_memberships`) and keeps
//! the wider `resource-group-sdk` surface off the domain
//! layer entirely. The infrastructure-side adapter
//! [`crate::infra::rg_adapter`] translates the upstream `ResourceGroup*`
//! shapes into the RBAC-owned projection types below; everything in
//! `domain/` consumes only those projections.
//!
//! Why projections instead of re-exports: the domain layer is meant to be
//! SDK-agnostic. Importing `resource_group_sdk::error::ResourceGroupError` into
//! the port would couple `RoleAssignmentService` and `ScopeValidator` to another
//! gear's wire vocabulary, so any upstream rename would ripple straight into
//! RBAC's pure-domain layer.

use std::collections::HashMap;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// RBAC-owned projection of a resource group. Carries only the fields
/// the domain layer actually reads; the infra-side adapter is free to
/// discard everything else from the upstream `ResourceGroup` shape.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RbacRgGroup {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// Group display name. Backs `principal_name` on role-assignment
    /// reads for `Group` principals: the upstream shape already carries
    /// it, so naming a group costs no extra round trip beyond the
    /// listing itself.
    pub name: String,
}

/// RBAC-owned projection of a resource-group membership row. The
/// evaluator currently only needs `group_id`; widen the struct here
/// when a future RBAC code path needs more fields, not by re-exporting
/// the upstream shape.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RbacRgMembership {
    pub group_id: Uuid,
}

/// Error variants `RbacRgRead` implementations can surface to the
/// domain layer. `NotFound` is callable-handleable; everything else
/// rides in `Upstream` so callers can preserve the source chain
/// without leaking the SDK's discriminator into domain code.
#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum RbacRgReadError {
    /// The requested resource group / membership does not exist.
    #[error("resource group not found")]
    NotFound,
    /// Any other upstream failure (transport, internal, validation).
    /// The boxed error is the original SDK error; the domain layer
    /// reads it through `source()` for audit logging only.
    #[error("resource-group upstream: {0}")]
    Upstream(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// Internal RBAC-facing read port for resource-group data.
#[async_trait]
#[allow(
    dead_code,
    reason = "Methods are exercised at evaluation time via the \
              `ResourceGroupReadAdapter` (`Gear::init()` wraps it around \
              `dyn ResourceGroupReadHierarchy`): `get_group` in `ScopeValidator`, \
              `list_memberships` in `resolve_group_memberships`, `group_names` \
              in the role-assignment display-name hydrator."
)]
pub trait RbacRgRead: Send + Sync {
    /// Fetch a single resource group by ID.
    async fn get_group(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<RbacRgGroup, RbacRgReadError>;

    /// Resolve display names for a set of group ids in as few upstream
    /// calls as possible.
    ///
    /// Unknown ids are absent from the returned map — this is a display
    /// read, so a group that was deleted between the assignment write
    /// and this read is not an error. Duplicate ids in `ids` are
    /// deduplicated by the implementation.
    ///
    /// # Errors
    ///
    /// [`RbacRgReadError`] when the upstream listing could not be
    /// completed. Callers render that as "no names", never as a failed
    /// read.
    async fn group_names(
        &self,
        ctx: &SecurityContext,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, RbacRgReadError>;

    /// List resource-group memberships matching the given `OData` query.
    ///
    /// Forwards `query` verbatim to the upstream
    /// `ResourceGroupReadHierarchy` read.
    /// Callers that need a subject-scoped view MUST supply an `OData`
    /// filter such as `resource_id eq '<subject_id>'` — omitting it can
    /// fold in memberships of other subjects visible to the caller.
    async fn list_memberships(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<RbacRgMembership>, RbacRgReadError>;
}
