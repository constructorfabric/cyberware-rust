//! Layer 3 — Redis-specific scenarios (docs/TESTING.md §4.6).
//!
//! The declaration-and-environment tests. Several are the **only** coverage of
//! DESIGN.md's honesty claims, which no conformance scenario can reach: a
//! conformance suite exercises what a backend does, and these exercise whether
//! what it *says about itself* is true.
//!
//! That is not a stylistic distinction. This plugin's declared consistency is
//! **computed from the server it is talking to** (DESIGN.md §3.6), and every
//! capability check downstream — the strict leader constructor, the SDK default
//! lock's guard, `CacheCapability::Linearizable` — dispatches on that answer. A
//! plugin that declared `Linearizable` against a replicated server would defeat
//! all of them at once, and no scenario in §4.2–§4.5 would notice.
//!
//! # Four of these need a multi-node fixture
//!
//! `RD-SPEC-002` (replicated topology) runs on the Sentinel fixture, and
//! `RD-SPEC-008`/`009`/`010` (cluster mode) on the 3-node Cluster one. Both
//! fixtures are built and both run on every PR: `nextest` overlaps their
//! container startup with the rest of the suite, so the quorum and slot-assignment
//! waits cost no wall clock against the run as a whole.
//!
//! They are what closes the two claims that would otherwise rest on unit tests:
//! `RD-SPEC-010` is the gate DESIGN.md §13 D2 designates for lifting cluster-mode
//! `prefix_watch: false`, and `RD-SPEC-002` puts §3.6's replicated row against a
//! live replica rather than against parsed `INFO replication` output.
//!
//! # Why so many scenarios take their own container
//!
//! Almost every fixture here differs in *server* configuration, and two of them
//! mutate it. `RD-SPEC-005b` in particular issues the plugin's one
//! `CONFIG SET notify-keyspace-events`, which is server-wide and outlives the
//! test — sharing a container with `RD-SPEC-005`, which asserts those flags are
//! absent, would make the pair order-dependent and intermittently red.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cluster_sdk::cache::{
    CacheCapability, CacheConsistency, CacheEvent, CacheWatchEvent, PutRequest, Ttl,
};
use cluster_sdk::{ClusterCacheBackend, ClusterError};
use fred::interfaces::ServerInterface;
use redis_cluster_plugin::{RedisClusterPlugin, logs};
use serde_json::json;

const VALUE: &[u8] = b"v";

/// `RD-SPEC-001` — a stock container declares `EventuallyConsistent`, the lock
/// agrees, and the WARN fires exactly once.
///
/// The plugin's default posture, and the one an operator will actually meet.
/// "Exactly once" matters because this WARN appears in the log of **every**
/// ordinary Redis deployment: emitted per operation it would be noise an operator
/// filters out, and the one time it mattered they would have filtered it already.
///
/// The lock's declaration is asserted alongside the cache's because they come from
/// one preflight: the lock is exactly as safe as the server it runs on
/// (DESIGN.md §5.1), and a lock that declared `linearizable: true` over a weak
/// cache would let a consumer require the capability and be told it was met.
#[tokio::test]
async fn rd_spec_001_a_stock_container_declares_eventually_consistent() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (_container, config) = common::start_redis().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("a stock container starts - a weak declaration is not a startup failure");

    assert_eq!(
        handle.cache().consistency(),
        CacheConsistency::EventuallyConsistent,
        "stock Redis has no AOF, so nothing is fsynced before it is acknowledged"
    );
    assert!(
        !handle.lock().features().linearizable,
        "the lock's declaration tracks the cache's - both come from one preflight"
    );
    assert_eq!(
        common::count_occurrences(&log, logs::WEAK_CONSISTENCY),
        1,
        "the weak-consistency WARN must be logged exactly once at startup, not per operation. \
         Captured: {}",
        common::captured(&log)
    );

    handle.stop().await;
}

/// `RD-SPEC-003` — a verified single-node durable topology declares
/// `Linearizable`, and logs **no** weak-consistency WARN.
///
/// The positive branch the leader conformance suite depends on
/// (`tests/conformance.rs::leader_conformance`): `CasBasedLeaderElectionBackend::new`
/// is the strict constructor and accepts only this. If the declaration on this
/// fixture ever weakened, that whole suite would stop being able to construct its
/// backend — so this scenario is what localises such a regression to the
/// declaration rather than to the leader suite.
///
/// The absent WARN is asserted with a **thread-local** capture, since it is a
/// negative: a process-global buffer would be polluted by every other test's
/// plugin, and `RD-SPEC-001`'s own WARN would satisfy the search.
#[tokio::test]
async fn rd_spec_003_a_durable_single_node_declares_linearizable() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (_container, config) = common::start_redis_durable().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the durable single-node fixture starts");

    assert_eq!(
        handle.cache().consistency(),
        CacheConsistency::Linearizable,
        "a verified single node with appendonly yes and appendfsync always is the one \
         configuration ADR-009 rates safe"
    );
    assert!(
        handle.lock().features().linearizable,
        "and the lock declares it too"
    );
    assert_eq!(
        common::count_occurrences(&log, logs::WEAK_CONSISTENCY),
        0,
        "no weak-consistency WARN may be logged for a Linearizable declaration. Captured: {}",
        common::captured(&log)
    );
    assert_eq!(
        common::count_occurrences(&log, logs::CONSISTENCY_ASSERTED),
        0,
        "and this declaration is *verified*, not asserted - CONFIG GET was readable, so nothing \
         rests on an operator hint. Captured: {}",
        common::captured(&log)
    );

    handle.stop().await;
}

/// `RD-SPEC-004` — the strict leader constructor **refuses** a weak cache, and a
/// profile that omits `leader_election` over a Redis cache fails startup.
///
/// This asserts the blocker rather than working around it (DESIGN.md §7, §13 D1),
/// and it is the more important half of the `004`/`004b` pair: it pins that the
/// opt-in defaults to *off*, so a weak-consistency leader election can only ever
/// be something an operator asked for in writing.
///
/// The wiring half is what makes it real. `ClusterWiring::from_config` auto-fills
/// the omitted primitives with the SDK defaults over the profile's cache, so
/// `cache: { provider: redis }` alone produces exactly the deployment shape
/// `../../../docs/DESIGN.md` §4.2 lists as "Redis-only" — and it must fail loudly
/// with an actionable error rather than start and elect two leaders later.
#[tokio::test]
async fn rd_spec_004_the_strict_leader_constructor_refuses_a_weak_cache() {
    use cluster::defaults::CasBasedLeaderElectionBackend;
    use cluster::{ClusterConfig, ClusterWiring, ProviderRegistry};
    use redis_cluster_plugin::RedisCacheProvider;
    use toolkit::client_hub::ClientHub;

    let (_container, config) = common::start_redis().await;
    let url = config.url.clone();
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin itself starts fine - it is the CAS default over it that cannot");

    // The direct half: the constructor itself refuses.
    assert!(
        CasBasedLeaderElectionBackend::new(handle.cache()).is_err(),
        "the strict constructor must refuse an EventuallyConsistent cache rather than construct a \
         backend that can elect two leaders"
    );
    handle.stop().await;

    // The wiring half: the same refusal reached through operator config.
    let mut profiles = serde_json::Map::new();
    profiles.insert(
        "redisweak".to_owned(),
        json!({ "cache": { "provider": "redis", "url": url } }),
    );
    let cluster_config: ClusterConfig =
        serde_json::from_value(json!({ "profiles": profiles })).expect("the profile config parses");
    let providers = ProviderRegistry::new().with_cache_provider(Arc::new(RedisCacheProvider));
    let outcome =
        ClusterWiring::from_config(Arc::new(ClientHub::new()), &cluster_config, &providers).await;

    let Err(err) = outcome else {
        panic!(
            "a profile binding cache: redis and omitting leader_election must fail startup - the \
             omit-default is a CAS backend that rejects a weak cache, and starting anyway would \
             mean silently electing two leaders"
        );
    };
    let message = format!("{err:?}");
    assert!(
        message.to_lowercase().contains("consisten"),
        "the error must be actionable about *why* - an operator needs to know their cache's \
         consistency is the problem, not merely that startup failed. Got {message}"
    );
}

/// `RD-SPEC-004b` — the opt-in flag reaches the weak constructor, and launders
/// nothing.
///
/// The end-to-end half of the pair whose wiring-level half lives in
/// `cluster/src/config_tests.rs`. Two things about the profile are load-bearing
/// (DESIGN.md §13 D1):
///
/// - **`provider: default`**, not an omitted provider. `BackendBinding.provider` is
///   a required field, so `leader_election: { allow_weak_consistency: true }`
///   cannot deserialize at all; the reserved name `default` means "the SDK default
///   backend over this profile's cache, with options". Making `provider` an
///   `Option` instead was rejected because `BackendBinding` flattens unknown keys
///   into `options`, so a misspelled `providr: redis` would silently become an SDK
///   default rather than a startup error — a config typo must never change which
///   backend runs.
/// - **An explicit `lock: { provider: redis }`.** The omit-default *lock* is also a
///   CAS backend sharing the same `reject_weak_consistency` guard, so a profile
///   that opted in on leader election alone would clear that hurdle and then die on
///   the lock. A single-flag implementation passes every leader test and fails only
///   here.
///
/// The capability assertion at the end is what keeps the flag honest: waiving a
/// constructor guard confers no guarantee, so a consumer that *requires*
/// linearizability must still be refused.
#[tokio::test]
async fn rd_spec_004b_the_opt_in_reaches_the_weak_constructor_without_laundering() {
    use cluster::{ClusterConfig, ClusterWiring, ProfileRegistry, ProviderRegistry};
    use cluster_sdk::leader::LeaderStatus;
    use cluster_sdk::profile::ClusterProfile;
    use cluster_sdk::{ClusterCacheV1, LeaderElectionV1};
    use redis_cluster_plugin::{RedisCacheProvider, RedisLockProvider};
    use toolkit::client_hub::ClientHub;

    #[derive(Clone, Copy)]
    struct WeakProfile;
    impl ClusterProfile for WeakProfile {
        const NAME: &'static str = "redisweakoptin";
    }

    let (_container, config) = common::start_redis().await;
    let url = config.url.clone();

    let mut profiles = serde_json::Map::new();
    profiles.insert(
        WeakProfile::NAME.to_owned(),
        json!({
            "cache": { "provider": "redis", "url": url },
            // The reserved sentinel plus the explicit acknowledgement.
            "leader_election": { "provider": "default", "allow_weak_consistency": true },
            // Defect 2: without this the omit-default CAS lock rejects the same
            // weak cache and startup fails after leader election has been cleared.
            "lock": { "provider": "redis", "url": url },
        }),
    );
    let cluster_config: ClusterConfig = serde_json::from_value(json!({ "profiles": profiles }))
        .expect("the opt-in profile config parses");

    let providers = ProviderRegistry::new()
        .with_cache_provider(Arc::new(RedisCacheProvider))
        .with_lock_provider(Arc::new(RedisLockProvider));
    let hub = Arc::new(ClientHub::new());
    let (mut handle, bound) = ClusterWiring::from_config(
        Arc::clone(&hub),
        &cluster_config,
        &providers,
    )
    .await
    .expect(
        "the same profile that fails in RD-SPEC-004 must start once the operator opts in on \
             both CAS defaults",
    );
    // The gear's post-`from_config` step: publish the bound set so the process can
    // resolve the profile through its local cluster client.
    handle.publish(&Arc::new(ProfileRegistry::new()), bound);

    // The resolved backend really elects, over the weak Redis cache.
    let leader = LeaderElectionV1::resolver(&hub)
        .profile(WeakProfile)
        .resolve()
        .await
        .expect("the leader-election facade resolves for the opted-in profile");
    let mut watch = leader.elect("svc").await.expect("elect succeeds");
    let became_leader = tokio::time::timeout(Duration::from_secs(10), async {
        while !watch.is_leader() {
            let _event = watch.changed().await;
        }
    })
    .await
    .is_ok();
    assert!(
        became_leader,
        "the sole candidate must become leader over the weak cache - the flag routes to the SDK's \
         new_allow_weak_consistency constructor, which works, rather than to a stub. Last status: \
         {:?}",
        watch.status()
    );
    assert_eq!(watch.status(), LeaderStatus::Leader);

    // And the flag launders nothing: a consumer requiring linearizability is still
    // refused, naming the capability and the provider. `ClusterCacheV1` is not
    // `Debug`, so the outcome is matched rather than formatted.
    match ClusterCacheV1::resolver(&hub)
        .profile(WeakProfile)
        .require(CacheCapability::Linearizable)
        .resolve()
        .await
    {
        Err(ClusterError::CapabilityNotMet { capability, .. }) => {
            assert_eq!(
                capability, "Linearizable",
                "the refusal must name the capability that was not met, so an operator reading the \
                 startup error knows which requirement their backend cannot serve"
            );
        }
        Err(other) => panic!("expected CapabilityNotMet, got {other:?}"),
        Ok(_resolved) => panic!(
            "opting in waives a *constructor guard*, not the capability model - a consumer that \
             requires Linearizable must still be told it is not available"
        ),
    }

    drop(watch);
    handle.stop().await;
}

/// `RD-SPEC-005` — missing keyspace notifications degrade loudly and safely.
///
/// The trade in one scenario: **promptness is lost, correctness is intact.**
/// `Changed` and `Deleted` still arrive because the plugin publishes those itself
/// from inside its scripts; `Expired` never does, because that one can only come
/// from the server. And an expired entry still reads as absent — the TTL is
/// Redis's own `PEXPIRE`, so expiry is unaffected by whether anyone is *told*
/// about it.
///
/// This is the environment a managed Redis often is, so startup must not fail. It
/// must, however, say so once — an operator debugging "why don't I get expiry
/// events" needs the answer in their log rather than in this document.
#[tokio::test]
async fn rd_spec_005_missing_keyspace_notifications_degrade_loudly_and_safely() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (_container, config) = common::start_redis_no_notifications().await;
    let url = config.url.clone();
    let database = config.database;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect(
            "a server without keyspace notifications must still start - this is what a managed \
                 Redis often looks like",
        );
    let cache = handle.cache();
    let raw = common::raw_client_on(&url, database).await;

    assert_eq!(
        common::keyspace_flags(&raw).await,
        "",
        "the fixture must genuinely have no flags, and the plugin must not have set them: \
         manage_keyspace_notifications defaults to false because the setting is server-wide"
    );
    assert_eq!(
        common::count_occurrences(&log, logs::EXPIRY_EVENTS_UNAVAILABLE),
        1,
        "the degradation must be reported once. Captured: {}",
        common::captured(&log)
    );

    let mut watch = cache.watch("deg:key").await.expect("watch succeeds");
    cache
        .put(PutRequest {
            key: "deg:key",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_millis(400)),
        })
        .await
        .expect("put succeeds");
    let changed = tokio::time::timeout(Duration::from_secs(2), watch.recv())
        .await
        .expect("Changed still arrives")
        .expect("the stream is open");
    assert!(
        matches!(changed, CacheWatchEvent::Event(CacheEvent::Changed { .. })),
        "Changed is published by the plugin's own script and is unaffected, got {changed:?}"
    );

    // The entry still expires — that is Redis's PEXPIRE, not a notification.
    let gone = common::wait_until(
        Duration::from_secs(4),
        Duration::from_millis(50),
        async || matches!(cache.get("deg:key").await, Ok(None)),
    )
    .await;
    assert!(
        gone,
        "correctness is intact: the TTL is the server's own and expires regardless of whether \
         anybody is told"
    );

    // But no `Expired` ever arrives.
    let stray = tokio::time::timeout(Duration::from_millis(700), watch.recv()).await;
    assert!(
        !matches!(
            stray,
            Ok(Some(CacheWatchEvent::Event(CacheEvent::Expired { .. })))
        ),
        "no Expired may arrive without keyspace notifications - this is exactly the promptness \
         that was lost, got {stray:?}"
    );

    handle.stop().await;
}

/// `RD-SPEC-005b` — `manage_keyspace_notifications: true` sets the flags, says so,
/// and `Expired` then arrives.
///
/// **This scenario has its own container, deliberately.** It is the one place the
/// plugin mutates server state: `CONFIG SET notify-keyspace-events` is server-wide
/// and outlives the test, so sharing a container with `RD-SPEC-005` — which asserts
/// the flags are *absent* — would make the pair order-dependent and intermittently
/// red depending on which ran first.
///
/// `keyspace_notifications_set` is INFO rather than DEBUG for the same reason this
/// whole scenario needs its own container: it is the only
/// configuration *mutation* this plugin performs, and it is global. An operator
/// should be able to find it without turning DEBUG on.
///
/// The merge is additive, not a replacement — the plugin adds the flags it needs to
/// whatever is already there, since clobbering the value would switch off
/// notifications another tenant of the same server depends on.
#[tokio::test]
async fn rd_spec_005b_managed_keyspace_notifications_are_set_and_reported() {
    let (_guard, log) = common::scoped_capture(tracing::Level::INFO);
    let (_container, config) =
        common::start_redis_no_notifications_with(json!({ "manage_keyspace_notifications": true }))
            .await;
    let url = config.url.clone();
    let database = config.database;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts and manages the flags");
    let cache = handle.cache();
    let raw = common::raw_client_on(&url, database).await;

    let flags = common::keyspace_flags(&raw).await;
    for required in ['K', 'x', 'e'] {
        assert!(
            flags.contains(required),
            "CONFIG GET must confirm the flag `{required}` was set; got {flags:?}. Kxe is the \
             minimal correct set: K+x yields expired and K+e yields evicted, while g and $ would \
             add a notification per generic and per string command server-wide"
        );
    }
    assert_eq!(
        common::count_occurrences(&log, logs::KEYSPACE_NOTIFICATIONS_SET),
        1,
        "the only server-wide mutation this plugin performs must be reported at INFO, once. \
         Captured: {}",
        common::captured(&log)
    );
    assert_eq!(
        common::count_occurrences(&log, logs::EXPIRY_EVENTS_UNAVAILABLE),
        0,
        "and the degradation WARN must *not* fire, since the flags are now present. Captured: {}",
        common::captured(&log)
    );

    // The payoff: Expired now arrives where RD-SPEC-005 saw none.
    let mut watch = cache.watch("man:key").await.expect("watch succeeds");
    cache
        .put(PutRequest {
            key: "man:key",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_millis(400)),
        })
        .await
        .expect("put succeeds");

    let expired = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match watch.recv().await {
                Some(CacheWatchEvent::Event(CacheEvent::Expired { key })) => return Some(key),
                Some(_other) => {}
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();
    assert_eq!(
        expired.as_deref(),
        Some("man:key"),
        "with the flags set by the plugin, a TTL lapse must deliver Expired"
    );

    handle.stop().await;
}

/// `RD-SPEC-005b` (the default half) — with the flag `false`, no `CONFIG SET` is
/// issued at all.
///
/// Separated so a regression that made the plugin *always* manage the flags is
/// distinguishable from one that made it never. Without this, a plugin that
/// ignored the setting and always wrote would pass the scenario above — and would
/// be silently reconfiguring every server it ever connected to, which is precisely
/// what the default-`false` exists to prevent.
#[tokio::test]
async fn rd_spec_005b_the_default_issues_no_config_set() {
    let (_container, config) = common::start_redis_no_notifications().await;
    let url = config.url.clone();
    let database = config.database;
    let raw = common::raw_client_on(&url, database).await;
    let baseline = common::command_calls(&raw, "config|set").await;

    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts");

    assert_eq!(
        common::command_calls(&raw, "config|set").await,
        baseline,
        "with manage_keyspace_notifications at its default of false, no CONFIG SET may be issued: \
         the setting is server-wide and shared with unrelated tenants, so it is exactly the kind \
         of surprise an operator must opt into"
    );
    assert_eq!(
        common::keyspace_flags(&raw).await,
        "",
        "and the server's flags must be exactly as they were found"
    );

    handle.stop().await;
}

/// `RD-SPEC-006` — an unsafe `maxmemory-policy` is warned about at startup, and
/// startup still succeeds.
///
/// Both halves matter and they pull in opposite directions. `allkeys-lru` means
/// Redis may delete this plugin's keys under memory pressure — an evicted lock
/// lease hands the lock to a second holder, and an evicted leader key elects a
/// second leader — so it has to be said. But `maxmemory-policy` is a *server-wide*
/// setting the gear's operator may not control, so refusing to start would take a
/// service down over a condition it cannot fix (DESIGN.md §3.7).
///
/// This fixture sets the policy with **no** `maxmemory`, so the risk exists and no
/// eviction actually happens; driving a real eviction is `RD-SPEC-007`. Keeping
/// them apart means this scenario cannot pass or fail on whether an eviction
/// happened to land mid-test.
#[tokio::test]
async fn rd_spec_006_an_unsafe_maxmemory_policy_is_warned_at_startup() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (_container, config) = common::start_redis_unsafe_policy().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("an unsafe server-wide policy must not block a gear from starting");

    assert_eq!(
        common::count_occurrences(&log, logs::MAXMEMORY_POLICY_UNSAFE),
        1,
        "the unsafe policy must be named once at startup. Captured: {}",
        common::captured(&log)
    );
    assert!(
        common::captured(&log).contains("allkeys-lru"),
        "and the WARN must name the actual policy, so an operator knows what to change. \
         Captured: {}",
        common::captured(&log)
    );

    // The cache still works — the warning is about risk, not about capability.
    let cache = handle.cache();
    cache
        .put(PutRequest {
            key: "warned:key",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the cache works normally on a server with an unsafe policy");

    handle.stop().await;
}

/// `RD-SPEC-007` — a real eviction is observed, reported, and mapped correctly.
///
/// The plugin's top operational risk, driven end to end under genuine memory
/// pressure rather than asserted in prose. Four claims, and each fails differently:
///
/// - the watcher receives **`Deleted`, not `Expired`**. No TTL elapsed, so calling
///   it `Expired` would tell a consumer its data aged out when in fact the server
///   threw it away (DESIGN.md §3.7);
/// - `cluster.provider.eviction_observed` (WARN) is logged;
/// - `cluster_redis_evictions_observed_total` increments;
/// - `cluster_provider_errors_total` does **not**.
///
/// The last two are the point of the pair. The catalog counter cannot carry an
/// eviction in two independent ways: it takes `{provider, kind}` and no
/// `op` at all, and an eviction is not a `ClusterError`, so it cannot travel
/// through `emit_provider_error`. Folding it on as `kind = "other"` would make an
/// eviction indistinguishable from every other unclassified backend failure *and*
/// inflate a provider-error rate with something that is not an operation failure —
/// which is why the negative assertion is here rather than merely implied.
///
/// The shape of the setup is what makes it reliable: write a small watched victim
/// **first** and never touch it again, then push filler in until `allkeys-lru`
/// picks the least-recently-used key — which is the victim, by construction.
#[tokio::test]
async fn rd_spec_007_a_real_eviction_is_observed_reported_and_mapped() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (meter, metrics) = common::in_memory_meter();
    let (_container, config) = common::start_redis_evicting().await;
    let handle = RedisClusterPlugin::builder(config)
        .__with_meter(meter)
        .build_and_start()
        .await
        .expect("the plugin starts against a memory-capped container");
    let cache = handle.cache();

    // The victim: small, watched, and never read again, so LRU picks it first.
    cache
        .put(PutRequest {
            key: "victim",
            value: b"small",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the victim is written");
    let mut watch = cache.watch("victim").await.expect("watch succeeds");

    // Filler until the victim is gone. 16 KiB values against a 3 MiB ceiling, and
    // a generous bound: the eviction happens when Redis decides it must, not on a
    // schedule this test controls.
    let filler = vec![b'f'; 16 * 1024];
    let mut evicted = false;
    for index in 0..600 {
        cache
            .put(PutRequest {
                key: &format!("filler:{index}"),
                value: &filler,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect(
                "filler writes succeed under allkeys-lru - eviction makes room rather than \
                     refusing the write",
            );
        if matches!(cache.get("victim").await, Ok(None)) {
            evicted = true;
            break;
        }
    }
    assert!(
        evicted,
        "the watched victim must be evicted under real memory pressure, or this scenario proves \
         nothing about eviction"
    );

    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match watch.recv().await {
                Some(CacheWatchEvent::Event(event)) => return Some(event),
                Some(_other) => {}
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
    .expect("the eviction must reach the watcher");
    assert!(
        matches!(&event, CacheEvent::Deleted { key } if key == "victim"),
        "an eviction maps to Deleted, never Expired: no TTL elapsed, the server simply threw the \
         key away, and a consumer told 'Expired' would believe its data aged out. Got {event:?}"
    );

    assert!(
        common::count_occurrences(&log, logs::EVICTION_OBSERVED) >= 1,
        "the eviction WARN must be logged. Captured: {}",
        common::captured(&log)
    );
    assert!(
        metrics.counter("cluster_redis_evictions_observed_total") >= 1,
        "and counted under a name that says what it counts (DESIGN.md sec 3.7)"
    );
    assert_eq!(
        metrics.counter("cluster_provider_errors_total"),
        0,
        "an eviction is not an operation failure and must not inflate the provider-error rate - \
         which is what folding it onto that counter as kind=other would have done, while also \
         making it indistinguishable from every other unclassified backend failure"
    );

    handle.stop().await;
}

/// `RD-SPEC-007b` — an evicted **lock lease** is observed and attributed to the
/// lock, alongside the cache's own evictions.
///
/// `RD-SPEC-007` covers the cache entry. This covers the case DESIGN.md §3.7
/// *opens* with and rates worst: an evicted lease hands the lock to a second
/// holder while the first still believes it holds it, with no TTL having lapsed
/// and nothing else in the system able to notice. The keyspace pattern spans
/// `<prefix>:*` so that notification arrives at all, and the fan-out sorts it by
/// the primitive that owns the key.
///
/// Two things are asserted that a shared signal would fail:
///
/// - the line carries `primitive="lock"`, so an operator can tell a re-read from
///   a double-held lock. A single unlabelled eviction event would leave the two
///   indistinguishable at exactly the moment the difference matters;
/// - the cache's own line survives the same storm. The WARN is rate-limited, and
///   a limiter shared across primitives would let the flood of evicted *entries*
///   — which is what memory pressure produces most of — spend the window the one
///   lock line needs.
///
/// The lease is acquired **first** and never renewed, so it is the oldest
/// untouched key in the database when `allkeys-lru` starts choosing victims. Its
/// TTL is far longer than the scenario, so an expiry cannot be mistaken for the
/// eviction under test.
#[tokio::test]
async fn rd_spec_007b_an_evicted_lock_lease_is_observed_and_attributed() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (meter, metrics) = common::in_memory_meter();
    let (_container, config) = common::start_redis_evicting().await;
    let handle = RedisClusterPlugin::builder(config)
        .__with_meter(meter)
        .build_and_start()
        .await
        .expect("the plugin starts against a memory-capped container");
    let cache = handle.cache();

    // Held for the whole scenario and never renewed: nothing touches this key
    // again, which is what puts it at the front of the LRU queue.
    let _guard_lease = handle
        .lock()
        .try_lock("evictable", Duration::from_mins(10))
        .await
        .expect("the lease is acquired on an empty container");

    // Filler until the lock line appears. The bound is generous for the same
    // reason `RD-SPEC-007`'s is: eviction happens when Redis decides it must,
    // and which key it samples is approximate.
    let filler = vec![b'f'; 16 * 1024];
    let mut lock_evicted = false;
    for index in 0..1_200 {
        cache
            .put(PutRequest {
                key: &format!("filler:{index}"),
                value: &filler,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("filler writes succeed under allkeys-lru");
        if common::count_occurrences(&log, "primitive=\"lock\"") >= 1 {
            lock_evicted = true;
            break;
        }
    }

    assert!(
        lock_evicted,
        "an evicted lock lease must be reported. A keyspace pattern scoped to `<prefix>:c:*` \
         never receives the lease notification at all, which leaves the worst case DESIGN.md \
         sec 3.7 names entirely unreported. Captured: {}",
        common::captured(&log)
    );
    assert!(
        common::count_occurrences(&log, logs::EVICTION_OBSERVED) >= 1,
        "the eviction WARN keeps its contracted name whichever primitive owned the key"
    );
    assert!(
        common::count_occurrences(&log, "primitive=\"cache\"") >= 1,
        "the cache's own eviction line must survive the same storm - a rate limiter shared \
         between the primitives would let the flood of evicted entries suppress it, or the lock \
         line, depending on which arrived first. Captured: {}",
        common::captured(&log)
    );
    assert!(
        metrics.counter("cluster_redis_evictions_observed_total") >= 2,
        "both primitives' evictions are counted on the one counter, separated by the label"
    );
    assert_eq!(
        metrics.counter("cluster_provider_errors_total"),
        0,
        "an evicted lease is still not an operation failure"
    );

    handle.stop().await;
}

/// `RD-SPEC-011` — an operator hint the server contradicts fails startup, and the
/// same hint is trusted when the server will not say.
///
/// The two branches of DESIGN.md §3.6's hint handling, and the asymmetry between
/// them is the design:
///
/// - **Contradicted**: `durability: fsync_always` against a server running
///   `appendfsync everysec` fails startup naming both values. `fsync_always` is
///   the one setting that unlocks `Linearizable`, so it is the one claim worth
///   checking — a wrong hint here does not degrade a guarantee, it fabricates one.
/// - **Unverifiable**: the same hint against a server that refuses `CONFIG GET` is
///   *trusted*, because that is the escape hatch a locked-down managed instance
///   needs, and flagged `consistency_asserted` so the declaration's provenance is
///   in the log rather than only in the operator's memory.
#[tokio::test]
async fn rd_spec_011_a_contradicted_hint_fails_and_an_unverifiable_one_is_flagged() {
    // Contradicted: the server says everysec, the operator says always.
    let (_container, config) =
        common::start_redis_everysec_with(json!({ "durability": "fsync_always" })).await;
    match RedisClusterPlugin::builder(config).build_and_start().await {
        Err(ClusterError::InvalidConfig { reason }) => {
            assert!(
                reason.contains("always") && reason.contains("everysec"),
                "the error must name both the claimed and the actual value, so an operator can see \
                 which of the two to change. Got {reason}"
            );
        }
        Err(other) => panic!("expected InvalidConfig naming both values, got {other:?}"),
        // Stopped rather than dropped: an un-stopped handle panics on drop
        // (ADR-006), which would replace this scenario's failure message with a
        // teardown one.
        Ok(started) => {
            started.stop().await;
            panic!(
                "a durability hint the server contradicts must fail startup: fsync_always is what \
                 unlocks Linearizable, so trusting a false one fabricates a guarantee rather than \
                 degrading one"
            );
        }
    }

    // Unverifiable: CONFIG is denied to the connecting user, so the same hint is
    // trusted — and flagged.
    let (_denied_container, denied_config) =
        common::start_redis_config_denied_with(json!({ "durability": "fsync_always" })).await;
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let handle = RedisClusterPlugin::builder(denied_config)
        .build_and_start()
        .await
        .expect(
            "a server that will not let the plugin read CONFIG must not block startup - that is \
             the managed-Redis escape hatch the hint exists for",
        );

    assert_eq!(
        handle.cache().consistency(),
        CacheConsistency::Linearizable,
        "the hint is trusted when it cannot be checked"
    );
    assert_eq!(
        common::count_occurrences(&log, logs::CONSISTENCY_ASSERTED),
        1,
        "but the declaration's provenance must be in the log: this Linearizable rests on an \
         operator's word rather than on evidence. Captured: {}",
        common::captured(&log)
    );

    handle.stop().await;
}

/// `RD-SPEC-012` — `database` selects a logical DB, and the event channels follow
/// it.
///
/// The second half is the whole scenario. Redis keyspace notifications are
/// published on `__keyspace@<db>__:<key>`, so a plugin that subscribed on `@0`
/// while writing to DB 3 would work perfectly for `Changed` and `Deleted` — which
/// it publishes itself on channels of its own naming — and silently deliver **no
/// `Expired` at all**. That is an off-by-one nobody would notice until an entry
/// aged out and no consumer heard about it.
///
/// DB 0 being untouched is the cheap half, and still worth holding: it is what
/// makes `database` usable for isolating two deployments on one server.
#[tokio::test]
async fn rd_spec_012_the_database_selects_a_logical_db_and_channels_follow() {
    let (_container, config) = common::start_redis_with(json!({ "database": 3 })).await;
    let url = config.url.clone();
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against database 3");
    let cache = handle.cache();

    let mut watch = cache.watch("db:key").await.expect("watch succeeds");
    cache
        .put(PutRequest {
            key: "db:key",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_millis(400)),
        })
        .await
        .expect("put succeeds");

    let in_db_3 = common::raw_client_on(&url, 3).await;
    let in_db_0 = common::raw_client_on(&url, 0).await;
    let size_3: u64 = in_db_3.dbsize().await.expect("DBSIZE succeeds");
    let size_0: u64 = in_db_0.dbsize().await.expect("DBSIZE succeeds");
    assert!(size_3 >= 1, "the key must land in database 3, saw {size_3}");
    assert_eq!(
        size_0, 0,
        "database 0 must be untouched - this is what makes `database` usable for isolating two \
         deployments on one server"
    );

    // And `Expired` arrives, which it only can if the keyspace subscription is on
    // `__keyspace@3__` rather than `@0`.
    let expired = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match watch.recv().await {
                Some(CacheWatchEvent::Event(CacheEvent::Expired { key })) => return Some(key),
                Some(_other) => {}
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();
    assert_eq!(
        expired.as_deref(),
        Some("db:key"),
        "Expired must arrive on database 3: a plugin subscribed on __keyspace@0__ while writing to \
         DB 3 would deliver Changed and Deleted perfectly and no Expired ever"
    );

    handle.stop().await;
}

/// `RD-SPEC-014` — sharded pub/sub is **detected but not used**.
///
/// v1 records the capability without acting on it (DESIGN.md §13 D3): plain
/// `PUBLISH` is broadcast cluster-wide and is therefore correct on every topology,
/// while `SPUBLISH` would be a Cluster-only optimisation with its own
/// slot-migration story. The DEBUG line means a follow-up can tell from a log
/// whether a given deployment could support it.
///
/// The assertion that no `SPUBLISH`/`SSUBSCRIBE` is issued is the important one: it
/// guards against a **half-landed follow-up** silently switching the publish path,
/// which would work on a single node and quietly change delivery semantics on a
/// cluster.
///
/// The Redis 6 half is why this scenario needs two containers. A detection that
/// reported "available" everywhere would be indistinguishable from a working one
/// here, and the version gate is the whole logic.
#[tokio::test]
async fn rd_spec_014_sharded_pubsub_is_detected_but_not_used() {
    {
        let (_guard, log) = common::scoped_capture(tracing::Level::DEBUG);
        let (_container, config) = common::start_redis().await;
        let url = config.url.clone();
        let database = config.database;
        let handle = RedisClusterPlugin::builder(config)
            .build_and_start()
            .await
            .expect("the plugin starts against Redis 7");
        let cache = handle.cache();
        cache
            .put(PutRequest {
                key: "sharded:key",
                value: VALUE,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");

        assert_eq!(
            common::count_occurrences(&log, logs::SHARDED_PUBSUB_AVAILABLE),
            1,
            "Redis 7 supports SPUBLISH/SSUBSCRIBE, and the capability must be recorded once. \
             Captured: {}",
            common::captured(&log)
        );

        let raw = common::raw_client_on(&url, database).await;
        for command in ["spublish", "ssubscribe"] {
            assert_eq!(
                common::command_calls(&raw, command).await,
                0,
                "v1 records the capability and does not act on it: a {command} here would mean a \
                 half-landed follow-up had switched the publish path, which works on a single node \
                 and changes delivery semantics on a cluster"
            );
        }

        handle.stop().await;
    }

    // Redis 6: neither the log line nor the commands.
    let (_guard, log) = common::scoped_capture(tracing::Level::DEBUG);
    let (_container, config) = common::start_redis_6().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against Redis 6 - every command it issues predates 7");
    assert_eq!(
        common::count_occurrences(&log, logs::SHARDED_PUBSUB_AVAILABLE),
        0,
        "Redis 6 has no sharded pub/sub, so the detection must say nothing. A detection that \
         reported it everywhere would be indistinguishable from a working one on Redis 7. \
         Captured: {}",
        common::captured(&log)
    );
    handle.stop().await;
}

/// `RD-SPEC-015` — a `topology` hint skips the `INFO replication` round trip.
///
/// Both `RedisClusterConfig::topology` and `PreflightRequest::topology_hint`
/// document the skip, and DESIGN.md §3.4 says the hint "replaces detection". The
/// operator-facing reason it has to be true is the locked-down managed instance:
/// setting `topology: standalone` there is precisely a way of saying *do not ask
/// this server, it will refuse* — and a preflight that asks anyway logs
/// `cluster.provider.topology_unknown` announcing a conservative
/// `EventuallyConsistent` declaration that `resolve_topology` never makes.
///
/// **Asserted as a call-count delta rather than as an absent WARN.** No fixture
/// refuses `INFO` — `start_redis_config_denied_with` denies `CONFIG` — so the
/// WARN has no trigger to withhold. The delta is the stronger assertion anyway:
/// the WARN is unreachable once the command is not issued.
///
/// Both startups run against one container so the two counts are comparable, and
/// each is measured as a delta from a baseline taken immediately before it.
/// `command_calls` reports `cmdstat_info`, which counts `INFO server`,
/// `INFO replication` **and the helper's own `INFO commandstats`** — so only the
/// difference between the two deltas is meaningful, and it must be exactly one.
#[tokio::test]
async fn rd_spec_015_a_topology_hint_skips_the_info_replication_round_trip() {
    let (_container, config) = common::start_redis().await;
    let url = config.url.clone();
    let raw = common::raw_client(&url).await;

    // Unhinted: detection runs.
    let before = common::command_calls(&raw, "info").await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against a stock container");
    handle.stop().await;
    let unhinted = common::command_calls(&raw, "info").await - before;

    // Hinted: the same startup with the answer already supplied.
    let hinted_config = common::cluster_config_json(&url, json!({ "topology": "standalone" }));
    let before = common::command_calls(&raw, "info").await;
    let handle = RedisClusterPlugin::builder(hinted_config)
        .build_and_start()
        .await
        .expect("the plugin starts with a topology hint");
    assert_eq!(
        handle.cache().consistency(),
        CacheConsistency::EventuallyConsistent,
        "a standalone hint on a non-durable server still declares the weak consistency - this \
         scenario is about the round trip, not about the declaration (DESIGN.md sec 3.6)"
    );
    handle.stop().await;
    let hinted = common::command_calls(&raw, "info").await - before;

    assert_eq!(
        // Widened rather than subtracted in `u64`: a flipped delta must fail as
        // this assertion, with both counts in the message, and not as an
        // overflow panic that hides them.
        i128::from(unhinted) - i128::from(hinted),
        1,
        "a topology hint must skip exactly the INFO replication round trip. Both field docs \
         promise it and DESIGN.md sec 3.4 says the hint replaces detection, so issuing it anyway \
         is a wasted command on every hinted deployment and a false \
         cluster.provider.topology_unknown wherever the server refuses it \
         (unhinted={unhinted}, hinted={hinted})"
    );
}

/// `RD-SPEC-013` — throughput smoke against the OAGW envelope.
///
/// **Measured and printed as an artefact, never asserted against a threshold.** A
/// CI container's absolute numbers are not a production predictor, and per
/// `docs/PRD.md` §6.2 quantitative per-backend SLOs are explicitly excluded from
/// the cluster-wide NFR set. What the artefact makes visible is an
/// order-of-magnitude regression, which is the actionable part.
///
/// Recorded because `cpt-cf-clst-actor-oagw`'s 10 000+ counter updates per second
/// is the reason this plugin exists at all: ADR-001 puts Redis 10–100× above every
/// other backend on cache and lock throughput, and `compare_and_swap` on a small
/// key set is the exact shape of an OAGW counter update.
///
/// Read it with `-- --nocapture`.
#[tokio::test]
async fn rd_spec_013_throughput_smoke_against_the_oagw_envelope() {
    const OPERATIONS: u32 = 10_000;
    const KEYS: u32 = 16;

    let (_container, config) = common::start_redis_with(json!({ "pool_size": 8 })).await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts");
    let cache: Arc<dyn ClusterCacheBackend> = handle.cache();

    for index in 0..KEYS {
        cache
            .put(PutRequest {
                key: &format!("bench:{index}"),
                value: b"0",
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("seed put succeeds");
    }

    let started = Instant::now();
    let mut done = 0_u32;
    let mut conflicts = 0_u32;
    for round in 0..OPERATIONS {
        let key = format!("bench:{}", round % KEYS);
        let Some(current) = cache.get(&key).await.expect("get succeeds") else {
            panic!("a seeded key must be present");
        };
        match cache
            .compare_and_swap(&key, current.version, b"1", Ttl::Indefinite)
            .await
        {
            Ok(_entry) => done += 1,
            Err(ClusterError::CasConflict { .. }) => conflicts += 1,
            Err(other) => panic!("an unexpected failure during the smoke run: {other:?}"),
        }
    }
    let elapsed = started.elapsed();

    #[allow(
        clippy::cast_precision_loss,
        reason = "an operations-per-second figure printed for a human to eyeball; the precision \
                  lost at these magnitudes is far below the run-to-run variance it is read against"
    )]
    let per_second = f64::from(done) / elapsed.as_secs_f64();
    // A millisecond count rather than `{elapsed:?}`: clippy's `use_debug` is
    // denied workspace-wide, and a plain integer reads better in an artefact line
    // than `Duration`'s own representation anyway.
    let elapsed_ms = elapsed.as_millis();
    println!(
        "RD-SPEC-013 artefact: {done} compare_and_swap ops ({conflicts} conflicts) over {KEYS} \
         keys in {elapsed_ms} ms = {per_second:.0} ops/sec. Not a threshold - a CI container's \
         absolute numbers are not a production predictor (TESTING.md sec 8). The reference point \
         is cpt-cf-clst-actor-oagw's 10 000+ counter updates/sec, and what this makes visible is \
         an order-of-magnitude regression."
    );
    assert_eq!(
        done + conflicts,
        OPERATIONS,
        "every operation must have completed one way or the other - a smoke run that silently did \
         less work would report a flattering number"
    );

    handle.stop().await;
}

/// `RD-SPEC-002` — a replicated topology declares `EventuallyConsistent` **even
/// with `appendfsync always`**.
///
/// The most consequential thing this plugin computes, and the one row of
/// DESIGN.md §3.6 that decides whether a Redis-backed profile starts at all. Every
/// other fixture makes the declaration easy: a stock container is weak on
/// durability *and* topology, and the durable single node is safe on both. This
/// one puts them in conflict — the strongest possible durability setting on a
/// server that also has a replica — so a plugin that read `appendfsync` and
/// stopped would declare `Linearizable` here and be wrong.
///
/// Async replication is the binding weakness (ADR-009): an acknowledged write can
/// be lost to a failover no matter how faithfully it was fsynced first. The test
/// asserts the premise as well as the conclusion, reading `appendfsync` back off
/// the primary — without that, a fixture that quietly failed to enable AOF would
/// let this pass for the wrong reason.
#[tokio::test]
async fn rd_spec_002_a_replicated_topology_declares_eventually_consistent() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (container, config, primary, _replica) = common::start_redis_sentinel().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against a sentinel-managed primary");

    // The premise: durability really is at its strongest here.
    let appendfsync = common::exec_in(
        &container,
        &[
            "redis-cli",
            "-p",
            &primary.to_string(),
            "config",
            "get",
            "appendfsync",
        ],
    )
    .await;
    assert!(
        appendfsync.contains("always"),
        "the fixture's primary must run appendfsync always, or this scenario is not the conflict \
         it exists to resolve. Got {appendfsync:?}"
    );
    let replication = common::exec_in(
        &container,
        &[
            "redis-cli",
            "-p",
            &primary.to_string(),
            "info",
            "replication",
        ],
    )
    .await;
    assert!(
        replication.contains("state=online"),
        "and it must really have a replica attached. Got {replication:?}"
    );

    assert_eq!(
        handle.cache().consistency(),
        CacheConsistency::EventuallyConsistent,
        "a replicated topology is EventuallyConsistent whatever its durability: async replication \
         can lose an acknowledged write to a failover, and no amount of fsync changes that \
         (ADR-009, DESIGN.md sec 3.6)"
    );
    assert!(
        !handle.lock().features().linearizable,
        "and the lock declares the same, from the same preflight - a lock claiming linearizable \
         here would let a consumer require the capability and be told a failover-losable write \
         satisfies it"
    );
    assert!(
        common::count_occurrences(&log, logs::WEAK_CONSISTENCY) >= 1,
        "the weak-consistency WARN must fire on a replicated server. Captured: {}",
        common::captured(&log)
    );

    handle.stop().await;
}

/// `RD-SPEC-008` — cluster mode routes every operation, with zero `CROSSSLOT`.
///
/// The single-key-script invariant (DESIGN.md §6) is held *statically* by
/// `scripts.rs`'s source-derived `KEYS[n]` assertion and by `evalsha`'s one-key
/// signature. What no single-node test can show is that Redis routes what those
/// produce: `CROSSSLOT` is an error only a clustered server raises, so on one node
/// the invariant is unfalsifiable.
///
/// Every mutation here is a Lua script, and a script whose keys hash to different
/// slots is rejected outright — so "all of these succeeded" is the assertion. The
/// spread is asserted too: keys landing on one shard would exercise no routing at
/// all and pass anyway.
#[tokio::test]
async fn rd_spec_008_cluster_mode_routes_every_operation() {
    let (container, config, ports) = common::start_redis_cluster().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against a 3-primary cluster");
    let cache = handle.cache();

    for index in 0..300 {
        cache
            .put(PutRequest {
                key: &format!("routed/{index}"),
                value: VALUE,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("every put routes to its own slot - a CROSSSLOT would surface here");
    }

    let counts = common::keys_per_node(&container, &ports).await;
    let populated = counts.iter().filter(|count| **count > 0).count();
    assert!(
        populated >= 2,
        "the keys must land on more than one shard, or nothing about routing is being tested. \
         Per-node counts: {counts:?}"
    );

    // The rest of the surface, once each: every one is a script dispatched with
    // exactly one key, and each would fail with CROSSSLOT if that ever stopped
    // being true.
    let entry = cache
        .get("routed/1")
        .await
        .expect("get routes")
        .expect("the entry is present");
    cache
        .compare_and_swap("routed/1", entry.version, VALUE, Ttl::Indefinite)
        .await
        .expect("compare_and_swap routes");
    cache
        .put_if_absent(PutRequest {
            key: "routed/fresh",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_secs(30)),
        })
        .await
        .expect("put_if_absent routes");
    assert!(
        cache.contains("routed/1").await.expect("contains routes"),
        "contains routes and finds the entry"
    );
    cache.delete("routed/2").await.expect("delete routes");

    let guard = handle
        .lock()
        .try_lock("routed-lock", Duration::from_secs(30))
        .await
        .expect("the lock's SET NX routes to its own slot");
    guard.release().await.expect("the release script routes");

    handle.stop().await;
}

/// `RD-SPEC-009` — `scan_prefix` in cluster mode covers **every** shard.
///
/// `cache/scan.rs` branches on `clustered` and takes `scan_cluster_buffered`,
/// which iterates each primary in turn rather than issuing one `SCAN`. On a single
/// node that branch is dead code: a per-shard scan that silently returned only the
/// keys of whichever primary the client happened to reach would pass every other
/// scenario in this suite, and would break service discovery in production, which
/// is built on `scan_prefix` (DESIGN.md §4.4).
///
/// The spread assertion is what makes this a cluster test rather than a repeat of
/// `RD-CACHE-*`'s: if every key landed on one shard, a single-shard scan would
/// return all of them and the bug would go unseen.
#[tokio::test]
async fn rd_spec_009_scan_prefix_covers_every_shard() {
    /// Enough keys that the default hash-slot distribution reaches every shard
    /// with overwhelming likelihood, while staying well inside a per-PR budget.
    const PLANTED: usize = 300;

    let (container, config, ports) = common::start_redis_cluster().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against a 3-primary cluster");
    let cache = handle.cache();

    for index in 0..PLANTED {
        cache
            .put(PutRequest {
                key: &format!("sd/instance-{index}"),
                value: VALUE,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");
    }

    let counts = common::keys_per_node(&container, &ports).await;
    let populated = counts.iter().filter(|count| **count > 0).count();
    assert!(
        populated >= 2,
        "the planted keys must span shards, or a single-shard scan would pass this. Per-node \
         counts: {counts:?}"
    );

    let mut found = cache
        .scan_prefix("sd/")
        .await
        .expect("scan_prefix succeeds in cluster mode");
    found.sort();
    found.dedup();
    assert_eq!(
        found.len(),
        PLANTED,
        "one scan_prefix must return every key under the prefix, from every shard. Got \
         {} of {PLANTED}, with per-node counts {counts:?} - a short count here is the per-shard \
         iteration returning only the primary the client reached",
        found.len()
    );

    handle.stop().await;
}

/// `RD-SPEC-010` — cluster mode declares `prefix_watch: false`, **the gate
/// DESIGN.md §13 D2 designates**.
///
/// Deliberately an assertion of a *current limitation* rather than of a feature.
/// A prefix watch is served by one `PSUBSCRIBE` on the plugin's own event
/// channels, which is broadcast cluster-wide and works — but `expired` and
/// `evicted` arrive as **keyspace notifications, which are node-local**, so a
/// prefix watcher in cluster mode would silently miss every expiry outside the
/// shard its subscriber happens to be attached to. Declaring `false` sends the SDK
/// to `PollingPrefixWatch`, which is slower and correct.
///
/// The per-key watch is asserted to still work in the same breath, because that is
/// the half that does not depend on keyspace notifications for its ordinary event:
/// a `put` publishes, and a cluster-wide `PUBLISH` reaches the subscriber whichever
/// node it landed on.
///
/// **The follow-up that implements per-shard expiry subscriptions replaces this
/// test with its positive counterpart.** Until then nothing may declare `true`,
/// and this is what says so mechanically rather than in review.
#[tokio::test]
async fn rd_spec_010_cluster_mode_declares_no_prefix_watch() {
    let (_container, config, _ports) = common::start_redis_cluster().await;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against a 3-primary cluster");
    let cache = handle.cache();

    assert!(
        !cache.features().prefix_watch,
        "cluster mode must declare no native prefix watch: keyspace notifications are node-local, \
         so a prefix watcher would miss every expiry outside its subscriber's shard. The SDK's \
         PollingPrefixWatch is the honest fallback (DESIGN.md sec 4.3, sec 13 D2)"
    );
    assert!(
        matches!(
            cache.watch_prefix("sd/").await,
            Err(ClusterError::Unsupported {
                feature: "prefix_watch"
            })
        ),
        "and watch_prefix must answer Unsupported rather than opening a stream that drops events"
    );

    // The per-key watch still works: its event is a plugin `PUBLISH`, which is
    // broadcast cluster-wide rather than confined to one node.
    let mut watch = cache
        .watch("watched/key")
        .await
        .expect("a per-key watch is supported in cluster mode");
    cache
        .put(PutRequest {
            key: "watched/key",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match watch.recv().await {
                Some(CacheWatchEvent::Event(event)) => return Some(event),
                Some(_other) => {}
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
    .expect("the write must reach the watcher - PUBLISH is cluster-wide");
    assert!(
        matches!(&event, CacheEvent::Changed { key } if key == "watched/key"),
        "got {event:?}"
    );

    handle.stop().await;
}
