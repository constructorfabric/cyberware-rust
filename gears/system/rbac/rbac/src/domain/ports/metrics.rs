//! Typed metrics port for the RBAC permission evaluator.
//!
//! Mirrors the canonical `account-management` pattern: label values come from
//! closed enums (`as_str()` → `snake_case`) so metric cardinality is bounded at
//! compile time. The infra adapter (`crate::infra::metrics`) implements
//! [`PermissionMetricsPort`]; [`NoopMetrics`] is the safe default for unit
//! tests and any construction before an exporter is wired.

use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{DenyReason, PermissionScopeType};
use toolkit_macros::domain_model;

/// Outcome class of one `evaluate_permission` call.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalResult {
    Allow,
    Deny,
    Error,
}

impl EvalResult {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Error => "error",
        }
    }
}

/// Categorical deny-reason label (mirrors `rbac_sdk::models::DenyReason`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalDenyReason {
    NoMatchingPermission,
    NotPermissionExclusion,
}

impl EvalDenyReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoMatchingPermission => "no_matching_permission",
            Self::NotPermissionExclusion => "not_permission_exclusion",
        }
    }
}

impl From<DenyReason> for EvalDenyReason {
    fn from(r: DenyReason) -> Self {
        match r {
            DenyReason::NotPermissionExclusion => Self::NotPermissionExclusion,
            // `DenyReason::NoMatchingPermission` plus any future
            // (`#[non_exhaustive]`) variant: fail-closed default.
            _ => Self::NoMatchingPermission,
        }
    }
}

/// Aggregated scope-type discriminant of an allowed evaluation (no UUIDs —
/// only the variant name, to keep cardinality bounded).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalScopeType {
    Global,
    TenantSubtree,
    TenantDirect,
    GroupSubtree,
    ExplicitGroups,
    Combined,
    Other,
}

impl EvalScopeType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::TenantSubtree => "tenant_subtree",
            Self::TenantDirect => "tenant_direct",
            Self::GroupSubtree => "group_subtree",
            Self::ExplicitGroups => "explicit_groups",
            Self::Combined => "combined",
            Self::Other => "other",
        }
    }
}

impl From<&PermissionScopeType> for EvalScopeType {
    fn from(s: &PermissionScopeType) -> Self {
        match s {
            PermissionScopeType::Global => Self::Global,
            PermissionScopeType::TenantSubtree { .. } => Self::TenantSubtree,
            PermissionScopeType::TenantDirect { .. } => Self::TenantDirect,
            PermissionScopeType::GroupSubtree { .. } => Self::GroupSubtree,
            PermissionScopeType::ExplicitGroups { .. } => Self::ExplicitGroups,
            PermissionScopeType::Combined { .. } => Self::Combined,
            // `PermissionScopeType` is #[non_exhaustive].
            _ => Self::Other,
        }
    }
}

/// Error class bucketed from `RbacServiceError` on the evaluate hot path.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalErrorType {
    Validation,
    DependencyUnavailable,
    InvalidStoredScope,
    Internal,
}

impl EvalErrorType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Validation => "validation",
            Self::DependencyUnavailable => "dependency_unavailable",
            Self::InvalidStoredScope => "invalid_stored_scope",
            Self::Internal => "internal",
        }
    }
}

impl From<&RbacServiceError> for EvalErrorType {
    fn from(e: &RbacServiceError) -> Self {
        match e {
            RbacServiceError::Validation { .. } => Self::Validation,
            RbacServiceError::DependencyUnavailable { .. } => Self::DependencyUnavailable,
            RbacServiceError::InvalidStoredScope { .. } => Self::InvalidStoredScope,
            // Everything else surfacing on the evaluate path buckets as internal.
            _ => Self::Internal,
        }
    }
}

/// External dependency a query targets.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dependency {
    TenantResolver,
    ResourceGroup,
}

impl Dependency {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TenantResolver => "tenant_resolver",
            Self::ResourceGroup => "resource_group",
        }
    }
}

/// Operation performed against a dependency.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyOp {
    GetAncestors,
    ListMemberships,
}

impl DependencyOp {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GetAncestors => "get_ancestors",
            Self::ListMemberships => "list_memberships",
        }
    }
}

#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyOutcome {
    Success,
    NotFound,
    Error,
}

impl DependencyOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NotFound => "not_found",
            Self::Error => "error",
        }
    }
}

/// Which name on a role-assignment row a display-name resolution was
/// attempted for — the three principal kinds, the row's author, and the
/// granted role definition.
///
/// Categorical by construction: the label carries the *kind*, never the
/// principal id, name or tenant. A display-name feature must not turn the
/// metrics endpoint into an identity side-channel.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameKind {
    User,
    Group,
    /// The row's author (`created_by`), resolved as a user.
    Author,
    /// The granted role definition, resolved from RBAC's own
    /// `role_definitions` table. Kept as its own kind because it is the
    /// only name on the row that needs no upstream gear: a spike in its
    /// `degraded` count means RBAC's own database is unhappy, which is a
    /// different page for the operator than "account management is down".
    RoleDefinition,
    /// A principal kind no reader answers for: `ServicePrincipal` today
    /// (the platform has no `subject_id` to `client_id` reverse lookup),
    /// and any kind added to the enum later. Deliberately not folded into
    /// `User` — a row that was never going to be named must not read as a
    /// failed user lookup.
    Other,
}

impl NameKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
            Self::Author => "author",
            Self::RoleDefinition => "role_definition",
            Self::Other => "other",
        }
    }
}

/// Outcome of a display-name resolution attempt, counted per principal
/// rather than per upstream call — "how many rows on this page came out
/// named" is the operator's actual question.
///
/// There is deliberately no `cached` outcome: the cache lives inside the
/// infra reader, below the port that reports these, so a cache hit is
/// indistinguishable from a fresh resolve at the point of counting.
/// Reporting one would be a guess.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameOutcome {
    /// A name was produced.
    Resolved,
    /// No name: upstream failed, denied the read, or simply had no name
    /// for that id. All three are the same thing to a reader — the id is
    /// served without a name — and the distinguishing detail is in the
    /// logs, not in a label.
    Degraded,
    /// No upstream can name this kind (a service principal has no
    /// `subject_id -> client_id` reverse lookup on the platform), so no
    /// attempt was made. Distinct from `Degraded` so a dashboard does not
    /// read a permanent platform gap as an outage.
    Unsupported,
}

impl NameOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Degraded => "degraded",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Metrics sink for display-name resolution on the role-assignment read
/// path.
///
/// Deliberately separate from [`PermissionMetricsPort`]: name resolution
/// is not permission evaluation, and the hydrator has no business being
/// able to emit eval-latency samples. The infra `OTel` adapter implements
/// both; [`NoopMetrics`] is the default for tests.
pub trait PrincipalNameMetricsPort: Send + Sync + 'static {
    /// Count `count` principals resolved (or not) for `kind`. A `count`
    /// of zero MUST NOT emit a sample.
    fn principal_name_resolve(&self, kind: NameKind, outcome: NameOutcome, count: u64);
}

/// Metrics sink for the permission evaluator. Implemented by the infra `OTel`
/// adapter; [`NoopMetrics`] is the default for tests / pre-init.
pub trait PermissionMetricsPort: Send + Sync + 'static {
    /// Record one `evaluate_permission` return: latency + outcome class.
    fn permission_eval_duration(&self, result: EvalResult, secs: f64);
    /// Increment the deny counter for a denied evaluation.
    fn permission_deny(&self, reason: EvalDenyReason);
    /// Increment the per-scope-type counter for an allowed evaluation.
    fn permission_allow_scope_type(&self, scope_type: EvalScopeType);
    /// Increment the error counter for a failed evaluation.
    fn permission_eval_error(&self, error: EvalErrorType);
    /// Record `get_subject_roles` latency.
    fn subject_roles_duration(&self, include_group_roles: bool, secs: f64);
    /// Record a dependency call's latency + outcome (both the duration
    /// histogram and the health counter).
    fn dependency_query(
        &self,
        dep: Dependency,
        op: DependencyOp,
        outcome: DependencyOutcome,
        secs: f64,
    );
}

/// No-op metrics. Used by unit tests and any construction before an exporter
/// is wired.
#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl PermissionMetricsPort for NoopMetrics {
    fn permission_eval_duration(&self, _: EvalResult, _: f64) {}
    fn permission_deny(&self, _: EvalDenyReason) {}
    fn permission_allow_scope_type(&self, _: EvalScopeType) {}
    fn permission_eval_error(&self, _: EvalErrorType) {}
    fn subject_roles_duration(&self, _: bool, _: f64) {}
    fn dependency_query(&self, _: Dependency, _: DependencyOp, _: DependencyOutcome, _: f64) {}
}

impl PrincipalNameMetricsPort for NoopMetrics {
    fn principal_name_resolve(&self, _: NameKind, _: NameOutcome, _: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_strings_are_snake_case() {
        assert_eq!(EvalResult::Deny.as_str(), "deny");
        assert_eq!(
            EvalDenyReason::NotPermissionExclusion.as_str(),
            "not_permission_exclusion"
        );
        assert_eq!(EvalScopeType::GroupSubtree.as_str(), "group_subtree");
        assert_eq!(
            EvalErrorType::DependencyUnavailable.as_str(),
            "dependency_unavailable"
        );
        assert_eq!(Dependency::TenantResolver.as_str(), "tenant_resolver");
        assert_eq!(DependencyOp::ListMemberships.as_str(), "list_memberships");
        assert_eq!(DependencyOutcome::NotFound.as_str(), "not_found");
        assert_eq!(NameKind::Group.as_str(), "group");
        assert_eq!(NameKind::Other.as_str(), "other");
        assert_eq!(NameKind::RoleDefinition.as_str(), "role_definition");
        assert_eq!(NameOutcome::Unsupported.as_str(), "unsupported");
    }

    #[test]
    fn scope_type_from_discriminant_drops_uuids() {
        let s = PermissionScopeType::TenantSubtree {
            root_tenant_id: uuid::Uuid::nil(),
        };
        assert_eq!(EvalScopeType::from(&s), EvalScopeType::TenantSubtree);
    }

    #[test]
    fn deny_reason_maps() {
        assert_eq!(
            EvalDenyReason::from(DenyReason::NoMatchingPermission),
            EvalDenyReason::NoMatchingPermission
        );
    }
}
