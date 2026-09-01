//! Caller-authority → [`CallerScope`] derivation.
//!
//! This is an authorization decision, not a transport concern: it decides
//! whether a caller is treated as an unrestricted first-party root or as
//! scoped to its own tenant, and every read and write path branches on the
//! answer. It lived in `api/rest/auth_context.rs`, which meant the
//! in-process `ClientHub` client (`api/service/local_client.rs`) reached into
//! the HTTP layer to gate calls that never touch HTTP.
//!
//! `require_authenticated` stays in `api/rest`: extracting a
//! `SecurityContext` from Axum request extensions genuinely is transport
//! work.

use toolkit_security::SecurityContext;

use crate::domain::role_definition::CallerScope;

/// Token-scope marker designating an unrestricted first-party caller.
/// Not a scope *path* — distinct from the `/` root scope used by
/// role-assignment scope.
const FIRST_PARTY_ROOT_TOKEN_SCOPE: &str = "*";

/// Derive the caller's effective [`CallerScope`] from an authenticated
/// [`SecurityContext`]. "Root" callers MUST be identified by the explicit
/// `"*"` first-party token scope — never by an absent `subject_tenant_id`,
/// which would collapse the unauthenticated path into the root-caller path.
#[must_use]
pub fn caller_scope_from_context(ctx: &SecurityContext) -> CallerScope {
    if is_first_party_root(ctx) {
        CallerScope::Root
    } else {
        CallerScope::Tenant(ctx.subject_tenant_id())
    }
}

/// True when the caller presents the `"*"` first-party token scope.
/// An empty `token_scopes` is intentionally NOT treated as
/// "unrestricted" here — for RBAC the cost of a false-positive root
/// caller is too high; integrations must opt in with `token_scopes = ["*"]`.
#[must_use]
pub fn is_first_party_root(ctx: &SecurityContext) -> bool {
    let scopes = ctx.token_scopes();
    scopes.len() == 1 && scopes[0] == FIRST_PARTY_ROOT_TOKEN_SCOPE
}

#[cfg(test)]
#[path = "caller_scope_tests.rs"]
mod caller_scope_tests;
