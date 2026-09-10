# Technical Design — Redis Cluster Plugin

> **Status: implemented.** This document describes the crate as it is built,
> registered, and tested. It is the implementation design for
> `cf-redis-cluster-plugin`, paired with [TESTING.md](./TESTING.md), and it
> originated as the design deliverable of
> [#4373](https://github.com/constructorfabric/gears-rust/issues/4373).
>
> The six questions this design opened are **decided** — §13 records each decision
> and what it commits v1 to. One of them (D1) is a change to `cf-gears-cluster`
> rather than to this crate, delivered alongside the plugin.
>
> What is *not* here: the fault-injection layer.
> [TESTING.md](./TESTING.md) §8 is the register of what that leaves unverified.
> Both multi-node fixtures are built and run on every PR, so the two statements in
> this document that once rested on unit tests are now tested against the topology
> they describe — §3.6's replicated-topology row by `RD-SPEC-002` on Sentinel, and
> §13 D2's cluster-mode declaration by `RD-SPEC-010` on a 3-node Cluster.

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Role in the Cluster Architecture](#11-role-in-the-cluster-architecture)
  - [1.2 Primitive Coverage](#12-primitive-coverage)
  - [1.3 What This Plugin Deliberately Is Not](#13-what-this-plugin-deliberately-is-not)
- [2. Domain Model](#2-domain-model)
  - [2.1 Key Layout](#21-key-layout)
  - [2.2 The Cache Entry Is a Hash, and Why Not a Framed String](#22-the-cache-entry-is-a-hash-and-why-not-a-framed-string)
  - [2.3 Version Semantics](#23-version-semantics)
  - [2.4 The Lock Entry](#24-the-lock-entry)
  - [2.5 Event Channel and Payload Format](#25-event-channel-and-payload-format)
- [3. Component Model](#3-component-model)
  - [3.1 Crate Structure](#31-crate-structure)
  - [3.2 Builder / Handle Lifecycle](#32-builder--handle-lifecycle)
  - [3.3 Connection Model](#33-connection-model)
  - [3.4 Startup Preflight](#34-startup-preflight)
  - [3.5 Standalone Lock Provider](#35-standalone-lock-provider)
  - [3.6 Topology and the Consistency Declaration](#36-topology-and-the-consistency-declaration)
  - [3.7 Eviction Is a Correctness Concern, Not a Capacity One](#37-eviction-is-a-correctness-concern-not-a-capacity-one)
- [4. Cache Implementation](#4-cache-implementation)
  - [4.1 Command Contract per Operation](#41-command-contract-per-operation)
  - [4.2 TTL](#42-ttl)
  - [4.3 Watch](#43-watch)
  - [4.4 scan_prefix](#44-scan_prefix)
  - [4.5 Consistency Declaration](#45-consistency-declaration)
- [5. Distributed Lock Implementation](#5-distributed-lock-implementation)
  - [5.1 The Lock Entry and the Holder Token](#51-the-lock-entry-and-the-holder-token)
  - [5.2 Renew and Release Are Token-Fenced](#52-renew-and-release-are-token-fenced)
  - [5.3 Blocking lock()](#53-blocking-lock)
  - [5.4 No RedLock, No Fencing Tokens](#54-no-redlock-no-fencing-tokens)
  - [5.5 Inspecting Locks (operators)](#55-inspecting-locks-operators)
- [6. Lua Script Catalog](#6-lua-script-catalog)
- [7. Leader Election](#7-leader-election)
- [8. Configuration](#8-configuration)
- [9. Observability](#9-observability)
- [10. ProviderErrorKind Mapping](#10-providererrorkind-mapping)
- [11. Shutdown Sequence](#11-shutdown-sequence)
- [12. Risks / Trade-offs](#12-risks--trade-offs)
- [13. Decisions (formerly Open Questions)](#13-decisions-formerly-open-questions)

<!-- /toc -->

## 1. Overview

`cf-redis-cluster-plugin` is the Redis backend plugin for the cluster gear. It
provides a native `ClusterCacheBackend` over a `fred` connection pool and a native
`DistributedLockBackend` over a `SET NX PX` lease key with Lua-fenced renew and
release. Leader election is derived from the SDK default backend over the Redis
cache — no additional keys or connections are required for that primitive.

The plugin is the recommended cache and lock backend for the **K8s +
high-throughput cache** deployment shape (`docs/DESIGN.md` §4.2), where Redis serves
cache and lock and K8s Lease serves leader election. ADR-001
puts Redis 10–100× above every other backend on cache and lock throughput
(100k–200k ops/sec single node, ~0.15 ms p50), which is what makes the OAGW
requirement of 10 000+ counter updates per second (`cpt-cf-clst-actor-oagw`)
reachable at all.

It is also the backend ADR-009 rates **unsafe for CAS-based leader election in
every replicated configuration**. That tension is the single most important thing
this design has to handle honestly rather than paper over, and §3.6 / §4.5 / §7 are
where it is handled.

### 1.1 Role in the Cluster Architecture

The plugin satisfies `cpt-cf-clst-component-plugins` for the Redis backend. It:

- Implements `ClusterCacheProvider` (the provider trait from `cluster-sdk`) so the
  wiring crate can instantiate the cache from operator YAML
  (`cache: { provider: redis }`).
- Implements `ClusterLockProvider` so the wiring crate can *independently*
  instantiate the native lock (`lock: { provider: redis }`), whether or not `cache`
  in the same profile is bound to redis — see §3.5. This is what makes the native
  `SET NX PX` lock reachable from YAML at all; the wiring's per-primitive routing
  (`cpt-cf-clst-fr-routing-per-primitive`, already implemented in
  `cluster/src/domain/wiring.rs`) dispatches `lock` against its own registry and needs
  something registered there.
- Exposes a builder/handle pair
  (`RedisClusterPlugin::builder(...).build_and_start() -> RedisClusterHandle`)
  following the outbox-style lifecycle pattern (`docs/DESIGN.md` §3.7, ADR-006). It
  is NOT a `RunnableCapability`; the cluster gear (`cf-gears-cluster`) owns its
  lifecycle.
- Returns a `StopHook` from `build_cache` (and, independently, from `build_lock`)
  that closes the relevant connection pool, the subscriber client, and all
  background tasks it owns.

Registration is two lines in the wiring's provider registry (`cluster/src/gear.rs`):

```rust
ProviderRegistry::new()
    // … existing providers …
    .with_cache_provider(Arc::new(redis_cluster_plugin::RedisCacheProvider))
    .with_lock_provider(Arc::new(redis_cluster_plugin::RedisLockProvider))
```

### 1.2 Primitive Coverage

| Primitive | Implementation | Consistency | `*Features` |
|---|---|---|---|
| `ClusterCacheBackend` | Native — hash-per-entry, Lua CAS, native `PX` TTL | `EventuallyConsistent` by default; `Linearizable` only under a verified single-node durable topology (§3.6) | `prefix_watch: true` in `watch_mode: publish` on a non-clustered deployment; `false` otherwise (§4.3) |
| `DistributedLockBackend` | Native — `SET NX PX` lease key, Lua token-fenced renew/release. Independently routable via `lock: { provider: redis }` (§3.5), with its own pool and config | `linearizable` mirrors the cache's declared consistency — `false` by default | — |
| `LeaderElectionBackend` | SDK default `CasBasedLeaderElectionBackend` over the Redis cache — **but the strict constructor rejects an `EventuallyConsistent` cache, so a Redis-only profile that omits `leader_election` fails startup unless the operator opts in explicitly** (§7). The same is true of an omitted `lock`, which auto-wraps the same cache in `CasBasedDistributedLockBackend` and shares the guard | Inherits cache | — |

`prefix_watch: true` (where it holds) means a consumer may declare
`CacheCapability::PrefixWatch` against this backend and gets real reactive prefix
watches. That it is *conditional* on topology and watch mode is the part that must
not be glossed — see §4.3.

### 1.3 What This Plugin Deliberately Is Not

- **Not RedLock.** Multi-master lock acquisition across independent Redis nodes is
  not implemented and will not be. ADR-009 and Kleppmann's analysis are the
  reasons; §5.4 states the position.
- **Not a fencing-token issuer.** ADR-002's "no remote I/O inside the critical
  section" rule removes the failure mode fencing tokens exist for, and explicitly
  spares every plugin the Lua `INCR` counter it would otherwise need.
- **Not a replica-read cache.** All traffic — reads included — is routed to
  primaries. A replica read would let a `get` observe a value older than a
  `compare_and_swap` this same process just committed, which is a stronger
  violation than the topology-level weakness §3.6 already declares. `fred`'s
  replica-routing feature is deliberately left off.
- **Not a durability layer.** Redis persistence configuration belongs to the
  operator. The plugin *reads* it (§3.4) to decide what it may honestly declare,
  and never changes it.

## 2. Domain Model

Redis has no schema, so the "domain model" here is a key layout and a set of value
encodings. There is no migration step and nothing to create at startup — the
first write creates its own key.

### 2.1 Key Layout

Every key this plugin touches is built from an operator-configurable
`key_prefix` (default `cluster`) plus a one-character primitive segment:

| Purpose | Key | Type |
|---|---|---|
| Cache entry | `<prefix>:c:<key>` | hash (`v`, `ver`) |
| Lock lease | `<prefix>:l:<name>` | string (the holder token) |
| Cache event channel | `<prefix>:e:c:<key>` | pub/sub channel (no key exists) |
| Lock-release channel | `<prefix>:e:l:<name>` | pub/sub channel (no key exists) |

`<key>` and `<name>` arrive already scope-prefixed by the SDK's
`ScopedCacheBackend` / `ScopedDistributedLockBackend`
(`cpt-cf-clst-fr-namespacing-scoped`); the plugin adds its own prefix on top and
never inspects the consumer's portion.

**No hash tags.** Keys are deliberately *not* wrapped in `{…}` to co-locate them
in one Redis Cluster slot. Cluster's whole benefit here is that a million cache
keys spread across shards, and every script this plugin runs touches exactly one
key (§6), so there is no `CROSSSLOT` failure to design around. The cost is that
`SCAN` and expiry notifications become per-shard concerns (§4.3, §4.4) — the right
trade, since forcing one slot would cap the deployment at one shard's throughput
and defeat the reason Redis was chosen.

**Key length** is not constrained by the plugin. Redis keys may be up to 512 MB and
are not index entries, so there is no size ceiling a legitimate key — including one
carrying several composed scope prefixes — could realistically exceed.
`InvalidName` is therefore never returned for length; the SDK's own name validation
is the only gate.

### 2.2 The Cache Entry Is a Hash, and Why Not a Framed String

Each cache entry is one Redis hash with exactly two fields:

```
HGETALL cluster:c:shard/t-42
1) "v"     2) "\x00\x01…"        # opaque consumer bytes
3) "ver"   4) "7"                # decimal integer, as a Redis string
```

The obvious alternative — a plain string holding a fixed-width version prefix
followed by the value (`<20 ASCII digits><value>`), which keeps `get` a single
`GET` and needs no hash overhead — was rejected on one specific ground: **Lua
cannot compare or increment a u64 version safely.** Redis's embedded Lua is 5.1,
whose numbers are IEEE doubles, so `tonumber` on a version past 2^53 silently
loses precision and `ver + 1` starts producing duplicate versions. With a hash:

- **Comparison is a string compare.** `redis.call('HGET', key, 'ver')` returns a
  string, `ARGV[1]` is a string, and `==` between them is exact for every u64. No
  number ever enters Lua.
- **Increment is server-side 64-bit.** `HINCRBY` is integer arithmetic in C, exact
  to i64 (§2.3 records the resulting ceiling).

The costs are real and accepted: `get` is `HMGET key v ver` rather than `GET key`
(one round trip either way, marginally larger reply), `put_if_absent` needs Lua
rather than a bare `SET NX` (§4.1), and a two-field hash carries a little more
memory than a string — mitigated by Redis's listpack encoding for small hashes,
which keeps a 2-field hash within tens of bytes of the equivalent string. ADR-001's
~180 MB-per-million-entries envelope still holds.

Every mutation writes both fields inside one Lua script, so no reader ever observes
a value with a stale version or vice versa.

### 2.3 Version Semantics

Version starts at 1 on first insert and increments by 1 on every successful write,
matching the SDK contract (`docs/DESIGN.md` §3.1 `CacheEntry`): version 0 is the
reserved "absent" sentinel, `put_if_absent` returns version 1, each subsequent write
increments by 1. The counter is per key — there is no global sequence.

Two ceilings and one caveat:

- **`HINCRBY` is i64**, so the version wraps out of range at 2^63−1 rather than
  2^64−1. At the OAGW's 10 000 writes/sec against a single key that is ~29 million
  years; recorded for completeness, not mitigated. Exceeding it surfaces as a Redis
  `increment or decrement would overflow` error → `Provider { Other }`.
- **Version resets to 1 on delete-and-recreate**, as the SDK documents for every
  backend (`[cluster-cache-version-reset-caveat]`). This is
  why `compare_and_delete` is **value**-guarded, not version-guarded: a successor
  that re-claimed a key after a TTL lapse wrote a different value, so the guarded
  delete is a safe no-op against it and cannot wipe the successor's claim.
- **Expiry and eviction both destroy the counter**, and eviction can do so without
  any TTL having lapsed. §3.7 is why that is a configuration error rather than a
  fact of life.

### 2.4 The Lock Entry

A lock is one string key whose *value is the holder token* — a per-acquisition
random UUID:

```
GET cluster:l:tenant-42/rate-limit
"9f1c…"          # holder token; PTTL is the remaining lease
```

The key's presence is the lock; its TTL is the lease deadline, enforced by Redis
itself with no reaper anywhere in this plugin (§4.2, §5.1). The token is what makes
`renew` and `release` safe against a successor (§5.2). Nothing sits beside it
tracking holder liveness, because none is needed: a lease that is not renewed simply
ceases to exist, with nothing having to notice or sweep it.

### 2.5 Event Channel and Payload Format

The plugin publishes its own watch events rather than relying solely on Redis
keyspace notifications (§4.3 argues why). Channel and payload:

```
channel: <prefix>:e:c:<key>
payload: C | D          # C = Changed, D = Deleted
```

One character, because the channel already carries the key and the SDK's cache
events are key-only by contract (`docs/DESIGN.md` §2.1 Lightweight Notifications).
`Expired` has no `E` payload: nothing runs plugin code when a TTL lapses, so that
one event necessarily comes from Redis's own `expired` keyspace notification
(§4.3). The lock-release channel is the same shape with a fixed `R` payload on
`<prefix>:e:l:<name>`.

An unrecognized payload on a *published* channel is treated as `Reset` and broadcast to
every watcher on that key, per ADR-003's mapping for an unintelligible backend signal.
It costs a spurious re-read and cannot be mistaken for a real event, and it emits
`cluster.watch.reset` (§9) because that is what it is.

**The same rule is wrong for the keyspace family, which is why that one drops
instead.** A server whose `notify-keyspace-events` is configured more widely than
§4.3 asks for emits a notification per Redis *command* — `hset`, `hincrby`,
`expire` — and mapping those to `Reset` would cost a re-read on every mutation the
in-script publish has already reported correctly. So the parser has a fourth
outcome for them: dropped, silently, on the grounds that nothing was lost. Only
`expired` and `evicted` carry information no plugin code could have published.

The lock-release channel is the same shape with a fixed `R` payload on
`<prefix>:e:l:<name>`, but a waiter wakes on **any** payload there rather than only
on `R`. The wake is a hint and the acquisition loop re-attempts its `SET NX` as the
source of truth (§5.3), so a spurious wake costs one round trip while a wake
withheld on an unrecognized payload would cost a real waiter its whole delay.

## 3. Component Model

### 3.1 Crate Structure

```
cf-redis-cluster-plugin/
  src/
    lib.rs          — public API re-exports
    config.rs       — RedisClusterConfig, RedisLockConfig (serde, §8)
    provider.rs     — ClusterCacheProvider impl ("redis") + ClusterLockProvider impl ("redis")
    plugin.rs       — RedisClusterPlugin, builder, handle (combined cache+lock)
    preflight.rs    — startup INFO / CONFIG GET checks and the consistency decision (§3.4, §3.6)
    scripts.rs      — the Lua catalog (§6): source, SHA cache, EVALSHA with EVAL recovery
    redis_error.rs  — fred Error → ClusterError / ProviderErrorKind (§10)
    observability.rs— RedisSignals: the ADR-004 sink plus the plugin-local meter (§9)
    connect.rs      — the pool + subscriber open and the reconnect policy, shared by both handles
    subscriber.rs   — the subscriber's fan-out, reconnect-observer, and watchdog tasks,
                      carrying all three message families (§3.3), shared by both handles
    wait.rs         — WaitPolicy + wait_for_replicas, shared by the cache and the lock
    shutdown.rs     — POOL_CLOSE_TIMEOUT, GUARD_DRAIN_TIMEOUT, close_pool,
                      drain_tracked_tasks, cancel_and_diagnose_drop, shared by both handles
    cache/
      mod.rs        — RedisCache (ClusterCacheBackend impl)
      watch.rs      — per-key/per-prefix watcher registry, fan-out, and channel naming
      scan.rs       — SCAN-based scan_prefix, per-shard in cluster mode (§4.4)
    lock/
      mod.rs        — RedisLock (DistributedLockBackend impl); RedisLockPlugin, builder,
                       handle (standalone lock-only construction, §3.5)
      waiters.rs    — in-process release-waiter registry fed by the subscriber (§5.3)
  docs/
    DESIGN.md       — this document
    TESTING.md
```

**The five top-level files below `redis_error.rs` are shared machinery, and each is
there because there are two handles rather than one.** `RedisClusterHandle` and
`RedisLockHandle` (§3.5) open the same pair of connections, apply the same shutdown
rules, and offer the same `WAIT`, and every one of those is a *policy* the two must
not differ on — the client-side command timeout that makes `stop()` finite (§11,
§12), the bounded pool close, the ADR-006 `Drop` guard's cancel-before-diagnose
ordering, what a short `WAIT` count means. The obvious alternative — the `Drop`
guard in `plugin.rs` and the bounded pool close in `lock/mod.rs`, one copy per
handle — is exactly what `postgres-cluster-plugin` does, and its two copies drifted
in both directions: `postgres-cluster-plugin/src/shutdown.rs` exists to record that
they did. This plugin has the identical two-handle shape, so it holds one
implementation of each rule and neither handle can drift from a rule it does not
own. `subscriber.rs` is shared for the same reason: the connection it drives is a
*plugin*-level one carrying three families (§3.3), one of which is the lock's, and
it draws names from `cache/watch.rs` and `lock/` alike, so it belongs to neither.
The standalone lock handle starts the same fan-out with no cache route at all,
which a module owned by `cache/` could not serve.

No `migrations/` directory and no schema step: Redis creates each key on first write,
so there is nothing to provision at startup. That is also why the combined and
standalone shapes (§3.5) differ so little.

**Why `fred`, and why a Redis client at all.** The workspace has no Redis client
today (verified: no `redis`, `fred`, `deadpool-redis`, or `bb8-redis` anywhere in
`Cargo.toml` or `Cargo.lock`), so this plugin introduces one. `docs/DESIGN.md` §3.5
already names `fred` for this plugin, and §4.1's `ProviderErrorKind` mapping table
is written in `fred`'s `ErrorKind` vocabulary (`IO`, `Timeout`, `Auth`,
`Backpressure`) — adopting `redis-rs` instead would mean rewriting that committed
column. On top of matching the parent design, `fred` supplies four things this
plugin would otherwise hand-roll: a reconnect/backoff policy with subscription
replay (§4.3 depends on it), first-class Sentinel and Cluster routing, a
`SubscriberClient` that survives failover, and `EVALSHA` with `NOSCRIPT`
recovery (§6). Two of those four carry a caveat: `fred` supplies **no reconnect
policy by default** (§10), so the replay half of the third holds only because
`connect.rs` sets one; and the fourth is reached through the SHA `SCRIPT LOAD`
returns rather than through `Script`, for the reason the feature list gives below.

`fred = "10"` (10.1.0 at time of writing), pinned with an explicit interface list:

```toml
fred = { version = "10", default-features = false, features = [
    "i-keys",            # GET/SET/DEL/EXISTS/PEXPIRE/PERSIST/PTTL
    "i-hashes",          # HGET/HSET/HMGET/HINCRBY
    "i-pubsub",          # PUBLISH/SUBSCRIBE/PSUBSCRIBE/PUBSUB NUMPAT
    "i-server",          # INFO, WAIT
    "i-scripts",         # SCRIPT LOAD / EVALSHA / EVAL
    "i-config",          # CONFIG GET / CONFIG SET  (the §3.4 preflight)
    "i-cluster",         # CLUSTER routing + CLUSTER COUNTKEYSINSLOT
    "subscriber-client", # fred::clients::SubscriberClient (§3.3, §4.3)
    "sentinel-client",   # Sentinel topology
    "enable-rustls",     # rediss:// — activates tokio-rustls/aws_lc_rs
    "partial-tracing",
] }
```

No `metrics`/`dynamic-pool`: this plugin emits the ADR-004 catalog itself (§9).
Four properties of that list are load-bearing rather than incidental:

- **`i-config` and `subscriber-client` are not optional extras.** `i-config` gates
  `ConfigInterface`, which is every `CONFIG GET` and `CONFIG SET` in §3.4;
  `subscriber-client` gates `SubscriberClient`, which §3.3 and §4.3 are built on.
  Without either, this design does not compile. `enable-rustls` is likewise not
  optional, since §8's production YAML uses `rediss://`.
- **`enable-rustls`, never `enable-rustls-ring`.** The workspace standardizes on
  the `aws-lc-rs` rustls provider, and the root `Cargo.toml` records the reason in
  the imperative: with both providers active, rustls 0.23 can no longer auto-detect
  and panics at runtime. `fred`'s `enable-rustls` activates
  `tokio-rustls/aws_lc_rs`; `enable-rustls-ring` activates `tokio-rustls/ring` and
  would break the whole binary, not just this plugin.
- **No `i-std`.** It expands to `i-hashes i-keys i-lists i-sets i-streams i-pubsub
  i-sorted-sets i-server`, pulling in four interfaces this plugin never uses.
  Naming interfaces exactly leaves `i-lists` out, which makes `BLPOP` and the rest
  of the blocking family *unreachable at compile time* — so TESTING §6's "no
  blocking commands" rule stops being a grep and becomes a type error. That is why
  §5.3 can dismiss a companion-list wait in one sentence, and why the mechanical
  check in TESTING §6 scans for `KEYS`/`FLUSHALL`/`FLUSHDB` and deliberately does
  not scan for `B*`.
- **No `sha-1`.** `Script::from_lua` is gated behind it because it hashes the
  script client-side. The plugin uses the SHA `SCRIPT LOAD` returns instead — which
  is what §3.2 step 3 describes anyway — keeping a SHA-1 implementation out of a
  FIPS-gated workspace's dependency graph for what is only script-cache addressing;
  `dylint.toml`'s DE0708 non-FIPS hasher allow-list is the independent
  corroboration. The consequence to know about is that `script_load_cluster`,
  `fred`'s only broadcasting API, is gated behind the same feature and therefore
  unreachable, which is what §6's recovery design is built around.

`fred` is used directly and needs no lint exemption: no workspace rule routes Redis
access through a shared abstraction, and adding one is not proposed here — this is
the workspace's only Redis consumer, so an abstraction would have one implementation
and one caller (§13 D6).

### 3.2 Builder / Handle Lifecycle

```rust
pub struct RedisClusterPlugin;

impl RedisClusterPlugin {
    pub fn builder(config: RedisClusterConfig) -> RedisClusterBuilder;
}

impl RedisClusterBuilder {
    pub async fn build_and_start(self) -> Result<RedisClusterHandle, ClusterError>;
}

pub struct RedisClusterHandle {
    cache: Arc<RedisCache>,
    lock:  Arc<RedisLock>,
    /* command pool, subscriber client, background tasks, script SHAs */
    /// Set by `stop` so the `Drop` guard can tell a graceful shutdown from a
    /// forgotten one (ADR-006 §Confirmation).
    stopped: bool,
}

impl RedisClusterHandle {
    pub fn cache(&self) -> Arc<dyn ClusterCacheBackend>;
    pub fn lock(&self)  -> Arc<dyn DistributedLockBackend>;
    pub async fn stop(mut self);
}
```

`Drop` carries the ADR-006 diagnostic guard in full — `stopped` short-circuit,
`std::thread::panicking()` check so a
handle dropped mid-unwind degrades to a WARN instead of a double-panic abort,
debug-build `panic!` / release-build `tracing::warn!` otherwise, and cancellation of
the shared token *before* the diagnostic so a dropped `stop()` future still unwinds
the background tasks. It follows the guard the wiring's own `ClusterHandle` uses
(`cluster/src/domain/wiring.rs`) rather than inventing a variation, so a reader who knows one
knows both.

`build_and_start`:

0. Validates the config values that can only fail at startup — a zero `pool_size`,
   `command_timeout_ms`, or `wait_timeout_ms` (§8) — before
   anything is opened, since each of those silently *removes* a bound rather than
   tightening it.
1. Builds the `fred` client pool against the configured server(s) and `.await`s the
   initial connect, so a bad DSN or an unreachable server fails here rather than at
   first use. Both clients are built with an **explicit bounded reconnect policy**,
   because `fred` supplies none (§10), and the initial connect is separately bounded
   by a 10 s `CONNECT_TIMEOUT` — the policy applies to the first connect too, so
   without that bound a URL pointing at a closed port would retry for the policy's
   whole schedule before this returned (`RD-LIFE-005`).
2. Runs the startup preflight (§3.4) — server topology, persistence settings,
   `maxmemory-policy`, keyspace-notification flags — and computes the consistency
   declaration (§3.6) once, storing it for `consistency()` / `features()` to return.
3. `SCRIPT LOAD`s the Lua catalog once and keeps the SHAs the *server* computed (§6).
   Deliberately **not** broadcast to every primary in cluster mode: `fred`'s only
   broadcasting API is gated behind the `sha-1` feature §3.1 excludes, and the
   `NOSCRIPT` recovery §6 describes warms each node lazily and self-heals across a
   reshard, which a startup broadcast could not.
4. Opens the `SubscriberClient`, issues its initial `PSUBSCRIBE`s, and **confirms
   them with an awaited `PING` on the same connection** before returning. Both
   halves are load-bearing. The ordering is: a release published in the startup
   window would otherwise have no subscriber, and the resulting bug is invisible
   except as a blocked `lock()` that waits out its heartbeat instead of waking
   promptly (`RD-LOCK-003`, TESTING.md §4.3, is the regression test). The `PING` is:
   awaiting `fred`'s `subscribe`/`psubscribe` is **not** a barrier that the server
   has processed the subscription — those futures resolve when the command reaches
   the connection, and `fred`'s reader classifies the server's confirmation frame as
   a subscription response and drops it before it can complete anything. §4.3
   explains the mechanism and what it costs.
5. Spawns the tasks that ride the subscriber: the fan-out, the reconnect observer
   (cache only — a lock has nothing to reset, §3.5), the connection watchdog that
   fires when the reconnect policy is exhausted (§10), and `fred`'s own
   subscription-replay task. A fifth task samples `cluster_redis_connection_state`
   (§9).
6. Returns the handle. By the time it resolves, the pool is connected, the scripts
   are loaded, and the subscriber is live — no readiness gate for callers to reason
   about. A failure at any step tears down whatever the earlier steps started: past
   the connect, every error path closes the pool on the way out, and a subscriber
   that fails to subscribe is quit rather than left half-open.

`stop`: §11.

### 3.3 Connection Model

| Connection | Purpose | Owned by |
|---|---|---|
| Command pool (`fred::clients::Pool`, default 4 connections) | Every cache read/write, every lock command, every `EVALSHA`, every `SCAN` | The plugin (combined or standalone) |
| Subscriber client (1, outside the pool) | All `SUBSCRIBE`/`PSUBSCRIBE` traffic: plugin-published cache events, lock-release events, and Redis `expired`/`evicted` keyspace notifications | The plugin |

Two shapes, and deliberately no more. There is no liveness-tracking connection (a
lease that stops being renewed expires by itself) and no TTL-sweep task (Redis expires
keys natively), so the plugin owns no connection whose purpose is to prove something
about itself. A held lock consumes **no connection at all** — not a pooled one, and
nothing per-lock.

The subscriber must be its own client because a Redis connection in subscribe mode
accepts only subscribe-family commands; `fred`'s `SubscriberClient` additionally
tracks its subscription set and replays it after reconnect, which is what §4.3's
`Reset` handling is built on.

**The subscriber is opened whatever `watch_mode` says, including `disabled`.** That
is worth stating because the opposite is the intuitive reading: `watch_mode` is a
*cache* setting, and closing the connection on its account would silently take the
lock's release wake with it — the third family in the table above — and push every
blocked acquisition onto the jittered heartbeat. What `disabled` saves is the
watcher registry, the cache's subscriptions, and the in-script `PUBLISH` on the
write path (§4.3); what it does not save is the connection.

Steady-state connection count is therefore `pool_size + 1` for the combined plugin
and `pool_size + 1` for the standalone lock plugin, independent of how many locks
are held or keys are watched. Redis connections cost ~20 KB each (ADR-001), so there is
no pressure to keep the pool small; the default of 4 is about pipelining headroom,
not resource thrift.

### 3.4 Startup Preflight

`build_and_start` (and `build_lock`) issue a fixed set of read-only commands once,
before returning, and decide three things from the answers. Each check degrades
explicitly when the command is refused — managed Redis (ElastiCache,
MemoryDB, Azure Cache) commonly restricts `CONFIG`, and a plugin that treated an
ACL denial as a hard failure would be unusable there.

| Command | Decides | If refused / unreadable |
|---|---|---|
| `INFO server` | Redis version; whether `spublish`-family sharded pub/sub is available | Fail `InvalidConfig` — an unreadable `INFO` means an unusably locked-down server |
| `INFO replication` | Topology: standalone / replicated primary / cluster | WARN + treat as replicated (the conservative direction, §3.6) |
| `CONFIG GET appendonly`, `appendfsync` | Whether single-node writes are durable at ack time | Treat as non-durable (conservative) |
| `CONFIG GET maxmemory-policy` | Whether cluster keys can be evicted (§3.7) | WARN `cluster.provider.maxmemory_policy_unknown`; proceed |
| `CONFIG GET notify-keyspace-events` | Whether `expired` notifications will arrive (§4.3) | WARN; `Expired` events are then best-effort |

Three inputs on this behaviour, in strict precedence order — each layer is more
specific about intent than the one below it:

- **`topology` / `durability` config (§8)** — operator-supplied hints, and the
  strongest input. This is the escape hatch for a locked-down managed instance whose
  operator *knows* the answer the plugin cannot read. The two hints are treated
  asymmetrically: a `topology` hint replaces detection outright, while a
  `durability: fsync_always` hint is **cross-checked** against `CONFIG GET` wherever
  that is readable, because `fsync_always` is the one claim that unlocks
  `Linearizable` on its own and is therefore the one worth verifying (§3.6).
- **The connection URL's own scheme** — `redis-cluster://` means cluster and
  `redis-sentinel://` means Sentinel, and both outrank detection because they are
  facts about the *client* rather than claims about a server: a clustered client is
  talking to a cluster whatever one node's `INFO replication` reports about its own
  replicas. A plain `redis://`/`rediss://` URL says nothing and falls through — in
  particular it is **not** read as `standalone`, since it means "one address", not
  "no replicas", and the single-node row of §3.6 is the one place a wrong answer
  weakens a guarantee.
- **Detection**, per the table above.

Plus one lever that writes rather than reads:

- **`manage_keyspace_notifications: bool` (default `false`)** — when `true`, and the
  detected `notify-keyspace-events` flags lack what §4.3 needs, the plugin issues one
  `CONFIG SET notify-keyspace-events` **additively** merging in the missing flags,
  then re-reads to confirm the write took. Additive because the setting is
  server-wide: replacing it with just this plugin's flags would switch off
  notifications an unrelated tenant of the same Redis is subscribed to. Re-read
  rather than trusted because a `CONFIG SET` can succeed against a proxy that
  accepts and drops it, and the plugin would then spend the deployment's life
  believing in events that never arrive. Default off because mutating a shared
  server's global config on a gear's behalf is exactly the kind of surprise an
  operator should opt into. The flag check itself honours Redis's `A` alias — a
  server configured `KA` already emits `expired` and `evicted` and needs nothing
  added, though `K` is still required alongside it, since `A` does not imply the
  keyspace routing that puts those events on a `__keyspace@…__` channel.

Nothing in the preflight ever *upgrades* a guarantee beyond what it verified, and
nothing it discovers changes behaviour silently: every conservative fallback logs
the reason (§9).

### 3.5 Standalone Lock Provider

`RedisLockProvider` implements `ClusterLockProvider` (`provider() -> "redis"`) and
builds a standalone `RedisLockPlugin` with its own pool, its own subscriber, and its
own config type (`RedisLockConfig`, §8) — never reusing a pool from a co-located
`cache: { provider: redis }` binding, per the SDK provider contract's "non-cache
providers do not receive the cache backend". Sharing would couple two providers the
SDK deliberately made independent, and would need a lifecycle-ownership story for the
shared pool — which provider's `stop()` closes it? The cost of not sharing is a
second small pool when both primitives point at the same server: at ~20 KB per
connection, cheaper than the coupling it avoids.

What differs between the two shapes:

| | Combined (`RedisClusterPlugin`) | Standalone (`RedisLockPlugin`) |
|---|---|---|
| Lua scripts loaded | Cache + lock catalog (§6) | Lock catalog only |
| Subscriber subscriptions | Cache event channels + lock-release channels + the `expired`/`evicted` keyspace pattern | Lock-release channels + the same keyspace pattern |
| `notify-keyspace-events` flags | `Kxe` — `x` for the cache's `Expired` (§4.3), `e` for the eviction signal (§3.7) | `Ke` — the eviction signal only, **never** `x` |
| Without those flags | The cache's `Expired` degrades to best-effort, and no eviction is reported | **Nothing degrades.** The lock keeps working unchanged; only the eviction report is lost |
| Preflight checks | All of §3.4 | All of §3.4, asking for the narrower flag set and never writing it |
| Background tasks | Subscriber fan-out, reconnect observer, connection watchdog, connection-state sampler | Subscriber fan-out, connection watchdog, connection-state sampler — **no reconnect observer** |

The middle rows are worth calling out: a Redis **lock-only** deployment works
against a managed instance with keyspace notifications entirely unavailable, with
no degradation at all. Nothing it needs to operate depends on them — a lease lapse
is discovered by the next acquire attempt, which is the source of truth throughout
— so the flags it asks for buy *observability* rather than function. That is why it
asks for `Ke` and not `Kxe`: `x` would be a server-wide flag charged to every other
tenant of the instance for an event this deployment never reads. It is also why it
never sets them itself. `manage_keyspace_notifications` stays a combined-plugin
opt-in, because a server-wide `CONFIG SET` is too blunt an instrument to reach for
on behalf of a report, as against a cache that genuinely degrades without it.

So is the last. **The standalone plugin runs no reconnect observer**, because that
task exists to broadcast a cache `Reset` after a subscription gap and a lock has no
equivalent to reset: a release missed during a gap costs its waiter one jittered
delay, after which the `SET NX` — the source of truth throughout — answers
correctly. Telling anyone about the gap would give them nothing to do with it. The
consequence to know about is in §9: `cluster_redis_subscriber_resubscribes_total`
lives on that observer, so the standalone plugin does not emit it. The *watchdog*
is spawned either way, because an exhausted reconnect policy has something to say
about the lock too — a permanently gone subscriber means every blocked acquisition
falls back to the heartbeat, which costs latency rather than correctness, and that
is worth a line even though there is nothing to close.

The fan-out itself is the same task in both shapes rather than two near-copies: it
takes a routing table whose cache half is optional, and the standalone plugin runs
it with that half empty. That is what keeps the lock's wake path identical between
the two (`RD-LOCK-009`) instead of a second `select!` free to drift from the first.

Operator YAML — Redis lock alongside a non-Redis cache:

```yaml
cluster:
  profiles:
    default:
      cache:
        provider: standalone
      lock:
        provider: redis
        url: "redis://:${REDIS_PASSWORD}@redis:6379/0"
        pool_size: 4
```

`RedisLockPlugin`'s handle carries the same `stopped` field and the same ADR-006
`Drop` guard as the combined handle — it owns its own pool and subscriber, so it
needs the same protection independently.

### 3.6 Topology and the Consistency Declaration

ADR-009's per-backend table is unambiguous, and this plugin's declaration is
derived from it mechanically rather than chosen:

| Detected / declared topology and durability | `consistency()` | Rationale (ADR-009) |
|---|---|---|
| Single node, `appendonly yes` + `appendfsync always`, no replicas | `Linearizable` | The one Redis configuration ADR-009 marks safe: nothing is acked before it is fsynced, and there is no replica to fail over to |
| Single node, `appendfsync everysec` (default) or `appendonly no` | `EventuallyConsistent` | Crash between ack and fsync loses up to 1 s of accepted writes → two leaders |
| Sentinel / any replicated primary | `EventuallyConsistent` | Async replication: every failover may promote a replica that never saw an accepted write |
| Redis Cluster | `EventuallyConsistent` | Same async replication, plus slot-migration edge cases |
| Unknown (preflight refused, no operator hint) | `EventuallyConsistent` | Conservative default |

Three properties of this mechanism matter more than the table:

- **`Linearizable` is opt-in and verified, not asserted.** An operator claiming
  `durability: fsync_always` still has it checked against `CONFIG GET appendonly`
  *and* `appendfsync` when those are readable — both, because `appendfsync always`
  means nothing with `appendonly no`, and a check reading only the policy would
  declare `Linearizable` for a server keeping no durable log at all. A claim
  contradicted by the server fails startup with `InvalidConfig` naming both values.
  A claim the plugin *cannot* check is trusted instead, and logs
  `cluster.provider.consistency_asserted` (WARN, once) saying so — an operator lie
  is then visible in the logs of the deployment that told it.

  That warning fires whenever **either** leg of a `Linearizable` declaration rests
  on something unchecked, not only the durability one: a `topology: standalone` hint
  means no `INFO replication` was read, so the "no replicas" half is also the
  operator's word. Only the single-node row can reach `Linearizable`, so only it
  needs the provenance distinction at all — every other row decides
  `EventuallyConsistent` whether the plugin read it off the server or took the
  operator's word for it, and there is no such thing as an unverified *downgrade*.
  Only the upgrade direction can fail, too: a hint *weaker* than the server's actual
  setting can only under-declare, which is safe and is a legitimate choice for an
  operator who does not want a durable-today server silently becoming the basis of a
  `Linearizable` declaration tomorrow.

  The contradiction check runs on **every** row rather than only the one that could
  reach `Linearizable`. A `durability: fsync_always` the server denies is untrue on
  a Sentinel deployment too, and checking it only where it would have mattered lets
  the same config sit unreported until the day it is moved to a single node.
- **`WAIT` does not upgrade anything.** The plugin supports an optional
  `wait_replicas` / `wait_timeout_ms` pair that appends `WAIT n timeout` after a
  write whose outcome a failover must not silently undo, which genuinely narrows the
  Sentinel failover window and is worth having. Per ADR-009 §"No linearizable-ish
  middle ground" it does **not** move the declaration: `WAIT 1` reduces but does not
  eliminate the window, and the `CacheConsistency` enum is deliberately two-valued.
  A `WAIT` that times out is surfaced as `Provider { ResourceExhausted }` on that
  write rather than being swallowed — the command does not *error* on timeout, it
  returns however many replicas acknowledged, so a caller that ignored the count
  would have opted into a guarantee and then silently not received it.

  **The policy is a `WaitPolicy` built once, before anything is opened**, rather
  than an `Option<WaitPolicy>` read at each write. `WAIT`'s timeout argument is
  signed, so a `wait_timeout_ms` past `i64::MAX` has no honest rendering — and the
  only place with somewhere to report that is startup. `WaitPolicy::from_config`
  is therefore fallible and its enabled variant's fields are unreachable outside
  `wait.rs`, so the clamp that would otherwise turn an unreadable config into a
  ~292-million-year deadline cannot be written. It is called beside
  `config.validate()` and for the same reason: an unrepresentable value fails
  before there is a pool or a subscriber to tear down.

  **Which writes carry it is narrower than "each lock and CAS write" reads.** On
  the cache it is the three *conditional* writes — `put_if_absent`,
  `compare_and_swap`, `compare_and_delete` — and not an unconditional `put` or
  `delete`, which carry no decision made on a value a failover could invalidate. On
  the lock it is **acquisition only**, not `renew` and not `release`: losing an
  acquisition to a promotion is the failure `WAIT` exists for, since the new primary
  has no lease key and a second instance takes the name while this one believes it
  holds it, whereas losing a renewal fails the other way (the lease reverts to its
  earlier, shorter deadline) and losing a release only leaves a name unacquirable
  until its TTL. Neither of those can produce two holders, so neither is worth a
  round trip on every call. A short count on acquisition gives the lease back before
  reporting, rather than wedging a name nobody took for a whole TTL.
- **The declaration is computed once at startup, never re-evaluated.** A topology
  that changes under a running plugin (a replica attached to what was a single node)
  will not downgrade a live `Linearizable` declaration. This is a real gap, recorded
  in §12 rather than solved: re-declaring mid-flight would mean a backend whose
  capability answers change after consumers have already resolved against them,
  which the resolution model (`cpt-cf-clst-fr-validation-startup-fail`) has no way to
  express.

### 3.7 Eviction Is a Correctness Concern, Not a Capacity One

Under `maxmemory` pressure with any `allkeys-*` or `volatile-*` policy, Redis
deletes keys nobody asked it to delete. For an application cache that is the
feature. For this plugin it is silent corruption of every primitive at once: an
evicted lock key hands the lock to a second holder while the first still believes it
holds it; an evicted leader-election key elects a second leader. No TTL has lapsed
and no consumer is told.

The plugin therefore:

- Reads `maxmemory-policy` at startup and, if it is anything other than
  `noeviction`, logs `cluster.provider.maxmemory_policy_unsafe` (WARN, once, naming
  the policy and this section) — warn rather than fail, because the policy is a
  server-wide setting the cluster keys may be sharing with unrelated tenants, and
  refusing to start would make this plugin unusable on any shared Redis.
- Subscribes to the `evicted` keyspace notification (when notifications are
  available at all) and, on each one for **any key** under its own prefix — cache
  entry or lock lease — logs `cluster.provider.eviction_observed` (WARN,
  rate-limited) and increments
  `cluster_redis_evictions_observed_total{provider, primitive}` (§9). This turns a
  silent failure into an alertable one — the single highest-value operational
  signal this plugin emits.
- Maps the event to `CacheEvent::Deleted` for watchers, not `Expired`: no TTL
  elapsed, and a consumer distinguishing the two would be misled.

Two things about that counter and its scope:

- **The counter is plugin-local, and it has to be.** The catalog counter
  `cluster_provider_errors_total` cannot carry an eviction in two independent ways:
  it takes `{provider, kind}` and no `op` at all, and its label set is a hard
  ADR-004 contract rather than a convention; and an eviction is not a
  `ClusterError`, so it cannot travel through `emit_provider_error`, which returns
  early on anything that is not `ClusterError::Provider`. Folding it on as
  `kind = "other"` would make an eviction indistinguishable from every other
  unclassified backend failure *and* would put something that is not an operation
  failure into a provider-error rate. A counter that says what it counts is the
  honest form. The WARN keeps its contracted name.
- **Observation spans both primitives, and the label is what makes it usable.**
  The keyspace pattern is `__keyspace@<db>__:<prefix>:*` rather than one scoped to
  the cache's `:c:` segment, because the paragraph above opens with the lock case
  and rates it worst. One pattern and a classifier, rather than one pattern per
  primitive: the notification stream is a single connection's, sorting a key is a
  prefix compare, and the classifier asks `ChannelNames` and then `LockNames` — the
  same types that *build* those names — so no third place spells `:c:` or `:l:`.
  A key under this prefix that neither claims is declined and logged at DEBUG,
  which today means the event channels the pattern also matches and at which no key
  exists.

  `primitive` is `cache` or `lock` and it labels the counter, appears on the WARN,
  and selects the rate limiter. The last of those is not bookkeeping: the two
  windows are independent, because a shared one would let an eviction storm in the
  cache — thousands of entries, each costing a re-read — spend the budget that the
  one line reporting a **double-held lock** needs, and it is exactly under that
  much memory pressure that the lock line gets emitted.

  The **standalone lock plugin** (§3.5) subscribes the same pattern for the same
  reason, with its cache half empty so that a `<prefix>:c:` key belongs to a
  different deployment sharing the prefix and is never claimed. It is the shape
  likeliest to be pointed at a shared Redis, having no cache working set to argue
  for its own instance.

**The operator guidance is unambiguous and belongs in the deployment docs: run
cluster keys on a Redis instance (or `noeviction` policy) where they cannot be
evicted.** A dedicated instance, or a separate logical database, is the
recommendation; a shared cache instance under `allkeys-lru` is a documented
misconfiguration this plugin can report but not prevent.

## 4. Cache Implementation

### 4.1 Command Contract per Operation

`K` is `<prefix>:c:<key>`; `CH` is `<prefix>:e:c:<key>`. `PX` is the millisecond
TTL from `PutRequest.ttl` (`Ttl::Of(d)` → `d.as_millis()`, `Ttl::Indefinite` →
`PERSIST`). Scripts are named per §6 and invoked by `EVALSHA` with one key.

| Operation | Redis |
|---|---|
| `get(key) -> Option<CacheEntry>` | `HMGET K v ver` — both `nil` → `None`; both present → `CacheEntry { value, version }`; **exactly one present → `Provider { Other }`**, not `None` (see below). No Lua |
| `put(req) -> ()` | `cache_put` script: `HSET K v <value>`, `HINCRBY K ver 1` (creating at 1 when absent), TTL applied or cleared, `PUBLISH CH C` |
| `put_if_absent(req) -> Option<CacheEntry>` | `cache_put_if_absent` script: if `EXISTS K` → return `nil`; else `HSET K v <value> ver 1`, apply TTL, `PUBLISH CH C`, return version 1 |
| `compare_and_swap(key, expected, new, ttl) -> CacheEntry` | `cache_cas` script: string-compare `HGET K ver` against `expected`; equal → `HSET`/`HINCRBY`/TTL/`PUBLISH`, return new version; unequal or absent → return the current `{ver, v}` so the caller's `CasConflict.current` is populated in the same round trip |
| `compare_and_delete(key, expected_value) -> bool` | `cache_compare_and_delete` script: byte-compare `HGET K v`; equal → `DEL K`, `PUBLISH CH D`, return 1; else return 0 (never an error) |
| `delete(key) -> bool` | `cache_delete` script: `DEL K`; if it removed something, `PUBLISH CH D` and return 1; else return 0 with no event. A script rather than a bare `DEL` only so the publish is atomic with the delete, and so a delete of an absent key emits nothing |
| `contains(key) -> bool` | `EXISTS K`. No Lua |
| `scan_prefix(prefix) -> Vec<String>` | `SCAN` loop, §4.4. No Lua |
| `watch(key)` / `watch_prefix(prefix)` | Subscriber registration, §4.3. No server round trip beyond the initial subscribe |

Two invariants across the table: **every mutation publishes its own event inside the
same script**, so a consumer cannot observe a write without its notification
arriving (modulo pub/sub's fire-and-forget delivery, §4.3, and modulo
`watch_mode: disabled`, under which the channel argument is empty and every script
skips its `PUBLISH`); and **every script touches exactly one key**, passed via
`KEYS[1]`, so Redis Cluster routes it correctly and no `CROSSSLOT` error is
reachable (§6).

`get`, `contains`, and `scan_prefix` are the only operations that avoid Lua. That is
not an oversight to fix later: each mutation needs an atomic read-modify-write over
two hash fields plus a conditional TTL plus a publish, which no single Redis command
expresses.

**A half-populated hash is an error, not an absence.** Every mutation writes `v` and
`ver` inside one script, so exactly one field present means a key at this name that
something other than this plugin owns. Reporting it as `None` would be worse than
failing: the caller's next `put_if_absent` would find the key "absent", and the
write would merge into a stranger's hash rather than being refused. The three
conditional writes additionally issue the operator's `WAIT`, if one is configured
(§3.6).

### 4.2 TTL

TTL is native. `Ttl::Of(d)` becomes `PEXPIRE K <ms>` inside the write script;
`Ttl::Indefinite` becomes `PERSIST K`. **There is no TTL reaper in this plugin**: TTL
enforcement is entirely Redis's own, which is why ADR-001 rates native per-entry TTL a
first-class advantage of this backend.

Consequences worth stating:

- A write always sets the entry's TTL explicitly from the request; it never
  preserves a previous one implicitly. `Ttl::Indefinite` on an entry that had a TTL
  clears it, which is what the SDK's two-valued `Ttl` says should happen.
- A sub-millisecond `Ttl::Of` rounds **up** to 1 ms rather than down to 0.
  `PEXPIRE k 0` deletes the key outright, so rounding down would turn "expires
  almost immediately" into "was never stored", and the caller's next read would see
  an absence it could not distinguish from a failed write. The lock's `PX` argument
  is floored the same way and for a sharper reason: `PX 0` is an error reply, so an
  acquisition would fail rather than expire.
- `Expired` watch events depend on Redis's own notification, which fires when the key
  is actively expired or lazily deleted on access — up to the active-expire cycle
  (~100 ms typical) after the deadline. That is "shortly after", not "at the
  deadline": a consumer needing the exact moment must read the TTL rather than wait
  for the event.
- Redis replicas do not expire keys themselves. Since all traffic goes to primaries
  (§1.3), no read can observe a logically-expired entry.

### 4.3 Watch

The subscriber client holds three kinds of subscription and one fan-out task routes
everything to a per-key / per-prefix watcher registry. **Three families, but two
streams** — which is a `fred` behaviour rather than a design choice, and the single
most misleading thing about the obvious reading of this diagram:

```
plugin PUBLISH (in-script)  ──►  <prefix>:e:c:<key>       ──►  message_rx()        ──┐
Redis `expired` keyspace    ──►  __keyspace@<db>__:<K>    ──►  keyspace_event_rx() ──┼─► fan-out ─► watchers
Redis `evicted` keyspace    ──►  __keyspace@<db>__:<K>    ──►  keyspace_event_rx() ──┘
```

`fred`'s router recognizes the `__keyspace@`/`__keyevent@` channel prefixes and
diverts those messages to a separate broadcast stream, pre-parsed into a
`{ db, key, operation }` triple, so they **never appear on the pub/sub stream at
all**. A fan-out reading only the pub/sub stream — which is what "one fan-out task
routes everything" reads as — subscribes to the keyspace pattern successfully,
watches the server deliver on it, and still emits not one `Expired`. The split is
invisible below Layer 3: every unit test passes throughout, because nothing about it
is a decision this plugin makes. So the fan-out `select!`s over both receivers. The
keyspace half needs no channel parsing in exchange, `fred` having already done it —
but it does need its `db` checked against the cache's own, or a notification from
another logical database on the same server is reported as a deletion of a key this
cache never had.

**Why plugin-published events rather than keyspace notifications alone.** Raw
keyspace notifications would need no `PUBLISH` at all, which is tempting on a
10 000-writes/sec path. They were rejected as the primary mechanism for a reason
that is not about cost:

- **They fire per Redis command, not per logical write.** A single `put` executes
  `HSET` + `HINCRBY` + `PEXPIRE`, so a hash-based entry (§2.2) would emit three
  notifications — three `Changed` events for one write, against
  `cpt-cf-clst-nfr-watch-delivery`'s "zero duplicate events per subscriber per key
  in normal operation". Coalescing them downstream means guessing which commands
  belonged to which write.
- **They cannot express the plugin's event taxonomy.** `hset`, `hincrby`, `expire`,
  `del`, `unlink`, `expired`, `evicted` must be mapped to three SDK events, and the
  mapping is lossy in the direction that matters (an `expire` command is not a
  change to the value).
- **They require global server configuration** (`notify-keyspace-events`) that
  managed Redis often will not grant, and getting the flag string wrong degrades
  silently.

Publishing inside the script solves all three: exactly one event per logical
mutation, the correct event type by construction, atomic with the write, and no
server configuration needed. The costs, accepted and documented: one extra command
per write (no extra round trip — it is inside the same script), and Redis Cluster's
`PUBLISH` broadcast to all nodes, whose ~12 500 publishes/sec ceiling (ADR-001) is
well below the plugin's write ceiling and therefore becomes the binding constraint
on a clustered, heavily-watched deployment (§12).

**`Expired` is the one event that cannot be self-published**, since no plugin code
runs when a TTL lapses. It comes from Redis's `expired` keyspace notification, which
requires `notify-keyspace-events` to include `K` to route events to a
`__keyspace@…__` channel at all, `x` for `expired`, and `e` for `evicted` (§3.7).
**The minimal correct flag set is therefore `Kxe`**, and that is what the plugin
checks for at startup (§3.4). Minimal matters here rather than being tidiness: every
flag is **server-wide**, and `manage_keyspace_notifications: true` writes the
setting globally, so anything beyond `Kxe` is traffic unrelated tenants of the same
Redis pay for and this plugin never reads. The tempting additions are the worst
offenders — `g` adds a notification for every generic command and `$` for every
string command. A server configured more widely still works: the extra
notifications arrive, are recognized as outside this plugin's vocabulary, and are
dropped (§2.5).

When the flags are absent and cannot be set, the plugin logs
`cluster.provider.expiry_events_unavailable` (WARN, once) and delivers no `Expired`
events at all. This degrades promptness, not correctness: an expired entry still
reads as absent, and both SDK default backends that ride this cache have
timer-driven fallbacks (`CasBasedLeaderElectionBackend` renews on a timer and only
uses the watch for promptness; `CasBasedDistributedLockBackend` has a timeout
fallback), so neither depends on the event arriving.

**Watch modes.** One config lever (`watch_mode`, §8) with exactly two settings —
§13 D4 decided against a third:

| Mode | Behaviour | `features().prefix_watch` |
|---|---|---|
| `publish` (default) | As above: in-script publish + `expired`/`evicted` keyspace events | `true` on standalone/Sentinel; `false` in Cluster (see below) |
| `disabled` | No publish, no watcher registry, no cache subscriptions. `watch`/`watch_prefix` return `Unsupported`; a consumer relying on prefix watch falls back to `PollingPrefixWatch` over `scan_prefix`, the documented behaviour on any prefix-watch-incapable cache | `false` |

`disabled` exists because the issue explicitly permits it ("if prefix-watch proves
inefficient, use the SDK's polling fallback") and because it is the honest answer for
a managed Redis where neither `CONFIG` nor the publish overhead is acceptable.

**"No publish" is enforced through one accessor, and it has to be.** The write path
is where the saving is: gating only the watcher registry and the subscriptions would
leave every mutation still running its `PUBLISH`, to a channel that by construction
has no subscriber — watches off, and none of the cost saved that the mode exists to
save, which is precisely backwards for the deployment the previous paragraph names.
So the cache's channel accessor answers the **empty string** under `disabled`, and
every mutation script guards its `PUBLISH` on a non-empty channel argument. Putting
it in the one accessor all five mutation paths already call is what keeps them from
disagreeing about the mode — none of them needs to know it exists — and a Layer 1
test asserts that every script in the catalog carries as many empty-channel guards
as it has `PUBLISH` calls, so a sixth mutation script cannot be added unguarded.

What `disabled` does **not** turn off is the subscriber connection itself (§3.3):
the lock's release wake rides it, and a cache setting must not silently push every
blocked acquisition onto the heartbeat.

A third mode — raw keyspace notifications with best-effort duplicate coalescing, for
a deployment wanting watches without paying `PUBLISH` — was considered and rejected
(§13 D4): the two modes above cover both ends, and coalescing per-command
notifications back into logical writes carries a test matrix disproportionate to a
benefit nothing has asked for. `WatchMode` is therefore a two-variant enum, not a
three-variant one with a `todo!()`.

**Prefix watch.** `watch_prefix(p)` is `PSUBSCRIBE <prefix>:e:c:<p>*` — a genuine
native prefix watch, satisfying `CacheCapability::PrefixWatch` rather than polyfilling
it. Two caveats bound it:

- **Redis Cluster.** Plain `PUBLISH` is broadcast cluster-wide, so plugin-published
  events *do* reach a subscriber on any node — but `expired`/`evicted` keyspace
  notifications are emitted only by the node owning the key, so expiry events in
  cluster mode need a subscription on every primary plus re-subscription on slot
  migration. Until that is implemented and verified against a real 3-node cluster
  (`RD-SPEC-010`, TESTING.md §4.6), **`features().prefix_watch` returns `false` in
  cluster mode** and `watch_prefix` returns `Unsupported`. Declaring `true` on the
  strength of an untested code path is precisely the dishonest declaration ADR-009
  §"Honest backend declaration" forbids. §13 D2 records the decision to ship `false`
  and designates `RD-SPEC-010` as the gate on lifting it.
- **Pattern-subscription cost.** `PSUBSCRIBE` matching is per-published-message
  against every registered pattern, so a deployment with thousands of distinct
  prefix watches pays on every write. The plugin registers **one** Redis pattern per
  distinct prefix and fans out in-process to every watcher on it, so N consumers
  watching the same prefix cost one pattern, not N.

The consumer's prefix is **glob-escaped** before it becomes a pattern, for the same
reason `scan_prefix` escapes it (§4.4) and with a worse consequence if it is not:
the key space is opaque to this plugin (§2.1), so a prefix containing `[` or `*`
would subscribe to something other than what was asked for, and the watcher would
then receive events for keys it does not watch *and* miss the ones it does.

Three registry properties beyond the fan-out itself:

- **Subscriptions are reference-counted, not merely deduplicated.** `SUBSCRIBE` on a
  key's first watcher and `UNSUBSCRIBE` when its last one is pruned. The second half
  matters as much as the first: a key watched once and then abandoned would
  otherwise keep costing a message per write for the life of the process. Both
  decisions run under one mutex, and that is load-bearing rather than defensive —
  the prune is noticed on the *delivery* path while registration happens on the
  *caller's*, so unserialized an `UNSUBSCRIBE` decided just before a new `watch()`
  can land just after its `SUBSCRIBE`, leaving a registered watcher with no
  server-side subscription and no event ever again. Delivery itself never takes the
  mutex.
- **Each message serves one watcher family.** A key with both an exact watcher and a
  covering prefix watcher is subscribed twice server-side, so one `PUBLISH` arrives
  twice. Redis distinguishes the two (`message` versus `pmessage`) and so does the
  fan-out: an exact-subscription message reaches only exact watchers and a pattern
  message only prefix watchers. Routing both to both — the obvious implementation —
  would deliver two `Changed`s for one `put` to any doubly-covered watcher, which is
  the duplicate `cpt-cf-clst-nfr-watch-delivery` forbids and `RD-WATCH-001` pins.
  The keyspace family has no such twin, being one blanket pattern, so it is served
  to both.
- **The keyspace pattern is always on and covers every key this cache owns**, not
  just the watched ones, because §3.7's eviction signal has to observe evictions of
  keys nobody is watching.

**Lifecycle signals.** All three are produced, and Redis is the first backend to
produce all three natively:

- **`Lagged { count }`** — the per-watcher channel is bounded (64 slots). A
  `try_send` that reports full drops the event and coalesces the drop count, and the
  count then **rides the next successful send to that watcher**. Not "when the
  buffer drains": nothing polls a drained buffer, so there is no moment at which
  something could notice. The limitation that follows is real and is recorded rather
  than papered over — *a watcher that lags and then sees no further traffic on its
  key never learns it lagged*. It is why `RD-WATCH-008` drives enough writes to make
  the coalesced event actually fire. This is ADR-003's "Redis pub/sub backpressure →
  `Lagged`" row, and this plugin is the first thing in the platform that produces
  the variant at all.
- **`Reset`** — emitted from three places, all meaning "re-read": the subscriber
  reconnected; the fan-out's own broadcast stream lagged, so messages were lost
  before the task could read them and there is no way to know which; or an
  unrecognized payload arrived on a published channel (§2.5), which resets that
  key's watchers rather than the registry. Redis pub/sub is fire-and-forget with no
  resumption point, so every gap is a total gap. Each `Reset` logs
  `cluster.watch.reset` and increments
  `cluster_watch_resets_total{provider="redis",primitive="cache"}`.

  On the reconnect path the `Reset` goes out **as soon as the reconnect is
  observed**, which may be marginally before `fred` finishes replaying the
  subscription set. That ordering is deliberate: the events in the gap are lost
  either way, the re-read travels the command pool rather than this client, and
  telling the consumer late would leave a window in which it believes stale data is
  current. And **every** reconnect notification is acted on, including the first —
  see §10 for why skipping one was wrong.
- **`Closed(ClusterError::Shutdown)`** — on `stop()`, before it returns (§11).
  `Closed(Provider { ConnectionLost })` when the reconnect policy gives up (§10);
  `ConnectionLost` specifically, because only a retryable error lets the SDK's
  `RestartingWatch` combinator run against what is in fact a recoverable outage.

Two registry properties are load-bearing rather than incidental. The fan-out task
**never awaits a slow watcher** — one stalled subscriber must not stall delivery for
everyone, so a full buffer produces `Lagged` rather than backpressure. And a `closed`
latch plus a mutex serializing terminal broadcasts guarantee that no `Reset` is ever
delivered after a terminal `Closed` (which the SDK's `CacheWatch` contract forbids)
and that a `watch()` arriving after shutdown receives its terminal event immediately
rather than registering into a registry nothing will dispatch to again.

### 4.4 scan_prefix

`scan_prefix(p)` is a cursor loop — `SCAN <cursor> MATCH <prefix>:c:<p>* COUNT 500`
until the cursor returns to 0 — with the plugin's own prefix stripped from each
returned key. `KEYS` is never used: it is O(N) *blocking* the whole server, which on
a shared production Redis is an outage.

Three properties to be explicit about:

- **Cluster mode scans every primary** and concatenates, since `SCAN` is per-node.
  Slot migration during a scan can therefore duplicate or miss a key; the returned
  order is unspecified anyway, and the
  polling polyfill that consumes this diffs sets rather than trusting order.
- **`SCAN` guarantees are weak by design**: keys present for the whole scan are
  returned at least once; keys added or removed mid-scan may or may not appear. That
  is exactly the contract `PollingPrefixWatch` needs and no stronger.
- **Cost scales with the whole keyspace**, not the matched subset, because `MATCH`
  filters after the fact — which is why `watch_mode: publish`, making native prefix
  watch available and the polling polyfill unnecessary, is the default.

**The consumer's prefix is glob-escaped before it becomes a `MATCH` pattern.** The
key space is opaque to this plugin (§2.1), so nothing upstream rules out a `[` or a
`*` in it: unescaped, `scan_prefix("report[2024]")` is a character class matching
keys the caller never asked about, and `scan_prefix("*")` returns the entire cache.
Redis's glob syntax treats `\` as the escape character and gives `*`, `?`, `[`, and
`]` their special meanings, so the backslash is escaped first — otherwise a key that
legitimately contains one consumes the character after it. The same escaping is
applied to a prefix watch's pattern (§4.3) and to the lock's release pattern, for
the same reason each time.

### 4.5 Consistency Declaration

`consistency()` returns whatever §3.6 computed at startup — `EventuallyConsistent`
in every replicated or non-fsync-durable configuration, `Linearizable` only for a
verified single-node `appendfsync always` deployment. It never changes over the
handle's life.

What is *not* weak, and should not be conflated with the topology-level declaration:
every individual operation in §4.1 is atomic. Redis executes a Lua script to
completion without interleaving, so a `compare_and_swap` cannot lose to a concurrent
CAS on the same key, and `put_if_absent` cannot admit two winners — on a single
primary. The weakness ADR-009 names is entirely about *losing an acknowledged write*
(to an fsync gap or a failover), not about racing two concurrent ones. Consumers
that need the former are the ones that must not bind Redis; consumers that only need
the latter (rate-limit counters, per-tenant budgets — the OAGW's actual use case)
are well served here.

## 5. Distributed Lock Implementation

**A held lock costs one task.** The SDK hands the consumer a `LockGuard` built by
`LockGuard::channel`, and the backend owns the paired `LockCommandReceiver`: `renew`
and `release` do not arrive as method calls on the backend at all, they arrive as
`LockRequest`s on that channel, long after the acquisition that produced the guard
returned. So something has to be selecting on it for as long as the lock is held —
one task per held guard.

That is compatible with §3.3's "a held lock consumes no *connection*", which remains
true: a guard task borrows a pooled connection only for the round trip a `renew` or
a `release` actually needs. But it is exactly the detached-task class §3.1's
`shutdown.rs` exists to warn about, so the tasks are spawned onto a tracker rather
than with a bare spawn, and `stop()` drains them under a bound **before** closing
the pool they share — a task caught mid-`renew` still needs a connection to finish
on (§11). Each task also selects on the shutdown token, which is what keeps that
drain bounded by the in-flight command rather than by the consumer's critical
section.

### 5.1 The Lock Entry and the Holder Token

Acquisition is one command:

```
SET <prefix>:l:<name> <holder_token> NX PX <ttl_ms>
```

`OK` → acquired, `nil` → held by someone else (`ClusterError::LockContended`). The
holder token is a fresh `uuid::Uuid` per acquisition. That is the entire mutual
exclusion mechanism: `NX` is atomic on a single primary, and `PX` makes the entry
its own reaper.

**Nothing else participates in exclusion.** There is no liveness proxy, no
reclamation sweep, and no in-process registry of what this instance holds — none is
needed, because the design never has to distinguish "the holder crashed" from "the
lease lapsed". A crashed holder stops renewing and the key evaporates on its own
deadline, and the next acquirer's `SET NX` is the only thing that has to notice.
There is no orphaned-state class either: a `try_lock` cancelled after its `SET`
committed leaves a key that expires at its TTL exactly like any other, with nothing
local claiming to own it.

The cost of that simplicity is that reclamation is **TTL-bounded, never faster**:
nothing detects a dead holder sooner than its lease expires, because nothing is
watching the holder. Consumers tune that with the `ttl` they pass per acquisition,
which is where the SDK already puts the control — a shorter TTL buys faster recovery
at the price of more renewal traffic.

`features().linearizable` mirrors the cache's declared consistency: `true` only
under the verified single-node durable topology, `false` otherwise. A consumer
requiring `LockCapability::Linearizable` against a Sentinel or Cluster deployment
therefore fails startup with `CapabilityNotMet` — loudly, per
`cpt-cf-clst-fr-validation-startup-fail`, rather than getting a lock that a failover
can hand to two holders.

### 5.2 Renew and Release Are Token-Fenced

Both are Lua, both fence on the token, and both are one round trip:

- **`renew(ttl)`** — `lock_renew`: if `GET K == token` then `PEXPIRE K <ms>`, return
  1; else return 0 → `ClusterError::LockExpired`. Zero rows means either the lease
  lapsed or a successor took it; the consumer's response is the same in both cases
  (abort the critical section), which is why the SDK models it as one error.
- **`release()`** — `lock_release`: if `GET K == token` then `DEL K` +
  `PUBLISH <prefix>:e:l:<name> R`, return 1; else return 0. Returning 0 is not an
  error: it is the SDK's release-if-still-holder contract
  (`cpt-cf-clst-algo-distributed-lock-release-if-holder`) — a foreign holder's entry
  is left alone rather than deleted.

The token check is what makes the canonical `SET NX PX` pattern safe; a bare `DEL`
on release is the classic bug where a holder whose lease lapsed deletes its
successor's lock. Doing the compare in Lua rather than as `GET`-then-`DEL` is what
makes it atomic.

`PEXPIRE` on renew is absolute-from-now, computed by Redis's own clock, so a lease
deadline never depends on the client's wall clock and a skewed instance cannot hold a
lock longer than it was granted.

### 5.3 Blocking lock()

```
loop {
    register interest in `name` with the in-process waiter registry
    SET K token NX PX ttl  → OK? return LockGuard
    if past deadline → LockTimeout
    wait on (that registration resolving)
         OR jitter(min(PTTL K, heartbeat 250ms, remaining budget))
         OR shutdown
}
```

**The registration comes before the attempt, not after it.** A release landing in
the window between a failed `SET NX` and a registration made afterwards would be
missed entirely, and the waiter would then sit out a full delay for a lock that is
already free — which is precisely the "the notification was missed, not slow"
outcome `RD-LOCK-003` exists to catch. Registering first costs nothing: a waiter
that acquires on the attempt drops its registration unused, and the registry
deregisters on drop.

Three things wake a waiter, in order of preference: an explicit release (the holder
published to `<prefix>:e:l:<name>`, and the subscriber's fan-out task notifies the
in-process waiter registry), the lock's own remaining `PTTL` (read on the previous
attempt — a lease due in 40 ms should not be waited out for a 250 ms heartbeat), and
a 250 ms heartbeat as the safety net against a missed publish. A lost wake costs
latency up to the heartbeat, never correctness: the loop always re-attempts the
`SET NX` itself as the source of truth.

The delay is capped by a **third** bound beside those two — whatever is left of the
caller's own `timeout`. Without it a waiter with 5 ms of budget left sleeps 250 ms
and reports `LockTimeout` 245 ms late, which is visible to any caller that measures
its own budget.

`PTTL`'s two negative sentinels are read as opposites, and getting them the same way
round would be a real bug: **`-2`** (the key does not exist) means the name is free
*now*, so the waiter should retry immediately rather than sleep, while **`-1`** (the
key exists with no expiry) is not a lease this plugin wrote — every acquisition
carries `PX` — so there is no deadline to schedule against and the heartbeat is the
honest answer. An unreadable `PTTL` is not worth failing over: the heartbeat covers
it and the next attempt reports the outage properly if it is still there.

Retry delays carry **full jitter** — uniform over `[0, cap]`, drawing from zero
rather than perturbing a fixed delay — which is why `rand` is a direct dependency.
Without it, every instance contending for the same name retries on the same
deterministic schedule, and a hot lock turns the fleet's retries into synchronized
`SET NX` bursts, a thundering herd against the one key already under contention.
Drawing from zero means an occasional near-immediate retry, so the mean is half the
cap and a waiter blocked on a healthy lock costs about eight `SET NX`s a second
rather than four. A floor would be cheap and is deliberately absent: it would
reintroduce exactly the correlation the jitter exists to break.

The wake path itself is one blanket `PSUBSCRIBE <prefix>:e:l:*` rather than a
`SUBSCRIBE` per contended name. A waiter's interest lasts one loop iteration, so
per-name subscriptions would put a round trip either side of every retry and
re-introduce the ordering race the cache's registry needs a mutex to exclude (§4.3).
Releases are bounded by critical sections rather than by write rate, so what a
blanket pattern costs an uninterested instance is a hash-map miss.

`lock()` distinguishes three failure outcomes, because a caller's correct response
differs for each:

- `ClusterError::Shutdown` — checked before any lock work, so an acquisition
  arriving after `stop()` answers immediately instead of retrying a torn-down
  backend. `try_lock` takes the same check so the two agree.
- `Provider { ConnectionLost }` / `{ Timeout }` — the budget ran out while Redis was
  unreachable. Retried inside the caller's budget (`fred`'s reconnect is what
  carries a `lock()` through a Sentinel failover), then reported as itself rather
  than as a contention timeout.
- `LockTimeout` — the budget ran out while Redis was answering and saying the lock
  was held. Genuine contention, and only this case reports it.

No server-side blocking primitive is used. Redis has no "wait for this key" command
short of `BLPOP` on a companion list, which would need its own cleanup story and
would leak a list per lock name; the retry-plus-publish loop is both simpler and
cheap, since a blocked waiter holds no connection between attempts.

### 5.4 No RedLock, No Fencing Tokens

**RedLock is not implemented.** ADR-009 rates Redis Cluster as having "no known safe
config for this use case" and directs Redis-heavy stacks to route correctness-
critical coordination elsewhere; ADR-001 records RedLock's known correctness issues
directly ("use single-node + Sentinel instead"). Implementing a quorum protocol
across N independent Redis masters would add substantial machinery to obtain a
guarantee this platform's own ADRs decline to rely on, and would invite exactly the
misreading — "the Redis lock is safe now" — that §3.6's honest declaration exists to
prevent. A single primary is the arbiter; its declared consistency is the truth
about what that buys.

**No fencing tokens** (ADR-002). The `LockGuard` API has no `fencing_token()`, and
this plugin adds no Lua `INCR` counter to produce one — ADR-002 lists that omission
as a named benefit of the no-remote-I/O-in-the-critical-section rule.

**Nothing mechanically enforces that rule.** There is no
`no-remote-in-lock-critical-section` dylint lint in this workspace:
`tools/dylint_lints/` contains only `de12_documentation`, and there is no
`lint_utils` crate for such a rule to be built on. (The postgres plugin's
TESTING.md claims one exists, and its `Cargo.toml` comment references a
`lint_utils::is_in_postgres_cluster_plugin_path` that does not exist either — both
are worth disregarding rather than following.) ADR-002's rule therefore stands as a
design constraint **reviewers** enforce, and the benefit above is a benefit of the
constraint rather than of any tooling. Worth stating plainly, because a rule
believed to be lint-enforced gets less review attention than one known not to be.

### 5.5 Inspecting Locks (operators)

```
# What is held right now, and for how long
SCAN 0 MATCH 'cluster:l:*' COUNT 500
GET  cluster:l:tenant-42/rate-limit    # → the holder token
PTTL cluster:l:tenant-42/rate-limit    # → ms remaining on the lease
```

The token identifies an *acquisition*, not a human-meaningful instance: a random UUID
does not resolve to a pod, host, or process on its own. It is greppable, though — the
token is a log field on `cluster.lock.acquired` (§9), so a token read out of Redis
leads back to the acquiring instance's logs. Storing a `holder_instance` value
alongside it would duplicate identity already present in log context, and is
deliberately not done.

`SCAN`, never `KEYS` (§4.4) — including in operator runbooks, since a `KEYS
cluster:l:*` on a production instance blocks the server for the duration.

## 6. Lua Script Catalog

Seven scripts — five for the cache, two for the lock — loaded once at startup by
`SCRIPT LOAD` and invoked by `EVALSHA`.

**The startup load is one `SCRIPT LOAD` per script, not a per-primary broadcast, and
a `NOSCRIPT` recovers with `EVAL` rather than with a second load.** Those two are
one decision. A per-primary broadcast is unreachable here: `fred`'s only
broadcasting API, `script_load_cluster`, is gated behind the `sha-1` feature §3.1
excludes — it hashes the script client-side in order to have a SHA to return — and
`WithOptions` does not implement the Lua interface, so
`with_cluster_node(...).script_load(...)` is unavailable too, while `split_cluster()`
returns *unconnected* clients, which would mean a connection per primary opened and
closed just to warm a cache.

The recovery path answers the same need better than a broadcast would. On a
`NOSCRIPT` — the server restarted, its cache was flushed, or in cluster mode this is
simply the first call to reach a given shard — the call is retried as
`EVAL <source>` with the same key:

- **it is routed by the key**, so it necessarily reaches the node that reported the
  miss, where a keyless `SCRIPT LOAD` goes to whichever node the client picks and
  could easily warm a different one;
- **it is one round trip instead of two**, and it both executes the call and
  populates that node's script cache, so subsequent `EVALSHA`s on the shard hit;
- **it self-heals**, which a startup broadcast cannot: a primary added by a reshard,
  or restarted mid-life, has an empty script cache no startup-time load ever
  reached.

Each recovery is counted as `cluster_redis_script_reloads_total` and logged at
DEBUG. The recovery path is entered **at most once per call** and never re-entered —
that is the property that bounds it, and it is what the tests assert. A server
restarting under load answers `NOSCRIPT` to everything, and a policy that recovered
on each failure without a limit would turn one restart into a retry storm against a
server already struggling. Anything that goes wrong inside the recovery, including a
second `NOSCRIPT` that `EVAL` cannot actually produce, falls through the §10 mapping
to `Provider { Other }`.

Startup still issues `SCRIPT LOAD` for a reason the recovery does not cover: it is
where the SHA comes from, and it is the check that the server accepts each script at
all, rather than discovering a syntax error on the first write.

**Every script takes exactly one key via `KEYS[1]`.** That is what makes them
Cluster-correct: Redis routes an `EVALSHA` by its declared keys, so a single-key
script always lands on the owning node and `CROSSSLOT` is unreachable. The published
channel name is passed as an `ARGV` rather than as a second key, so the publish does
not make the script multi-key (`PUBLISH` is not slot-routed).

The invariant is asserted **structurally** rather than trusted: a unit test reads
the distinct `KEYS[n]` indices back out of each script's own Lua source and requires
the answer to be exactly `{1}`, so a future two-key script fails the build's tests
instead of production's `CROSSSLOT`. Reading it out of the source rather than
storing a hand-maintained key count is the point — a count gets copied along with
the script it has stopped describing, and a scan cannot. The rule is encoded a
second time in the executor's signature, which takes one key and cannot express two.

`ARGV[n]` conventions: `-1` in a TTL position means "no TTL" (`PERSIST`); an
**empty string** in a channel position means "do not publish", which is how
`watch_mode: disabled` reaches the write path (§4.3); versions are always compared
as strings and only ever incremented by `HINCRBY` (§2.2).

```lua
-- cache_put: KEYS[1]=entry  ARGV[1]=value  ARGV[2]=px_or_-1  ARGV[3]=channel_suffix
-- Returns: the new version, as an integer
local ver = redis.call('HINCRBY', KEYS[1], 'ver', 1)
redis.call('HSET', KEYS[1], 'v', ARGV[1])
if ARGV[2] == '-1' then redis.call('PERSIST', KEYS[1])
else redis.call('PEXPIRE', KEYS[1], ARGV[2]) end
if ARGV[3] ~= '' then redis.call('PUBLISH', ARGV[3], 'C') end
return ver
```

```lua
-- cache_put_if_absent: KEYS[1]=entry  ARGV[1]=value  ARGV[2]=px_or_-1  ARGV[3]=channel
-- Returns: 1 when created (version 1), or false when the key already existed
if redis.call('EXISTS', KEYS[1]) == 1 then return false end
redis.call('HSET', KEYS[1], 'v', ARGV[1], 'ver', 1)
if ARGV[2] ~= '-1' then redis.call('PEXPIRE', KEYS[1], ARGV[2]) end
if ARGV[3] ~= '' then redis.call('PUBLISH', ARGV[3], 'C') end
return 1
```

```lua
-- cache_cas: KEYS[1]=entry  ARGV[1]=expected_version  ARGV[2]=new_value
--            ARGV[3]=px_or_-1  ARGV[4]=channel
-- Returns: {1, new_version} on success
--          {0, current_version, current_value} on version mismatch
--          {0} when the key is absent (caller reports CasConflict{current: None})
local cur = redis.call('HGET', KEYS[1], 'ver')
if not cur then return {0} end
if cur ~= ARGV[1] then
    return {0, cur, redis.call('HGET', KEYS[1], 'v')}
end
local ver = redis.call('HINCRBY', KEYS[1], 'ver', 1)
redis.call('HSET', KEYS[1], 'v', ARGV[2])
if ARGV[3] == '-1' then redis.call('PERSIST', KEYS[1])
else redis.call('PEXPIRE', KEYS[1], ARGV[3]) end
if ARGV[4] ~= '' then redis.call('PUBLISH', ARGV[4], 'C') end
return {1, ver}
```

```lua
-- cache_compare_and_delete: KEYS[1]=entry  ARGV[1]=expected_value  ARGV[2]=channel
-- Returns: 1 when deleted, 0 on value mismatch or absent key (never an error)
if redis.call('HGET', KEYS[1], 'v') ~= ARGV[1] then return 0 end
redis.call('DEL', KEYS[1])
if ARGV[2] ~= '' then redis.call('PUBLISH', ARGV[2], 'D') end
return 1
```

```lua
-- cache_delete: KEYS[1]=entry  ARGV[1]=channel
-- Returns: 1 when the key existed, else 0
if redis.call('DEL', KEYS[1]) == 0 then return 0 end
if ARGV[1] ~= '' then redis.call('PUBLISH', ARGV[1], 'D') end
return 1
```

```lua
-- lock_renew: KEYS[1]=lock  ARGV[1]=holder_token  ARGV[2]=px
-- Returns: 1 when renewed, 0 when the token does not match or the key is gone
if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end
return redis.call('PEXPIRE', KEYS[1], ARGV[2])
```

```lua
-- lock_release: KEYS[1]=lock  ARGV[1]=holder_token  ARGV[2]=channel
-- Returns: 1 when released, 0 when we are no longer the holder
if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end
redis.call('DEL', KEYS[1])
redis.call('PUBLISH', ARGV[2], 'R')
return 1
```

Acquisition needs no script — `SET K token NX PX ttl` is already atomic (§5.1) — and
`get`/`contains`/`scan_prefix` need none either (§4.1). Seven across the two
primitives, then: five cache and two lock, and the standalone lock plugin (§3.5)
loads only the last two.

`lock_release` publishes unguarded where the five cache scripts guard on a non-empty
channel, and that asymmetry is correct rather than an oversight: the empty-channel
sentinel exists for `watch_mode: disabled`, which is a *cache* setting, and a
release wake has no equivalent switch to be turned off by (§3.3).

Two Redis-version notes: `PUBLISH` from a script is permitted on modern Redis and
flags the script as "may replicate", which is why scripts run only against primaries
(§1.3); and `redis.call` errors abort the script and surface to the caller, so no
script needs its own error handling.

## 7. Leader Election

Leader election uses the SDK default over the Redis cache, and §1.2's table records a
blocker that this section states plainly because it affects whether the "Redis-only"
deployment shape in `docs/DESIGN.md` §4.2 is expressible at all today.

**Leader election and the omit-default lock** — both are CAS defaults over this
cache, and both hit the same guard. `ClusterWiring::from_config` builds an omitted
`leader_election` with the **strict** `CasBasedLeaderElectionBackend::new(cache)?`,
which rejects an `EventuallyConsistent` cache — and it builds an omitted `lock` with
`CasBasedDistributedLockBackend::new(cache)?`, which shares that rejection. So a
profile binding `cache: { provider: redis }` and omitting either primitive **fails
startup** in every Redis configuration except the verified single-node durable one.
Naming both matters: an operator who fixed only the leader-election half would clear
one failure and land on the next.

That is arguably correct behaviour — ADR-009's whole point is that Redis leader
election is unsafe and should fail loudly rather than silently — but it means a
Redis-only deployment has exactly three options, and an operator hitting the error
deserves to find them documented here:

1. **Route leader election to a backend ADR-009's safety table rates linearizable**
   (K8s Lease being the one paired with Redis in `docs/DESIGN.md` §4.2's recommended
   production shape) in the same profile, and bind `lock: { provider: redis }`
   explicitly so the native lease-based lock serves it rather than the CAS default.
   This is ADR-009's own recommendation for Redis-heavy stacks, and the intended
   answer. Note that the native lock needs **no** opt-in over a weak cache: it is
   not a CAS default and no consistency guard stands in front of it — it declares
   `linearizable: false` and a consumer requiring otherwise fails capability
   validation, which is the check that belongs there.
2. **Verify a single-node durable topology** (`appendonly yes`,
   `appendfsync always`, no replicas) so the cache declares `Linearizable` and the
   strict constructors pass. Legitimate for dev and single-instance deployments;
   not a production HA answer.
3. **Opt in explicitly, per profile and per binding**, through the reserved provider
   name `default` (§13 D1):

   ```yaml
   leader_election:
     provider: default            # the SDK default over this profile's cache
     allow_weak_consistency: true # explicit acknowledgement; default false
   ```

   which routes to the SDK's `new_allow_weak_consistency`, and which the `lock`
   binding accepts identically. This is the honest way to express "I am running
   Redis-only in dev and I accept split-brain on failover", and it stays an explicit
   operator opt-in: **nothing in this plugin makes it implicit.** There is no
   plugin-side "pretend linearizable" path and no plugin-side default-backend
   construction bypassing the wiring, even though both crates were changed together.

   The sentinel is `provider: default` rather than a bare
   `leader_election: { allow_weak_consistency: true }`, because that shorter form
   cannot deserialize at all — `BackendBinding.provider` is a required field — and
   writing `provider: redis` instead fails differently, since this plugin registers
   no leader-election provider and the wiring answers "unknown leader_election
   provider". §13 D1 records the three homes the flag was weighed against and why
   this one won.

`LeaderElectionFeatures::linearizable` follows the cache's declaration either way:
waiving a constructor guard confers no guarantee, and a consumer declaring
`CacheCapability::Linearizable` against the same profile still fails startup with
`CapabilityNotMet` whatever the flag says.

## 8. Configuration

```rust
// `Debug` is hand-written, not derived — see below.
#[derive(Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct RedisClusterConfig {
    /// Connection URL(s). One entry for standalone; several for Sentinel or
    /// Cluster (`redis://`, `rediss://`, `redis-sentinel://`, `redis-cluster://`).
    /// Supports `${VAR}` / `${VAR:-default}` expansion via
    /// `toolkit_utils::var_expand`, resolved through `ctx.config_expanded()` —
    /// the same mechanism `libs/toolkit-db` uses for DB DSNs. A credstore-backed
    /// (`secret_ref`) path is deferred (§13 D5).
    #[expand_vars]
    pub url: String,

    /// Command-pool size. Default: 4.
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,

    /// Per-command timeout. Default: 5s. Enforced client-side by `fred`, so no
    /// command can block indefinitely once issued — the bound `stop()` and
    /// `RD-LIFE-009` rely on (§11, §12).
    #[serde(default = "default_command_timeout")]
    pub command_timeout_ms: u64,

    /// Prefix for every key and channel this plugin owns. Default: "cluster".
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Logical database index. Ignored in Cluster mode (db 0 only). Default: 0.
    #[serde(default)]
    pub database: u8,

    /// Operator hint for topology. Omitted → detected via `INFO replication`
    /// (§3.4). Drives the consistency declaration (§3.6).
    #[serde(default)]
    pub topology: Option<Topology>,

    /// Operator hint for write durability. Omitted → detected via
    /// `CONFIG GET appendonly|appendfsync`. A hint contradicted by a readable
    /// server config fails startup with `InvalidConfig` (§3.6).
    #[serde(default)]
    pub durability: Option<Durability>,

    /// Append `WAIT <n> <wait_timeout_ms>` to lock and CAS writes. Narrows the
    /// Sentinel failover window; per ADR-009 it does NOT upgrade the declared
    /// consistency (§3.6). Default: none.
    #[serde(default)]
    pub wait_replicas: Option<u32>,

    /// Timeout for the `WAIT` above, in milliseconds. Default: **1000**. `WAIT`
    /// rides the tail of a write that already carries a 5 s command timeout, and a
    /// replica that has not acknowledged within a second is not about to, so
    /// spending the whole command budget there would turn a narrowed failover
    /// window into a latency regression. A short count is surfaced rather than
    /// swallowed (§3.6), so the shorter bound loses no information. A value past
    /// `i64::MAX` — which `WAIT`'s signed timeout cannot express — fails startup
    /// with `InvalidConfig` rather than being clamped.
    #[serde(default = "default_wait_timeout")]
    pub wait_timeout_ms: u64,

    /// How cache watches are sourced (§4.3). Default: `publish`.
    #[serde(default)]
    pub watch_mode: WatchMode,

    /// When true, and the server's `notify-keyspace-events` lacks the flags
    /// `Expired` events need, issue one `CONFIG SET` to add them (§3.4).
    /// Default: false — mutating a shared server's global config is opt-in.
    #[serde(default)]
    pub manage_keyspace_notifications: bool,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Topology { Standalone, Sentinel, Cluster }

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Durability { FsyncAlways, FsyncEverysec, None }

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode { #[default] Publish, Disabled }
```

`RedisLockConfig` (standalone lock provider, §3.5) carries the same fields **minus**
`watch_mode` and `manage_keyspace_notifications`, which have
no meaning without the cache half — and `deny_unknown_fields` turns each of them
into a startup error there rather than an ignored key, since a lock-only binding
that sets `watch_mode` has misunderstood something and should hear so.

**The shared subset is duplicated field-for-field, not factored into an inner
struct.** Factoring it out is the obvious way to keep the two types from drifting
and it is not available: sharing an inner struct needs `#[serde(flatten)]`, and
serde refuses to derive `flatten` together with the `deny_unknown_fields` this
section and TESTING.md §2 both require. Flattening is
implemented by buffering every unmatched key and replaying it into the inner type,
so the outer type can no longer tell an inner field from an operator's typo — serde
rejects the combination at compile time rather than silently picking one.
`deny_unknown_fields` is the more valuable half by some distance: it is what turns
`pool_sise: 8` into a startup error instead of a silently-ignored key, and it is
also what makes a misplaced `allow_weak_consistency` on a *native* binding (§7) fail
loudly rather than being swallowed into an options map.

Two things hold the duplicated copies together instead. Every default lives in one
`default_*` function shared by both types, so a default cannot drift even in
principle; and a unit test reads each type's accepted field set back out of serde's
own `unknown field …, expected one of …` error and asserts the lock type is exactly
the cluster type minus the two cache-only fields. Deriving the guard from serde
rather than from a hand-written list is deliberate: a hand-written list would have
to be maintained by the same person who forgot to update the second type.

**`Debug` is hand-written on both types and masks `url`.** After `${VAR}` expansion
that field embeds a password, and a `{:?}` in a log line or a panic message would
leak it. This is not hypothetical for a config type: a `#[derive(Debug)]` config is
exactly the sort of thing that ends up in a startup error's context. Everything else
prints normally, following the postgres plugin's `REDACTED_DSN`.

**`validate()` runs before anything is opened**, rejecting a zero `pool_size`,
`command_timeout_ms`, or `wait_timeout_ms`. Each of those
zeros *removes* a bound rather than tightening it, which is why none can be left for
the backend to discover: `fred` reads a zero command timeout as "no timeout at all",
which silently deletes the property §11 and §12 rest on; and `wait_timeout_ms: 0`
makes `WAIT` block with no deadline. `pool_size: 0` is refused
here rather than by `fred` so the operator reads the name of the field they have to
change.

Operator YAML — the recommended production shape from `docs/DESIGN.md` §4.2, with
Redis serving cache and lock and K8s Lease serving the other two (the K8s plugin
being a separate follow-up):

```yaml
cluster:
  profiles:
    event-broker:
      cache:
        provider: redis
        url: "rediss://:${REDIS_PASSWORD}@redis-primary:6379/0"
        pool_size: 8
        topology: sentinel
      lock:
        provider: redis
        url: "rediss://:${REDIS_PASSWORD}@redis-primary:6379/0"
        wait_replicas: 1
      leader_election:
        provider: k8s-lease      # follow-up plugin; see §7
```

## 9. Observability

The plugin satisfies the versioned observability contract (ADR-004,
`docs/OBSERVABILITY.md`) verbatim and emits no catalog signal under a different name.
All signals carry `provider = "redis"`.

**Cache** — `RedisCache` is wrapped in the SDK's
`cluster_sdk::observability::InstrumentedCache` decorator, the supported path for the
cache signal set, so the full set comes for free: spans
`cluster.cache.{get,put,delete,contains,put_if_absent,compare_and_swap,watch,watch_prefix}`,
counter `cluster_cache_ops_total{provider,op,result}`, histogram
`cluster_cache_op_duration_seconds{provider,op}`.

**Lock** — `RedisLock` is a native implementation, so it emits lock signals directly
at each site (mirroring `CasBasedDistributedLockBackend::record_lock`): spans
`cluster.lock.{try_lock,lock,renew,release}`, counter
`cluster_lock_ops_total{provider,op,result}`, histogram
`cluster_lock_op_duration_seconds{provider,op}` via the injected
`cluster_sdk::observability::ClusterMetrics` sink.

**Shared** — every backend failure routes through
`cluster_sdk::observability::emit_provider_error`, incrementing
`cluster_provider_errors_total{provider,kind}` and logging `cluster.provider.error`
(ERROR) with `op`, `kind`, `message`, and the `key`/`lock` resource as a *field*.
Watch resets call `ClusterMetrics::watch_reset("cache")`, backing
`cluster_watch_resets_total{provider,primitive}`.

**Plugin-local metrics** (outside the ADR-004 catalog; adding signals is
non-breaking per ADR-004):

| Metric | Type | Why |
|---|---|---|
| `cluster_redis_watch_events_dropped_total{provider}` | counter | Events dropped to a full watcher buffer, i.e. the `Lagged` count. The signal that a consumer is too slow, which no catalog metric carries. Incremented once per dropped event — including a dropped `Lagged` that itself found no room — so it agrees with the `dropped` count the consumer eventually receives |
| `cluster_redis_subscriber_resubscribes_total{provider}` | counter | Subscriber reconnect-and-replay cycles. Pairs with `cluster_watch_resets_total` to separate "the subscriber flapped once" from "every watcher reset". **Combined plugin only** — it lives on the reconnect observer, which the standalone lock plugin does not spawn (§3.5); spawning a task purely to count was not worth it, so the gap is named rather than filled |
| `cluster_redis_script_reloads_total{provider}` | counter | `NOSCRIPT` recoveries (§6). A steady stream means something is flushing the script cache under the plugin; a burst at startup in cluster mode is just each shard being reached for the first time |
| `cluster_redis_evictions_observed_total{provider, primitive}` | counter | Evictions of this plugin's own keys (§3.7). `primitive` is `cache` or `lock`, and it is what makes the counter actionable rather than merely present: an evicted entry costs a re-read, an evicted lease means two holders believe they hold one lock, and an alert that cannot separate them has to treat every eviction as one or the other. **Never throttled**, unlike the WARN beside it: an alert has to see every eviction, while an operator reading the log needs one line naming a key plus how many others it stands for |
| `cluster_redis_connection_state{provider}` | gauge, 0/1 | Whether every connection in the command pool believes it is connected. The cheapest "is Redis reachable" panel |

**The gauge is sampled by a cancellable task, not reported by an OpenTelemetry
*observable* gauge.** The observable form is the obvious fit and the wrong one: its
callback is registered on the meter provider for the life of the process and is not
unregistered when the instrument handle drops, so it would outlive `stop()` holding
a pool clone, and a second plugin instance would register a second callback
reporting the same `{provider}` series — a conflict rather than a sum. A task is
cancellable, which is exactly the property that mismatch needs. It samples every
10 s, which is under any ordinary scrape interval, and records 0 on the way out so
the last sample a scrape can see says the pool is gone rather than leaving the
series stuck at its final live reading.

**Why these four are not `ClusterMetrics` calls.** The ADR-004 port has no gauge
method at all, so `cluster_redis_connection_state` could not go through it under any
naming; and the other three name Redis-specific subjects no catalog instrument
covers. So this plugin owns an OpenTelemetry meter directly, under its own
instrumentation scope, exactly as `postgres-cluster-plugin` does for its own
plugin-local gauge. `RedisSignals` holds both sinks and is threaded into the cache,
the lock, the watcher registry, and the subscriber fan-out as one value, so no
emitting site has to know which half a given signal belongs to — and one per plugin
rather than one per component, because the `provider` label is fixed at construction
and two sinks would mean two `cluster_cache_ops_total` instruments disagreeing about
one deployment.

`cluster_redis_evictions_observed_total` is where §3.7's eviction signal lands. The
catalog counter cannot carry it — `cluster_provider_errors_total` has no `op` label
and an eviction is not a `ClusterError` — and §3.7 records why folding it on as
`kind = "other"` would be worse than either alternative.

Keys, lock names, and holder tokens are **never** metric label values
(`METRIC_LABEL_ALLOWLIST`); they appear only as span attributes and log fields. That
rule is structural rather than observed here: the `ClusterMetrics` port takes no
such parameter, and the plugin-local instruments above attach only `provider`.

**Plugin-local log events**, all following `cluster.{primitive}.{event}`:

| Event | Level | Meaning |
|---|---|---|
| `cluster.provider.maxmemory_policy_unsafe` | WARN, once at startup | `maxmemory-policy` is not `noeviction`, so cluster keys can be evicted — locks handed to two holders, leader keys vanishing (§3.7). Alert on this |
| `cluster.provider.eviction_observed` | WARN, rate-limited **per primitive** | An `evicted` notification arrived for a key under this plugin's prefix, carrying `primitive` and the key. The above risk *materializing*. The highest-value signal this plugin emits. The two primitives have independent windows, so a cache eviction storm cannot suppress the one line reporting a lost lease (§3.7) |
| `cluster.provider.weak_consistency` | WARN, once at startup | The declared consistency is `EventuallyConsistent`, naming the detected topology and ADR-009's table. Not an error — the expected state for Sentinel and Cluster — but it must appear in the log of every deployment it applies to |
| `cluster.provider.consistency_asserted` | WARN, once at startup | An operator `durability`/`topology` hint was trusted because the server refused the verifying `CONFIG GET` (§3.6). If the hint is wrong, this line is the only trace |
| `cluster.provider.expiry_events_unavailable` | WARN, once at startup | `notify-keyspace-events` lacks the expiry flags and could not be set, so `Expired` watch events will not be delivered (§4.3). Promptness only |
| `cluster.provider.keyspace_notifications_set` | INFO, once | `manage_keyspace_notifications: true` took effect and the server's global flags were changed. A config mutation should never be silent |
| `cluster.provider.sharded_pubsub_available` | DEBUG, once at startup | The server supports `SPUBLISH`/`SSUBSCRIBE`, which v1 detects and records but does not use (§13 D3). DEBUG, not INFO: it is an input to a future decision, not an operator action |
| `cluster.provider.topology_unknown` | WARN, once at startup | `INFO replication` was refused, so the topology could not be detected and the conservative `EventuallyConsistent` is declared (§3.4). Distinct from the row above it because an operator debugging a surprise weak declaration needs to know whether the plugin *saw* a replica or saw nothing |
| `cluster.provider.durability_unknown` | WARN, once at startup | `appendfsync` reported a value this plugin does not recognize, so durability is treated as unverifiable (§3.4) |
| `cluster.provider.maxmemory_policy_unknown` | WARN, once at startup | `CONFIG GET maxmemory-policy` was refused, so the plugin cannot tell whether its keys can be evicted at all — the `unsafe` row above it is the answer being *known* to be bad, this one is it being unknowable (§3.7) |
| `cluster.provider.pool_close_timeout` | WARN, at shutdown | The command pool did not finish draining inside `POOL_CLOSE_TIMEOUT`; the client is shut down either way, but at least one connection may outlive `stop()` (§11) |
| `cluster.provider.task_drain_timeout` | WARN, at shutdown | Tracked background tasks — the per-guard lock tasks of §5 — did not finish inside their bound. Same shape as the row above and the same reason it is a WARN rather than an error: `stop()` proceeds regardless, and the line is the only trace that it proceeded over something |
| `cluster.provider.subscriber_lost` | WARN, once | The subscriber's reconnect policy is exhausted, so the connection is not coming back (§10). Every cache watch is closed terminally with a *retryable* error, and blocked `lock()` callers fall back to the jittered heartbeat, which costs latency rather than correctness (§5.3). The most severe runtime condition this plugin reports, and the reason it carries a name at all: a collector filtering structurally on `name` has to be able to match the one line announcing that outcome |
| `cluster.lock.acquired` | DEBUG | Lock name and holder token — the line that makes a token read out of Redis traceable to an instance (§5.5). A *log field* and never a metric label: a holder token is unbounded and would explode the cardinality of anything it touched |
| `cluster.watch.reset` | WARN | Catalog event, emitted from all three of §4.3's `Reset` sources: a subscriber reconnect, the fan-out's own stream lagging, and an unintelligible payload on a published channel |

**Every event carries its name twice**: `name:` set for a collector to match on
structurally, and the same name opening the human message. The default `tracing`
`fmt` layer prints the message and not the event name, so the structural form alone
would leave an operator tailing logs with prose and no way to tell which catalogued
event they are reading. The cost is one duplicated string constant per site.

The plugin emits no `cluster.leader.transition` of its own: leader election is the
SDK default over this cache and emits that itself.

**Startup failures are deliberately not routed through `emit_provider_error`.** A
`build_and_start` that fails returns its error to the wiring, which fails the gear's
boot with the text — it is not silent — and a counter incremented by a process that
then exits is not something a scrape will ever see. What *is* routed, because
nothing else would count it, are the two backend failures no catalogued operation
wraps: the lock's giveback of a lease it took but never handed out, and the
registry's `UNSUBSCRIBE` for a subscription whose last watcher went away. Both were
previously swallowed at DEBUG, which would have left a Redis failing every release
with no signal but a debug line.

## 10. ProviderErrorKind Mapping

Matches the platform mapping table (`docs/DESIGN.md` §4.1, Redis/`fred` column) and
extends it with the Redis-specific server replies that column does not enumerate.

| `fred` error / server reply | `ClusterError` / `ProviderErrorKind` |
|---|---|
| `ErrorKind::IO` | `ConnectionLost` |
| `ErrorKind::Timeout` | `Timeout` |
| `ErrorKind::Auth`, `NOAUTH`, `WRONGPASS` | `AuthFailure` |
| `ErrorKind::Backpressure` | `ResourceExhausted` |
| `ErrorKind::Config`, `ErrorKind::Url`, malformed URL | `InvalidConfig` — an unparseable URL is an operator config error, not a runtime backend fault, so it is *not* wrapped as `Provider` — an operator reading the error should be looking at their YAML, not at their server (`RD-LIFE-004`) |
| `OOM command not allowed when used memory > 'maxmemory'` | `ResourceExhausted` — retryable with backoff, and a pointer to §3.7 |
| `READONLY You can't write against a read only replica` | `ConnectionLost` — the routing landed on a demoted primary mid-failover. Retryable; `fred` re-resolves the topology |
| `LOADING Redis is loading the dataset in memory` | `ResourceExhausted` — retryable; a restarting server |
| `MASTERDOWN`, `CLUSTERDOWN` | `ResourceExhausted` — retryable |
| `NOSCRIPT` | Not surfaced: recovered once with a key-routed `EVAL` (§6), and `Other` if anything goes wrong inside that recovery |
| `CROSSSLOT` | `Other` — unreachable by construction (§6, every script is single-key); if it ever appears it is a plugin bug, not an operator one |
| `NOPERM` / ACL denial on a preflight command | Handled per §3.4's degradation table, not as an operation error |
| Any other `fred::Error` | `Other` |

Two notes on how those rows are actually recognized, neither of which is guessable
from the table. **A Redis error reply is not a distinct `ErrorKind`**: `fred`'s reply
parser switches on the first token and recognizes only `ERR`, `WRONGTYPE`, `NOAUTH`,
`WRONGPASS`, `MOVED`, `ASK`, and `CLUSTERDOWN`, so `OOM`, `READONLY`, `LOADING`,
`MASTERDOWN`, `NOSCRIPT`, and `CROSSSLOT` all arrive as `ErrorKind::Unknown` with the
reply text in the details. The server-reply rows above are therefore matched by
reading that leading code back out, and that check has to run *before* the
`ErrorKind` match or every one of them collapses into `Other`. And **`MOVED`/`ASK`
are left to `Other` on purpose**: `fred` follows redirections and re-resolves the
slot map itself, so one surfacing to a caller means the redirection could not be
followed, which no retry at this layer improves.

**The reconnect policy is this plugin's, not `fred`'s.** `fred` supplies none at all — `Builder::from_config` leaves it
unset — and everything above and in §4.3 about recovery presumes one: the
subscription replay, the `Reset` a watcher receives afterwards, and the terminal
close "once the reconnect policy is exhausted". Without one, a single dropped TCP
connection ends the client permanently, so every subsequent command fails forever
and every watcher is closed on the first blip. So both clients are built with an
explicit **bounded exponential** policy: 100 ms doubling to a 30 s ceiling, 20
attempts, roughly six minutes of outage ridden out — enough for a rolling restart or
a Sentinel failover.

**Bounded rather than unlimited is the load-bearing half.** The watchdog task that
closes every watch has exactly one signal, the connect handle resolving, and that
happens only when the policy gives up; unlimited retries would make it dead code,
and a watcher waiting on a server that is never coming back would wait forever,
told nothing. The policy also governs the *initial* connect, which is why §3.2 step 1
wraps that separately in a 10 s bound.

Subscriber connection loss therefore surfaces to watchers as `Reset` while `fred` is
still retrying, and as `Closed(Provider { ConnectionLost })` once the policy is
exhausted. **Every reconnect notification produces the `Reset`, including the
first, and the observer deliberately skips none.** The tempting reasoning for a skip
is that `fred` fires a notification on the initial connect, which it does — but the
observer's receiver is created *after* `build_and_start` has awaited that connect,
and a broadcast receiver only ever sees sends that follow its own subscription, so
that notification is already gone and a skip consumes the first **genuine**
reconnect instead. The failure that produces is quiet and severe: the first
subscription gap of a process's life delivers no `Reset` at all, and every watcher
keeps believing a stale view is current until a second reconnect happens along.
Acting on every notification is safe even if that ordering ever changes, since an
initial-connect notification would cost one registry-wide `Reset` at startup, and
`build_and_start` has not returned at that point, so no consumer has had the chance
to register a watch for it to reach.

## 11. Shutdown Sequence

`RedisClusterHandle::stop()` follows `docs/DESIGN.md` §3.13:

1. Cancel the `CancellationToken` shared by every task this handle owns. That ends
   the subscriber fan-out, the reconnect observer, the watchdog, and the
   connection-state sampler; it ends every per-guard lock task (§5); and it unparks
   every blocked `lock()` waiter, which returns `ClusterError::Shutdown` rather than
   `LockTimeout` (§5.3).
2. Send `CacheWatchEvent::Closed(ClusterError::Shutdown)` to every active watcher,
   dispatched directly against the registry before any task is awaited, so every
   watcher observes it before `stop()` returns (`cpt-cf-clst-fr-shutdown-revoke`).
   Going through the registry rather than through the fan-out is what makes that
   independent of whether the fan-out has noticed the cancel yet.
3. Drain the tracked guard tasks under `GUARD_DRAIN_TIMEOUT` (5 s), **before** the
   pool closes — a task caught mid-`renew` still needs a connection to finish on.
   The bound is generous rather than tight: each task selects on the token, so the
   only thing that can delay one is an in-flight command, which
   `command_timeout_ms` already bounds client-side.
4. Await the remaining task handles, then quit the subscriber client, then the
   command pool under a bounded `POOL_CLOSE_TIMEOUT` (10 s). `fred`'s `quit` drains
   in-flight commands, which is the behaviour worth having — severing the socket
   mid-command would turn an orderly shutdown into a spurious `ConnectionLost` — but
   that drain is only as fast as the server, so a supervisor's shutdown budget must
   not be spent on an unresponsive one. Both timeouts log rather than fail (§9);
   giving up on the wait still leaves the client shut down, and what is lost is only
   the guarantee that the server saw the `QUIT`, which an unresponsive peer was
   never going to give.
5. Set `self.stopped = true` last, so the ADR-006 `Drop` guard does not fire.

**The `Drop` guard cancels before it diagnoses**, and that ordering is deliberate
rather than incidental. Reaching `Drop` with `stopped == false` covers two cases: a
handle genuinely forgotten, which is the programming error the diagnostic exists to
shout about, and a `stop()` future *dropped part-way*, which is exactly what
`tokio::time::timeout(d, handle.stop())` does when its budget elapses — the
supervisor-level pattern this section recommends. In both, cancelling is what lets
the background tasks observe shutdown and exit instead of running on against a
handle nobody owns while each still holds a pool clone. In the second the caller has
not misused anything at all, so a diagnostic that ran *instead of* cancelling would
punish them for following the advice. The cancel must also precede the debug-build
`panic!`, or it would be unreachable in exactly the configuration tests run in.

**No remote cleanup**, per `cpt-cf-clst-fr-shutdown-ttl-cleanup`: held locks, leader
claims, and service registrations are not deleted on the way out — they lapse via
their TTL. This is cheap to honour here: a Redis lease expires by itself, so there is
no drain step on the way out, no statement that can half-succeed, and no
partial-cleanup failure mode to log or alert on. The cost is the TTL-bounded gap the
requirement already accepts: a
name held by a cleanly-stopped instance stays held until its lease expires. A
consumer wanting the gap closed calls `release()` before shutdown, which is what the
explicit-release contract asks of it anyway.

## 12. Risks / Trade-offs

**[Risk: eviction silently breaks every primitive]** Covered in §3.7. The plugin
warns at startup on a non-`noeviction` policy and warns again on each observed
eviction, but cannot prevent it: `maxmemory-policy` is server-wide and may be owned
by an unrelated tenant. **This is the top operational risk of running this plugin**
and the reason the deployment recommendation is a dedicated instance. Alert on
`cluster.provider.eviction_observed`.

**[Risk: Redis is not linearizable in any HA configuration]** §3.6 declares this
rather than mitigating it, per ADR-009. The consequence is not theoretical: an
operator who binds Redis cache and omits `leader_election` gets a startup failure
today (§7), and one who binds the native lock on Sentinel gets a lock that a
failover can grant twice. The mitigations are architectural, not code:
per-primitive routing, honest declaration, and startup capability validation.

**[Risk: `PUBLISH` cost in Redis Cluster caps the watch path well below the write
path]** Pre-7.0 Cluster broadcasts every `PUBLISH` to all nodes; ADR-001 puts the
ceiling around 12 500 publishes/sec on a 10-node cluster, an order of magnitude
below this plugin's write ceiling. A clustered deployment doing 10 000+ writes/sec
with watches enabled will hit the pub/sub ceiling first. Mitigations, in order:
`watch_mode: disabled` (polling polyfill, no publish at all); Redis 7 sharded
pub/sub (`SPUBLISH`/`SSUBSCRIBE`), which confines a publish to the key's own shard
and is the real fix — **decided as detect-and-record only in v1** (§13 D3), designed
together with D2's per-shard subscription work when a deployment actually hits the
ceiling; or keeping the high-write primitive on a non-clustered instance.

**[Risk: pub/sub delivery is fire-and-forget]** There is no resumption point, no
sequence number, and no replay. Every subscriber gap is total, which is why every
reconnect emits `Reset` and every consumer must re-read (§4.3). This is a permanent
property of Redis pub/sub, not a gap to close, and it is why ADR-003's uniform
lag/reset/close contract exists. Consumers needing delivery guarantees want the
event broker, not cluster watches.

**[Risk: the consistency declaration is computed once and never re-evaluated]**
§3.6. A single node that gains a replica under a running plugin keeps a
`Linearizable` declaration that is no longer true. Deliberately not solved:
re-declaring mid-flight would change capability answers after consumers have already
resolved against them, which the resolution model cannot express. The practical
mitigation is that topology changes are deployment events, and a deployment event
restarts the gear.

**[Trade-off: lock reclamation is strictly TTL-bounded]** §5.1. A crashed holder's
lock waits out its full TTL; nothing in the design detects the crash sooner, because
nothing watches the holder. Accepted: sub-TTL detection would mean a per-instance
heartbeat key, a TTL on that heartbeat, and a sweep to act on it — machinery native
expiry exists to avoid — for a promptness gain consumers can already buy by passing a
shorter `ttl`.

**[Trade-off: `Expired` events depend on server configuration]** §4.3. Without the
`notify-keyspace-events` expiry flags there are no `Expired` events at all. Costs
promptness only (both SDK defaults over this cache have timer-driven fallbacks;
an expired entry still reads as absent), and the state is logged loudly at startup.

**[Trade-off: hash-per-entry costs a little memory and a Lua call on
`put_if_absent`]** §2.2. The framed-string alternative would make `get` a bare `GET`
and `put_if_absent` a bare `SET NX`, but puts a u64 version through Lua 5.1's
doubles. Precision over micro-optimization; revisit only if profiling on the OAGW
path shows the hash is the bottleneck, which the ADR-001 envelope says it will not
be.

**[Design choice: no in-process read cache]** `get` is always a round trip. A local
cache would be per instance rather than shared, so at N instances it multiplies
staleness risk instead of amortizing it — each instance racing event-driven
invalidation against its own concurrent reads, so two instances could transiently
observe different values for one key. It would also silently reach the leader-election
default riding on this cache (§7). At ~0.15 ms per read
(ADR-001) there is little to buy anyway. A consumer wanting a local hot cache should
build one where it can reason about its own invalidation, not inherit one here.

**[Trade-off: no RedLock]** §5.4. Operators wanting multi-master lock quorum do not
get it from this plugin, by ADR decision rather than omission.

**[Property: every command is bounded client-side]** A server that freezes *after*
accepting a command is the case a server-side timeout cannot cover — the peer that
would enforce it is the one that stopped answering — and where the caller is a
background task, an unbounded command blocks `stop()` on that task's join. `fred`
enforces `command_timeout` (§8, default 5 s) on every command, so no operation in this
plugin is unbounded and `RD-LIFE-009`'s shutdown budget is a general claim rather than
an accident of timing. Stated as a property rather than left implicit, because it is
what lets §11 promise a bounded `stop()` at all.

## 13. Decisions (formerly Open Questions)

All six questions this design opened are decided. Each states the decision, what it
commits v1 to, and what would reopen it. **D1 is in scope for this change** — it
lands in `cf-gears-cluster` alongside the plugin rather than as separate work. The
rest are settled scope boundaries, deliberately narrow rather than deferred
indefinitely.

### D1 — Wiring-level opt-in for the weak-consistency CAS defaults

**Decided: reserve the provider name `default` to mean "the SDK default backend over
this profile's cache, with options", and carry `allow_weak_consistency: bool` on
that binding — for `leader_election` and for `lock` alike — routing to the SDK's
existing `new_allow_weak_consistency`. A `cf-gears-cluster` change rather than a
plugin one, delivered as part of this change.**

Without it, the strict `CasBasedLeaderElectionBackend::new(cache)?` means a profile
binding `cache: { provider: redis }` and omitting `leader_election` fails startup in
every Redis configuration except the verified single-node durable one (§7) — so the
`Redis-only` shape in `docs/DESIGN.md` §4.2 would be unreachable, and the
`K8s + Redis` shape depends on a K8s plugin that has not shipped.

Two properties of that shape are easy to get wrong, and both are load-bearing.

*It covers the lock as well as leader election.* The omit-default `lock` is built
with `CasBasedDistributedLockBackend::new(cache)?`, which shares the leader
default's consistency guard, so a leader-only flag clears one startup failure and
lands on the next — `RD-SPEC-004b` would elect a leader over the weak cache and then
die on the lock. The flag therefore covers both primitives.

*The flag needs a binding to live on, and `provider` is required.* The compact form —

```yaml
leader_election:
  allow_weak_consistency: true     # cannot deserialize
```

— cannot parse at all: `BackendBinding.provider` is a required field. Writing
`provider: redis` instead fails differently, since this plugin registers no
leader-election provider and the wiring answers "unknown leader_election provider".
Three homes were weighed:

| Option | Why not chosen |
|---|---|
| Make `provider` optional, absent ⇒ SDK default | Matches the YAML above exactly, but `BackendBinding` captures unknown keys into its options map, so a misspelled `providr: redis` would silently become an SDK default instead of a startup error. A config typo must not change which backend runs |
| A profile-level `allow_weak_consistency: bool` | Fixes leader and lock in one field with no new vocabulary, but is per-*profile*, and the constraint below requires per-binding |
| **Reserved `provider: default`** | **Chosen.** Per-binding as required, uniform across `leader_election`/`lock`, self-documenting in YAML, and typo-safe because `provider` stays required |

```yaml
leader_election:
  provider: default                # the SDK default over this profile's cache
  allow_weak_consistency: true     # explicit acknowledgement; default false
```

Two rules on the sentinel itself. It is **rejected on `cache`**, which is the
omit-default anchor and has no default to resolve to, with a naming error rather
than a confusing "unknown cache provider `default`". And it is **intercepted before
the registry lookup**, so a plugin that registered a provider literally named
`default` cannot shadow it.

One pleasant side effect worth keeping: `allow_weak_consistency` misplaced onto a
*native* binding — `lock: { provider: redis, allow_weak_consistency: true }` — lands
in this plugin's options map and trips its `deny_unknown_fields` (§8), so a
misplacement fails loudly instead of being quietly ignored.

Three constraints on that change, all following from ADR-009:

- **Default `false`.** Absent the flag, the current loud startup failure is the
  correct behaviour and must not change.
- **The flag is per profile and per binding**, never global — a deployment may
  legitimately accept weak leader election for one profile and require linearizable
  for another.
- **It routes to the SDK's existing constructor**, which already emits its own
  warning at construction. No new warning, no new consistency semantics, and no
  change to capability validation: a consumer that declares
  `CacheCapability::Linearizable` still fails startup regardless of this flag.

**The plugin still must not work around it** — no plugin-side "pretend linearizable"
path and no plugin-side default-backend construction bypassing the wiring, even
though both crates are being changed together. The flag is the operator's
acknowledgement; nothing in the plugin may make it implicit.

**Delivery.** Two crates, one change, in this order so neither half is ever on a
broken base:

1. `cf-gears-cluster`: the reserved `default` provider name, the options struct
   carrying `allow_weak_consistency`, its dispatch to `new_allow_weak_consistency`
   on both primitives, and its own tests over a stub weak cache — default-off still
   fails, flag-on constructs, **the leader flag alone still fails on the lock**, the
   reserved name is never looked up in the registry, and capability validation is
   unaffected. Those tests need no Redis and belong to the cluster crate's own
   suite, split across its config-parsing and builder-API test modules because the
   crate already partitions them that way.
2. The Redis plugin, whose `RD-SPEC-004` / `RD-SPEC-004b` (TESTING.md §4.6) then
   cover the same two paths end to end through real operator YAML against a real
   Redis container. `RD-SPEC-004b`'s profile binds `lock: { provider: redis }`
   explicitly alongside the opted-in `leader_election`, which is what keeps the
   lock's own guard out of the way of a scenario about leader election.

The duplication between (1) and (2) is deliberate and not redundant: (1) proves the
wiring dispatches correctly against any weak cache, (2) proves an operator's YAML
actually reaches it with the Redis backend bound.

### D2 — Native prefix watch in Redis Cluster

**Decided: v1 declares `features().prefix_watch == false` in cluster mode;
`watch_prefix` returns `Unsupported` there.** Per-shard `expired` subscriptions and
slot-migration re-subscribe are a follow-up, not v1 scope. Declaring `true` on an
unwritten, unverified code path is exactly the dishonest declaration ADR-009
§"Honest backend declaration" forbids, and a consumer relying on prefix watch
degrades cleanly to `PollingPrefixWatch` in the meantime (§4.3).

`RD-SPEC-010` (TESTING.md §4.6) is the gate: it asserts the `false` declaration, and
the follow-up that implements per-shard subscriptions **replaces** that test with
its positive counterpart rather than adding one beside it. Standalone and Sentinel
deployments are unaffected — they get native prefix watch in v1.

**The gate is built**, on the 3-node Cluster fixture of TESTING.md §4.1, so the
declaration this decision turns on is held mechanically rather than by review: a
change that made `prefix_watch` ignore cluster mode fails `RD-SPEC-010` rather than
reaching a clustered deployment that would otherwise start
trusting a prefix watch that silently drops every `expired` outside its own shard.
The unit test on the decision function over a constructed clustered cache remains
beside it — it pins the logic, and `RD-SPEC-010` pins the environment.

Note that only *prefix* watch is refused in cluster mode. Exact `watch(key)` keeps
working there, because plain `PUBLISH` is broadcast cluster-wide so `Changed` and
`Deleted` reach a subscriber on any node — it is `expired`/`evicted` that are
emitted only by the owning node, which makes `Expired` best-effort in cluster mode
exactly as it is on a server with notifications unavailable (§4.3). Refusing exact
watch as well would remove a working capability in order to describe one that is
partial.

### D3 — Redis 7 sharded pub/sub (`SPUBLISH` / `SSUBSCRIBE`)

**Decided: v1 detects availability and logs it; it does not use it and does not
carry it.** The preflight already reads the server version from `INFO server`
(§3.4), so v1 emits one DEBUG line at startup when the server is new enough — an
observation, not a behaviour change. Publishing stays on plain `PUBLISH`.

The finding is deliberately **not** carried on the preflight's outcome. Nothing
reads it: no code path branches on sharded pub/sub, so a field holding it would be
state the plugin maintains and never consults, and the change that first *uses* it
should decide where it lives alongside the per-shard subscription work D2 defers
rather than inherit a slot chosen a change earlier. A log line is the whole of what
"record it" needs to mean, and `RD-SPEC-014` asserts it in both directions —
present on a Redis 7 container, absent on a Redis 6 one, with `INFO commandstats`
confirming no `SPUBLISH`/`SSUBSCRIBE` is issued either way.

Switching to sharded publish is the real fix for the Cluster publish ceiling (§12)
but changes the channel-to-shard mapping, which means a subscriber must derive a
shard from a key prefix to know where to `SSUBSCRIBE` — that interacts directly with
D2's per-shard work and wants one design pass covering both, not two independent
ones. Reopened by: a clustered deployment measurably hitting the ~12 500
publishes/sec ceiling, at which point the two follow-ups are designed together.

### D4 — `watch_mode: keyspace`

**Decided: not implemented. `WatchMode` is `Publish | Disabled` and nothing else.**
The two shipped modes cover both ends — `publish` for correct, low-latency watches;
`disabled` for a deployment that wants no `PUBLISH` overhead and accepts polling. The
middle mode's only advantage is watches without publish cost, paid for with
duplicate-event coalescing whose test matrix (every command that touches a hash,
against every event type) is disproportionate to a benefit no deployment has asked
for. Reopened by: a real deployment that needs watches, cannot afford `PUBLISH`, and
cannot use polling — all three at once.

### D5 — Credential resolution for `url`

**Decided: `${VAR}` / `${VAR:-default}` env-var expansion via
`#[derive(toolkit_macros::ExpandVars)]` + `#[expand_vars]`, resolved through
`ctx.config_expanded()` — the mechanism `libs/toolkit-db` already uses for DB
credentials and DSNs. No plugin-local `secret_ref` field.**

When the OOP/credstore wiring contract is committed, the credential path reuses the
wiring's existing `BackendBinding.secret_ref` (`cluster/src/config.rs:83`) rather than
adding a plugin-level field of the same name at a different layer — two `secret_ref`s
at two layers is exactly the ambiguity that makes credential handling hard to audit.
Tracked by the platform OOP deployment design, not by this plugin.

### D6 — A shared Redis-access abstraction for the workspace

**Decided: no. This plugin uses `fred` directly and no `libs/`-level Redis
abstraction is created.** This is the workspace's first and only Redis consumer, so
an abstraction would have exactly one implementation and one caller — premature by
construction, and it would have to be redesigned the moment a second consumer's
needs differed. No dylint rule is added either, since there is nothing yet to
constrain (§3.1).

Reopened by: a second gear needing Redis directly. At that point the two consumers'
actual overlap is visible, and the crate-local no-`KEYS` check (TESTING.md §6) is
the first thing worth promoting to a workspace dylint rule — it exists as a source
scan today and inherits nothing to a second consumer, which is precisely the
limitation that makes it the natural first candidate rather than an argument for
writing the rule now.
