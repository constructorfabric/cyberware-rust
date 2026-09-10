# Testing Strategy — Redis Cluster Plugin

> **Status: implemented, with §8 as the register of what is not.** The suite is
> **280 tests** — 211 unit, three conformance suites, and the 63 Layer 3 scenarios
> of §4.2–§4.6 — running green in about ten seconds of test time via
> `make test-cluster-redis`, which is wired into `ci.yml`'s `integration` job. This
> is the test plan for `cf-redis-cluster-plugin`, paired with
> [DESIGN.md](./DESIGN.md), and it originated as the design deliverable of
> [#4373](https://github.com/constructorfabric/gears-rust/issues/4373).
>
> **Three of the four layers are delivered.** Layer 3 now includes the Sentinel and
> 3-node Cluster topologies of §4.1, so `RD-SPEC-002`, `008`, `009`, `010` and
> `RD-LOCK-014` run on every PR. What remains undelivered is §5's **fault-injection
> layer** — the platform has no harness for it — and §8 is where that is recorded
> rather than implied. Read §5 and the rows marked **Not built** as specifications,
> not as coverage.

> **Companion documents:**
> - [DESIGN.md](./DESIGN.md) — implementation design for this plugin
> - [TESTING-STRATEGY.md](../../../docs/TESTING-STRATEGY.md) — platform-wide cluster testing strategy (layers, tooling, CI cadence)
> - [Scenario Catalog](../../../docs/scenarios/README.md) — `SC-*` IDs referenced below

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Layer 1 — Unit Tests (in-crate)](#2-layer-1--unit-tests-in-crate)
- [3. Layer 2 — Conformance Suite](#3-layer-2--conformance-suite)
- [4. Layer 3 — Integration Tests (testcontainers)](#4-layer-3--integration-tests-testcontainers)
  - [4.1 Container Setup](#41-container-setup)
  - [4.2 Cache Integration Scenarios](#42-cache-integration-scenarios)
  - [4.3 Lock Integration Scenarios](#43-lock-integration-scenarios)
  - [4.4 Watch Integration Scenarios](#44-watch-integration-scenarios)
  - [4.5 Lifecycle Integration Scenarios](#45-lifecycle-integration-scenarios)
  - [4.6 Redis-specific Scenarios](#46-redis-specific-scenarios)
- [5. Layer 4 — Fault Injection (Toxiproxy) and Failover](#5-layer-4--fault-injection-toxiproxy-and-failover)
- [6. Static Analysis](#6-static-analysis)
- [7. CI Cadence](#7-ci-cadence)
- [8. Coverage Gaps and Follow-ups](#8-coverage-gaps-and-follow-ups)
  - [8.1 Deferred scope — specified but not covered](#81-deferred-scope--specified-but-not-covered)
  - [8.2 Known limits of what *is* covered](#82-known-limits-of-what-is-covered)
  - [8.3 Non-gaps, recorded so they are not re-investigated](#83-non-gaps-recorded-so-they-are-not-re-investigated)

<!-- /toc -->

## 1. Overview

The Redis plugin follows the four-layer pyramid from the platform-wide
[TESTING-STRATEGY.md](../../../docs/TESTING-STRATEGY.md):

```
L4  Fault injection (Toxiproxy) + Sentinel failover               — NOT BUILT (§5, §8)
L3  Integration tests (testcontainers Redis)                       — per-PR in this crate
      …  Sentinel / Cluster fixtures                               — per-PR in this crate
L2  Conformance suite (cluster-conformance crate)                  — driven by L3 container
L1  Unit tests (co-located, no external dependencies)              — every PR, sub-second
```

The conformance suite (L2) is the keystone: the shared, backend-agnostic scenario
bodies every cluster backend runs, executed here against a real Redis container.
Passing it is the primary signal that this plugin implements the
`ClusterCacheBackend` and `DistributedLockBackend` contracts.

Three concerns shape this plan beyond the shared pyramid, and account for most of what
is specific to it:

- **The consistency declaration is computed, so it must be tested as a function of
  the environment** (DESIGN §3.6). Most of §4.6 exists for this: the same plugin
  code must declare `EventuallyConsistent` against a replicated container and
  `Linearizable` against a verified single-node durable one, and must *fail startup*
  when an operator hint contradicts a readable server config. A plugin that
  declared the wrong thing here would defeat every capability check downstream and
  no conformance scenario would notice.
- **`Lagged` is producible here.** Redis pub/sub backpressure is ADR-003's canonical
  `Lagged` source, so `RD-WATCH-008` exercises a watch variant the contract has always
  specified but which needs a backend that can actually drop events under
  backpressure to test.
- **Eviction is a correctness scenario, not a capacity one** (DESIGN §3.7).
  `RD-SPEC-006`/`007` drive a real `maxmemory` eviction and assert both the warning
  path and the watch mapping, because this is the plugin's top operational risk.

## 2. Layer 1 — Unit Tests (in-crate)

Co-located with source (`src/**/*_tests.rs`). No Redis, no Docker; run with
`cargo test -p cf-redis-cluster-plugin --lib`.

| Module | What is tested |
|---|---|
| `config.rs` | serde round-trip for every field and every default (`pool_size` 4, `command_timeout_ms` 5000, `key_prefix` "cluster", `wait_timeout_ms` 1000, `watch_mode` `publish`, `manage_keyspace_notifications` false); `topology`/`durability`/`watch_mode` enum variants round-trip and an unknown variant is rejected; `deny_unknown_fields` turns an operator typo into `InvalidConfig`; `validate()` rejects each of the three zeros that would remove a bound rather than tighten it (DESIGN §8); `Debug` masks `url` on both types; `RedisLockConfig`'s accepted field set is exactly `RedisClusterConfig`'s minus the two cache-only fields, read back out of serde's own error rather than from a hand-written list (the drift guard the duplication needs, DESIGN §8) |
| `config.rs` — `${VAR}` expansion | **Half-coverable, permanently.** The `${VAR:-default}` branch is tested and the plain `${VAR}` branch cannot be: edition 2024 makes `std::env::set_var` `unsafe` and this crate is `#![forbid(unsafe_code)]`, so no unit test can set a variable for it to resolve — nothing in the cluster tree uses `set_var`, for the same reason. The `:-default` test drives the identical `expand_vars()` call down the other branch, and `provider.rs`'s unresolvable-variable test covers the failure path, so what is untested is the successful *lookup* alone. Recorded as a constraint rather than left as an apparent gap (§8.2) |
| `preflight.rs` | The consistency decision table (DESIGN §3.6) as a pure function of (topology, durability, readability): all five rows; unknown → `EventuallyConsistent`; a `durability: fsync_always` hint contradicted by a readable `appendfsync everysec` → `InvalidConfig` naming both values; the same hint with `CONFIG GET` unreadable → trusted, and flagged as asserted-not-verified. Parsing of `INFO replication` (`role:master` with and without `connected_slaves`, `role:slave`) and of `CONFIG GET` replies including the empty-value case |
| `scripts.rs` | Every script's SHA is loaded once and cached; a `NOSCRIPT` classification enters the `EVAL` recovery **at most once per call** and never re-enters it, which is the property that actually bounds it (DESIGN §6 — the earlier phrasing, "exactly one reload-and-retry", described a two-round-trip mechanism the recovery replaced); every script in the catalog declares exactly `{1}` as its `KEYS[n]` set, read back out of the Lua source rather than from a stored count, with a deliberately two-key script proving the guard is capable of failing; and every cache script carries as many empty-channel `PUBLISH` guards as it has `PUBLISH` calls, so a sixth mutation script cannot be added unguarded (DESIGN §4.3). All of it drives a `ScriptExecutor` test double, which is what makes the retry bound observable at all — it is only visible by counting calls |
| `connect.rs` | The URL-scheme topology mapping (DESIGN §3.4): a `redis-cluster://` URL reports `Cluster`, and a plain `redis://` reports **nothing** rather than `Standalone` — the trap being that reading one address as "no replicas" would let a Sentinel-managed primary reach the one row of DESIGN §3.6 that declares `Linearizable` |
| `wait.rs`, `shutdown.rs` | The `WAIT` short-count arm as an error rather than a silent success; what the operator's two config fields become — an absent `wait_replicas` is `Disabled`, a present one carries both values, and a `wait_timeout_ms` past `i64::MAX` is rejected as `InvalidConfig` at startup rather than clamped into a ~292-million-year deadline at the first write (DESIGN §3.6); and the `Drop`-ordering rule — that the shutdown token is cancelled *before* the diagnosis is returned, which is why the diagnosis is returned rather than emitted in place (DESIGN §11) |
| `observability.rs` | The eviction WARN's rate-limiting policy as a pure function of an elapsed-millisecond reading — first eviction always emits, a second inside the window is suppressed and counted, the suppressed count rides the next line that gets through — that an evicted entry and an evicted lease are both counted under their own `primitive` label, and the contract-name/`_total` rule the Prometheus exporter forces. The signal-per-outcome assertions live beside the primitives that emit them, over a recording `ClusterMetrics` double and an in-memory OTel reader |
| `cache/mod.rs` | Key construction: `<prefix>:c:<key>` and the channel `<prefix>:e:c:<key>` for the same input; `Ttl::Of`→ ms, `Ttl::Indefinite` → the `-1` sentinel the scripts expect, a sub-millisecond `Ttl` rounding *up* to 1 rather than down to the `0` that deletes, and a `Duration::MAX` saturating rather than wrapping into a short TTL — deliberate, and pinned as such, exactly as `px_millis`'s sibling test does for the lock; `CacheEntry` decoding from an `HMGET` reply shape, including both-`nil` → `None` and a version string at `i64::MAX` decoding losslessly (the precision claim DESIGN §2.2 rests on) |
| `cache/watch.rs` | The registry: per-key fan-out; one Redis pattern per distinct prefix with N in-process watchers on it; a dropped watcher pruned; `Reset` broadcast and counted, leaving every registration in place and every stream open, with a later `dispatch` asserted to still reach the reset watcher — the positive form, because a `Reset` that ended the stream would make `recv()` hang rather than fail; the terminal `Closed` still delivered to a full 64-slot buffer; no `Reset` ever delivered after a terminal `Closed`; a `watch()` arriving after `close_all` gets its terminal event immediately; a full buffer drops events, coalesces the count, and emits exactly one `Lagged { count }` when it drains. Payload mapping: `C`→`Changed`, `D`→`Deleted`, keyspace `expired`→`Expired`, keyspace `evicted`→`Deleted` (not `Expired` — DESIGN §3.7), unrecognized→`Reset` |
| `subscriber.rs` | The keyspace naming and its classifier (DESIGN §3.7): the pattern spans `<prefix>:*` rather than the cache's `<prefix>:c:` share of it — the regression that left an evicted **lock lease** unobservable — stays scoped to this plugin's prefix and database, and glob-escapes an operator prefix containing `[`. Classification sorts an entry to `Cache` and a lease to `Lock` by delegating to the two namers that *build* those names, declines a key under this prefix that neither owns (the event channels the pattern also matches), declines another deployment's prefix entirely, and — with the cache half absent, as in the standalone lock plugin — claims leases while disclaiming cache entries. The reconnect observer's arms, over `observe_reconnect`: a missed notification resets every watcher, moves `cluster_watch_resets_total`, **and** emits the catalogued `cluster.watch.reset` carrying the missed count — asserted together, since the defect was a counter moving with nothing in the log to explain it — while a closed stream ends the observer and resets nothing |
| `lock/mod.rs` | Key/channel construction; the holder token is a fresh v4 UUID per acquisition and two acquisitions differ; the `SET NX PX` argument assembly; the three-outcome classification of a failed `lock()` (`Shutdown` / `Provider` / `LockTimeout`, DESIGN §5.3) as a pure function of (elapsed, last error); and DESIGN §5.3's third bound over `park`, driven against a pool that is built but never `init()`ed — the `PTTL` future is then pending forever, which is a stalled server without a server, and `tokio::time::pause()` supplies the clock so the assertion is on virtual time |
| `lock/waiters.rs` | Register/notify/deregister-on-drop; a notification for a name with no waiter is a no-op; a waiter dropped mid-wait leaves nothing behind; the wake delay is `min(PTTL, heartbeat)` and is jittered (asserted as a range plus non-identity across draws, not as an exact value) |
| `redis_error.rs` | The full §10 mapping table, row by row, including the ones the parent `docs/DESIGN.md` §4.1 column does not list: `OOM`→`ResourceExhausted`, `READONLY`→`ConnectionLost`, `LOADING`/`MASTERDOWN`/`CLUSTERDOWN`→`ResourceExhausted`, `NOAUTH`/`WRONGPASS`→`AuthFailure`, malformed URL→`InvalidConfig` (**not** a `Provider` error — a config fault must not read as a backend fault) |
| `provider.rs` | `ClusterCacheProvider::provider()` and `ClusterLockProvider::provider()` both return `"redis"`; `build_cache`/`build_lock` each return `InvalidConfig` for a malformed URL and for an unknown option key; `build_lock` neither receives nor consults a cache backend (the SDK's "non-cache providers do not receive the cache backend" contract) |

No Redis command is executed at layer 1. All command and script behaviour is
covered at layer 3.

Test modules that would exceed 100 lines live out-of-line in a `*_tests.rs` file
reached by `#[path = "..."]`, because `dylint.toml`'s DE1101 caps an inline test
block at that and the cluster tree is not in its excluded paths — the same
arrangement the Postgres plugin uses. §6's mechanical source check is the one L1
module that is crate-level rather than co-located, since the rule it enforces is
about the whole of `src/`.

## 3. Layer 2 — Conformance Suite

`cf-gears-cluster-conformance` is a `[dev-dependencies]` entry;
`tests/conformance.rs` wires the layer-3 container fixture into every entry point.
Each suite goes through one `run_*_conformance(factory, time)` call whose async
factory returns a `cluster_conformance::ScenarioBackend` that **owns the plugin
handle** and stops it via teardown before the next scenario is built — mandatory,
not cosmetic: `RedisClusterHandle`/`RedisLockHandle` panic on drop if never
`stop()`ed (DESIGN §3.2's ADR-006 guard).

```rust
// tests/conformance.rs

#[tokio::test]
async fn cache_conformance() {
    run_cache_conformance(
        || async {
            let handle = RedisClusterPlugin::builder(test_config())
                .build_and_start()
                .await
                .expect("plugin starts against test container");
            let cache = handle.cache();
            ScenarioBackend::with_teardown(cache, async move { handle.stop().await })
        },
        // A real backend gets real (bounded) time, never a paused clock: `fred`'s
        // own command timeouts and reconnect timers run on the same runtime and a
        // paused/auto-advancing clock fires them spuriously.
        TimeControl::Real,
    )
    .await;
}

#[tokio::test]
async fn lock_conformance() {
    run_lock_conformance(
        || async {
            // The standalone lock-only path (DESIGN §3.5) — the same one
            // ClusterLockProvider::build_lock uses in production. The runner
            // hands the factory no cache (`F: Fn() -> Fut`), which matches this
            // plugin's shape exactly: the native lock builds its own pool and
            // never rides a cache, so there is no shared-pool shortcut to take.
            let handle = RedisLockPlugin::builder(test_lock_config())
                .build_and_start()
                .await
                .expect("standalone lock plugin starts");
            let lock = handle.lock();
            ScenarioBackend::with_teardown(lock, async move { handle.stop().await })
        },
        TimeControl::Real,
    )
    .await;
}
```

`leader_conformance` is wired the same way, over
`CasBasedLeaderElectionBackend` on this
plugin's cache (DESIGN §7) — and the leader suite carries a wrinkle no other backend
has:

**The leader suite runs against a single-node durable container, not the default
one.** `CasBasedLeaderElectionBackend::new` is the strict constructor and rejects an
`EventuallyConsistent` cache, so on the default container it would fail to construct
at all. `tests/common` therefore provides a second fixture
(`start_redis_durable()` — `appendonly yes`, `appendfsync always`, no replicas) whose
cache declares `Linearizable`, and the leader suite uses it. The *negative* half —
that the strict constructor really does refuse the ordinary container's cache — is
`RD-SPEC-004`, and it is the more important of the two assertions.

This stays the arrangement despite DESIGN §13 D1's wiring flag landing in the same
change: the flag makes weak-cache leader election *expressible by an operator who
opts in*, not correct, so the conformance suite — which asserts correctness
properties like single-leader-among-contenders — keeps running on the fixture whose
declaration supports them. Pointing the suite at a weak cache via the flag would
produce a suite that asserts guarantees the backend does not claim, which is a test
that fails for the right reason at the wrong layer. The flag's own path is
`RD-SPEC-004b` instead.

Each scenario runs on a shared container under **both** isolations rather than
either: its own logical Redis database *and* its own `key_prefix`. Database
isolation alone runs out at 15, and prefix isolation alone leaves a scenario able to
see another's keyspace via `scan_prefix` if a prefix bug exists — which is exactly
the class of bug `RD-CACHE-006` looks for. Doing both is strictly stronger than
either, costs nothing, and removes the branch: the suites with more than 15
scenarios need no special case, and no suite needs a container per scenario. The
database cycles `1..=15` rather than from 0, so nothing lands where a
stray client would connect by default and every scenario's `expired` notifications
arrive on a non-zero `__keyspace@<n>__` — the off-by-one-database bug `RD-SPEC-012`
exists to catch would otherwise be invisible here.

A fresh plugin instance is built per scenario rather than shared across them, and
that is required rather than tidy: the scenarios reuse key and lock names, and this
plugin's `stop()` deliberately *leaves* held leases to expire rather than handing
them back (`cpt-cf-clst-fr-shutdown-ttl-cleanup`, `RD-LOCK-013`), so a lease held
past one scenario's teardown would still be held when the next asked for the name.

**One conformance scenario does not run here at all.** `TimeControl::Real` is
mandatory against a real backend — `fred` runs its own command timeouts and
reconnect backoff on the same runtime, and `tokio`'s paused clock auto-advances to
the next pending timer whenever nothing is polling, which while a socket read is
parked is constantly, so every command would report `Provider { Timeout }` against a
healthy server. The cost is that the shared runners skip `SC-LEAD-006`,
which forces a *missed* renewal that a healthy real backend
never exhibits by waiting. It maps to fault injection, which §8 defers; §8.2
records it, because a green suite says nothing about a scenario it skipped.

**Capability-gated assertions.** The suite reads `features()` and `consistency()`
before running scenarios. For this plugin they vary by fixture, which is itself
worth asserting:

- Default (replicated/non-durable) container: `CacheConsistency::EventuallyConsistent`
  → linearizability-dependent scenarios are skipped, and the
  `CacheCapability::Linearizable` mismatch scenario runs (expects `CapabilityNotMet`).
- Durable single-node container: `Linearizable` → single-leader and
  lock-contention correctness scenarios run.
- `watch_mode: publish`, non-clustered: `CacheFeatures::prefix_watch == true` →
  the native-prefix-watch scenarios run and the `PrefixWatch` mismatch scenario does
  **not**. This is the first backend for which that is true, so it is also the first
  real exercise of the capability check's *positive* branch.
- `watch_mode: disabled` or cluster mode: `prefix_watch == false` → `watch_prefix`
  returns `Unsupported`.
- `LockFeatures::linearizable` tracks the cache declaration per fixture.

**Routing conformance is out of scope.** `run_routing_conformance` does not exist in
`cluster-conformance` and per-primitive routing
(`cpt-cf-clst-fr-routing-per-primitive`) is wiring-crate logic — `ClusterWiring::from_config`
dispatching through `ProviderRegistry` — not backend logic any plugin implements or
could conformance-test in isolation. It belongs to the `cluster` gear's own suite, plus
this plugin's one routing-adjacent integration test (`RD-LOCK-008`).

**`SC-SCOP-001..006` need no per-backend conformance function.** The scoping wrappers
(`ScopedCacheBackend` and siblings) are pure decorators: each holds an
`Arc<dyn ClusterCacheBackend>` and only ever calls the generic trait interface, so the
wrapped backend could be Redis or a test stub and the prefix apply/strip/compose logic
behaves identically. They have their own SDK-level unit tests against recording stubs,
and running the same string manipulation again through a Redis container would reach no
Redis-specific code path. There is not even a key-length interaction to cover: Redis
keys have no size ceiling composed scope prefixes could push a key past (DESIGN §2.1).

## 4. Layer 3 — Integration Tests (testcontainers)

### 4.1 Container Setup

`testcontainers-modules` carries a `redis` feature, enabled on its workspace entry.
The fixtures are **one parameterized `RedisRecipe`** — a set of `redis-server`
command-line flags plus an image tag — behind seventeen named wrappers covering
**ten container shapes**, eight of them single-node and two multi-node.

There are ten rather than one or two for two separate reasons. Eight of them are
single-node because this plugin's declared capabilities and several of its
warnings are functions of *server configuration* rather than of plugin logic, so a
scenario about one of them needs a server configured for it. The other two are
multi-node because topology is itself one of those inputs: the consistency
declaration reads `INFO replication`, and cluster mode changes how keys route and
what the cache can honestly claim about prefix watching. Several wrappers share a
shape and differ only in the config type they hand back — a `RedisLockConfig`
rather than a `RedisClusterConfig`, say — which is why there are more wrappers than
shapes:

| Shape | Needed by |
|---|---|
| stock (no AOF) — declares `EventuallyConsistent` | most of §4.2–§4.5, `RD-SPEC-001` |
| durable single node (`--appendonly yes --appendfsync always`) — declares `Linearizable` | the leader conformance suite (§3), `RD-SPEC-003` |
| no `notify-keyspace-events` | `RD-SPEC-005`, `RD-SPEC-005b`, `RD-LOCK-009` |
| `--maxmemory-policy allkeys-lru` with **no** ceiling | `RD-SPEC-006` |
| tiny `--maxmemory` plus `allkeys-lru`, so writes really evict | `RD-SPEC-007`, `RD-SPEC-007b` |
| the same, returning a `RedisLockConfig` **and its URL** | `RD-LOCK-015` — the standalone plugin owns no cache to write filler through, so the pressure comes from a raw client |
| `appendfsync everysec` | `RD-SPEC-011`'s contradiction half |
| the same, plus a `CONFIG`-denied ACL user | `RD-SPEC-011`'s asserted-not-verified half |
| Redis 6 | `RD-SPEC-014`'s negative half |
| **Sentinel**: primary + replica + Sentinel, one container, ports mapped 1:1 | `RD-SPEC-002`, `RD-LOCK-014` |
| **Cluster**: 3 primaries, one container, ports mapped 1:1 | `RD-SPEC-008`, `RD-SPEC-009`, `RD-SPEC-010` |

Two of those shapes are worth singling out. The unsafe-policy and evicting shapes
are deliberately **separate**: `RD-SPEC-006` is about the startup WARN firing on a
policy that *could* evict, and keeping the ceiling off means it asserts that without
also being at the mercy of a real eviction landing mid-test. And the ACL user is
declared as a **server flag** (`--user limited on '>pw' '~*' '&*' +@all -config`)
rather than with `ACL SETUSER`, which is not merely tidier: `ACL` sits on a `fred`
interface the feature list leaves out, so no build of this crate can issue one. The
`&*` in that flag is load-bearing — without it the user cannot subscribe, and the
plugin opens its subscriber before it reaches the durability check.

One retrying start-and-map-port sequence serves all the single-node shapes, which is
the reason for a recipe rather than a function each: a duplicated retry loop is the
kind of thing that gets fixed in one copy.

#### The two multi-node topologies, and why each is one container

`start_redis_sentinel` (primary + replica + Sentinel) and `start_redis_cluster`
(3 primaries) each run **every node as a process inside a single container**, with
host ports mapped **1:1**. That is not a shortcut; it is the only arrangement that
works from this side of the Docker network boundary.

Both topologies *advertise an address to the client*. Sentinel answers
`get-master-addr-by-name` with the primary's address; a cluster node answers a
wrong-slot command with `MOVED <slot> <addr>`, which the client is required to
follow. Under testcontainers' usual random port mapping the address a node knows
about is its container port, and the address the host can reach is the mapped port —
so every advertised address points somewhere the test process cannot go. Mapping
1:1 collapses the difference: `127.0.0.1:7000` means the same endpoint inside the
container and outside it, so one advertised address is correct for both the nodes'
gossip and the test's client. Separate containers on a Docker network cannot achieve
that, because an address that reaches a peer across the network is not one the host
can dial.

The ports are therefore chosen by the fixture rather than by Docker, from a probed
run of free ports in a fixed window (the ephemeral range is unusable: a cluster
node's bus port sits `10000` above its data port and would overflow `u16`).

**What the arrangement costs is failover.** The processes share a failure domain, so
killing the container takes the quorum with the primary. This fixture can watch a
replica *leave* — which is what `RD-LOCK-014` needs — and cannot observe a failover,
which is why `RD-FAULT-005..007` stay deferred to a harness with separately killable
nodes (§8.1).

Every fixture that wants keyspace notifications sets **`notify-keyspace-events Kxe`**
by container flag rather than by `CONFIG SET`, so the *default* test posture matches
a well-configured production server — a plugin that only works after mutating a
server-wide setting is not what §4 means to exercise. `Kxe` is the minimal correct
set: `K` plus `x` yields `expired` and `K` plus `e` yields `evicted`, the two events
no plugin code can publish for itself, while anything wider adds notifications
server-wide that unrelated tenants pay for and this plugin never reads
(DESIGN §4.3).

The fixtures deliberately spell that constant out rather than importing the plugin's
own `REQUIRED_KEYSPACE_FLAGS`: a fixture configured from the same constant the
plugin checks against would agree with the plugin by construction, and `RD-SPEC-005`'s
whole subject is what happens when the two disagree. The no-notifications shape is
how that degradation path is covered, and `RD-SPEC-005b` takes its **own** container
rather than sharing one — it is the single scenario that issues
`CONFIG SET notify-keyspace-events`, and that setting is server-wide and outlives the
test, so a shared container would make the pair order-dependent.

### 4.2 Cache Integration Scenarios

These mirror the conformance scenarios (§3) with Redis-specific assertions on the
actual keyspace.

| ID | Scenario | What it verifies |
|---|---|---|
| `RD-CACHE-001` | `put` + `get` round-trip | Value and version stored and retrieved; the underlying key is a **hash** with exactly the fields `v` and `ver` (the DESIGN §2.2 encoding, asserted at the server so a future re-encoding is a test failure rather than a silent wire change) |
| `RD-CACHE-002` | Version increment monotonicity | Each `put` increments `ver` by exactly 1; `put_if_absent` creates at 1; `HGET ver` read directly from Redis agrees with the `CacheEntry` the plugin returned |
| `RD-CACHE-003` | `compare_and_swap` atomicity under concurrent writers | 20 concurrent tasks CAS the same key from the same expected version; exactly one succeeds and 19 get `CasConflict`, each carrying a populated `current` (the same-round-trip conflict payload, DESIGN §4.1) |
| `RD-CACHE-004` | Native TTL expiry | An entry with a 500 ms TTL is absent from `get`/`contains` after it lapses **with no reaper running anywhere** — Redis is the only expiry mechanism (DESIGN §4.2). `PTTL` confirms the TTL was set from the request, not inherited |
| `RD-CACHE-005` | `compare_and_delete` survives a version reset | Delete-and-recreate resets the version to 1; `compare_and_delete` with the *old* value is a no-op and the new holder's claim is intact (`[cluster-cache-version-reset-caveat]`, DESIGN §2.3) |
| `RD-CACHE-006` | `scan_prefix` correctness and isolation | Keys under the prefix returned with the plugin's own `<prefix>:c:` stripped; expired keys excluded; keys under a *different* `key_prefix` in the same database excluded (the isolation half — see §3 on why prefix isolation is the fallback); `KEYS` never issued (asserted via `MONITOR` or `INFO commandstats`) |
| `RD-CACHE-007` | `Ttl::Indefinite` clears an existing TTL | `put` with a TTL, then `put` with `Indefinite`: `PTTL` reports −1 (persistent), not the old deadline. The SDK's two-valued `Ttl` says a write always states the TTL rather than preserving it |
| `RD-CACHE-008` | `put_if_absent` on a live entry does not overwrite | Returns `None`, and the stored value and version are untouched — the contract leader election's `claim` depends on |
| `RD-CACHE-009` | Command timeout is bounded client-side | With `command_timeout_ms: 200` against a container paused mid-command, a `get` returns `Provider { Timeout }` within a second rather than hanging. The bound DESIGN §12 records as a property: `fred` enforces `command_timeout` on every command, so no operation is unbounded once issued |
| `RD-CACHE-010` | Every script is single-key at runtime | Each of the **seven** cache and lock scripts is invoked and `INFO commandstats` / a `MONITOR` capture confirms it was dispatched with exactly one key. The build-time structural check (`scripts.rs`, §2) plus this runtime one are what make `CROSSSLOT` unreachable (DESIGN §6) — and `RD-SPEC-008` exercises the routing that invariant exists for, on a real cluster |

### 4.3 Lock Integration Scenarios

| ID | Scenario | What it verifies |
|---|---|---|
| `RD-LOCK-001` | `try_lock` acquires and `release` frees | The lease key exists with the holder token as its value and a `PTTL` in range; a second `try_lock` — **from the same instance**, refused by the same `SET NX` a foreign one would be — returns `LockContended`; after `release` the key is gone and the name is acquirable |
| `RD-LOCK-002` | `lock` with timeout | A blocked `lock` returns `LockTimeout` (not `Provider`) after the budget elapses, and leaves nothing behind: the name is acquirable the moment the holder releases |
| `RD-LOCK-003` | `lock` wakes on an explicit release | A blocked `lock` acquires well under the 250 ms heartbeat after the holder calls `release`, confirming the publish-driven wake. A wake measured at ~one heartbeat means the notification was *missed*, not merely slow, which is what makes this assertion sharp rather than a latency check — and it is why DESIGN §3.2 step 4 awaits the initial subscribe before `build_and_start` returns: a release landing in that startup window would otherwise have no subscriber |
| `RD-LOCK-004` | An expired lease is reclaimed with no reaper and no cooperation | A holds a 500 ms lock and never renews or releases; B acquires it purely on Redis expiry. The sharpest statement of what native TTL buys: there is no sweep in this plugin to be stalled (DESIGN §5.1). A's subsequent `renew` reports `LockExpired` |
| `RD-LOCK-005` | `renew` extends the lease | `PTTL` after `renew(new_ttl)` reflects the new deadline; the lock is still held past the original one |
| `RD-LOCK-006` | `renew` and `release` are token-fenced | A's lease lapses and B acquires the same name. A's `renew` → `LockExpired`; A's `release` → a no-op that leaves **B's** key intact (the release-if-still-holder contract, DESIGN §5.2). The one test that would fail on the classic bare-`DEL`-on-release bug |
| `RD-LOCK-007` | 20 concurrent local acquirers, at most one holder | Exactly one succeeds, 19 get `LockContended`. Kept distinct from `RD-LOCK-011` even though both exercise `SET NX`: that local and cross-instance contention are arbitrated *identically* is the claim worth holding both halves to, and a regression adding a local short-circuit shows up here first |
| `RD-LOCK-008` | End-to-end YAML routing: lock on Redis, cache elsewhere | Via `ClusterWiring::from_config` with `cache: { provider: standalone }` and `lock: { provider: redis, url: … }`: the resolved profile's lock writes a real lease key in the container while its cache is the in-process standalone one. Confirms `ClusterLockProvider` registration makes `provider: redis` resolvable for `lock` independently of `cache` (DESIGN §3.5) |
| `RD-LOCK-009` | Standalone `RedisLockPlugin` needs no keyspace notifications | Against a container started **without** `notify-keyspace-events`, `RedisLockPlugin::build_and_start` returns `Ok` and `try_lock`/`renew`/`release`/blocking `lock` all behave identically to the combined plugin's lock (DESIGN §3.5). It does subscribe a keyspace pattern, for the eviction signal alone (`RD-LOCK-015`), and this is what pins that nothing it needs to *operate* rides on that subscription: with the notifications absent the plugin loses the eviction report and nothing else |
| `RD-LOCK-010` | Held locks consume no connections | With `pool_size: 2`, hold 12 locks at once: all 12 succeed, all 12 keys exist, and a `renew` still gets a connection while they are held |
| `RD-LOCK-011` | Two instances cannot hold the same lock | Two independent plugin instances on one server: A acquires, B's `try_lock` returns `LockContended`, exactly one key exists and its value is A's token, and B acquires as soon as A releases. The cross-replica guarantee the primitive rests on |
| `RD-LOCK-012` | `lock()` after `stop()` answers `Shutdown` immediately | After a clean `stop()`, `lock(name, ttl, 30s)` returns `ClusterError::Shutdown` in well under a second rather than retrying a torn-down backend for its whole budget and reporting `LockTimeout` — which would leave a caller unable to tell "someone else holds it" from "this backend is gone". `try_lock` asserted alongside, since both take the same pre-work check |
| `RD-LOCK-013` | `stop()` leaves held leases to expire, and says so | Hold three locks with a 10 s TTL, `stop()`, then assert the three keys are **still present** and then expire on their own deadlines. Deliberately asserts that leases are *left behind*: `cpt-cf-clst-fr-shutdown-ttl-cleanup` forbids best-effort remote cleanup on shutdown, and a lease needs no drain because it reaps itself (DESIGN §11). A future "tidy up on stop" change would fail here, which is the point |
| `RD-LOCK-015` | The **standalone** plugin observes an evicted lease | The half of DESIGN §3.7 that a cache-scoped pattern cannot reach at all rather than merely narrowly: without its own keyspace subscription a lock-only deployment has every lease evicted out from under it and reports nothing — and it is the shape likeliest to be pointed at a *shared* Redis, having no cache working set to argue for its own instance. Memory pressure comes from a **raw client**, since there is no cache here to write filler through, which is why the fixture returns a URL. Asserts the WARN, its `primitive="lock"`, and the counter. Asserts no behaviour change: the lock keeps working through the eviction, and still asks for no `x` flag |
| `RD-LOCK-014` | `WAIT` is applied when configured, and a short count surfaces | On the Sentinel fixture with `wait_replicas: 1`: an acquire succeeds while the replica acknowledges, then the replica is **stopped** and the next acquire surfaces `Provider { ResourceExhausted }` rather than reporting a success that is not yet replicated. Stopped rather than partitioned — partitioning is fault injection (§8), and the short count is the same observable either way, since the plugin cannot tell a gone replica from an unreachable one and is not meant to. Also asserts what `WAIT` does **not** do: `features().linearizable` is unchanged, because `WAIT` narrows the window a failover can lose a write in and does not close it (ADR-009, DESIGN §3.6) |

### 4.4 Watch Integration Scenarios

| ID | Scenario | What it verifies |
|---|---|---|
| `RD-WATCH-001` | `watch(key)` receives exactly one `Changed` per `put` | One event, not three. A `put` runs `HSET`+`HINCRBY`+`PEXPIRE`, so a raw-keyspace-notification implementation would deliver three — this is the assertion that holds the in-script publish design (DESIGN §4.3) to `cpt-cf-clst-nfr-watch-delivery`'s no-duplicates requirement |
| `RD-WATCH-002` | `watch(key)` receives `Deleted` on `delete` and on `compare_and_delete` | Both paths publish `D`; a `compare_and_delete` that *mismatches* publishes nothing |
| `RD-WATCH-003` | `watch(key)` receives `Expired` on TTL lapse | With keyspace notifications enabled, a 500 ms entry produces `Expired` (not `Deleted`) within ~1 s — sourced from Redis's own notification, the one event the plugin cannot publish itself |
| `RD-WATCH-004` | `watch_prefix` is native and delivers per-key events | `PSUBSCRIBE`-backed: writes to three keys under the prefix produce three events on one prefix watch; a write outside the prefix produces none; `features().prefix_watch == true` on this fixture |
| `RD-WATCH-005` | N watchers on one prefix cost one Redis pattern | Five prefix watches on the same prefix; `PUBSUB NUMPAT` reports 1 and all five receive every event (the in-process fan-out claim, DESIGN §4.3) |
| `RD-WATCH-006` | Per-key ordering is preserved | 100 sequential `put`s on one key: the watcher observes 100 `Changed` in order with no gaps (`cpt-cf-clst-nfr-watch-delivery`) |
| `RD-WATCH-007` | No cross-key delivery; independent watchers | A watcher on `"a"` sees nothing when `"b"` is written; two watchers on the same key both receive events and one dropping does not affect the other |
| `RD-WATCH-008` | Buffer overflow produces `Lagged`, once, with a count | A watcher that stops draining while thousands of writes land receives `Lagged { count }` on resumption — coalesced into one event, not one per dropped message — and `cluster_redis_watch_events_dropped_total` agrees with the count. **The first test in the platform that produces `Lagged` at all**, and the reason ADR-003's variant is not dead weight |
| `RD-WATCH-009` | `Closed(Shutdown)` before `stop()` returns | Every active watch (exact and prefix) observes the terminal `Closed(ClusterError::Shutdown)` before `stop().await` resolves (`cpt-cf-clst-fr-shutdown-revoke`) |
| `RD-WATCH-010` | `watch_mode: disabled` degrades honestly | `watch`/`watch_prefix` return `Unsupported`; `features().prefix_watch == false`; no `PUBLISH` is issued on the write path (asserted via `INFO commandstats`) |

### 4.5 Lifecycle Integration Scenarios

| ID | Scenario | What it verifies |
|---|---|---|
| `RD-LIFE-001` | `build_and_start` connects, preflights, and loads scripts | Returns `Ok` against a fresh container; `SCRIPT EXISTS` reports every catalogued SHA present; the subscriber is subscribed before the call resolves (DESIGN §3.2 step 4) |
| `RD-LIFE-002` | `build_and_start` is idempotent and creates nothing | Called twice against the same server; the second succeeds, and the keyspace is unchanged — there is no schema to create and no migration to re-run |
| `RD-LIFE-003` | `stop` closes every connection | Counted with `INFO clients`, as a delta against a baseline taken before the plugin starts — not with `CLIENT LIST`, which sits on a `fred` interface the feature list leaves out, so `connected_clients` is what a build of this plugin can read. A delta rather than an absolute because the test's own control connection is one of them. The count returns to baseline after `stop`, command pool and subscriber alike |
| `RD-LIFE-004` | Invalid URL is rejected as config, not as a fault | `build_and_start` with a malformed URL returns `InvalidConfig` immediately, not a `Provider` error and not a timeout — an operator reading it should be looking at their YAML, not their server (DESIGN §10) |
| `RD-LIFE-005` | Unreachable server fails at startup, bounded | A valid URL pointing at a closed port returns `Provider { ConnectionLost }` within the connect budget rather than hanging or returning `Ok` with a background reconnect |
| `RD-LIFE-006` | `Drop` without `stop()` surfaces loudly (ADR-006) | A `RedisClusterHandle` — and, separately, a `RedisLockHandle` — dropped without `stop()`: debug build panics with the "dropped without stop()" message, release build logs the WARN; `stop()`-then-drop does neither |
| `RD-LIFE-007` | `Drop` during panic unwind degrades to a warning | A panic inside a closure owning an un-stopped handle does not abort the process (which a debug-build double panic would) and logs the skip message instead |
| `RD-LIFE-008` | `NOSCRIPT` recovery | `SCRIPT FLUSH` behind the plugin's back, then a `put`: it succeeds via one reload-and-retry, `cluster_redis_script_reloads_total` increments by 1, and no error reaches the caller (DESIGN §6) |
| `RD-LIFE-010` | A startup that fails after the connect leaks no subscriber | The failure half of `RD-LIFE-003`. A contradicted `durability` hint fails the preflight, which is past the connect, so both the pool and the already-connected subscriber have to be torn down (DESIGN §3.2 step 6). `connected_clients` returns to its baseline, polled through `wait_until` because `QUIT` is asynchronous server-side. Dropping the subscriber closes nothing — `fred` 10.1.0 gates `Drop for ClientInner` behind `credential-provider`, which nothing here enables — so without the explicit teardown the connection and its router task survive every failed boot, one per supervisor retry |
| `RD-LIFE-009` | `stop()` terminates against an unresponsive server | Hold four locks and an active watch, `pause` the container so the socket stays open but nothing answers, then `stop()`: it returns inside a 30 s budget. Bounded by `command_timeout_ms` and `POOL_CLOSE_TIMEOUT` (DESIGN §11), and a general claim rather than an accident of timing: every command this plugin issues is bounded client-side, so no in-flight operation can hold a background task's join open indefinitely (DESIGN §12) |

### 4.6 Redis-specific Scenarios

The declaration-and-environment tests. Several are the only coverage of DESIGN's
honesty claims, which no conformance scenario can reach.

| ID | Scenario | What it verifies |
|---|---|---|
| `RD-SPEC-001` | Stock container declares `EventuallyConsistent` | Default fixture (no AOF): `consistency()` is `EventuallyConsistent`, `features().linearizable` on the lock is `false`, and `cluster.provider.weak_consistency` (WARN) is logged exactly once at startup naming the detected topology (DESIGN §3.6) |
| `RD-SPEC-002` | Replicated topology declares `EventuallyConsistent` even with AOF | Sentinel fixture with `appendfsync always` on the primary: still `EventuallyConsistent`, because async replication is the binding weakness (ADR-009). The only fixture that puts durability and topology in *conflict* — every other one is weak on both or safe on both — so it is what catches reading `appendfsync` without reading topology. The premise is asserted too, reading `appendfsync` and `state=online` back off the primary: a fixture that quietly failed to enable AOF, or to attach the replica, would let this pass for the wrong reason |
| `RD-SPEC-003` | Verified single-node durable topology declares `Linearizable` | `start_redis_durable`: `consistency()` is `Linearizable`, the lock declares `linearizable: true`, and **no** `weak_consistency` WARN is logged. The positive branch the leader conformance suite depends on (§3) |
| `RD-SPEC-004` | The strict leader constructor refuses a weak cache | `CasBasedLeaderElectionBackend::new` over the default fixture's cache returns `Err`, and `ClusterWiring::from_config` on a profile binding `cache: { provider: redis }` with `leader_election` omitted **fails startup** with an actionable error. Asserts the blocker rather than working around it (DESIGN §7, §13 D1) and stays green after D1 lands, since the flag defaults to `false` — the default-off behaviour is exactly what this pins |
| `RD-SPEC-004b` | The opt-in flag reaches the weak constructor | With `leader_election: { provider: default, allow_weak_consistency: true }` **and an explicit `lock: { provider: redis, url: … }`** (the `cf-gears-cluster` addition delivered in this change, DESIGN §13 D1). Both halves of that profile are load-bearing: `provider: default` is the reserved sentinel the flag rides, since a bare `allow_weak_consistency` cannot deserialize against a required `provider` field; and the explicit lock binding is what keeps the omit-default lock's own consistency guard out of the way of a scenario about leader election, which a leader-only opt-in would otherwise have died on. The same profile that fails in `RD-SPEC-004` starts successfully, the resolved leader-election backend elects a leader over the weak Redis cache, and the SDK's own construction-time warning is logged. Also asserts the flag does **not** launder capability validation — a consumer declaring `CacheCapability::Linearizable` against the same profile still fails with `CapabilityNotMet`. The end-to-end half of the pair whose wiring-level half lives in `cluster/src/wiring_tests.rs` (DESIGN §13 D1 "Delivery") |
| `RD-SPEC-005` | Missing keyspace notifications degrade, loudly and safely | Container without `notify-keyspace-events`: `build_and_start` returns `Ok`, `cluster.provider.expiry_events_unavailable` (WARN, once) is logged, `Changed`/`Deleted` still arrive, no `Expired` ever does, and an expired entry still reads as absent. Promptness lost, correctness intact (DESIGN §4.3) |
| `RD-SPEC-005b` | `manage_keyspace_notifications: true` sets the flags and says so | Same container with the flag: the plugin issues one `CONFIG SET`, `CONFIG GET` confirms the required flags, `cluster.provider.keyspace_notifications_set` (INFO) is logged, and `Expired` events then arrive. With the flag `false` (default) no `CONFIG SET` is issued at all |
| `RD-SPEC-006` | Unsafe `maxmemory-policy` is warned at startup | Container with `--maxmemory-policy allkeys-lru`: `cluster.provider.maxmemory_policy_unsafe` (WARN, once) names the policy; startup still succeeds (a server-wide setting must not block a gear, DESIGN §3.7) |
| `RD-SPEC-007` | A real eviction is observed, reported, and mapped | With a tiny `maxmemory` and `allkeys-lru`, write enough to force eviction of a watched cluster key: the watcher receives `Deleted` (**not** `Expired`), `cluster.provider.eviction_observed` (WARN) is logged, and `cluster_redis_evictions_observed_total{provider}` increments — **and `cluster_provider_errors_total` does not move**, which is asserted alongside because the tempting shortcut (`kind = "other"` on the catalog counter) would make an eviction indistinguishable from every other backend failure while inflating a rate that is supposed to mean "operations are failing". The plugin's top operational risk, driven end to end rather than asserted in prose. Covers the evicted *cache entry*; `RD-SPEC-007b` covers the lease |
| `RD-SPEC-007b` | An evicted **lock lease** is observed and attributed | The case DESIGN §3.7 opens with and rates worst: an evicted lease hands the lock to a second holder with no TTL having lapsed. Acquire a lease with a 600 s TTL, never renew it — so it is the oldest untouched key when `allkeys-lru` starts choosing, and cannot be lost to expiry instead — then write filler through the cache until the eviction line appears. Asserts the line carries `primitive="lock"`, that the **cache's** line survives the same storm (a rate limiter shared between primitives would let the flood of evicted entries suppress the one lock line, which is emitted precisely under that much pressure), that both are counted, and that `cluster_provider_errors_total` still does not move |
| `RD-SPEC-008` | Cluster mode routes every operation correctly | 3-primary cluster fixture: 300 keys, asserted to have landed on more than one shard (per-node `DBSIZE`), then every cache and lock operation once — `get`, `compare_and_swap`, `put_if_absent`, `contains`, `delete`, `try_lock`, `release`. Each is a Lua script, and a script whose keys hash to different slots is rejected outright, so "all of these succeeded" *is* the zero-`CROSSSLOT` assertion (DESIGN §6). The spread assertion is what makes it a routing test: keys landing on one shard would exercise no routing and pass anyway. The invariant itself remains held statically by `scripts.rs`'s source-derived single-key assertion; this is the runtime half |
| `RD-SPEC-009` | `scan_prefix` in cluster mode covers every shard | 300 keys planted under one prefix, asserted to span shards, are **all** returned by one `scan_prefix` call — so `scan_cluster_buffered` really iterates every primary rather than only the one the client reached (DESIGN §4.4). A short count here — a key on an unvisited shard silently dropped — would pass every other scenario in this suite |
| `RD-SPEC-010` | Cluster mode declares `prefix_watch: false` — **the gate on lifting it** | On the cluster fixture, `features().prefix_watch` is `false` and `watch_prefix` returns `Unsupported`, while `watch(key)` still works — its event is a plugin `PUBLISH`, which is broadcast cluster-wide, unlike the node-local keyspace notifications a prefix watcher would need. Deliberately an assertion of the *current, honest* limitation (DESIGN §4.3, §13 D2): the follow-up that implements per-shard expiry subscriptions replaces this test with its positive counterpart, and until then nothing may declare `true` — which this now holds mechanically rather than by review |
| `RD-SPEC-014` | Sharded pub/sub is detected but not used | Against a Redis 7 container, `cluster.provider.sharded_pubsub_available` (DEBUG) is logged once, and `INFO commandstats` confirms **no** `SPUBLISH`/`SSUBSCRIBE` was issued — v1 records the capability without acting on it (DESIGN §13 D3). Against a Redis 6 container, neither the log nor the commands appear. Guards against a half-landed follow-up silently switching the publish path |
| `RD-SPEC-011` | An operator hint contradicted by the server fails startup | `durability: fsync_always` against a container running `appendfsync everysec`: `build_and_start` returns `InvalidConfig` naming both the claimed and the actual value. With `CONFIG GET` denied by ACL, the same hint is trusted and `cluster.provider.consistency_asserted` (WARN) is logged instead (DESIGN §3.6) |
| `RD-SPEC-012` | `database` selects a logical DB, and channels follow it | With `database: 3`, keys land in DB 3 (`SELECT 3` + `DBSIZE`), DB 0 is untouched, and `expired` notifications are received on `__keyspace@3__:…` rather than `@0` — the off-by-one-database bug that would silently deliver no expiry events |
| `RD-SPEC-015` | A `topology` hint skips the `INFO replication` round trip | Two startups against one container, unhinted then with `topology: standalone`, each measured as an `INFO` call-count delta from a baseline taken immediately before it: the difference between the two deltas is exactly 1. Both `RedisClusterConfig::topology` and `PreflightRequest::topology_hint` promise the skip and DESIGN §3.4 says the hint "replaces detection", so a preflight that detects anyway wastes a round trip on every hinted deployment — and on the locked-down managed instance the hint exists for, logs `cluster.provider.topology_unknown` announcing a conservative declaration `resolve_topology` never makes. Asserted as a call count rather than as an absent WARN because no fixture refuses `INFO`, and because the count is the stronger claim: the WARN is unreachable once the command is not issued |
| `RD-SPEC-013` | Throughput smoke against the OAGW envelope | 10 000 `compare_and_swap` operations on a small key set, measured and printed as an artefact rather than asserted against a threshold (a CI container's absolute numbers are not a production predictor). Recorded because `cpt-cf-clst-actor-oagw`'s 10k+ counter updates/sec is the reason this plugin exists; a regression of an order of magnitude is what the artefact makes visible. Read with `-- --nocapture` |

## 5. Layer 4 — Fault Injection (Toxiproxy) and Failover

> **None of this layer exists, and neither does the infrastructure it assumes.**
> Toxiproxy appears nowhere in this repository — not in any `Cargo.toml`, not in
> any test, not in any workflow — and there is no nightly integration workflow to
> hang it off: the only `schedule:`d workflows are `clippy-nightly.yml`,
> `e2e.yml`, `shear-nightly.yml`, `codeql.yml`, `scorecard.yml`, and
> `cache_cleanup.yml`, none of which runs cluster integration tests. So this
> section describes infrastructure the platform does not have rather than a
> harness this plugin declined to plug into, and the postgres plugin's own
> TESTING.md §5 (`PG-FAULT-001..007`) is the same document written a change
> earlier and never built either.
>
> It is kept rather than deleted because the scenarios below are a *worked design*
> — `RD-FAULT-006` and `RD-FAULT-007` in particular are the only place this
> plugin's consistency story is stated as a pair of executable claims rather than
> as prose — and because the `RD-FAULT-*` IDs are referenced from §7, §8, and the
> implementation plan. §8 records the whole layer as one gap, with what stands in
> for each scenario today. Read every row below as a specification, not as
> coverage.

Toxiproxy sits between the plugin and Redis; the Sentinel fixture supplies
the failover cases, which are the ones that actually probe ADR-009's claims.

| ID | Scenario | Fault | Expected behaviour |
|---|---|---|---|
| `RD-FAULT-001` | Subscriber connection loss → `Reset` | Kill the subscriber's TCP connection | Every watcher receives `Reset` (never a silent gap); `fred` reconnects and replays subscriptions; subsequent events are delivered; `cluster_watch_resets_total` and `cluster_redis_subscriber_resubscribes_total` both increment |
| `RD-FAULT-002` | Command connection loss → `ConnectionLost` | Kill a pool connection mid-command | `get`/`put` return `Provider { ConnectionLost }`; the next call succeeds on a reconnected connection |
| `RD-FAULT-003` | Latency spike → `Timeout` | 10 s latency, `command_timeout_ms: 500` | `get` returns `Provider { Timeout }` at ~500 ms, not 10 s |
| `RD-FAULT-004` | Reconnect fails past the retry budget | Permanent blackhole | Watchers receive `Closed(Provider { ConnectionLost })` once `fred`'s reconnect policy is exhausted; `cluster_redis_connection_state` reads 0 |
| `RD-FAULT-005` | A blocking `lock()` survives a Sentinel failover | Partition the primary; Sentinel promotes the replica mid-`lock()` | `lock()` retries through the topology change inside the caller's budget and acquires against the new primary, rather than returning `LockTimeout` (DESIGN §5.3's `Provider`-vs-`LockTimeout` distinction) |
| `RD-FAULT-006` | Failover **can** grant a lock twice — the documented unsafety, demonstrated | Acquire against the primary, partition it before replication, promote the replica, then acquire the same name from a second instance | Both instances hold the lock. **This test asserts the failure**, because DESIGN §3.6/§5.1 promise only `EventuallyConsistent`/`linearizable: false` in this topology, and a plugin whose declaration says one thing while its behaviour says another is the exact dishonesty ADR-009 §"Honest backend declaration" forbids. It is also the evidence for §7's recommendation to route leader election elsewhere |
| `RD-FAULT-007` | No split-brain under partition on the durable single-node fixture | 5 `CasBasedLeaderElectionBackend` instances, each with its own pool, electing the same name against `start_redis_durable`; Toxiproxy partitions a random subset for several TTL intervals, then restores | Sampling every candidate's `status()` throughout (`tokio::time`-driven, not wall-clock sleeps): at no sampled instant do two report `Leader`. The real-backend counterpart to the non-runnable `SC-LEAD-010`, and the *positive* pair to `RD-FAULT-006` — the same test on the fixture whose declaration says it should hold |
| `RD-FAULT-008` | `OOM` under `maxmemory` with `noeviction` | Fill to `maxmemory` with `noeviction` | Writes return `Provider { ResourceExhausted }` (retryable), reads keep working, and no key is silently lost — the safe failure mode `noeviction` buys versus `RD-SPEC-007`'s eviction |

`RD-FAULT-006` and `RD-FAULT-007` are the pair that make this plugin's consistency
story testable rather than merely documented: the same code, the same scenario, one
topology where the guarantee is claimed and one where it is explicitly not.

## 6. Static Analysis

- **`cargo check`** — no errors.
- **`cargo clippy`** — no warnings beyond the workspace allow-list.
- **`dylint`** — `cargo gears lint --dylint` is clean. Note what this does **not**
  include: there is no `no-remote-in-lock-critical-section` rule anywhere in this
  workspace. `tools/dylint_lints/` contains only `de12_documentation`, and there is
  no `lint_utils` crate for such a rule to be built on. ADR-002's
  no-remote-I/O-in-the-critical-section rule therefore stands as a design constraint
  **reviewers** enforce, and it is worth being explicit about: a rule believed to be
  lint-enforced gets less review attention than one known not to be.
- **No serde in SDK contract types** — the workspace layer rule. This plugin's
  `config.rs` uses serde; the plugin adds no serde derive to any `cluster-sdk` type.
- **`cargo test --doc`** — all doc examples compile and pass; `cargo doc --no-deps`
  is warning-free.
- **No `KEYS`, no `FLUSHALL`, no `FLUSHDB`** — a crate-local test
  (`src/static_analysis_tests.rs`) walking every non-test `.rs` file under `src/`,
  plus a Lua half in `scripts_tests.rs` for the `redis.call` form. `KEYS` on a shared
  production Redis is an outage rather than a slow query — O(N) over the whole
  keyspace, blocking the single-threaded server for the duration (DESIGN §4.4) — and
  `FLUSHALL`/`FLUSHDB` would delete other tenants' data, so all three are worth a
  mechanical check rather than review vigilance.

  **The scan is on the fred call form (`.keys(`, `.flushall(`, `.flushdb(`), not on
  the command name**, and that is not a shortcut. Matching the bare word `KEYS` is
  unusable here: it appears in `REQUIRED_KEYSPACE_FLAGS`, in the
  `keyspace_notifications_set` event name, and in **every script's `KEYS[1]`** —
  where it is Lua's argument global rather than a command, and the correct use. The
  trailing `(` is what distinguishes a call site from a mention in a doc comment.
  Two meta-tests pin the matcher: one that it recognizes a planted call, so a check
  that has quietly stopped matching anything cannot pass vacuously, and one that it
  tolerates all three legitimate uses, so a future tightening back to a bare word
  match fails here rather than in whichever module it breaks first. The walk is
  recursive rather than a file list, so a module added later is covered without
  anyone remembering to add it — a check that silently stops covering new code is
  worse than no check, because it still reads as a green guarantee.

  **The `B*` blocking family is deliberately not scanned.** DESIGN §3.1 names
  `fred`'s interface features individually and leaves `i-lists` out, so `BLPOP`,
  `BRPOP`, `BLMOVE` and the rest are not in the trait surface this crate can see:
  reaching for one is a **compile error**, which is a stronger guarantee than any
  text scan.
  Scanning for them would in fact be counterproductive — `lock/mod.rs` and
  `lock/waiters.rs` both name `BLPOP` in prose, explaining why the release-waiter
  registry exists instead of it, and a text scan would turn that explanation into a
  failure.

- **Every `warn!`/`error!` carries a `name:`** — the same crate-local test file,
  over the same source walk. DESIGN §9's rule is that a catalogued event carries
  its name twice, and the structural half is what a collector filters on: an event
  without it is unalertable however severe the condition it reports. That is not
  hypothetical — the WARN announcing that the subscriber is permanently gone
  shipped without one, which is precisely the line an operator most needs to match.

  A site that is genuinely not an operator's business is marked
  `not-a-catalogued-event:` with a reason, in the comment-and-attribute run
  immediately above it. The four ADR-006 `Drop` guards are the whole of the current
  set: they fire on a programming error, in the release build of the same arm that
  panics in debug, so cataloguing them would put a developer diagnostic in the
  table an operator alerts on. The matcher has its own capable-of-failing test,
  for the reason the forbidden-call one does.

## 7. CI Cadence

**Everything this plugin runs, runs on every PR.** There is no nightly row, because
nothing has earned one: the fixtures that would ordinarily justify a slower cadence
are the Sentinel and Cluster ones, and `nextest` overlaps their container startup
with the rest of the suite, so they cost no wall clock against the run as a whole
(the L3 row below).

| Layer | Trigger | Approx. duration |
|---|---|---|
| L1 unit tests | Every PR — `cargo test -p cf-redis-cluster-plugin --lib`, no Docker | 211 tests, sub-second |
| L2 conformance + L3 integration, over eight single-node shapes plus two multi-node topologies | Every PR touching Rust — `make test-cluster-redis`, in `ci.yml`'s `integration` job | 280 tests total in ~10 s of test time, ~35 s including the build |
| L3 Sentinel and Cluster topologies (`RD-SPEC-002`, `008..010`, `RD-LOCK-014`) | Every PR, in the same `make test-cluster-redis` run | 5 tests, and **no wall-clock cost** — `nextest` overlaps their containers with the single-node ones, so the suite still finishes in about ten seconds |
| ~~L4 fault injection and failover~~ (`RD-FAULT-001..008`) | **Never — not built**; §5 and §8 | — |

The 280 are 211 unit tests, 66 Layer 3 test functions, and the three conformance
suites. Note that the 66 functions cover the **63** scenarios of §4.2–§4.6 (10
`RD-CACHE-*`, 15 `RD-LOCK-*`, 10 `RD-WATCH-*`, 10 `RD-LIFE-*`, 18 `RD-SPEC-*`) and
not 66 of them: a scenario whose halves must fail distinguishably is split across
two functions — `RD-LIFE-006`'s silent-drop and panicking-drop halves are the
worked example — so scenario count and test count are deliberately different
numbers, and this is the only place either is written down.

`make test-cluster-redis` passes `--retries 1`, for the reason
`make test-cluster-pg` does: container setup is load-sensitive on a busy CI host,
and a genuine logic regression fails both attempts, so the retry absorbs Docker
churn without masking one.

L3 tests are gated behind an `integration` feature so a default `cargo test` needs
no Docker. Cargo forces the mechanics: `testcontainers`/`testcontainers-modules` are
declared **optional under `[dependencies]`** rather than `[dev-dependencies]`, because
Cargo does not allow optional dev-dependencies and the feature array must name them
via `dep:`. That placement trips `cargo-shear`'s misplaced-optional-dependency
heuristic, so it needs a `[package.metadata.cargo-shear] ignored` entry; every
reference to them lives in `tests/`, so the feature has no effect on the compiled
plugin. Each test binary carries `required-features = ["integration"]` so the binaries
are not even built without the flag:

```toml
[features]
integration = ["dep:testcontainers", "dep:testcontainers-modules"]
```

**There is no `integration-topology` feature.** The natural place for one would be
gating the Sentinel and Cluster fixtures so a per-PR run does not pay their startup
— but there is no startup to avoid paying. `nextest` runs the suite's containers
concurrently, so both multi-node fixtures come up alongside the single-node ones and
the run finishes in about the same ten seconds it would without them (the L3 row
above). A gate would therefore buy nothing and cost a configuration in which the
strongest topology coverage in the suite is off by default, which is the wrong
default for the scenarios DESIGN §13 D2 designates as its gates.

Workspace entries this plugin relies on:

- `fred` in `[workspace.dependencies]`, with the feature set DESIGN §3.1 records.
- `testcontainers-modules` with its `redis` feature enabled.

## 8. Coverage Gaps and Follow-ups

This section is the register of what the suite does **not** hold. It is written to
be read against §1's four-layer pyramid, because the honest summary is that this
plugin ships three of those four layers and no L4 at all: L1 and L3 in full —
every topology, single-node and multi-node alike — and L2 with one scenario
short, `SC-LEAD-006`, which the shared runner skips under
`TimeControl::Real` and which therefore has no Redis coverage at all. "Three
layers delivered" is a statement about the layers, not a claim that every
scenario in them runs; the row below is where the one that does not is recorded.

That is a narrower delivery than the platform-wide
[TESTING-STRATEGY.md](../../../docs/TESTING-STRATEGY.md) promises — its §6 and its
cadence table (§"Every PR" / §"Nightly") describe Toxiproxy fault injection and
`turmoil` deterministic simulation as standing layers, and neither exists for any
cluster backend: the postgres plugin documents the identical L4 and does not have it
either. **The discrepancy is recorded here rather than resolved by editing the
platform document**, because whether the platform's four-layer promise should be
narrowed or the two missing layers should be built is a decision about every
cluster backend, not about this one.

### 8.1 Deferred scope — specified but not covered

Every row here is a scenario or fixture §4 and §5 describe and the suite does not
run. The `RD-*` IDs are retained deliberately: a deferred scenario still needs an
ID to be referenceable, from source comments and from the follow-up that eventually
builds it.

| Gap | Severity | Tracking |
|---|---|---|
| `RD-FAULT-001..008` and **the whole of §5's Toxiproxy layer** | Warning — Toxiproxy appears nowhere in this repository and there is no nightly integration workflow to run it from, so §5 specifies infrastructure the platform does not have. What is lost is every deliberate-failure path: subscriber loss → `Reset`, an exhausted reconnect policy → `Closed(Provider { ConnectionLost })`, a latency spike → `Timeout`, `OOM` under `noeviction`, and both halves of the failover pair. Several of those paths have been exercised by hand against containers — killing the subscriber's connection to observe `Reset`, a closed port for `RD-LIFE-005` — but by-hand is not a regression test | Follow-up, gated on the platform acquiring a fault-injection harness |
| `RD-FAULT-006`/`RD-FAULT-007` specifically — the pair that makes the consistency story *executable* | Warning — these are the only two scenarios anywhere that assert the declaration against behaviour in both directions: that a Sentinel failover really can grant one lock twice (which DESIGN §3.6 promises it may, and which is the evidence for §7's "route leader election elsewhere"), and that the same test does *not* split-brain on the durable single-node fixture. Without them, "honest declaration" is a claim this suite documents rather than one it demonstrates. `RD-SPEC-002` now asserts the *declaration* against a live replicated topology, which is the static half; these two are the behavioural half | Follow-up; needs fault injection **and** a Sentinel fixture whose nodes are separately killable — this one co-locates them (§4.1), so it can observe a replica leaving but not a failover |
| `RD-LIFE-011` — a startup whose **subscriber** connect times out leaks no connection and no router task | Warning — the failure arm is `connect.rs`'s subscriber `wait_for_connect()` timeout (as opposed to the pool's, `RD-LIFE-005`, or the post-connect steps, `RD-LIFE-010`). Forcing it end-to-end needs the subscriber's connection to accept-then-hang past `CONNECT_TIMEOUT` *while the pool connects*, and pool and subscriber share one URL against one real server — so the pool-succeeds/subscriber-hangs split cannot be arranged without per-connection latency injection (Toxiproxy, the §5 layer this crate does not build). What **is** covered: the teardown primitive both arms call, `abandon_subscriber`, is unit-tested to abort the router task rather than leak it (`shutdown_tests.rs::abandon_subscriber_stops_the_router_task`), and reading `connect.rs` confirms both the timeout and the connect-error arm call it before returning | Follow-up, gated on the platform acquiring a fault-injection harness |

### 8.2 Known limits of what *is* covered

| Gap | Severity | Tracking |
|---|---|---|
| The conformance runner skips `SC-LEAD-006` under `TimeControl::Real` | Warning — it forces a *missed* renewal to assert re-enrolment, which a healthy real backend never exhibits by waiting, so the shared runner skips it on a real clock. It maps to fault injection, which §8.1 defers. One conformance scenario therefore has no Redis coverage at all, which the suite's green result does not say | Accepted — would need L4 |
| `Lagged` count exactness under extreme backpressure | Warning — `RD-WATCH-008` asserts a `Lagged` arrives with a plausible count and that `cluster_redis_watch_events_dropped_total` agrees with it, not that the count is exact. Exactness would require pausing the fan-out task at a known point, which needs a `#[cfg(test)]` seam. Worth keeping in view precisely because this is the **first test in the platform that produces `Lagged` at all** — ADR-003's variant had no producer before it, so this row is the limit of the only coverage that variant has anywhere | Accepted — would need a pause hook |
| Sharded pub/sub (`SPUBLISH`) is detected and not used, so the Cluster publish ceiling stands | Warning — ADR-001 puts clustered `PUBLISH` around 12 500/sec, below this plugin's write ceiling, making pub/sub the binding constraint on a clustered heavily-watched deployment (DESIGN §12). `RD-SPEC-014` asserts the detect-and-do-not-use behaviour in both directions; no test measures where the ceiling actually lands. The Cluster fixture could now carry one — what is missing is a reason to spend the wall clock on a number that a CI container cannot make a production predictor anyway (see the performance row in §8.3) | Follow-up, gated on a deployment hitting it (DESIGN §13 D3) |
| Managed-Redis ACL restrictions are approximated by a `CONFIG`-denied ACL user, not by a real managed instance | Warning — `RD-SPEC-011`'s denied-`CONFIG` branch runs against a container started with `--user limited on '>pw' '~*' '&*' +@all -config`, so the approximation is real and executed rather than deferred. It is still an approximation: the plugin only cares that the command errors, which is the part ElastiCache/MemoryDB reproduce, but no CI job runs against one of those. (`&*` in that flag is load-bearing — without it the user cannot subscribe, and the plugin opens its subscriber before it reaches the durability check) | Accepted |
| The consistency declaration is never re-evaluated after startup | Warning — a single node that gains a replica keeps a now-false `Linearizable` declaration (DESIGN §12). Untestable without a fixture that changes topology under a running plugin, and unfixable without a capability model that allows post-resolution changes | Accepted |
| Multi-node Cluster **failover** (as opposed to Sentinel failover, `RD-FAULT-005..006`) | Future — needs a cluster fixture with replicas plus a real slot handover, to probe whether slot migration adds failure modes beyond async replication (ADR-009 hints it does). The 3-primary fixture of §4.1 has no replicas, so it cannot exercise it | Out of initial scope |
| No test drives two *different* plugin instances against one Redis with different `key_prefix`es for a full cross-primitive isolation sweep | Warning — `RD-CACHE-006` covers `scan_prefix` isolation and the conformance suites isolate by database *and* prefix, but lock names, event channels, and keyspace notifications are only covered per-primitive. A shared Redis with two independent cluster deployments is a plausible arrangement | Follow-up |
| `${VAR}` expansion is covered by the `${VAR:-default}` branch only | Warning, and permanently so — edition 2024 makes `std::env::set_var` `unsafe` and this crate is `#![forbid(unsafe_code)]`, so no unit test can set a variable for the plain `${VAR}` branch to resolve. The `:-default` test drives the identical `expand_vars()` call down the other branch, and `provider.rs`'s unresolvable-var test covers the failure path. Not a gap to close — a constraint to record | Accepted — structural |

### 8.3 Non-gaps, recorded so they are not re-investigated

| Apparent gap | Why it is not one |
|---|---|
| The `allow_weak_consistency` opt-in spans two crates, so the Redis suite alone does not prove the wiring half | **Both halves run.** `cf-gears-cluster` carries the reserved-`default`-provider tests over a stub weak cache in `config_tests.rs` and `wiring_tests.rs` — default-off fails, flag-on constructs, the leader flag alone still fails on the lock, and capability validation is unaffected — and `RD-SPEC-004`/`004b` cover the same paths end to end through real operator YAML against a real container, `cluster/src/gear.rs` having registered both Redis providers. The duplication is deliberate: the wiring tests prove the dispatch against *any* weak cache, the scenarios prove an operator's YAML reaches it with Redis bound |
| The no-`KEYS` check does not scan for the `B*` blocking family, and matches a call form rather than a command name | **Both are deliberate, and §6 gives the reasoning in full.** `KEYS` appears legitimately in `REQUIRED_KEYSPACE_FLAGS`, in `KEYSPACE_NOTIFICATIONS_SET`, and in every script's `KEYS[1]` — where it is Lua's argument global and the *correct* use — so a bare word match is unusable and the scan is on `.keys(`. The blocking family is a compile error rather than a convention, since `i-lists` is absent from `fred`'s feature list, and two source files name `BLPOP` in prose explaining exactly that, which a text scan would turn into a failure. **Genuinely open:** the check is crate-local, so a future Redis consumer elsewhere in the workspace inherits no protection. Promoting it to a dylint rule is worth doing once a second Redis consumer exists (DESIGN §13 D6) |
| Performance numbers are artefacts, not thresholds | **Intended, not a shortfall.** `RD-SPEC-013` prints one — 10 000 `compare_and_swap` operations over a small key set, measured and reported rather than asserted against a threshold, read with `-- --nocapture`. Per `docs/PRD.md` §6.2 quantitative per-backend SLOs are excluded from the cluster-wide NFR set and owned by each plugin's own tests, and a CI container's absolute timings are not a production predictor; the artefact makes an order-of-magnitude regression visible, which is the actionable part |
| `watch_mode: keyspace` (the middle mode) is untested | **There is nothing to test.** The mode was considered and rejected (DESIGN §13 D4); `WatchMode` is a two-variant enum with no `todo!()` arm, so no unimplemented path exists. Retained here only so the question is not reopened as an oversight |
