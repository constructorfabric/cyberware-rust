//! Shared authz primitives for `IngestServiceImpl::publish_event` and
//! `DeliveryServiceImpl::join` (`eb-authz-enforcement`, `gears-rust#4516`).
//! `TOPIC_RESOURCE`/`EVENT_TYPE_RESOURCE` reuse `event-broker-sdk::gts`'s
//! `TopicV1::TYPE_ID`/`EVENT_TYPE_RESOURCE_TYPE` constants - the same GTS
//! strings `api/rest/error.rs`'s `#[resource_error(...)]` macros use for
//! `TopicResourceError`/`EventTypeResourceError`, though that file keeps its
//! own literal (a round-trip test asserts they stay equal - see
//! `eb-gts-type-registration`'s design.md "cannot be centralized").
//!
//! Tenant scope (`TENANT_SCOPE_RESOURCE`) is *also* a `PolicyEnforcer` call,
//! not a `tenant-resolver-sdk` call - `oagw`'s
//! `bind.rs::validate_bind_constraints` precedent expresses "authorized to
//! act for tenant X" purely via `resource_property(OWNER_TENANT_ID, ..)` on
//! a `PolicyEnforcer::access_scope_with` call, trusting the deployed
//! authz-resolver plugin/policy to actually enforce tenant hierarchy -
//! "depending on configuration", not something this domain layer
//! re-implements against a second SDK.

use authz_resolver_sdk::{AccessRequest, PolicyEnforcer, ResourceType};
use event_broker_sdk::gts::{
    CONSUMER_GROUP_RESOURCE_TYPE, EVENT_TYPE_RESOURCE_TYPE, REQUEST_RESOURCE_TYPE, TopicV1,
};
use toolkit_gts::GtsSchema;
use toolkit_security::{SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

pub const TOPIC_RESOURCE: ResourceType = ResourceType::from_static(TopicV1::TYPE_ID, &[]);
pub const EVENT_TYPE_RESOURCE: ResourceType =
    ResourceType::from_static(EVENT_TYPE_RESOURCE_TYPE, &[]);
/// `Named` consumer-group authorization (`docs/DESIGN.md`'s Consumer Group
/// Lifecycle: `consumer_group:define`/`:consume`/`:manage` via PEP) -
/// `Anonymous` groups use `TENANT_SCOPE_RESOURCE` instead (owner-tenant
/// equality), never this (`eb-tenant-isolation-fix`).
pub const CONSUMER_GROUP_RESOURCE: ResourceType =
    ResourceType::from_static(CONSUMER_GROUP_RESOURCE_TYPE, &[]);
/// Generic resource for the platform-tenant-scope check
/// (`docs/openapi.yaml`'s `TenantIdNotAuthorized`) - deliberately distinct
/// from `TOPIC_RESOURCE`/`EVENT_TYPE_RESOURCE`: this call is about the
/// calling principal's authority over the *target tenant*, not the
/// topic/event type being acted on, so it stays its own `PolicyEnforcer`
/// call (and therefore its own denial code, independent of whichever of
/// the other checks passed or failed).
pub const TENANT_SCOPE_RESOURCE: ResourceType =
    ResourceType::from_static(REQUEST_RESOURCE_TYPE, &[pep_properties::OWNER_TENANT_ID]);

/// Overwrites a `DomainError::Forbidden`'s `code` with a specific
/// `docs/openapi.yaml` code (`TopicNotAuthorized`/`EventTypeNotAuthorized`/
/// `NotAuthorizedToProduce`/`TenantIdNotAuthorized`) - `From<EnforcerError>`
/// only knows a generic `"AuthzDenied"`, since which specific check failed
/// is known only at the call site (design.md "code-per-call-site"). A no-op
/// for every other `DomainError` variant (a PEP failure only ever produces
/// `Forbidden` or `Internal` per that `From` impl).
#[must_use]
pub fn with_forbidden_code(mut err: DomainError, code: &'static str) -> DomainError {
    if let DomainError::Forbidden { code: c, .. } = &mut err {
        *c = code;
    }
    err
}

/// Confirms the calling principal is authorized to act as `target` tenant -
/// a dedicated `PolicyEnforcer` check against `TENANT_SCOPE_RESOURCE`
/// (`pep_properties::OWNER_TENANT_ID` set to `target`), not a
/// `tenant-resolver-sdk` call. Whether this actually enforces tenant
/// hierarchy (vs. e.g. always allowing) is entirely the deployed
/// authz-resolver plugin/policy's concern.
///
/// # Errors
/// Returns `DomainError::Forbidden { code: "TenantIdNotAuthorized", .. }` if
/// the PEP denies, or the mapped `DomainError` for any other `EnforcerError`.
pub async fn tenant_authorized(
    ctx: &SecurityContext,
    policy_enforcer: &PolicyEnforcer,
    action: &'static str,
    target: Uuid,
) -> Result<(), DomainError> {
    policy_enforcer
        .access_scope_with(
            ctx,
            &TENANT_SCOPE_RESOURCE,
            action,
            None,
            &AccessRequest::new()
                .resource_property(pep_properties::OWNER_TENANT_ID, target)
                .require_constraints(false),
        )
        .await
        .map_err(|e| with_forbidden_code(e.into(), "TenantIdNotAuthorized"))?;
    Ok(())
}
