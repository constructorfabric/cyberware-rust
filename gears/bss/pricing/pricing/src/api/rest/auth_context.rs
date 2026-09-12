//! Authentication-context extraction for the REST handlers.
//!
//! Every catalog surface requires an authenticated [`SecurityContext`]:
//! authoring is governed and audited by actor, and even the read surfaces are
//! tenant-scoped, so a request without one is refused with 401 rather than
//! silently treated as an anonymous identity.

use axum::extract::Extension;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::error::unauthenticated;
use crate::domain::audit::AuditStamp;
use time::OffsetDateTime;

/// Extract the [`SecurityContext`] from the request extensions, returning 401
/// when it is missing, carries the anonymous all-zero placeholder, or lacks the
/// positive `subject_type` marker a real `AuthN` resolver always populates.
///
/// # Errors
/// [`CanonicalError`] (401) when no authenticated context is present.
pub(crate) fn require_authenticated(
    extension_ctx: Option<Extension<SecurityContext>>,
) -> Result<SecurityContext, CanonicalError> {
    let Some(Extension(ctx)) = extension_ctx else {
        return Err(unauthenticated());
    };
    if ctx.subject_id().is_nil() || ctx.subject_tenant_id().is_nil() {
        return Err(unauthenticated());
    }
    if ctx.subject_type().is_none() {
        return Err(unauthenticated());
    }
    Ok(ctx)
}

#[cfg(test)]
#[path = "auth_context_tests.rs"]
mod auth_context_tests;

/// The audit stamp every mutating authoring route carries.
///
/// Built once here rather than at each of the six handlers, because it is one
/// fact — *who is acting, when, under which request* — and six spellings of it
/// would be six chances for one route to record a different actor than the one
/// the gate was asked about.
///
/// The actor is the security context's subject id, which `inst-au-pii` requires
/// to be **pseudonymous**: the record stores the principal id and never a display
/// name or an email, so the ≥ 7-year retention holds no directly-identifying
/// operator PII.
///
/// **`correlation` is the request's, not the record's** (D-178). It is
/// established once per request by
/// [`correlation::establish`](crate::api::rest::correlation::establish) and taken
/// here as an argument rather than minted, which is the whole of D-178 clause
/// (2): every record and every outbox row one operator call produces carries one
/// value, and a `Uuid::now_v7()` on this line would give each of them a different
/// one while satisfying "never NULL". The parameter is what makes that a property
/// of the type rather than of a convention — a handler cannot build a stamp
/// without having asked the edge for the value.
#[must_use]
pub fn audit_stamp(ctx: &SecurityContext, now: OffsetDateTime, correlation: Uuid) -> AuditStamp {
    AuditStamp {
        actor_principal_id: ctx.subject_id(),
        recorded_at: now,
        correlation_id: correlation,
    }
}
