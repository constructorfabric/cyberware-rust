//! Subject-type classification — maps the `SecurityContext.subject_type`
//! string (carried in the `AuthZEN` `Subject.type`) to an RBAC `PrincipalType`.
//!
//! Two shapes reach the plugin in real deployments, exactly as documented on
//! the RBAC service's canonical resolver (`principal_type_resolver`):
//!
//! 1. **Raw `IdP` claim values** — `oidc`/`static` authn plugins pass the
//!    incoming `user_type` claim through to `SecurityContext::subject_type`
//!    verbatim, so the plugin sees the bare `"user"` / `"service"`. (On the
//!    shared cluster Keycloak emits `user_type=user`, which overrides the
//!    configured `default_subject_type` GTS id.)
//! 2. **GTS-tagged subject types** — `gts.cf.core.security.subject_user.v1~`.
//!    Matched by the `subject_*` substring so the vendor segment and `.vN~`
//!    suffix don't matter.
//!
//! This mirrors RBAC's `classify_subject_type` so the two layers agree on what
//! a caller is. The plugin only authorizes `User` and `ServicePrincipal`
//! subjects (groups are never direct subjects — DESIGN §3.5); a `group` shape
//! or any unrecognized value returns `None`, which callers turn into a
//! fail-closed `unknown subject type` error.

use rbac_sdk::models::PrincipalType;
use uuid::Uuid;

/// Raw `IdP` claim value for human users.
const RAW_USER: &str = "user";
/// Raw `IdP` claim value some `IdPs` emit for service principals.
const RAW_SERVICE: &str = "service";
/// Alternate raw value some `IdPs` emit for service principals.
const RAW_SERVICE_PRINCIPAL: &str = "service_principal";

/// GTS subject-type substring marking a human user (vendor/version-agnostic).
const SUBJECT_USER_PATTERN: &str = "subject_user";
/// GTS subject-type substring marking a service principal.
const SUBJECT_SERVICE_PATTERN: &str = "subject_service";

/// The set of in-process system actors this deployment trusts, compiled once
/// from `AuthZResolverPluginConfig::trusted_system_actors`.
///
/// A match short-circuits the PDP to Allow, skips scope enforcement, and
/// bypasses subject-type classification — so the set is a privilege bypass and
/// is **empty unless configured**. Nothing here is compiled in: which actors
/// exist, and under which subject ids, belongs to the platform the plugin runs
/// in, not to the plugin.
///
/// Both halves must match within the same entry. The subject id is the
/// load-bearing half: it is minted in-process and never issued to a token
/// holder, so a forged `subject_type` alone cannot ride the bypass.
#[derive(Debug, Clone, Default)]
pub struct TrustedSystemActors(Vec<(String, Uuid)>);

impl TrustedSystemActors {
    /// Compile the configured entries. Duplicates are harmless (first match
    /// wins) and no validation is needed beyond what serde already did: an
    /// unparseable UUID fails config loading.
    #[must_use]
    pub fn from_config(entries: &[crate::config::TrustedSystemActor]) -> Self {
        Self(
            entries
                .iter()
                .map(|actor| (actor.subject_type.clone(), actor.subject_id))
                .collect(),
        )
    }

    /// True iff `subject_id` and `subject_type` match the same configured
    /// entry. An absent `subject_type`, or an empty set, is never trusted.
    #[must_use]
    pub fn matches(&self, subject_id: Uuid, subject_type: Option<&str>) -> bool {
        let Some(subject_type) = subject_type else {
            return false;
        };
        self.0
            .iter()
            .any(|(ty, id)| *id == subject_id && ty == subject_type)
    }

    /// How many actors are trusted. Logged once at startup so an operator can
    /// see the bypass surface without reading config back.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` when nothing is trusted — the default, and the state a
    /// deployment stays in unless it names an actor.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Classify a non-empty `subject_type` string into the `PrincipalType` the
/// plugin authorizes.
///
/// Raw `IdP` claim values are matched by exact equality — so a tag like
/// `super_user` (which contains "user") never classifies as `User`. GTS tags
/// are matched by the `subject_*` substring. Probes the more specific service
/// pattern before the user one. Returns `None` for groups and any
/// unrecognized value; callers fail closed.
pub(crate) fn classify_subject_type(subject_type: &str) -> Option<PrincipalType> {
    match subject_type {
        RAW_USER => return Some(PrincipalType::User),
        RAW_SERVICE | RAW_SERVICE_PRINCIPAL => return Some(PrincipalType::ServicePrincipal),
        _ => {}
    }
    if subject_type.contains(SUBJECT_SERVICE_PATTERN) {
        Some(PrincipalType::ServicePrincipal)
    } else if subject_type.contains(SUBJECT_USER_PATTERN) {
        Some(PrincipalType::User)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "subject_type_tests.rs"]
mod tests;
