//! In-process adapter for `rbac_sdk::RbacServiceClientV1`.
//!
//! Registered in `ClientHub` so in-process consumers (e.g. AuthZ Resolver
//! Plugin) can resolve `dyn RbacServiceClientV1` and call into RBAC. The
//! adapter forwards both trait methods to the owned
//! [`crate::domain::permission_evaluator::PermissionEvaluator`]
//! AFTER enforcing the trait's trust contract:
//!
//! 1. The caller's `SecurityContext` MUST be authenticated (non-nil
//!    `subject_id` and `subject_tenant_id`).
//! 2. The request's `context_scope` MUST be reachable from the caller's
//!    authority — a first-party root caller (`token_scopes == ["*"]`) may
//!    address any scope; every other caller is confined to scopes under its
//!    own `subject_tenant_id`.
//!
//! The check is the **caller-side** symmetric of
//! [`crate::domain::service::permission_evaluator::assignment_applies`]:
//! `assignment_applies` filters grants by scope, this gate filters callers
//! by scope. The SDK trait carries `&SecurityContext`, so there is no
//! nil-UUID fallback.

use std::sync::Arc;

use async_trait::async_trait;

use rbac_sdk::api::{RbacServiceClientV1, sealed};
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    EvaluatePermissionRequest, EvaluatePermissionResponse, GetSubjectRolesRequest,
    GetSubjectRolesResponse, Scope,
};
use toolkit_security::SecurityContext;

use crate::domain::caller_scope::caller_scope_from_context;
use crate::domain::permission_evaluator::PermissionEvaluator;
use crate::domain::role_assignment_repo::RoleAssignmentRepository;
use crate::domain::role_definition::CallerScope;
use crate::domain::role_definition_repo::RoleDefinitionRepository;

/// In-process client adapter forwarding `RbacServiceClientV1` calls
/// to the owned [`PermissionEvaluator`].
///
/// Generic over the evaluator's repositories rather than pinned to the
/// production pair: the repo traits take `<C: DBRunner>` and cannot be
/// trait objects, so pinning would stop a test injecting stub repos —
/// which is exactly what `authz-resolver-plugin`'s vertical test does.
/// The `dyn` boundary stays where it was, on `RbacServiceClientV1`.
pub struct RbacServiceLocalClient<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> {
    evaluator: Arc<PermissionEvaluator<AR, DR>>,
}

impl<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> std::fmt::Debug
    for RbacServiceLocalClient<AR, DR>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RbacServiceLocalClient").finish()
    }
}

impl<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> RbacServiceLocalClient<AR, DR> {
    #[must_use]
    pub fn new(evaluator: Arc<PermissionEvaluator<AR, DR>>) -> Self {
        Self { evaluator }
    }
}

// Sealing token for `RbacServiceClientV1`. Adding a second
// implementor anywhere in the workspace is a deliberate act and inherits
// the trait's trust contract.
impl<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> sealed::Sealed
    for RbacServiceLocalClient<AR, DR>
{
}

/// Enforce the trait's trust contract before forwarding to the evaluator.
/// Returns the caller's effective [`CallerScope`] on success.
fn authorize_caller(
    ctx: &SecurityContext,
    request_scope: &Scope,
) -> Result<CallerScope, RbacServiceError> {
    if ctx.subject_id().is_nil() || ctx.subject_tenant_id().is_nil() {
        return Err(RbacServiceError::authorization_denied(
            "anonymous caller cannot query RBAC state",
        ));
    }
    let caller = caller_scope_from_context(ctx);
    let allowed = match (&caller, request_scope) {
        // Root callers may address any scope.
        (CallerScope::Root, _) => true,
        // Tenant(T) callers may address only their own tenant or RGs under it.
        (
            CallerScope::Tenant(t),
            Scope::Tenant { tenant_id } | Scope::ResourceGroup { tenant_id, .. },
        ) => t == tenant_id,
        // Tenant callers may NOT address `Scope::Root`, and `Scope` is
        // `#[non_exhaustive]` — reject `Root` and every future variant.
        _ => false,
    };
    if !allowed {
        return Err(RbacServiceError::authorization_denied(
            "caller's tenant does not cover request scope",
        ));
    }
    Ok(caller)
}

#[async_trait]
impl<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> RbacServiceClientV1
    for RbacServiceLocalClient<AR, DR>
{
    async fn get_subject_roles(
        &self,
        ctx: &SecurityContext,
        request: GetSubjectRolesRequest,
    ) -> Result<GetSubjectRolesResponse, RbacServiceError> {
        authorize_caller(ctx, &request.context_scope)?;
        // For `Scope::Root` (only reachable by a root caller after the gate)
        // fall back to the caller's home tenant — non-nil because anonymous
        // contexts were rejected above.
        let context_tenant_id = request
            .context_scope
            .tenant_id()
            .unwrap_or_else(|| ctx.subject_tenant_id());
        let roles = self
            .evaluator
            .get_subject_roles(
                ctx,
                &request.subject_id,
                request.principal_type,
                context_tenant_id,
                request.include_group_roles,
            )
            .await?;
        Ok(GetSubjectRolesResponse::new(roles))
    }

    async fn evaluate_permission(
        &self,
        ctx: &SecurityContext,
        request: EvaluatePermissionRequest,
    ) -> Result<EvaluatePermissionResponse, RbacServiceError> {
        authorize_caller(ctx, &request.context_scope)?;
        // Pass the typed `Scope` through. Collapsing it to `tenant_id`
        // here would re-introduce the cross-RG bypass closed in the
        // evaluator (see `PermissionEvaluator::evaluate_permission`).
        let result = self
            .evaluator
            .evaluate_permission(
                ctx,
                &request.subject_id,
                request.principal_type,
                &request.operation,
                &request.resource_type,
                &request.context_scope,
            )
            .await?;
        Ok(EvaluatePermissionResponse::from_result(result))
    }
}

// The caller-side authz gate is unit-tested next door (no Docker); the
// end-to-end paths through a real Postgres live in
// `tests/postgres_local_client.rs`.
#[cfg(test)]
#[path = "local_client_tests.rs"]
mod local_client_tests;
