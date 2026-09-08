//! Shared response construction — deny shapes and allow shapes.
//!
//! Despite the module name, this is the home for both:
//! - `build_deny_response(error_code, details)` for `decision: false` returns
//!   (scope mismatch, insufficient permissions, etc.).
//! - `build_allow_response(constraints)` for `decision: true` returns with a
//!   populated `Vec<Constraint>` for tenant-scoped allows.
//!
//! Plus the set of GTS error-code constants under `error_codes` so string
//! identifiers cannot drift via typo across the crate.
use authz_resolver_sdk::constraints::Constraint;
use authz_resolver_sdk::models::{DenyReason, EvaluationResponse, EvaluationResponseContext};

/// GTS error-code identifiers used in deny responses.
///
/// All of them live in one namespace — `gts.cf.core.errors.err.v1~cf.authz.errors.*`:
/// the canonical Constructor Fabric error base plus a single `cf.authz.errors`
/// instance namespace. There is deliberately no per-vendor split; the platform
/// recognises `cf` as its only vendor authority, so a vendor-scoped error
/// namespace would be a distinction these identifiers have no business
/// carrying. `deny_tests` pins the prefix so a code cannot drift out of it.
///
/// Constants live here so a typo (e.g. `scope_match.v1` vs `scope_mismatch.v1`)
/// becomes a compile error rather than a silent downstream contract break.
pub mod error_codes {
    use toolkit_gts::gts_id;

    /// Token scopes do not authorize the requested operation.
    pub(crate) const SCOPE_MISMATCH_V1: &str =
        gts_id!("cf.core.errors.err.v1~cf.authz.errors.scope_mismatch.v1");

    /// Subject has no role granting the requested operation on the resource
    /// type. Also the no-plugin path; plugins reuse this code rather than
    /// minting duplicates.
    pub(crate) const INSUFFICIENT_PERMISSIONS_V1: &str =
        gts_id!("cf.core.errors.err.v1~cf.authz.errors.insufficient_permissions.v1");

    /// Generated predicate references a property the PEP does not declare
    /// in `request.context.supported_properties`.
    pub(crate) const UNSUPPORTED_PROPERTY_V1: &str =
        gts_id!("cf.core.errors.err.v1~cf.authz.errors.unsupported_property.v1");

    /// Materialized `In`-list exceeds `max_expansion_ids` — shipping the
    /// predicate would yield an impractically large SQL `IN` clause.
    pub(crate) const EXPANSION_INFEASIBLE_V1: &str =
        gts_id!("cf.core.errors.err.v1~cf.authz.errors.expansion_infeasible.v1");

    /// Strict-mode GTS validator could not resolve the subject or resource
    /// type against the registry. Covers both subject and resource paths
    /// despite the "`resource_type`" name (PRD wording).
    pub(crate) const UNKNOWN_RESOURCE_TYPE_V1: &str =
        gts_id!("cf.core.errors.err.v1~cf.authz.errors.unknown_resource_type.v1");

    /// `require_constraints=true` but the constraint generator produced an
    /// empty constraint set — data access blocked rather than unconstrained.
    pub(crate) const CONSTRAINTS_UNAVAILABLE_V1: &str =
        gts_id!("cf.core.errors.err.v1~cf.authz.errors.constraints_unavailable.v1");

    /// The `AuthZEN` request itself is malformed — unknown subject type, empty or
    /// wildcard action, missing resource type, unreadable subject tenant.
    ///
    /// A caller's own mistake is a business deny, not a plugin failure. The SDK
    /// error enum has no `InvalidRequest` variant, so propagating these as `Err`
    /// reached the PEP as a 500-class `Internal`: PEPs retry it and on-call gets
    /// paged for a malformed request nobody can fix by retrying.
    pub(crate) const INVALID_REQUEST_V1: &str =
        gts_id!("cf.core.errors.err.v1~cf.authz.errors.invalid_request.v1");
}

/// Canonical `ServiceUnavailable` message strings.
///
/// Message TEXT only — nothing classifies on them. Each is the `Display`
/// output of one [`crate::domain::error::PluginError`] variant, and the variant
/// carries the classification. They stay centralized because integration tests
/// and log-based dashboards pin the exact strings, so a reword is a contract
/// change that belongs in one place.
pub mod service_errors {
    pub(crate) const RBAC_UNAVAILABLE: &str = "rbac service unavailable";
    pub(crate) const TENANT_RESOLVER_UNAVAILABLE: &str = "tenant resolver unavailable";
    pub(crate) const RESOURCE_GROUP_UNAVAILABLE: &str = "resource group unavailable";

    /// RBAC returned a normal allow whose aggregate scope could not be derived
    /// from its contributing role assignments. This is deterministic producer
    /// contract drift or partial payload corruption, not a resolver outage.
    /// Kept as an exact sentinel so metrics classify the fail-closed error as
    /// RBAC provenance failure rather than constraint-compilation failure.
    pub(crate) const RBAC_SCOPE_PROVENANCE_INVALID: &str =
        "rbac allow has invalid assignment-scope provenance";

    /// The GTS types-registry could not be reached during Strict-mode type
    /// validation. A transient dependency outage like the resolver ones, but
    /// kept as its own const+label so the metrics classifier does NOT bucket it
    /// under the catch-all `resolver_timeout` (which would page on-call for a
    /// phantom *resolver* outage when the *registry* is what is down).
    pub(crate) const GTS_REGISTRY_UNAVAILABLE: &str = "gts schema registry unavailable";

    /// A granted scope referenced a tenant the resolver would not resolve.
    /// DETERMINISTIC, not a transient outage: the tenant-resolver SDK overloads
    /// `TenantNotFound` to mean "not found" OR "unauthorized" (built-in plugins
    /// return it for both), and neither self-heals on retry. Kept distinct from
    /// `TENANT_RESOLVER_UNAVAILABLE` so metrics don't mislabel it as a
    /// `resolver_timeout` (which pages on-call for a phantom outage).
    pub(crate) const TENANT_NOT_FOUND: &str = "tenant not found or not accessible";

    /// A granted group scope referenced a resource group the resolver would not
    /// resolve. DETERMINISTIC (RG `NotFound`), not a transient outage — see
    /// `TENANT_NOT_FOUND`.
    pub(crate) const RESOURCE_GROUP_NOT_FOUND: &str = "resource group not found or not accessible";
}

/// Canonical request-validation message strings.
///
/// Message TEXT for the client-fault [`crate::domain::error::PluginError`]
/// variants — see `service_errors` above for why they are still centralized.
///
/// The SDK error enum has no `InvalidRequest` variant, so these reach a caller
/// as `Internal(...)`. The `invalid_request` metric classification comes from
/// the variant, not from this wording.
pub mod validation_messages {
    pub(crate) const MISSING_RESOURCE_TYPE: &str = "missing resource type";
    pub(crate) const INVALID_OPERATION_EMPTY: &str = "invalid operation: empty";
    pub(crate) const INVALID_OPERATION_WILDCARD: &str = "invalid operation: wildcards not allowed";
    /// Prefix of the dynamic `"unknown subject type: <value>"` message.
    pub(crate) const UNKNOWN_SUBJECT_TYPE_PREFIX: &str = "unknown subject type:";
    /// Prefix of the dynamic
    /// `"subject.properties[\"tenant_id\"] is present but ..."` messages.
    pub(crate) const UNREADABLE_SUBJECT_TENANT_PREFIX: &str =
        "subject.properties[\"tenant_id\"] is present but";
}

/// Build the standard deny-response: `decision: false`, no constraints, the
/// supplied `error_code` and optional human-readable `details` carried in
/// `deny_reason`.
pub(crate) fn build_deny_response(error_code: &str, details: Option<String>) -> EvaluationResponse {
    EvaluationResponse {
        decision: false,
        context: EvaluationResponseContext {
            constraints: Vec::new(),
            deny_reason: Some(DenyReason {
                error_code: error_code.to_owned(),
                details,
            }),
        },
    }
}

/// Build the standard allow-response: `decision: true` with the supplied
/// constraints and no `deny_reason`. Multiple constraints in the vector are
/// OR-combined at the response level (per the SDK doc on
/// `EvaluationResponseContext::constraints`).
pub(crate) fn build_allow_response(constraints: Vec<Constraint>) -> EvaluationResponse {
    EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints,
            deny_reason: None,
        },
    }
}

#[cfg(test)]
#[path = "deny_tests.rs"]
mod tests;
