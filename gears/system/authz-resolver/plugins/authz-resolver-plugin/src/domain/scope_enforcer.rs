//! OAuth token-scope enforcement — first real evaluation step after request
//! validation.
//!
//! Owns three short-circuit behaviors:
//! - **Empty `token_scopes`** → fail-closed deny with `scope_mismatch.v1`.
//! - **Wildcard scope** (configured `wildcard_scope`, default `"*"`) → pass
//!   immediately without operation-to-scope mapping.
//! - **Third-party intersection** → map `action.name` to a scope class via
//!   `operation_to_scope`, then — only when the id is not mapped verbatim —
//!   derive a class from the id's boundary verb ([`derive_scope_class`]), and
//!   only then fall back to `default_unmapped_scope`. Require at least one
//!   token scope to match the resulting class. Match is satisfied by exact
//!   equality OR `<class> + ":"` prefix (OAuth-style namespaced scopes) — pure
//!   prefix would let `"reader"` satisfy `"read"`.
//!
//! `default_unmapped_scope` is the only fallback, and every path that cannot
//! produce a class reaches it: the derivation returns `None` rather than
//! inventing one, including for a boundary it recognizes as mutating but cannot
//! classify. Nothing here hardcodes a class name, so a deployment that
//! configures a stricter (or differently named) fallback gets it applied
//! uniformly.
//!
//! Scope enforcement is **not** the security source of truth; RBAC remains
//! authoritative. A passing scope check only continues evaluation; RBAC may
//! still deny.

use std::collections::HashMap;
use std::sync::Arc;

use authz_resolver_sdk::models::EvaluationResponse;
use tracing::debug;

use crate::config::{AuthZResolverPluginConfig, MUTATING_BOUNDARY_VERBS};
use crate::domain::deny::{build_deny_response, error_codes};
use toolkit_macros::domain_model;

#[domain_model]
pub(crate) struct ScopeEnforcer {
    config: Arc<AuthZResolverPluginConfig>,
}

impl ScopeEnforcer {
    pub(crate) fn new(config: Arc<AuthZResolverPluginConfig>) -> Self {
        Self { config }
    }

    /// Run the three-stage scope check. `Ok(())` means evaluation can
    /// proceed to the next pipeline step; `Err(response)` carries the
    /// fully-built deny response the caller should return verbatim.
    // Three sequential scope-check stages with distinct deny reasons.
    #[allow(clippy::cognitive_complexity)]
    pub(crate) fn check_scopes(
        &self,
        token_scopes: &[String],
        action_name: &str,
    ) -> Result<(), EvaluationResponse> {
        let scope_cfg = &self.config.scope_enforcement;

        // 1. Empty scopes deny fail-closed — runs first so the intent is
        //    obvious to a reviewer (an empty Vec cannot contain "*", so the
        //    explicit order is purely for code clarity, not behavior).
        if token_scopes.is_empty() {
            debug!(
                action = action_name,
                token_scopes_count = 0_usize,
                result = "denied",
                reason = "empty_token_scopes",
                "scope check"
            );
            return Err(build_deny_response(
                error_codes::SCOPE_MISMATCH_V1,
                Some(format!(
                    "no token scopes presented; cannot authorize operation '{action_name}'"
                )),
            ));
        }

        // 2. Wildcard short-circuit.
        if token_scopes.iter().any(|s| s == &scope_cfg.wildcard_scope) {
            debug!(
                action = action_name,
                token_scopes_count = token_scopes.len(),
                result = "allowed",
                reason = "wildcard",
                "scope check"
            );
            return Ok(());
        }

        // 3. Map action → scope class, then match. Three ordered sources, most
        //    specific first, so an operator can always pin an exact id and get
        //    the derivation out of the way.
        let (scope_class, scope_class_source): (&str, &str) =
            match scope_cfg.operation_to_scope.get(action_name) {
                Some(mapped) => (mapped.as_str(), "mapped"),
                None => derive_scope_class(&scope_cfg.operation_to_scope, action_name).map_or(
                    (scope_cfg.default_unmapped_scope.as_str(), "default"),
                    |derived| (derived, "derived"),
                ),
            };

        if token_scopes
            .iter()
            .any(|token| scope_class_matches(scope_class, token))
        {
            debug!(
                action = action_name,
                scope_class,
                scope_class_source,
                token_scopes_count = token_scopes.len(),
                result = "allowed",
                reason = "class_match",
                "scope check"
            );
            return Ok(());
        }

        debug!(
            action = action_name,
            scope_class,
            scope_class_source,
            token_scopes_count = token_scopes.len(),
            result = "denied",
            reason = "class_mismatch",
            "scope check"
        );
        Err(build_deny_response(
            error_codes::SCOPE_MISMATCH_V1,
            Some(format!(
                "token scopes do not authorize operation '{action_name}' \
                 (required scope class '{scope_class}')"
            )),
        ))
    }
}

/// Derive a scope class for a **compound** operation id from the verb at one
/// of its boundaries, using the same operator-configurable `operation_to_scope`
/// map. Returns `None` when nothing can be derived, leaving the caller on
/// `default_unmapped_scope`. See `docs/DESIGN.md` §3.4.
///
/// # Why derive at all
///
/// The flat per-id map is the whole enforcement input, which works for the
/// platform's closed verb vocabulary but not for data-plane ids declared by
/// adapter manifests (`list_objects`, `signed_url_write`): open, unbounded, and
/// shared across adapters that may mean opposite things by the same id. Without
/// derivation each such id resolves to `write`, denying a read-only caller a
/// read-only operation before RBAC is consulted; relaxing
/// `default_unmapped_scope` instead would relax it platform-wide.
///
/// # The rule
///
/// Only the **first and last** `-`/`_` separated segments count, and when both
/// are recognized they must agree; disagreement (`read_things_delete`) yields
/// `None`. Interior segments are ignored on purpose — data-plane ids put the
/// effect verb at a boundary, so reading the middle would only let a
/// destructive id be talked down to `read` by a word in its own name.
///
/// # The residual
///
/// One boundary recognized and the other not is weak in exactly one direction:
/// a recognized read verb opposite the id's real, unrecognized effect.
/// `read_replica_create` has that shape and is structurally indistinguishable
/// from the genuine read `list_access_keys`, so only *recognizing* the mutating
/// verb separates them. Hence [`MUTATING_BOUNDARY_VERBS`] is read here directly
/// rather than through the map: a caller-supplied `operation_to_scope` that
/// omits a mutating verb cannot weaken the rule. A marker verb carries no class
/// of its own — it forces the disagreement that lands the id back on
/// `default_unmapped_scope` — and naming it in `operation_to_scope` overrides
/// the marker, since a map hit wins first.
///
/// What remains is bounded: the unrecognized verb must sit opposite a read verb
/// to matter; the rule can only move an id *off* the fallback; an operator can
/// pin any id; and RBAC still has to allow the operation.
fn derive_scope_class<'cfg>(
    operation_to_scope: &'cfg HashMap<String, String>,
    action_name: &str,
) -> Option<&'cfg str> {
    let mut segments = action_name.split(['-', '_']).filter(|s| !s.is_empty());
    let first = segments.next()?;
    // `None` for a single-segment id: the caller already looked the whole id up
    // verbatim and missed, so re-testing the one segment cannot succeed.
    let last = segments.next_back();

    let head = classify_boundary(operation_to_scope, Some(first));
    let tail = classify_boundary(operation_to_scope, last);

    // Only `Boundary::Class` can ever produce a class, so the two positive arms
    // below name it explicitly and everything else refuses. `Boundary::Mutating`
    // therefore needs no arm of its own: it matches neither positive pattern —
    // notably not the one-sided arm, which requires a literal `None` on the far
    // side — so it drops through to the refusal, which is exactly the
    // `read_replica_create` behaviour it exists for.
    match (head, tail) {
        // Both boundaries classified by the map and in agreement — the confident
        // case.
        (Some(Boundary::Class(head_class)), Some(Boundary::Class(tail_class)))
            if head_class == tail_class =>
        {
            Some(head_class)
        }
        // Exactly one boundary classified and the other unrecognized entirely: it
        // is the only signal there is. A `Mutating` far side is a signal rather
        // than an absence, and must not be talked down by this arm.
        (Some(Boundary::Class(only)), None) | (None, Some(Boundary::Class(only))) => Some(only),
        // Refuse and leave the caller on `default_unmapped_scope`: classified but
        // contradictory, either boundary mutating-without-a-class, or nothing
        // recognized at all. A wildcard rather than an enumeration so that a
        // future `Boundary` variant fails closed instead of deriving a class by
        // accident.
        _ => None,
    }
}

/// What a boundary segment tells the derivation.
///
/// Separate from `Option<&str>` because "mutating, class unknown" is a third
/// answer and not a missing one: it has to *contradict* a read boundary rather
/// than yield to it, which a plain absence cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary<'cfg> {
    /// The map classifies this segment. An operator entry lands here, so naming a
    /// mutating verb in `operation_to_scope` overrides the marker below.
    Class(&'cfg str),
    /// Recognized as a mutating boundary verb, with no class from the map.
    Mutating,
}

/// Resolve one boundary segment, map first so an operator entry always wins.
fn classify_boundary<'cfg>(
    operation_to_scope: &'cfg HashMap<String, String>,
    segment: Option<&str>,
) -> Option<Boundary<'cfg>> {
    let segment = segment?;
    if let Some(class) = operation_to_scope.get(segment) {
        return Some(Boundary::Class(class.as_str()));
    }
    MUTATING_BOUNDARY_VERBS
        .contains(&segment)
        .then_some(Boundary::Mutating)
}

/// Returns `true` when `token` is exactly the scope class, or starts with
/// `<scope_class>:` (OAuth-style sub-scope). Prevents `"reader"` /
/// `"readonly"` from satisfying scope class `"read"` while still letting
/// `"read:events"` and `"read:*"` match it.
fn scope_class_matches(scope_class: &str, token: &str) -> bool {
    token == scope_class
        || token
            .strip_prefix(scope_class)
            .is_some_and(|rest| rest.starts_with(':'))
}

#[cfg(test)]
#[path = "scope_enforcer_tests.rs"]
mod tests;
