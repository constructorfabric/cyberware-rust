//! The in-process release-waiter registry a blocking `lock()` waits on (DESIGN.md
//! §5.3).
//!
//! A blocking [`lock()`](cluster_sdk::lock::DistributedLockBackend::lock) that finds
//! a name held establishes a watch on that one Lease and waits for the holder to
//! change or clear. When several tasks in *this* process block on the **same** name,
//! they share one watch rather than opening one each: the first subscriber's watch
//! feeds [`wake`](LockWaiters::wake), which unparks every waiter so each re-tries its
//! own guarded acquire. This registry is that sharing, and it is deliberately free
//! of any Kubernetes type so its reference-counting is unit-testable — the backend
//! wires a watch to `wake` and consults [`subscribe`](LockWaiters::subscribe) /
//! [`unsubscribe`](LockWaiters::unsubscribe) to know when to start and stop it.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// One name's shared notifier, its live subscriber count, and the shared watch task
/// feeding [`wake`](LockWaiters::wake).
struct Entry {
    notify: Arc<Notify>,
    subscribers: usize,
    /// The shared release-watch task's handle, attached by the first subscriber and
    /// aborted by whoever removes the last subscriber (§5.3). Owned here rather than
    /// in a waiter's guard so it is torn down even when the first subscriber leaves
    /// before the last — otherwise the last leaver (which never held it) could not
    /// stop it, orphaning the watch until the next event or shutdown.
    watch: Option<JoinHandle<()>>,
}

/// A per-backend registry of blocked `lock()` waiters, keyed by lock name (§5.3).
#[derive(Default)]
pub struct LockWaiters {
    names: DashMap<String, Entry>,
}

impl LockWaiters {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            names: DashMap::new(),
        }
    }

    /// Registers a waiter on `name`, returning the shared [`Notify`] to await and
    /// whether this was the **first** subscriber — the caller starts the watch only
    /// then, so N concurrent waiters share one stream.
    pub fn subscribe(&self, name: &str) -> (Arc<Notify>, bool) {
        let mut entry = self.names.entry(name.to_owned()).or_insert_with(|| Entry {
            notify: Arc::new(Notify::new()),
            subscribers: 0,
            watch: None,
        });
        entry.subscribers += 1;
        (Arc::clone(&entry.notify), entry.subscribers == 1)
    }

    /// Records the shared watch task's handle for `name` (called by the first
    /// subscriber after it spawns the watch), so [`unsubscribe`](Self::unsubscribe)
    /// can abort it when the last waiter leaves. Aborts the handle immediately if the
    /// waiters have already all left (a lost race between spawn and unsubscribe).
    pub fn attach_watch(&self, name: &str, handle: JoinHandle<()>) {
        match self.names.get_mut(name) {
            Some(mut entry) => entry.watch = Some(handle),
            None => handle.abort(),
        }
    }

    /// Deregisters a waiter on `name`, returning whether it was the **last**. On the
    /// last leaver it removes the entry and aborts the shared watch task — so the
    /// watch is stopped regardless of *which* waiter leaves last.
    pub fn unsubscribe(&self, name: &str) -> bool {
        // Decrement inside the `get_mut` scope so the `RefMut` is dropped before we
        // touch the map again (DashMap is re-entrant-lock-prone).
        let now_empty = match self.names.get_mut(name) {
            Some(mut entry) => {
                entry.subscribers = entry.subscribers.saturating_sub(1);
                entry.subscribers == 0
            }
            None => false,
        };
        if !now_empty {
            return false;
        }
        // Remove only while the entry is *still* empty. Between the `RefMut` drop
        // above and here, a concurrent `subscribe` can re-populate this entry
        // (subscribers 0→1, first==true → it spawns a watch and `attach_watch`es
        // it). `remove_if`'s predicate runs under the shard lock, so such a
        // re-subscription keeps its entry and its watch, rather than being silently
        // dropped — which would strand that waiter (its `wake` finds nothing) until
        // its own timeout budget elapses (§5.3). Abort the shared watch only for an
        // entry we actually removed, taking the handle from the removed value so we
        // never steal a live re-subscriber's handle.
        match self
            .names
            .remove_if(name, |_, entry| entry.subscribers == 0)
        {
            Some((_, entry)) => {
                if let Some(handle) = entry.watch {
                    handle.abort();
                }
                true
            }
            None => false,
        }
    }

    /// Wakes every waiter on `name` (the watch observed the holder change or clear),
    /// so each re-tries its guarded acquire. A no-op when nothing is waiting.
    pub fn wake(&self, name: &str) {
        if let Some(entry) = self.names.get(name) {
            entry.notify.notify_waiters();
        }
    }

    /// Whether any task is currently waiting on `name` (drives the shared watch's
    /// lifetime).
    #[must_use]
    pub fn has_waiters(&self, name: &str) -> bool {
        self.names.get(name).is_some_and(|e| e.subscribers > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::LockWaiters;
    use std::time::Duration;

    #[test]
    fn first_and_last_subscriber_are_flagged() {
        let waiters = LockWaiters::new();
        let (_n1, first) = waiters.subscribe("ledger");
        assert!(first, "the first subscriber starts the watch");
        let (_n2, first) = waiters.subscribe("ledger");
        assert!(!first, "a second subscriber shares the existing watch");
        assert!(waiters.has_waiters("ledger"));

        assert!(!waiters.unsubscribe("ledger"), "one waiter remains");
        assert!(
            waiters.unsubscribe("ledger"),
            "the last leaver stops the watch"
        );
        assert!(!waiters.has_waiters("ledger"));
    }

    #[test]
    fn distinct_names_are_independent() {
        let waiters = LockWaiters::new();
        let (_a, a_first) = waiters.subscribe("a");
        let (_b, b_first) = waiters.subscribe("b");
        assert!(
            a_first && b_first,
            "each name's first subscriber starts its own watch"
        );
        assert!(waiters.has_waiters("a") && waiters.has_waiters("b"));
    }

    #[test]
    fn waking_an_unwatched_name_is_a_noop() {
        let waiters = LockWaiters::new();
        waiters.wake("never-subscribed"); // must not panic
        assert!(!waiters.has_waiters("never-subscribed"));
    }

    #[tokio::test]
    async fn wake_unparks_a_registered_waiter() {
        let waiters = LockWaiters::new();
        let (notify, _first) = waiters.subscribe("ledger");
        // Register interest *before* waking (the standard Notify ordering).
        let waited = notify.notified();
        tokio::pin!(waited);
        // Not yet woken.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waited)
                .await
                .is_err()
        );
        waiters.wake("ledger");
        // Now the wake is observed promptly.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waited)
                .await
                .is_ok()
        );
    }
}
