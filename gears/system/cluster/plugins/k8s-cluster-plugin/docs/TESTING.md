# Testing Strategy — Kubernetes Cluster Plugin

> **Status: implemented.** The plugin and its Layer 1–3 test suites are in place
> (`tests/conformance.rs`, the `*_integration.rs` suites, `tests/k8s_specific.rs`,
> and `tests/common/` — k3s startup + CRD apply). This document is the test plan
> they are built against, paired with [DESIGN.md](./DESIGN.md), from
> [#4372](https://github.com/constructorfabric/gears-rust/issues/4372).

> **Companion documents:**
> - [DESIGN.md](./DESIGN.md) — implementation design for this plugin
> - [TESTING-STRATEGY.md](../../../docs/TESTING-STRATEGY.md) — platform-wide cluster testing strategy (layers, tooling, CI cadence)
> - [Scenario Catalog](../../../docs/scenarios/README.md) — `SC-*` IDs referenced below

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Layer 1 — Unit Tests (in-crate)](#2-layer-1--unit-tests-in-crate)
- [3. Layer 2 — Conformance Suite](#3-layer-2--conformance-suite)
- [4. Layer 3 — Integration Tests (testcontainers k3s)](#4-layer-3--integration-tests-testcontainers-k3s)
  - [4.1 Container Setup](#41-container-setup)
  - [4.2 Cache Integration Scenarios](#42-cache-integration-scenarios)
  - [4.3 Leader Election Integration Scenarios](#43-leader-election-integration-scenarios)
  - [4.4 Lock Integration Scenarios](#44-lock-integration-scenarios)
  - [4.5 Watch Integration Scenarios](#45-watch-integration-scenarios)
  - [4.6 Lifecycle Integration Scenarios](#46-lifecycle-integration-scenarios)
  - [4.7 Kubernetes-specific Scenarios](#47-kubernetes-specific-scenarios)
- [5. Layer 4 — Fault Injection and Failover](#5-layer-4--fault-injection-and-failover)
- [6. Static Analysis](#6-static-analysis)
- [7. CI Cadence](#7-ci-cadence)
- [8. Coverage Gaps and Follow-ups](#8-coverage-gaps-and-follow-ups)

<!-- /toc -->

## 1. Overview

The K8s plugin follows the four-layer pyramid from the platform-wide
[TESTING-STRATEGY.md](../../../docs/TESTING-STRATEGY.md):

```
L4  Fault injection (Toxiproxy) + API-server restart / RBAC revocation  — nightly
L3  Integration tests (testcontainers k3s)                              — per-PR in this crate
L2  Conformance suite (cluster-conformance crate)                       — driven by the L3 cluster
L1  Unit tests (co-located, no external dependencies)                   — every PR, sub-second
```

The conformance suite (L2) is the keystone: the shared, backend-agnostic scenario
bodies every cluster backend runs, executed here against a real Kubernetes API
server. Passing it is the primary signal that this plugin implements the
`ClusterCacheBackend`, `LeaderElectionBackend`, and `DistributedLockBackend`
contracts.

Five concerns shape this plan beyond the shared pyramid, and account for most of
what is specific to it:

- **This is the first plugin whose three primitives are all native.** Every other
  backend derives at least one of them from the SDK defaults over its cache, so
  `run_leader_conformance` has only ever run
  against the SDK's own implementations plus the standalone plugin's in-process
  ones. Here all three suites run against genuinely independent implementations —
  the first real exercise of `cpt-cf-clst-nfr-cross-backend-stability` for leader
  election, and the reason §3 treats it as the headline
  rather than an afterthought.
- **The cache's version and TTL semantics are the two places this backend could
  most plausibly get the contract wrong, and conformance catches both.**
  `SC-CACHE-002`/`003`/`009` pin `spec.version` to start at 1, strictly increase,
  and reset to 1 on delete-and-recreate — which is precisely what disqualified
  `resourceVersion` (DESIGN §2.7), so these three scenarios are the regression test
  for that decision. `SC-CACHE-010` writes a **50 ms** TTL and expects an `Expired`
  event about 100 ms later, which no fixed sweep interval an operator would tolerate
  can satisfy; it is the reason the sweeper is deadline-armed (DESIGN §6.2), and
  §4.2's `K8S-CACHE-006` asserts the mechanism directly rather than only its effect.
- **A shared scenario is unreachable under `TimeControl::Real`, and this
  plugin cannot use `Virtual`.** `SC-LEAD-006` (transient `Status(Lost)`
  re-enrolls) is skipped by the runner under `Real`, and a paused clock is not an
  option for a backend with real network I/O and `kube`'s own internal timers. The
  property is load-bearing here, so it is re-covered as a plugin-specific L3
  scenario with a real, short TTL (`K8S-LEAD-008`). §3 records why
  that is the right layer for it rather than a gap.
- **The clock rules differ between the primitives, deliberately, and both halves
  need holding.** DESIGN §2.8's monotonic `Observed` expiry — no cross-machine
  clock comparison anywhere on the lease path — is a pure function and §2 tests it
  exhaustively, including the two skew directions that make the naive wall-clock
  comparison unsound; `K8S-SPEC-007` drives it against a real API server with a
  deliberately wrong clock. The **cache** takes the opposite decision on purpose
  (writer's clock for `expiresAt`, DESIGN §6.2), so `K8S-CACHE-013` asserts that
  exception is bounded to where it was intended and has not leaked onto a lease.
- **Every operation lands on a shared control-plane budget, so request volume is
  itself an assertion.** DESIGN §12's top risk is that this plugin overloads etcd,
  and shipping the cache is what raises it from bounded to unbounded.
  `K8S-SPEC-010`, `K8S-SPEC-011`, and `K8S-CACHE-014` assert *how many requests*
  the plugin makes — a follower that starts polling, or a cache `get` that stops
  being one round trip would pass every behavioural test and fail these.

## 2. Layer 1 — Unit Tests (in-crate)

Co-located with source (`src/**/*_tests.rs`). No Kubernetes, no Docker; run with
`cargo test -p cf-k8s-cluster-plugin --lib`.

| Module | What is tested |
|---|---|
| `naming.rs` | The §2.2 mapping: output is always a legal RFC 1123 subdomain for adversarial inputs (`/`-separated scoped names, uppercase, unicode, leading/trailing separators, an already-legal name, a 4 KiB name); **injectivity** — `a/b` and `a-b` and `a_b` all map to distinct names, the property a lossy slug would break; determinism across calls and across all three `seg` values (`el`/`lk`/`ca`); the hash is exactly 16 lowercase hex chars and is preserved when the slug is truncated; total length ≤ 253 for a maximal `lease_prefix` plus a maximal name; `lease_prefix` validation rejects uppercase, `/`, a leading digit-only-then-dash form, and > 40 chars. Label/annotation key constants are legal label keys |
| `observed.rs` | The §2.8 expiry predicate, as a pure function. A record younger than its duration is live; older is expired; an observed change to `holderIdentity` **or** to `renewTime` resets `seen_at`; an identical re-observation does **not** reset it (the bug that would make a lease immortal); a fresh observer of an already-stale lease waits a full duration; and the two skew cases — a `renewTime` an hour in the future and one an hour in the past both behave *identically*, because neither is read. That last assertion is the whole point of the module and is written to fail loudly if anyone reintroduces a wall-clock comparison |
| `config.rs` | serde round-trip for every field and every default (`lease_prefix` "cluster", `request_timeout_ms` 10000, `reaper` true, `lock_object_retention_ms` 24h); `deny_unknown_fields` turns an operator typo into `InvalidConfig`; `namespace`/`identity` `${VAR}` / `${VAR:-default}` expansion resolves via `ExpandVars`/`config_expanded()` and a missing referenced var errors rather than leaving a literal `${VAR}`; `request_timeout_ms` ≥ the shortest derived renewal interval is rejected at construction (the §12 bounded-request property); `election_lease_names` values are validated as RFC 1123 subdomains; cache defaults (`cache_reads` `quorum`, `cache_watch` true, `max_value_bytes` 256 KiB, `put_max_retries` 3) round-trip and `ReadMode` rejects an unknown variant; the **four** config shapes duplicate the shared subset (serde `flatten` cannot coexist with `deny_unknown_fields`) and a drift guard asserts they deserialize the shared keys identically (DESIGN §8) |
| `lease.rs` | TTL encoding (§2.9): `leaseDurationSeconds == max(1, ceil(ttl))` for 1 ms / 750 ms / 1 s / 1.001 s / 29 s; the `ttl-ms` annotation carries the exact millisecond value; a TTL above `i32::MAX` seconds is `InvalidConfig`; an election TTL below `min_election_ttl` is `InvalidConfig` naming the derived renewal rate |
| `guarded.rs` | The 409 classifier, which is the one function every primitive's write path funnels through, so no call site can classify independently (DESIGN §2.7, §10). Per call site: lock acquire → `LockContended`; renewal → re-read-and-retry; cache CAS → `CasConflict`; cache `put` → bounded retry then `Provider { ResourceExhausted }`; resign and guarded release → `Ok(())`; guarded delete → `Ok(false)`. Separately, that `409 AlreadyExists` and `409 Conflict` are distinguished by `reason` and never confused — they share a status code and mean opposite things (DESIGN §10) |
| `leader/renew.rs` | The renewal state machine as a pure transition function over `(outcome, failure_count, config)`: a retryable error increments and does **not** emit; `max_missed_renewals` consecutive retryable errors emit `Lost` exactly once; a success resets the counter; a **409 emits `Lost` immediately and does not consume budget** (the longest-lived split-brain window this design could have — DESIGN §4.2); a `403` emits `Lost` immediately (non-retryable, DESIGN §10); a renewal attempted past the local deadline emits `Lost` without issuing a write |
| `leader/watch.rs` | Watcher-signal → `LeaderWatchEvent` mapping (DESIGN §4.3), row by row, over a synthetic stream: holder becomes us → `Leader`; becomes someone else → `Follower`, preceded by `Lost` if it was us; **only `renewTime` advanced → no event at all** (the dedup that keeps a healthy holder's renewals off the consumer's stream); `Delete` → re-acquire; an `Init`/`InitApply`/`InitDone` sequence → exactly one `Reset` and one `watch_reset("leader")`; `401`/`403` → `Closed(Provider { AuthFailure })`; and that **no `Lagged` is ever synthesised** (DESIGN §4.3 — the K8s watch protocol has no drop count, and fabricating one would be worse than omitting it) |
| `lock/mod.rs` | Holder-token construction: `<identity>#<uuid-v4>`, a fresh UUID per acquisition, two acquisitions differ, parsing splits on the **last** `#` so an identity containing one round-trips. The three-outcome classification of a failed `lock()` (`Shutdown` / `Provider` / `LockTimeout`) as a pure function of `(elapsed, cancellation state, last error)` — the distinction DESIGN §5.3 says a caller cannot afford to lose. The token fence: `renew`/`release` decisions for holder-is-us / holder-is-foreign / holder-empty / deadline-passed, asserting `release` on a foreign holder issues **no write** |
| `lock/waiters.rs` | Register/notify/deregister-on-drop; a notification for a name with no waiter is a no-op; a waiter dropped mid-wait leaves nothing behind; the wake deadline is `min(holder's Observed deadline, caller's remaining budget)` |
| `lock/reaper.rs` | The prune predicate (DESIGN §5.5): a lock Lease with an empty holder past `lock_object_retention` is deleted; one with a **holder** is never deleted regardless of age; a `primitive` label the reaper does not own is never touched. Deletes always carry `resourceVersion` **and** `uid` preconditions |
| `crd/cache_entry.rs` | The `ClusterCacheEntry` ⇄ `CacheEntry` projection both ways: `spec.value` base64 round-trips arbitrary bytes including empty and 0xFF-heavy payloads; `spec.version` maps to `CacheEntry::version`; an absent `spec.expiresAt` is `Ttl::Indefinite` and a present one round-trips at **millisecond** precision (the claim DESIGN §2.9 makes to distinguish the cache from the lease primitives); the derived `CustomResource` metadata matches `deploy/crd.yaml` field-for-field — group, version, kind, plural, scope — asserted structurally so the shipped manifest and the Rust type cannot drift |
| `cache/mod.rs` | Key construction (`<prefix>-ca-<slug>-<hash16>`) and the `name` annotation round-trip. Version arithmetic: `put_if_absent` writes 1, each write writes `prev + 1`, and a write is **always** issued with a differing version so a byte-identical `put` still bumps (the no-op-`PUT` trap DESIGN §6.1 records). CAS decision table as a pure function of `(expected, stored)`: equal → write, unequal → `CasConflict` carrying the entry from the same read and **no write**. `compare_and_delete` decision table, asserting a value mismatch issues no request. Value-size rejection at `max_value_bytes` returns `InvalidConfig` naming the size **before** any request, and the 4/3 base64 inflation is accounted for in the check (DESIGN §6.6). Read-path expiry: an entry whose `expiresAt` has passed reads as `None` from `get`/`contains` regardless of whether the object still exists (DESIGN §6.2) |
| `cache/watch.rs` | Watcher-signal → `CacheEvent` mapping (DESIGN §6.3) row by row over a synthetic stream: `spec.version` advanced → `Changed`; **label/annotation-only change → no event**; `Delete` with a past `expiresAt` → `Expired`; `Delete` with an absent or future `expiresAt` → `Deleted` (the distinction read straight off the payload, needing no side table); `Init`/`InitApply`/`InitDone` → exactly one `Reset` plus one `watch_reset("cache")`; and **no `Lagged` ever synthesised**. The registry: per-key and per-prefix fan-out from one stream, five subscribers on one prefix, a dropped subscriber pruned, the terminal `Closed` delivered to a full buffer, and no `Reset` after a terminal `Closed` |
| `cache/sweeper.rs` | The deadline heap as a pure structure: entries ordered by deadline, the timer armed at the **nearest** deadline rather than a fixed interval (DESIGN §6.2 — the property `SC-CACHE-010`'s 50 ms TTL depends on); a `Ttl::Indefinite` entry never enters the heap (`SC-CACHE-011`); an entry re-`put` with a later deadline is re-keyed rather than duplicated; a `Delete` observed from the stream removes it; deletes always carry `resourceVersion` **and** `uid` preconditions; and a due entry whose delete returns 404/409 is dropped from the heap rather than retried forever |
| `k8s_error.rs` | The full §10 mapping table, row by row: transport → `ConnectionLost`; timeout/`504` → `Timeout`; `401`/`403` → `AuthFailure` **and not retryable**; `429` → `ResourceExhausted` with `Retry-After` honoured when present and a fallback schedule when absent; `500`/`503` → `ResourceExhausted`; `422` → `Other`; malformed kubeconfig / unresolvable namespace / invalid `lease_prefix` → `InvalidConfig` (**not** a `Provider` error — a config fault must not read as a cluster fault); `409` never produced as a `Provider` error at all |
| `client.rs` | Namespace resolution order (config → `POD_NAMESPACE` → service-account file → kubeconfig context → `InvalidConfig`), asserting **no `default` fallback**; identity resolution order (config → `POD_NAME` → hostname) and truncation past 512 chars with a WARN |
| `provider.rs` | All three `provider()` methods return `"k8s"`; each `build_*` returns `InvalidConfig` for an unknown option key and for an invalid `lease_prefix`; the two non-cache providers neither receive nor consult a cache backend (the SDK's "non-cache providers do not receive the cache backend" contract), which here is load-bearing rather than incidental since both are native; and all three provider traits **are** implemented — asserted structurally, so dropping one later is a deliberate change to DESIGN §13 D1 rather than a quiet one |

No Kubernetes API call is executed at layer 1. All request behaviour is covered at
layer 3.

## 3. Layer 2 — Conformance Suite

`cf-gears-cluster-conformance` is a `[dev-dependencies]` entry;
`tests/conformance.rs` wires the layer-3 cluster fixture into every applicable
entry point. Each suite goes through one `run_*_conformance(factory, time)` call
whose async factory returns a `cluster_conformance::ScenarioBackend` that **owns
the plugin handle** and stops it via teardown before the next scenario is built —
mandatory, not cosmetic: the handles panic on drop if never `stop()`ed (DESIGN
§3.2's ADR-006 guard).

```rust
// tests/conformance.rs

#[tokio::test]
async fn leader_conformance() {
    let (cluster, config) = start_k3s().await;
    run_leader_conformance(
        || async {
            let handle = K8sLeaderElectionPlugin::builder(fresh_namespace(&config).await)
                .build_and_start()
                .await
                .expect("plugin starts against the test cluster");
            let leader = handle.leader_election();
            ScenarioBackend::with_teardown(leader, async move { handle.stop().await })
        },
        // A real backend gets real (bounded) time, never a paused clock: `kube`'s
        // own request timeouts and watcher backoff timers run on the same runtime
        // and a paused/auto-advancing clock fires them spuriously.
        TimeControl::Real,
    )
    .await;
    drop(cluster);
}
```

`cache_conformance` and `lock_conformance` are wired
identically over `K8sCachePlugin` and `K8sLockPlugin` — the same standalone,
independently routable shapes `ClusterCacheProvider::build_cache` and
`ClusterLockProvider::build_lock` use in production
(DESIGN §3.5), not a shortcut through the combined handle.

`cache_conformance` is the one with a fixture prerequisite: the CRD must be applied
to the cluster before the first `K8sCachePlugin::build_and_start`, or the startup
canary (DESIGN §6.7) fails every scenario identically. `start_k3s()` applies it once
per container (§4.1), which also means the **whole suite depends on
`deploy/crd.yaml` being valid** — a manifest error surfaces as every cache scenario
failing at construction rather than as an assertion, so the fixture asserts the
apply succeeded before handing back a config.

**Per-scenario isolation is a fresh Kubernetes namespace**, created by
`fresh_namespace()` and deleted by the teardown. A shared namespace with
per-scenario `lease_prefix`es was the alternative and is worse for the same
reason the Redis plugin rejected prefix isolation as a default: a prefix bug would
leave one scenario able to see another's objects through the
reaper's list, which is exactly the class of bug
`K8S-SPEC-004` looks for. Namespaces are cheap to create and are the API server's
own isolation boundary, so the stronger option is also the simpler one.

**Not run, and why:**

- **`run_watch_lifecycle_conformance` / `run_restart_conformance`** — both take no
  backend factory: they are SDK-level structural and channel-harness suites over
  `RestartingWatch` and the watch unions. Running them here would exercise
  `cluster-sdk` code with no K8s involvement. The plugin's own `Reset` / `Closed`
  behaviour against a real API server is `K8S-WATCH-*` and `K8S-FAULT-*` instead.
- **`SC-SCOP-001..006`** — the scoping wrappers are pure decorators over
  `Arc<dyn *Backend>` that only ever call the generic trait interface, so the
  wrapped backend could be K8s or a stub and the prefix apply/strip/compose logic
  is identical. They have SDK-level unit tests against recording stubs. There *is*
  a K8s-specific interaction — a composed scope prefix lengthens the name that
  DESIGN §2.2 must map — and it is covered directly by `naming.rs` (§2) and
  `K8S-SPEC-001`, which is the layer that can assert the mapping's properties
  rather than only that a scoped call round-trips.
- **Routing conformance** — `run_routing_conformance` does not exist, and
  per-primitive routing (`cpt-cf-clst-fr-routing-per-primitive`) is wiring-crate
  logic (`ClusterWiring::from_config` dispatching through `ProviderRegistry`), not
  backend logic any plugin implements. It belongs to the `cluster` gear's own
  suite, plus this plugin's two routing-adjacent integration tests —
  `K8S-SPEC-005` (all three primitives on `k8s`) and `K8S-SPEC-006` (mixed, in both
  directions). Serving all three lets it
  put the *same* provider on both sides of a mix, which is the only arrangement
  that meaningfully exercises per-primitive dispatch.

**Capability-gated assertions.** The suites read `features()` / `consistency()`
before running scenarios. Only one declaration varies by configuration here
(DESIGN §3.7), which makes the set worth stating:

- `LeaderElectionFeatures::linearizable == true` → every single-leader-under-
  contention scenario runs, and the `LeaderElectionCapability::Linearizable`
  mismatch scenario does **not**. This is the first backend for which that check's
  *positive* branch is exercised with no configuration caveat.
- `LockFeatures::linearizable == true` → the lock-contention correctness scenarios
  run.
- `CacheConsistency::Linearizable` under the default `reads: quorum` → the
  linearizability-dependent cache scenarios run and the
  `CacheCapability::Linearizable` mismatch scenario does not. The suite is **also
  run once with `reads: cached`** (`cache_conformance_cached`), where the
  declaration is `EventuallyConsistent` and the gating inverts — the only place in
  this plugin where an operator config moves a declaration, so both branches are
  exercised rather than just the default (DESIGN §6.5, `K8S-CACHE-011`).
- `CacheFeatures::prefix_watch == true` → `SC-CACHE-013` runs the native
  prefix-watch path and `SC-CACHE-014` (the `PollingPrefixWatch` polyfill) returns
  immediately, since it is gated on `!prefix_watch`. Worth naming because it means
  the polyfill's 25 ms poll loop never runs against this backend — which would
  otherwise have been the heaviest API-server load in the suite.

**The model-based cache suite runs too.** `cluster_conformance::replay_against_model`
drives generated `CacheOp` sequences against the backend and checks the reference
model's invariants after every op — a present entry's version is never 0, a mutating
op strictly increases it, a **non-mutating op leaves it unchanged**, and CAS succeeds
iff `expected_version` matches. That third invariant is the one worth having here:
it is exactly the property a version derived from `resourceVersion` or
`metadata.generation` would violate (DESIGN §2.7, §6.1), and the model finds it on
sequences a fixed example would miss.

**One skipped scenario and where it went.** `run_leader_conformance` skips
`SC-LEAD-006` under
`TimeControl::Real`, because it induces a TTL lapse by fast-forwarding virtual
time. The property matters here — transient leader loss with auto-reenroll — so it
is covered as a plugin-specific
L3 scenario (`K8S-LEAD-008`) with a real short TTL against the real
cluster. That is arguably the *better* layer for it: the shared scenario proves
the SDK's state machine handles a lapse, while the L3 version proves a real API
server and a real `Observed` record produce the lapse in the first place.

One fixture note the suites depend on: DESIGN §2.8 floors election TTLs at
`min_election_ttl` (default 5 s) to keep renewal rates off the API server. The
conformance `ElectionConfig::default()` is a 30 s TTL and clears it comfortably;
`K8S-LEAD-008`'s deliberately short TTL requires the fixture to lower
`min_election_ttl_ms`, which is why it is a config field rather than a constant.

## 4. Layer 3 — Integration Tests (testcontainers k3s)

### 4.1 Container Setup

`testcontainers-modules` needs its `k3s` feature added to the workspace
`Cargo.toml` (its existing feature list is `postgres` and `mysql`). One fixture,
plus a namespace factory:

```rust
// tests/common/mod.rs

/// A single-node k3s cluster: a real API server backed by real etcd, which is
/// the only thing this plugin needs. Started once per test binary and shared,
/// because k3s takes ~15-25s to become ready and every scenario isolates itself
/// by namespace instead. Applies `deploy/crd.yaml` before returning and asserts
/// the CRD becomes `Established`, so a cache scenario never races the install
/// (§3) — and so a broken manifest fails the fixture with a clear message rather
/// than every cache scenario at construction.
pub async fn start_k3s() -> (ContainerAsync<K3s>, K8sClusterConfig);

/// Creates a fresh namespace and returns a config pointing at it, plus the
/// Role/RoleBinding the plugin needs (§7). The returned guard deletes the
/// namespace on drop, which cascades to every Lease in it.
pub async fn fresh_namespace(base: &K8sClusterConfig) -> NamespaceGuard;

/// A config whose client authenticates as a ServiceAccount with a deliberately
/// reduced verb set, for the RBAC scenarios (`K8S-LIFE-004`, `K8S-SPEC-008`).
pub async fn restricted_config(ns: &NamespaceGuard, verbs: &[&str]) -> K8sClusterConfig;
```

`K3s::default().with_conf_mount(...)` writes a kubeconfig the fixture reads via
`read_kube_config()`, rewrites the server URL to the mapped host port, and hands
to `kube::Config::from_kubeconfig`. The k3s container needs privileged mode
(the module handles it), which is the one CI requirement beyond a Docker daemon.

**Why k3s rather than kind or envtest.** k3s is a `testcontainers-modules` image,
so it composes with the same fixture pattern every other cluster plugin uses and
needs no host tooling — where `kind` needs the `kind` binary and its own docker
network management, and `envtest` needs `setup-envtest` to download control-plane
binaries per platform. Both alternatives are faster to start (envtest markedly so,
since it runs no kubelet), and if the ~20 s k3s startup becomes the binding CI
cost, envtest is the migration to make — the plugin talks to nothing but an API
server, so the fixture is the only thing that would change. Recorded in §8.

`K8S-SPEC-008` runs against a **second** namespace with a restricted
ServiceAccount rather than a second cluster, which is why one fixture suffices.

### 4.2 Cache Integration Scenarios

These mirror the conformance scenarios (§3) with assertions on the actual
`ClusterCacheEntry` objects, plus the mechanisms conformance cannot see.

| ID | Scenario | What it verifies |
|---|---|---|
| `K8S-CACHE-001` | `put` + `get` round-trip | Value and version stored and retrieved; the underlying object is a `ClusterCacheEntry` whose `spec.value` base64-decodes to the exact bytes, `spec.version` is 1, and whose labels (`managed-by`, `primitive=cache`) and `name` annotation carry the **original** unmapped key — asserted at the server, so a future re-encoding is a test failure rather than a silent wire change (DESIGN §2.6) |
| `K8S-CACHE-002` | Version is ours, monotonic, and reset-on-recreate | Each `put` increments `spec.version` by exactly 1 read straight off the object; `put_if_absent` creates at 1; delete-and-recreate returns to 1; and `metadata.resourceVersion` is observed to move by amounts **unrelated** to the version, which is the positive demonstration that we are not exposing it (DESIGN §2.7). The regression test for the decision this whole primitive turned on |
| `K8S-CACHE-003` | A byte-identical `put` still bumps the version | `put` twice with the same value: `spec.version` goes 1 → 2 and the entry's `resourceVersion` also changes. Guards the no-op-`PUT` trap: a design leaning on `resourceVersion` or `metadata.generation` would leave both unchanged and silently violate the contract's strictly-increasing-on-mutation invariant (DESIGN §6.1) |
| `K8S-CACHE-004` | `compare_and_swap` under concurrent writers | 20 concurrent tasks CAS the same key from the same expected version; exactly one succeeds and 19 get `CasConflict`, each carrying a populated `current` obtained from the same read that detected the mismatch — asserted alongside a request count showing no second round trip was made for it (DESIGN §6.1) |
| `K8S-CACHE-005` | A conflicted CAS writes nothing | After 19 conflicts, `spec.version` advanced by exactly 1 and `resourceVersion` changed exactly once. The invariant the conformance model also checks, held here against the real object so a "retry the CAS internally" regression is caught |
| `K8S-CACHE-006` | TTL expiry is deadline-armed, not interval-polled | A 50 ms TTL produces an `Expired` watch event within ~150 ms and the object is gone — with **no sweep interval configured at all**. Then the negative half: with `cache_watch: false`, the same TTL expires on the fixed-interval fallback and takes proportionally longer. This is the mechanism `SC-CACHE-010` depends on, asserted directly rather than only through its effect (DESIGN §6.2) |
| `K8S-CACHE-007` | Expiry is enforced on the read path, independent of the sweeper | With the sweeper task killed, an entry past its `expiresAt` reads as `None` from `get` and `contains` and is absent from `scan_prefix`, **while the object still exists** in the API server. The single sharpest statement of DESIGN §6.2's central property: a stopped, throttled, or lagging sweeper cannot cause a stale read |
| `K8S-CACHE-008` | `Ttl::Indefinite` is never swept | An entry written with `Indefinite` has no `spec.expiresAt`, never enters the sweeper's heap, and is still present after many multiples of any TTL in the suite (`SC-CACHE-011`'s real-cluster counterpart) |
| `K8S-CACHE-009` | `compare_and_delete` is atomic and value-guarded | Match → deleted; mismatch → `Ok(false)` with **no request issued** beyond the read; and the guarded delete carries `resourceVersion` + `uid` so a delete-and-recreate between read and delete leaves the successor's object intact. The SDK's default for this method is explicitly best-effort; this asserts the override actually closes the window (DESIGN §6.1) |
| `K8S-CACHE-010` | `put_if_absent` is one round trip and does not overwrite | On a live entry it returns `None`, leaves value and version untouched, and — asserted via the request counter — issues exactly one request (the `CREATE` that got `409 AlreadyExists`), never a read (DESIGN §6.1) |
| `K8S-CACHE-011` | `reads: cached` downgrades the declaration and the reads | With `cache_reads: cached`: `consistency()` is `EventuallyConsistent`, `cluster.provider.weak_consistency` (WARN) is logged once, `GET`s carry `resourceVersion=0` (asserted from the request log), and a consumer requiring `CacheCapability::Linearizable` against that profile fails resolution with `CapabilityNotMet`. Also that the wiring's strict `CasBasedLeaderElectionBackend::new` **refuses** to auto-wrap it. The honest-declaration mechanism working in the one place this plugin lets an operator move a declaration (DESIGN §6.5) |
| `K8S-CACHE-012` | Oversized values are refused locally | A value one byte over `max_value_bytes` returns `InvalidConfig` naming the limit and the actual size with **zero requests issued**; one at the limit succeeds and round-trips. Failing locally is the point — the API server's `413`/`422` says nothing about cache values (DESIGN §6.6) |
| `K8S-CACHE-013` | The cache's writer-clock TTL exception has not leaked onto a lease | Two instances with deliberately skewed timestamp sources: the cache entry's `expiresAt` reflects the *writer's* clock and expires by the *reader's* — the documented, accepted precision defect (DESIGN §6.2) — while in the same run an election on the same skew arbitrates identically to the no-skew case. The pair is the assertion: the exception is bounded to where it was intended, and `K8S-SPEC-007`'s guarantee is unaffected |
| `K8S-CACHE-014` | Cache request volume per operation is exactly as documented | Against DESIGN §6.1's table: `get`/`contains`/`delete`/`put_if_absent` are 1 request; `compare_and_swap`/`compare_and_delete` are 2; `put` is 1 on create and 2 on overwrite; `watch`/`watch_prefix` are **0**. Asserted as counts. A `get` that grows a second round trip, or a `watch` that starts issuing a request per subscriber, passes every behavioural test and fails this one |
| `K8S-CACHE-015` | `scan_prefix` paginates, filters, and excludes expired | 1200 entries under one prefix returned across three pages with no request carrying `limit > 500`; entries under a different prefix excluded; expired-but-not-yet-swept entries excluded; and the returned keys are **original** keys, not mapped object names. Also asserts a real `LIST` is issued rather than the watcher's in-memory index being consulted (DESIGN §6.4) |
| `K8S-CACHE-016` | A missing CRD is a config error, not a missing key | With the CRD deleted from a running cluster, a cache `get` returns `InvalidConfig` naming `deploy/crd.yaml` — **not** `Ok(None)`, which would silently report every key as absent. The one runtime path DESIGN §10 adds specifically because the startup canary cannot cover a CRD removed later |

### 4.3 Leader Election Integration Scenarios

These mirror the conformance scenarios (§3) with K8s-specific assertions on the
actual Lease objects.

| ID | Scenario | What it verifies |
|---|---|---|
| `K8S-LEAD-001` | `elect` acquires and reports `Leader` | A single candidate becomes leader; the Lease exists with `holderIdentity` = our identity, `leaseDurationSeconds` = 30, non-null `acquireTime` and `renewTime`; the object carries the `managed-by` and `primitive=election` labels and the `name` annotation with the **original** unmapped name (DESIGN §2.3, asserted at the server so a future re-encoding is a test failure rather than a silent wire change) |
| `K8S-LEAD-002` | 10 concurrent candidates, exactly one leader | Ten `elect()` calls on one name across ten independent plugin instances; exactly one observes `Leader` and nine observe `Follower`; exactly one Lease exists and its holder is the winner. The `cpt-cf-clst-nfr-leader-guarantee` assertion for the backend ADR-009 rates safe unconditionally |
| `K8S-LEAD-003` | Renewal keeps leadership past the TTL | With a 6 s TTL (2 s renewal), the leader still reports `Leader` after 15 s, and `renewTime` advanced at least six times. Asserts the renewal loop actually runs rather than the TTL merely being long |
| `K8S-LEAD-004` | Acquisition is `resourceVersion`-guarded — the 409 path | Two candidates read the same free Lease, then both attempt the replace: exactly one succeeds and the other receives a 409 and becomes a follower. `cluster_k8s_conflicts_total{primitive="election"}` increments by exactly 1. **The test that fails if anyone converts the guarded `replace` to an unconditional `patch`** (DESIGN §2.6), which is the single easiest way to introduce split-brain into this plugin |
| `K8S-LEAD-005` | Failover on holder death | The leader's plugin is `stop()`ed without resigning (a `stop` performs no remote release — DESIGN §11); a follower takes over after ~one lease duration, `leaseTransitions` incremented, and the new holder's identity is in the Lease. Also asserts the Lease object is **still present** immediately after the stop with the dead holder's identity — the claim lapses, the object does not (DESIGN §11) |
| `K8S-LEAD-006` | `resign` hands over within a round-trip | The leader resigns; the Lease's `holderIdentity` is null within one request, and a follower acquires in well under one lease duration — the `cpt-cf-clst-fr-leader-resign` promise, measured against the TTL it is supposed to beat. The resigner observes `Status(Lost)` |
| `K8S-LEAD-007` | A follower issues no writes | While a leader renews for 15 s, five followers' request counters show **zero** mutating verbs and exactly one watch each. The DESIGN §12 "no primitive writes when it is not the holder" property, which is what keeps N candidates from being N writers |
| `K8S-LEAD-008` | Transient loss re-enrolls without consumer code | The L3 counterpart to the skipped `SC-LEAD-006` (§3). With a short TTL and `min_election_ttl_ms` lowered, the leader's Lease is overwritten out-of-band by a third party; the leader observes `Status(Lost)` and then, with no re-enrollment code, a subsequent `Leader` or `Follower` — and never a terminal `Closed` (`cpt-cf-clst-fr-leader-observability`: loss is transient) |
| `K8S-LEAD-009` | A 409 on renewal loses leadership immediately | A third party takes the Lease while the leader is between renewals: the next renewal 409s and `Status(Lost)` is emitted on that attempt, **not** after `max_missed_renewals` more intervals. The narrowest split-brain window this design has, held to one renewal interval (DESIGN §4.2) |
| `K8S-LEAD-010` | Sub-`min_election_ttl` config is rejected at the call | `elect_with_config` with a 1 s TTL returns `InvalidConfig` naming the derived renewal rate, rather than silently becoming a 3-writes-per-second load generator (DESIGN §2.8). With `min_election_ttl_ms` lowered, the same call succeeds — so the floor is a policy, not a hard limit |
| `K8S-LEAD-011` | `election_lease_names` pins a pre-existing object | With the override set for an election, the plugin contends on the literal configured Lease name and creates nothing under the mapped name — the rolling-migration escape hatch (DESIGN §14) exercised end to end, including that a candidate *without* the override lands on a different object (which is precisely the hazard the field exists to avoid) |

### 4.4 Lock Integration Scenarios

| ID | Scenario | What it verifies |
|---|---|---|
| `K8S-LOCK-001` | `try_lock` acquires and `release` frees | The Lease exists with `holderIdentity` = `<identity>#<uuid>` and the `ttl-ms` annotation; a second `try_lock` — **from the same instance**, arbitrated by the same guarded write a foreign one would hit — returns `LockContended`; after `release` the object still exists with a **null** holder and the name is immediately re-acquirable (DESIGN §5.5's clear-not-delete, asserted at the server) |
| `K8S-LOCK-002` | `lock` with timeout | A blocked `lock` returns `LockTimeout` (not `Provider`, not `LockContended`) after the budget elapses, and leaves nothing behind: the name is acquirable the moment the holder releases |
| `K8S-LOCK-003` | `lock` wakes on an explicit release | A blocked `lock` with a 30 s budget acquires within a few hundred milliseconds of the holder's `release`, far inside the holder's TTL. A wake measured at ~one TTL means the watch notification was *missed*, not merely slow, which is what makes this assertion sharp rather than a latency check — and it is why DESIGN §5.3 establishes the watch before the first attempt rather than after the first `LockContended` |
| `K8S-LOCK-004` | An expired lease is reclaimed with no cooperation | A holds a 2 s lock and never renews or releases; B acquires it once A's `Observed` deadline passes. A's subsequent `renew` reports `LockExpired`. No reaper is involved — reclamation is the next acquirer overwriting a lapsed claim (DESIGN §5.2) |
| `K8S-LOCK-005` | `renew` extends the lease | After `renew(new_ttl)`, `renewTime` advanced and both `leaseDurationSeconds` and `ttl-ms` reflect the new TTL; the lock is still held past the original deadline |
| `K8S-LOCK-006` | `renew` and `release` are token-fenced | A's lease lapses and B acquires the same name. A's `renew` → `LockExpired`; A's `release` → `Ok(())` that issues **no write** and leaves **B's** holder intact (verified by reading the object's `holderIdentity` and `resourceVersion` before and after). The one test that would fail on the classic release-deletes-unconditionally bug |
| `K8S-LOCK-007` | 20 concurrent local acquirers, at most one holder | Exactly one succeeds, 19 get `LockContended`. Kept distinct from `K8S-LOCK-008` even though both exercise the same guarded write: that local and cross-instance contention are arbitrated *identically* is the claim worth holding both halves to, and a regression adding an in-process fast path (DESIGN §5.1) shows up here first |
| `K8S-LOCK-008` | Two instances cannot hold the same lock | Two independent plugin instances: A acquires, B's `try_lock` returns `LockContended`, exactly one Lease exists and its holder is A's token, and B acquires as soon as A releases. The cross-replica guarantee the primitive rests on |
| `K8S-LOCK-009` | Sub-second TTLs are honoured at millisecond precision | A 300 ms lock is genuinely re-acquirable ~300 ms later (not 1 s later), while `leaseDurationSeconds` on the object reads `1` — the §2.8 asymmetry, both halves asserted together. This is also what lets the conformance lock suite's timing scenarios pass under `TimeControl::Real`'s 500 ms `elapse` cap |
| `K8S-LOCK-010` | Held locks consume no connections and no tasks | Hold 50 locks at once from one plugin instance: all 50 succeed, all 50 Leases exist, a `renew` on any of them still completes, and the process's task count is unchanged from the zero-locks baseline (DESIGN §3.3) |
| `K8S-LOCK-011` | `lock()` after `stop()` answers `Shutdown` immediately | After a clean `stop()`, `lock(name, ttl, 30s)` returns `ClusterError::Shutdown` in well under a second rather than retrying a torn-down backend for its whole budget and reporting `LockTimeout` — which would leave a caller unable to tell "someone else holds it" from "this backend is gone". `try_lock` asserted alongside |
| `K8S-LOCK-012` | `stop()` leaves held claims to lapse, and says so | Hold three locks with a 10 s TTL, `stop()`, then assert the three Leases are **still present with our holder identity**, and that the names become acquirable once the deadline passes. Deliberately asserts that claims are *left behind*: `cpt-cf-clst-fr-shutdown-ttl-cleanup` forbids best-effort remote cleanup on shutdown. A future "tidy up on stop" change would fail here, which is the point |
| `K8S-LOCK-013` | Released lock objects are reaped, held ones are not | With `lock_object_retention_ms` lowered, a released lock's empty Lease is deleted by the reaper while a **held** lock's Lease of the same age is untouched; `cluster_k8s_reaped_total{primitive="lock"}` agrees with the count. The bound on DESIGN §5.5's accepted object leak |

### 4.5 Watch Integration Scenarios

| ID | Scenario | What it verifies |
|---|---|---|
| `K8S-WATCH-001` | Leader watch reports transitions and nothing else | Over 20 s with a leader renewing every 2 s, a follower's `LeaderWatch` receives exactly the transition events and **zero** events for the renewals. A raw watcher forwarding every `Apply` would deliver ten — this is the dedup DESIGN §4.3 requires, held to `cpt-cf-clst-nfr-watch-delivery`'s no-duplicates rule |
| `K8S-WATCH-002` | No `Lagged` is ever emitted from a K8s watch signal | Under a forced re-list with hundreds of objects, the subscriber receives `Reset` and never `Lagged`. The K8s watch protocol has no drop count (DESIGN §4.3); a `Lagged` here would be fabricated. Asserted so a future "let's estimate the drop count" change has to argue with a test |
| `K8S-WATCH-003` | `Closed(Shutdown)` before `stop()` returns | Every active leader watch and cache watch observes the terminal `Closed(ClusterError::Shutdown)` before `stop().await` resolves, and a leader observes `Status(Lost)` **first** (`cpt-cf-clst-fr-shutdown-revoke`, DESIGN §11) |
| `K8S-WATCH-004` | The cache uses exactly one watch stream for the whole keyspace | Ten `watch(key)` subscribers on ten different keys plus five `watch_prefix` subscribers: the process holds **one** watch connection for `ClusterCacheEntry` (asserted via the request counter's `watch` verb), and all fifteen receive their own events and nothing else. The claim DESIGN §6.3 and §3.3 both rest on |
| `K8S-WATCH-005` | Cache events are exactly one per mutation, and typed correctly | 100 sequential `put`s on one key produce 100 `Changed` in order with no gaps or duplicates (`cpt-cf-clst-nfr-watch-delivery`); a `delete` produces `Deleted`; a TTL lapse produces `Expired` and **not** `Deleted`; a `compare_and_delete` that mismatches produces nothing; and a label-only server-side edit produces nothing (DESIGN §6.3) |
| `K8S-WATCH-006` | A cache re-list surfaces as one `Reset` and rebuilds the sweeper index | Force a `410 Gone` on the cache watcher with 200 live entries, 50 of them with deadlines: subscribers receive exactly one `Reset` (not 200 `Changed`), `cluster_watch_resets_total{primitive="cache"}` increments by 1, and the 50 deadlines are still honoured afterwards — i.e. the sweeper's heap was rebuilt from the `InitApply` payloads rather than silently emptied (DESIGN §6.2, §6.3) |

### 4.6 Lifecycle Integration Scenarios

| ID | Scenario | What it verifies |
|---|---|---|
| `K8S-LIFE-001` | `build_and_start` authenticates, preflights, and reports | Returns `Ok` against the fixture; `cluster.provider.started` (INFO) names the namespace, identity, server version, and **which source supplied** namespace and identity (DESIGN §3.6); no Lease exists yet — startup creates nothing |
| `K8S-LIFE-002` | `build_and_start` is idempotent and creates nothing | Called twice against the same namespace; the second succeeds and the object inventory is unchanged. There is no schema to create and no migration to re-run |
| `K8S-LIFE-003` | Unresolvable namespace is a config error, not a fault | With no `namespace`, no `POD_NAMESPACE`, no service-account file, and no kubeconfig namespace: `InvalidConfig`, and specifically **not** a fallback to `default` (DESIGN §3.6 — silently coordinating in `default` is a cross-tenant collision) |
| `K8S-LIFE-004` | Missing RBAC fails startup with an actionable error | A ServiceAccount granted `get`/`create` but not `update`: `build_and_start` returns `InvalidConfig` naming the verb, the resource, the namespace, and the service account — **at boot**, not as a `403` on the first renewal minutes later (DESIGN §3.4). The single highest-value thing the preflight buys |
| `K8S-LIFE-005` | Preflight verbs are scoped to the enabled primitives | `K8sLeaderElectionPlugin` starts successfully against a ServiceAccount with **no** `list` or `delete`, while `K8sLockPlugin` against the same account fails naming `list` — which its reaper needs. An operator granting the minimum for one primitive is not told to grant the others' (DESIGN §3.4, §7) |
| `K8S-LIFE-006` | A denied preflight degrades, loudly | With `create` on `selfsubjectaccessreviews` denied: `build_and_start` returns `Ok`, `cluster.provider.rbac_unverified` (WARN, once) is logged, and the plugin works. Refusing to start because a *diagnostic* is unavailable would make the plugin unusable on a hardened cluster. `skip_rbac_preflight: true` produces the same WARN and skips the requests entirely |
| `K8S-LIFE-007` | Unreachable API server fails at startup, bounded | A valid kubeconfig pointing at a closed port returns `Provider { ConnectionLost }` inside the connect budget rather than hanging or returning `Ok` with a background retry |
| `K8S-LIFE-008` | `Drop` without `stop()` surfaces loudly (ADR-006) | Each of the three handle types dropped without `stop()`: debug build panics with the "dropped without stop()" message, release build logs the WARN; `stop()`-then-drop does neither |
| `K8S-LIFE-009` | `Drop` during panic unwind degrades to a warning | A panic inside a closure owning an un-stopped handle does not abort the process (which a debug-build double panic would) and logs the skip message instead |
| `K8S-LIFE-010` | `stop()` terminates against an unresponsive API server | Hold a leadership, three locks, and two watches, then `pause` the container so the socket stays open but nothing answers, then `stop()`: it returns inside a 30 s budget. Bounded by `request_timeout_ms` and `TASK_JOIN_TIMEOUT` (DESIGN §11), and a general claim rather than an accident of timing: every request is bounded client-side, so no in-flight call can hold a task's join open indefinitely |
| `K8S-LIFE-011` | `with_client` adopts an existing client | A plugin built with a caller-supplied `kube::Client` works identically and creates no second client — the path mini-chat and chat-engine will use post-migration (DESIGN §3.3, §14) |
| `K8S-LIFE-012` | A missing CRD fails startup with an actionable message | `K8sCachePlugin::build_and_start` against a cluster without the CRD returns `InvalidConfig` naming `clustercacheentries.cluster.cf-gears.io` and `deploy/crd.yaml` — at boot, not on a consumer's first `put`. The other two plugins start **successfully** against the same cluster, since neither touches the custom resource (DESIGN §6.7) |
| `K8S-LIFE-013` | The canary verifies the schema, not just the resource's existence | With a deliberately truncated CRD installed (schema missing `expiresAt`), `build_and_start` fails `InvalidConfig` naming the manifest, rather than succeeding and then failing on the first TTL'd write. The version-skew hazard a cluster-scoped singleton invites (DESIGN §6.7) |
| `K8S-LIFE-014` | The canary leaves nothing behind | After a successful `build_and_start`, no object matching `<prefix>-ca-preflight-*` exists; after a `build_and_start` interrupted between canary create and delete, the leftover canary carries a 60 s TTL and is gone shortly after (DESIGN §3.4) |

### 4.7 Kubernetes-specific Scenarios

The wire-format, request-volume, and declaration tests. Several are the only
coverage of DESIGN's honesty and cost claims, which no conformance scenario can
reach.

| ID | Scenario | What it verifies |
|---|---|---|
| `K8S-SPEC-001` | Adversarial names produce legal, distinct objects | Elections/locks/services named with `/` separators, uppercase, unicode, and a 4 KiB length are all created successfully; the resulting object names are legal RFC 1123 subdomains ≤ 253 chars; `a/b` and `a-b` land on **different** Leases and do not interfere; each object's `name` annotation round-trips the original exactly. The API server is the authority on legality here, which is why this cannot be a unit test alone (DESIGN §2.2) |
| `K8S-SPEC-002` | Scope composition survives the mapping | A doubly-scoped name (per-gear then per-shard, `cpt-cf-clst-fr-namespacing-scoped`) maps to a legal object; two shards of one gear do not collide; and the consumer's view stays name-relative. The one scoping interaction that is genuinely K8s-specific (§3 explains why the rest is not) |
| `K8S-SPEC-003` | `linearizable: true` is declared for both primitives | `features().linearizable` is `true` on the leader and lock backends with no configuration hint involved, and no `weak_consistency` WARN is logged. The positive branch of capability validation, which no other shipped backend reaches unconditionally (DESIGN §3.7) |
| `K8S-SPEC-004` | Two plugin instances with different `lease_prefix`es are fully isolated | In one namespace: two instances, different prefixes, same coordination names. Neither's elections nor locks are visible to the other across the lock reaper's list. A shared namespace with two independent cluster deployments is a plausible arrangement |
| `K8S-SPEC-005` | End-to-end YAML routing: all three primitives on `k8s` | Via `ClusterWiring::from_config` with `cache`, `leader_election`, and `lock` all bound to `k8s`: the resolved profile writes a cache entry, elects a leader, and takes a lock against the real cluster. Confirms all three provider registrations make `provider: k8s` resolvable per primitive, and that the recommended single-provider shape (DESIGN §3.5) actually resolves end to end from operator YAML |
| `K8S-SPEC-006` | Mixed routing: a K8s cache beside a foreign lock, and the reverse | Two profiles through `ClusterWiring::from_config`. First `cache: { provider: k8s }` with `lock: { provider: standalone }`: cache writes land as `ClusterCacheEntry` objects in the cluster while the lock is in-process. Then the inverse — `cache: { provider: standalone }` with both K8s coordination primitives — which is the `docs/DESIGN.md` §4.2 "K8s + Redis" shape with standalone standing in for Redis. Per-primitive routing is only meaningfully tested by binding the *same* plugin on both sides of a mix (DESIGN §3.5) |
| `K8S-SPEC-007` | Clock skew does not affect arbitration | Two candidates whose Lease writes carry `renewTime` values an hour apart (injected by overriding the timestamp source in one instance): leadership is arbitrated identically to the no-skew case — the fast-clock instance does **not** steal a live lease and the slow-clock one does **not** refuse a dead one. `cluster.provider.clock_skew_observed` (WARN) is logged. **The test that would fail against mini-chat's current elector** (DESIGN §2.7, §14), and the sharpest correctness claim in the plugin |
| `K8S-SPEC-008` | RBAC revoked mid-flight loses leadership rather than retrying | The leader's RoleBinding is deleted while it holds leadership: the next renewal gets a `403`, `Status(Lost)` is emitted **immediately** (not after `max_missed_renewals`), and the consumer stops acting as leader. Retrying a `403` would keep a leader that has lost the right to renew believing it leads (DESIGN §10's most-argued row) |
| `K8S-SPEC-009` | 409 conflicts are never `Provider` errors | Across forced conflicts on election acquire, lock acquire, renewal, and resign: every outcome is the primitive-appropriate one (`LockContended`, internal re-read, `Ok(())`) and `cluster_provider_errors_total` does **not** increment for any of them. A 409 misclassified as a backend fault would make every contended lock look like an outage on a dashboard (DESIGN §10) |
| `K8S-SPEC-010` | A follower's request volume is bounded and known | Over 30 s: five election followers issue 0 mutating requests and 5 watches; a leader issues exactly `30s / renewal_interval` updates. Asserted as counts, not orders of magnitude. **The regression test for DESIGN §12's top risk** — a follower that starts polling passes every behavioural test and fails this one |
| `K8S-SPEC-011` | Steady-state write rate matches the documented formula | 5 elections for 60 s: total mutating requests are within 10% of `elections/renewal` plus per-acquisition lock writes, and `cluster_k8s_api_requests_total{verb="update"}` agrees. Recorded as an artefact alongside the assertion, because the formula in DESIGN §12 is what an operator sizes their control-plane budget against and a silent drift in it is a capacity-planning bug |
| `K8S-SPEC-012` | Throughput smoke against the etcd envelope | 2 000 lock acquire/release cycles on a small bounded name set, measured and printed as an artefact rather than asserted against a threshold (a CI container's absolute numbers are not a production predictor, and single-node k3s etcd is not production etcd). Recorded because ADR-001's ~3 000–5 000 writes/sec ceiling is the reason DESIGN §6 declines a cache; an order-of-magnitude regression is what the artefact makes visible. Read with `-- --nocapture` |
| `K8S-SPEC-013` | Object inventory is bounded under churn | 500 acquire/release cycles across 20 lock names: the Lease count returns to a bounded steady state rather than growing with cycle count, and `cluster_k8s_lease_objects` agrees. The end-to-end statement that DESIGN §5.5's accepted object leak is actually bounded, as opposed to bounded in principle |
| `K8S-SPEC-014` | Unbounded lock names are warned about | With `lock_name_cardinality_warn_threshold` lowered, acquiring locks on distinct names past the threshold logs `cluster.lock.name_cardinality_high` (WARN, rate-limited) naming the count. The misuse DESIGN §5.5 says the plugin can report but not prevent, reported |
| `K8S-SPEC-016` | The full ADR-004 catalog is emitted, by this plugin alone | Drive every operation of all three primitives through an in-process OTel reader and assert every span, counter, and histogram in `docs/OBSERVABILITY.md` appears with `provider = "k8s"`, that no operation key / lock name / election name / cache key appears as a **metric label** (`METRIC_LABEL_ALLOWLIST`), and that the `op` and `result` label values stay inside their bounded sets. Load-bearing here in a way it is not for other plugins: all three primitives are native, so **nothing is emitting on this plugin's behalf** (DESIGN §9) |
| `K8S-SPEC-017` | The SDK defaults over this cache are reachable, functional, and worse | A profile binding `cache: { provider: k8s }` and omitting the other two starts successfully — the strict `CasBasedLeaderElectionBackend::new` accepts our `Linearizable` cache — elects a leader and takes a lock through the SDK defaults. Then the comparison that is the actual point: the same workload through the **native** two issues materially fewer mutating requests (asserted as counts). Documents DESIGN §12's "reachable but worse" risk as a measured fact rather than an assertion of taste, and pins the arrangement so a future wiring change cannot break it silently |
| `K8S-SPEC-018` | Two plugin instances share a CRD and share no data | Two namespaces, two plugin instances, identical cache keys: each `get` returns only its own namespace's value, `scan_prefix` never crosses, and the cache watcher in one namespace receives no events from the other (the watch is namespace-scoped). The isolation half of DESIGN §6.7's cluster-scoped-singleton trade |

## 5. Layer 4 — Fault Injection and Failover

Nightly. Toxiproxy sits between the plugin and the k3s API server; the API
server's own restartability supplies the control-plane failure cases, which are
the ones that actually probe ADR-009's claims for this backend.

| ID | Scenario | Fault | Expected behaviour |
|---|---|---|---|
| `K8S-FAULT-001` | Watch connection loss → `Reset` | Kill the watch's TCP connection | Every subscriber receives `Reset` (never a silent gap); `kube`'s watcher re-lists and resumes; subsequent events are delivered; `cluster_watch_resets_total` and `cluster_k8s_watch_relists_total` both increment |
| `K8S-FAULT-002` | Request connection loss → `ConnectionLost` | Kill a connection mid-request | `try_lock` returns `Provider { ConnectionLost }`; the next call succeeds on a reconnected connection |
| `K8S-FAULT-003` | Latency spike → `Timeout`, not a stalled task | 30 s latency, `request_timeout_ms: 2000` | A renewal *fails* at ~2 s rather than the renewal task ceasing to tick; the failure counts against `max_missed_renewals`; the task is still ticking after the latency clears. The DESIGN §12 bounded-request property, which §11's `stop()` budget depends on |
| `K8S-FAULT-004` | Renewal budget exhausted under a partition → `Lost`, then re-enroll | Blackhole for `ttl + ε`, then restore | The leader observes `Status(Lost)` after `max_missed_renewals` failures — not before — then re-enrolls with no consumer code and resolves to `Leader` or `Follower`. The transient-loss contract under a real fault rather than an injected overwrite |
| `K8S-FAULT-005` | No split-brain across a partition | 5 candidates, Toxiproxy partitions a random subset for several lease durations, then restores | Sampling every candidate's `status()` throughout: at no sampled instant do two report `Leader`. The real-backend counterpart to the non-runnable `SC-LEAD-010`, and the assertion that `linearizable: true` (DESIGN §3.7) is earned rather than declared |
| `K8S-FAULT-006` | No two lock holders across a partition | Same partition, 5 instances contending on one lock name, sampling holder identity from the API server | Exactly one holder at every sampled instant; a partitioned holder's claim lapses and exactly one successor takes it. The lock half of `K8S-FAULT-005` — and note that unlike the Redis plugin's `RD-FAULT-006`, this test asserts the guarantee *holds*, because this backend claims it |
| `K8S-FAULT-007` | API server restart → recovery without split-brain | Restart the k3s API server process | Renewals fail with `ConnectionLost`/`503` and are retried internally; if the outage exceeds the lease duration the leader observes `Lost` and re-enrolls; across the whole window at most one candidate ever reports `Leader`. Quorum-adjacent unavailability produces **no** leader, never two (DESIGN §3.7) |
| `K8S-FAULT-008` | `429` throttling delays but does not break | Toxiproxy rewrites responses to `429` with a `Retry-After` for a bounded window | Renewals back off **per `Retry-After`** rather than on the plugin's own schedule (asserted from request timing); `cluster_k8s_throttled_total` and `cluster.provider.throttled` fire; leadership survives if the window is shorter than `ttl`, and is lost-then-regained if longer. DESIGN §12's APF risk, driven rather than described |
| `K8S-FAULT-009` | A watch stuck open with no events is not mistaken for "no change" | Blackhole only the watch connection while the API server still serves requests, then have a third party take the Lease | The follower's expiry timer (DESIGN §4.1) — not the watch — drives its acquisition attempt, so a silently dead watch delays nothing beyond one lease duration. The failure mode a purely event-driven follower would sleep through forever |
| `K8S-FAULT-010` | Cache watch loss → `Reset`, and the sweeper survives it | Kill the cache watcher's TCP connection with entries holding live deadlines | Subscribers receive `Reset`; `kube` re-lists; the sweeper's heap is rebuilt and the pending deadlines are still honoured afterwards. A sweeper that silently emptied on reconnect would leak every entry written before the blip, and read-path expiry (DESIGN §6.2) would hide it |
| `K8S-FAULT-011` | Cache writes under `429` back off and do not corrupt the version | APF-style `429` with `Retry-After` during a CAS-heavy loop | CAS operations return `Provider { ResourceExhausted }` (retryable) and the caller's retry succeeds once the window clears; `spec.version` advanced by exactly the number of *successful* writes, with no gaps or double increments. The throttling path exercised where an off-by-one in the version would be a real bug |
| `K8S-FAULT-012` | Expiry survives total sweeper unavailability | Blackhole the API server for several multiples of a short TTL, then restore | Throughout the outage a `get` on an expired key returns `None` (read-path enforcement needs no server round trip for the *decision*, only for the read that fails); after restore, the backlog is swept and `cluster_k8s_cache_sweep_backlog` returns to zero. The failure mode that would be a correctness bug on a sweeper-authoritative design |

`K8S-FAULT-005`/`006` and `K8S-FAULT-007` are the tests that make this plugin's
consistency story verified rather than merely inherited from ADR-009's table: the
same code, the same scenario, under the exact failure conditions the table's
"safe" rating is conditioned on.

## 6. Static Analysis

- **`cargo check`** — no errors.
- **`cargo clippy`** — no warnings beyond the workspace allow-list.
- **No remote I/O in the lock critical section** — ADR-002's "no remote I/O inside
  the critical section" is a **documented convention**, not a lint: there is no
  `no-remote-in-lock-critical-section` dylint rule in the workspace (the only dylint
  today is `de1201_docs_rs_all_features`), and the rule lives as doc-comment prose on
  the SDK's `LockGuard` (`cluster-sdk/src/lock/mod.rs`, `lock/guard.rs`). This plugin
  follows it by construction — no `kube` request or `Api` method is issued inside a
  `LockGuard`'s lifetime scope — but the guardrail is review discipline, not CI.
- **No serde in SDK contract types** — the workspace layer rule. This plugin's
  `config.rs` uses serde; the plugin adds no serde derive to any `cluster-sdk`
  type.
- **`cargo test --doc`** — all doc examples compile and pass.
- **No unconditional writes** — a crate-local test asserting that
  `Api::patch`, `Patch::Apply`, and `Api::replace_status` appear nowhere in
  `src/`, and that every `Api::replace` and `Api::delete` call site is reached
  through `guarded.rs`'s helper. Converting a guarded `replace` to a `patch` is the
  single easiest way to introduce split-brain into this plugin (DESIGN §2.7), and
  `K8S-LEAD-004` catches it behaviourally only if the reviewer's change did not
  also touch that test — so the mechanical check is worth having alongside it.
- **No wall-clock expiry comparison on a lease** — a companion crate-local test
  asserting that no *lease* expiry decision reads `SystemTime::now()` or
  `Timestamp::now()`: every such call site must be a *write* path (stamping
  `renewTime`), a log field, or the cache's `expiresAt` computation, which is the
  one documented exception (DESIGN §6.2). The exception is named explicitly in the
  allowlist rather than left to the check's coarseness, so widening it later is a
  visible edit. DESIGN §2.8's rule is the plugin's sharpest correctness property and
  the naive alternative is what the code being migrated from does (§14), which makes
  accidental reintroduction a live risk rather than a theoretical one.
- **The CRD manifest and the Rust type agree** — `crd/cache_entry.rs`'s unit test
  (§2) compares the `CustomResource`-derived group/version/kind/plural/scope against
  the parsed `deploy/crd.yaml`. Cheap, and it catches the failure mode where a type
  change ships without the manifest change, which would otherwise surface only as
  `K8S-LIFE-013`'s schema canary in an integration run.

## 7. CI Cadence

| Layer | Trigger | Approx. duration |
|---|---|---|
| L1 unit tests | Every PR | < 5 s |
| L2 + L3 against `start_k3s` | Every PR touching Rust — `make test-cluster-k8s`, in `ci.yml`'s `integration` job | ~3–5 min (~20 s k3s startup plus the CRD `Established` wait, then namespace-isolated scenarios; the TTL-lapse scenarios and the two cache conformance passes dominate the remainder) |
| L4 fault injection and failover | Nightly; manually before a release | ~15–25 min |

L3/L4 tests are gated behind an `integration` feature so a default `cargo test`
needs no Docker. Cargo forces the mechanics:
`testcontainers`/`testcontainers-modules` are declared **optional under
`[dependencies]`** rather than `[dev-dependencies]`, because Cargo does not allow
optional dev-dependencies and the feature array must name them via `dep:`. That
placement trips `cargo-shear`'s misplaced-optional-dependency heuristic, so it
needs a `[package.metadata.cargo-shear] ignored` entry; every reference to them
lives in `tests/`, so the feature has no effect on the compiled plugin. Each test
binary carries `required-features = ["integration"]` so the binaries are not even
built without the flag:

```toml
[features]
integration = ["dep:testcontainers", "dep:testcontainers-modules"]
```

Workspace changes this plugin requires:

- `kube`'s **`runtime`** feature enabled for this crate's dependency entry.
  `kube-runtime` is absent from `Cargo.lock` today — the existing consumers use
  only `client` — and `kube::runtime::watcher` is what DESIGN §4.3, §5.3, and
  §6.3 are built on.
- `kube`'s **`derive`** feature, for the `CustomResource` derive that generates the
  `ClusterCacheEntry` type (DESIGN §2.6). Also absent today.
- `testcontainers-modules`' **`k3s`** feature added to its existing workspace
  entry, whose feature list is currently `postgres` and `mysql`.
- A `make test-cluster-k8s` target mirroring `test-cluster-pg`
  (`cargo nextest run -p cf-k8s-cluster-plugin --features integration --retries 1`).

The k3s container requires **privileged mode**, which is the one CI capability
this plugin needs beyond a Docker daemon. Runners that cannot grant it cannot run
L2/L3 — recorded in §8 with envtest as the migration if that becomes binding.

## 8. Coverage Gaps and Follow-ups

| Gap | Severity | Tracking |
|---|---|---|
| Single-node k3s etcd is not production etcd, so no test exercises real quorum behaviour | Warning — `K8S-FAULT-007` restarts a single API server, which probes unavailability but not quorum loss with a surviving minority. ADR-009's "failure mode only on etcd quorum loss" is therefore inherited from the ADR rather than verified here. A 3-node etcd fixture would verify it, at several minutes of startup per run | Follow-up, nightly-only if built |
| No test runs against a managed control plane (EKS/GKE/AKS) | Warning — APF configuration, admission webhooks, and audit-policy overhead all differ from k3s, and `K8S-FAULT-008` approximates throttling with a rewritten response rather than real APF. The approximation is close (the plugin only cares that a `429` with a `Retry-After` arrives) but is not the real environment | Accepted |
| The clock-skew scenario injects skew at the timestamp source, not at the OS | Warning — `K8S-SPEC-007` overrides one instance's timestamp source rather than skewing a container's clock, because Docker containers share the host clock and cannot be skewed independently without `libfaketime`. The injected version exercises the same code path (DESIGN §2.7 reads no foreign clock, so there is nothing else for a real skew to reach) but is not the environment | Accepted — the unit tests in §2 carry the exhaustive version |
| The cache watch stream carries every mutation to every instance, and no test measures where that becomes the binding cost | Warning — DESIGN §12 records the `N × W` events/sec amplification. `K8S-WATCH-004` proves it is *one* stream per instance, which is the part that was designable; the volume ceiling depends on API-server CPU and egress in a way a single-node k3s container cannot predict. Narrowing it would need a server-side selector on keys, which arbitrary keys cannot be | Follow-up, gated on a deployment hitting it |
| The CRD is a cluster-scoped singleton, so no test covers two plugin *versions* with incompatible schemas coexisting | Warning — `K8S-LIFE-013` covers the newer-plugin-older-CRD direction (fails at boot, actionable) and `K8S-SPEC-018` covers data isolation across namespaces. The genuinely awkward case — two live deployments needing different schemas — has no answer in v1 beyond "one of them cannot start", and testing it would only assert that. A second CRD version plus conversion is the real fix (DESIGN §12) | Accepted; follow-up if a schema change is ever needed |
| Cache TTL precision under real node clock skew is asserted with an injected timestamp source, not a skewed OS clock | Warning — same limitation as `K8S-SPEC-007` (containers share the host clock). `K8S-CACHE-013` asserts the accepted precision defect and, more importantly, that it has **not** leaked onto a lease. The residual untested thing is the magnitude of the defect under real skew, which is by definition the skew | Accepted |
| The cache's throughput envelope (DESIGN §6.8) is documented as verdicts, and no test enforces any of them | No action needed by design — a plugin cannot refuse a consumer's write rate, and a CI container's absolute numbers are not a production predictor. `K8S-CACHE-014` pins per-operation request *counts*, which is the deterministic and actionable part; `K8S-SPEC-011`/`012` publish rates as artefacts. The envelope is an operator decision informed by `cluster_k8s_api_requests_total` and `cluster_k8s_throttled_total`, which §6.8 names | N/A — documented |
| No test exercises a cache key space large enough to reproduce the K8s #47532 many-objects failure mode | Warning — `K8S-SPEC-013` asserts the inventory returns to a bounded steady state under churn, which catches leaks. It does not establish where etcd degrades, and doing so on single-node k3s would measure the container rather than production etcd | Accepted |
| `Lagged` is never produced by this plugin, so no scenario covers a `Lagged` recovery path against it | No action needed — `K8S-WATCH-002` asserts the *absence* deliberately. The K8s watch protocol has no drop count (DESIGN §4.3); the Redis plugin's `RD-WATCH-008` is where `Lagged` is exercised platform-wide | N/A — by design |
| Requires privileged Docker for k3s | Warning — a CI runner that cannot grant it cannot run L2/L3 at all. envtest (control-plane binaries, no kubelet, no privileged mode) is the migration if this becomes binding, and would also cut the ~20 s startup; the plugin talks to nothing but an API server, so only `tests/common/mod.rs` would change (§4.1) | Follow-up, gated on a runner constraint |
| The mini-chat / chat-engine migration is not covered by this plugin's tests | No action needed — the migration is an explicitly separate per-gear change (`docs/DESIGN.md` §4.3). DESIGN §14 states what those changes must handle, and `K8S-LEAD-011` covers the `election_lease_names` escape hatch the risky path depends on | N/A — separate change |
| Performance numbers are artefacts, not thresholds (`K8S-SPEC-011`, `K8S-SPEC-012`) | No action needed — per `docs/PRD.md` §6.2, quantitative per-backend SLOs are explicitly excluded from the cluster-wide NFR set and owned by each plugin's own tests. `K8S-SPEC-010`'s *request counts* are asserted, which is the part that is deterministic and the part that matters for the etcd-budget risk | N/A — documented |
| No scenario exercises cross-namespace coordination | No action needed — DESIGN §13 D5 decides against supporting it. `K8S-SPEC-018` asserts the namespace boundary holds, which is the testable half of that decision | N/A — decided |
