//! The shared read/mutate/guarded-`replace` helper over both resource types, and
//! **the single place a `409` is classified** (DESIGN.md §2.8, §10).
//!
//! Every mutating call this plugin makes is a *guarded* update: an `Api::replace`
//! carries the `resourceVersion` read moments earlier (so the API server rejects a
//! stale write with `409`), and an `Api::delete` carries
//! `Preconditions { resource_version, uid }` (so neither the lock reaper nor the
//! cache sweeper can delete an object revived between its read and its delete).
//! That native compare-and-swap, arbitrated by a Raft quorum, is why ADR-009 rates
//! this backend safe with no qualifier.
//!
//! ## Why one classifier
//!
//! A `409` means different things at different call sites — contention on a lock
//! acquisition, a lost race on a renewal, a CAS conflict, an already-satisfied
//! release — and the two `409`s that share the status code (`AlreadyExists` on a
//! create vs. `Conflict` on a stale write) are told apart only by the `Status.reason`
//! field. Scattering that logic across the three backends is how one call site ends
//! up treating an `AlreadyExists` as a `Conflict` and introduces a split-brain bug.
//! So the decision lives here, in [`classify_conflict`], as a **pure function of
//! `(call site, code, reason)`** that every guarded helper routes its errors
//! through — and that the L1 suite exercises exhaustively.

use kube::api::{DeleteParams, Preconditions};
use kube::core::{ObjectMeta, Resource};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;

use cluster_sdk::ClusterError;

use crate::k8s_error::map_kube_error;

/// The K8s `Status.reason` for a create colliding with an existing object.
const REASON_ALREADY_EXISTS: &str = "AlreadyExists";

/// The call site issuing a guarded write, which — together with the `409`'s
/// `reason` — determines how a `409 Conflict` is classified (§10).
///
/// `AlreadyExists` (a create colliding) resolves the same way regardless of site
/// (the object exists; proceed), so the site only disambiguates the `Conflict`
/// flavour of `409`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSite {
    /// A lock `try_lock`/`lock()` attempt: the create, or the guarded replace that
    /// claims a free/expired lock (§5.2).
    LockAcquire,
    /// A leader-election acquire/steal: the create, or the guarded replace that
    /// claims a free/lapsed election Lease (§4.1).
    LeaderAcquire,
    /// A leader-election renewal's guarded replace (§4.2).
    LeaderRenew,
    /// A lock renewal's guarded replace (§5.4).
    LockRenew,
    /// A cache `put`'s overwrite: the create, or the guarded replace on an existing
    /// key (§6.1).
    CachePut,
    /// A cache `put_if_absent` create (§6.1).
    PutIfAbsent,
    /// A cache `compare_and_swap`'s guarded replace after `expected` matched at the
    /// read (§6.1).
    CompareAndSwap,
    /// A leader resign's guarded replace clearing `holderIdentity` (§4.4).
    Resign,
    /// A lock release's guarded replace clearing `holderIdentity` (§5.4).
    Release,
    /// Any guarded delete: cache `delete`/`compare_and_delete`, the cache sweeper,
    /// the lock reaper (§5.5, §6.1).
    GuardedDelete,
    /// The startup CRD canary's create/delete (§3.4). The canary is a direct,
    /// unguarded create in `preflight` (it needs the raw `404`/`422` to name the CRD
    /// manifest), so this variant is never *constructed* on that path — it documents
    /// the §10 row for a canary `409` and is exercised by the classifier unit tests.
    #[allow(dead_code)]
    Canary,
}

/// How a `409` at a given [`CallSite`] must be handled (§10). Each variant is a row
/// of the §10 "409" table; no call site classifies a `409` on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// `409 AlreadyExists` on a create: the object already exists. A create-then-
    /// overwrite path (`put`, `LockAcquire`) falls through to its guarded-replace
    /// branch; `put_if_absent` maps this to `Ok(None)`; the canary treats it as a
    /// benign pre-existing object.
    AlreadyExists,
    /// `409 Conflict` on a lock acquisition: another holder won the create or the
    /// guarded replace. → `ClusterError::LockContended`.
    LockContended,
    /// `409 Conflict` on a leader-election acquire/steal: another candidate won the
    /// race, so this candidate becomes a follower — an ordinary outcome, not an
    /// error (§4.1).
    LeaderLostRace,
    /// `409 Conflict` on a renewal (leader or lock) or an unconditional `put`: the
    /// object moved under the guarded write. Re-read and re-decide — a renewal
    /// re-reads to see whether it still holds; a `put` retries within its bounded
    /// budget (§4.2, §5.4, §6.1).
    ReReadRetry,
    /// `409 Conflict` on a `compare_and_swap` whose guarded replace lost the race
    /// after `expected` matched at the read: re-read once to populate
    /// `CasConflict { current }` (§6.1).
    CasLostRace,
    /// `409 Conflict` on a resign or a guarded release: the claim already moved on,
    /// which is exactly the postcondition the caller asked for. → `Ok(())`.
    AlreadyReleased,
    /// `409 Conflict` on a guarded delete: the object changed under the delete
    /// precondition, or was already deleted (and perhaps recreated) — either way
    /// this delete did not remove *this* object. → `Ok(false)`.
    DeleteLostRace,
}

/// Classifies a `409` at `site` from its HTTP `code` and `Status.reason` (§10).
///
/// Returns `None` when `code` is not `409` — the caller then maps the error through
/// [`map_kube_error`] like any other status. When `code` **is** `409`, the `reason`
/// separates the two same-code cases: `"AlreadyExists"` (a create collided) always
/// yields [`Conflict::AlreadyExists`]; anything else (`"Conflict"`, or an empty
/// reason) is a `resourceVersion` conflict whose meaning depends on the `site`.
///
/// A pure `create` site (`PutIfAbsent`, `Canary`) can only `409` with
/// `AlreadyExists` — the API server never answers a create with a `resourceVersion`
/// conflict — so those sites resolve to [`Conflict::AlreadyExists`] even if the
/// reason field is somehow absent.
#[must_use]
pub fn classify_conflict(site: CallSite, code: u16, reason: &str) -> Option<Conflict> {
    if code != 409 {
        return None;
    }
    if reason == REASON_ALREADY_EXISTS {
        return Some(Conflict::AlreadyExists);
    }
    // A `409` that is not `AlreadyExists` is a `resourceVersion` conflict on a
    // guarded write; its meaning is the call site's.
    Some(match site {
        CallSite::LockAcquire => Conflict::LockContended,
        CallSite::LeaderAcquire => Conflict::LeaderLostRace,
        CallSite::LeaderRenew | CallSite::LockRenew | CallSite::CachePut => Conflict::ReReadRetry,
        CallSite::CompareAndSwap => Conflict::CasLostRace,
        CallSite::Resign | CallSite::Release => Conflict::AlreadyReleased,
        CallSite::GuardedDelete => Conflict::DeleteLostRace,
        // Pure creates cannot produce a `Conflict`; a `409` here is `AlreadyExists`.
        CallSite::PutIfAbsent | CallSite::Canary => Conflict::AlreadyExists,
    })
}

/// The outcome of a guarded [`replace`]: the write applied, or the API server
/// rejected it with a classified [`Conflict`] the caller acts on.
#[derive(Debug)]
pub enum Replaced<K> {
    /// The guarded replace applied; carries the server's returned object (its fresh
    /// `resourceVersion` for the next guarded write).
    Applied(Box<K>),
    /// The API server answered `409`. Callers branch on this variant, not on the
    /// specific [`Conflict`] flavour (each already knows its own call site) — the
    /// carried value is retained for diagnostics and to keep the guarded-replace
    /// outcome symmetric with [`classify_conflict`], which the L1 suite covers.
    Conflict(#[allow(dead_code)] Conflict),
}

/// The outcome of a [`create`]: the object was created, or it already existed
/// (`409 AlreadyExists`).
#[derive(Debug)]
pub enum Created<K> {
    /// The create succeeded; carries the server's returned object.
    Created(Box<K>),
    /// A `409 AlreadyExists`: the object is already present. The caller's create
    /// path decides what that means (fall through to a guarded replace, or
    /// `put_if_absent` → `Ok(None)`).
    Exists,
}

/// A create that classifies a `409 AlreadyExists` as [`Created::Exists`] rather
/// than an error (§10). `site` routes the classification so a stray `409 Conflict`
/// on a create — which the API server does not produce — is *not* silently read as
/// "exists" but mapped as the error it is.
///
/// # Errors
///
/// Any `kube::Error` that is not a `409 AlreadyExists`, mapped to a [`ClusterError`].
pub async fn create<K>(
    api: &kube::Api<K>,
    obj: &K,
    site: CallSite,
) -> Result<Created<K>, ClusterError>
where
    K: Resource + Clone + Serialize + DeserializeOwned + Debug,
{
    match api.create(&kube::api::PostParams::default(), obj).await {
        Ok(created) => Ok(Created::Created(Box::new(created))),
        Err(err) => match status_of(&err) {
            Some((code, reason))
                if classify_conflict(site, code, reason) == Some(Conflict::AlreadyExists) =>
            {
                Ok(Created::Exists)
            }
            _ => Err(map_kube_error(&err)),
        },
    }
}

/// Extracts `(code, reason)` from a `kube::Error` when it is an API `Status`,
/// so the pure [`classify_conflict`] can be applied to a real error.
fn status_of(err: &kube::Error) -> Option<(u16, &str)> {
    match err {
        kube::Error::Api(status) => Some((status.code, status.reason.as_str())),
        _ => None,
    }
}

/// Reads `name`, returning `None` on `404` (§6.1). A `404` is "no claim / no entry
/// exists", which every primitive's create path expects — never an error.
///
/// # Errors
///
/// Maps any non-`404` `kube::Error` through [`map_kube_error`].
pub async fn read<K>(api: &kube::Api<K>, name: &str) -> Result<Option<K>, ClusterError>
where
    K: Resource + Clone + DeserializeOwned + Debug,
{
    api.get_opt(name).await.map_err(|err| map_kube_error(&err))
}

/// A guarded `replace`: `obj` **must** carry the `resourceVersion` from the read it
/// was mutated from, so the API server rejects a stale write with `409` (§2.7).
///
/// On success returns [`Replaced::Applied`]; on a `409` returns
/// [`Replaced::Conflict`] carrying [`classify_conflict`]'s decision for `site`. Any
/// other error is mapped through [`map_kube_error`].
///
/// # Errors
///
/// Any non-`409` `kube::Error` (transport, auth, other status codes), mapped to a
/// [`ClusterError`].
pub async fn replace<K>(
    api: &kube::Api<K>,
    name: &str,
    obj: &K,
    site: CallSite,
) -> Result<Replaced<K>, ClusterError>
where
    K: Resource + Clone + Serialize + DeserializeOwned + Debug,
{
    debug_assert!(
        obj.meta().resource_version.is_some(),
        "guarded replace must carry a resourceVersion precondition (DESIGN.md 2.7)"
    );
    match api
        .replace(name, &kube::api::PostParams::default(), obj)
        .await
    {
        Ok(applied) => Ok(Replaced::Applied(Box::new(applied))),
        Err(err) => {
            match status_of(&err).and_then(|(code, reason)| classify_conflict(site, code, reason)) {
                Some(conflict) => Ok(Replaced::Conflict(conflict)),
                None => Err(map_kube_error(&err)),
            }
        }
    }
}

/// A guarded `delete`: carries `Preconditions { resource_version, uid }` from
/// `meta`, so the delete lands only on the exact object read — never on a
/// different object that reused the name after a delete-and-recreate (§2.7, §5.5).
///
/// Returns `Ok(true)` when this call removed the object, and `Ok(false)` when it
/// did not — a `404` (already gone) or a `409` (the precondition failed; §10's
/// `Ok(false)` on a guarded delete).
///
/// # Errors
///
/// Any other `kube::Error`, mapped to a [`ClusterError`]. `meta` missing a
/// `resource_version` or `uid` is a caller bug (a delete built from something other
/// than a fresh read) and trips a `debug_assert`.
pub async fn delete<K>(
    api: &kube::Api<K>,
    name: &str,
    meta: &ObjectMeta,
) -> Result<bool, ClusterError>
where
    K: Resource + Clone + DeserializeOwned + Debug,
{
    debug_assert!(
        meta.resource_version.is_some() && meta.uid.is_some(),
        "guarded delete must carry resourceVersion + uid preconditions (DESIGN.md 2.7)"
    );
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            resource_version: meta.resource_version.clone(),
            uid: meta.uid.clone(),
        }),
        ..DeleteParams::default()
    };
    match api.delete(name, &params).await {
        Ok(_) => Ok(true),
        Err(err) => match status_of(&err) {
            // 404: already gone. 409: the precondition lost — a concurrent writer
            // moved the object, so this delete did not remove it.
            Some((404, _)) => Ok(false),
            Some((code, reason))
                if classify_conflict(CallSite::GuardedDelete, code, reason)
                    == Some(Conflict::DeleteLostRace) =>
            {
                Ok(false)
            }
            _ => Err(map_kube_error(&err)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{CallSite, Conflict, classify_conflict};

    /// Every non-`409` code defers to the generic status mapper (`None`), whatever
    /// the call site.
    #[test]
    fn non_409_is_not_a_conflict() {
        for code in [200, 401, 403, 404, 410, 422, 429, 500, 503, 504] {
            for site in [
                CallSite::LockAcquire,
                CallSite::CompareAndSwap,
                CallSite::GuardedDelete,
            ] {
                assert_eq!(
                    classify_conflict(site, code, "Conflict"),
                    None,
                    "code {code} at {site:?} must not classify as a conflict"
                );
            }
        }
    }

    /// `409 AlreadyExists` is `AlreadyExists` at every call site — the create-collision
    /// case, never confused with a `resourceVersion` `Conflict`.
    #[test]
    fn already_exists_reason_wins_at_every_site() {
        for site in [
            CallSite::LockAcquire,
            CallSite::LeaderAcquire,
            CallSite::LeaderRenew,
            CallSite::LockRenew,
            CallSite::CachePut,
            CallSite::PutIfAbsent,
            CallSite::CompareAndSwap,
            CallSite::Resign,
            CallSite::Release,
            CallSite::GuardedDelete,
            CallSite::Canary,
        ] {
            assert_eq!(
                classify_conflict(site, 409, "AlreadyExists"),
                Some(Conflict::AlreadyExists),
                "AlreadyExists must not be confused with Conflict at {site:?}"
            );
        }
    }

    /// The `409 Conflict` row of §10, site by site.
    #[test]
    fn conflict_reason_maps_per_call_site() {
        let cases = [
            (CallSite::LockAcquire, Conflict::LockContended),
            (CallSite::LeaderAcquire, Conflict::LeaderLostRace),
            (CallSite::LeaderRenew, Conflict::ReReadRetry),
            (CallSite::LockRenew, Conflict::ReReadRetry),
            (CallSite::CachePut, Conflict::ReReadRetry),
            (CallSite::CompareAndSwap, Conflict::CasLostRace),
            (CallSite::Resign, Conflict::AlreadyReleased),
            (CallSite::Release, Conflict::AlreadyReleased),
            (CallSite::GuardedDelete, Conflict::DeleteLostRace),
        ];
        for (site, expected) in cases {
            assert_eq!(
                classify_conflict(site, 409, "Conflict"),
                Some(expected),
                "{site:?} on 409 Conflict"
            );
        }
    }

    /// A pure create (`put_if_absent`, canary) resolves to `AlreadyExists` even when
    /// the reason field is empty — the API server never answers a create with a
    /// `resourceVersion` conflict, so an unlabelled `409` there is still a collision.
    #[test]
    fn pure_create_sites_treat_bare_409_as_already_exists() {
        for site in [CallSite::PutIfAbsent, CallSite::Canary] {
            assert_eq!(
                classify_conflict(site, 409, ""),
                Some(Conflict::AlreadyExists),
                "{site:?} with an empty reason"
            );
            // And the explicit reason agrees.
            assert_eq!(
                classify_conflict(site, 409, "AlreadyExists"),
                Some(Conflict::AlreadyExists)
            );
        }
    }

    /// An unlabelled `409` at a *mutating* site is a `resourceVersion` conflict —
    /// the common case, since the reason field is frequently just `"Conflict"` or
    /// blank.
    #[test]
    fn bare_409_at_a_mutating_site_is_a_conflict() {
        assert_eq!(
            classify_conflict(CallSite::LockAcquire, 409, ""),
            Some(Conflict::LockContended)
        );
        assert_eq!(
            classify_conflict(CallSite::CompareAndSwap, 409, ""),
            Some(Conflict::CasLostRace)
        );
    }
}
