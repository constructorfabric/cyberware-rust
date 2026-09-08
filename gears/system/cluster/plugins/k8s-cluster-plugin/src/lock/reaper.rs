//! The stale lock-object reaper (DESIGN.md §5.5).
//!
//! Release *clears* a lock's `holderIdentity`; it does not delete the object (§5.5),
//! so one empty Lease accumulates per lock name ever used. For a bounded name space
//! that is a handful of objects; for an unbounded one it is an etcd leak. The reaper
//! bounds it: every `reaper_interval` it lists this plugin's Leases in the namespace
//! and issues a **guarded delete** (on `resourceVersion` *and* `uid`) of each
//! `primitive=lock` Lease whose holder is empty and whose `renewTime` is older than
//! `lock_object_retention`.
//!
//! Two properties keep it from becoming its own problem, both tested here at the
//! decision level: it is **safe to run from every replica** — two reapers racing
//! means one gets a `409` and moves on, which the guarded delete already yields as
//! `Ok(false)` — and the `uid` precondition means a reap can never land on a
//! *different* lock object that reused the name after a delete-and-recreate.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::jiff::Timestamp;
use kube::api::ListParams;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use cluster_sdk::ClusterError;

use crate::guarded;
use crate::naming::{LABEL_MANAGED_BY, LABEL_PRIMITIVE, MANAGED_BY_VALUE, Seg};

use super::LockRuntime;

/// One paginated list page size for the reaper's scan (§5.5).
const LIST_PAGE: u32 = 500;

/// Whether a lock Lease is eligible to be reaped (§5.5): its holder is empty **and**
/// its last renewal is older than `retention`.
///
/// A pure decision over `(holder emptiness, renewTime age, retention)`, so the
/// eligibility rule is unit-testable without a list call. A `renew_age` of `None`
/// (a Lease with no `renewTime` at all) is treated as *not* eligible — the reaper
/// declines to guess the age of an object it cannot date.
#[must_use]
pub fn reap_eligible(holder_empty: bool, renew_age: Option<Duration>, retention: Duration) -> bool {
    holder_empty && renew_age.is_some_and(|age| age > retention)
}

/// The age of a Lease's `renewTime` as of `now`, or `None` when the Lease carries no
/// `renewTime`. A `renewTime` in the future (clock skew) reads as a zero age, so a
/// freshly-written object is never mistaken for a stale one.
#[must_use]
pub fn renew_age(lease: &Lease, now: Timestamp) -> Option<Duration> {
    let renew = lease.spec.as_ref()?.renew_time.as_ref()?.0;
    let signed = now.duration_since(renew);
    // Clamp a negative (future) age to zero; whole-second resolution is ample for a
    // retention measured in hours.
    Some(Duration::from_secs(signed.as_secs().max(0).unsigned_abs()))
}

/// Whether a Lease's holder is empty (cleared by a release, or never set).
#[must_use]
pub fn holder_is_empty(lease: &Lease) -> bool {
    lease
        .spec
        .as_ref()
        .and_then(|spec| spec.holder_identity.as_deref())
        .is_none_or(str::is_empty)
}

/// The label selector matching every lock Lease this plugin owns in the namespace.
fn lock_selector() -> String {
    format!(
        "{LABEL_MANAGED_BY}={MANAGED_BY_VALUE},{LABEL_PRIMITIVE}={}",
        Seg::Lock.primitive_label()
    )
}

/// The reaper background loop (§5.5): every `reaper_interval`, prune long-empty lock
/// Leases with a guarded delete. Safe to run from every replica.
pub(super) async fn run_reaper(runtime: Arc<LockRuntime>, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(runtime.reaper_interval) => {
                if let Err(err) = reap_once(&runtime).await {
                    warn!(error = %err, "cluster.lock.reaper_failed: a reaper pass could not \
                          complete; retrying next interval");
                }
            }
        }
    }
}

/// Guarded-deletes one Lease if it is reap-eligible, swallowing races and logging a
/// real fault (§5.5). A guarded delete yields `Ok(false)` on a `409`/`404` (a
/// concurrent reaper or a re-acquire), so a race never surfaces as an error and can
/// never delete the wrong object. A genuine fault — e.g. RBAC that grants `list` but
/// not `delete`, or an admission-webhook rejection — is logged and swallowed rather
/// than propagated, so one un-reapable object does not abort the whole pass.
async fn reap_lease(api: &kube::Api<Lease>, lease: &Lease, retention: Duration, now: Timestamp) {
    if !reap_eligible(holder_is_empty(lease), renew_age(lease, now), retention) {
        return;
    }
    let Some(name) = lease.metadata.name.as_deref() else {
        return;
    };
    if let Err(err) = guarded::delete(api, name, &lease.metadata).await {
        warn!(
            error = %err, lease = %name,
            "cluster.lock.reap_failed: this object could not be reaped; continuing the pass"
        );
    }
}

/// One reaper pass: paginate the plugin's lock Leases, WARN if the name cardinality
/// is high (§5.5), and guarded-delete each reap-eligible object.
async fn reap_once(runtime: &LockRuntime) -> Result<(), ClusterError> {
    let api = runtime.api();
    let retention = runtime.lock_object_retention;
    let mut continue_token: Option<String> = None;
    let mut total: u64 = 0;

    loop {
        let mut params = ListParams::default()
            .labels(&lock_selector())
            .limit(LIST_PAGE);
        if let Some(token) = &continue_token {
            params = params.continue_token(token);
        }
        let list = api
            .list(&params)
            .await
            .map_err(|e| crate::k8s_error::map_kube_error(&e))?;
        total += u64::try_from(list.items.len()).unwrap_or(u64::MAX);

        let now = Timestamp::now();
        for lease in &list.items {
            reap_lease(&api, lease, retention, now).await;
        }

        continue_token = list.metadata.continue_.filter(|t| !t.is_empty());
        if continue_token.is_none() {
            break;
        }
    }

    if total > runtime.lock_name_cardinality_warn {
        warn!(
            count = total,
            threshold = runtime.lock_name_cardinality_warn,
            "cluster.lock.name_cardinality_high: this namespace holds many lock Leases; cluster \
             lock names must be bounded (a lock per request id is a misuse - DESIGN.md 5.5)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{holder_is_empty, reap_eligible, renew_age};
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    use k8s_openapi::jiff::{SignedDuration, Timestamp};
    use std::time::Duration;

    const RETENTION: Duration = Duration::from_hours(24); // 24h

    #[test]
    fn eligible_only_when_empty_and_aged_past_retention() {
        // Empty holder, aged well past retention → reap.
        assert!(reap_eligible(
            true,
            Some(Duration::from_hours(25)),
            RETENTION
        ));
        // Empty but still within retention → keep (a recently-released lock).
        assert!(!reap_eligible(
            true,
            Some(Duration::from_hours(1)),
            RETENTION
        ));
        // Held (non-empty) → never reaped, however old.
        assert!(!reap_eligible(
            false,
            Some(Duration::from_hours(25)),
            RETENTION
        ));
        // Undatable (no renewTime) → declined.
        assert!(!reap_eligible(true, None, RETENTION));
    }

    #[test]
    fn holder_emptiness_covers_none_and_empty_string() {
        let cleared = Lease {
            spec: Some(LeaseSpec {
                holder_identity: None,
                ..LeaseSpec::default()
            }),
            ..Lease::default()
        };
        assert!(holder_is_empty(&cleared));

        let empty = Lease {
            spec: Some(LeaseSpec {
                holder_identity: Some(String::new()),
                ..LeaseSpec::default()
            }),
            ..Lease::default()
        };
        assert!(holder_is_empty(&empty));

        let held = Lease {
            spec: Some(LeaseSpec {
                holder_identity: Some("broker-7#uuid".to_owned()),
                ..LeaseSpec::default()
            }),
            ..Lease::default()
        };
        assert!(!holder_is_empty(&held));
    }

    #[test]
    fn renew_age_is_measured_from_now_and_future_reads_as_zero() {
        let now = Timestamp::from_second(1_000_000).unwrap();
        let one_hour_ago = now - SignedDuration::from_hours(1);
        let lease = |renew: Timestamp| Lease {
            spec: Some(LeaseSpec {
                renew_time: Some(MicroTime(renew)),
                ..LeaseSpec::default()
            }),
            ..Lease::default()
        };
        assert_eq!(
            renew_age(&lease(one_hour_ago), now),
            Some(Duration::from_hours(1))
        );
        // A future renewTime (skew) reads as zero, never negative.
        let future = now + SignedDuration::from_hours(1);
        assert_eq!(renew_age(&lease(future), now), Some(Duration::ZERO));
        // No renewTime → None.
        assert_eq!(renew_age(&Lease::default(), now), None);
    }
}
