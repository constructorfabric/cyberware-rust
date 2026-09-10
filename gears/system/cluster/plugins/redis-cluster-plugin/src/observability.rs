//! The plugin's observability surface (DESIGN.md §9, ADR-004): the one metrics
//! sink both plugins share, the four plugin-local instruments the contract sink
//! cannot carry, and the log-event names §9 catalogues.
//!
//! ## Two sinks, because the contract has a shape the plugin exceeds
//!
//! [`RedisSignals`] holds both halves and hands them out through one value, so
//! no call site has to know which half a given signal belongs to:
//!
//! - the **ADR-004 catalog** goes through `cluster_sdk`'s [`ClusterMetrics`]
//!   port — in production the SDK's own `OtelClusterMetrics`, so instrument
//!   names, units, and the label allowlist are defined once for every provider
//!   rather than re-derived here;
//! - the **four plugin-local metrics** of DESIGN.md §9 go through an
//!   OpenTelemetry [`Meter`] this plugin owns directly. They have to:
//!   `cluster_redis_connection_state` is a gauge and [`ClusterMetrics`] has no
//!   gauge method, and the other three cover Redis-specific subjects that no
//!   catalog instrument names.
//!
//! ## The cardinality rule is structural here
//!
//! Keys, lock names, and holder tokens reach [`ClusterMetrics`] nowhere — the
//! port takes no such parameter — and the plugin-local instruments below attach
//! only `provider`. They appear as span attributes and log fields instead:
//! `cluster.lock.acquired` carries a holder token because it is a DEBUG *log
//! line* (DESIGN.md §5.5), and [`ResourceId`] is what puts a key or a lock name
//! on `cluster.provider.error` without it ever becoming a label.
//!
//! ## Log events carry their name twice, on purpose
//!
//! Every event in [`logs`] is emitted with `name:` set — the structural form,
//! which is what a filtering collector matches on — *and* with the same name
//! opening the human message. The default `tracing` `fmt` layer prints the
//! message and not the event name, so an operator tailing logs would otherwise
//! see prose with no way to tell which catalogued event it is. The two halves
//! cost one duplicated string constant per site and make the same event
//! findable both ways.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cluster_sdk::observability::otel::OtelClusterMetrics;
use cluster_sdk::observability::{
    ClusterMetrics, ResourceId, emit_provider_error, primitive, result,
};
use cluster_sdk::{ClusterCacheBackend, ClusterError, InstrumentedCache};
use fred::clients::Pool;
use fred::interfaces::ClientLike;
use opentelemetry::metrics::{Counter, Gauge, Meter};
use opentelemetry::{InstrumentationScope, KeyValue, global};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The log-event names DESIGN.md §9 catalogues for this plugin, plus the two
/// catalog events it also emits.
///
/// Constants rather than literals at each site because several are emitted from
/// more than one place — `expiry_events_unavailable` from three preflight
/// branches, `cluster.watch.reset` from the fan-out's lagged path and the
/// reconnect observer — and a catalogued name that drifted between two of its
/// own emission sites is exactly the failure ADR-004's naming contract exists to
/// prevent.
pub mod logs {
    /// Re-exported so a caller emitting the catalog's watch-reset event does not
    /// have to reach into two modules for two names in the same table.
    pub use cluster_sdk::observability::logs::{PROVIDER_ERROR, WATCH_RESET};

    /// `maxmemory-policy` is not `noeviction` (WARN, once at startup).
    pub const MAXMEMORY_POLICY_UNSAFE: &str = "cluster.provider.maxmemory_policy_unsafe";
    /// `CONFIG GET maxmemory-policy` was refused, so the risk above cannot be
    /// assessed at all (WARN, once at startup).
    ///
    /// Plugin-local rather than an ADR-004 catalog event (DESIGN.md §9).
    pub const MAXMEMORY_POLICY_UNKNOWN: &str = "cluster.provider.maxmemory_policy_unknown";
    /// An `evicted` notification arrived for one of this plugin's keys (WARN,
    /// rate-limited). See [`EvictionReporter`](super::EvictionReporter).
    pub const EVICTION_OBSERVED: &str = "cluster.provider.eviction_observed";
    /// The declared consistency is `EventuallyConsistent` (WARN, once at
    /// startup).
    pub const WEAK_CONSISTENCY: &str = "cluster.provider.weak_consistency";
    /// A `Linearizable` declaration rests on an operator hint the server would
    /// not let the plugin verify (WARN, once at startup).
    pub const CONSISTENCY_ASSERTED: &str = "cluster.provider.consistency_asserted";
    /// `Expired` watch events will not be delivered (WARN, once at startup).
    pub const EXPIRY_EVENTS_UNAVAILABLE: &str = "cluster.provider.expiry_events_unavailable";
    /// `manage_keyspace_notifications: true` changed the server's global flags
    /// (INFO, once).
    pub const KEYSPACE_NOTIFICATIONS_SET: &str = "cluster.provider.keyspace_notifications_set";
    /// The server supports `SPUBLISH`/`SSUBSCRIBE`, which v1 records but does
    /// not use (DEBUG, once at startup).
    pub const SHARDED_PUBSUB_AVAILABLE: &str = "cluster.provider.sharded_pubsub_available";
    /// `INFO replication` was refused, so the topology could not be detected
    /// (WARN, once at startup).
    ///
    /// Plugin-local rather than an ADR-004 catalog event (DESIGN.md §9).
    pub const TOPOLOGY_UNKNOWN: &str = "cluster.provider.topology_unknown";
    /// `appendfsync` reported a value this plugin does not recognize (WARN, once
    /// at startup).
    ///
    /// Plugin-local rather than an ADR-004 catalog event (DESIGN.md §9).
    pub const DURABILITY_UNKNOWN: &str = "cluster.provider.durability_unknown";
    /// The command pool did not close inside its bound (WARN, at shutdown).
    ///
    /// Plugin-local rather than an ADR-004 catalog event (DESIGN.md §9).
    pub const POOL_CLOSE_TIMEOUT: &str = "cluster.provider.pool_close_timeout";
    /// Tracked background tasks did not drain inside their bound (WARN, at
    /// shutdown).
    ///
    /// Plugin-local rather than an ADR-004 catalog event (DESIGN.md §9).
    pub const TASK_DRAIN_TIMEOUT: &str = "cluster.provider.task_drain_timeout";
    /// The subscriber's reconnect policy is exhausted, so every cache watch is
    /// closed terminally and blocked `lock()` callers fall back to the heartbeat
    /// (WARN, once, DESIGN.md §10).
    ///
    /// In the `cluster.provider.*` family rather than `cluster.watch.*` because
    /// the condition is a connection outcome that also costs the lock its
    /// release wake: `spawn_connection_watchdog` runs with `registry: None`
    /// under `watch_mode: disabled` and in the standalone lock plugin, where
    /// there are no watches at all.
    ///
    /// Plugin-local rather than an ADR-004 catalog event (DESIGN.md §9).
    pub const SUBSCRIBER_LOST: &str = "cluster.provider.subscriber_lost";
    /// A lock was acquired: its name and the holder token now under its key
    /// (DEBUG, DESIGN.md §5.5).
    pub const LOCK_ACQUIRED: &str = "cluster.lock.acquired";
}

/// The instrumentation scope the four plugin-local metrics are registered under.
///
/// Distinct from the `cf-gears-cluster` scope `OtelClusterMetrics` uses for the
/// ADR-004 contract signals: these are this plugin's own additions, and a
/// dashboard that wants to know which build emitted them should see this
/// plugin's name rather than the SDK's.
pub const PLUGIN_SCOPE: &str = "cf-redis-cluster-plugin";

/// How often the connection-state observer samples the command pool.
///
/// A gauge that only moved at startup and shutdown would answer "was Redis
/// reachable when this process booted", which is not the question DESIGN.md §9
/// asks it. Ten seconds is under every ordinary Prometheus scrape interval, so
/// a transient outage is visible in at least one sample, while costing one timer
/// wakeup per plugin.
const CONNECTION_STATE_INTERVAL: Duration = Duration::from_secs(10);

/// The minimum gap between two `cluster.provider.eviction_observed` WARNs.
///
/// Evictions arrive in bursts by their nature — `maxmemory` pressure does not
/// remove one key — so an unthrottled WARN would turn one incident into
/// thousands of lines and bury the rest of the log. The suppressed count rides
/// the next line that is emitted, so the burst is still *reported*, just not
/// line by line.
const EVICTION_WARN_WINDOW: Duration = Duration::from_secs(30);

/// Strips the ADR-004 `_total` suffix from a counter name.
///
/// The same rule `cluster_sdk::observability::otel` applies to the catalog
/// counters and for the same reason: the `opentelemetry-prometheus` exporter
/// appends `_total` when it renders a counter, so an instrument created with the
/// suffix already on it scrapes as `..._total_total`. The constant below keeps
/// the *contract* name — what a dashboard queries — as the single source of
/// truth for the name.
fn counter_name(contract: &'static str) -> &'static str {
    contract.strip_suffix("_total").unwrap_or(contract)
}

/// `cluster_redis_watch_events_dropped_total{provider}` — events dropped to a
/// full watcher buffer, i.e. the `Lagged` count (DESIGN.md §9).
const WATCH_EVENTS_DROPPED: &str = "cluster_redis_watch_events_dropped_total";
/// `cluster_redis_subscriber_resubscribes_total{provider}` — subscriber
/// reconnect-and-replay cycles (DESIGN.md §9).
/// `pub` rather than private only so `subscriber_tests` can assert on it by the
/// same name the emitter uses; the module itself is private and this is not
/// re-exported, so it stays crate-internal.
pub const SUBSCRIBER_RESUBSCRIBES: &str = "cluster_redis_subscriber_resubscribes_total";
/// `cluster_redis_script_reloads_total{provider}` — `NOSCRIPT` recoveries
/// (DESIGN.md §9, §6).
const SCRIPT_RELOADS: &str = "cluster_redis_script_reloads_total";
/// `cluster_redis_connection_state{provider}` — 1 when every connection in the
/// command pool believes it is connected, else 0 (DESIGN.md §9).
const CONNECTION_STATE: &str = "cluster_redis_connection_state";
/// `cluster_redis_evictions_observed_total{provider, primitive}` — evictions of
/// this plugin's own keys.
///
/// `primitive` is `cache` or `lock`, and it is the label that makes the counter
/// actionable rather than merely present: an evicted cache entry costs a re-read,
/// while an evicted lock lease means two holders believe they hold one lock
/// (DESIGN.md §3.7). An alert that cannot tell them apart has to treat every
/// eviction as the worse one or the milder one, and both are wrong.
///
/// **Not in DESIGN.md §9.** §3.7 and TESTING.md `RD-SPEC-007` both specify
/// `cluster_provider_errors_total{op="eviction"}`, which cannot be emitted as
/// written: that counter carries `{provider, kind}` and no `op`
/// (`cluster-sdk/src/observability/otel.rs`), and an eviction is not a
/// [`ClusterError`] at all, so it cannot travel through [`emit_provider_error`]
/// either. Folding it onto the catalog counter as `kind = "other"` would make an
/// eviction indistinguishable from every other unclassified backend failure and
/// would inflate a provider-error rate with something that is not an operation
/// failure. A counter that says what it counts is the honest form
/// (DESIGN.md §3.7).
const EVICTIONS_OBSERVED: &str = "cluster_redis_evictions_observed_total";

/// The process-global meter under [`PLUGIN_SCOPE`], used when no meter is
/// injected (production).
///
/// Tests and the `RD-SPEC-*` smoke runs inject their own meter over an in-memory
/// reader instead, through [`RedisSignals::over_meter`] and the builders'
/// `__with_meter` seam.
#[must_use]
pub fn plugin_meter() -> Meter {
    global::meter_with_scope(InstrumentationScope::builder(PLUGIN_SCOPE).build())
}

/// Which primitive owned a key the plugin has something to say about
/// (DESIGN.md §3.7, §9).
///
/// A type rather than a bare `&'static str` because it selects behaviour as
/// well as labelling it — [`RedisSignals::eviction_observed`] picks a rate
/// limiter from it — and a typo in a string would silently give one primitive
/// the other's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primitive {
    /// A cache entry. An eviction costs a re-read.
    Cache,
    /// A lock lease. An eviction hands the lock to a second holder.
    Lock,
}

impl Primitive {
    /// The bounded `primitive` label value this reports as.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Lock => "lock",
        }
    }
}

/// Rate limiter for `cluster.provider.eviction_observed`.
///
/// Deliberately allocation-free and lock-free: it is consulted on the subscriber
/// fan-out's read loop, which must never block or stop draining — a stalled loop
/// overflows `fred`'s broadcast buffer and resets every watcher, so an eviction
/// storm would cost every consumer a re-read on top of the eviction itself.
/// [`claim`](Self::claim) is a pure function of the elapsed-millisecond reading
/// it is handed, which is what makes the whole policy unit-testable with no
/// clock.
pub struct EvictionReporter {
    /// The zero point [`elapsed_millis`](Self::elapsed_millis) measures from.
    started: Instant,
    /// Milliseconds (since `started`) at which the last WARN was emitted, or
    /// [`Self::NEVER`].
    last_report: AtomicU64,
    /// Evictions observed since the last WARN, drained onto the next one.
    suppressed: AtomicU64,
    window_millis: u64,
}

impl EvictionReporter {
    /// The `last_report` value meaning "nothing has been reported yet", so the
    /// very first eviction always emits.
    const NEVER: u64 = u64::MAX;

    /// Builds a reporter that emits at most one WARN per `window`.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            started: Instant::now(),
            last_report: AtomicU64::new(Self::NEVER),
            suppressed: AtomicU64::new(0),
            window_millis: u64::try_from(window.as_millis()).unwrap_or(u64::MAX),
        }
    }

    /// Milliseconds since this reporter was built — one monotonic clock read, no
    /// allocation.
    #[must_use]
    pub fn elapsed_millis(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Records one observed eviction and reports whether it should be logged,
    /// carrying the number of evictions suppressed since the last line.
    ///
    /// `Some(0)` is an ordinary answer — a lone eviction with nothing suppressed
    /// behind it — and distinct from `None`, which means the window is not open.
    /// The compare-exchange is what keeps two fan-out tasks (the two plugins can
    /// both be running) from emitting for the same window: the loser suppresses
    /// rather than retrying, because a WARN a few microseconds later carries no
    /// information the winner's did not.
    #[must_use]
    pub fn claim(&self, now_millis: u64) -> Option<u64> {
        let last = self.last_report.load(Ordering::Relaxed);
        let open = last == Self::NEVER || now_millis.saturating_sub(last) >= self.window_millis;
        if !open
            || self
                .last_report
                .compare_exchange(last, now_millis, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(self.suppressed.swap(0, Ordering::Relaxed))
    }
}

/// Everything this plugin emits through, built once per plugin and shared by the
/// cache decorator, the lock, the watcher registry, and the subscriber fan-out.
///
/// One per plugin rather than one per component, because the `provider` label is
/// fixed at construction (the [`ClusterMetrics`] port has no per-call provider
/// argument) and because two sinks would mean two `cluster_cache_ops_total`
/// instruments disagreeing about the same deployment.
pub struct RedisSignals {
    /// The bounded `provider` label, on every signal this plugin emits.
    provider: &'static str,
    /// The ADR-004 catalog sink.
    metrics: Arc<dyn ClusterMetrics>,
    watch_events_dropped: Counter<u64>,
    subscriber_resubscribes: Counter<u64>,
    script_reloads: Counter<u64>,
    evictions_observed: Counter<u64>,
    connection_state: Gauge<u64>,
    /// The rate limiter for the cache's eviction WARN. Its counter above is
    /// *not* rate limited: a suppressed line is still a counted eviction.
    cache_evictions: EvictionReporter,
    /// The lock's, kept **separate** rather than sharing the cache's window.
    ///
    /// A shared limiter would let an eviction storm in the cache — thousands of
    /// entries, each costing a re-read — spend the budget that the one line
    /// saying a *lock lease* was evicted needs. That line reports two holders
    /// believing they hold one lock (DESIGN.md §3.7), and it is precisely under
    /// memory pressure heavy enough to storm the cache that it gets emitted, so
    /// one window for both would suppress it exactly when it matters.
    lock_evictions: EvictionReporter,
}

impl RedisSignals {
    /// Builds the sink over an explicit [`ClusterMetrics`] and an explicit
    /// meter.
    ///
    /// The seam the Layer-1 tests use: a recording `ClusterMetrics` double turns
    /// "which signal fires for which outcome" into a question answerable with no
    /// server at all.
    #[must_use]
    pub fn new(metrics: Arc<dyn ClusterMetrics>, meter: &Meter, provider: &'static str) -> Self {
        Self {
            provider,
            metrics,
            watch_events_dropped: meter
                .u64_counter(counter_name(WATCH_EVENTS_DROPPED))
                .with_description("Cache watch events dropped to a full watcher buffer")
                .build(),
            subscriber_resubscribes: meter
                .u64_counter(counter_name(SUBSCRIBER_RESUBSCRIBES))
                .with_description("Redis subscriber reconnect-and-replay cycles")
                .build(),
            script_reloads: meter
                .u64_counter(counter_name(SCRIPT_RELOADS))
                .with_description("Lua script cache misses recovered with EVAL")
                .build(),
            evictions_observed: meter
                .u64_counter(counter_name(EVICTIONS_OBSERVED))
                .with_description("Evictions observed on keys owned by this plugin")
                .build(),
            connection_state: meter
                .u64_gauge(CONNECTION_STATE)
                .with_description("Whether the Redis command pool believes it is connected")
                .build(),
            cache_evictions: EvictionReporter::new(EVICTION_WARN_WINDOW),
            lock_evictions: EvictionReporter::new(EVICTION_WARN_WINDOW),
        }
    }

    /// Production: the SDK's OpenTelemetry sink over the process-global meter
    /// for the catalog, and this plugin's own scope for its four additions.
    #[must_use]
    pub fn from_global_meters(provider: &'static str) -> Self {
        Self::new(
            Arc::new(OtelClusterMetrics::from_global_meter(provider)),
            &plugin_meter(),
            provider,
        )
    }

    /// Routes **both** halves through `meter`, so one in-memory reader observes
    /// the catalog signals and the plugin-local ones together.
    ///
    /// The two scopes collapse into one here, which is the deliberate cost of
    /// reading everything back through a single reader; nothing in either name
    /// set collides, so no signal is lost to the merge.
    #[must_use]
    pub fn over_meter(meter: &Meter, provider: &'static str) -> Self {
        Self::new(
            Arc::new(OtelClusterMetrics::new(meter, provider)),
            meter,
            provider,
        )
    }

    /// The ADR-004 sink, for the SDK decorators that take one directly.
    #[must_use]
    pub fn metrics(&self) -> Arc<dyn ClusterMetrics> {
        Arc::clone(&self.metrics)
    }

    /// The bounded `provider` label.
    #[must_use]
    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// Wraps `cache` in the SDK's [`InstrumentedCache`] decorator — the
    /// supported path for the ADR-004 cache signal set (DESIGN.md §9).
    ///
    /// Every `cluster.cache.*` span, `cluster_cache_ops_total` increment, and
    /// `cluster.provider.error` on a cache operation comes from there rather
    /// than from this plugin, which is what keeps the naming contract identical
    /// across providers instead of re-implemented per backend.
    #[must_use]
    pub fn instrument_cache(
        &self,
        cache: Arc<dyn ClusterCacheBackend>,
    ) -> Arc<dyn ClusterCacheBackend> {
        Arc::new(InstrumentedCache::new(
            cache,
            self.provider,
            Arc::clone(&self.metrics),
        ))
    }

    /// Records the metric side of a finished lock op — duration, the
    /// bounded-`result` counter, and the shared provider-error signals.
    ///
    /// Mirrors `cluster::defaults::lock::record_lock` deliberately: the SDK's
    /// CAS-based default lock and this native one must be indistinguishable on a
    /// dashboard, or an operator moving a profile from the default lock to the
    /// Redis one would see their panels go blank.
    pub fn record_lock<T>(
        &self,
        op: &'static str,
        lock: &str,
        started: Instant,
        outcome: &Result<T, ClusterError>,
    ) {
        self.metrics
            .lock_op_duration(op, started.elapsed().as_secs_f64());
        self.metrics.lock_op(op, result::label(outcome));
        if let Err(err) = outcome {
            self.provider_error(op, ResourceId::Lock(lock), err);
        }
    }

    /// Emits the shared provider-error signals for a failure that is not
    /// wrapped by a catalogued operation.
    ///
    /// A no-op unless `err` is a genuine [`ClusterError::Provider`]: a CAS
    /// conflict, lock contention, a lock timeout, an expired lease, and a
    /// shutdown are normal outcomes rather than backend errors, and the SDK's
    /// [`emit_provider_error`] is where that rule lives so no plugin has to
    /// re-derive it.
    pub fn provider_error(&self, op: &str, resource: ResourceId<'_>, err: &ClusterError) {
        emit_provider_error(&*self.metrics, self.provider, op, resource, err);
    }

    /// `cluster_watch_resets_total{provider,primitive="cache"}` — the catalog
    /// counter behind the `cluster.watch.reset` event.
    pub fn watch_reset(&self) {
        self.metrics.watch_reset(primitive::CACHE);
    }

    /// Counts events a full watcher buffer forced the fan-out to drop.
    ///
    /// One per dropped event rather than one per `Lagged`, so the counter agrees
    /// with the `dropped` count the consumer eventually receives
    /// (`RD-WATCH-008`).
    pub fn watch_event_dropped(&self) {
        self.watch_events_dropped.add(1, &self.labels());
    }

    /// Counts one subscriber reconnect-and-replay cycle.
    pub fn subscriber_resubscribed(&self) {
        self.subscriber_resubscribes.add(1, &self.labels());
    }

    /// Counts one `NOSCRIPT` recovery (DESIGN.md §6).
    pub fn script_reloaded(&self) {
        self.script_reloads.add(1, &self.labels());
    }

    /// Records the command pool's current connectedness.
    pub fn connection_state(&self, connected: bool) {
        self.connection_state
            .record(u64::from(connected), &self.labels());
    }

    /// Counts an observed eviction and emits the rate-limited WARN
    /// (DESIGN.md §3.7).
    ///
    /// `primitive` is whichever owned the evicted key, as the subscriber's
    /// `OwnedKey::primitive` reports it. It labels the counter, appears on the
    /// line, and selects the rate
    /// limiter — the two are throttled independently, so a cache storm cannot
    /// spend the budget the lock's line needs.
    ///
    /// The counter is unconditional and only the log line is throttled: an
    /// alert has to see every eviction, while an operator reading the log needs
    /// one line naming a key plus how many others it stands for.
    pub fn eviction_observed(&self, primitive: Primitive, key: &str) {
        self.evictions_observed.add(
            1,
            &[
                KeyValue::new(
                    cluster_sdk::observability::fields::label::PROVIDER,
                    self.provider,
                ),
                KeyValue::new(
                    cluster_sdk::observability::fields::label::PRIMITIVE,
                    primitive.label(),
                ),
            ],
        );
        let reporter = match primitive {
            Primitive::Cache => &self.cache_evictions,
            Primitive::Lock => &self.lock_evictions,
        };
        let Some(suppressed) = reporter.claim(reporter.elapsed_millis()) else {
            return;
        };
        tracing::warn!(
            name: logs::EVICTION_OBSERVED,
            provider = self.provider,
            primitive = primitive.label(),
            key = %key,
            suppressed,
            "cluster.provider.eviction_observed: redis evicted a key owned by this plugin. No TTL \
             lapsed and no consumer asked for it: under memory pressure an evicted lock key hands \
             the lock to a second holder, and an evicted leader key elects a second leader. Run \
             cluster keys on a dedicated instance, or set maxmemory-policy noeviction \
             (DESIGN.md sec 3.7)"
        );
    }

    /// The one label every plugin-local instrument carries. Rebuilt per call
    /// rather than cached because [`KeyValue`] is not `Copy` and the array is a
    /// stack value either way.
    fn labels(&self) -> [KeyValue; 1] {
        [KeyValue::new(
            cluster_sdk::observability::fields::label::PROVIDER,
            self.provider,
        )]
    }
}

/// Spawns the task that keeps `cluster_redis_connection_state` current
/// (DESIGN.md §9).
///
/// A sampling task rather than an OpenTelemetry *observable* gauge, which is the
/// obvious fit and the wrong one here: an observable gauge's callback is
/// registered on the meter provider for the life of the process and is not
/// unregistered when the instrument handle drops, so it would outlive `stop()`
/// holding a `Pool` clone — and a second plugin instance would register a second
/// callback reporting the same `{provider}` series, which is a conflict rather
/// than a sum. A task is cancellable, which is exactly the property that
/// mismatch needs.
///
/// The pool is read through every one of its clients rather than through
/// `ClientLike` on the pool itself: `Pool::inner()` rotates, so the trait
/// methods answer for whichever client came next rather than for the pool.
#[must_use]
pub fn spawn_connection_state_observer(
    pool: Pool,
    signals: Arc<RedisSignals>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            signals.connection_state(pool.clients().iter().all(ClientLike::is_connected));
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(CONNECTION_STATE_INTERVAL) => {}
            }
        }
        // The pool is about to close, so the last sample a scrape can see should
        // say so rather than leave the series stuck at its final live reading.
        signals.connection_state(false);
    })
}

// Layer-1 unit tests (the eviction rate limiter's pure policy, and the
// contract-name/`_total` rule). The signal-per-outcome tests live beside the
// primitives that emit them. Out-of-line per DE1101.
#[cfg(test)]
#[path = "observability_tests.rs"]
mod tests;
