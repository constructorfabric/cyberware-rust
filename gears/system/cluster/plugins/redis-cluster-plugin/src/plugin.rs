//! The combined [`RedisClusterPlugin`] (cache + lock) builder and lifecycle
//! handle (DESIGN.md §3.2), following the outbox-style builder/handle pattern
//! (ADR-006). Not a `RunnableCapability` — the cluster gear (`cf-gears-cluster`)
//! owns its lifecycle via `build_and_start`/`stop`.

use std::sync::Arc;

use cluster_sdk::{CacheConsistency, ClusterCacheBackend, ClusterError, DistributedLockBackend};
use fred::clients::{Pool, SubscriberClient};
use fred::interfaces::PubsubInterface;
use fred::types::ConnectHandle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cache::watch::WatchRegistry;
use crate::cache::{CacheInit, RedisCache};
use crate::config::{RedisClusterConfig, WatchMode};
use crate::connect::{ConnectSpec, Connected, connect};
use crate::lock::waiters::ReleaseWaiters;
use crate::lock::{LockInit, LockNames, RedisLock};
use crate::observability::{RedisSignals, spawn_connection_state_observer};
use crate::preflight::{PreflightRequest, REQUIRED_KEYSPACE_FLAGS, run_preflight};
use crate::provider::PROVIDER_NAME;
use crate::redis_error::map_redis_error;
use crate::scripts::{ALL_SCRIPTS, PoolScriptExecutor, load_catalog};
use crate::shutdown::{DropDiagnosis, abandon_subscriber, cancel_and_diagnose_drop, close_pool};
use crate::subscriber::{
    CacheRoute, FanOutRoutes, KeyspaceNames, LockRoute, confirm_subscriptions, quit_subscriber,
    spawn_connection_watchdog, spawn_fan_out, spawn_reconnect_observer,
};
use crate::wait::WaitPolicy;

/// Entry point for constructing the combined Redis cluster plugin.
///
/// ```no_run
/// # async fn doc(config: redis_cluster_plugin::RedisClusterConfig) -> Result<(), cluster_sdk::ClusterError> {
/// use redis_cluster_plugin::RedisClusterPlugin;
/// let handle = RedisClusterPlugin::builder(config).build_and_start().await?;
/// let _cache = handle.cache();
/// handle.stop().await;
/// # Ok(())
/// # }
/// ```
pub struct RedisClusterPlugin;

impl RedisClusterPlugin {
    // No `#[must_use]` here: `RedisClusterBuilder` already carries a
    // `#[must_use = "..."]` message, so a bare attribute on this function would
    // be a `clippy::double_must_use` no-op.
    /// Starts building the plugin from operator config.
    pub fn builder(config: RedisClusterConfig) -> RedisClusterBuilder {
        RedisClusterBuilder {
            config,
            meter: None,
        }
    }
}

/// Fluent builder for [`RedisClusterPlugin`].
#[must_use = "a builder starts nothing until `.build_and_start()` is called"]
pub struct RedisClusterBuilder {
    config: RedisClusterConfig,
    /// Optional override for the meter every signal is emitted through. `None`
    /// in production (the process-global provider). See
    /// [`__with_meter`](Self::__with_meter).
    meter: Option<opentelemetry::metrics::Meter>,
}

impl RedisClusterBuilder {
    /// Test-only: routes both the ADR-004 catalog signals and this plugin's four
    /// local metrics through `meter` instead of the process-global provider, so
    /// a test can attach an in-memory reader and read every signal back by name
    /// rather than by eye.
    ///
    /// Gated behind `--features integration` so the seam is compiled out of
    /// release builds entirely, mirroring
    /// `PostgresClusterBuilder::__with_reaper_meter`.
    #[cfg(feature = "integration")]
    #[doc(hidden)]
    pub fn __with_meter(mut self, meter: opentelemetry::metrics::Meter) -> Self {
        self.meter = Some(meter);
        self
    }

    /// Builds and starts the plugin, following DESIGN.md §3.2's six steps.
    ///
    /// Step 4's ordering is load-bearing rather than tidy: the initial
    /// `PSUBSCRIBE`s are **live on the server before this returns**, so a write
    /// in the startup window cannot publish to a channel nobody is listening on
    /// yet. Redis pub/sub does not queue or replay for a client that subscribes
    /// late, so that event would be lost with nothing to show for it.
    ///
    /// Two things are needed for that, not one. `tokio::spawn` only schedules a
    /// task, so subscribing inside the fan-out task would let `build_and_start`
    /// resolve — and a caller's first publish land — before the subscription
    /// existed. And *awaiting* the subscribe is not by itself the guarantee it
    /// reads as: `fred` resolves it when the command reaches the connection, not
    /// when the server has answered, so the round trip in
    /// `subscriber::confirm_subscriptions` is what actually closes the
    /// window (DESIGN.md §3.2 step 4).
    ///
    /// By the time this resolves, the pool is connected, the preflight has run,
    /// the scripts are loaded, and the subscriber is live — there is no
    /// readiness gate for callers to reason about, and a failure at any step
    /// tears down whatever the earlier steps started.
    ///
    /// # Errors
    /// - [`ClusterError::InvalidConfig`] for a zero-valued config bound, a URL
    ///   `fred` cannot parse, or an `INFO server` the server refuses.
    /// - [`ClusterError::Provider`] if the initial connect or the `SCRIPT LOAD`
    ///   fails.
    pub async fn build_and_start(self) -> Result<RedisClusterHandle, ClusterError> {
        let config = self.config;
        // Before anything is opened: a zero `command_timeout_ms` disables
        // `fred`'s command timeout outright rather than shortening it, and the
        // bounded `stop()` below depends on that timeout existing.
        config.validate()?;
        // Here rather than at its use below, for the same reason: this is the
        // one conversion of `wait_timeout_ms` into the signed count `WAIT`
        // takes, and doing it before the pool exists means an unrepresentable
        // one is a plain `?` rather than an error path that has to tear down a
        // connected pool and subscriber.
        let wait = WaitPolicy::from_config(config.wait_replicas, config.wait_timeout_ms)?;

        // One sink for this whole plugin (DESIGN.md §9): the `InstrumentedCache`
        // decorator, the native lock, the watcher registry, the subscriber
        // fan-out, and the connection-state gauge all share it. One rather than
        // one per component, because the `provider` label is fixed at
        // construction and two sinks would mean two `cluster_cache_ops_total`
        // instruments disagreeing about the same deployment.
        let signals = Arc::new(match self.meter {
            Some(meter) => RedisSignals::over_meter(&meter, PROVIDER_NAME),
            None => RedisSignals::from_global_meters(PROVIDER_NAME),
        });

        let Connected {
            pool,
            subscriber,
            clustered,
            url_topology,
        } = connect(ConnectSpec {
            url: &config.url,
            database: config.database,
            pool_size: config.pool_size,
            command_timeout: config.command_timeout(),
        })
        .await?;
        // The registry owns the subscriber for subscription purposes; the handle
        // owns it for lifecycle purposes. One clone each — `SubscriberClient` is
        // a handle to one connection, not the connection itself.
        //
        // `None` under `watch_mode: disabled`, which is now what that mode
        // means: no watcher registry and no cache subscriptions, rather than no
        // second connection (see `ConnectSpec`).
        let registry = (config.watch_mode == WatchMode::Publish)
            .then(|| WatchRegistry::new(Some(subscriber.0.clone()), Arc::clone(&signals)));

        // Everything after the connect has to tear the pool *and the subscriber*
        // down on the way out — both are connected, so returning the error alone
        // would leak the connections until the process exited. See
        // [`abandon_subscriber`] for why dropping the subscriber is not enough.
        let outcome = match run_preflight(
            &pool,
            PreflightRequest {
                topology_hint: config.topology,
                url_topology,
                durability_hint: config.durability,
                // The combined plugin owns a cache, so it needs `expired` as
                // well as `evicted` — the standalone lock asks for the narrower
                // set (DESIGN.md §3.5's third row).
                keyspace_flags: Some(REQUIRED_KEYSPACE_FLAGS),
                manage_keyspace_notifications: config.manage_keyspace_notifications,
            },
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                abandon_subscriber(&subscriber.0, &subscriber.1).await;
                close_pool(&pool).await;
                return Err(err);
            }
        };

        let executor = PoolScriptExecutor::new(pool.clone());
        let scripts = match load_catalog(&executor, ALL_SCRIPTS).await {
            Ok(scripts) => Arc::new(scripts),
            Err(err) => {
                abandon_subscriber(&subscriber.0, &subscriber.1).await;
                close_pool(&pool).await;
                return Err(err);
            }
        };

        // Created before the subscriber so every task it spawns observes the
        // same signal, and so the teardown paths below have something to cancel.
        let shutdown = CancellationToken::new();

        let cache = Arc::new(RedisCache::new(CacheInit {
            pool: pool.clone(),
            scripts: Arc::clone(&scripts),
            key_prefix: config.key_prefix.clone(),
            consistency: outcome.consistency,
            watch_mode: config.watch_mode,
            clustered,
            wait,
            database: config.database,
            watchers: registry,
            signals: Arc::clone(&signals),
        }));

        // Wrapped once, here, rather than per `cache()` call: the decorator is
        // what emits the whole ADR-004 cache signal set (DESIGN.md §9), and the
        // handle keeps the concrete `Arc<RedisCache>` beside it because `stop()`
        // and `start_subscriber` need `watch_registry()` and `channel_names()`,
        // which the trait object does not carry.
        let instrumented =
            signals.instrument_cache(Arc::clone(&cache) as Arc<dyn ClusterCacheBackend>);

        // The lock shares the pool, the catalog, and the shutdown token with the
        // cache — one plugin, one connection budget (DESIGN.md §3.3). It is the
        // *standalone* plugin that gets its own of each, and only because the
        // SDK keeps non-cache providers independent of the cache one.
        let waiters = ReleaseWaiters::new();
        let lock_names = LockNames::new(&config.key_prefix);
        let lock = Arc::new(RedisLock::new(LockInit {
            pool: pool.clone(),
            scripts,
            names: lock_names.clone(),
            // The same declaration the cache makes, from the same preflight: the
            // lock is exactly as safe as the server it runs on (DESIGN.md §5.1).
            linearizable: outcome.consistency == CacheConsistency::Linearizable,
            wait,
            waiters: Arc::clone(&waiters),
            shutdown: shutdown.clone(),
            signals: Arc::clone(&signals),
        }));

        // Built here rather than inside `start_subscriber` because it needs the
        // operator's prefix and database, which the cache and the lock each
        // carry only their own half of.
        let keyspace = KeyspaceNames::new(
            &config.key_prefix,
            config.database,
            Some(cache.channel_names()),
            lock_names.clone(),
        );

        let subscription = match start_subscriber(SubscriberSetup {
            cache: &cache,
            lock_names,
            keyspace,
            waiters,
            subscriber,
            signals: Arc::clone(&signals),
            shutdown: &shutdown,
        })
        .await
        {
            Ok(subscription) => subscription,
            Err(err) => {
                close_pool(&pool).await;
                return Err(err);
            }
        };

        let connection_state =
            spawn_connection_state_observer(pool.clone(), signals, shutdown.clone());

        Ok(RedisClusterHandle {
            cache,
            instrumented,
            lock,
            pool,
            subscription: Some(subscription),
            connection_state: Some(connection_state),
            shutdown,
            stopped: false,
        })
    }
}

/// The subscriber client and the two tasks that ride it, owned by the handle so
/// `stop()` can end them in order.
struct Subscription {
    client: SubscriberClient,
    fan_out: JoinHandle<()>,
    /// `None` under `watch_mode: disabled` — there are no watchers to reset.
    reconnects: Option<JoinHandle<()>>,
    /// `fred`'s own subscription-replay task, which is what makes a reconnect
    /// recoverable at all (DESIGN.md §4.3).
    manager: JoinHandle<()>,
    /// Reports an exhausted reconnect policy, closing every watch if there are
    /// any.
    watchdog: JoinHandle<()>,
}

/// What [`start_subscriber`] needs. A struct because six parameters, three of
/// them references, is where a positional call stops being readable.
struct SubscriberSetup<'a> {
    cache: &'a RedisCache,
    lock_names: LockNames,
    keyspace: KeyspaceNames,
    waiters: Arc<ReleaseWaiters>,
    subscriber: (SubscriberClient, ConnectHandle),
    signals: Arc<RedisSignals>,
    shutdown: &'a CancellationToken,
}

/// Awaits the initial `PSUBSCRIBE`s and spawns the tasks that ride the
/// subscriber — DESIGN.md §3.2 steps 4 and 5.
///
/// Two always-on patterns, and each is always-on for its own reason:
///
/// - the **keyspace** pattern carries `expired` and `evicted` for every key this
///   plugin owns — cache entries *and* lock leases — and cannot be subscribed
///   lazily because §3.7's eviction signal has to observe keys nobody is
///   watching. Subscribed under `watch_mode: disabled` too, where it delivers no
///   watcher events because there is no registry, and still counts every
///   eviction;
/// - the **lock-release** pattern carries every release under this prefix,
///   because a waiter's interest lasts one loop iteration and per-name
///   subscriptions would put a round trip either side of every retry (see
///   [`LockNames::release_pattern`]).
///
/// Both are awaited before this returns, for the reason DESIGN.md §3.2 step 4
/// gives: Redis pub/sub does not queue or replay for a client that subscribes
/// late, so an event landing in the startup window would be lost with nothing to
/// show for it. `RD-LOCK-003` is the test that would fail first.
///
/// # Errors
/// Whatever [`map_redis_error`] makes of a failing `PSUBSCRIBE`. The subscriber
/// is quit on that path, so a half-open second connection does not outlive the
/// failed startup.
async fn start_subscriber(setup: SubscriberSetup<'_>) -> Result<Subscription, ClusterError> {
    let SubscriberSetup {
        cache,
        lock_names,
        keyspace,
        waiters,
        subscriber: (client, connection),
        signals,
        shutdown,
    } = setup;
    let registry = cache.watch_registry();
    let names = cache.channel_names();

    // The replay task first: a reconnect between the subscribes below and this
    // spawn would otherwise lose the subscription set permanently.
    let manager = client.manage_subscriptions();

    let patterns = [lock_names.release_pattern(), keyspace.pattern().to_owned()];
    for pattern in patterns {
        if let Err(err) = client.psubscribe(pattern).await {
            manager.abort();
            connection.abort();
            quit_subscriber(&client).await;
            return Err(map_redis_error(err));
        }
    }

    // Awaited, and awaited *here*: `psubscribe` resolving is not the server
    // having processed it (see [`confirm_subscriptions`]), and DESIGN.md §3.2
    // step 4's whole point is that no write in the startup window can publish
    // to a channel nobody is listening on yet.
    if let Err(err) = confirm_subscriptions(&client).await {
        manager.abort();
        connection.abort();
        quit_subscriber(&client).await;
        return Err(err);
    }

    // The reconnect observer is cache-only — its whole job is broadcasting a
    // `Reset` after a subscription gap, and a lock has no equivalent to reset —
    // so under `watch_mode: disabled` it is not spawned at all. The watchdog is
    // spawned either way: it also has something to say about the lock.
    let reconnects = registry.as_ref().map(|registry| {
        spawn_reconnect_observer(
            &client,
            Arc::clone(registry),
            Arc::clone(&signals),
            shutdown.clone(),
        )
    });
    let watchdog = spawn_connection_watchdog(connection, registry.clone(), shutdown.clone());
    let fan_out = spawn_fan_out(
        &client,
        FanOutRoutes {
            cache: registry.map(|registry| CacheRoute { registry, names }),
            locks: LockRoute {
                waiters,
                names: lock_names,
            },
            keyspace: Some(keyspace),
            signals,
        },
        shutdown.clone(),
    );
    Ok(Subscription {
        client,
        fan_out,
        reconnects,
        manager,
        watchdog,
    })
}

/// The running combined plugin. Hands its cache backend to the wiring crate for
/// `ClientHub` registration.
///
/// Call [`stop`](Self::stop) on graceful shutdown (DESIGN.md §11). Dropping the
/// handle without it is a programming error and says so.
pub struct RedisClusterHandle {
    /// The concrete backend, kept beside the instrumented one because `stop()`
    /// and `start_subscriber` need `watch_registry()` and `channel_names()` —
    /// neither of which survives the widening to `dyn ClusterCacheBackend`.
    cache: Arc<RedisCache>,
    /// What [`cache`](Self::cache) hands out: the same backend behind the SDK's
    /// telemetry decorator.
    instrumented: Arc<dyn ClusterCacheBackend>,
    lock: Arc<RedisLock>,
    pool: Pool,
    /// `Option`, not a bare field, for the same reason the join handles are:
    /// this type has a `Drop` impl, so `stop` cannot move out of it and uses
    /// `.take()` to drain in place.
    subscription: Option<Subscription>,
    /// The task keeping `cluster_redis_connection_state` current (DESIGN.md §9).
    connection_state: Option<JoinHandle<()>>,
    shutdown: CancellationToken,
    /// Set by `stop` so the `Drop` guard can tell a graceful shutdown apart from
    /// a forgotten one (ADR-006 §Confirmation).
    stopped: bool,
}

impl RedisClusterHandle {
    /// The cache backend, wrapped in the SDK's `InstrumentedCache` decorator.
    ///
    /// The decorator is the supported path for the ADR-004 cache signal set
    /// (DESIGN.md §9), and using it rather than emitting here is what keeps the
    /// span names, the bounded `result` vocabulary, and the cardinality rule
    /// identical to every other provider instead of re-derived per backend. It
    /// also stamps each returned `CacheWatch` with the provider and the sink, so
    /// an `auto_restart`ed consumer's own resets reach
    /// `cluster_watch_resets_total` alongside this plugin's.
    ///
    /// Wrapped once at startup rather than per call: the decorator holds two
    /// `Arc`s, and a fresh one per `cache()` would be two allocations for a
    /// value the wiring asks for once.
    #[must_use]
    pub fn cache(&self) -> Arc<dyn ClusterCacheBackend> {
        Arc::clone(&self.instrumented)
    }

    /// The lock backend, for a profile that binds `lock: { provider: redis }`
    /// alongside this plugin's cache.
    ///
    /// The same backend the standalone `RedisLockPlugin` hands out, over this
    /// plugin's pool rather than a second one — which is the whole difference
    /// between the two shapes (DESIGN.md §3.5).
    #[must_use]
    pub fn lock(&self) -> Arc<dyn DistributedLockBackend> {
        Arc::clone(&self.lock) as Arc<dyn DistributedLockBackend>
    }

    /// Shuts the plugin down (DESIGN.md §11).
    ///
    /// 1. Cancels the shared `CancellationToken`, which the subscriber fan-out
    ///    and reconnect-observer tasks join on, which ends every per-guard lock
    ///    task, and which unparks every blocked `lock()` waiter so it returns
    ///    `Shutdown` rather than `LockTimeout`.
    /// 2. Broadcasts `Closed(Shutdown)` to every active watcher, dispatched
    ///    directly against the registry **before** the fan-out task is awaited,
    ///    so every watcher observes it before `stop()` returns
    ///    (`cpt-cf-clst-fr-shutdown-revoke`). Going through the registry rather
    ///    than the task is what makes that independent of whether the task has
    ///    noticed the cancel yet.
    /// 3. Drains the guard tasks under a bound, **before** the pool closes: a
    ///    task caught mid-`renew` still needs a connection to finish on.
    /// 4. Quits the subscriber, then the command pool, both bounded — so an
    ///    unresponsive server cannot spend a supervisor's whole shutdown budget.
    ///
    /// **No remote cleanup** (`cpt-cf-clst-fr-shutdown-ttl-cleanup`): held
    /// locks, leader claims, and service registrations are left to lapse via
    /// their TTL rather than deleted on the way out. That is nearly free to
    /// honour here — a Redis lease expires by itself, so there is no drain step,
    /// no statement that can half-succeed, and no partial-cleanup failure mode
    /// to alert on. `RD-LOCK-013` asserts the lease keys are still present after
    /// this returns, so a future "tidy up on stop" would fail there on purpose.
    ///
    /// [`POOL_CLOSE_TIMEOUT`]: crate::shutdown::POOL_CLOSE_TIMEOUT
    pub async fn stop(mut self) {
        self.shutdown.cancel();
        // Before the tasks are joined, so a watcher cannot be left waiting on a
        // fan-out that has already exited.
        if let Some(registry) = self.cache.watch_registry() {
            registry.close_all().await;
        }
        self.lock.drain_guards().await;
        if let Some(observer) = self.connection_state.take() {
            let _observer_exited = observer.await;
        }
        if let Some(subscription) = self.subscription.take() {
            let _fan_out_exited = subscription.fan_out.await;
            if let Some(reconnects) = subscription.reconnects {
                let _observer_exited = reconnects.await;
            }
            // `fred`'s replay task ends with the client rather than with the
            // token, so it is aborted rather than joined.
            subscription.manager.abort();
            let _watchdog_exited = subscription.watchdog.await;
            quit_subscriber(&subscription.client).await;
        }
        close_pool(&self.pool).await;
        self.stopped = true;
    }
}

/// Diagnostic guard (ADR-006 §Confirmation), mirroring the wiring's own
/// `ClusterHandle` guard: dropping a `RedisClusterHandle` without calling
/// `stop()` leaves its pool, its subscriber, and its background tasks running,
/// surfaced loudly rather than silently.
impl Drop for RedisClusterHandle {
    fn drop(&mut self) {
        match cancel_and_diagnose_drop(self.stopped, &self.shutdown) {
            DropDiagnosis::StoppedCleanly => {}
            // not-a-catalogued-event: an ADR-006 developer diagnostic, not an
            // operator's business — this is the release-build arm of the same
            // programming error that panics in debug.
            DropDiagnosis::DuringPanic => tracing::warn!(
                "RedisClusterHandle dropped during panic unwind without stop(); skipping debug \
                 panic to avoid double-panic abort"
            ),
            DropDiagnosis::Unstopped => {
                #[cfg(debug_assertions)]
                panic!("RedisClusterHandle dropped without stop() - programming error");
                // not-a-catalogued-event: as above.
                #[cfg(not(debug_assertions))]
                tracing::warn!(
                    "RedisClusterHandle dropped without stop() - programming error; the command \
                     pool and any background tasks may leak"
                );
            }
        }
    }
}
