//! Authentication-context extraction for the REST handlers.
//!
//! Every RBAC endpoint requires an authenticated [`SecurityContext`];
//! requests without one MUST be refused with 401 — never silently
//! treated as an unauthenticated "root" identity. "Root" callers are
//! identified by the presence of the `"*"` first-party token scope,
//! not by the absence of a tenant — but that decision now lives in
//! `domain::caller_scope`, because it is an authorization decision rather
//! than transport work. Only the extraction stays here.

use axum::extract::Extension;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::SecurityContext;

use crate::api::rest::error::unauthenticated_error;

/// Extract the [`SecurityContext`] from the request extensions, returning
/// 401 when it is missing, carries the anonymous all-zero placeholder, or
/// is missing the positive `subject_type` marker that a real `AuthN`
/// resolver always populates.
pub fn require_authenticated(
    extension_ctx: Option<Extension<SecurityContext>>,
) -> Result<SecurityContext, CanonicalError> {
    let Some(Extension(ctx)) = extension_ctx else {
        return Err(unauthenticated_error());
    };
    if ctx.subject_id().is_nil() || ctx.subject_tenant_id().is_nil() {
        return Err(unauthenticated_error());
    }
    if ctx.subject_type().is_none() {
        return Err(unauthenticated_error());
    }
    Ok(ctx)
}

#[cfg(test)]
#[path = "auth_context_tests.rs"]
mod auth_context_tests;
