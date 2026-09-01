#![allow(unknown_lints, de0309_must_have_domain_model)]

//! Deterministic [`PolicyEnforcer`] mock + supporting predicate types.
//! Paths remain reachable via `crate::domain::policy_enforcer::*`
//! through a re-export.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use rbac_sdk::models::{PrincipalType, Scope};
use toolkit_security::SecurityContext;

use super::policy_enforcer::{AuthorizationError, PolicyEnforcer, ReadableScopes};

/// Decision returned by [`MockPolicyEnforcer`] for a matched predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The mock returns `Ok(())`.
    Allow,
    /// The mock returns `Err(AuthorizationError::Denied)`.
    Deny,
    /// The mock returns `Err(AuthorizationError::Internal(_))` — the
    /// enforcer-unreachable path.
    ///
    /// Distinct from [`Decision::Deny`] because the consumers map the
    /// two to different outcomes: `Denied` is collapsed into
    /// `RoleAssignmentNotFound` (a deliberate non-leakage 404) while
    /// `Internal` must surface as a 500. Without this variant no test
    /// could reach the `Internal` arms at all, so a regression routing
    /// an enforcer outage to 404 would not fail any test.
    Internal(String),
}

/// Predicate the mock matches a request against. `None` fields match
/// anything; the predicate matches when every `Some` field equals the
/// corresponding field on the request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchPred {
    pub subject_id: Option<String>,
    pub principal_type: Option<PrincipalType>,
    pub operation: Option<String>,
    pub target_type: Option<String>,
    /// Typed scope predicate. Compared as `Scope::eq`, which
    /// matches the variant + payload UUIDs exactly.
    pub context_scope: Option<Scope>,
}

impl MatchPred {
    fn matches(
        &self,
        subject_id: &str,
        principal_type: PrincipalType,
        operation: &str,
        target_type: &str,
        context_scope: &Scope,
    ) -> bool {
        self.subject_id.as_deref().is_none_or(|s| s == subject_id)
            && self.principal_type.is_none_or(|p| p == principal_type)
            && self.operation.as_deref().is_none_or(|s| s == operation)
            && self.target_type.as_deref().is_none_or(|s| s == target_type)
            && self
                .context_scope
                .as_ref()
                .is_none_or(|s| s == context_scope)
    }
}

/// Predicate over the [`PolicyEnforcer::readable_scopes`] arguments.
/// `None` fields match anything (same convention as [`MatchPred`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadableScopesPred {
    pub subject_id: Option<String>,
    pub principal_type: Option<PrincipalType>,
    pub target_type: Option<String>,
    /// Typed scope predicate (see [`MatchPred::context_scope`]).
    pub context_scope: Option<Scope>,
}

impl ReadableScopesPred {
    fn matches(
        &self,
        subject_id: &str,
        principal_type: PrincipalType,
        target_type: &str,
        context_scope: &Scope,
    ) -> bool {
        self.subject_id.as_deref().is_none_or(|s| s == subject_id)
            && self.principal_type.is_none_or(|p| p == principal_type)
            && self.target_type.as_deref().is_none_or(|s| s == target_type)
            && self
                .context_scope
                .as_ref()
                .is_none_or(|s| s == context_scope)
    }
}

/// Deterministic policy-enforcer mock.
///
/// * [`MockPolicyEnforcer::allow_all`] — every request allowed.
/// * [`MockPolicyEnforcer::deny_all`] — every request denied.
/// * [`MockPolicyEnforcer::match_table`] — first matching predicate
///   wins; closed-posture default is **Deny** when no entry matches.
///
/// `Default` is hand-written rather than derived: the derive was a
/// fourth, unguarded constructor that skipped [`release_build_guard`]
/// entirely. The private `inner` field already blocks a struct literal
/// from outside this module, so routing `default()` through the guard
/// closes the last bypass.
#[derive(Debug)]
pub struct MockPolicyEnforcer {
    inner: Arc<Mutex<MockState>>,
}

impl Default for MockPolicyEnforcer {
    fn default() -> Self {
        release_build_guard();
        Self {
            inner: Arc::new(Mutex::new(MockState::default())),
        }
    }
}

#[derive(Debug, Default)]
struct MockState {
    /// Default decision applied when `entries` is empty or no entry
    /// matches.
    default: Option<Decision>,
    /// Ordered list of `(predicate, decision)` — first matching entry
    /// wins.
    entries: Vec<(MatchPred, Decision)>,
    /// Recorded `(subject_id, principal_type, operation, target_type, context_scope)`
    /// tuples — used by tests to assert that the handler actually
    /// invoked the enforcer.
    calls: Vec<(String, PrincipalType, String, String, Scope)>,
    /// `readable_scopes` decision matrix keyed by
    /// `(subject_id, target_type, context_tenant_id)` — first match wins.
    /// Default when no entry matches is [`ReadableScopes::None`] (closed
    /// posture).
    readable_scopes_table: Vec<(ReadableScopesPred, ReadableScopes)>,
    /// When set, `readable_scopes` returns
    /// `Err(AuthorizationError::Internal(_))` carrying this message and
    /// the table is not consulted. Kept separate from
    /// `readable_scopes_table` so the existing `with_readable_scopes`
    /// signature — used by dozens of call sites — stays unchanged.
    readable_scopes_failure: Option<String>,
}

/// Refuse to construct a `MockPolicyEnforcer` in a release-profile
/// build. The module is gated `#[cfg(any(test, feature = "test-support"))]`,
/// but cargo feature unification means a workspace member depending on
/// `rbac/test-support` would re-export `MockPolicyEnforcer::allow_all`
/// into a release binary. The runtime guard closes the security risk
/// without requiring a separate `rbac-test-support` crate: any test
/// build (`cargo test` / `cargo build` debug profile) has
/// `debug_assertions = true` and the check is a no-op; a release build
/// (`cargo build --release`) panics on first use.
// Intentional fail-loud guard (see fn doc). `manual_assert` is suppressed
// too: converting to `assert!(cfg!(debug_assertions), …)` would trip
// `assertions_on_constants` since the condition is a compile-time const.
#[allow(clippy::panic, clippy::manual_assert)]
fn release_build_guard() {
    if !cfg!(debug_assertions) {
        panic!(
            "rbac::domain::ports::policy_enforcer_mock::MockPolicyEnforcer constructed in a \
             release-profile build \u{2014} this type is a test double and must never reach \
             production. If you need a release-safe permission enforcer use the real \
             PermissionEvaluator (see infra::* wiring) instead."
        );
    }
}

impl MockPolicyEnforcer {
    /// Allow every request.
    #[must_use]
    pub fn allow_all() -> Self {
        release_build_guard();
        Self {
            inner: Arc::new(Mutex::new(MockState {
                default: Some(Decision::Allow),
                entries: Vec::new(),
                calls: Vec::new(),
                readable_scopes_table: Vec::new(),
                readable_scopes_failure: None,
            })),
        }
    }

    /// Deny every request.
    #[must_use]
    pub fn deny_all() -> Self {
        release_build_guard();
        Self {
            inner: Arc::new(Mutex::new(MockState {
                default: Some(Decision::Deny),
                entries: Vec::new(),
                calls: Vec::new(),
                readable_scopes_table: Vec::new(),
                readable_scopes_failure: None,
            })),
        }
    }

    /// Fail every request with `AuthorizationError::Internal(msg)` — the
    /// enforcer-unreachable path.
    ///
    /// Exists so the `Internal` arms of the consuming services are
    /// reachable from a test. They must map to a 500, never to the
    /// `NotFound` that `Denied` deliberately produces.
    #[must_use]
    pub fn internal_all(msg: impl Into<String>) -> Self {
        release_build_guard();
        Self {
            inner: Arc::new(Mutex::new(MockState {
                default: Some(Decision::Internal(msg.into())),
                entries: Vec::new(),
                calls: Vec::new(),
                readable_scopes_table: Vec::new(),
                readable_scopes_failure: None,
            })),
        }
    }

    /// First matching predicate wins; default is `Deny` when no
    /// predicate matches (closed posture).
    #[must_use]
    pub fn match_table(entries: Vec<(MatchPred, Decision)>) -> Self {
        release_build_guard();
        Self {
            inner: Arc::new(Mutex::new(MockState {
                default: Some(Decision::Deny),
                entries,
                calls: Vec::new(),
                readable_scopes_table: Vec::new(),
                readable_scopes_failure: None,
            })),
        }
    }

    /// Install a [`PolicyEnforcer::readable_scopes`] decision matrix on
    /// an existing enforcer. The first matching predicate wins; when no
    /// predicate matches the default is [`ReadableScopes::None`] (closed
    /// posture).
    #[must_use]
    pub fn with_readable_scopes(self, table: Vec<(ReadableScopesPred, ReadableScopes)>) -> Self {
        self.inner.lock().readable_scopes_table = table;
        self
    }

    /// Make [`PolicyEnforcer::readable_scopes`] fail with
    /// `AuthorizationError::Internal(msg)` instead of consulting the
    /// decision matrix.
    ///
    /// The read paths must surface this as a 500. Returning an empty
    /// scope set instead would render as an ordinary empty page, hiding
    /// an enforcer outage as "you may read nothing".
    #[must_use]
    pub fn with_readable_scopes_failure(self, msg: impl Into<String>) -> Self {
        self.inner.lock().readable_scopes_failure = Some(msg.into());
        self
    }

    /// Snapshot of every `(subject_id, principal_type, operation, target_type, context_scope)`
    /// tuple the enforcer observed since construction. Tests use this
    /// to assert that the handler issued the expected check.
    #[must_use]
    pub fn recorded_calls(&self) -> Vec<(String, PrincipalType, String, String, Scope)> {
        self.inner.lock().calls.clone()
    }
}

#[async_trait]
impl PolicyEnforcer for MockPolicyEnforcer {
    async fn enforce(
        &self,
        _ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        operation: &str,
        target_type: &str,
        context_scope: &Scope,
    ) -> Result<(), AuthorizationError> {
        let mut guard = self.inner.lock();
        guard.calls.push((
            subject_id.to_owned(),
            principal_type,
            operation.to_owned(),
            target_type.to_owned(),
            context_scope.clone(),
        ));

        let decision = guard
            .entries
            .iter()
            .find(|(pred, _)| {
                pred.matches(
                    subject_id,
                    principal_type,
                    operation,
                    target_type,
                    context_scope,
                )
            })
            .map(|(_, d)| d.clone())
            .or_else(|| guard.default.clone())
            .unwrap_or(Decision::Deny);

        match decision {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(AuthorizationError::Denied),
            Decision::Internal(msg) => Err(AuthorizationError::Internal(msg)),
        }
    }

    async fn readable_scopes(
        &self,
        _ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        target_type: &str,
        context_scope: &Scope,
    ) -> Result<ReadableScopes, AuthorizationError> {
        let guard = self.inner.lock();
        if let Some(msg) = guard.readable_scopes_failure.as_ref() {
            return Err(AuthorizationError::Internal(msg.clone()));
        }
        let outcome = guard
            .readable_scopes_table
            .iter()
            .find(|(pred, _)| pred.matches(subject_id, principal_type, target_type, context_scope))
            .map_or(ReadableScopes::None, |(_, decision)| decision.clone());
        Ok(outcome)
    }
}
