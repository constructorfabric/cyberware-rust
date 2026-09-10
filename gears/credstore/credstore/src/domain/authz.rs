//! Authorization policy-enforcement helpers for typed credentials.
//!
//! Builds the concrete GTS resource type for each credential and converts PDP
//! decisions into tenant-scoped [`AccessScope`]
//! values or fail-closed domain errors.

use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};

use crate::domain::error::DomainError;

/// PDP resource type for a concrete credential type: the full derived GTS id
/// (design §5.4), e.g.
/// `gts.cf.core.credstore.credential.v1~cf.core.credstore.api_key.v1~`.
///
/// This is the **only** resource type credstore evaluates: every operation
/// authorizes against the credential's full concrete type (including
/// `generic`), so policies can target any type without a separate base-type
/// gate. The id comes from the per-operation types-registry resolution
/// ([`crate::domain::secret::type_resolver::ResolvedSecretType::gts_id`]),
/// so dynamically registered types are addressable without a release.
#[must_use]
pub fn credential_type_resource(gts_id: &str) -> ResourceType {
    ResourceType::new(gts_id.to_owned(), &[pep_properties::OWNER_TENANT_ID])
}

/// The six PDP actions ADR-0004 replaces the shipped `read`/`write`/`delete`
/// with: plain verbs for the record, `_secret`-suffixed verbs for the value.
/// `LIST` is evaluated by the collection read (Phase 3, ADR-0005) — defined
/// here now so the vocabulary is complete and stable.
pub mod actions {
    /// Collection read (Phase 3). Also implied by `read` (a record reader
    /// can list what it can read), per ADR-0004's "Implications between the
    /// actions".
    pub const LIST: &str = "list";
    /// Point read of the credential record (`GET /credentials/{ref}`).
    pub const READ: &str = "read";
    /// `PUT`/`PATCH` of any metadata field (`sharing`, `fallback`,
    /// `expires_at`, `secret_type` on replace).
    pub const WRITE: &str = "write";
    /// `DELETE /credentials/{ref}`.
    pub const DELETE: &str = "delete";
    /// Point read of the value (`GET /credentials/{ref}/secret`).
    pub const READ_SECRET: &str = "read_secret";
    /// `PUT`/`PATCH` of the `value` field.
    pub const WRITE_SECRET: &str = "write_secret";
}

/// Map a PEP enforcement failure to a domain error (fail-closed).
/// `Denied` / `CompileFailed` → `AccessDenied` (403); `EvaluationFailed` → `ServiceUnavailable` (503).
#[must_use]
pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DomainError::AccessDenied {
                cause: Some(Box::new(err)),
            }
        }
        EnforcerError::EvaluationFailed(source) => DomainError::ServiceUnavailable {
            detail: "authorization evaluation failed".to_owned(),
            retry_after: None,
            cause: Some(Box::new(EnforcerError::EvaluationFailed(source))),
        },
    }
}

/// Returns the PDP `AccessScope` for `action` on `resource` for the
/// caller's tenant.
///
/// # Errors
///
/// Returns `DomainError::AccessDenied` if the PDP denies access or fails to compile constraints.
/// Returns `DomainError::ServiceUnavailable` if the PDP evaluation call fails.
pub async fn scope_for(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    resource: &ResourceType,
    action: &str,
) -> Result<AccessScope, DomainError> {
    let tenant = ctx.subject_tenant_id();
    let request = AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, tenant)
        .require_constraints(true);
    enforcer
        .access_scope_with(ctx, resource, action, None, &request)
        .await
        .map_err(map_enforcer_err)
}
