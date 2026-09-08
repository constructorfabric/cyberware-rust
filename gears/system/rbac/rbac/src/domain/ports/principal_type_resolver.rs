//! Map a [`SecurityContext`] to a [`PrincipalType`] for the policy
//! enforcer.
//!
//! Two shapes reach this resolver in real deployments:
//!
//! 1. **`GTS`-tagged subject types** like
//!    `gts.cf.core.security.subject_user.v1~`. Some plugins / SDKs
//!    emit these. Matched by the `subject_*` substring inside the tag.
//! 2. **Raw `IdP` claim values** like `"user"` or `"service"`.
//!    `keycloak-authn-plugin` and `static-authn-plugin` pass the
//!    incoming claim through to `SecurityContext::subject_type`
//!    verbatim, so `RBAC` sees the bare value. Matched by exact
//!    equality against [`RAW_USER`] / [`RAW_SERVICE`] / [`RAW_GROUP`].
//!
//! Mapping rules:
//!
//! * Recognised shape (raw or `GTS`) → `Ok(PrincipalType::…)`.
//! * Unknown `subject_type` → `Err(DomainError::Validation)`. Falling back to
//!   `PrincipalType::User` would turn an identity-layer typo or an upstream
//!   regression into an authorisation decision — a `User`-shaped grant applying
//!   where a `Group`-shaped one would have been denied.
//! * Absent `subject_type` (`None`) → `Ok(User)`. Some auth flows legitimately
//!   omit the tag.

use rbac_sdk::models::PrincipalType;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// Raw `IdP` claim value for human users (Keycloak `user_type=user`).
const RAW_USER: &str = "user";
/// Raw `IdP` claim value for service principals (Keycloak / static-authn
/// emit `"service"` for S2S identities).
const RAW_SERVICE: &str = "service";
/// Alternate raw value some `IdPs` emit for service principals.
const RAW_SERVICE_PRINCIPAL: &str = "service_principal";
/// Raw `IdP` claim value for groups.
const RAW_GROUP: &str = "group";

/// `GTS` subject-type substring marking a human user.
const SUBJECT_USER_PATTERN: &str = "subject_user";
/// `GTS` subject-type substring marking a service principal.
const SUBJECT_SERVICE_PATTERN: &str = "subject_service";
/// `GTS` subject-type substring marking a group.
const SUBJECT_GROUP_PATTERN: &str = "subject_group";

/// Classify a non-empty `subject_type` string into a `PrincipalType`.
/// Raw `IdP` claim values are matched by exact equality; `GTS` tags are
/// matched by the canonical `subject_*` substring so versioned tags
/// (`subject_user.v1~`, `subject_user.v2~`, …) stay accepted without
/// a code change.
fn classify_subject_type(s: &str) -> Option<PrincipalType> {
    // Exact match against the raw IdP claim values that
    // keycloak-authn-plugin and static-authn-plugin emit. Exact
    // equality keeps this from matching unrelated substrings — e.g.
    // a tag containing "super_user" must not classify as User just
    // because it contains "user".
    match s {
        RAW_USER => return Some(PrincipalType::User),
        RAW_SERVICE | RAW_SERVICE_PRINCIPAL => return Some(PrincipalType::ServicePrincipal),
        RAW_GROUP => return Some(PrincipalType::Group),
        _ => {}
    }

    // GTS-tag fallback. Substring is intentional here — the canonical
    // tag carries a .vN~ suffix and may carry additional namespacing.
    // Order matters: probe the more specific service pattern before
    // the user one.
    if s.contains(SUBJECT_SERVICE_PATTERN) || s.contains(RAW_SERVICE_PRINCIPAL) {
        Some(PrincipalType::ServicePrincipal)
    } else if s.contains(SUBJECT_GROUP_PATTERN) {
        Some(PrincipalType::Group)
    } else if s.contains(SUBJECT_USER_PATTERN) {
        Some(PrincipalType::User)
    } else {
        None
    }
}

/// Derive the [`PrincipalType`] for an authenticated caller.
///
/// See module-level docs for the mapping table and the rationale for
/// rejecting unknown subject types.
///
/// # Errors
///
/// Returns [`DomainError::Validation`] if `subject_type` is present
/// but matches none of the known shapes (raw value or `GTS` tag).
pub fn principal_type_from_security_context(
    ctx: &SecurityContext,
) -> Result<PrincipalType, DomainError> {
    match ctx.subject_type() {
        Some(s) => classify_subject_type(s).ok_or_else(|| DomainError::Validation {
            detail: format!(
                "unknown subject_type '{s}'; expected one of the raw values \
                 'user' / 'service' / 'service_principal' / 'group' or a GTS \
                 tag containing 'subject_user' / 'subject_service' / 'subject_group'"
            ),
        }),
        None => Ok(PrincipalType::User),
    }
}

#[cfg(test)]
#[path = "principal_type_resolver_tests.rs"]
mod tests;
