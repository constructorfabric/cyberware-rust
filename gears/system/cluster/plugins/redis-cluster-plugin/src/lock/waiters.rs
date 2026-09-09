//! The in-process release-waiter registry and the wake delay a blocked
//! `lock()` sleeps for between attempts (DESIGN.md §5.3).
//!
//! ## Why an in-process registry rather than a server-side wait
//!
//! Redis has no "wait for this key" command short of `BLPOP` on a companion
//! list — and this plugin cannot reach one: `i-lists` is deliberately absent
//! from `fred`'s feature list (DESIGN.md §3.1), which makes the
//! whole blocking family unreachable at compile time. That absence is the
//! reason this file exists rather than an obstacle it works around. A
//! companion-list wait would leak one list per lock name and need its own
//! cleanup story, to produce a wake the `lock_release` script already publishes
//! for free on `<prefix>:e:l:<name>` (DESIGN.md §2.5).
//!
//! So the wake path is: holder releases → the release script `PUBLISH`es →
//! every instance's subscriber fan-out sees the message → that instance's
//! registry wakes its own blocked waiters for that name. The registry is
//! per-process; the publish is what crosses instances.
//!
//! **A missed wake costs latency, never correctness.** The acquisition loop
//! re-attempts `SET NX PX` itself as the source of truth, so a waiter that
//! registered a microsecond after the publish landed, or one whose subscriber
//! is mid-reconnect, simply waits out [`wake_delay`] and tries again.
//!
//! ## What is ported and what is new
//!
//! The registry's shape is `postgres-cluster-plugin/src/lock/notify.rs`'s: a
//! per-name map of per-waiter ids, and deregistration on drop. Both halves are
//! load-bearing there for a reason that applies identically here — an earlier
//! `Vec<Sender>` was only ever drained by a notification, so a name that is
//! renewed but never released grew one dead sender per heartbeat tick for the
//! whole life of the waiter (PGR-M7).
//!
//! [`wake_delay`] is new: Postgres has no remaining-TTL to read, so its loop
//! sleeps a flat heartbeat. Redis answers `PTTL` on the lease that is blocking
//! us, and a lease due in 40 ms should not be waited out for 250 ms.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use dashmap::DashMap;
use rand::RngExt as _;
use tokio::sync::oneshot;

/// The safety-net wake interval for a blocked `lock()` (DESIGN.md §5.3).
///
/// This is what a waiter falls back to when no release notification arrives —
/// because the holder crashed rather than releasing, because the subscriber is
/// reconnecting, or because the publish landed in the instant between a
/// waiter's failed attempt and its registration. `RD-LOCK-003` asserts a
/// publish-driven wake lands *well under* this, which is what makes that test a
/// statement about the notification path rather than a latency measurement: a
/// wake at roughly one heartbeat means the notification was missed, not slow.
pub const HEARTBEAT: Duration = Duration::from_millis(250);

/// The upper bound on the next wake, before jitter: `min(PTTL, HEARTBEAT)`,
/// further clamped to the caller's own remaining budget.
///
/// Three bounds, each for its own reason:
///
/// - **`HEARTBEAT`** is the safety net against a wake that never comes.
/// - **`pttl`** is the lease that is actually blocking us, read on the attempt
///   that just failed. A lock due to expire in 40 ms should be retried in 40 ms;
///   sleeping a full heartbeat past its deadline would hand the name to whoever
///   happens to poll next instead of to the waiter that has been waiting for it.
///   `None` means the `PTTL` was unreadable or the key had already gone (in
///   which case the next attempt will find it free anyway).
/// - **`remaining`** is what is left of the caller's `timeout`. Without it a
///   waiter with 5 ms of budget left sleeps 250 ms and reports `LockTimeout`
///   245 ms late, which is visible to any caller that measures its own budget.
#[must_use]
pub fn wake_cap(pttl: Option<Duration>, remaining: Duration) -> Duration {
    HEARTBEAT.min(pttl.unwrap_or(HEARTBEAT)).min(remaining)
}

/// The actual sleep: **full jitter** over `[0, wake_cap]` (DESIGN.md §5.3).
///
/// Full jitter — uniform from zero rather than a fixed delay, or a fixed delay
/// plus a small perturbation — is what `rand` is a direct dependency for.
/// Without it every instance contending for one name retries on the same
/// deterministic schedule, and a hot lock turns the fleet's retries into
/// synchronized `SET NX` bursts against the single key already under contention.
/// Decorrelation is the whole product here; the mean delay it happens to
/// produce is not the point.
///
/// Drawing from zero means an occasional near-immediate retry, which is one
/// extra round trip and no correctness cost. The mean is `wake_cap / 2`, so a
/// waiter blocked on a healthy lock costs about eight `SET NX`s a second rather
/// than four — cheap enough that a floor, which would reintroduce exactly the
/// correlation this exists to break, is not worth adding.
#[must_use]
pub fn wake_delay(pttl: Option<Duration>, remaining: Duration) -> Duration {
    let cap = wake_cap(pttl, remaining);
    // Nanoseconds, saturating: `wake_cap` is bounded above by `HEARTBEAT`, so
    // the fallback is unreachable in practice and exists only so the conversion
    // needs no `unwrap`.
    let nanos = u64::try_from(cap.as_nanos()).unwrap_or(u64::MAX);
    // Inclusive, because an exclusive `0..0` is an empty range and panics.
    Duration::from_nanos(rand::rng().random_range(0..=nanos))
}

/// Registry of in-process waiters blocked in
/// [`lock()`](cluster_sdk::DistributedLockBackend::lock), keyed by the lock name
/// they are waiting to retry.
///
/// Fed by the subscriber fan-out, which is the only caller of
/// [`notify`](Self::notify): a release anywhere in the fleet reaches this
/// process as a `PUBLISH` on the lock's release channel, and reaches this map
/// from there.
pub struct ReleaseWaiters {
    /// Per-name set of live waiters, each keyed by a process-unique id so a
    /// waiter can withdraw *its own* registration on drop without disturbing
    /// its siblings. See the module docs for why the id is not optional.
    waiters: DashMap<String, HashMap<u64, oneshot::Sender<()>>>,
    /// Monotonic source of the per-waiter ids above.
    next_id: AtomicU64,
}

impl ReleaseWaiters {
    /// Builds an empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            waiters: DashMap::new(),
            next_id: AtomicU64::new(0),
        })
    }

    /// Registers interest in `name`'s next release, returning a future that
    /// resolves once [`notify`](Self::notify) fires for that name — or
    /// immediately if this registry is dropped first, which only the plugin's
    /// own teardown can cause and which the caller's timeout covers anyway.
    ///
    /// Dropping the returned future without it resolving deregisters the
    /// waiter, so a caller that gives up (its budget elapsed, or its heartbeat
    /// re-attempt succeeded) leaves nothing behind.
    pub fn wait_for(self: &Arc<Self>, name: &str) -> ReleaseWait {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiters
            .entry(name.to_owned())
            .or_default()
            .insert(id, tx);
        ReleaseWait {
            // A `Weak`, not an `Arc`: a parked waiter must not keep the registry
            // — and through it the plugin's state — alive past the handle's own
            // lifetime. Dropping the registry closes this waiter's sender, which
            // resolves the wait and sends the caller back to its `SET NX`.
            registry: Arc::downgrade(self),
            name: name.to_owned(),
            id,
            rx,
        }
    }

    /// Wakes every waiter registered under `name`.
    ///
    /// Synchronous and allocation-light on purpose: it runs inline on the
    /// subscriber fan-out's read loop, which must never block. A waiter that has
    /// already given up and dropped its receiver is silently skipped.
    pub fn notify(&self, name: &str) {
        if let Some((_name, senders)) = self.waiters.remove(name) {
            for (_id, sender) in senders {
                // `Err` only means the waiter gave up first, which is not an
                // event: it re-attempts the acquire on its own schedule either
                // way. Bound rather than `_` to satisfy `let_underscore_must_use`.
                let _delivered = sender.send(());
            }
        }
    }

    /// The number of live waiters registered under `name`, for the unit tests
    /// that assert registration and deregistration.
    #[cfg(test)]
    fn registered(&self, name: &str) -> usize {
        self.waiters.get(name).map_or(0, |entry| entry.len())
    }

    /// Withdraws waiter `id` under `name` — called from [`ReleaseWait::drop`].
    fn deregister(&self, name: &str, id: u64) {
        let now_empty = {
            let Some(mut entry) = self.waiters.get_mut(name) else {
                return;
            };
            entry.remove(&id);
            entry.is_empty()
        };
        // Prune the now-empty per-name entry, but only if a concurrent
        // `wait_for` has not repopulated it in the gap after the `get_mut` guard
        // was dropped — dropping it first is what avoids a same-shard re-entrant
        // lock.
        if now_empty {
            self.waiters
                .remove_if(name, |_name, waiters| waiters.is_empty());
        }
    }
}

/// A registered blocked-`lock()` waiter, resolving when its name is released.
///
/// Deregisters itself from [`ReleaseWaiters`] on drop, so the acquisition
/// loop's per-attempt `wait_for` cannot accumulate stale senders for a name
/// that is renewed but never released.
pub struct ReleaseWait {
    registry: Weak<ReleaseWaiters>,
    name: String,
    id: u64,
    rx: oneshot::Receiver<()>,
}

impl Future for ReleaseWait {
    /// `Ok(())` — woken by a release for this name. `Err(())` — the sender was
    /// dropped, meaning the registry itself went away. Both mean "stop waiting
    /// and re-attempt the acquire", since the `SET NX` is the source of truth
    /// regardless; the distinction exists only for the unit tests.
    type Output = Result<(), ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|received| received.map_err(|_dropped| ()))
    }
}

impl Drop for ReleaseWait {
    fn drop(&mut self) {
        // Nothing to withdraw from if the registry is already gone.
        if let Some(registry) = self.registry.upgrade() {
            registry.deregister(&self.name, self.id);
        }
    }
}

// Layer-1 unit tests (TESTING.md §2, `lock/waiters.rs` row). Out-of-line per
// DE1101.
#[cfg(test)]
#[path = "waiters_tests.rs"]
mod tests;
