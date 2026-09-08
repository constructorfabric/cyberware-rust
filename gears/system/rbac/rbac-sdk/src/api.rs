//! In-process RBAC client trait published in toolkit `ClientHub`.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::error::RbacServiceError;
use crate::models::{
    EvaluatePermissionRequest, EvaluatePermissionResponse, GetSubjectRolesRequest,
    GetSubjectRolesResponse,
};

/// Sealing module — `RbacServiceClientV1` is closed for extension. Only impls
/// inside the workspace that explicitly add `impl rbac_sdk::api::sealed::Sealed
/// for X` can satisfy the bound. This is how the trait advertises a fixed
/// trust contract: every implementor passes through a deliberate review.
pub mod sealed {
    /// Witness type that locks `RbacServiceClientV1` to in-tree implementors.
    pub trait Sealed {}
}

/// In-process RBAC client surface published in toolkit `ClientHub`.
///
/// Consumers resolve `dyn RbacServiceClientV1` from `ClientHub` and call these
/// methods to read RBAC state. The trait is `Send + Sync` so it can be stored
/// in `arc_swap::ArcSwapOption` and resolved across runtime threads.
///
/// # Trust contract
///
/// `ctx` is the **caller's** verified `SecurityContext`, NOT the subject being
/// queried. Implementations:
///
/// 1. MUST reject anonymous / partially-built contexts (nil `subject_id` or
///    `subject_tenant_id`) with [`RbacServiceError::AuthorizationDenied`].
/// 2. MUST verify that `request.context_scope` is reachable from `ctx`'s
///    authority. A first-party root caller (`token_scopes == ["*"]`) may
///    address any scope; every other caller is constrained to scopes under
///    its `subject_tenant_id`.
/// 3. MUST NOT re-authenticate `ctx` — callers pass values derived from
///    middleware-verified contexts. Reusing `ctx` here is the same trust
///    boundary that REST handlers rely on.
///
/// The trait is **sealed** via [`sealed::Sealed`]: third-party crates cannot
/// implement it. Adding a new in-tree implementor is a deliberate act and
/// inherits this contract.
#[async_trait]
pub trait RbacServiceClientV1: sealed::Sealed + Send + Sync {
    /// Returns all role assignments for a subject in a tenant context,
    /// including RG-scoped assignments under the context tenant.
    ///
    /// `ctx` is the caller's verified identity; see the trait-level trust
    /// contract.
    async fn get_subject_roles(
        &self,
        ctx: &SecurityContext,
        request: GetSubjectRolesRequest,
    ) -> Result<GetSubjectRolesResponse, RbacServiceError>;

    /// Evaluates a single `{ operation, resource_type }` access check for the
    /// given subject in the caller's tenant context.
    ///
    /// `ctx` is the caller's verified identity; see the trait-level trust
    /// contract.
    async fn evaluate_permission(
        &self,
        ctx: &SecurityContext,
        request: EvaluatePermissionRequest,
    ) -> Result<EvaluatePermissionResponse, RbacServiceError>;
}
