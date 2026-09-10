//! The watcher registry and the subscriber fan-out (DESIGN.md §4.3).
//!
//! Ported from `postgres-cluster-plugin/src/cache/watch.rs` — the slot/`Lagged`
//! bookkeeping, the `closed` latch, and the mutex serializing terminal broadcasts
//! are that registry's, and the reasoning recorded on each is its reasoning.
//! Copied rather than hoisted into `cluster-sdk`: sharing it would mean new
//! public SDK surface, a refactor of a shipped crate, and postgres regression
//! risk, and the real overlap across three implementations is worth seeing before
//! it is factored. What is new here is
//! everything to do with *subscriptions*: Postgres has one static `LISTEN`
//! channel, and Redis has one channel per watched key, one pattern per watched
//! prefix, and a keyspace pattern besides.
//!
//! ## Three channel families, and why each message reaches each watcher once
//!
//! ```text
//! plugin PUBLISH (in-script)  ──►  <prefix>:e:c:<key>       ──►  message_rx        ──┐
//! Redis `expired`  keyspace   ──►  __keyspace@<db>__:<key>  ──►  keyspace_event_rx ──┼─► fan-out ─► watchers
//! Redis `evicted`  keyspace   ──►  __keyspace@<db>__:<key>  ──►  keyspace_event_rx ──┘
//! ```
//!
//! Three families but **two streams**, and that is a `fred` behaviour rather
//! than a design choice: its router recognizes the `__keyspace@`/`__keyevent@`
//! channel prefixes and diverts those messages to `keyspace_event_rx()`,
//! pre-parsed into a `KeyspaceEvent`, so they never appear on `message_rx()` at
//! all (`fred/src/router/responses.rs`). A fan-out reading only the pub/sub
//! stream — the obvious reading of DESIGN.md §4.3's diagram — subscribes to the
//! keyspace pattern successfully, sees the server deliver on it, and still
//! never emits a single `Expired`.
//!
//! A key with both an exact watcher and a covering prefix watcher is subscribed
//! **twice** on the server — once by `SUBSCRIBE <prefix>:e:c:<key>` and once by
//! the prefix's `PSUBSCRIBE` — so one `PUBLISH` produces two deliveries to this
//! client. Redis distinguishes them (`message` versus `pmessage`) and so does
//! this fan-out: an exact-subscription message is routed only to exact watchers,
//! and a pattern message only to prefix watchers. Each watcher therefore sees
//! exactly one event per write, which is what `RD-WATCH-001` pins and what
//! `cpt-cf-clst-nfr-watch-delivery`'s "zero duplicate events per subscriber per
//! key" requires. Routing both message kinds to both sets — the obvious
//! implementation — would deliver two `Changed`s for one `put` to any watcher
//! whose key is covered both ways.
//!
//! The keyspace family has no such twin: it is one blanket pattern, so its
//! messages go to both sets.
//!
//! ## Subscriptions are reference-counted, and the counting is serialized
//!
//! `SUBSCRIBE` on the first watcher of a key and `UNSUBSCRIBE` when the last one
//! goes away, so N consumers watching one prefix cost one Redis pattern
//! (`RD-WATCH-005`) and a key nobody watches any more stops costing a message
//! per write. Both decisions run under [`WatchRegistry::subscriptions`], and
//! that mutex is load-bearing rather than defensive: pruning notices emptiness
//! on the delivery path while registration inserts on the caller's, so without
//! serialization an `UNSUBSCRIBE` decided just before a new `watch()` can land
//! just after its `SUBSCRIBE`, leaving a registered watcher with no server-side
//! subscription and no event ever again. Under the mutex, "still empty" is
//! authoritative.
//!
//! It is not on the hot path: delivery never takes it. Only `watch`,
//! `watch_prefix`, and the rare prune-to-empty do.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cluster_sdk::observability::ResourceId;
use cluster_sdk::{
    CacheEvent, CacheWatch, CacheWatchEvent, CacheWatchSender, CacheWatchTrySendError, ClusterError,
};
use dashmap::DashMap;
use fred::clients::SubscriberClient;
use fred::interfaces::PubsubInterface;
use fred::types::Value;

use crate::observability::RedisSignals;
use crate::redis_error::map_redis_error;
use crate::subscriber::confirm_subscriptions;

/// The per-watcher buffer, in events (DESIGN.md §4.3).
///
/// Bounded on purpose: the fan-out never awaits a slow watcher, so a full buffer
/// has to drop and report rather than apply backpressure to the write path. 64
/// is deep enough that an ordinarily-scheduled consumer never sees `Lagged`, and
/// shallow enough that a stalled one is reported promptly instead of
/// accumulating unbounded memory per watcher.
const WATCH_BUFFER: usize = 64;

/// Upper bound on how long one terminal delivery waits for a full consumer to
/// free a buffer slot before giving up.
///
/// The terminal `Reset`/`Closed` is delivered with a blocking `send` rather than
/// the fan-out's `try_send`, so a watcher whose buffer is momentarily full still
/// gets the *typed* event instead of a bare channel close it cannot tell apart
/// from a dropped sender. A consumer that has stopped draining altogether must
/// not stall `stop()` on that, hence the bound.
const TERMINAL_GRACE: Duration = Duration::from_secs(5);

/// The payload a mutation script publishes (DESIGN.md §2.5).
///
/// One character, because the channel already carries the key and the SDK's
/// cache events are key-only by contract. There is no `Expired` payload:
/// nothing runs plugin code when a TTL lapses, so that one event can only come
/// from Redis's own keyspace notification.
const PAYLOAD_CHANGED: &str = "C";
/// See [`PAYLOAD_CHANGED`].
const PAYLOAD_DELETED: &str = "D";

/// The Redis keyspace-notification event name for a lapsed TTL.
const KEYSPACE_EXPIRED: &str = "expired";
/// The Redis keyspace-notification event name for a key `maxmemory` pressure
/// removed.
const KEYSPACE_EVICTED: &str = "evicted";

/// What the fan-out decided a raw message means (DESIGN.md §2.5, §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedNotification {
    /// The key was created or updated.
    Changed {
        /// The affected key, with the plugin's prefix already stripped.
        key: String,
    },
    /// The key was deleted — by an explicit delete, or by an eviction.
    Deleted {
        /// The affected key.
        key: String,
    },
    /// The key's TTL lapsed.
    Expired {
        /// The affected key.
        key: String,
    },
    /// Something arrived on this key's channel that the plugin cannot
    /// interpret.
    ///
    /// Delivered as a `Reset` to every watcher **on that key**, per ADR-003's
    /// mapping for an unintelligible backend signal and DESIGN.md §2.5: it costs
    /// a spurious re-read and cannot be mistaken for a real event. Distinct from
    /// the registry-wide `Reset` of [`WatchRegistry::broadcast_reset`], which
    /// says the whole subscription gapped.
    Reset {
        /// The key whose channel carried the unintelligible payload.
        key: String,
    },
    /// A keyspace notification outside this plugin's vocabulary, dropped.
    ///
    /// Not a `Reset`: these arrive only when the server's `notify-keyspace-events`
    /// is configured more widely than the plugin asked for, and there is one per
    /// Redis *command* rather than per logical write — so turning them into
    /// resets would make an over-configured server cost a re-read on every
    /// mutation.
    Ignored,
}

/// Maps a published payload to its event (DESIGN.md §2.5).
///
/// An unrecognized payload is [`ParsedNotification::Reset`] rather than a
/// discarded message: it means either an unrelated publisher on this plugin's
/// channel or a future payload format this version does not know, and in both
/// cases a re-read is the safe answer.
#[must_use]
pub fn parse_publish_payload(key: &str, payload: &Value) -> ParsedNotification {
    match payload.as_str().as_deref() {
        Some(PAYLOAD_CHANGED) => ParsedNotification::Changed {
            key: key.to_owned(),
        },
        Some(PAYLOAD_DELETED) => ParsedNotification::Deleted {
            key: key.to_owned(),
        },
        _ => ParsedNotification::Reset {
            key: key.to_owned(),
        },
    }
}

/// Whether `operation` is Redis's own eviction notification (DESIGN.md §3.7).
///
/// Separate from [`parse_keyspace_event`] because that function deliberately
/// erases the distinction — an eviction is delivered to watchers as `Deleted` —
/// while the operational signal needs precisely the distinction it erased: an
/// `expired` is a TTL doing its job, and an `evicted` is memory pressure
/// removing a key nobody asked it to.
#[must_use]
pub fn is_eviction(operation: &str) -> bool {
    operation == KEYSPACE_EVICTED
}

/// Maps a Redis keyspace-notification operation to its cache event.
///
/// **`evicted` becomes `Deleted`, not `Expired`** (DESIGN.md §3.7). No TTL
/// lapsed — `maxmemory` pressure removed a key nobody asked to remove — and a
/// consumer distinguishing the two would be told the entry aged out when in
/// fact its instance is misconfigured. `Deleted` is the truthful one of the two
/// the SDK offers.
#[must_use]
pub fn parse_keyspace_event(key: &str, operation: &str) -> ParsedNotification {
    match operation {
        KEYSPACE_EXPIRED => ParsedNotification::Expired {
            key: key.to_owned(),
        },
        KEYSPACE_EVICTED => ParsedNotification::Deleted {
            key: key.to_owned(),
        },
        // `del`, `hset`, `rename_from` and the rest of the keyspace vocabulary
        // reach here only if the server's flags are wider than this plugin asked
        // for. Reporting them would double every mutation, since the in-script
        // publish already carried it, so they are dropped outright rather than
        // turned into a `Reset` that would make every write cost a re-read.
        _ => ParsedNotification::Ignored,
    }
}

/// One registered watcher: the sender plus a count of events dropped because
/// its buffer was full, drained as a synthesized [`CacheWatchEvent::Lagged`] the
/// next time a delivery to it succeeds.
struct WatcherSlot {
    /// Process-unique, so [`WatchRegistry::register`] can withdraw *its own*
    /// slot when it loses the race against a terminal close.
    id: u64,
    sender: CacheWatchSender,
    dropped: AtomicU64,
}

/// Delivers `event` to `slot` via `try_send` — a fan-out path must never block
/// on one slow consumer — first flushing any pending `Lagged` count. Returns
/// `false` when the slot should be pruned because its consumer dropped the
/// watch.
///
/// The `Lagged` rides the next successful send rather than being emitted the
/// moment the buffer drains, because nothing polls a drained buffer. A watcher
/// that lags and then sees no further traffic on its key never learns it lagged
/// — a real limitation, stated rather than papered over (DESIGN.md §4.3), and the
/// reason `RD-WATCH-008` drives enough writes to make it fire.
///
/// `cluster_redis_watch_events_dropped_total` is incremented once per dropped
/// event, including the drop of a `Lagged` that itself found no room. That is
/// what keeps the counter equal to the `dropped` count the consumer eventually
/// receives, which is the equality `RD-WATCH-008` asserts.
fn deliver(slot: &WatcherSlot, event: CacheWatchEvent, signals: &RedisSignals) -> bool {
    let dropped = slot.dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        match slot.sender.try_send(CacheWatchEvent::Lagged { dropped }) {
            Ok(()) => slot.dropped.store(0, Ordering::Relaxed),
            Err(CacheWatchTrySendError::Full) => {
                slot.dropped.fetch_add(1, Ordering::Relaxed);
                signals.watch_event_dropped();
                return true;
            }
            Err(CacheWatchTrySendError::Closed) => return false,
        }
    }
    match slot.sender.try_send(event) {
        Ok(()) => true,
        Err(CacheWatchTrySendError::Full) => {
            slot.dropped.fetch_add(1, Ordering::Relaxed);
            signals.watch_event_dropped();
            true
        }
        Err(CacheWatchTrySendError::Closed) => false,
    }
}

/// Which family of watchers a message is for — see the module docs on why one
/// message never reaches both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherKind {
    /// Watchers registered through `watch(key)`, served by an exact
    /// `SUBSCRIBE`.
    Exact,
    /// Watchers registered through `watch_prefix(prefix)`, served by a
    /// `PSUBSCRIBE`.
    Prefix,
    /// Both — the keyspace family, which has a single blanket pattern and so
    /// cannot double-deliver.
    Both,
}

/// Registry of active watchers, plus the subscription bookkeeping that keeps the
/// server-side channel and pattern sets in step with it.
pub struct WatchRegistry {
    /// Exact-key watchers, keyed by the consumer's key.
    keys: DashMap<String, Vec<WatcherSlot>>,
    /// Prefix watchers, keyed by the consumer's prefix.
    prefixes: DashMap<String, Vec<WatcherSlot>>,
    /// Source of [`WatcherSlot::id`].
    next_id: AtomicU64,
    /// The subscriber this registry drives, and the serialization point for
    /// every `SUBSCRIBE`/`UNSUBSCRIBE` it issues. See the module docs.
    ///
    /// `None` under `watch_mode: disabled`, where no subscriber exists — but
    /// that path never reaches the registry, since `watch`/`watch_prefix` answer
    /// `Unsupported` before registering.
    subscriptions: tokio::sync::Mutex<Option<SubscriberClient>>,
    /// Serializes every terminal broadcast — the `Reset` fan-out and
    /// [`close_all`](Self::close_all) — against each other.
    ///
    /// Without it those two interleave and both interleavings are wrong. A
    /// `Reset` broadcast that had already collected its senders when `close_all`
    /// ran would send on the same channels *after* the terminal
    /// `Closed(Shutdown)`, which the SDK's `CacheWatch` contract forbids. The
    /// other order is no better: the `Reset` empties the maps first, so
    /// `close_all` finds nothing and delivers no terminal event at all.
    terminal: tokio::sync::Mutex<()>,
    /// Set, under `terminal`, by the first terminal broadcast, to the very error
    /// that broadcast delivered. A registry that has closed stays closed.
    ///
    /// It holds the error rather than a bare flag because *which* error closed
    /// the registry is load-bearing: `close_all` closes with
    /// [`ClusterError::Shutdown`], while an exhausted reconnect budget closes
    /// with `Provider { ConnectionLost }`, and only the latter is
    /// [`ClusterError::is_retryable`]. A late registration answered with a
    /// hardcoded `Shutdown` would tell the SDK's `RestartingWatch` combinator
    /// that the subsystem is going away when the truth is a lost connection, so
    /// the consumer's own retry policy would never get to run.
    closed: std::sync::OnceLock<ClusterError>,
    /// The plugin's signal sink: the dropped-event counter on the delivery path,
    /// and `cluster.provider.error` for an `UNSUBSCRIBE` the server refused.
    signals: Arc<RedisSignals>,
}

impl WatchRegistry {
    /// Builds a registry over `subscriber`, or over nothing under
    /// `watch_mode: disabled`.
    #[must_use]
    pub fn new(subscriber: Option<SubscriberClient>, signals: Arc<RedisSignals>) -> Arc<Self> {
        Arc::new(Self {
            keys: DashMap::new(),
            prefixes: DashMap::new(),
            next_id: AtomicU64::new(0),
            subscriptions: tokio::sync::Mutex::new(subscriber),
            terminal: tokio::sync::Mutex::new(()),
            closed: std::sync::OnceLock::new(),
            signals,
        })
    }

    /// Registers an exact-key watch, subscribing to the key's channel if this is
    /// its first watcher.
    ///
    /// # Errors
    /// Whatever [`map_redis_error`] makes of a failing `SUBSCRIBE`. The slot is
    /// withdrawn first, so a failed subscribe leaves no watcher behind that
    /// would receive nothing forever.
    pub async fn register_key(&self, key: &str, channel: &str) -> Result<CacheWatch, ClusterError> {
        self.register(WatcherKind::Exact, key, channel).await
    }

    /// Registers a prefix watch, pattern-subscribing if this is the prefix's
    /// first watcher.
    ///
    /// # Errors
    /// Whatever [`map_redis_error`] makes of a failing `PSUBSCRIBE`.
    pub async fn register_prefix(
        &self,
        prefix: &str,
        pattern: &str,
    ) -> Result<CacheWatch, ClusterError> {
        self.register(WatcherKind::Prefix, prefix, pattern).await
    }

    /// The shared body of the two registrations above.
    ///
    /// A `watch()` landing during or after [`close_all`](Self::close_all) must
    /// not produce a watcher that silently receives nothing forever, so this is
    /// a check-insert-recheck against the [`closed`](Self::closed) latch. The
    /// recheck is what makes it airtight: the latch can be set between the first
    /// check and the insert, and losing that race is resolved by *whoever
    /// actually holds the slot* — [`take_slot`](Self::take_slot) returns `true`
    /// only if this call removed it, which means the terminal broadcast did not
    /// collect it and this call owes the watcher its terminal event.
    async fn register(
        &self,
        kind: WatcherKind,
        target: &str,
        subscription: &str,
    ) -> Result<CacheWatch, ClusterError> {
        let (sender, watch) = CacheWatch::channel(WATCH_BUFFER);

        if !self.is_closed() {
            // Held across the insert and the subscribe, so a concurrent
            // prune-to-empty cannot decide to unsubscribe a channel this call
            // has just claimed. See the module docs.
            let held = self.subscriptions.lock().await;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let first = self.insert_slot(kind, target, id, sender.clone());
            if first && let Some(client) = held.as_ref() {
                let issued = match kind {
                    WatcherKind::Exact => client.subscribe(subscription.to_owned()).await,
                    // `Both` never registers: it is a routing target, not a
                    // watcher kind a consumer can ask for.
                    WatcherKind::Prefix | WatcherKind::Both => {
                        client.psubscribe(subscription.to_owned()).await
                    }
                };
                // Awaiting the subscribe is *not* enough — see
                // [`confirm_subscriptions`]. Without the round trip that follows
                // it, `watch()` can return before the server has processed the
                // `SUBSCRIBE`, and a `put` issued immediately afterwards
                // publishes to a channel nobody is listening on yet.
                let confirmed = match issued {
                    Ok(()) => confirm_subscriptions(client).await,
                    Err(err) => Err(map_redis_error(err)),
                };
                if let Err(err) = confirmed {
                    // Withdraw before returning: a registered slot with no
                    // server-side subscription is a watch that never fires and
                    // never says why.
                    let _withdrawn = self.take_slot(kind, target, id);
                    return Err(err);
                }
            }
            drop(held);
            if !self.is_closed() || !self.take_slot(kind, target, id) {
                return Ok(watch);
            }
        }

        // The registry is closed and this watcher's slot is ours to answer for.
        // `try_send` on a channel with an empty buffer always has room.
        let _delivered = sender.try_send(CacheWatchEvent::Closed(self.terminal_error()));
        Ok(watch)
    }

    /// Inserts a slot, reporting whether it is the first for `target` — the
    /// signal to subscribe.
    fn insert_slot(
        &self,
        kind: WatcherKind,
        target: &str,
        id: u64,
        sender: CacheWatchSender,
    ) -> bool {
        let map = self.map_for(kind);
        let mut slots = map.entry(target.to_owned()).or_default();
        let first = slots.is_empty();
        slots.push(WatcherSlot {
            id,
            sender,
            dropped: AtomicU64::new(0),
        });
        first
    }

    fn map_for(&self, kind: WatcherKind) -> &DashMap<String, Vec<WatcherSlot>> {
        match kind {
            WatcherKind::Exact => &self.keys,
            WatcherKind::Prefix | WatcherKind::Both => &self.prefixes,
        }
    }

    /// Whether a terminal broadcast has already run.
    fn is_closed(&self) -> bool {
        self.closed.get().is_some()
    }

    /// The error the terminal broadcast closed with, for replaying to a
    /// registration that arrived too late to be collected by it.
    fn terminal_error(&self) -> ClusterError {
        self.closed.get().cloned().unwrap_or(ClusterError::Shutdown)
    }

    /// Removes the slot with `id` under `target`, reporting whether it was still
    /// there. See [`register`](Self::register) for why the answer matters.
    ///
    /// The emptied entry goes through [`DashMap::remove_if`] rather than
    /// `remove`, for the reason spelled out on [`deliver_to`](Self::deliver_to):
    /// releasing the guard and re-acquiring the shard lock opens a window a
    /// concurrent `register` can insert into, and an unconditional `remove`
    /// would then discard that live watcher.
    fn take_slot(&self, kind: WatcherKind, target: &str, id: u64) -> bool {
        let map = self.map_for(kind);
        let removed = {
            let Some(mut slots) = map.get_mut(target) else {
                return false;
            };
            let before = slots.len();
            slots.retain(|slot| slot.id != id);
            slots.len() != before
        };
        map.remove_if(target, |_, slots| slots.is_empty());
        removed
    }

    /// Fans one parsed notification out to the watchers `kind` selects,
    /// returning the subscriptions that lost their last watcher and should be
    /// torn down.
    ///
    /// Synchronous, and every send on it is a non-blocking `try_send`, which is
    /// what makes it safe to run inline in the subscriber's read loop: a loop
    /// that stopped reading would let `fred`'s broadcast buffer overflow, and
    /// every watcher would then be told `Reset` because one was slow.
    ///
    /// A prefix watcher receives an event for any key it covers, matched here
    /// rather than by the server, because a message arrives stamped with the
    /// channel it was published on and not with the pattern that matched it —
    /// so with several overlapping patterns registered there is nothing in the
    /// message to route on. The scan is over distinct watched prefixes, which is
    /// the count `RD-WATCH-005` keeps at one per prefix rather than one per
    /// watcher.
    #[must_use]
    pub fn dispatch(&self, notification: &ParsedNotification, kind: WatcherKind) -> Vec<Orphaned> {
        let (key, event) = match notification {
            ParsedNotification::Changed { key } => (
                key,
                CacheWatchEvent::Event(CacheEvent::Changed { key: key.clone() }),
            ),
            ParsedNotification::Deleted { key } => (
                key,
                CacheWatchEvent::Event(CacheEvent::Deleted { key: key.clone() }),
            ),
            ParsedNotification::Expired { key } => (
                key,
                CacheWatchEvent::Event(CacheEvent::Expired { key: key.clone() }),
            ),
            // Per-key, not registry-wide: one message was unintelligible, which
            // is not evidence the subscription gapped.
            ParsedNotification::Reset { key } => (key, CacheWatchEvent::Reset),
            ParsedNotification::Ignored => return Vec::new(),
        };

        let mut orphaned = Vec::new();
        if matches!(kind, WatcherKind::Exact | WatcherKind::Both)
            && Self::deliver_to(&self.keys, key, &event, &self.signals)
        {
            orphaned.push(Orphaned {
                kind: WatcherKind::Exact,
                target: key.clone(),
            });
        }
        if matches!(kind, WatcherKind::Prefix | WatcherKind::Both) {
            // Collected before delivering, not iterated in place: `deliver_to`
            // takes a `get_mut` on the same map, and holding a `DashMap`
            // iterator across that deadlocks on the shard.
            let covering: Vec<String> = self
                .prefixes
                .iter()
                .filter(|entry| key.starts_with(entry.key().as_str()))
                .map(|entry| entry.key().clone())
                .collect();
            for prefix in covering {
                if Self::deliver_to(&self.prefixes, &prefix, &event, &self.signals) {
                    orphaned.push(Orphaned {
                        kind: WatcherKind::Prefix,
                        target: prefix,
                    });
                }
            }
        }
        orphaned
    }

    /// Delivers to every slot under `target`, pruning the dead ones. Returns
    /// whether the entry was left empty and its subscription is now orphaned.
    ///
    /// The removal is [`DashMap::remove_if`] rather than a `drop` followed by
    /// `remove`, and the difference is not stylistic. Those are two separate
    /// acquisitions of the shard lock, so a `register` racing in between would
    /// push a live slot into the vector this path has already decided is empty,
    /// and the `remove` would then throw that watcher away — its consumer would
    /// see `recv() -> None` with no terminal event, which this registry
    /// promises never happens. The `subscriptions` mutex does not close the gap,
    /// because delivery never takes it. `remove_if` re-tests emptiness under the
    /// same lock, so the racing watcher survives and — the entry still being
    /// present — is correctly reported as *not* orphaned.
    fn deliver_to(
        map: &DashMap<String, Vec<WatcherSlot>>,
        target: &str,
        event: &CacheWatchEvent,
        signals: &RedisSignals,
    ) -> bool {
        {
            let Some(mut slots) = map.get_mut(target) else {
                return false;
            };
            slots.retain(|slot| deliver(slot, event.clone(), signals));
        }
        map.remove_if(target, |_, slots| slots.is_empty()).is_some()
    }

    /// Tears down a subscription whose last watcher went away, unless one
    /// arrived again in the meantime.
    ///
    /// The re-check under the lock is the whole point — see the module docs on
    /// why an unsubscribe decided on the delivery path cannot be issued
    /// unconditionally.
    pub async fn release_subscription(&self, orphaned: &Orphaned, subscription: &str) {
        let held = self.subscriptions.lock().await;
        if self.map_for(orphaned.kind).contains_key(&orphaned.target) {
            // A new watcher claimed it while this was in flight. Its own
            // `register` already ensured the subscription, so leaving it alone
            // is both correct and the only safe move.
            return;
        }
        let Some(client) = held.as_ref() else {
            return;
        };
        let released = match orphaned.kind {
            WatcherKind::Exact => client.unsubscribe(subscription.to_owned()).await,
            WatcherKind::Prefix | WatcherKind::Both => {
                client.punsubscribe(subscription.to_owned()).await
            }
        };
        if let Err(err) = released {
            // Not fatal: the cost of a stale subscription is messages this
            // registry drops on arrival, not incorrect delivery. So the DEBUG
            // stays, for explaining an unexpectedly busy subscriber. It is still
            // a command the server refused, though, and every backend failure
            // reaches `cluster_provider_errors_total` and the
            // `cluster.provider.error` ERROR through the one shared emitter
            // (DESIGN.md §9) - and unlike a cache or lock operation, no
            // catalogued op wraps this one, so nothing else would count it.
            let err = map_redis_error(err);
            tracing::debug!(
                subscription,
                error = %err,
                "failed to release a redis subscription with no watchers left"
            );
            self.signals
                .provider_error("unsubscribe", ResourceId::Key(&orphaned.target), &err);
        }
    }

    /// Broadcasts a non-terminal `Reset` to every watcher, so consumers re-read
    /// (DESIGN.md §4.3).
    ///
    /// The registrations survive it. `Reset` means *re-read*, not *this watch is
    /// over*, so every watcher keeps receiving on the same [`CacheWatch`] and a
    /// consumer that responds by calling `watch()` again merely gets a second,
    /// redundant one.
    ///
    /// Returns whether the broadcast ran; `false` means the registry was already
    /// closed and the `Reset` was deliberately dropped, since nothing may follow
    /// a terminal `Closed`.
    pub async fn broadcast_reset(&self) -> bool {
        self.broadcast_and_clear(None).await
    }

    /// Closes every active watch terminally with [`ClusterError::Shutdown`]
    /// (DESIGN.md §11 step 2) and latches the registry closed so nothing can
    /// follow it.
    pub async fn close_all(&self) {
        let _broadcast = self.broadcast_and_clear(Some(ClusterError::Shutdown)).await;
    }

    /// Closes every active watch terminally with `err` — the path an exhausted
    /// reconnect budget takes, where the error must survive to the consumer so
    /// its own retry policy can read it as retryable.
    pub async fn close_all_with(&self, err: ClusterError) {
        let _broadcast = self.broadcast_and_clear(Some(err)).await;
    }

    /// Sends the terminal or reset event to every active watcher, clearing the
    /// registrations only when the event is a terminal one.
    ///
    /// Unlike the per-key fan-out — which drops on a full buffer and coalesces a
    /// later `Lagged` — this delivers the event with a blocking `send`, so a
    /// watcher whose buffer is momentarily full still receives the typed event
    /// rather than a bare channel close it cannot distinguish from a dropped
    /// sender. Each delivery is bounded by [`TERMINAL_GRACE`] and they run
    /// concurrently, so a consumer that is alive but has stopped draining cannot
    /// stall shutdown.
    async fn broadcast_and_clear(&self, terminal: Option<ClusterError>) -> bool {
        // Held across collection *and* delivery. See [`terminal`](Self::terminal)
        // for the two interleavings this excludes.
        let _serialized = self.terminal.lock().await;
        if self.is_closed() {
            return false;
        }
        if let Some(err) = terminal.clone() {
            let _latched = self.closed.set(err);
        }

        // A terminal close ends every watch, so the registrations go with it. A
        // `Reset` is non-terminal (DESIGN.md §4.3) and the consumer keeps
        // polling the same `CacheWatch`, so its sender has to survive the
        // broadcast — dropping it would end the stream on a signal that means
        // *re-read*. The registrations survive with it: `fred`'s
        // `manage_subscriptions()` replay task restores the server-side
        // subscriptions on reconnect, so there is nothing here to re-establish.
        let senders = match &terminal {
            Some(_) => self.drain_senders(),
            None => self.clone_senders(),
        };
        let mut deliveries = tokio::task::JoinSet::new();
        for sender in senders {
            let event = match &terminal {
                Some(err) => CacheWatchEvent::Closed(err.clone()),
                None => CacheWatchEvent::Reset,
            };
            deliveries.spawn(async move {
                let _delivered = tokio::time::timeout(TERMINAL_GRACE, sender.send(event)).await;
            });
        }
        while deliveries.join_next().await.is_some() {}
        true
    }

    /// Removes every registered watcher and returns their senders.
    ///
    /// Key by key via [`DashMap::remove`], **not** `iter()` then `clear()`.
    /// Those are separate operations with no atomicity between them, and a
    /// watcher registering in the gap would be collected by neither the
    /// iteration nor — having been inserted after it — spared by the clear: it
    /// would simply be removed with no event ever delivered, and its consumer
    /// would see an end-of-stream `None` instead of `Reset`. `remove` is atomic
    /// per key against `entry().or_default().push()`, which holds the same shard
    /// lock, so every registration falls cleanly on one side.
    fn drain_senders(&self) -> Vec<CacheWatchSender> {
        let mut senders = Vec::new();
        for map in [&self.keys, &self.prefixes] {
            let targets: Vec<String> = map.iter().map(|entry| entry.key().clone()).collect();
            for target in targets {
                if let Some((_, slots)) = map.remove(&target) {
                    senders.extend(slots.into_iter().map(|slot| slot.sender));
                }
            }
        }
        senders
    }

    /// Clones every registered watcher's sender, leaving the registrations in
    /// place.
    ///
    /// The counterpart to [`drain_senders`](Self::drain_senders) for a
    /// non-terminal broadcast. [`CacheWatchSender`] is `Clone` and every clone
    /// feeds the same consumer channel, so the delivery below reaches the
    /// watcher while the slot it was cloned from keeps the stream open.
    fn clone_senders(&self) -> Vec<CacheWatchSender> {
        let mut senders = Vec::new();
        for map in [&self.keys, &self.prefixes] {
            for entry in map {
                senders.extend(entry.value().iter().map(|slot| slot.sender.clone()));
            }
        }
        senders
    }

    /// The number of distinct prefixes with at least one live watcher — the
    /// Redis pattern count this registry is responsible for, which
    /// `RD-WATCH-005` asserts stays at one per prefix however many consumers
    /// watch it.
    #[must_use]
    pub fn prefix_pattern_count(&self) -> usize {
        self.prefixes.len()
    }

    /// The number of distinct keys with at least one live watcher.
    #[must_use]
    pub fn key_subscription_count(&self) -> usize {
        self.keys.len()
    }
}

/// A subscription whose last watcher went away, handed back by
/// [`WatchRegistry::dispatch`] so the caller — which is async and can await a
/// round trip — tears it down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphaned {
    /// Which family the subscription belongs to.
    pub kind: WatcherKind,
    /// The consumer-facing key or prefix, from which the caller rebuilds the
    /// channel or pattern.
    pub target: String,
}

/// The channel and pattern names this plugin subscribes to, all derived from the
/// operator's `key_prefix` (DESIGN.md §2.1).
///
/// One place, because the publisher and the subscriber have to agree exactly:
/// the scripts build `<prefix>:e:c:<key>` from `KEYS[1]` server-side, and a
/// mismatch here would be a watch that silently never fires.
#[derive(Debug, Clone)]
pub struct ChannelNames {
    /// `<key_prefix>:e:c:`.
    event_prefix: String,
    /// `<key_prefix>:c:`.
    entry_prefix: String,
    /// The logical database, which scopes the keyspace-notification channel.
    database: u8,
}

impl ChannelNames {
    /// Builds the naming scheme for one cache.
    #[must_use]
    pub fn new(key_prefix: &str, database: u8) -> Self {
        Self {
            event_prefix: format!("{key_prefix}:e:c:"),
            entry_prefix: format!("{key_prefix}:c:"),
            database,
        }
    }

    /// The channel one key's events are published on.
    #[must_use]
    pub fn channel_for_key(&self, key: &str) -> String {
        format!("{}{key}", self.event_prefix)
    }

    /// The pattern covering every key under `prefix`.
    ///
    /// The consumer's prefix is glob-escaped for the same reason `scan_prefix`
    /// escapes it: the key space is opaque to this plugin, so a prefix
    /// containing `[` or `*` would otherwise subscribe to something other than
    /// what was asked for — and here the consequence is worse than a wrong scan
    /// result, because the watcher would receive events for keys it does not
    /// watch and miss the ones it does.
    ///
    /// The `<key_prefix>:e:c:` stem is escaped too, matching what
    /// `LockNames::release_pattern` and `KeyspaceNames::new` already do with the
    /// same operator prefix: nothing validates `key_prefix` against a glob-free
    /// charset, and unescaped a `[` in it silently redirects every prefix watch
    /// this cache registers.
    #[must_use]
    pub fn pattern_for_prefix(&self, prefix: &str) -> String {
        format!(
            "{}{}*",
            super::scan::escape_glob(&self.event_prefix),
            super::scan::escape_glob(prefix)
        )
    }

    /// Recovers the consumer's key from a published event channel, or `None`
    /// when the channel is not one of this cache's.
    #[must_use]
    pub fn key_from_event_channel(&self, channel: &str) -> Option<String> {
        channel.strip_prefix(&self.event_prefix).map(str::to_owned)
    }

    /// Recovers the consumer's key from a *Redis key* — what a keyspace
    /// notification reports, `fred` having already split the channel apart for
    /// us — or `None` when the key is not one of this cache's entries.
    ///
    /// `None` is a routing answer rather than an error: the keyspace pattern is
    /// plugin-wide (`KeyspaceNames`), so a **lock lease** legitimately arrives
    /// here and must be declined so the lock's own parser can claim it. Both
    /// share the operator's prefix and differ by one segment.
    #[must_use]
    pub fn key_from_entry_key(&self, entry_key: &str) -> Option<String> {
        entry_key
            .strip_prefix(&self.entry_prefix)
            .map(str::to_owned)
    }

    /// The logical database this cache lives in, so a keyspace notification
    /// from another database on the same server is not mistaken for one of
    /// this cache's own keys.
    #[must_use]
    pub fn database(&self) -> u8 {
        self.database
    }
}

// Layer-1 unit tests (TESTING.md §2, `cache/watch.rs` row). Out-of-line per
// DE1101.
#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
