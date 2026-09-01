//! Typed metric labels + port trait for the `AuthZ` resolver plugin.
//!
//! Canon parity (mirrors the `account-management` / `vp-idp-plugin` pattern):
//! every label value comes from a closed enum (`as_str()` → `snake_case`) so
//! metric cardinality is bounded at compile time. The infra adapter
//! (`crate::infra::metrics::AuthZMetrics`) implements `AuthzMetricsPort`;
//! `NoopMetrics` is provided for canon parity.

use toolkit_macros::domain_model;

/// Final decision label on `authz.evaluation_duration`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Deny,
    Error,
}

impl Decision {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Error => "error",
        }
    }
}

/// `reason` label on `authz.evaluation_deny`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DenyReason {
    NoPermission,
    ScopeMismatch,
    UnknownResourceType,
    UnsupportedProperty,
    ConstraintsUnavailable,
    ExpansionInfeasible,
    InvalidRequest,
    Unknown,
}

impl DenyReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NoPermission => "no_permission",
            Self::ScopeMismatch => "scope_mismatch",
            Self::UnknownResourceType => "unknown_resource_type",
            Self::UnsupportedProperty => "unsupported_property",
            Self::ConstraintsUnavailable => "constraints_unavailable",
            Self::ExpansionInfeasible => "expansion_infeasible",
            // Same label the `ErrorType` client-fault classification uses: the
            // deny moved off `evaluation_error` onto `evaluation_deny`, but the
            // classification a dashboard filters on did not change.
            Self::InvalidRequest => "invalid_request",
            Self::Unknown => "unknown",
        }
    }
}

/// `error_type` label on `authz.evaluation_error`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorType {
    RbacUnavailable,
    RbacScopeProvenanceInvalid,
    TenantResolverTimeout,
    RgResolverTimeout,
    GtsRegistryUnavailable,
    TenantResolverNotFound,
    RgResolverNotFound,
    InvalidRequest,
    Unexpected,
}

impl ErrorType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RbacUnavailable => "rbac_unavailable",
            Self::RbacScopeProvenanceInvalid => "rbac_scope_provenance_invalid",
            Self::TenantResolverTimeout => "tenant_resolver_timeout",
            Self::RgResolverTimeout => "rg_resolver_timeout",
            Self::GtsRegistryUnavailable => "gts_registry_unavailable",
            Self::TenantResolverNotFound => "tenant_resolver_not_found",
            Self::RgResolverNotFound => "rg_resolver_not_found",
            Self::InvalidRequest => "invalid_request",
            Self::Unexpected => "unexpected",
        }
    }
}

/// `reason` label on `authz.fail_closed`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailClosedReason {
    ConstraintsUnavailable,
    RbacUnavailable,
    RbacScopeProvenanceInvalid,
    ResolverTimeout,
    GtsRegistryUnavailable,
    ScopeUnresolvable,
    AllConstraintsFailed,
}

impl FailClosedReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ConstraintsUnavailable => "constraints_unavailable",
            Self::RbacUnavailable => "rbac_unavailable",
            Self::RbacScopeProvenanceInvalid => "rbac_scope_provenance_invalid",
            Self::ResolverTimeout => "resolver_timeout",
            Self::GtsRegistryUnavailable => "gts_registry_unavailable",
            Self::ScopeUnresolvable => "scope_unresolvable",
            Self::AllConstraintsFailed => "all_constraints_failed",
        }
    }
}

/// `scope_type` label on `authz.evaluation_by_scope_type` and
/// `authz.constraint_compilation_duration`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeTypeLabel {
    Global,
    TenantSubtree,
    TenantDirect,
    GroupSubtree,
    ExplicitGroups,
    Combined,
    Other,
}

impl ScopeTypeLabel {
    pub(crate) const fn as_str(self) -> &'static str {
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

/// `resolver` label on `authz.hierarchy_query_duration`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolver {
    Tenant,
    Rg,
}

impl Resolver {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Rg => "rg",
        }
    }
}

/// `operation` label on `authz.hierarchy_query_duration`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HierarchyOp {
    SubtreeIds,
    GroupSubtree,
}

impl HierarchyOp {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SubtreeIds => "subtree_ids",
            Self::GroupSubtree => "group_subtree",
        }
    }
}

/// `operation` label on `authz.rbac_query_duration`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RbacOp {
    EvaluatePermission,
}

impl RbacOp {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::EvaluatePermission => "evaluate_permission",
        }
    }
}

/// `operation` label on `authz.token_scope_narrowing`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NarrowingOp {
    Read,
    Write,
    Delete,
    List,
    Get,
    Create,
    Update,
    Start,
    Stop,
    Restart,
    Other,
}

impl NarrowingOp {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::List => "list",
            Self::Get => "get",
            Self::Create => "create",
            Self::Update => "update",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Other => "other",
        }
    }

    /// Bounded mapping from a raw request action name.
    pub(crate) fn from_action(operation: &str) -> Self {
        match operation {
            "read" => Self::Read,
            "write" => Self::Write,
            "delete" => Self::Delete,
            "list" => Self::List,
            "get" => Self::Get,
            "create" => Self::Create,
            "update" => Self::Update,
            "start" => Self::Start,
            "stop" => Self::Stop,
            "restart" => Self::Restart,
            _ => Self::Other,
        }
    }
}

/// `cache_type` label on `authz.evaluation_cache_hit_ratio`.
///
/// Lives in the domain port (not infra) so the `AuthzMetricsPort` trait
/// references only domain types — the infra adapter maps these to its
/// atomic hit/total packing. The string values are pinned to the same
/// labels emitted today.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheKind {
    TenantSubtree,
    TenantMeta,
    GroupSubtree,
    GroupMembers,
}

impl CacheKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TenantSubtree => "tenant_subtree",
            Self::TenantMeta => "tenant_meta",
            Self::GroupSubtree => "group_subtree",
            Self::GroupMembers => "group_members",
        }
    }
}

/// Behavioural port for the `AuthZ` metrics sink. Implemented by the
/// OpenTelemetry adapter [`crate::infra::metrics::AuthZMetrics`]; [`NoopMetrics`] is the
/// canon-parity no-op. Every argument is a domain type so the trait holds no
/// dependency on the infra layer.
///
/// The plugin's components hold the concrete `Arc<AuthZMetrics>` handle
/// directly, so the trait currently has no production caller — it exists for
/// canon parity (the `account-management` / `vp-idp-plugin` typed-port shape)
/// and to make [`NoopMetrics`] a drop-in. Like `vp-idp-plugin`'s `NoopMetrics`,
/// it is therefore `#[cfg(test)]`-gated until a DI call site materializes; the
/// adapter's trait impl in `infra::metrics` is gated the same way. When a
/// component switches to `Arc<dyn AuthzMetricsPort>`, drop both gates.
#[cfg(test)]
pub(crate) trait AuthzMetricsPort: Send + Sync + 'static {
    fn record_outcome(
        &self,
        elapsed: std::time::Duration,
        result: &Result<authz_resolver_sdk::EvaluationResponse, crate::domain::error::PluginError>,
    );
    fn record_scope_type(&self, scope_type: ScopeTypeLabel);
    fn record_cache_access(&self, kind: CacheKind, hit: bool);
    fn record_hierarchy_query(
        &self,
        resolver: Resolver,
        operation: HierarchyOp,
        duration: std::time::Duration,
    );
    fn record_rbac_query(&self, operation: RbacOp, duration: std::time::Duration);
    fn record_constraint_compilation(
        &self,
        scope_type: ScopeTypeLabel,
        duration: std::time::Duration,
    );
    fn inc_unsupported_property(&self);
    fn inc_scope_provenance_rejection(&self);
    fn inc_token_scope_narrowing(&self, operation: NarrowingOp);
    fn inc_barrier_mode_override(&self);
    fn inc_capability_negotiation(&self, capabilities: &str);
}

/// Canon-parity no-op metrics sink. See [`AuthzMetricsPort`] for why this is
/// `#[cfg(test)]`-gated.
#[cfg(test)]
#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct NoopMetrics;

#[cfg(test)]
impl AuthzMetricsPort for NoopMetrics {
    fn record_outcome(
        &self,
        _: std::time::Duration,
        _: &Result<authz_resolver_sdk::EvaluationResponse, crate::domain::error::PluginError>,
    ) {
    }
    fn record_scope_type(&self, _: ScopeTypeLabel) {}
    fn record_cache_access(&self, _: CacheKind, _: bool) {}
    fn record_hierarchy_query(&self, _: Resolver, _: HierarchyOp, _: std::time::Duration) {}
    fn record_rbac_query(&self, _: RbacOp, _: std::time::Duration) {}
    fn record_constraint_compilation(&self, _: ScopeTypeLabel, _: std::time::Duration) {}
    fn inc_unsupported_property(&self) {}
    fn inc_scope_provenance_rejection(&self) {}
    fn inc_token_scope_narrowing(&self, _: NarrowingOp) {}
    fn inc_barrier_mode_override(&self) {}
    fn inc_capability_negotiation(&self, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_values_match_legacy_strings() {
        assert_eq!(Decision::Allow.as_str(), "allow");
        assert_eq!(
            DenyReason::ConstraintsUnavailable.as_str(),
            "constraints_unavailable"
        );
        assert_eq!(
            ErrorType::TenantResolverNotFound.as_str(),
            "tenant_resolver_not_found"
        );
        assert_eq!(
            ErrorType::RbacScopeProvenanceInvalid.as_str(),
            "rbac_scope_provenance_invalid"
        );
        assert_eq!(
            FailClosedReason::ScopeUnresolvable.as_str(),
            "scope_unresolvable"
        );
        assert_eq!(
            FailClosedReason::RbacScopeProvenanceInvalid.as_str(),
            "rbac_scope_provenance_invalid"
        );
        assert_eq!(ScopeTypeLabel::ExplicitGroups.as_str(), "explicit_groups");
        assert_eq!(Resolver::Rg.as_str(), "rg");
        assert_eq!(HierarchyOp::SubtreeIds.as_str(), "subtree_ids");
        assert_eq!(RbacOp::EvaluatePermission.as_str(), "evaluate_permission");
        assert_eq!(NarrowingOp::from_action("frobnicate").as_str(), "other");
        assert_eq!(NarrowingOp::from_action("read").as_str(), "read");
        assert_eq!(CacheKind::TenantSubtree.as_str(), "tenant_subtree");
        assert_eq!(CacheKind::GroupMembers.as_str(), "group_members");
    }

    /// `NoopMetrics` is a drop-in `AuthzMetricsPort` (canon parity): the trait
    /// is object-safe and every method is a no-op. Exercises the whole port so
    /// it stays a real, callable contract rather than dead code.
    #[test]
    fn noop_metrics_is_a_drop_in_port() {
        use std::sync::Arc;
        use std::time::Duration;

        use authz_resolver_sdk::EvaluationResponse;

        use crate::domain::error::PluginError;

        let sink: Arc<dyn AuthzMetricsPort> = Arc::new(NoopMetrics);
        let allow: Result<EvaluationResponse, PluginError> =
            Err(PluginError::internal("no plugin available"));
        sink.record_outcome(Duration::from_millis(1), &allow);
        sink.record_scope_type(ScopeTypeLabel::Global);
        sink.record_cache_access(CacheKind::TenantSubtree, true);
        sink.record_hierarchy_query(Resolver::Tenant, HierarchyOp::SubtreeIds, Duration::ZERO);
        sink.record_rbac_query(RbacOp::EvaluatePermission, Duration::ZERO);
        sink.record_constraint_compilation(ScopeTypeLabel::Combined, Duration::ZERO);
        sink.inc_unsupported_property();
        sink.inc_scope_provenance_rejection();
        sink.inc_token_scope_narrowing(NarrowingOp::Read);
        sink.inc_barrier_mode_override();
        sink.inc_capability_negotiation("tenant_hierarchy");
    }
}
