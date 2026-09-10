//! The subscriber client's fan-out and reconnect-observer tasks
//! (DESIGN.md §3.2 steps 4–5, §4.3).
//!
//! ## Why the subscriber is its own client
//!
//! A Redis connection in subscribe mode accepts only subscribe-family commands,
//! so the cache's reads and writes cannot share it. `fred`'s `SubscriberClient`
//! additionally tracks its own subscription set and replays it after a
//! reconnect, which is what the `Reset` handling below is built on: without the
//! replay, a reconnect would leave every watcher silently subscribed to nothing.
//!
//! ## One subscriber, two primitives
//!
//! The connection this drives is a *plugin*-level one (DESIGN.md §3.3), which is
//! why the module sits beside `connect.rs` and `shutdown.rs` rather than under
//! either primitive: the subscriber carries plugin-published cache events, Redis
//! `expired`/`evicted` keyspace notifications, **and** the lock-release events a
//! blocked `lock()` wakes on (DESIGN.md §5.3), so it draws its names from
//! `cache::watch` and `lock` alike and belongs to neither. The fan-out therefore
//! takes [`FanOutRoutes`], with the cache half optional: the standalone lock
//! plugin (DESIGN.md §3.5) runs this same task with only [`LockRoute`]
//! populated, which is what keeps its wake path identical to the combined
//! plugin's (`RD-LOCK-009`) instead of a second near-copy of this loop. A module
//! owned by `cache/` could not serve a deployment that has no cache.
//!
//! ## Two tasks, because they wait on different things
//!
//! The fan-out task drains the message stream and must never block; the
//! reconnect observer parks on a separate notification stream and, when it
//! fires, runs a registry-wide broadcast that *does* block. Folding them into
//! one `select!` would mean a `Reset` broadcast — bounded by `TERMINAL_GRACE`
//! per non-draining watcher — stalls message delivery for everyone, which is
//! exactly the "one slow consumer must not stall delivery" rule the registry
//! exists to keep.

use std::sync::Arc;

use cluster_sdk::{ClusterError, ProviderErrorKind};
use fred::clients::SubscriberClient;
use fred::interfaces::{ClientLike, EventInterface};
use fred::types::config::Server;
use fred::types::{ConnectHandle, KeyspaceEvent, Message, MessageKind};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cache::watch::{
    ChannelNames, Orphaned, ParsedNotification, WatchRegistry, WatcherKind, is_eviction,
    parse_keyspace_event, parse_publish_payload,
};
use crate::lock::LockNames;
use crate::lock::waiters::ReleaseWaiters;
use crate::observability::{Primitive, RedisSignals, logs};

/// The cache half of the fan-out's routing table.
pub struct CacheRoute {
    /// The watcher registry every cache notification is dispatched into.
    pub registry: Arc<WatchRegistry>,
    /// The cache's channel and pattern names.
    pub names: ChannelNames,
}

/// The lock half of the fan-out's routing table.
pub struct LockRoute {
    /// The release-waiter registry a release wakes.
    pub waiters: Arc<ReleaseWaiters>,
    /// The lock's key and channel names.
    pub names: LockNames,
}

/// Which primitive a notified Redis key belongs to, with the operator's prefix
/// already stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedKey {
    /// A cache entry, from `<prefix>:c:<key>`.
    Cache(String),
    /// A lock lease, from `<prefix>:l:<name>`.
    Lock(String),
}

impl OwnedKey {
    /// Which primitive owns this key, for the `primitive` label and the rate
    /// limiter it selects (DESIGN.md §9).
    #[must_use]
    pub fn primitive(&self) -> Primitive {
        match self {
            Self::Cache(_) => Primitive::Cache,
            Self::Lock(_) => Primitive::Lock,
        }
    }

    /// The name itself, whichever primitive owns it.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Cache(key) | Self::Lock(key) => key,
        }
    }
}

/// The keyspace-notification pattern, and the classifier that says which
/// primitive a notified key belongs to (DESIGN.md §3.7).
///
/// ## Why the pattern spans the whole prefix
///
/// The eviction signal exists because an evicted **lock lease** hands the lock
/// to a second holder while the first still believes it holds it — that is the
/// case DESIGN.md §3.7 opens with and the worst one it names. A pattern scoped
/// to `<prefix>:c:` would observe every case except that one. So there is a
/// single `<prefix>:*` pattern and this type sorts what arrives, rather than one
/// pattern per primitive: the notification stream is one connection's, the
/// classification is a prefix compare, and two patterns would double the
/// server-side matching for nothing.
///
/// ## Why it delegates rather than re-deriving
///
/// Nothing here spells `:c:` or `:l:`. Classification asks [`ChannelNames`] and
/// then [`LockNames`], each of which owns its own segment and is the same type
/// the publisher builds names with. A copy of those rules here would be a second
/// place for them to drift, and a drift would show up as evictions silently
/// attributed to the wrong primitive — or to neither.
#[derive(Debug, Clone)]
pub struct KeyspaceNames {
    /// `__keyspace@<db>__:<key_prefix>:*`.
    pattern: String,
    /// The logical database the pattern is scoped to.
    database: u8,
    /// `None` in the standalone lock plugin, which owns no cache entries.
    ///
    /// Present under `watch_mode: disabled` even though no watcher registry is:
    /// the mode turns off the cache's *watch*, and an evicted entry is still an
    /// incident worth counting.
    cache: Option<ChannelNames>,
    /// The lock's namer, present in both plugins — both own a lock backend.
    locks: LockNames,
}

impl KeyspaceNames {
    /// Builds the keyspace naming for one plugin.
    #[must_use]
    pub fn new(
        key_prefix: &str,
        database: u8,
        cache: Option<ChannelNames>,
        locks: LockNames,
    ) -> Self {
        Self {
            // Scoped to the operator's prefix, so a shared Redis does not
            // deliver unrelated tenants' keyspace traffic to this subscriber,
            // and glob-escaped for the reason `pattern_for_prefix` escapes:
            // a `[` in the prefix would otherwise be a character class.
            pattern: format!(
                "__keyspace@{database}__:{}:*",
                crate::cache::scan::escape_glob(key_prefix)
            ),
            database,
            cache,
            locks,
        }
    }

    /// The one blanket pattern carrying `expired` and `evicted` for every key
    /// this plugin owns.
    ///
    /// Always-on rather than registered per watcher, because §3.7's signal has
    /// to observe evictions of keys nobody is watching — including every lock
    /// lease, which nothing ever watches.
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The logical database this plugin's keys live in.
    #[must_use]
    pub fn database(&self) -> u8 {
        self.database
    }

    /// Sorts a notified Redis key into the primitive that owns it, or `None`
    /// when it belongs to neither.
    ///
    /// `None` is reachable in ordinary operation rather than only on a mistyped
    /// prefix: the pattern covers `<prefix>:*`, and this plugin's own event
    /// *channels* (`<prefix>:e:c:`, `<prefix>:e:l:`) share that prefix. No key
    /// exists at a channel name, so nothing real is dropped — but a future key
    /// family under this prefix would land here silently, which is why the
    /// caller logs it.
    #[must_use]
    pub fn classify(&self, entry_key: &str) -> Option<OwnedKey> {
        if let Some(key) = self
            .cache
            .as_ref()
            .and_then(|names| names.key_from_entry_key(entry_key))
        {
            return Some(OwnedKey::Cache(key));
        }
        self.locks
            .name_from_lease_key(entry_key)
            .map(OwnedKey::Lock)
    }
}

/// Where the fan-out delivers each family of message.
///
/// The lock route is not optional: both plugins own a lock backend, and the
/// combined one subscribes the release pattern even under `watch_mode:
/// disabled`, where the cache route is `None`. That mode disables the *cache's*
/// watch, not the connection — see [`spawn_fan_out`].
pub struct FanOutRoutes {
    /// `None` under `watch_mode: disabled`, where no watcher registry exists.
    pub cache: Option<CacheRoute>,
    /// The lock's release-wake route.
    pub locks: LockRoute,
    /// The keyspace route, or `None` when no keyspace pattern was subscribed.
    ///
    /// Independent of [`cache`](Self::cache): under `watch_mode: disabled` the
    /// keyspace route is live and the cache route is not, because an eviction is
    /// worth counting whether or not anyone is watching the key it removed.
    pub keyspace: Option<KeyspaceNames>,
    /// The plugin's signal sink: the eviction WARN and counter of DESIGN.md
    /// §3.7, and the watch-reset counter behind a lagged stream.
    pub signals: Arc<RedisSignals>,
}

/// Spawns the task that routes every subscriber message to the registry that
/// asked for it.
///
/// Cancellation-aware so `stop()` can join it; the watch registry's own
/// `close_all` is what tells the watchers, and it runs independently of this
/// task so a watcher observes `Closed(Shutdown)` whether or not the task has
/// noticed the cancel yet.
#[must_use]
pub fn spawn_fan_out(
    subscriber: &SubscriberClient,
    routes: FanOutRoutes,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    // Two receivers, because `fred` splits the streams: anything on a
    // `__keyspace@`/`__keyevent@` channel is diverted to the keyspace broadcast
    // and never appears on the pub/sub one. Reading only the latter subscribes
    // to the keyspace pattern, watches the server deliver on it, and still
    // emits no `Expired` — see the `watch.rs` module docs.
    let mut messages = subscriber.message_rx();
    let mut keyspace = subscriber.keyspace_event_rx();
    tokio::spawn(async move {
        loop {
            let dispatched = tokio::select! {
                () = shutdown.cancelled() => return,
                received = messages.recv() => match received {
                    Ok(message) => published(&routes, &message),
                    Err(err) => match on_lag(&routes, err, "pub/sub").await {
                        Continue::Yes => continue,
                        Continue::No => return,
                    },
                },
                received = keyspace.recv() => match received {
                    Ok(event) => keyspace_notification(&routes, &event),
                    Err(err) => match on_lag(&routes, err, "keyspace").await {
                        Continue::Yes => continue,
                        Continue::No => return,
                    },
                },
            };
            // Only the cache produces something to dispatch: a lock release is
            // delivered to its waiters inside `published`, because the waiter
            // registry is woken synchronously and has no orphaned subscriptions
            // to tear down afterwards.
            let (Some((notification, kind)), Some(cache)) = (dispatched, routes.cache.as_ref())
            else {
                continue;
            };
            let orphaned = cache.registry.dispatch(&notification, kind);
            release_all(&cache.registry, &cache.names, orphaned).await;
        }
    })
}

/// Whether the fan-out loop should keep going after a stream error.
enum Continue {
    Yes,
    No,
}

/// Turns a broadcast-stream error into the loop's next move.
///
/// A lagged buffer means messages were lost before this task could read them,
/// with no way to know which — so a registry-wide `Reset` is the honest answer,
/// the same signal a reconnect gap produces. A closed stream means the client
/// is gone and the loop is done.
///
/// Lock releases lost in the same gap need no equivalent, and get none: their
/// waiters re-attempt the `SET NX` on the jittered heartbeat regardless, so the
/// cost is latency rather than a stale view somebody has to be told about.
async fn on_lag(routes: &FanOutRoutes, err: RecvError, stream: &str) -> Continue {
    match err {
        RecvError::Lagged(missed) => {
            let Some(cache) = routes.cache.as_ref() else {
                // The standalone lock plugin: nothing to reset, and nothing that
                // would make `cluster_watch_resets_total` true if incremented.
                tracing::debug!(
                    missed,
                    stream,
                    "the redis subscriber fell behind on a lock-only client; blocked lock() \
                     callers fall back to the jittered heartbeat"
                );
                return Continue::Yes;
            };
            // Named, not merely written into the message: this is the catalog's
            // `cluster.watch.reset` and it is emitted from two places (here and
            // the reconnect observer), so both have to answer to the same name
            // and the same counter (ADR-004).
            tracing::warn!(
                name: logs::WATCH_RESET,
                provider = routes.signals.provider(),
                primitive = "cache",
                missed,
                stream,
                "cluster.watch.reset: the redis subscriber fell behind and dropped messages; \
                 every watcher is being reset (DESIGN.md sec 4.3)"
            );
            routes.signals.watch_reset();
            let _broadcast = cache.registry.broadcast_reset().await;
            Continue::Yes
        }
        RecvError::Closed => Continue::No,
    }
}

/// Interprets a message from the pub/sub stream — an in-script `PUBLISH`, from
/// either a cache mutation or a lock release.
///
/// The lock family is answered here and reports `None`, because waking a waiter
/// is a synchronous registry hit with nothing for the caller to dispatch
/// afterwards. The two families are told apart by their own channel prefixes
/// (`:e:c:` against `:e:l:`), each parsed by the type that builds it, so neither
/// can claim the other's message.
fn published(
    routes: &FanOutRoutes,
    message: &Message,
) -> Option<(ParsedNotification, WatcherKind)> {
    if let Some(name) = routes
        .locks
        .names
        .name_from_release_channel(&message.channel)
    {
        // Woken on *any* payload rather than only on the `R` of DESIGN.md §2.5.
        // A wake is a hint: the acquisition loop re-attempts the `SET NX` as the
        // source of truth, so a spurious one costs a single round trip while a
        // wake withheld on an unrecognized payload would cost a real waiter its
        // whole delay.
        routes.locks.waiters.notify(&name);
        return None;
    }
    let Some(cache) = routes.cache.as_ref() else {
        // The standalone lock plugin subscribes only the release pattern, so
        // anything else here is a message it never asked for.
        tracing::debug!(
            channel = %message.channel,
            "redis subscriber saw a message outside this plugin's lock channels"
        );
        return None;
    };
    let names = &cache.names;
    let Some(key) = names.key_from_event_channel(&message.channel) else {
        // Not one of this cache's channels. The subscriber is this plugin's own
        // client, so the only way here is a pattern it registered matching
        // something it did not expect — worth a DEBUG, not an event.
        tracing::debug!(
            channel = %message.channel,
            "redis subscriber saw a message outside this cache's channels"
        );
        return None;
    };
    // The routing rule the `watch.rs` module docs explain: an exact
    // subscription and a covering pattern both deliver the same `PUBLISH`, so
    // each message serves only its own family, or every doubly-covered watcher
    // sees the write twice.
    let kind = match message.kind {
        MessageKind::PMessage => WatcherKind::Prefix,
        MessageKind::Message | MessageKind::SMessage => WatcherKind::Exact,
    };
    let notification = parse_publish_payload(&key, &message.value);
    report_unintelligible_payload(&routes.signals, &notification, &key);
    Some((notification, kind))
}

/// Emits `cluster.watch.reset` for a payload this plugin cannot interpret —
/// DESIGN.md §9's second source for that event, alongside the reconnect
/// observer.
///
/// It means either an unrelated publisher on this prefix or a future payload
/// format, and the watchers on that key are being told to re-read, which is a
/// watch reset in every sense the catalog means even though only one key's
/// watchers are affected.
fn report_unintelligible_payload(
    signals: &RedisSignals,
    notification: &ParsedNotification,
    key: &str,
) {
    if !matches!(notification, ParsedNotification::Reset { .. }) {
        return;
    }
    tracing::warn!(
        name: logs::WATCH_RESET,
        provider = signals.provider(),
        primitive = "cache",
        key = %key,
        "cluster.watch.reset: an unintelligible payload arrived on this cache's own event \
         channel; the key's watchers are being told to re-read (DESIGN.md sec 2.5, sec 4.3)"
    );
    signals.watch_reset();
}

/// Interprets an event from the keyspace stream — Redis's own `expired` or
/// `evicted` notification, already split into `(db, key, operation)` by `fred`.
///
/// **Both primitives arrive here**, because the pattern spans `<prefix>:*`
/// (see [`KeyspaceNames`]), and they are answered differently:
///
/// - an **eviction** is reported for either, labelled with the primitive that
///   owned the key. This is the whole reason the pattern is not scoped to the
///   cache: an evicted lease is the worst case DESIGN.md §3.7 names;
/// - only a **cache entry** goes on to the watchers. A lock lease has none — the
///   acquisition loop treats the `SET NX` as the source of truth (DESIGN.md
///   §5), so there is nothing a delivery would tell anyone.
///
/// A cache `expired`/`evicted` is served to **both** watcher families: one
/// blanket pattern means no twin delivery to split, unlike the published family,
/// and routing it to one family would silently drop the other's `Expired`.
fn keyspace_notification(
    routes: &FanOutRoutes,
    event: &KeyspaceEvent,
) -> Option<(ParsedNotification, WatcherKind)> {
    let names = routes.keyspace.as_ref()?;
    // The pattern is already scoped to this database, but the check is cheap
    // and a notification attributed to the wrong database would be reported as
    // a deletion of a key this plugin never had.
    if event.db != names.database() {
        return None;
    }
    let entry_key = event.key.as_str()?;
    let Some(owned) = names.classify(entry_key.as_ref()) else {
        // Under this plugin's prefix but owned by neither primitive. Today the
        // only keys matching that are none — the event *channels* share the
        // prefix but no key exists at a channel name — so this is a DEBUG
        // against a future key family rather than an expected path.
        tracing::debug!(
            key = %entry_key,
            operation = %event.operation,
            "redis keyspace notification under this plugin's prefix belongs to neither the cache \
             nor the lock"
        );
        return None;
    };
    if is_eviction(&event.operation) {
        // The one place the plugin's top operational risk stops being a
        // prediction (DESIGN.md §3.7). Reported for *every* key under this
        // plugin's prefix, watched or not and whichever primitive owns it,
        // which is why the keyspace pattern is always on rather than registered
        // per watcher.
        //
        // Rate-limited, allocation-free, and synchronous, because this runs on
        // the fan-out's read loop: a loop that stopped draining would overflow
        // `fred`'s broadcast buffer and reset every watcher, so an eviction
        // storm would cost every consumer a re-read on top of the eviction.
        routes
            .signals
            .eviction_observed(owned.primitive(), owned.name());
    }
    // An `expired` lease is the ordinary end of a lock's life rather than an
    // incident, and nothing waits on the event, so the lock's half of the
    // stream ends here.
    let OwnedKey::Cache(key) = &owned else {
        return None;
    };
    match parse_keyspace_event(key, &event.operation) {
        ParsedNotification::Ignored => None,
        notification => Some((notification, WatcherKind::Both)),
    }
}

/// Tears down the subscriptions that lost their last watcher during a dispatch.
///
/// Awaited on the fan-out path rather than spawned, so a teardown and a fresh
/// `watch()` for the same target cannot be reordered against each other. It is
/// rare — only a dropped watch reaches it — and the alternative costs the
/// serialization the registry's subscription mutex exists to provide.
async fn release_all(registry: &WatchRegistry, names: &ChannelNames, orphaned: Vec<Orphaned>) {
    for entry in orphaned {
        let subscription = match entry.kind {
            WatcherKind::Exact => names.channel_for_key(&entry.target),
            WatcherKind::Prefix | WatcherKind::Both => names.pattern_for_prefix(&entry.target),
        };
        registry.release_subscription(&entry, &subscription).await;
    }
}

/// Spawns the task that turns a subscriber reconnect into a registry-wide
/// `Reset` (DESIGN.md §4.3).
///
/// **Every notification is acted on, including the first, and this task skips
/// none.** The tempting reasoning for a skip is that `fred` fires a reconnect
/// event on the *initial* connect, which it does — but the receiver is created
/// here, *after* `connect()` has already awaited `init()`, and a `tokio`
/// broadcast receiver only ever sees sends that follow its own subscription. The
/// initial notification is therefore already gone by the time this subscribes, so
/// a skip consumes the first **genuine** reconnect instead: the first
/// subscription gap of a process's life delivers no `Reset` at all, leaving every
/// watcher believing a stale view is current until a second reconnect comes
/// along. Confirmed against a container by killing the subscriber's connection
/// twice — with a skip in place, only the second kill produces a `Reset`.
///
/// Acting on every notification is safe even if that ordering ever changes. The
/// only cost of acting on an initial-connect one would be a single registry-wide
/// `Reset` broadcast at startup — and `build_and_start` has not returned yet at
/// that point, so no consumer has had the chance to register a watch for it to
/// reach.
///
/// The `Reset` is emitted as soon as the reconnect is observed, which may be
/// marginally before `fred` finishes replaying the subscription set. That
/// ordering is deliberate rather than sloppy: `Reset` means "re-read", the
/// re-read goes through the command pool rather than this client, and every
/// event in the gap is lost whether the consumer is told early or late. Telling
/// it late would leave a window in which a consumer believes stale data is
/// current.
#[must_use]
pub fn spawn_reconnect_observer(
    subscriber: &SubscriberClient,
    registry: Arc<WatchRegistry>,
    signals: Arc<RedisSignals>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let mut reconnects = subscriber.reconnect_rx();
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                () = shutdown.cancelled() => return,
                received = reconnects.recv() => received,
            };
            if observe_reconnect(event, &registry, &signals)
                .await
                .is_break()
            {
                return;
            }
        }
    })
}

/// Handles one reconnect notification, returning whether the observer carries on.
///
/// Split out of [`spawn_reconnect_observer`]'s loop so both non-terminal arms are
/// reachable from a unit test: driving a real `RecvError::Lagged` means
/// overflowing `fred`'s internal notification channel, which needs a connection
/// that flaps faster than the observer reads. The arms are what is worth testing
/// — that each moves the same two signals *and* says so — and they need no
/// client at all.
async fn observe_reconnect(
    event: Result<Server, RecvError>,
    registry: &WatchRegistry,
    signals: &RedisSignals,
) -> std::ops::ControlFlow<()> {
    match event {
        Ok(server) => {
            tracing::warn!(
                name: logs::WATCH_RESET,
                provider = signals.provider(),
                server = %server,
                primitive = "cache",
                "cluster.watch.reset: the redis subscriber reconnected; resetting every \
                 watcher, since pub/sub is fire-and-forget and every gap is a total gap \
                 (DESIGN.md sec 4.3)"
            );
            // Two counters, because they answer different questions.
            // `cluster_watch_resets_total` says every watcher was reset;
            // `cluster_redis_subscriber_resubscribes_total` says the
            // subscriber flapped. Together they separate "one flap" from
            // "a flap per minute" (DESIGN.md §9).
            signals.subscriber_resubscribed();
            signals.watch_reset();
            let _broadcast = registry.broadcast_reset().await;
        }
        // A missed reconnect notification is still a reconnect: reset
        // rather than ignore.
        //
        // Logged under the same catalogued name as the `Ok` arm above,
        // and for the same reason it is logged at all: this arm moves
        // `cluster_watch_resets_total`, so a silent one leaves the
        // counter and the log stream disagreeing, with no line to
        // explain a reset an operator can see in the metric. Not a
        // fourth `Reset` source — it is DESIGN.md §4.3's first source,
        // "the subscriber reconnected", reached by a lagged receiver.
        //
        // `missed` is the one thing this path can say that the `Ok` path
        // cannot: how many reconnects went unobserved. A `u64` count
        // rather than a key or a token, so it is safe as a field — and
        // it stays off every metric label.
        Err(RecvError::Lagged(missed)) => {
            tracing::warn!(
                name: logs::WATCH_RESET,
                provider = signals.provider(),
                missed,
                primitive = "cache",
                "cluster.watch.reset: reconnect notifications were missed, which is still \
                 a reconnect; resetting every watcher (DESIGN.md sec 4.3)"
            );
            signals.subscriber_resubscribed();
            signals.watch_reset();
            let _broadcast = registry.broadcast_reset().await;
        }
        // The sender is gone, so no further notification can arrive and the
        // observer has nothing left to do.
        Err(RecvError::Closed) => return std::ops::ControlFlow::Break(()),
    }
    std::ops::ControlFlow::Continue(())
}

/// Spawns the task that reacts to `fred` giving up on reconnecting the
/// subscriber (DESIGN.md §10).
///
/// The signal is the `ConnectHandle` `init()` returned: it stays pending for as
/// long as the client is connected *or* retrying, and resolves only when the
/// reconnect policy is exhausted. Watching that rather than the error stream is
/// what distinguishes "a connection dropped and is coming back" — which is a
/// `Reset`, handled above — from "this subscriber is never coming back".
///
/// The two primitives lose different things and are told differently. A
/// **watcher** would otherwise wait forever, receiving nothing and told nothing,
/// so every watch is closed terminally with a *retryable* error (see
/// [`subscriber_lost`]). A blocked **`lock()`** loses only its release wake and
/// keeps acquiring on the jittered heartbeat, so there is nothing to close and
/// the WARN is the whole of the response — which is also why `registry` is
/// optional: the standalone lock plugin and `watch_mode: disabled` run this same
/// task with nothing to close.
#[must_use]
pub fn spawn_connection_watchdog(
    connection: ConnectHandle,
    registry: Option<Arc<WatchRegistry>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let outcome = tokio::select! {
            // An ordinary `stop()`: `close_all` has already run (or is about to)
            // with `Shutdown`, which is the right error for that path.
            () = shutdown.cancelled() => return,
            resolved = connection => resolved,
        };
        tracing::warn!(
            name: logs::SUBSCRIBER_LOST,
            outcome = ?outcome,
            "cluster.provider.subscriber_lost: the redis subscriber's reconnect policy is \
             exhausted; every cache watch is being \
             closed so consumers see a retryable terminal error rather than silence, and blocked \
             lock() callers fall back to the jittered heartbeat, which costs latency rather than \
             correctness (DESIGN.md sec 10, sec 5.3)"
        );
        if let Some(registry) = registry {
            registry.close_all_with(subscriber_lost()).await;
        }
    })
}

/// The terminal error a watcher is closed with when the subscriber's reconnect
/// policy gives up (DESIGN.md §10).
///
/// `ConnectionLost` rather than `Shutdown` because only the former is
/// [`ClusterError::is_retryable`], and the SDK's `RestartingWatch` combinator
/// branches on exactly that: told `Shutdown`, a consumer's retry policy would
/// never run against what is in fact a recoverable outage.
#[must_use]
pub fn subscriber_lost() -> ClusterError {
    ClusterError::Provider {
        kind: ProviderErrorKind::ConnectionLost,
        message: "the redis subscriber's reconnect policy was exhausted; cache watches are closed \
                  and must be re-established"
            .to_owned(),
    }
}

/// Blocks until every subscribe-family command already issued on `client` has
/// been **processed by the server**, by awaiting one `PING` behind them.
///
/// ## Why awaiting the `SUBSCRIBE` itself is not enough
///
/// `fred`'s `subscribe`/`psubscribe` futures resolve when the command has been
/// handed to the connection, not when Redis has answered it. The confirmation
/// frame never completes the command: `fred`'s reader classifies
/// `["subscribe", <channel>, <count>]` as a subscription response and **drops
/// it** (`router/responses.rs::is_subscription_response`, "Dropping unused
/// subscription response") before the frame can be matched to anything.
///
/// So the ordering DESIGN.md §3.2 step 4 asks for — the subscription live before
/// `build_and_start` returns, and before `watch()` returns — is not obtainable
/// from `fred`'s subscribe API alone. Without this barrier a `watch(k)` followed
/// immediately by a `put(k)` from the same process can miss its own event: the
/// `PUBLISH` travels the command pool's connection and reaches the server first,
/// while the `SUBSCRIBE` is still in flight on the subscriber's. Verified
/// against a container with `MONITOR`, which showed `SUBSCRIBE` executing
/// *after* the `PUBLISH` it was supposed to precede. It is a race rather than a
/// certainty, and an easy one to miss: it widens sharply with a second subscriber
/// command outstanding, which the lock-release pattern supplies, so a plugin
/// carrying only the startup subscription can pass its own checks and still lose
/// the race in production.
///
/// `PING` is the barrier because it is one of the few commands a connection in
/// subscribe mode accepts, and — unlike the subscribe family — its reply is an
/// ordinary frame that completes an ordinary command. Redis processes one
/// connection's commands in order, so a `PING` that has answered proves
/// everything written before it on that connection has run.
///
/// The cost is one round trip per `watch()` and per startup, on paths that are
/// already doing a round trip. Delivery is untouched: nothing on the fan-out
/// path calls this.
///
/// # Errors
/// Whatever [`map_redis_error`] makes of a failing `PING` — which the caller
/// should treat exactly like a failing `SUBSCRIBE`, since an unconfirmed
/// subscription is one that may not exist.
pub async fn confirm_subscriptions(client: &SubscriberClient) -> Result<(), ClusterError> {
    client
        .ping::<()>(None)
        .await
        .map_err(crate::redis_error::map_redis_error)
}

/// Quits the subscriber client, logging rather than propagating a failure —
/// `stop()` has nothing useful to do with one.
pub async fn quit_subscriber(subscriber: &SubscriberClient) {
    if let Err(err) = subscriber.quit().await {
        tracing::debug!(error = %err, "redis subscriber quit reported an error during shutdown");
    }
}

// Layer-1 unit tests (TESTING.md §2, `subscriber.rs` row). Out-of-line per
// DE1101.
#[cfg(test)]
#[path = "subscriber_tests.rs"]
mod tests;
