//! Request validation. Pure function — no I/O, no state, no plugin construction.
//!
//! Runs first in `evaluate()` so malformed requests fail fast. Each failure is
//! a client-fault [`PluginError`] variant, which carries its own
//! `invalid_request` classification — see `domain::error`.
use authz_resolver_sdk::models::EvaluationRequest;
use tracing::warn;

use crate::domain::error::PluginError;
use crate::domain::subject_type::{TrustedSystemActors, classify_subject_type};

/// Validate an incoming evaluation request.
///
/// Sequence (short-circuits on first failure):
/// 1. `subject.type` present (`Option::Some`)
/// 2. `subject.type` classifies to a known `PrincipalType` (raw `IdP` claim or
///    GTS tag — see `subject_type::classify_subject_type`)
/// 3. `action.name` non-empty
/// 4. `action.name` contains no wildcard characters (`*`, `?`)
/// 5. `resource.type` non-empty (the field is `String` in the SDK, so
///    "missing" means the empty string at the typed-struct level)
// Flat sequence of independent field checks.
#[allow(clippy::cognitive_complexity)]
pub(crate) fn validate(
    request: &EvaluationRequest,
    trusted: &TrustedSystemActors,
) -> Result<(), PluginError> {
    // `subject_type` is optional: an absent value defaults to `User` in
    // `map_subject_type` (mirrors RBAC's `principal_type_from_security_context`,
    // which some auth flows reach with no tag). Only a PRESENT-but-unrecognized
    // value is rejected here — except a configured trusted system actor, whose
    // tag is neither a user nor a service principal and which is short-circuited
    // to a trusted Allow in `policy_evaluator`; it must pass validation first.
    if let Some(subject_type) = request.subject.subject_type.as_deref()
        && !trusted.matches(request.subject.id, Some(subject_type))
        && classify_subject_type(subject_type).is_none()
    {
        warn!(
            subject_type,
            "authz request validation failed: unknown subject type"
        );
        return Err(PluginError::UnknownSubjectType {
            value: subject_type.to_owned(),
        });
    }

    let action_name = request.action.name.as_str();
    if action_name.is_empty() {
        warn!("authz request validation failed: empty action name");
        return Err(PluginError::InvalidOperationEmpty);
    }
    if action_name.contains('*') || action_name.contains('?') {
        warn!(
            action_name,
            "authz request validation failed: wildcard in action name"
        );
        return Err(PluginError::InvalidOperationWildcard);
    }

    if request.resource.resource_type.is_empty() {
        warn!("authz request validation failed: missing resource type");
        return Err(PluginError::MissingResourceType);
    }

    Ok(())
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
