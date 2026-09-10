//! Hard session-ownership invariant for authenticated point-ops.
//!
//! The PEP passes the prefetched owner pair to the PDP as ABAC input, but no
//! shipped policy plugin emits an `owner_id` predicate — `static-authz` and
//! `tr-authz` both constrain `owner_tenant_id` only. A tenant-only scope lets
//! any authenticated subject read or mutate another subject's session, which
//! NFR-006 forbids: "Session access must be restricted to the owning user
//! (`user_id` match) within the owning tenant (`tenant_id` match), or to share
//! token holders for read-only access."
//!
//! [`ensure_session_owner`] restores that invariant inside the gear. It runs on
//! the trusted prefetch, before the PDP decision, so the policy engine can only
//! ever narrow access further — never widen it. Share-token access is a
//! separate, unauthenticated route (`ExportService::access_shared`) and does
//! not pass through this guard.
//!
//! A mismatch is reported as `NotFound`, not `Forbidden`, so a probing caller
//! cannot distinguish "someone else's session" from "no such session"
//! (anti-enumeration, ADR-0021).
//
// @cpt-cf-chat-engine-nfr-authentication
// @cpt-cf-chat-engine-design-auth-model

use toolkit_security::{
    AccessScope, ScopeConstraint, ScopeFilter, SecurityContext, pep_properties,
};
use tracing::warn;
use uuid::Uuid;

use crate::domain::error::{ChatEngineError, Result};
use crate::domain::session::Session;

/// Fail closed unless `ctx` is the owner of `session`.
///
/// Both halves of the owner pair are compared as UUIDs: since migration
/// `m20260417_000006_authz_owner_columns` every persisted owner value is
/// UUID-formatted, so a value that fails to parse is a corrupt row and is
/// treated as a mismatch rather than falling back to a string comparison that
/// could match on a differently-cased or padded value.
///
/// # Errors
///
/// [`ChatEngineError::NotFound`] when the caller is not the owning user of the
/// owning tenant.
pub fn ensure_session_owner(ctx: &SecurityContext, session: &Session) -> Result<()> {
    let owner_id = Uuid::parse_str(session.user_id.as_str()).ok();
    let owner_tenant_id = Uuid::parse_str(session.tenant_id.as_str()).ok();
    // Bound once and reused by both the comparison and the denial log: a
    // method call inside `warn!` is also evaluated in a macro branch that a
    // normal subscriber never takes, which reads as dead code to coverage.
    let subject_id = ctx.subject_id();
    let subject_tenant_id = ctx.subject_tenant_id();

    if owner_id == Some(subject_id) && owner_tenant_id == Some(subject_tenant_id) {
        return Ok(());
    }

    warn!(
        session_id = %session.session_id,
        %subject_id,
        %subject_tenant_id,
        "session ownership check failed - responding 404 (anti-enumeration)",
    );
    Err(ChatEngineError::not_found("session", session.session_id))
}

/// Narrow a PDP-compiled scope to the calling subject's own rows.
///
/// The row-scoped counterpart of [`ensure_session_owner`], for the operations
/// that never prefetch a session — they hand an `AccessScope` straight to the
/// repository (`find_message_by_id_scoped`, `fetch_active_history_scoped`,
/// `delete_message_subtree_scoped`). Post-filtering the result is not an option
/// there: it would silently corrupt paging on the list paths, so the caller
/// clamp has to travel with the scope into the SQL `WHERE`.
///
/// Both halves of the owner pair are pinned:
///
/// - `owner_id` via the platform's [`AccessScope::ensure_owner`] — injected
///   where the PDP left it open, intersected where the PDP set it (a policy
///   that grants someone else's rows loses that constraint, and a scope with
///   nothing left over becomes deny-all).
/// - `owner_tenant_id` is injected only where a constraint carries no tenant
///   filter of its own. A PDP that already scoped the tenant keeps its own
///   predicate, which may legitimately be a subtree rather than a single id.
///
/// Deny-all and unconstrained scopes are handled by `ensure_owner`: deny-all
/// stays deny-all, and an unconstrained allow collapses to the caller's own
/// owner pair rather than to every row in the table.
#[must_use]
pub fn caller_scope(ctx: &SecurityContext, scope: &AccessScope) -> AccessScope {
    let owned = scope.ensure_owner(ctx.subject_id());
    if owned.is_deny_all() {
        return owned;
    }

    let tenant_filter = ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id());
    let constraints = owned
        .constraints()
        .iter()
        .map(|constraint| {
            if constraint
                .filters()
                .iter()
                .any(|f| f.property() == pep_properties::OWNER_TENANT_ID)
            {
                return constraint.clone();
            }
            let mut filters = constraint.filters().to_vec();
            filters.push(tenant_filter.clone());
            ScopeConstraint::new(filters)
        })
        .collect();
    AccessScope::from_constraints(constraints)
}

#[cfg(test)]
#[path = "owner_guard_tests.rs"]
mod owner_guard_tests;
