//! Pure hierarchy reduction: turning every visible row of one reference into
//! a single effective outcome (ADR-0005, "Reducing a reference to one item").
//!
//! Shared, verbatim, by the point read
//! ([`crate::domain::secret::service::Service::resolve_credential`]) and the
//! collection read
//! ([`crate::domain::secret::service::Service::list`](../service/list/index.html))
//! so a change to visibility/priority rules cannot apply to one and miss the
//! other (ADR-0005, D3). Takes no [`crate::domain::secret::service::Service`]
//! state — it is a function of the candidate rows and the caller's tenant
//! chain alone, which is what lets it be called from either place without
//! threading `Service`'s private fields through a module boundary.

use credstore_sdk::{CredentialStatus, InheritanceStatus, OwnerId, SharingMode, TenantId};
use time::OffsetDateTime;

use crate::domain::secret::model::{Fallback, SecretRow, SecretStatus};

/// Outcome of reducing every visible row of one reference to one item.
pub(crate) struct Reduced<'a> {
    /// The caller's own-tenant row, if any (of any status — a `declared`
    /// row still "holds" the reference, ADR-0004). Two-phase priority:
    /// `private` beats non-`private` at the caller's own tenant.
    pub own: Option<&'a SecretRow>,
    /// The nearest row that actually resolves — `active` and not expired, or
    /// `declared` with `fallback: none` (a suppressing row that competes and
    /// blocks). `None` iff nothing in `candidates` resolves.
    pub winner: Option<&'a SecretRow>,
    /// `own.or(winner)`: the row whose `sharing`/`expires_at`/
    /// `secret_type_uuid` describe the reference's effective record.
    pub effective: &'a SecretRow,
    /// Whether the effective value is the caller's own, inherited, an
    /// override of an inherited value, or a suppressing record with no
    /// value.
    pub inheritance: InheritanceStatus,
}

impl Reduced<'_> {
    /// The caller's own-row `status` (`none`/`declared`/`active`) — never a
    /// property of the effective row (ADR-0004, "Two representations").
    #[must_use]
    pub(crate) fn own_status(&self) -> CredentialStatus {
        match self.own {
            Some(o) if o.status == SecretStatus::Active => CredentialStatus::Active,
            Some(_) => CredentialStatus::Declared,
            None => CredentialStatus::None,
        }
    }
}

/// A row is a resolution candidate (ADR-0004, Suppression): `active` and not
/// expired, or `declared` with `fallback: none` (a suppressing row that
/// competes and, when nearest, wins). A `declared`/`inherit` row never
/// competes. Mirrors
/// `infra::storage::repo_impl::reads::resolution_eligible_condition`, which
/// applies the same predicate in SQL.
fn resolvable(r: &SecretRow, now: OffsetDateTime) -> bool {
    match r.status {
        SecretStatus::Active => r.expires_at.is_none_or(|at| at > now),
        SecretStatus::Declared => r.fallback == Fallback::None,
    }
}

/// Reduce every visible row of one reference — `candidates`, in any order —
/// to a single outcome, or `None` if `candidates` is empty (nothing to
/// report: neither an own row nor a resolvable ancestor row exists).
///
/// `candidates` MUST be every row of the reference visible to `req`/`subject`
/// across `chain` (ADR-0005: "reduction sees every row a value read would
/// see", not only those a SQL clamp admitted) — the same set
/// [`crate::domain::secret::repo::SecretRepo::resolve_candidates`] returns
/// for a single reference. `subject` is not consulted here: the repo's own
/// visibility predicate already restricts `candidates`'s private rows to the
/// caller, so nothing about *which* rows compete depends on it a second
/// time — it is carried for signature symmetry with the SQL-side predicate
/// it mirrors, and so a caller need not special-case dropping it.
#[allow(
    clippy::ref_option,
    reason = "Reduced borrows into `candidates`; `Option<&SecretRow>` is the natural shape for \
              fields callers match on, and `&Option<_>` would only add an extra deref"
)]
pub(crate) fn reduce_reference<'a>(
    candidates: &'a [SecretRow],
    req: TenantId,
    _subject: OwnerId,
    chain: &[uuid::Uuid],
) -> Option<Reduced<'a>> {
    if candidates.is_empty() {
        return None;
    }

    // The caller's own row: two-phase priority, private beats non-private at
    // the same (own) tenant — mirrors `find_own`/`resolve_for_get`.
    let own = candidates
        .iter()
        .filter(|r| r.tenant_id == req)
        .min_by_key(|r| i32::from(r.sharing != SharingMode::Private));

    let now = OffsetDateTime::now_utc();
    let pos = |t: TenantId| chain.iter().position(|c| *c == t.0).unwrap_or(usize::MAX);
    let winner = candidates
        .iter()
        .filter(|r| resolvable(r, now))
        .min_by(|a, b| {
            pos(a.tenant_id)
                .cmp(&pos(b.tenant_id))
                .then((a.sharing != SharingMode::Private).cmp(&(b.sharing != SharingMode::Private)))
        });

    let effective = own.or(winner)?;

    let inheritance = if let Some(w) = winner {
        if w.status == SecretStatus::Declared {
            // A declared/none winner blocks the walk — the caller's own row
            // or an ancestor's, either way the outcome is the same name
            // (ADR-0004, Suppression).
            InheritanceStatus::Suppressed
        } else if own.is_some_and(|o| o.id == w.id) {
            let ancestor_candidate_exists = candidates.iter().any(|r| r.tenant_id != req);
            if ancestor_candidate_exists {
                InheritanceStatus::Overridden
            } else {
                InheritanceStatus::Own
            }
        } else {
            InheritanceStatus::Inherited
        }
    } else {
        // Nothing resolves; the caller has an own row (declared/inherit, or
        // an expired-active row with nothing behind it) — reported as `Own`
        // (ADR-0004: "choose Own and document").
        InheritanceStatus::Own
    };

    Some(Reduced {
        own,
        winner,
        effective,
        inheritance,
    })
}

#[cfg(test)]
#[path = "reduce_tests.rs"]
mod tests;
