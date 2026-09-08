---
status: accepted
date: 2026-03-31
---

# ADR-001: Backend Compatibility and the Cache-CAS-Universal Model

**ID**: `cpt-cf-clst-adr-provider-compat-perf`

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Cache CAS as the unifying primitive](#cache-cas-as-the-unifying-primitive)
  - [Version-based vs value-based CAS](#version-based-vs-value-based-cas)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Redis](#redis)
  - [PostgreSQL](#postgresql)
  - [K8s API (Lease + CRD)](#k8s-api-lease--crd)
  - [NATS KV](#nats-kv)
  - [etcd (Direct)](#etcd-direct)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

The cluster abstraction must map cleanly to multiple backends while meeting real-world performance requirements. Different Gears have radically different coordination workloads:

- **Event broker**: Up to 1000 subscriber leases per instance, renewed every few seconds. Multi-instance deployments multiply this — 10 instances = 10,000 concurrent leases.
- **OAGW**: Distributed rate-limit counters (cache CAS) and a handful of configuration locks. Low lock count, high-frequency cache updates.
- **Scheduler**: A single leader election per pool. One lock, but correctness-critical.

These workloads span several orders of magnitude in concurrency. A provider that handles 5 leader elections perfectly may collapse under 10,000 subscriber leases. We need to select and recommend providers based on real performance characteristics, not just API compatibility.

Additionally, the subscriber lease use case (1000 concurrent reservations per instance) reveals that the right primitive is `ClusterCache` (put_if_absent + TTL renewal), not `DistributedLock`. This reframing avoids the lock scalability problem entirely.

## Decision Drivers

- Throughput: some workloads require 10,000+ ops/sec (subscriber leases, rate-limit counters), others need < 10 ops/sec (leader elections)
- Latency: sub-millisecond for hot-path cache operations, seconds-acceptable for leader election
- Infrastructure cost: Postgres and K8s are already deployed; Redis/NATS/etcd add operational overhead
- Correctness: leader election requires at-most-one guarantee; locks require fencing tokens; CAS requires atomicity
- Watch/notification overhead: backends vary dramatically in their ability to push change events at scale
- Connection model: Postgres connections are expensive (~5-10MB each); Redis connections are cheap (~20KB each); K8s API server has rate limiting

## Considered Options

1. **Redis** — high-throughput cache + locks, weak watch (keyspace notifications)
2. **PostgreSQL** — zero-new-infra, moderate throughput, strong consistency via advisory locks
3. **K8s API** — zero-new-infra on K8s, native leader election (Lease), poor throughput ceiling
4. **NATS KV** — best-in-class watch, native CAS via revisions, adds infrastructure dependency
5. **etcd (direct)** — purpose-built coordination, strong consistency, limited throughput

## Decision Outcome

Adopt a **hybrid model** where each sub-capability routes to the best backend for the workload. No single backend is optimal across all three sub-capabilities and all workload scales. The recommended combinations are:

- **Dev / single-instance**: Standalone (all three, zero deps)
- **Multi-instance, no K8s**: Postgres (cache + locks natively, election via SDK default)
- **K8s, low-throughput**: K8s (Lease for election, CRD for cache)
- **K8s + Redis (recommended production)**: Redis for cache + locks, K8s for election
- **Redis-only**: Redis for all three via SDK defaults

Per-primitive resolution with typed profile markers and per-primitive `*Capability` requirements allows consumers to declare what they need at the resolver call site. The operator maps profiles to per-primitive backend bindings. Startup validation matches the bound backend's actual characteristics (declared via `consistency()` for cache and `features()` for all three primitives) against the consumer's declared capability requirements; mismatch fails startup. (Resolver shape and capability typing are covered in detail in ADR-007.)

The event broker's subscriber lease management uses `ClusterCacheV1` (not `DistributedLockV1`) — cache CAS is the higher-throughput primitive on every backend.

### Cache CAS as the unifying primitive

The three primitives are not three independent contracts the platform binds to three independent backends. Cache CAS + watch is the *foundational* primitive — leader election and distributed locks can both be implemented on top of it. Concretely, the SDK ships two default backend implementations built solely on `Arc<dyn ClusterCacheBackend>`:

- `CasBasedLeaderElectionBackend` — `put_if_absent(election_key, node_id, ttl)` for candidacy, `watch(election_key)` for status changes, background renewal at `ttl / (max_missed_renewals + 1)`, TTL expiry → `Status(Lost)` followed by auto-reenroll.
- `CasBasedDistributedLockBackend` — `put_if_absent(lock_key, holder_id, ttl)` for `try_lock`, `watch(lock_key)` to notify blocked waiters on release, background TTL reaper, release via delete-if-still-holder using CAS.

This means a minimal plugin needs to implement only `ClusterCacheBackend` to deliver all three primitives. Native overrides exist for backends with purpose-built primitives (K8s Lease for elections; etcd's native Lock API), but they are never required. The wiring crate's omit-primitive auto-wrap behavior (covered in DESIGN §3.11) makes single-backend profiles a 1-line YAML config.

This decision is what makes "per-primitive routing" a *configurable convenience* rather than a forced complexity tax. Operators with a single backend (Postgres-only, Redis-only) get all three primitives by binding `cache` and omitting the rest. Operators with mixed needs (Redis cache + K8s Lease elections) bind explicitly. Consumers see the same `*V1` facade in every case.

### Version-based vs value-based CAS

`ClusterCacheV1::compare_and_swap` takes an `expected_version: u64` obtained from a prior `get()`, NOT an expected byte value. `get()` returns `CacheEntry { value, version }`. Backends increment the version on every successful write; version 0 is reserved as a sentinel.

Why version-based, not value-based:

- **Maps natively to every target backend**: K8s `resourceVersion`, NATS `revision`, etcd `mod_revision`, Postgres `BIGSERIAL`, Redis Lua `INCR` counter, in-process `AtomicU64`. Every backend already has a monotonic per-key counter or revision; we just expose it.
- **Value-based CAS forces racy compatibility shims on revision-based backends**. To do `compare_and_swap(key, expected_value, new_value)` on K8s, the implementation must `get(key)`, compare the value byte-for-byte, then `update(key, new_value, expected_resourceVersion=fetched_resourceVersion)`. That's a get-compare-put loop with a built-in race: a different writer can change the value to *the same expected value* between our read and write, and the CAS would still apply but our intended ordering is broken. Revision-based CAS has no such race.
- **Cheaper on the wire**. A `u64` is 8 bytes; an arbitrary value blob may be kilobytes. CAS-heavy hot paths (rate-limit counters, leader election renewals) save bandwidth and CPU.
- **Cleaner semantics for the consumer**. The version is opaque — consumers can't be tempted to interpret it (compare it across keys, use it as a logical timestamp, etc.).

Trade-off: consumers must hold the version from `get()` if they want to write back. This is straightforward in practice and matches the natural read-modify-write pattern. The ergonomic cost is one extra struct field on `CacheEntry`.

> **Correction (2026-08-06, added during the K8s plugin design).** The first bullet above is inaccurate for the backends whose revision is **cluster-global rather than per-key**, and the K8s plugin is the first backend to reach it.
>
> The decision this section records — version-based CAS, not value-based — stands unchanged, and the second bullet's use of `expected_resourceVersion` as the CAS *precondition* is correct and is exactly what the K8s plugin implements. What does not hold is "we just expose it": `metadata.resourceVersion` is a **cluster-global etcd revision**, so it neither starts at 1 for a new key nor returns to 1 when a key is deleted and re-created, both of which the cache contract requires and the conformance suite asserts (`SC-CACHE-002`, `SC-CACHE-009`). It is also documented as opaque and not to be parsed.
>
> The resolution is cheap and does not disturb this ADR's reasoning: a backend whose native revision is cluster-global maintains its own per-key `u64` counter in the same guarded write it was already issuing, and keeps the native revision in its correct role as the atomicity guard. The K8s plugin does this with a `spec.version` field on its own custom resource; see [`plugins/k8s-cluster-plugin/docs/DESIGN.md`](../../plugins/k8s-cluster-plugin/docs/DESIGN.md) §2.8.
>
> `etcd`'s `mod_revision` has the same cluster-global shape and very likely needs the same treatment; this has not been audited. `NATS revision` (per-key), `Postgres BIGSERIAL` (as used by the shipped plugin's own per-key column), Redis's Lua counter, and the in-process `AtomicU64` are unaffected.

### Consequences

- K8s-only deployments have a clear ceiling for cache/lock workloads (~5,000 writes/sec etcd practical limit). Documentation must recommend adding Redis when workloads exceed this.
- Gear authors need guidance: cache for high-count reservations, locks for low-count mutual exclusion, elections for singleton coordination. The wrong choice (locks for 1000 subscribers) performs poorly regardless of backend.
- Configuration surface increases slightly with profile-based routing and optional prefix-based overrides within a capability.
- The prefix-routing feature is optional — single-provider shorthand (`provider: redis`) remains the simplest configuration.

### Confirmation

- Integration tests per backend validate throughput and latency against documented characteristics for each primitive
- Event broker integration tests confirm subscriber lease management via `ClusterCacheV1` (not locks) sustains 10,000 concurrent leases on Redis
- K8s provider integration tests confirm Lease-based election works at 50 concurrent elections and CRD-based cache works at 100 entries
- Postgres provider integration tests confirm advisory lock performance at 1000 concurrent locks per connection

## Pros and Cons of the Options

### Redis

**Performance envelope**: 100k–200k ops/sec (SET NX EX, single node, no pipeline). ~0.15ms p50, ~0.5ms p99. 1M concurrent entries ≈ 180MB memory. Lua CAS adds ~4-5% overhead per call.

- Good, because cache and lock throughput is 10-100× higher than any other backend
- Good, because native per-key TTL via SET EX — no reaper needed
- Good, because memory overhead per lock entry is ~160 bytes — 10,000 locks costs 1.6MB
- Good, because Lua script CAS is atomic and the canonical Redis pattern
- Bad, because keyspace notifications (for watch) are disabled by default, CPU overhead is unquantified, and managed Redis may restrict CONFIG SET
- Bad, because Redis Cluster pre-7.0 PUBLISH broadcasts to ALL nodes — throughput ceiling ~12,500 publishes/sec on 10-node cluster
- Bad, because RedLock has known correctness issues (Kleppmann) — use single-node + Sentinel instead
- Neutral, because adds an infrastructure dependency (but most production stacks already have Redis)

### PostgreSQL

**Performance envelope**: ~10k-50k cache ops/sec (depends on pool size and hardware). Advisory lock acquire: ~0.01-0.05ms server-side. LISTEN/NOTIFY handles thousands of notifications/sec but has global commit lock under concurrent writers.

- Good, because zero new infrastructure — every Gears deployment has Postgres
- Good, because advisory locks are server-enforced, ACID, auto-release on disconnect
- Good, because a single connection can hold thousands of advisory locks simultaneously (~164KB for 1000 locks)
- Good, because version-based CAS maps natively to `UPDATE WHERE version = $expected` — zero Lua
- Bad, because cache throughput is 10-100× lower than Redis (bounded by connection pool and write latency)
- Bad, because LISTEN/NOTIFY has a global exclusive lock on commit — degrades under high concurrent writer + notify rates
- Bad, because each Postgres connection costs ~5-10MB server memory — scaling to many connections is expensive
- Bad, because session-level advisory locks are incompatible with PgBouncer transaction mode
- Neutral, because no native per-key TTL — needs reaper task (but straightforward)

### K8s API (Lease + CRD)

**Performance envelope**: 2-10ms per operation (API server → etcd round-trip). etcd practical write ceiling: ~3,000-5,000 sustained writes/sec under K8s workloads. etcd 8GiB database size limit.

- Good, because zero new infrastructure when running on K8s
- Good, because Lease API is purpose-built for leader election — battle-tested, used by K8s controllers
- Good, because resourceVersion is native CAS on every K8s object
- Bad, because every operation traverses API server → etcd at 2-10ms — not suitable for high-throughput caching
- Bad, because 1000 Lease objects renewed every 10s = 1000 PUTs/sec — consumes 20-33% of etcd write capacity, competes with K8s control-plane operations
- Bad, because K8s issue #47532 documented 100K+ leases causing etcd instability — the "many leases" pattern is a known anti-pattern
- Bad, because etcd issue #18109 (slow watchers blocking writes, 1-2s stalls) is still present in etcd 3.5.x
- Bad, because no native per-key TTL on CRDs — needs reaper controller
- Bad, because API Priority and Fairness may throttle custom Lease operations under load

### NATS KV

**Performance envelope**: 100k+ ops/sec (memory-backed, single-node). File-backed with R=3: 10k-50k ops/sec.

- Good, because native watch (`kv.watch(key)`) is best-in-class — built-in, event-driven, efficient
- Good, because revision-based CAS (`kv.update(key, val, revision)`) maps directly to our version-based CAS
- Good, because `kv.create(key, val)` is native put-if-absent
- Bad, because per-key TTL is bucket-level only (per-key via headers added in NATS 2.10+ but not universally supported)
- Bad, because no native distributed locks or leader election — SDK defaults required
- Bad, because adds an infrastructure dependency that most stacks don't already have
- Neutral, because throughput depends heavily on JetStream storage mode (memory vs file) and replication factor

### etcd (Direct)

**Performance envelope**: ~3,000-5,000 sustained writes/sec (K8s-like workload). <1ms p50 on SSD. Native election, lock, and watch APIs.

- Good, because purpose-built for distributed coordination — linearizable reads and writes
- Good, because native election, lock, and prefix-based watch APIs
- Good, because transactional CAS via `txn(mod_revision == expected → put)` is atomic and server-enforced
- Bad, because not designed for high-volume data caching — 8GiB database limit
- Bad, because requires dedicated infrastructure (sharing K8s etcd is dangerous — competes with control-plane)
- Bad, because each write goes through Raft consensus — replication latency scales with cluster size
- Neutral, because strong consistency guarantees are excellent for coordination but overkill for cache-tier workloads

## More Information

- [Martin Kleppmann — How to do distributed locking](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html) — RedLock correctness analysis
- [K8s issue #47532 — events create too many leases on etcd](https://github.com/kubernetes/kubernetes/issues/47532)
- [etcd issue #18109 — slow watchers block PUT latency](https://github.com/etcd-io/etcd/issues/18109)
- [KEP-589 — Efficient Node Heartbeats](https://github.com/kubernetes/enhancements/blob/master/keps/sig-node/589-efficient-node-heartbeats/README.md)
- [Recall.ai — Postgres LISTEN/NOTIFY does not scale](https://www.recall.ai/blog/postgres-listen-notify-does-not-scale)
- [Redis benchmark docs](https://redis.io/docs/latest/operate/oss_and_stack/management/optimization/benchmarks/)
- [etcd performance docs](https://etcd.io/docs/v3.4/op-guide/performance/)

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements and design elements:

- `cpt-cf-clst-fr-cache-storage` — Per-backend cache primitive realization (per-backend pros/cons inform plugin choice).
- `cpt-cf-clst-fr-leader-elect` — Leader election via cache CAS + watch + TTL when bound to a Linearizable cache.
- `cpt-cf-clst-fr-lock-acquire` — Lock primitive via cache CAS when bound to a Linearizable cache.
- `cpt-cf-clst-fr-routing-per-primitive` — Per-primitive backend routing as operator config; recommended deployment combinations document acceptable shapes.
- `cpt-cf-clst-nfr-leader-guarantee` — Per-backend safety analysis is the qualitative side; ADR-009 is the quantitative correctness counterpart.
- `cpt-cf-clst-principle-cas-universal` (DESIGN §2.1) — Cache CAS as the foundational primitive that derives the other three.
- `cpt-cf-clst-principle-per-primitive-routing` (DESIGN §2.1) — Operator-configurable per-primitive routing.
- `cpt-cf-clst-principle-version-based-cas` (DESIGN §2.1) — `expected_version: u64` rather than expected-byte-value CAS.
- `cpt-cf-clst-component-sdk` (DESIGN §3.2) — SDK ships default backend implementations built on `Arc<dyn ClusterCacheBackend>` per this ADR.
- DESIGN §3.11 SDK Default Backends — concrete implementations (`CasBasedLeaderElectionBackend`, `CasBasedDistributedLockBackend`).
- DESIGN §4.1 Backend Feature Compatibility — per-backend matrix consistent with this ADR's per-backend pros/cons.
- DESIGN §4.2 Recommended Deployment Combinations — operational guidance derived from this ADR's analysis.

**Sibling ADRs:**

- ADR-005 (facade + backend trait pattern) — the SDK-default backends are realized via this pattern.
- ADR-009 (leader election backend safety) — correctness counterpart to this ADR's performance analysis.
