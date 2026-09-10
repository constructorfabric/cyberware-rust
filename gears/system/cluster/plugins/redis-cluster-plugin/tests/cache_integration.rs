//! Layer 3 — cache integration scenarios (docs/TESTING.md §4.2), `RD-CACHE-001`
//! through `RD-CACHE-010`.
//!
//! These mirror the Layer 2 conformance scenarios but assert on the **actual
//! Redis keyspace** rather than only through the backend trait. That is the whole
//! reason they exist alongside L2: conformance pins the contract, and these pin
//! the encoding and the command choices the contract does not constrain. A future
//! re-encoding of a cache entry, or a `scan_prefix` quietly reaching for `KEYS`,
//! passes every conformance scenario and fails here.
//!
//! Each scenario starts its own container. That is more Docker than sharing one,
//! and it buys the thing sharing cannot: no scenario can be affected by a key,
//! a subscription, or a server setting another left behind — and several of these
//! read server-wide counters (`INFO commandstats`) where another scenario's
//! traffic would be indistinguishable from this one's.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use cluster_sdk::cache::{PutRequest, Ttl};
use cluster_sdk::{ClusterCacheBackend, ClusterError};
use fred::interfaces::{HashesInterface, KeysInterface};
use redis_cluster_plugin::{RedisClusterHandle, RedisClusterPlugin};
use serde_json::json;

/// The value every scenario writes when the bytes themselves do not matter.
const VALUE: &[u8] = b"v1";

/// Starts a plugin over a stock container and hands back the pieces every
/// scenario needs: the container (which must outlive the test), the plugin
/// handle, the cache, and a raw client for keyspace assertions.
///
/// The handle is returned rather than the cache alone because **every scenario
/// must `stop()` it**: `RedisClusterHandle` panics on drop without `stop()` in a
/// debug build (ADR-006), which is what `cargo test` produces.
async fn fixture(
    overrides: serde_json::Value,
) -> (
    testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    RedisClusterHandle,
    Arc<dyn ClusterCacheBackend>,
    fred::clients::Client,
    String,
) {
    let (container, config) = common::start_redis_with(overrides).await;
    let url = config.url.clone();
    let key_prefix = config.key_prefix.clone();
    let database = config.database;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against the test container");
    let cache = handle.cache();
    let raw = common::raw_client_on(&url, database).await;
    (container, handle, cache, raw, key_prefix)
}

/// The Redis key the plugin stores `key` under — `<prefix>:c:<key>` (DESIGN.md
/// §2.1).
///
/// Spelled out here rather than taken from the plugin so the test states the wire
/// format independently: reading it back through `RedisCache::entry_key` would
/// make `RD-CACHE-001` agree with whatever the plugin currently does, which is
/// the opposite of what a wire-format assertion is for.
fn entry_key(prefix: &str, key: &str) -> String {
    format!("{prefix}:c:{key}")
}

/// `RD-CACHE-001` — a `put`/`get` round-trip, and the stored key really is a hash
/// with exactly the fields `v` and `ver`.
///
/// The keyspace half is the point. DESIGN.md §2.2 specifies a two-field hash, and
/// every mutation script writes both fields together; asserting the shape *at the
/// server* means a future re-encoding (a serialized struct in a string key, a
/// third field) is a test failure rather than a silent wire change that only
/// breaks a mixed-version fleet.
#[tokio::test]
async fn rd_cache_001_put_get_round_trip_stores_a_two_field_hash() {
    let (_container, handle, cache, raw, prefix) = fixture(json!({})).await;

    cache
        .put(PutRequest {
            key: "alpha",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");
    let read = cache
        .get("alpha")
        .await
        .expect("get succeeds")
        .expect("the entry just written is present");
    assert_eq!(read.value, VALUE, "the value must round-trip unchanged");
    assert_eq!(
        read.version, 1,
        "the first write to a fresh key is version 1"
    );

    let stored: HashMap<String, String> = raw
        .hgetall(entry_key(&prefix, "alpha"))
        .await
        .expect("HGETALL on the entry key succeeds");
    let mut fields: Vec<&str> = stored.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        vec!["v", "ver"],
        "DESIGN.md sec 2.2's encoding is a hash with exactly `v` and `ver`; found {fields:?}"
    );
    assert_eq!(stored.get("v").map(String::as_str), Some("v1"));

    handle.stop().await;
}

/// `RD-CACHE-002` — versions increment by exactly 1 per write, `put_if_absent`
/// creates at 1, and the server agrees with what the plugin reported.
///
/// Reading `ver` straight out of Redis is what makes this more than a restatement
/// of the conformance scenario: a plugin that kept its own counter and never wrote
/// it, or wrote it as a different type, would satisfy the trait and fail here.
#[tokio::test]
async fn rd_cache_002_versions_increment_by_one_and_the_server_agrees() {
    let (_container, handle, cache, raw, prefix) = fixture(json!({})).await;

    let created = cache
        .put_if_absent(PutRequest {
            key: "beta",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put_if_absent succeeds")
        .expect("an absent key is created");
    assert_eq!(created.version, 1, "a created entry starts at version 1");

    let mut previous = created.version;
    for expected in 2..=5_u64 {
        cache
            .put(PutRequest {
                key: "beta",
                value: VALUE,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");
        let entry = cache
            .get("beta")
            .await
            .expect("get succeeds")
            .expect("the entry just written is present");
        assert_eq!(
            entry.version,
            previous + 1,
            "each put must increment the version by exactly 1, not by an arbitrary amount"
        );
        assert_eq!(entry.version, expected);
        previous = entry.version;
    }

    let server_version: String = raw
        .hget(entry_key(&prefix, "beta"), "ver")
        .await
        .expect("HGET ver succeeds");
    assert_eq!(
        server_version,
        previous.to_string(),
        "the version the plugin reported must be the one stored in the hash"
    );

    handle.stop().await;
}

/// `RD-CACHE-003` — 20 concurrent writers CAS the same key from the same expected
/// version: exactly one wins and 19 get `CasConflict`, each carrying a populated
/// `current`.
///
/// The conflict payload is the half worth holding: DESIGN.md §4.1 has the CAS
/// script return the current entry on mismatch, so a loser learns the value it
/// lost to in the *same round trip* rather than needing a follow-up `get` that
/// could itself be stale. A plugin that reported a bare conflict would pass the
/// contract and cost every retrying caller an extra read.
#[tokio::test]
async fn rd_cache_003_concurrent_cas_has_exactly_one_winner() {
    let (_container, handle, cache, _raw, _prefix) = fixture(json!({ "pool_size": 8 })).await;

    cache
        .put(PutRequest {
            key: "gamma",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the seed put succeeds");
    let base = cache
        .get("gamma")
        .await
        .expect("get succeeds")
        .expect("the seed entry is present");

    let mut tasks = Vec::new();
    for contender in 0..20_u32 {
        let cache = Arc::clone(&cache);
        let expected = base.version;
        tasks.push(tokio::spawn(async move {
            cache
                .compare_and_swap(
                    "gamma",
                    expected,
                    format!("writer-{contender}").as_bytes(),
                    Ttl::Indefinite,
                )
                .await
        }));
    }

    let mut winners = 0_u32;
    let mut conflicts = 0_u32;
    for task in tasks {
        match task.await.expect("no CAS task panics") {
            Ok(entry) => {
                winners += 1;
                assert_eq!(
                    entry.version,
                    base.version + 1,
                    "the winning CAS advances the version by one"
                );
            }
            Err(ClusterError::CasConflict { current, .. }) => {
                conflicts += 1;
                let current = current.expect(
                    "DESIGN.md sec 4.1 has the CAS script return the current entry on mismatch, so \
                     a loser must not have to issue a second read to learn what it lost to",
                );
                assert_eq!(
                    current.version,
                    base.version + 1,
                    "the conflict payload must carry the version that beat this writer"
                );
            }
            Err(other) => panic!("a losing CAS must be CasConflict, got {other:?}"),
        }
    }
    assert_eq!(
        winners, 1,
        "exactly one of 20 concurrent CAS writers may win"
    );
    assert_eq!(conflicts, 19, "the other 19 must all report CasConflict");

    handle.stop().await;
}

/// `RD-CACHE-004` — a TTL'd entry disappears on its own, with **no reaper running
/// anywhere**, and `PTTL` shows the TTL came from the request.
///
/// The sharpest statement of what native expiry buys (DESIGN.md §4.2): the
/// Postgres plugin needs a sweeper task per cache and a scenario like this has to
/// wait for a sweep tick. Here there is nothing in the plugin to be stalled,
/// because Redis is the only expiry mechanism. A regression that reintroduced a
/// reaper would still pass — but one that stopped setting `PX`, and leaned on a
/// reaper that does not exist, fails.
#[tokio::test]
async fn rd_cache_004_native_ttl_expires_with_no_reaper() {
    let (_container, handle, cache, raw, prefix) = fixture(json!({})).await;

    cache
        .put(PutRequest {
            key: "delta",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_millis(500)),
        })
        .await
        .expect("put with a ttl succeeds");

    // The TTL is the request's, not something inherited from a previous write or
    // a server default.
    let pttl: i64 = raw
        .pttl(entry_key(&prefix, "delta"))
        .await
        .expect("PTTL succeeds");
    assert!(
        (1..=500).contains(&pttl),
        "PTTL must reflect the 500 ms the caller asked for, got {pttl}"
    );

    let gone = common::wait_until(
        Duration::from_secs(3),
        Duration::from_millis(50),
        async || matches!(cache.get("delta").await, Ok(None)),
    )
    .await;
    assert!(
        gone,
        "a lapsed entry must be absent from get with no reaper"
    );
    assert!(
        !cache.contains("delta").await.expect("contains succeeds"),
        "contains must agree with get about a lapsed entry"
    );

    handle.stop().await;
}

/// `RD-CACHE-005` — `compare_and_delete` survives a version reset.
///
/// The regression this guards is subtle and is recorded in memory as
/// `cluster-cache-version-reset-caveat`: deleting and recreating a key resets
/// `ver` to 1, so a version is **not** monotonic across re-creation the way a
/// Kubernetes `resourceVersion` is. Any release-if-still-mine primitive built on
/// versions would therefore let a stale holder delete a new holder's claim. The
/// SDK's answer is that `compare_and_delete` is *value*-guarded (DESIGN.md §2.3),
/// and this is the scenario that holds it.
#[tokio::test]
async fn rd_cache_005_compare_and_delete_survives_a_version_reset() {
    let (_container, handle, cache, _raw, _prefix) = fixture(json!({})).await;

    cache
        .put(PutRequest {
            key: "epsilon",
            value: b"first-holder",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the first holder writes its claim");
    assert!(
        cache.delete("epsilon").await.expect("delete succeeds"),
        "the key existed, so delete reports true"
    );
    cache
        .put(PutRequest {
            key: "epsilon",
            value: b"second-holder",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the second holder writes its claim");
    let recreated = cache
        .get("epsilon")
        .await
        .expect("get succeeds")
        .expect("the second holder's entry is present");
    assert_eq!(
        recreated.version, 1,
        "delete-and-recreate resets the version to 1 - this is the caveat the value guard exists \
         for, so if it ever stops being true this scenario should be revisited rather than fixed"
    );

    let deleted = cache
        .compare_and_delete("epsilon", b"first-holder")
        .await
        .expect("compare_and_delete succeeds");
    assert!(
        !deleted,
        "the first holder's stale value must not match, so nothing is deleted"
    );
    let survivor = cache
        .get("epsilon")
        .await
        .expect("get succeeds")
        .expect("the second holder's claim must still be present");
    assert_eq!(
        survivor.value, b"second-holder",
        "the new holder's claim must be intact after a stale compare_and_delete"
    );

    handle.stop().await;
}

/// `RD-CACHE-006` — `scan_prefix` returns consumer-facing keys, excludes lapsed
/// ones and foreign prefixes, and **never issues `KEYS`**.
///
/// Three properties in one scenario because they share a keyspace setup, and the
/// third is the one that matters operationally: `KEYS` on a shared production
/// Redis is O(N) over the whole keyspace and blocks the single-threaded server for
/// the duration, so it is an outage rather than a slow query (DESIGN.md §4.4).
/// `INFO commandstats` is the mechanical check — the same rule is enforced
/// statically in `src/static_analysis_tests.rs`, and both are worth having: the
/// static one covers code the tests never reach, this one covers a `KEYS` reaching
/// the server from anywhere at all, including from inside a Lua script.
///
/// The foreign-prefix half is why TESTING §3 calls prefix isolation the *fallback*
/// for conformance: a prefix bug is exactly what would let one deployment read
/// another's keyspace, so the scenario that looks for it must not itself depend on
/// prefixes being right.
#[tokio::test]
async fn rd_cache_006_scan_prefix_is_isolated_and_never_issues_keys() {
    let (_container, handle, cache, raw, _prefix) = fixture(json!({})).await;
    let baseline_keys = common::command_calls(&raw, "keys").await;

    for key in ["reports:a", "reports:b", "reports:c"] {
        cache
            .put(PutRequest {
                key,
                value: VALUE,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");
    }
    cache
        .put(PutRequest {
            key: "reports:doomed",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_millis(300)),
        })
        .await
        .expect("put with a short ttl succeeds");
    cache
        .put(PutRequest {
            key: "invoices:a",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put outside the scanned prefix succeeds");

    // A second plugin instance on the same database under a *different*
    // `key_prefix` — the isolation half. Its keys share the scanned consumer
    // prefix and must still be invisible, because the plugin prefix is what
    // separates two deployments sharing one Redis.
    let foreign_config =
        common::cluster_config_json(&raw_url(&raw), json!({ "key_prefix": "otherdeployment" }));
    let foreign = RedisClusterPlugin::builder(foreign_config)
        .build_and_start()
        .await
        .expect("a second instance under a different key_prefix starts");
    foreign
        .cache()
        .put(PutRequest {
            key: "reports:foreign",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the foreign instance writes under its own prefix");

    // Waited on through `scan_prefix` itself rather than through `get`, and with a
    // generous budget. Two reasons, and the first is a correctness one: Redis
    // reaps a lapsed key either lazily on access or on its background cycle, so a
    // `get` that has returned `None` is not by itself proof that a `SCAN` no
    // longer lists it — waiting on one observation while asserting on another
    // leaves exactly that window open. The second is load: this is a precondition
    // rather than the property under test, and starving it on a busy host should
    // slow the scenario down, not fail it. (Observed once as a flake in a full
    // parallel run at a 3 s budget, while passing 6/6 in isolation.)
    let lapsed = common::wait_until(
        Duration::from_secs(15),
        Duration::from_millis(50),
        async || {
            cache
                .scan_prefix("reports:")
                .await
                .is_ok_and(|keys| !keys.iter().any(|key| key == "reports:doomed"))
        },
    )
    .await;
    assert!(lapsed, "the short-ttl key must lapse before the scan");

    let mut found = cache
        .scan_prefix("reports:")
        .await
        .expect("scan_prefix succeeds");
    found.sort();
    assert_eq!(
        found,
        vec![
            "reports:a".to_owned(),
            "reports:b".to_owned(),
            "reports:c".to_owned()
        ],
        "scan_prefix must return consumer keys with the plugin's own `<prefix>:c:` stripped, \
         excluding the lapsed key, the key outside the scanned prefix, and the other deployment's \
         key entirely"
    );

    assert_eq!(
        common::command_calls(&raw, "keys").await,
        baseline_keys,
        "scan_prefix must iterate with a SCAN cursor and never issue KEYS, which on a shared \
         production Redis is an outage rather than a slow query (DESIGN.md sec 4.4)"
    );
    let scans = common::command_calls(&raw, "scan").await;
    assert!(
        scans > 0,
        "and it must actually have scanned - a scan_prefix that issued neither KEYS nor SCAN would \
         satisfy the assertion above vacuously"
    );

    foreign.stop().await;
    handle.stop().await;
}

/// `RD-CACHE-007` — `Ttl::Indefinite` **clears** an existing TTL rather than
/// preserving it.
///
/// The SDK's `Ttl` is two-valued with no "leave it alone" variant, so a write
/// always states the TTL. That makes this a contract property rather than a
/// preference: a plugin whose `put` script omitted the `PERSIST` branch would keep
/// the old deadline and silently expire an entry the caller asked to keep
/// forever — and `get` would agree with the caller right up until it didn't.
#[tokio::test]
async fn rd_cache_007_indefinite_clears_an_existing_ttl() {
    let (_container, handle, cache, raw, prefix) = fixture(json!({})).await;

    cache
        .put(PutRequest {
            key: "zeta",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_secs(30)),
        })
        .await
        .expect("put with a ttl succeeds");
    let with_ttl: i64 = raw
        .pttl(entry_key(&prefix, "zeta"))
        .await
        .expect("PTTL succeeds");
    assert!(
        with_ttl > 0,
        "the first write sets a deadline, got {with_ttl}"
    );

    cache
        .put(PutRequest {
            key: "zeta",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put with Indefinite succeeds");
    let persisted: i64 = raw
        .pttl(entry_key(&prefix, "zeta"))
        .await
        .expect("PTTL succeeds");
    assert_eq!(
        persisted, -1,
        "PTTL must report -1 (persistent), not the old deadline: `Ttl::Indefinite` states a TTL \
         rather than declining to"
    );

    handle.stop().await;
}

/// `RD-CACHE-008` — `put_if_absent` on a live entry does not overwrite it.
///
/// The contract leader election's `claim` rests on: a candidate that lost the race
/// must not clobber the winner's key. Asserting the stored value *and* version are
/// untouched is what distinguishes "returned None" from "returned None and wrote
/// anyway", which are indistinguishable from the return value alone.
#[tokio::test]
async fn rd_cache_008_put_if_absent_does_not_overwrite_a_live_entry() {
    let (_container, handle, cache, _raw, _prefix) = fixture(json!({})).await;

    let winner = cache
        .put_if_absent(PutRequest {
            key: "eta",
            value: b"winner",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the first put_if_absent succeeds")
        .expect("an absent key is created");

    let loser = cache
        .put_if_absent(PutRequest {
            key: "eta",
            value: b"loser",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("the second put_if_absent succeeds as an operation");
    assert!(
        loser.is_none(),
        "put_if_absent on a live entry reports None rather than creating"
    );

    let stored = cache
        .get("eta")
        .await
        .expect("get succeeds")
        .expect("the winner's entry is present");
    assert_eq!(
        stored.value, b"winner",
        "the loser must not have overwritten the winner's value"
    );
    assert_eq!(
        stored.version, winner.version,
        "and must not have bumped the version either - a version change would tell every CAS \
         holder their claim was stale when it was not"
    );

    handle.stop().await;
}

/// `RD-CACHE-009` — a command against an unresponsive server is bounded
/// client-side.
///
/// The container is **paused**, not stopped: the socket stays open and nothing
/// answers, which is the failure mode a client-side timeout exists for — a closed
/// socket would fail fast on its own and prove nothing. DESIGN.md §12 records the
/// bound as a property of every command rather than of any one, since `fred`
/// enforces `default_command_timeout` on all of them, and it is what makes
/// `stop()` finite (`RD-LIFE-009`).
#[tokio::test]
async fn rd_cache_009_a_command_is_bounded_client_side() {
    let (container, handle, cache, _raw, _prefix) =
        fixture(json!({ "command_timeout_ms": 200 })).await;

    cache
        .put(PutRequest {
            key: "theta",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("a put against a live server succeeds");

    container.pause().await.expect("the container pauses");
    let started = std::time::Instant::now();
    let result = cache.get("theta").await;
    let elapsed = started.elapsed();
    // Unpause before asserting: a panic here would otherwise leave a paused
    // container for `stop()` to fight, and `stop()` runs on the unwind path.
    container.unpause().await.expect("the container unpauses");

    assert!(
        matches!(
            result,
            Err(ClusterError::Provider {
                kind: cluster_sdk::ProviderErrorKind::Timeout,
                ..
            })
        ),
        "a command against a paused server must surface as Provider {{ Timeout }}, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the 200 ms client-side bound must hold - a caller cannot be left waiting on a server that \
         is not answering. Took {elapsed:?}"
    );

    handle.stop().await;
}

/// `RD-CACHE-010` — every script is dispatched with exactly one key at runtime.
///
/// The runtime half of DESIGN.md §6's Cluster-correctness invariant, whose
/// build-time half is `scripts.rs`'s `every_catalogued_script_declares_exactly_one_key`.
/// Both are needed and neither is redundant: the structural check reads the Lua
/// source and would miss a call site passing two keys to a one-key script, and
/// this one exercises the call sites but only over the scripts a scenario happens
/// to reach.
///
/// `CROSSSLOT` is the failure being made unreachable. A multi-key script whose
/// keys hash to different slots is rejected by Redis Cluster at runtime, so a
/// violation would surface only on a clustered deployment — which has no fixture, and
/// which is exactly why the invariant is worth holding on a single node where it
/// *cannot* fail on its own.
#[tokio::test]
async fn rd_cache_010_every_script_is_dispatched_with_one_key() {
    let (_container, handle, cache, raw, _prefix) = fixture(json!({})).await;
    let baseline_errors = common::command_calls(&raw, "eval").await;

    // Drive all five cache scripts: put, put_if_absent, cas, compare_and_delete,
    // delete. A `CROSSSLOT` or a wrong-arity dispatch would fail the operation
    // outright rather than merely mis-route it, so "every one of these succeeded"
    // is the assertion.
    cache
        .put_if_absent(PutRequest {
            key: "iota",
            value: b"one",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put_if_absent dispatches with one key")
        .expect("created");
    cache
        .put(PutRequest {
            key: "iota",
            value: b"two",
            ttl: Ttl::Of(Duration::from_secs(30)),
        })
        .await
        .expect("put dispatches with one key");
    let written = cache
        .get("iota")
        .await
        .expect("get succeeds")
        .expect("the entry just written is present");
    let swapped = cache
        .compare_and_swap("iota", written.version, b"three", Ttl::Indefinite)
        .await
        .expect("compare_and_swap dispatches with one key");
    assert!(
        !cache
            .compare_and_delete("iota", b"not-the-value")
            .await
            .expect("compare_and_delete dispatches with one key"),
        "a mismatched compare_and_delete is a no-op rather than an error"
    );
    assert!(
        cache
            .compare_and_delete("iota", b"three")
            .await
            .expect("compare_and_delete dispatches with one key"),
        "a matching compare_and_delete removes the entry"
    );
    assert_eq!(swapped.value, b"three");

    cache
        .put(PutRequest {
            key: "iota",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");
    assert!(
        cache
            .delete("iota")
            .await
            .expect("delete dispatches with one key"),
        "delete removes the entry"
    );

    // No script needed the `EVAL` fallback: every dispatch found its SHA cached,
    // which is also the evidence that `SCRIPT LOAD` ran at startup as DESIGN.md
    // §3.2 step 3 specifies rather than each call re-sending its source.
    assert_eq!(
        common::command_calls(&raw, "eval").await,
        baseline_errors,
        "every script must dispatch via EVALSHA against the SHA loaded at startup; an EVAL here \
         means a NOSCRIPT recovery fired when nothing had flushed the cache"
    );
    assert!(
        common::command_calls(&raw, "evalsha").await > 0,
        "and EVALSHA must actually have been used - otherwise the assertion above is vacuous"
    );

    handle.stop().await;
}

/// The URL a raw client was built against, recovered from its own config so
/// `RD-CACHE-006` can start a second plugin instance against the same container
/// without the fixture having to thread the URL through every return type.
fn raw_url(client: &fred::clients::Client) -> String {
    use fred::interfaces::ClientLike;
    let server = client
        .client_config()
        .server
        .hosts()
        .first()
        .cloned()
        .expect("a centralized client has exactly one host");
    format!("redis://{}:{}", server.host, server.port)
}
