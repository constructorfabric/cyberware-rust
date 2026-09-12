// Created: 2026-06-24 by Constructor Tech
//! [`MemCache`]: a compact, linearizable in-process [`ClusterCacheBackend`] the
//! conformance suite self-tests against, and a reference fixture for the three
//! cache-derived suites (leader/lock/discovery feed it through the SDK
//! `CasBased*`/`CacheBased*` defaults).
//!
//! It implements real versioning (monotonic per key, reset to `1` on a fresh
//! create), lazy + swept TTL expiry that emits [`CacheEvent::Expired`], exact
//! and prefix watches, and an **atomic** [`compare_and_delete`] so the
//! owner-token guard (SC-CACHE-008/009) holds under contention rather than
//! relying on the best-effort get-then-delete default.
//!
//! It is deliberately compact — one state map behind one mutex — and makes no
//! attempt at the durability or partition tolerance a real backend needs. It
//! exists to (a) prove the suite's assertions pass against a known-correct
//! backend and (b) give cache-only behavior a backend in tests that don't stand
//! up a container.
//!
//! [`compare_and_delete`]: ClusterCacheBackend::compare_and_delete

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use cluster_sdk::cache::{
    CacheConsistency, CacheEntry, CacheEvent, CacheFeatures, CacheWatch, CacheWatchEvent,
    CacheWatchSender, ClusterCacheBackend, PutRequest, Ttl,
};
use cluster_sdk::error::ClusterError;
use parking_lot::Mutex;
use tokio::time::Instant;

/// How often the background sweeper scans for expired entries. Fine enough that
/// a `tokio::time::advance` past a TTL deterministically triggers an `Expired`
/// emission, coarse enough to avoid needless wakeups.
const SWEEP_INTERVAL: Duration = Duration::from_millis(25);

/// Per-watch in-flight buffer. Generous so a renewal/heartbeat storm in a test
/// never spuriously drops events.
const WATCH_CAPACITY: usize = 256;

/// A stored value with its version and optional expiry deadline.
struct Stored {
    value: Vec<u8>,
    version: u64,
    expires_at: Option<Instant>,
}

impl Stored {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|deadline| deadline <= now)
    }

    fn entry(&self) -> CacheEntry {
        CacheEntry {
            value: self.value.clone(),
            version: self.version,
        }
    }
}

/// The subscription kind a watcher matches keys against.
enum WatchKind {
    Exact(String),
    Prefix(String),
}

impl WatchKind {
    fn matches(&self, key: &str) -> bool {
        match self {
            Self::Exact(exact) => exact == key,
            Self::Prefix(prefix) => key.starts_with(prefix.as_str()),
        }
    }
}

/// One live watch subscription, identified so a failed send can prune it.
struct Watcher {
    id: u64,
    kind: WatchKind,
    sender: CacheWatchSender,
}

/// The fixture's locked interior: a single state map and a single monotonic
/// version source, plus the live watchers.
struct Inner {
    map: HashMap<String, Stored>,
    watchers: Vec<Watcher>,
    next_watch_id: u64,
}

/// A compact, linearizable in-memory [`ClusterCacheBackend`] for the
/// conformance suite. See the module docs: a fixture, not a production backend.
pub struct MemCache {
    inner: Mutex<Inner>,
    consistency: CacheConsistency,
    prefix_watch: bool,
    watch: bool,
}

impl MemCache {
    /// A linearizable cache with native prefix-watch support — the default
    /// fixture, which satisfies every capability-gated assertion in the suite.
    ///
    /// # Panics
    /// Panics if called outside the context of a Tokio runtime (the
    /// constructor spawns a background TTL sweeper task).
    #[must_use]
    pub fn linearizable() -> Arc<Self> {
        Self::spawn(CacheConsistency::Linearizable, true, true)
    }

    /// An eventually-consistent cache, for exercising the suite's capability
    /// gating (linearizable-only assertions must be skipped, not failed) and the
    /// default-backend consistency guard (ADR-009).
    ///
    /// # Panics
    /// Panics if called outside the context of a Tokio runtime (the
    /// constructor spawns a background TTL sweeper task).
    #[must_use]
    pub fn eventually_consistent() -> Arc<Self> {
        Self::spawn(CacheConsistency::EventuallyConsistent, true, true)
    }

    /// A linearizable cache that declares no native prefix watch, so
    /// `watch_prefix` returns [`ClusterError::Unsupported`] — for the
    /// prefix-watch capability gate and the polling polyfill.
    ///
    /// # Panics
    /// Panics if called outside the context of a Tokio runtime (the
    /// constructor spawns a background TTL sweeper task).
    #[must_use]
    pub fn linearizable_without_prefix_watch() -> Arc<Self> {
        Self::spawn(CacheConsistency::Linearizable, false, true)
    }

    /// A linearizable cache that serves no exact watch, so `features().watch` is
    /// `false` and both `watch` and `watch_prefix` return
    /// [`ClusterError::Unsupported`] — for the exact-watch capability gate (a
    /// backend that cannot watch one key cannot watch a family either). Models a
    /// real watchless deployment such as redis `watch_mode: disabled`.
    ///
    /// # Panics
    /// Panics if called outside the context of a Tokio runtime (the
    /// constructor spawns a background TTL sweeper task).
    #[must_use]
    pub fn linearizable_without_watch() -> Arc<Self> {
        Self::spawn(CacheConsistency::Linearizable, false, false)
    }

    fn spawn(consistency: CacheConsistency, prefix_watch: bool, watch: bool) -> Arc<Self> {
        let cache = Arc::new(Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                watchers: Vec::new(),
                next_watch_id: 0,
            }),
            consistency,
            prefix_watch,
            watch,
        });
        // The sweeper holds only a weak reference, so it self-terminates once
        // the test drops the cache.
        let weak = Arc::downgrade(&cache);
        tokio::spawn(sweep_loop(weak));
        cache
    }

    /// Sends `event` to every watcher matching `key`, pruning any whose consumer
    /// has dropped the watch.
    async fn broadcast(&self, key: &str, event: CacheEvent) {
        let targets: Vec<(u64, CacheWatchSender)> = {
            let guard = self.inner.lock();
            guard
                .watchers
                .iter()
                .filter(|watcher| watcher.kind.matches(key))
                .map(|watcher| (watcher.id, watcher.sender.clone()))
                .collect()
        };
        let mut dead = Vec::new();
        for (id, sender) in targets {
            if sender
                .send(CacheWatchEvent::Event(event.clone()))
                .await
                .is_err()
            {
                dead.push(id);
            }
        }
        if !dead.is_empty() {
            self.inner
                .lock()
                .watchers
                .retain(|watcher| !dead.contains(&watcher.id));
        }
    }

    /// Removes every expired entry and emits an `Expired` event for each.
    async fn sweep_expired(&self) {
        let now = Instant::now();
        let expired: Vec<String> = {
            let mut guard = self.inner.lock();
            let keys: Vec<String> = guard
                .map
                .iter()
                .filter(|(_, stored)| stored.is_expired(now))
                .map(|(key, _)| key.clone())
                .collect();
            for key in &keys {
                guard.map.remove(key);
            }
            keys
        };
        for key in expired {
            self.broadcast(&key, CacheEvent::Expired { key: key.clone() })
                .await;
        }
    }

    fn register_watch(&self, kind: WatchKind) -> CacheWatch {
        let (sender, watch) = CacheWatch::channel(WATCH_CAPACITY);
        let mut guard = self.inner.lock();
        let id = guard.next_watch_id;
        guard.next_watch_id += 1;
        guard.watchers.push(Watcher { id, kind, sender });
        watch
    }
}

/// The detached sweeper driving TTL expiry; exits once the cache is dropped.
async fn sweep_loop(weak: Weak<MemCache>) {
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        ticker.tick().await;
        let Some(cache) = weak.upgrade() else {
            return;
        };
        cache.sweep_expired().await;
    }
}

#[async_trait]
impl ClusterCacheBackend for MemCache {
    fn consistency(&self) -> CacheConsistency {
        self.consistency
    }

    fn features(&self) -> CacheFeatures {
        if self.watch {
            CacheFeatures::new(self.prefix_watch)
        } else {
            // No exact watch implies no prefix watch either.
            CacheFeatures::without_watch()
        }
    }

    async fn get(&self, key: &str) -> Result<Option<CacheEntry>, ClusterError> {
        let now = Instant::now();
        let guard = self.inner.lock();
        Ok(match guard.map.get(key) {
            Some(stored) if !stored.is_expired(now) => Some(stored.entry()),
            _ => None,
        })
    }

    async fn put(&self, req: PutRequest<'_>) -> Result<(), ClusterError> {
        let PutRequest { key, value, ttl } = req;
        let now = Instant::now();
        {
            let mut guard = self.inner.lock();
            let version = match guard.map.get(key) {
                Some(stored) if !stored.is_expired(now) => stored.version + 1,
                _ => 1,
            };
            guard.map.insert(
                key.to_owned(),
                Stored {
                    value: value.to_vec(),
                    version,
                    expires_at: ttl.as_duration().map(|d| now + d),
                },
            );
        }
        self.broadcast(
            key,
            CacheEvent::Changed {
                key: key.to_owned(),
            },
        )
        .await;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<bool, ClusterError> {
        let now = Instant::now();
        let was_live = {
            let mut guard = self.inner.lock();
            let live = matches!(guard.map.get(key), Some(stored) if !stored.is_expired(now));
            guard.map.remove(key);
            live
        };
        if was_live {
            self.broadcast(
                key,
                CacheEvent::Deleted {
                    key: key.to_owned(),
                },
            )
            .await;
        }
        Ok(was_live)
    }

    async fn contains(&self, key: &str) -> Result<bool, ClusterError> {
        let now = Instant::now();
        let guard = self.inner.lock();
        Ok(matches!(guard.map.get(key), Some(stored) if !stored.is_expired(now)))
    }

    async fn put_if_absent(&self, req: PutRequest<'_>) -> Result<Option<CacheEntry>, ClusterError> {
        let PutRequest { key, value, ttl } = req;
        let now = Instant::now();
        let created = {
            let mut guard = self.inner.lock();
            if matches!(guard.map.get(key), Some(stored) if !stored.is_expired(now)) {
                None
            } else {
                let stored = Stored {
                    value: value.to_vec(),
                    version: 1,
                    expires_at: ttl.as_duration().map(|d| now + d),
                };
                let entry = stored.entry();
                guard.map.insert(key.to_owned(), stored);
                Some(entry)
            }
        };
        if created.is_some() {
            self.broadcast(
                key,
                CacheEvent::Changed {
                    key: key.to_owned(),
                },
            )
            .await;
        }
        Ok(created)
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_version: u64,
        new_value: &[u8],
        ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        let now = Instant::now();
        let outcome = {
            let mut guard = self.inner.lock();
            match guard.map.get(key) {
                Some(stored) if !stored.is_expired(now) => {
                    if stored.version == expected_version {
                        let version = stored.version + 1;
                        let stored = Stored {
                            value: new_value.to_vec(),
                            version,
                            expires_at: ttl.as_duration().map(|d| now + d),
                        };
                        let entry = stored.entry();
                        guard.map.insert(key.to_owned(), stored);
                        Ok(entry)
                    } else {
                        Err(ClusterError::CasConflict {
                            key: key.to_owned(),
                            current: Some(stored.entry()),
                        })
                    }
                }
                _ => Err(ClusterError::CasConflict {
                    key: key.to_owned(),
                    current: None,
                }),
            }
        };
        if outcome.is_ok() {
            self.broadcast(
                key,
                CacheEvent::Changed {
                    key: key.to_owned(),
                },
            )
            .await;
        }
        outcome
    }

    /// Atomic owner-token-guarded delete: removes `key` only if its current
    /// value equals `expected_value`, under the same lock as the read, so a
    /// successor that re-created the key (resetting its version to 1) is not
    /// aliased by a stale claim (SC-CACHE-009).
    async fn compare_and_delete(
        &self,
        key: &str,
        expected_value: &[u8],
    ) -> Result<bool, ClusterError> {
        let now = Instant::now();
        let deleted = {
            let mut guard = self.inner.lock();
            match guard.map.get(key) {
                Some(stored)
                    if !stored.is_expired(now) && stored.value.as_slice() == expected_value =>
                {
                    guard.map.remove(key);
                    true
                }
                _ => false,
            }
        };
        if deleted {
            self.broadcast(
                key,
                CacheEvent::Deleted {
                    key: key.to_owned(),
                },
            )
            .await;
        }
        Ok(deleted)
    }

    async fn watch(&self, key: &str) -> Result<CacheWatch, ClusterError> {
        if !self.watch {
            return Err(ClusterError::Unsupported { feature: "watch" });
        }
        Ok(self.register_watch(WatchKind::Exact(key.to_owned())))
    }

    async fn watch_prefix(&self, prefix: &str) -> Result<CacheWatch, ClusterError> {
        if !self.prefix_watch {
            return Err(ClusterError::Unsupported {
                feature: "prefix_watch",
            });
        }
        Ok(self.register_watch(WatchKind::Prefix(prefix.to_owned())))
    }

    async fn scan_prefix(&self, prefix: &str) -> Result<Vec<String>, ClusterError> {
        let now = Instant::now();
        let guard = self.inner.lock();
        Ok(guard
            .map
            .iter()
            .filter(|(key, stored)| key.starts_with(prefix) && !stored.is_expired(now))
            .map(|(key, _)| key.clone())
            .collect())
    }
}
