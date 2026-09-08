//! In-process authorization enforcer abstraction.
//!
//! Handlers consume this small trait; unit tests inject the
//! deterministic `MockPolicyEnforcer` (see `policy_enforcer_mock`).
//! Production is implemented
//! directly on `PermissionEvaluator` (see `permission_evaluator.rs`).

#![allow(unknown_lints, de0309_must_have_domain_model)]

use async_trait::async_trait;
use rbac_sdk::models::{PrincipalType, Scope};
use thiserror::Error;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

/// Authorization failure surface returned by [`PolicyEnforcer::enforce`].
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthorizationError {
    /// The policy enforcer evaluated the request and the caller is
    /// not permitted to perform the action.
    #[error("authorization denied")]
    Denied,
    /// The policy enforcer could not be reached or returned an
    /// internal failure.
    #[error("policy enforcer internal failure: {0}")]
    Internal(String),
}

impl From<AuthorizationError> for crate::domain::error::DomainError {
    fn from(err: AuthorizationError) -> Self {
        match err {
            AuthorizationError::Denied => Self::authorization_denied("authorization denied"),
            AuthorizationError::Internal(msg) => Self::internal(msg),
        }
    }
}

/// In-process authorization enforcer.
///
/// `enforce` MUST return `Ok(())` when the caller is permitted and
/// `Err(AuthorizationError::Denied)` when they are not. Implementations
/// MAY return `AuthorizationError::Internal` for upstream failures
/// (RBAC client unavailable, tenant resolver disconnected, etc.); the
/// caller MUST surface those as 500/503, not as 403, because the
/// denial is not authoritative.
#[async_trait]
pub trait PolicyEnforcer: Send + Sync {
    /// Check whether the subject is permitted to perform `operation`
    /// against `target_type` within `context_scope`.
    ///
    /// `principal_type` MUST identify the caller's principal kind
    /// (`User` / `Group` / `ServicePrincipal`); callers derive it from
    /// [`SecurityContext::subject_type`] via
    /// `principal_type_from_security_context`.
    async fn enforce(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        operation: &str,
        target_type: &str,
        context_scope: &Scope,
    ) -> Result<(), AuthorizationError>;

    /// Compute the set of scopes at which the subject has `read` on
    /// `target_type`. Used by auto-filtered list endpoints to derive a
    /// repository-level `VisibilityFilter`.
    ///
    /// Production implementations MUST invoke `get_subject_roles` with
    /// `include_group_roles = true` so list visibility tracks the same
    /// effective permission set [`Self::enforce`] would grant.
    async fn readable_scopes(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        target_type: &str,
        context_scope: &Scope,
    ) -> Result<ReadableScopes, AuthorizationError>;
}

/// Readable-scope projection returned by [`PolicyEnforcer::readable_scopes`].
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadableScopes {
    /// Caller has read at no scope.
    None,
    /// Caller has read at each listed prefix and its descendants (the
    /// descendant rule matches [`crate::domain::role_assignment_repo::VisibilityFilter::Subtrees`]).
    Subtrees(Vec<String>),
    /// Caller has read at root `/` — every scope is readable.
    Unrestricted,
}

// Mock + supporting predicate types live in [`policy_enforcer_mock`];
// re-exported so existing import paths stay stable. Gated behind the
// `test-support` feature so the test doubles never appear in a release
// artifact.
#[cfg(feature = "test-support")]
pub use crate::domain::policy_enforcer_mock::{
    Decision, MatchPred, MockPolicyEnforcer, ReadableScopesPred,
};

// Production impl: `PermissionEvaluator` implements `PolicyEnforcer`
// directly in `permission_evaluator.rs`. The trait is kept so handler
// tests can use `MockPolicyEnforcer` without building a full
// role-assignment / definition / resolver / RG graph.

#[cfg(test)]
#[path = "policy_enforcer_tests.rs"]
mod policy_enforcer_tests;
