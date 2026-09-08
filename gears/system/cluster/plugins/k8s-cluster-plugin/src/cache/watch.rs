//! The one shared cache watcher and its per-key / per-prefix fan-out registry
//! (DESIGN.md §6.3).
//!
//! The cache costs **exactly one** watch stream no matter how many keys are watched:
//! a single label-selected `watcher` over the whole `ClusterCacheEntry` keyspace in
//! the namespace feeds an in-process [`CacheRegistry`], and every `watch(key)` /
//! `watch_prefix(prefix)` subscriber fans out from it (§3.3, §6.3). The flip side is
//! that the stream carries every cache mutation in the namespace to every instance,
//! which is the cache's binding scalability limit (§12).
//!
//! Two pure stages carry the L1 coverage: [`classify_event`] maps a
//! `watcher::Event<ClusterCacheEntry>` to a keyed [`CacheEvent`] (or a re-list
//! signal), and [`CacheRegistry`] routes an event's key to the exact- and
//! prefix-subscribers that match it.

use dashmap::DashMap;
use k8s_openapi::jiff::Timestamp;
use kube::runtime::watcher::Event;

use cluster_sdk::ClusterError;
use cluster_sdk::cache::{CacheEvent, CacheWatchEvent, CacheWatchSender, CacheWatchTrySendError};

use super::is_expired;
use crate::crd::ClusterCacheEntry;
use crate::naming::ANNOTATION_NAME;

/// What one watcher event means to the cache (§6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheSignal {
    /// A keyed mutation to fan out (`Changed` / `Deleted` / `Expired`).
    Event(CacheEvent),
    /// The watch re-listed (a reconnect); every subscriber is owed a `Reset`.
    Relisted,
    /// A boundary marker (an object without our name annotation, or `InitDone`)
    /// with nothing to fan out.
    Quiet,
}

/// The original cache key an entry was written under, recovered from its
/// `cluster.cf-gears.io/name` annotation (the inverse of the object-name mapping,
/// §2.2). `None` for an object this plugin did not write.
#[must_use]
pub fn key_of(entry: &ClusterCacheEntry) -> Option<String> {
    entry
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_NAME))
        .cloned()
}

/// Maps a `watcher::Event<ClusterCacheEntry>` to a [`CacheSignal`] (§6.3).
///
/// - `Apply` / `InitApply` → [`CacheEvent::Changed`] for the entry's key.
/// - `Delete` past its `expiresAt` → [`CacheEvent::Expired`]; otherwise (an explicit
///   delete, or one still inside its TTL, or an indefinite entry) →
///   [`CacheEvent::Deleted`].
/// - `Init` → [`CacheSignal::Relisted`] (emit `Reset`).
/// - `InitDone`, or an object with no recoverable key → [`CacheSignal::Quiet`].
///
/// The watch carries no value (`CacheEvent` is key-only, §6.3): a subscriber that
/// wants the new value calls `get(key)`. The expired-vs-deleted distinction is read
/// straight off the delete payload's `expiresAt` — the `Delete` event carries the
/// last-known spec, so no side table is needed (§6.3, TESTING §2). It matters to a
/// consumer that treats a TTL lapse (`Expired`) differently from an explicit removal
/// (`Deleted`), and `SC-CACHE-010` asserts a TTL expiry surfaces as `Expired`.
#[must_use]
pub fn classify_event(event: Event<ClusterCacheEntry>) -> CacheSignal {
    match event {
        Event::Apply(entry) | Event::InitApply(entry) => key_of(&entry)
            .map_or(CacheSignal::Quiet, |key| {
                CacheSignal::Event(CacheEvent::Changed { key })
            }),
        Event::Delete(entry) => key_of(&entry).map_or(CacheSignal::Quiet, |key| {
            let event = if is_expired(entry.spec.expires_at.as_deref(), Timestamp::now()) {
                CacheEvent::Expired { key }
            } else {
                CacheEvent::Deleted { key }
            };
            CacheSignal::Event(event)
        }),
        Event::Init => CacheSignal::Relisted,
        Event::InitDone => CacheSignal::Quiet,
    }
}

/// A subscriber's interest: one exact key, or a key prefix (§6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Interest {
    Exact(String),
    Prefix(String),
}

impl Interest {
    /// Whether an event on `key` should be delivered to this subscriber.
    fn matches(&self, key: &str) -> bool {
        match self {
            Self::Exact(exact) => exact == key,
            Self::Prefix(prefix) => key.starts_with(prefix.as_str()),
        }
    }
}

/// One registered subscriber: its interest and the sender to fan events into.
struct Subscriber {
    interest: Interest,
    sender: CacheWatchSender,
    /// Events dropped because this subscriber's buffer was full, drained as a
    /// synthesized [`CacheWatchEvent::Lagged`] the next time delivery succeeds
    /// (the [`CacheWatchSender`] contract: a momentarily slow consumer keeps its
    /// subscription rather than being pruned).
    dropped: std::sync::atomic::AtomicU64,
}

/// Delivers `event` to `subscriber` via `try_send`, first flushing any pending
/// `Lagged` count. Returns `false` **only** when the consumer dropped its watch
/// ([`CacheWatchTrySendError::Closed`]); a momentarily full buffer
/// ([`CacheWatchTrySendError::Full`]) keeps the subscription and records a drop to
/// surface as [`CacheWatchEvent::Lagged`] once space frees (§6.3, mirroring the
/// Postgres plugin's `deliver`).
fn deliver(subscriber: &Subscriber, event: CacheWatchEvent) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let dropped = subscriber.dropped.load(Relaxed);
    if dropped > 0 {
        match subscriber
            .sender
            .try_send(CacheWatchEvent::Lagged { dropped })
        {
            Ok(()) => subscriber.dropped.store(0, Relaxed),
            Err(CacheWatchTrySendError::Full) => {
                subscriber.dropped.fetch_add(1, Relaxed);
                return true;
            }
            Err(CacheWatchTrySendError::Closed) => return false,
        }
    }
    match subscriber.sender.try_send(event) {
        Ok(()) => true,
        Err(CacheWatchTrySendError::Full) => {
            subscriber.dropped.fetch_add(1, Relaxed);
            true
        }
        Err(CacheWatchTrySendError::Closed) => false,
    }
}

/// The in-process fan-out registry fed by the single shared cache watcher (§6.3).
#[derive(Default)]
pub struct CacheRegistry {
    /// A monotonic id per subscriber, so a dropped watch can deregister precisely.
    subscribers: DashMap<u64, Subscriber>,
    next_id: std::sync::atomic::AtomicU64,
}

impl CacheRegistry {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an exact-key subscriber, returning its deregistration id.
    pub fn subscribe_key(&self, key: &str, sender: CacheWatchSender) -> u64 {
        self.insert(Interest::Exact(key.to_owned()), sender)
    }

    /// Registers a prefix subscriber, returning its deregistration id.
    pub fn subscribe_prefix(&self, prefix: &str, sender: CacheWatchSender) -> u64 {
        self.insert(Interest::Prefix(prefix.to_owned()), sender)
    }

    fn insert(&self, interest: Interest, sender: CacheWatchSender) -> u64 {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.subscribers.insert(
            id,
            Subscriber {
                interest,
                sender,
                dropped: std::sync::atomic::AtomicU64::new(0),
            },
        );
        id
    }

    /// Deregisters a subscriber by its id (its consumer dropped the watch).
    ///
    /// A dropped [`CacheWatch`] is pruned lazily instead — the next
    /// [`dispatch`](Self::dispatch) drops any subscriber whose channel has closed —
    /// so this explicit path is retained for the registry's own tests and a future
    /// eager-deregistration wiring rather than called on the hot path.
    #[allow(dead_code)]
    pub fn unsubscribe(&self, id: u64) {
        self.subscribers.remove(&id);
    }

    /// The number of live subscribers.
    #[allow(dead_code)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.subscribers.len()
    }

    /// Whether the registry has no subscribers.
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subscribers.is_empty()
    }

    /// Fans a keyed [`CacheEvent`] out to every matching subscriber, pruning only
    /// those whose consumer has dropped its watch. A subscriber whose buffer is
    /// momentarily full is kept and owed a [`CacheWatchEvent::Lagged`] (§6.3).
    pub fn dispatch(&self, event: &CacheEvent) {
        let key = event.key();
        let mut dead = Vec::new();
        for entry in &self.subscribers {
            let subscriber = entry.value();
            if subscriber.interest.matches(key)
                && !deliver(subscriber, CacheWatchEvent::Event(event.clone()))
            {
                dead.push(*entry.key());
            }
        }
        for id in dead {
            self.subscribers.remove(&id);
        }
    }

    /// Delivers `Reset` to every subscriber after a re-list (§6.3).
    pub fn broadcast_reset(&self) {
        for entry in &self.subscribers {
            let _sent = entry.value().sender.try_send(CacheWatchEvent::Reset);
        }
    }

    /// Delivers a terminal `Closed(err)` to every active subscriber and drops them
    /// all (DESIGN.md §11 step 3).
    ///
    /// Called from [`K8sCache::stop`](crate::cache::K8sCache::stop) *before* the
    /// shared watcher task is cancelled: the watcher itself just returns on
    /// `shutdown`, so without this pass a subscriber would observe a silent
    /// end-of-stream instead of the `Closed(Shutdown)` the contract promises.
    /// Best-effort per subscriber — a full buffer or a dropped consumer is ignored,
    /// since dropping the sender (as `clear` does) ends the stream regardless.
    pub fn broadcast_closed(&self, err: &ClusterError) {
        for entry in &self.subscribers {
            let _sent = entry
                .value()
                .sender
                .try_send(CacheWatchEvent::Closed(err.clone()));
        }
        // Drop every sender: an active watch that could not receive the `Closed`
        // above still ends its stream, and no post-shutdown event can reference a
        // stale subscriber.
        self.subscribers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheRegistry, CacheSignal, classify_event, key_of};
    use crate::crd::{ClusterCacheEntry, ClusterCacheEntrySpec};
    use cluster_sdk::cache::{CacheEvent, CacheWatch, CacheWatchEvent};
    use kube::runtime::watcher::Event;
    use std::collections::BTreeMap;

    fn entry_named(key: Option<&str>) -> ClusterCacheEntry {
        entry_named_expiring(key, None)
    }

    fn entry_named_expiring(key: Option<&str>, expires_at: Option<&str>) -> ClusterCacheEntry {
        let mut entry = ClusterCacheEntry::new(
            "obj",
            ClusterCacheEntrySpec::new(b"v", 1, expires_at.map(str::to_owned)),
        );
        if let Some(key) = key {
            entry.metadata.annotations = Some(BTreeMap::from([(
                crate::naming::ANNOTATION_NAME.to_owned(),
                key.to_owned(),
            )]));
        }
        entry
    }

    #[test]
    fn apply_and_delete_map_to_keyed_events() {
        assert_eq!(
            classify_event(Event::Apply(entry_named(Some("shard/7")))),
            CacheSignal::Event(CacheEvent::Changed {
                key: "shard/7".to_owned()
            })
        );
        // A delete of an entry with no expiry, or one still inside its TTL, is a
        // plain `Deleted`.
        assert_eq!(
            classify_event(Event::Delete(entry_named(Some("shard/7")))),
            CacheSignal::Event(CacheEvent::Deleted {
                key: "shard/7".to_owned()
            })
        );
        let future = "2999-01-01T00:00:00Z";
        assert_eq!(
            classify_event(Event::Delete(entry_named_expiring(
                Some("shard/7"),
                Some(future)
            ))),
            CacheSignal::Event(CacheEvent::Deleted {
                key: "shard/7".to_owned()
            })
        );
    }

    /// A `Delete` whose `expiresAt` is already in the past is a TTL lapse, not an
    /// explicit removal — it maps to `Expired` (SC-CACHE-010, DESIGN §6.3).
    #[test]
    fn delete_past_expiry_maps_to_expired() {
        let past = "2000-01-01T00:00:00Z";
        assert_eq!(
            classify_event(Event::Delete(entry_named_expiring(
                Some("shard/7"),
                Some(past)
            ))),
            CacheSignal::Event(CacheEvent::Expired {
                key: "shard/7".to_owned()
            })
        );
    }

    #[test]
    fn relist_boundaries_and_unkeyed_objects_are_signalled() {
        assert_eq!(classify_event(Event::Init), CacheSignal::Relisted);
        assert_eq!(classify_event(Event::InitDone), CacheSignal::Quiet);
        // An object without our name annotation cannot be keyed → Quiet.
        assert_eq!(
            classify_event(Event::Apply(entry_named(None))),
            CacheSignal::Quiet
        );
        assert_eq!(key_of(&entry_named(None)), None);
    }

    #[tokio::test]
    async fn dispatch_routes_to_exact_and_prefix_subscribers_only() {
        let registry = CacheRegistry::new();
        let (exact_tx, mut exact) = CacheWatch::channel(8);
        let (prefix_tx, mut prefix) = CacheWatch::channel(8);
        let (other_tx, mut other) = CacheWatch::channel(8);

        registry.subscribe_key("shard/7", exact_tx);
        registry.subscribe_prefix("shard/", prefix_tx);
        registry.subscribe_key("unrelated", other_tx);

        registry.dispatch(&CacheEvent::Changed {
            key: "shard/7".to_owned(),
        });

        // Exact and prefix both see it; the unrelated key does not.
        assert!(matches!(
            exact.recv().await,
            Some(CacheWatchEvent::Event(CacheEvent::Changed { key })) if key == "shard/7"
        ));
        assert!(matches!(
            prefix.recv().await,
            Some(CacheWatchEvent::Event(CacheEvent::Changed { .. }))
        ));
        // The unrelated subscriber has nothing buffered.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), other.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_dropped_subscriber_is_pruned_on_dispatch() {
        let registry = CacheRegistry::new();
        let (tx, watch) = CacheWatch::channel(8);
        registry.subscribe_prefix("p/", tx);
        assert_eq!(registry.len(), 1);
        drop(watch); // consumer gone

        registry.dispatch(&CacheEvent::Changed {
            key: "p/1".to_owned(),
        });
        assert!(registry.is_empty(), "a closed subscriber is pruned");
    }

    #[tokio::test]
    async fn broadcast_closed_delivers_a_terminal_close_and_clears() {
        use cluster_sdk::ClusterError;

        let registry = CacheRegistry::new();
        let (a, mut wa) = CacheWatch::channel(8);
        let (b, mut wb) = CacheWatch::channel(8);
        registry.subscribe_key("a", a);
        registry.subscribe_prefix("p/", b);
        assert_eq!(registry.len(), 2);

        registry.broadcast_closed(&ClusterError::Shutdown);

        // Every subscriber observes the terminal `Closed(Shutdown)` ...
        assert!(matches!(
            wa.recv().await,
            Some(CacheWatchEvent::Closed(ClusterError::Shutdown))
        ));
        assert!(matches!(
            wb.recv().await,
            Some(CacheWatchEvent::Closed(ClusterError::Shutdown))
        ));
        // ... and the registry is emptied so no later event references them.
        assert!(registry.is_empty());
    }

    #[test]
    fn unsubscribe_removes_precisely() {
        let registry = CacheRegistry::new();
        let (a, _wa) = CacheWatch::channel(1);
        let (b, _wb) = CacheWatch::channel(1);
        let id_a = registry.subscribe_key("a", a);
        let _id_b = registry.subscribe_key("b", b);
        assert_eq!(registry.len(), 2);
        registry.unsubscribe(id_a);
        assert_eq!(registry.len(), 1);
    }
}
