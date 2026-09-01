//! Batch display-name reads for user principals.
//!
//! Narrow, RBAC-owned port — same posture as
//! [`crate::domain::rg_port`]: the domain layer never sees
//! `account_management_sdk` types, so an upstream rename cannot ripple
//! into pure-domain code (the port-isolation invariant).
//!
//! The contract is deliberately lossy: the implementation returns the
//! names it could resolve and says nothing about the rest. An id absent
//! from the returned map means "no name" — a deleted principal, a
//! principal that does not live in the queried tenant, or a profile with
//! nothing renderable — and the hydrator turns that into an omitted
//! field, never an error. Errors are reserved for *upstream* failure,
//! and even those are non-fatal for the caller: a display name must
//! never change the status code, the row set, or the cursor of a
//! role-assignment read.

use std::collections::HashMap;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The single definition of "this string is a usable display name".
///
/// A resolved-but-blank name is worse than no name at all: the wire
/// carries `"principal_name": "   "`, the UI renders a blank cell that
/// reads as a bug, and the id — which the row still carries and which a
/// client falls back to rendering when the field is *absent* — is hidden
/// behind it. So blank collapses to absent, everywhere, and "everywhere"
/// is why this lives on the port rather than inside one adapter: every
/// name source (account management, resource groups, RBAC's own
/// `role_definitions` table, and whatever is added next) passes through
/// it, and the hydrator applies it once more at the merge step so a new
/// source cannot forget.
///
/// Trimming, not just emptiness-checking: a name is display data, and a
/// leading newline out of a directory attribute is the same rendering
/// problem as an empty string.
#[must_use]
pub fn non_blank(name: String) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed.len() == name.len() {
        // Already clean — hand the original back rather than paying for
        // a copy on the overwhelmingly common path.
        Some(name)
    } else {
        Some(trimmed.to_owned())
    }
}

/// Failure surface of a name read.
///
/// Both variants mean the same thing to the caller — "no names this
/// time" — and are distinguished only so metrics and logs can tell an
/// upstream outage from an authorization gap. `Clone` is derived (unlike
/// [`crate::domain::rg_port::RbacRgReadError`], which boxes a source
/// error) so test doubles can hand out the same scripted failure on
/// every call.
#[domain_model]
#[derive(Debug, Clone, thiserror::Error)]
pub enum PrincipalNameError {
    /// Upstream unreachable, timed out, failed, or not registered in a
    /// way the adapter chose to report rather than swallow.
    #[error("principal-name upstream unavailable: {detail}")]
    Unavailable {
        /// Redacted upstream diagnostic; safe for logs, never for the
        /// response body.
        detail: String,
    },
    /// The caller may not read users in that tenant. Resolution runs
    /// with the caller's own `SecurityContext`, so this is an expected
    /// outcome for a caller that can read role assignments but not
    /// users — it degrades to ids, it does not fail the read.
    #[error("principal-name read denied for the calling subject")]
    Denied,
}

/// Resolve display names for user principals inside one tenant.
///
/// One call answers for one tenant, which is what lets the hydrator
/// collapse a whole page of rows into one upstream round trip per
/// distinct lookup tenant instead of one per row.
#[async_trait]
pub trait PrincipalNameReader: Send + Sync {
    /// Return `id -> display name` for those of `ids` that resolve
    /// inside `tenant_id`. Unresolved ids are simply absent from the
    /// map. `ids` may contain duplicates; implementations MUST
    /// deduplicate internally.
    ///
    /// `ctx` is the **caller's** context, never an elevated one: what a
    /// response says about an identity must not exceed what the caller
    /// is allowed to learn about it.
    ///
    /// # Errors
    ///
    /// [`PrincipalNameError`] when the upstream could not be consulted
    /// at all. Callers treat that as "no names", never as a failure.
    async fn user_names(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        ids: &[String],
    ) -> Result<HashMap<String, String>, PrincipalNameError>;
}
