//! Layer 3 — lifecycle integration scenarios (docs/TESTING.md §4.5),
//! `RD-LIFE-001` through `RD-LIFE-010`.
//!
//! Startup, shutdown, and the two failure modes an operator most needs told
//! apart: a config fault and a backend fault. Reading `InvalidConfig` should send
//! someone to their YAML and `Provider { ConnectionLost }` should send them to
//! their server, so `RD-LIFE-004` and `RD-LIFE-005` are as much about which error
//! is returned as about failing at all.
//!
//! The `stop()` scenarios carry most of the weight here. Redis makes shutdown
//! unusually cheap — there is no schema, no migration, and a lease reaps itself
//! (`RD-LOCK-013`) — so what is left to get wrong is boundedness, and
//! `RD-LIFE-009` drives it against a server that has stopped answering.
//!
//! Teardown is not only `stop()`'s job, which is what `RD-LIFE-010` covers: a
//! startup that fails after the connect has to tear down what the earlier steps
//! started, and the subscriber is the handle that does not close itself.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]

mod common;

use std::time::{Duration, Instant};

use cluster_sdk::cache::{PutRequest, Ttl};
use cluster_sdk::{ClusterError, ProviderErrorKind};
use fred::interfaces::{ClientLike, LuaInterface, PubsubInterface, ServerInterface};
use redis_cluster_plugin::{ALL_SCRIPTS, RedisClusterPlugin, RedisLockPlugin, logs};
use serde_json::json;

/// Downcasts a `catch_unwind` payload to its message.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// `RD-LIFE-001` — `build_and_start` connects, preflights, loads the script
/// catalog, and has the subscriber **live** before it returns.
///
/// The subscriber half is the load-bearing one (DESIGN.md §3.2 step 4). Redis
/// pub/sub does not queue or replay for a client that subscribes late, so a
/// release or a write landing between `build_and_start` returning and the
/// subscription being established is simply lost. Two things are needed for that
/// and only one is obvious: `tokio::spawn` merely schedules, and *awaiting*
/// `psubscribe` is not the server having processed it — `fred` resolves it when
/// the command reaches the connection (DESIGN.md §3.2 step 4). The
/// patterns being visible in `PUBSUB NUMPAT` **from another connection** is the
/// only assertion that distinguishes "we sent a subscribe" from "the server has
/// one".
#[tokio::test]
async fn rd_life_001_start_connects_preflights_loads_scripts_and_subscribes() {
    let (_container, config) = common::start_redis().await;
    let url = config.url.clone();
    let database = config.database;

    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("build_and_start must succeed against a fresh container");

    let raw = common::raw_client_on(&url, database).await;

    // The whole catalog was loaded, once each, at startup — not lazily on first
    // use, which is what makes `RD-CACHE-010`'s "no EVAL fallback fired" mean
    // something.
    let loads = common::command_calls(&raw, "script|load").await;
    assert!(
        loads >= ALL_SCRIPTS.len() as u64,
        "every one of the {} catalogued scripts must be SCRIPT LOADed at startup, saw {loads} \
         SCRIPT LOAD calls",
        ALL_SCRIPTS.len()
    );

    // The two always-on patterns (keyspace events, lock releases) are live on the
    // server before this point — asserted from a *different* connection, so it is
    // the server's view rather than the plugin's belief.
    let patterns: u64 = raw.pubsub_numpat().await.expect("PUBSUB NUMPAT succeeds");
    assert!(
        patterns >= 2,
        "the keyspace and lock-release patterns must be live on the server before \
         build_and_start returns, saw {patterns}"
    );

    handle.stop().await;
}

/// `RD-LIFE-002` — starting twice against the same server is idempotent and
/// creates nothing.
///
/// There is no schema here and no migration to re-run, so "idempotent" is a
/// stronger claim than it is for the Postgres plugin: the second start must leave
/// the keyspace **byte for byte** as it found it. `DBSIZE` before and after is the
/// check, and it would catch a plugin that started writing a registration key, an
/// instance marker, or a lock-table stand-in — anything that turned a stateless
/// startup into a stateful one.
#[tokio::test]
async fn rd_life_002_starting_twice_creates_nothing() {
    let (_container, config) = common::start_redis().await;
    let url = config.url.clone();
    let key_prefix = config.key_prefix.clone();
    let database = config.database;

    let first = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the first start succeeds");
    let raw = common::raw_client_on(&url, database).await;
    let before: u64 = raw.dbsize().await.expect("DBSIZE succeeds");

    let second_config = common::cluster_config_json(
        &url,
        json!({ "key_prefix": key_prefix, "database": database }),
    );
    let second = RedisClusterPlugin::builder(second_config)
        .build_and_start()
        .await
        .expect("a second start against the same server also succeeds");
    let after: u64 = raw.dbsize().await.expect("DBSIZE succeeds");

    assert_eq!(
        before, after,
        "starting a second time must create no keys - there is no schema to migrate and no \
         registration to write (before {before}, after {after})"
    );

    second.stop().await;
    first.stop().await;
}

/// `RD-LIFE-003` — `stop()` closes every connection this plugin opened, the
/// command pool and the subscriber alike.
///
/// Counted through `INFO clients` rather than `CLIENT LIST`: `client_list` sits on
/// `fred`'s `i-client` interface, which DESIGN.md §3.1 deliberately leaves out of the feature
/// list, so `connected_clients` is what a build of this plugin can actually read.
/// It counts the whole server, so the assertion is on the *drop* — the control
/// connection this test holds is one of them and stays.
///
/// A leaked subscriber is the specific failure worth catching: it is a single
/// connection, so it never exhausts anything, and a gear that restarted its
/// cluster profile a few hundred times would accumulate them silently.
#[tokio::test]
async fn rd_life_003_stop_closes_every_connection() {
    let (_container, config) = common::start_redis_with(json!({ "pool_size": 4 })).await;
    let url = config.url.clone();
    let database = config.database;

    let raw = common::raw_client_on(&url, database).await;
    let baseline = connected_clients(&raw).await;

    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts");
    let while_running = connected_clients(&raw).await;
    assert!(
        while_running >= baseline + 5,
        "a running plugin holds its pool of 4 plus a subscriber, so at least 5 more connections \
         than the baseline (baseline {baseline}, running {while_running})"
    );

    handle.stop().await;

    let closed = common::wait_until(
        Duration::from_secs(5),
        Duration::from_millis(100),
        async || connected_clients(&raw).await <= baseline,
    )
    .await;
    assert!(
        closed,
        "stop() must close the command pool *and* the subscriber; still {} connections against a \
         baseline of {baseline}",
        connected_clients(&raw).await
    );
}

/// `RD-LIFE-010` — a startup that fails *after* the connect leaks nothing.
///
/// The failure half of `RD-LIFE-003`, and the half that is easy to get wrong.
/// DESIGN.md §3.2 step 6 opens "A failure at any step tears down whatever the
/// earlier steps started", and between `connect()` and `start_subscriber` there
/// are two steps — the preflight and the `SCRIPT LOAD` — that had only the pool
/// to tear down. **Dropping the `SubscriberClient` does not close it**:
/// `fred` 10.1.0 gates `impl Drop for ClientInner` behind
/// `credential-provider`, which nothing in this tree enables, so the router task
/// `init()` spawned keeps the socket open under the reconnect policy. A
/// supervisor retrying gear boot every 30 s accumulates one connection and one
/// task per attempt, indefinitely.
///
/// A contradicted `durability` hint is the cheapest post-connect failure: the
/// server reports `everysec`, the operator claims `always`, and the preflight
/// refuses it (`RD-SPEC-011`'s first half). It fails on the first of the two
/// paths, which is enough — both call the same helper.
///
/// Asserted with `wait_until` rather than an immediate read, because `QUIT` is
/// asynchronous server-side. The control connection this test holds is the
/// baseline and stays.
#[tokio::test]
async fn rd_life_010_a_failed_startup_leaks_no_subscriber_connection() {
    let (_container, config) =
        common::start_redis_everysec_with(json!({ "durability": "fsync_always", "pool_size": 4 }))
            .await;
    let url = config.url.clone();
    let database = config.database;

    let raw = common::raw_client_on(&url, database).await;
    let baseline = connected_clients(&raw).await;

    match RedisClusterPlugin::builder(config).build_and_start().await {
        Err(ClusterError::InvalidConfig { .. }) => {}
        Err(other) => {
            panic!("expected the contradicted hint to fail as InvalidConfig, got {other:?}")
        }
        // Stopped rather than dropped: an un-stopped handle panics on drop
        // (ADR-006), which would replace this scenario's message with a teardown
        // one.
        Ok(started) => {
            started.stop().await;
            panic!("a durability hint the server contradicts must fail startup");
        }
    }

    let closed = common::wait_until(
        Duration::from_secs(5),
        Duration::from_millis(100),
        async || connected_clients(&raw).await <= baseline,
    )
    .await;
    assert!(
        closed,
        "a startup that fails after the connect must close the subscriber as well as the pool. \
         Dropping the client closes nothing in this build of fred, so the connection and its \
         router task survive the returned error; still {} connections against a baseline of \
         {baseline}",
        connected_clients(&raw).await
    );
}

/// `connected_clients` from `INFO clients`.
async fn connected_clients(client: &fred::clients::Client) -> u64 {
    use fred::types::InfoKind;
    let info: String = client
        .info(Some(InfoKind::Clients))
        .await
        .expect("INFO clients succeeds");
    for line in info.lines() {
        if let Some(value) = line.trim().strip_prefix("connected_clients:") {
            return value
                .trim()
                .parse()
                .expect("connected_clients is reported as a number");
        }
    }
    // Not `0`: `rd_life_003` and `rd_life_010` both assert
    // `connected_clients(..) <= baseline`, so a helper that answered `0` for
    // output it could not parse would satisfy them without a single connection
    // having been verified — a fixture fault reading as a passing test, which is
    // the one failure mode a leak check must not have.
    panic!("INFO clients must report connected_clients, got: {info}")
}

/// `RD-LIFE-004` — a malformed URL is rejected as **config**, not as a fault.
///
/// `InvalidConfig` rather than `Provider`, and immediately rather than after a
/// connect budget. The distinction is the operator's next action: a `Provider`
/// error sends someone to look at their Redis, and there is nothing wrong with
/// their Redis. DESIGN.md §10 makes this a rule rather than a nicety — a config
/// fault must never read as a backend fault.
#[tokio::test]
async fn rd_life_004_a_malformed_url_is_config_not_a_fault() {
    let config = common::cluster_config_json("not-a-redis-url", json!({}));
    let started = Instant::now();
    let result = RedisClusterPlugin::builder(config).build_and_start().await;
    let elapsed = started.elapsed();

    let Err(err) = result else {
        panic!("a malformed URL must not start a plugin");
    };
    assert!(
        matches!(err, ClusterError::InvalidConfig { .. }),
        "a URL fred cannot parse is an operator error, not a backend one - reading a Provider \
         error here would send someone to inspect a perfectly healthy server. Got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "and it must fail before dialling anything, took {elapsed:?}"
    );
}

/// `RD-LIFE-005` — a valid URL pointing at a closed port fails startup, bounded.
///
/// Three ways this could go wrong and all three are covered by one assertion pair:
/// hanging (the reconnect policy retrying its full ~6-minute schedule), returning
/// `Ok` with a background reconnect (so every later command fails against a plugin
/// that reported healthy), or reporting the wrong kind.
///
/// It is bounded only because `connect.rs` wraps `init()` in `CONNECT_TIMEOUT` —
/// which became necessary the moment a reconnect policy existed, since the policy
/// applies to the *initial* connect too (DESIGN.md §10). A
/// regression removing that wrapper passes every unit test and fails here.
#[tokio::test]
async fn rd_life_005_an_unreachable_server_fails_startup_bounded() {
    // Port 1 on loopback: valid to parse, nothing listening, and refused fast
    // rather than filtered (which would time out instead).
    let config = common::cluster_config_json("redis://127.0.0.1:1", json!({}));
    let started = Instant::now();
    let result = RedisClusterPlugin::builder(config).build_and_start().await;
    let elapsed = started.elapsed();

    let Err(err) = result else {
        panic!(
            "an unreachable server must not return a started plugin with a background reconnect"
        );
    };
    assert!(
        matches!(
            err,
            ClusterError::Provider {
                kind: ProviderErrorKind::ConnectionLost,
                ..
            }
        ),
        "an unreachable server is a backend fault and retryable, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "startup must fail inside the connect budget rather than spending the reconnect policy's \
         whole schedule, took {elapsed:?}"
    );
}

/// `RD-LIFE-006` — dropping a handle without `stop()` panics in a debug build
/// (ADR-006), for **both** handle shapes; `stop()`-then-drop does neither.
///
/// The guard exists because the failure it catches is otherwise silent: a dropped
/// handle leaves its pool, its subscriber, and its background tasks running, and
/// nothing observable goes wrong until the connections accumulate. A test build is
/// a debug build, so this is also what makes the *other* scenarios' teardown
/// discipline enforced rather than merely intended.
///
/// Both shapes are asserted separately because they are two `Drop` impls. The
/// Postgres plugin's two copies of this rule drifted, which is why this crate
/// shares one `cancel_and_diagnose_drop` between them — and why the test still
/// checks each.
#[cfg(debug_assertions)]
#[tokio::test]
async fn rd_life_006_drop_without_stop_panics_in_debug() {
    let (_container, config) = common::start_redis().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the combined plugin starts");
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(handle)))
        .expect_err("dropping an un-stopped RedisClusterHandle must panic in debug");
    assert!(
        panic_message(&*payload).contains("RedisClusterHandle dropped without stop()"),
        "the panic must name the programming error, got {:?}",
        panic_message(&*payload)
    );

    let (_lock_container, lock_config) = common::start_redis_lock_only().await;
    let lock_handle = RedisLockPlugin::builder(lock_config)
        .build_and_start()
        .await
        .expect("the standalone lock plugin starts");
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(lock_handle)))
        .expect_err("dropping an un-stopped RedisLockHandle must panic in debug");
    assert!(
        panic_message(&*payload).contains("RedisLockHandle dropped without stop()"),
        "the standalone handle's guard must fire too, got {:?}",
        panic_message(&*payload)
    );
}

/// `RD-LIFE-006` (the clean half) — `stop()` then drop is silent.
///
/// Separated from the panic half so a regression that made the guard fire
/// *always* is distinguishable from one that made it never fire. Without this, a
/// guard that panicked unconditionally would pass the scenario above.
#[tokio::test]
async fn rd_life_006_stop_then_drop_is_silent() {
    let (_container, config) = common::start_redis().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts");
    handle.stop().await;

    // `stop()` consumes the handle, so the drop under test already happened
    // inside it. Reaching this line is the whole assertion: a guard that fired
    // on a cleanly-stopped handle would have panicked out of `stop()` above.
}

/// `RD-LIFE-007` — a `Drop` during panic unwind degrades to a warning instead of
/// aborting the process.
///
/// A debug-build panic inside `Drop` **while already unwinding** is a double panic,
/// which Rust turns into an immediate `abort()` — no unwinding, no test harness
/// report, no original failure message. So the guard that exists to make a
/// forgotten `stop()` loud would, in exactly the situation where a test is already
/// failing, destroy the report of what actually went wrong. `cancel_and_diagnose_drop`
/// checks `std::thread::panicking()` and logs instead.
///
/// This test passing *at all* is most of the assertion: if the degradation were
/// missing, the process would abort and the whole binary's results would be lost
/// rather than this one test failing.
#[tokio::test]
async fn rd_life_007_drop_during_unwind_degrades_to_a_warning() {
    let (_container, config) = common::start_redis().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts");

    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        // `handle` is owned by this closure, so it drops during the unwind the
        // panic below starts.
        let _owned = handle;
        panic!("the original failure");
    }))
    .expect_err("the closure's own panic must propagate");

    assert_eq!(
        panic_message(&*payload),
        "the original failure",
        "the original panic must survive: a double panic from the Drop guard would have aborted \
         the process instead, taking the failure report with it"
    );
}

/// `RD-LIFE-008` — a `NOSCRIPT` is recovered transparently, once, and counted.
///
/// `SCRIPT FLUSH` behind the plugin's back is the realistic form of this: a Redis
/// restart, a failover to a replica that never saw the `SCRIPT LOAD`, or an
/// operator clearing the cache. The recovery is a key-routed `EVAL` rather than a
/// second `SCRIPT LOAD` plus a re-`EVALSHA` (DESIGN.md §6):
/// `EVAL` necessarily reaches the node that reported the miss, costs one round trip
/// instead of two, and caches the script there on the way through.
///
/// No error may reach the caller — that is what "transparently" means, and it is
/// the difference between a self-healing plugin and one that fails every write
/// after a restart until someone notices.
#[tokio::test]
async fn rd_life_008_noscript_is_recovered_transparently_and_counted() {
    let (meter, metrics) = common::in_memory_meter();
    let (_container, config) = common::start_redis().await;
    let url = config.url.clone();
    let database = config.database;
    let handle = RedisClusterPlugin::builder(config)
        .__with_meter(meter)
        .build_and_start()
        .await
        .expect("the plugin starts");
    let cache = handle.cache();

    cache
        .put(PutRequest {
            key: "flush:key",
            value: b"before",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("a put before the flush succeeds");
    assert_eq!(
        metrics.counter("cluster_redis_script_reloads_total"),
        0,
        "nothing has been flushed yet, so no recovery may have fired"
    );

    let raw = common::raw_client_on(&url, database).await;
    raw.script_flush(false)
        .await
        .expect("SCRIPT FLUSH succeeds");

    cache
        .put(PutRequest {
            key: "flush:key",
            value: b"after",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect(
            "the put after the flush must still succeed - the recovery is transparent to the \
                 caller, not an error it has to retry",
        );
    assert_eq!(
        cache
            .get("flush:key")
            .await
            .expect("get succeeds")
            .expect("the entry is present")
            .value,
        b"after",
        "and it must have actually written"
    );

    assert_eq!(
        metrics.counter("cluster_redis_script_reloads_total"),
        1,
        "exactly one recovery: the EVAL caches the script on the node it reaches, so a second \
         write must not need another"
    );

    cache
        .put(PutRequest {
            key: "flush:key",
            value: b"third",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("a third put succeeds");
    assert_eq!(
        metrics.counter("cluster_redis_script_reloads_total"),
        1,
        "the recovery is not repeated once the script is cached again"
    );

    handle.stop().await;
}

/// `RD-LIFE-009` — `stop()` terminates against a server that has stopped
/// answering.
///
/// The container is **paused**: sockets stay open and nothing replies, which is
/// the case a shutdown path can hang on. The plugin holds locks and an active
/// watch when it happens, so `stop()` has guard tasks to drain, a terminal
/// broadcast to make, a subscriber to quit, and a pool to close — every step of
/// DESIGN.md §11 with a server that will not cooperate.
///
/// It is bounded rather than lucky, and by a general property rather than by any
/// one timeout: every command this plugin issues carries a client-side
/// `command_timeout`, so no in-flight operation can hold a background task's join
/// open indefinitely (DESIGN.md §12), and `POOL_CLOSE_TIMEOUT` and
/// `GUARD_DRAIN_TIMEOUT` bound the two steps that wait on something other than a
/// command.
#[tokio::test]
async fn rd_life_009_stop_terminates_against_an_unresponsive_server() {
    let (container, config) = common::start_redis_with(json!({ "command_timeout_ms": 500 })).await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts");
    let cache = handle.cache();
    let lock = handle.lock();

    let mut guards = Vec::new();
    for index in 0..4 {
        guards.push(
            lock.try_lock(&format!("res-{index}"), Duration::from_secs(30))
                .await
                .expect("acquire"),
        );
    }
    let _watch = cache.watch("shut:key").await.expect("watch succeeds");

    container.pause().await.expect("the container pauses");

    let started = Instant::now();
    handle.stop().await;
    let elapsed = started.elapsed();

    // Unpause so the container can be removed cleanly on drop.
    container.unpause().await.expect("the container unpauses");

    assert!(
        elapsed < Duration::from_secs(30),
        "stop() must return inside a supervisor's shutdown budget even against a server that \
         answers nothing, took {elapsed:?}"
    );
    drop(guards);
}

/// The `RD-LIFE-*` log events, checked as a set.
///
/// A cheap guard on something none of the scenarios above would catch: an event
/// emitted under a name that is not its catalogued one. ADR-004's naming contract
/// is only useful if a collector can match on it, and a renamed event breaks every
/// dashboard and alert silently — the plugin keeps working and the operator stops
/// hearing from it.
///
/// Only the startup events are asserted here, since those are the ones a lifecycle
/// scenario provokes; the eviction, consistency, and keyspace events belong to
/// `redis_specific.rs`, which has the fixtures that produce them.
#[tokio::test]
async fn rd_life_startup_events_use_their_catalogued_names() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (_container, config) = common::start_redis().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts");

    // A stock container is EventuallyConsistent, so exactly this one WARN fires.
    assert_eq!(
        common::count_occurrences(&log, logs::WEAK_CONSISTENCY),
        1,
        "the weak-consistency WARN must be logged once, under its catalogued name. Captured: {}",
        common::captured(&log)
    );
    // And its counterpart must not: an asserted declaration is by construction a
    // Linearizable one, so exactly one of the pair can fire per startup.
    assert_eq!(
        common::count_occurrences(&log, logs::CONSISTENCY_ASSERTED),
        0,
        "consistency_asserted fires only for a Linearizable declaration resting on an unverifiable \
         hint, which this is not. Captured: {}",
        common::captured(&log)
    );

    handle.stop().await;
}
