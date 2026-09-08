# Technical Design — Cluster


<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
  - [1.4 Platform OoP Alignment](#14-platform-oop-alignment)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Resolution Pattern](#36-resolution-pattern)
  - [3.7 Lifecycle Pattern (Builder/Handle)](#37-lifecycle-pattern-builderhandle)
  - [3.8 Per-primitive Scoping](#38-per-primitive-scoping)
  - [3.9 Watch Event Shape](#39-watch-event-shape)
  - [3.10 Capability Validation](#310-capability-validation)
  - [3.11 SDK Default Backends](#311-sdk-default-backends)
  - [3.12 Polyfill](#312-polyfill)
  - [3.13 Interactions & Sequences](#313-interactions--sequences)
  - [3.14 Database schemas & tables](#314-database-schemas--tables)
  - [3.15 Deployment Topology](#315-deployment-topology)
  - [3.16 Remote Backend Seam](#316-remote-backend-seam)
  - [3.17 Making Cluster a Deployable Gear](#317-making-cluster-a-deployable-gear)
  - [3.18 Runtime Profile Management](#318-runtime-profile-management)
  - [3.19 Store-Owned Leases](#319-store-owned-leases)
  - [3.20 Wire Contract & API Projection](#320-wire-contract--api-projection)
- [4. Additional Context](#4-additional-context)
  - [4.1 Backend Feature Compatibility](#41-backend-feature-compatibility)
  - [4.2 Recommended Deployment Combinations](#42-recommended-deployment-combinations)
  - [4.3 Existing Code Migration](#43-existing-code-migration)
  - [4.4 Consumer & Gear End to End](#44-consumer--gear-end-to-end)
- [5. Traceability](#5-traceability)
- [6. Risks / Trade-offs](#6-risks--trade-offs)
- [7. Open Questions](#7-open-questions)
- [Appendix — Invariants](#appendix--invariants)

<!-- /toc -->

## 1. Architecture Overview

> **Open: backend authentication and credential wiring.** How cluster plugins (Redis, Postgres, K8s, NATS, etcd) acquire credentials for their backend connections is **not yet established** and is intentionally out of scope for this design. The shape (`secret_ref` on each backend config struct, resolved via the credstore plugin at start; K8s falling back to `kube-rs`'s in-cluster service-account / kubeconfig chain) is sketched but the concrete wiring, startup ordering, and per-backend mTLS/SASL/IAM specifics are deferred to the broader **OOP (out-of-process) deployment design**, where cluster meets the rest of the platform's credential and transport story (TLS termination, identity propagation, secret rotation). Treat any credential references below as placeholder shape, not committed contract.

### 1.1 Architectural Vision

Cluster is a platform-level system gear that provides cluster coordination and shared-state primitives to all Gears. It exposes three independent primitives — distributed cache (KV with TTL, version-based CAS, watch notifications), leader election, and distributed locks with TTL-bounded mutual exclusion — each as a versioned public-API facade struct (`ClusterCacheV1`, `LeaderElectionV1`, `DistributedLockV1`) wrapping a plugin-implemented backend trait (`ClusterCacheBackend`, `LeaderElectionBackend`, `DistributedLockBackend`). Plugins register their backend implementations in ClientHub per profile per primitive; consumers resolve via per-primitive fluent resolvers.

The architecture follows the ToolKit Gateway + Plugins pattern (same as authn-resolver, authz-resolver, credstore, tenant-resolver). An SDK crate (`cf-cluster-sdk`) defines the facade structs, backend traits, and resolver builders. The wiring — delivered in the `cf-gears-cluster` gear crate (§3.7 amendment: collapsed rather than a separate `cf-cluster`) — handles ClientHub registration, per-primitive provider dispatch, and plugin orchestration via the outbox-style builder/handle pattern. Backend-specific implementations ship as plugin crates under `plugins/`; `standalone-cluster-plugin` and `postgres-cluster-plugin` are shipped, with K8s, Redis, NATS, and etcd as follow-up changes.

The key architectural differentiator is **per-primitive backend routing as operator config**. Each profile in platform YAML maps each primitive to a specific plugin's backend impl independently. Operators can run Redis for cache and K8s Lease for leader election — all in the same profile, registered side-by-side in ClientHub under that profile's scope. There is no runtime compositor object; the wiring crate iterates the config and registers each backend independently.

The SDK also ships **default backend implementations** of leader election and distributed lock built entirely on `ClusterCacheBackend` CAS operations. This means a minimal plugin only needs to implement the cache backend trait — the SDK builds the other two on top. Native plugin backends override the defaults when a backend excels (e.g., K8s Lease for elections). Operators opt into SDK defaults by **omitting** the primitive in YAML; explicit binding always wins.

Lifecycle is owned by a parent host gear via the **outbox-style builder/handle pattern**. The wiring crate is NOT registered as its own `RunnableCapability` — it's a library exposing `ClusterWiring::builder(...).build_and_start() -> ClusterHandle`. The parent host gear's `RunnableCapability::start` calls `build_and_start()`; its `RunnableCapability::stop` calls `handle.stop()`. Plugins are nested builder/handle pairs owned by the cluster handle, NOT separate `RunnableCapability` implementors. Code-flow ordering inside the parent gear's `start` removes the need for a framework-level dependency mechanism between wiring and plugin lifecycles.

Explicit pub/sub messaging is excluded. The event broker gear provides reliable pub/sub with delivery guarantees, consumer groups, offsets, and replay. The cluster provides reactive cache notifications (watch by key or prefix) for data-change observation — "this data changed" vs "deliver this message reliably".

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| Cluster-wide shared state for gears | `ClusterCacheV1` with version-based CAS, TTL, and watch notifications |
| Worker pool coordination (event broker, schedulers) | `LeaderElectionV1` with watch-based status model and automatic renewal |
| Distributed rate limiting (OAGW) | `DistributedLockV1` with TTL and explicit async release |
| Multiple infrastructure backends per profile | Per-primitive backend routing as operator config; per-primitive ClientHub registration; no runtime compositor |
| Zero-infrastructure dev/test | SDK ships with in-process stub backends for smoke tests; production standalone plugin is a follow-up change |

#### Architecture Decision Records

| ADR | Summary |
|-----|---------|
| `cpt-cf-clst-adr-provider-compat-perf` (ADR-001) | Provider compatibility and performance analysis — per-primitive routing as operator config, per-backend characteristics, prefix-based routing, subscriber leases as cache not locks |
| `cpt-cf-clst-adr-async-boundary-no-remote-critical` (ADR-002) | Async boundary and no remote I/O in critical sections — no-op `Drop` with explicit async release, fencing tokens removed from public API, `cargo gears lint` enforcement (cluster-trait-scoped) |
| `cpt-cf-clst-adr-watch-event-lifecycle-contract` (ADR-003) | Watch event lifecycle contract for both watches — union-type `*WatchEvent { value-variant, Lagged, Reset, Closed }` instead of `Result`-based signaling, applied to cache and leader watches; lightweight key-only cache events as the contract twin of `Lagged`/`Reset` |
| `cpt-cf-clst-adr-observability-contract` (ADR-004) | Observability as a versioned naming contract — spans, metrics, log events are part of the SDK contract; cardinality rule forbids keys/names as metric labels |
| `cpt-cf-clst-adr-facade-backend-pattern` (ADR-005) | Per-primitive facade-plus-backend-trait pattern, per-primitive `*V1` versioning, no root `Cluster` trait |
| `cpt-cf-clst-adr-builder-handle-lifecycle` (ADR-006) | Outbox-style builder/handle lifecycle owned by parent host gear, no two-tier `RunnableCapability` ordering |
| `cpt-cf-clst-adr-capability-typing-and-profile-resolution` (ADR-007) | Per-primitive capability typing — `*Capability` enums replace bundled `CapabilityClass`; consequences: `ClusterProfile` typed marker, fluent resolver, capability-mismatch fails startup |
| `cpt-cf-clst-adr-leader-election-backend-safety` (ADR-009) | Per-backend correctness analysis for SDK-default leader election (and lock) under failure; constructor pair `new` (rejects `EventuallyConsistent`) + `new_allow_weak_consistency` (opt-in with warning); promotes the r2 deep-dive to decision-of-record |
| `cpt-cf-clst-adr-cache-scan-prefix-for-polyfill` (ADR-010) | Cache `scan_prefix` enumeration added to the frozen cache contract so the SDK `PollingPrefixWatch` polyfill can enumerate keys under a prefix without a native prefix-watch backend |
| `cpt-cf-clst-adr-remote-backend-seam` (ADR-011) | The process boundary is the three backend traits, with exactly one `dyn ClusterClient` per process as their factory (local winning over remote); the profile is a request parameter resolved server-side; facades bind lazily and capability validation reads the profile descriptor |
| `cpt-cf-clst-adr-store-owned-leases` (ADR-012) | Leases are fenced records in the backing store rather than session state, so no process's death ends another's lease and any replica serves any lease operation; the Postgres liveness beacon is removed and sub-TTL reclaim is traded for one lease mechanism across every profile |

#### NFR Allocation

| NFR Summary | Allocated To | Design Response | Verification Approach |
|-------------|--------------|-----------------|----------------------|
| At most one leader per election name (when bound to `Linearizable` cache) | All backends + SDK defaults | Trait contract enforces single-leader guarantee; capability validation rejects `EventuallyConsistent` cache without explicit opt-in | Multi-task contention smoke tests against `MemCacheBackend`; per-backend integration tests in plugin follow-ups |
| Bounded lock holding (no stale writers) | Consumers + architecture lint rule | Async + timeouts bound critical section; `cargo gears lint` forbids remote I/O inside `try_lock`/`release` scopes (lint scope is initially restricted to the three cluster backend traits; DB-tx enforcement is a follow-up rule extension) | Architecture lint rule check; smoke tests for lock release-on-timeout |
| No serde in contract types | SDK crate | `cargo gears lint` layer rules enforce no serde in trait definitions | `make check` (architecture lints) |
| Watch event delivery — at-most-once with per-key ordering and lifecycle signals | All backends | Union-type events (`*WatchEvent`) carry `Lagged{dropped}`, `Reset`, `Closed(err)` so consumers recover from missed events explicitly | Smoke tests across all three watches verifying each variant is observable |
| Backend trait dyn-compatibility | SDK crate | Compile-time assertions (`fn _assert_dyn_compat(_: Arc<dyn _Backend>) {}`) per trait | Build fails if dyn-compat is broken |

#### Functional Requirements Coverage

Each functional requirement from the PRD maps to the SDK surface and design section that satisfies it.

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-clst-fr-cache-storage` | `ClusterCacheV1` facade over `ClusterCacheBackend`; versioned key-value entries (§3.2, §3.3) |
| `cpt-cf-clst-fr-cache-atomic` | Version-based compare-and-set on `ClusterCacheBackend` (§2.1 CAS-as-universal, §3.3) |
| `cpt-cf-clst-fr-cache-ttl` | TTL-bounded entries with backend-side expiry (§3.3 `ClusterCacheV1`) |
| `cpt-cf-clst-fr-cache-watch` | Key- and prefix-scoped `CacheWatchEvent` stream (§3.9 Watch Event Shape) |
| `cpt-cf-clst-fr-leader-elect` | `LeaderElectionV1` with single-leader guarantee bound to `Linearizable` cache (§3.3, §3.10) |
| `cpt-cf-clst-fr-leader-config` | Configurable lease/renew timing on the leader resolver (§3.3, §3.7) |
| `cpt-cf-clst-fr-leader-observability` | Watch-based `LeaderWatchEvent` status model (§3.9) |
| `cpt-cf-clst-fr-leader-resign` | Graceful step-down on handle drop / shutdown sequence (§3.13 Shutdown Sequence) |
| `cpt-cf-clst-fr-leader-advisory` | Advisory semantics documented on the facade contract (§3.3, §4.1) |
| `cpt-cf-clst-fr-lock-acquire` | `DistributedLockV1` acquire-or-fail and acquire-with-wait (§3.3) |
| `cpt-cf-clst-fr-lock-release` | Explicit async release with TTL safety net; no-op `Drop` (§2.2 no-remote-in-critical-section, §3.3) |
| `cpt-cf-clst-fr-lock-no-remote` | Architecture lint rule forbidding remote I/O inside lock critical sections (§2.2, §3.10) |
| `cpt-cf-clst-fr-routing-cache-only-plugin` | SDK default backends derive all three primitives from `ClusterCacheBackend` (§2.1, §3.11) |
| `cpt-cf-clst-fr-validation-typed-profile` | `ClusterProfile` typed marker resolved via the fluent resolver (§3.6 Resolution Pattern, ADR-007) |
| `cpt-cf-clst-fr-validation-capability-declarations` | Per-primitive `*Capability` requirement enums on the resolver (§3.10 Capability Validation) |
| `cpt-cf-clst-fr-validation-honest-declaration` | Plugin-declared `*Features` characteristic structs (§3.10) |
| `cpt-cf-clst-fr-validation-startup-fail` | Capability mismatch fails resolution at startup, not production (§3.10) |
| `cpt-cf-clst-fr-watch-lifecycle-signals` | Union `*WatchEvent` carrying `Lagged`/`Reset`/`Closed` (§3.9, ADR-003) |
| `cpt-cf-clst-fr-watch-auto-restart` | SDK auto-restart combinator (§3.9 Watch Event Shape) / `PollingPrefixWatch` (§3.12 Polyfill) |
| `cpt-cf-clst-fr-namespacing-scoped` | Per-primitive `scoped()` sub-namespacing helpers (§3.8 Per-primitive Scoping) |
| `cpt-cf-clst-fr-routing-omit-default` | `ClusterHandle` wiring auto-fills unbound primitives with SDK defaults over the cache (§3.7 Lifecycle, §3.11) |
| `cpt-cf-clst-fr-lifecycle-owner` | Single owner: the cluster gear crate's `ClusterHandle` start/stop sequence (§3.7, §3.13) |
| `cpt-cf-clst-fr-shutdown-revoke` | `ClusterHandle::stop` revokes leadership (`Status(Lost)` then `Closed(Shutdown)`) before completing (§3.13 Shutdown Sequence) |
| `cpt-cf-clst-fr-shutdown-ttl-cleanup` | `ClusterHandle::stop` performs no remote cleanup; resources lapse via backend TTL (§3.13 Shutdown Sequence) |

#### Non-Functional Requirements Coverage

Each non-functional requirement from the PRD maps to its design response and verification approach (see §1.2 NFR Allocation for the cross-cutting allocation view).

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-clst-nfr-leader-guarantee` | Single-leader contract bound to `Linearizable` cache; weak-consistency requires explicit opt-in (§3.10, ADR-009) |
| `cpt-cf-clst-nfr-bounded-critical-section` | Async + timeouts plus architecture lint no-remote-I/O rule bound the critical section (§2.2, §3.10) |
| `cpt-cf-clst-nfr-watch-delivery` | At-most-once, per-key-ordered delivery with explicit `Lagged`/`Reset`/`Closed` recovery (§3.9, ADR-003) |
| `cpt-cf-clst-nfr-observability` | Versioned spans/metrics/log-event naming contract; cardinality rule (§3.10, ADR-004) |
| `cpt-cf-clst-nfr-capability-validation` | Capability requirements validated at resolution/startup (§3.10) |
| `cpt-cf-clst-nfr-cross-backend-stability` | Backend trait contract gives stable cross-backend behavior; per-backend smoke/integration tests (§3.2, §4.1) |
| `cpt-cf-clst-nfr-error-retryability` | Programmatic error classification exposes retryability on the facade error types (§3.3) |
| `cpt-cf-clst-nfr-plugin-stability` | Per-primitive `*V1` versioning isolates plugin contract changes (§2.1 facade-plus-backend-trait, ADR-005) |

### 1.3 Architecture Layers

```
┌─────────────────────────────────────────────────────────────────┐
│            Consumers (Event Broker, OAGW, gears)                │
│  Hold ClusterCacheV1 / LeaderElectionV1 / DistributedLockV1 /   │
│  facades. Define ClusterProfile markers.                        │
├─────────────────────────────────────────────────────────────────┤
│  Parent host gear (this change: out of scope; future)           │
│  Owns ClusterHandle from RunnableCapability::start/stop.        │
├─────────────────────────────────────────────────────────────────┤
│  cf-cluster-sdk (THIS CHANGE)                                   │
│  Facade structs, backend traits, resolver builders, profile     │
│  marker, *Capability and *Features enums/structs, SDK default   │
│  backends, scoping helpers, polyfill, shared types.             │
├─────────────────────────────────────────────────────────────────┤
│  cf-gears-cluster wiring (delivered)                            │
│  ClusterWiring::builder().build_and_start() -> ClusterHandle.   │
│  Reads operator YAML; instantiates plugins; registers each      │
│  Arc<dyn _Backend> per profile per primitive in ClientHub.      │
├─────────────────────────────────────────────────────────────────┤
│  Plugin crates (standalone + postgres shipped)                  │
│  ┌────────────────┐ ┌──────────────┐ ┌────────────────┐         │
│  │ standalone     │ │ postgres     │ │ k8s            │  ...    │
│  │ (in-process)   │ │ (table+L/N)  │ │ (Lease+CRD)    │         │
│  └────────────────┘ └──────────────┘ └────────────────┘         │
│  Each plugin: builder/handle pair (outbox pattern).             │
├─────────────────────────────────────────────────────────────────┤
│  External (out of all change scopes)                            │
│  PostgreSQL, K8s API, Redis, NATS, etcd                         │
└─────────────────────────────────────────────────────────────────┘
```

**The `grpc-client` feature layer, which the diagram above does not show.** The stack above is Profile 1: every
layer is linked into one process and the arrows are function calls. Profile 3 cuts it between the SDK and the
wiring, and the cut is a **Cargo feature on `cf-gears-cluster-sdk`**, not a new layer:

```
  Consumers                        (unchanged source, both profiles)
      |
  cf-gears-cluster-sdk             facades + backend traits + resolvers
      |                            ClusterClient: unfeatured, three sync factory
      |                            methods + async descriptor()
      +-- (feature off) ---------> LocalClusterClient  -> the real backend Arc
      |                            registered by the cluster gear's start
      |
      +-- (feature "grpc-client") -> RemoteClusterClient -> tonic stubs -> cluster pod
                                     Remote{Cache,Lock,Leader}Backend
```

Three properties of that seam matter more than the picture. The **boundary is the three backend traits**, so a
consumer names a facade and never a `Remote*Backend` (invariant I4) — the remote types are `pub` inside a private
module and reachable only as `Arc<dyn _Backend>`. **Exactly one `Arc<dyn ClusterClient>` is registered per
process**, local winning over remote at *registration* time, so the decision is made once by what the binary
linked rather than per call. And **Profile 1 links no cluster transport**: with the feature off, the SDK has no
direct `tonic` edge at all, which is why `ClusterClient` itself is unfeatured — a feature-gated trait would have
made the seam visible to consumers. The gear crate is the exception and enables `grpc-client` unconditionally,
because it *serves* the contract; see §3.15.1 for what that costs an embedding process.

| Layer | Responsibility | Technology |
|-------|---------------|------------|
| SDK | Public-API facade structs (`*V1`), backend traits (`*Backend`), per-primitive resolver builders, `ClusterProfile` marker trait, `*Capability` requirement enums, `*Features` characteristic structs, shared types, per-primitive `scoped()` helpers, `PollingPrefixWatch` polyfill, `register_*_backend` / `deregister_*_backend` helpers | Rust crate (`cf-cluster-sdk`) |
| Cluster gear | SDK default backend implementations (`CasBasedLeaderElectionBackend`, `CasBasedDistributedLockBackend`), `ShutdownRevoke` seam, wiring lifecycle (`ClusterHandle`) | Rust crate (`cf-gears-cluster`) |
| Wiring | Operator YAML parsing, per-primitive provider dispatch (`ProviderRegistry` → `ClusterWiring::from_config`), plugin orchestration, per-primitive ClientHub registration, builder/handle exposed as library API. Each of `leader_election` / `lock` resolves against its own provider registry independently of the `cache` binding, so one profile can mix backends (`cpt-cf-clst-fr-routing-per-primitive`); an omitted primitive falls back to the SDK default over that profile's cache (`cpt-cf-clst-fr-routing-omit-default`) | Rust crate (`cf-gears-cluster`) — collapsed into the gear crate, see the §3.7 amendment |
| Plugins | Backend-specific primitive implementations exposed as builder/handle pairs, plus the `Cluster*Provider` impls the wiring dispatches on. A plugin may ship a cache provider only, a native non-cache provider only, or both (the Postgres plugin ships a cache provider and a standalone lock provider) | Rust crates per backend (`standalone-cluster-plugin`, `postgres-cluster-plugin` today; K8s, Redis, NATS, etcd follow-up) |
| External | Persistence, coordination, cluster state | PostgreSQL, K8s API server, Redis, NATS, etcd |

### 1.4 Platform OoP Alignment

Cluster is a deployable gear within the platform's out-of-process (OoP) model, and two platform decisions govern how it serves consumers across a process boundary: which transport it uses, and where its process boundary is cut. This subsection settles the first; §3.16 settles the second.

**Cluster needs both planes, and they are not alternatives.** It runs a normal OoP gear's REST lifecycle plane and opts into gRPC for the coordination data plane:

| Plane | Transport | Contents | Why |
|---|---|---|---|
| **Lifecycle / operability** | REST via `oop_http` (Axum) | `/healthz`, `/readyz`, `/health`, `/openapi.json`, self-registration, drain, internal auth, admin endpoints (profiles, sessions) | ADR-0002's probe model, ADR-0006's internal auth and the Helm probe wiring all live on the HTTP server; it also gives operators `curl`-debuggable diagnostics |
| **Coordination data plane** | gRPC, co-hosted via `grpc-hub` | The three primitives | ADR-0002 sanctions gRPC "for performance-critical internal paths (explicitly opted in per gear)"; cluster qualifies |

The gRPC opt-in rests chiefly on **throughput and payload shape**: OAGW's 10k+ counter updates/second under a 5 ms p95 budget, against cache values that are `Vec<u8>` by contract. Protobuf carries opaque bytes natively where JSON needs base64 (+33% plus an encode/decode pass), and HTTP/2 multiplexing handles that concurrency on one connection more gracefully than HTTP/1.1 request-per-operation. Watch efficiency is a secondary, weaker argument (native server streaming over one connection vs. a request per batch). This argument applies to the **cache** primitive specifically; locks and leader election are ordinary unary request/response against a store-owned lease (§3.20.4), and nothing in cluster requires bidirectional streaming. A REST-only cluster API is structurally viable — the reason all four services go over gRPC is tooling, not structure: the contract codegen **rejects platform-plane REST projections at compile time** (the generated client cannot source the internal token), so a REST split would forfeit its codegen alignment and require a hand-written client anyway. All four coordination services therefore project over gRPC; a REST projection stays additive if the platform lifts that restriction.

**What cluster reuses, and what it adds.** Cluster owns as little transport, config and error machinery as possible. Client transport selection (embedded vs. remote), endpoint discovery, connect/RPC timeouts, per-RPC retry policy, `.proto` generation and field-number stability, the cross-process error model, and client-side spans/metrics are all delegated to platform mechanisms (§3.16–§3.20). What cluster genuinely adds — because no platform mechanism covers it — is the `ProfileRegistry` and its runtime profile enumeration (§3.18.1), the `BackendInstanceCache` keyed by connection identity (§3.18.2), a `probe()` on the cache backend for per-instance liveness (§3.17.3), a server-side session index for watch subscriptions and lease diagnostics (§3.18.3), profile self-registration via `inventory` (§3.17.7), and cluster-owned migration orchestration across N plugin-owned DSNs (§3.17.8).

Three places where cluster does not fit the standard mould, each carried as a consequence below rather than papered over: its watches and handle sessions are inherently **streaming** (SSE for REST projections, native for gRPC — cluster's watches are the platform's reference case); it is a **dependency of nearly every gear**, so it inverts the usual drain order (§3.17.6) and makes its own readiness a fleet-wide gate (§3.17.3); and its coordination is **non-tenant-scoped platform work**, so it sits on the platform plane (§3.17.5).

## 2. Principles & Constraints

### 2.1 Design Principles

#### Cache CAS as Universal Building Block

- [x] `p1` - **ID**: `cpt-cf-clst-principle-cas-universal`

`ClusterCacheBackend` with version-based CAS is the foundational primitive. Leader election and distributed locks can both be built on top of cache CAS + watch. The SDK ships default backend implementations of both using only cache operations. This means a minimal plugin needs to implement only `ClusterCacheBackend` to get all three primitives (the wiring crate auto-wraps the cache backend in the SDK defaults when a primitive is omitted in operator config). Native overrides improve performance but are never required.

#### Per-primitive Routing as Operator Config

- [x] `p1` - **ID**: `cpt-cf-clst-principle-per-primitive-routing`

Each primitive routes independently to the best backend for the job. The wiring crate's `ClusterWiring::builder(...).build_and_start()` reads each profile's per-primitive config and registers the corresponding `Arc<dyn _Backend>` in ClientHub under the profile scope. Mixed backends within one profile (Redis cache + K8s Lease for leader election) are the common case, supported directly by the per-primitive registration model. There is no runtime compositor object — registration is per-primitive and the wiring crate is a thin iterator over operator config.

#### Facade-plus-Backend-Trait Pattern

- [x] `p1` - **ID**: `cpt-cf-clst-principle-facade-plus-backend-trait`

There is no root `Cluster` trait. Each primitive is split into a public-API facade struct (`ClusterCacheV1`) and a plugin-facing backend trait (`ClusterCacheBackend`). Consumers hold the facade — a cheap-clone Arc-backed struct with inherent async methods. Plugins implement the backend trait. This keeps consumers off the `dyn` surface, lets the public API evolve independently of the plugin contract, and gives consumers a clean fluent resolver entry point: `ClusterCacheV1::resolver(hub).profile(P).require(...).resolve()`. Per-primitive versioning (`*V1`, `*V2`) allows incompatible primitive changes to coexist via separate `TypeKey`/ClientHub registration.

#### Lightweight Notifications, Not Messaging

- [x] `p1` - **ID**: `cpt-cf-clst-principle-lightweight-notifications`

Cache watch events carry only the key and event type (`Changed`, `Deleted`, `Expired`) — no value payload. Consumers call `cache.get(key)` for the current value. This avoids stale-value issues, maps cleanly to all backends (Redis keyspace notifications carry no value, Postgres NOTIFY has 8KB limit), and keeps events fixed-size. Reliable messaging belongs in the event broker.

#### Version-Based Optimistic Concurrency

- [x] `p1` - **ID**: `cpt-cf-clst-principle-version-based-cas`

`compare_and_swap` takes an `expected_version: u64` obtained from a prior `get()`, not an expected byte value. `get()` returns `CacheEntry { value, version }`. This maps natively to all backends: `resourceVersion` (K8s), `revision` (NATS), `mod_revision` (etcd), `BIGSERIAL` (Postgres), Lua counter (Redis), `AtomicU64` (in-process). Value-based CAS would require racy get-compare-put loops on revision-based backends.

#### Watch Union Shape Across All Three Watches

- [x] `p1` - **ID**: `cpt-cf-clst-principle-watch-union-shape`

Both watch event types (`CacheWatchEvent`, `LeaderWatchEvent`) follow the same union shape: `{value-variant, Lagged{dropped}, Reset, Closed(err)}`. Infallible at the type level — there is no `Result`-returning `changed()` method on any watch. Terminal errors arrive via `Closed(err)`. Transient backend errors (`ConnectionLost`, `Timeout`, `ResourceExhausted`) are retried internally by the watch's background task and do not surface as events. ADR-003 captures the rationale and applies to both watches.

### 2.2 Constraints

#### No Serde in Contract Types

- [x] `p1` - **ID**: `cpt-cf-clst-constraint-no-serde`

The **coordination contract types** — the facade methods, the three backend traits, the watch-event unions and `ClusterError` — MUST stay serde-free. Serialization concerns belong to plugin implementations and to the wire DTOs (§3.20.2), which are separate types. This is a constraint on which types derive `Serialize`/`Deserialize`, not on the crate's dependency graph: `cluster-sdk` already depends on `serde`, `serde_json` and `schemars` unconditionally for the GTS plugin-discovery scaffolding, and the `grpc-client` feature adds the DTO layer. Nothing enforces the constraint mechanically today — there is no such lint in `deny.toml`, no test and no CI check — so it is a design discipline the two-trait split (`*Backend` local and serde-free; `*Api` carrying the wire contract, §3.20.2) makes structural.

#### Leases Are Store-Owned, Not Session-Owned

- [x] `p1` - **ID**: `cpt-cf-clst-constraint-store-owned-leases`

A lease — a held lock or a leader claim — is a **fenced record in the backing store**, not session state in the process that issued it (ADR-012). Every lease-keyed operation (`renew`, `release`, `resign`) is a conditional write predicated on the token the client presents, so any replica serves any of them and no process's death — the holder's or the broker's — ends another's lease. Renewal is client-driven, which keeps it doubling as the consumer-liveness proxy (§3.19, invariants I7/I8). This constraint is why cluster can be restarted, upgraded and horizontally scaled without revoking the fleet's coordination state.

#### No Remote I/O in Cluster Critical Sections

- [x] `p1` - **ID**: `cpt-cf-clst-constraint-no-remote-in-critical-section`

Code protected by a `LockGuard` MUST NOT make **non-cluster** remote calls; those remote effects MUST occur before `try_lock` or after `release`, never between them. **Bounded cluster-primitive round trips are permitted** — in Profile 3 a consumer's own cache/CAS against cluster is itself a remote call (§6) — bounded by `rpc_timeout` and failing closed. Together with async + timeouts, this eliminates the Kleppmann fencing scenario at the architectural level. The enforcing workspace architecture lint is re-scoped to distinguish cluster coordination (permitted) from third-party remote I/O (forbidden); today it is still scoped to the three cluster backend traits within `try_lock`/`release`, so that re-scoping — and DB-tx enforcement — are follow-ups once the wiring crate and consumer migrations land. See ADR-002 (amended).

#### Backend Trait Dyn-Compatibility

- [x] `p1` - **ID**: `cpt-cf-clst-constraint-dyn-compat`

All three backend traits MUST be dyn-compatible. The SDK includes compile-time assertions per trait so any future change that breaks dyn-compatibility fails the build. No `Self: Sized` bounds on async trait methods; no GATs.

## 3. Technical Architecture

### 3.1 Domain Model

| Entity | Description |
|--------|-------------|
| `ClusterCacheV1` | Public-API facade struct; cheap-clone (Arc-backed) wrapper over `Arc<dyn ClusterCacheBackend>`. Inherent async methods: `get`, `put`, `delete`, `contains`, `put_if_absent`, `compare_and_swap`, `watch`, `watch_prefix`. Inherent sync: `consistency()`, `features()`, `resolver(hub)`, `scoped(prefix)`. |
| `LeaderElectionV1` | Public-API facade struct over `Arc<dyn LeaderElectionBackend>`. Inherent async: `elect`, `elect_with_config`. Inherent sync: `resolver(hub)`, `scoped(prefix)`. |
| `DistributedLockV1` | Public-API facade struct over `Arc<dyn DistributedLockBackend>`. Inherent async: `try_lock`, `lock`. Inherent sync: `resolver(hub)`, `scoped(prefix)`. |
| `ClusterCacheBackend` | Plugin-facing async trait. Methods: `consistency()`, `features()`, `get`, `put`, `delete`, `contains`, `put_if_absent`, `compare_and_swap`, `compare_and_delete`, `watch`, `watch_prefix`. `compare_and_delete` is backend-only — not surfaced on `ClusterCacheV1`. |
| `LeaderElectionBackend` | Plugin-facing async trait. Methods: `features() -> LeaderElectionFeatures`, `elect`, `elect_with_config`. |
| `DistributedLockBackend` | Plugin-facing async trait. Methods: `features() -> LockFeatures`, `try_lock`, `lock`. |
| `ClusterProfile` | Marker trait: `pub trait ClusterProfile: 'static + Send + Sync + Copy { const NAME: &'static str; }`. Consumer crates impl this on a ZST struct once per profile; the `NAME` is the only place the profile string lives on the consumer side. |
| `CacheCapability` | `#[non_exhaustive] enum { Linearizable, PrefixWatch }`. Per-primitive requirement enum used at resolver call sites. |
| `LeaderElectionCapability` | `#[non_exhaustive] enum { Linearizable }`. |
| `LockCapability` | `#[non_exhaustive] enum { Linearizable }`. |
| `CacheFeatures` | `#[non_exhaustive] struct { prefix_watch: bool, ... }`. Backend declares native capability availability. |
| `LeaderElectionFeatures` | `#[non_exhaustive] struct { linearizable: bool, ... }`. |
| `LockFeatures` | `#[non_exhaustive] struct { linearizable: bool, ... }`. |
| `*ResolverBuilder<'a>` | Per-primitive fluent builder: `.profile<P: ClusterProfile>(_: P)`, `.require(cap: *Capability)`, `.resolve() -> Result<*V1, ClusterError>`. |
| `CacheConsistency` | `enum { Linearizable, EventuallyConsistent }`. Cache-only — leader election and lock backends use `*Features { linearizable: bool }` instead. |
| `CacheEntry` | Versioned key-value pair: `{ value: Vec<u8>, version: u64 }`. Version is opaque, monotonically increasing per key, starting at 1. Version 0 is reserved as sentinel. **The monotonicity holds only while the key exists**: a `delete`, or a TTL reap, removes the counter with the key, and the next write of that key starts again at 1 (measured: `standalone-cluster-plugin/src/cache.rs`). It is therefore safe as a CAS predicate — which compares versions of a key it just read — and **unsafe as a durable fence**, because "version 1" does not identify one incarnation of a key. Anything needing a value that outlives its key must carry its own counter in the value: that is exactly what the store-owned lease record does with `fence`, and why `fence_retention` keeps the record alive past the lease (§3.19.1). |
| `CacheEvent` | Lightweight notification: `Changed { key }`, `Deleted { key }`, `Expired { key }`. No payload — consumer calls `get(key)` for current value. |
| `CacheWatchEvent` | Watch union: `Event(CacheEvent)`, `Lagged { dropped: u64 }`, `Reset`, `Closed(ClusterError)`. Per ADR-003. |
| `CacheWatch` | Async receiver yielding `CacheWatchEvent` items. Dropping unsubscribes. Per-key ordering guaranteed; no cross-key ordering. |
| `LeaderStatus` | `enum { Leader, Follower, Lost }`. `Lost` is a transient observable transition — the watch auto-reenrolls and the next `Status` event resolves to `Leader` or `Follower`. Not terminal. |
| `LeaderWatchEvent` | Watch union: `Status(LeaderStatus)`, `Lagged { dropped: u64 }`, `Reset`, `Closed(ClusterError)`. |
| `LeaderWatch` | Handle into an ongoing election. `async fn changed() -> LeaderWatchEvent`; `fn status() -> LeaderStatus`; `fn is_leader() -> bool`; `async fn resign(self) -> Result<()>`. `Drop` is a no-op (no I/O in `Drop`). |
| `ElectionConfig` | `{ ttl: Duration (default 30s), max_missed_renewals: u8 (default 2) }`. Constructor `new(ttl, max_missed_renewals)` validates both > 0. Derived: `renewal_interval() = ttl / (max_missed_renewals + 1)`. |
| `LockGuard` | Lock handle. `async fn renew(new_ttl)`, `async fn release(self)`. `Drop` is a no-op (TTL is the safety net; no I/O in `Drop`). |
| `RetryPolicy` | Combinator config: `initial_backoff: Duration`, `max_backoff: Duration`, `jitter_factor: f32` (0.0–1.0), `max_retries: Option<u32>` (None = retry forever). Constructor `default()` returns exponential backoff `1s → 30s`, full jitter (`jitter_factor: 1.0`), no retry cap. |
| `RestartingWatch<W>` | SDK combinator wrapping a base `*Watch`. Implemented for `W: CacheWatch | LeaderWatch`. Consumes `Closed(retryable)` internally per the bound `RetryPolicy`, synthesizes `Reset` to the consumer on each successful resubscribe, propagates `Closed(non-retryable)` and `Closed(Shutdown)` to the consumer unchanged. Constructed via `*Watch::auto_restart(policy)`. Retryability is read from `ProviderErrorKind`: `ConnectionLost`, `Timeout`, `ResourceExhausted` are retryable; `AuthFailure`, `Other` are not. `ClusterError::Shutdown`, `CapabilityNotMet`, and the lock/leader-specific terminal variants are also not retryable. |
| `ClusterError` | Unified error enum. Variants: `InvalidName { name, reason }`, `InvalidConfig { reason }`, `LockContended { name }`, `LockTimeout { name, waited }`, `LockExpired { name }`, `CasConflict { key, current: Option<CacheEntry> }`, `Unsupported { feature: &'static str }`, `ProfileNotSpecified`, `ProfileNotBound { profile: &'static str }`, `CapabilityNotMet { primitive: &'static str, capability: &'static str, provider: &'static str }`, `Shutdown`, `Provider { kind: ProviderErrorKind, message: String }`. `ClusterError` derives `Clone` so it can ride the watch-union `Closed(_)` signal to multiple watchers; the provider error chain is therefore flattened into `message` rather than carried as a non-`Clone` boxed `source`. **No `NotStarted` variant** — pre-resolution access surfaces as `ProfileNotBound` (the resolver enforces presence at consumer construction time, so resolved facades cannot observe a "not started" state). |
| `ProviderErrorKind` | `enum { ConnectionLost, Timeout, AuthFailure, ResourceExhausted, Other }`. Programmatic retryability classification. |
| `ScopedCacheBackend` (and three siblings) | Internal SDK wrapper struct implementing the corresponding `*Backend` trait by delegating to an inner `Arc<dyn _Backend>` with prefix translation. Returned by `*V1::scoped(prefix)`. |
| `PollingPrefixWatch` | SDK polyfill: synthesizes `watch_prefix` behavior on backends declaring `features().prefix_watch == false` by periodically listing the prefix and emitting `CacheWatchEvent::Event` diffs (Changed/Deleted). Explicit opt-in; doc comments describe the cost (N gets per interval). |
| `ClusterWiring` (follow-up) | Wiring crate's builder entry point. `ClusterWiring::builder(config, hub).build_and_start() -> ClusterHandle`. |
| `ClusterHandle` (follow-up) | Wiring crate's lifecycle handle. `handle.stop() -> ()` deregisters all backends and stops nested plugin handles. Owned by the parent host gear. |

**Relationships**:
- A `CacheEntry` belongs to exactly one key. Each `put` increments the version.
- A `LeaderWatch` belongs to one election name. At most one `LeaderWatch` across all nodes observes `Leader` (advisory — see staleness bound in §3.3).
- A `LockGuard` belongs to one lock name. Mutual exclusion is bounded by TTL; explicit `release().await` is the idiomatic release path. Consumers MUST NOT make **non-cluster** remote I/O calls inside the critical section; bounded cluster-primitive round trips are permitted (§2 Constraints, §6).
- A `ClusterCacheV1` is `Arc<dyn ClusterCacheBackend>`-backed; cloning the facade is a single atomic increment.

### 3.2 Component Model

```
┌────────────────────────────────────────────────────────────────────┐
│                          cf-cluster-sdk                            │
│  ┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐    │
│  │ ClusterCacheV1   │ │LeaderElectionV1  │ │ DistributedLockV1│    │
│  │ + CacheBackend   │ │ + LEBackend      │ │ + LockBackend    │    │
│  └──────────────────┘ └──────────────────┘ └──────────────────┘    │
│  ┌──────────────────┐ ┌─────────────────────────────────────────┐  │
│  │                  │ │ Resolver builders (one per primitive)   │  │
│  │ + SDBackend      │ │ ClusterProfile marker, *Capability,     │  │
│  └──────────────────┘ │ *Features, ClusterError, shared types   │  │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │ Per-primitive Scoped*Backend wrappers                       │   │
│  │ PollingPrefixWatch polyfill                                 │   │
│  │ register_*_backend / deregister_*_backend helpers           │   │
│  └─────────────────────────────────────────────────────────────┘   │
└────────────────────────────────────────────────────────────────────┘
                                   ▲
                                   │ Arc<dyn _Backend> registered per primitive per profile
                                   │
┌────────────────────────────────────────────────────────────────────┐
│                       cf-cluster (follow-up change)                │
│  ClusterWiring::builder(config, hub).build_and_start() →           │
│       ClusterHandle (owns nested plugin handles)                   │
│  Reads operator YAML; iterates profile×primitive matrix;           │
│  starts each plugin's builder; registers each backend in ClientHub │
└────────────────────────────────────────────────────────────────────┘
                                   ▲
                                   │ owned by parent host gear's RunnableCapability::start
                                   │
┌────────────────────────────────────────────────────────────────────┐
│             Plugin crates (each follow-up change)                  │
│  cf-standalone-cluster-plugin / cf-postgres-cluster-plugin /       │
│  cf-k8s-cluster-plugin / cf-cluster-redis / cf-cluster-nats / ...  │
│  Each: builder/handle pair (outbox pattern)                        │
└────────────────────────────────────────────────────────────────────┘
```

**Two components the diagram predates.** `cf-gears-cluster` is no longer only a wiring library owned by a parent
host gear: it is the cluster **gear** (`name = "cluster"`, capabilities `stateful, system, grpc, rest`), it serves
the four coordination services over gRPC from `src/api/grpc/`, and it ships a `cluster-oop` binary. The
builder/handle library described below still exists and is still embeddable — that half is unchanged — but the
same crate now also owns the profile registry, the composite readiness check, the local client and the deployable
entry point. And beside the SDK sits its `grpc-client` half (§1.3): `RemoteClusterClient` plus the three
`Remote*Backend` handles, compiled only when the feature is on, which is what lets a Profile 3 consumer link the
SDK and no plugins at all.

#### cf-cluster-sdk (this change)

- [x] `p1` - **ID**: `cpt-cf-clst-component-sdk`

Per-primitive public-API facade structs, plugin-facing backend traits, resolver builders, profile marker, capability and features types, shared types, scoping wrappers, polyfill, registration/deregistration helpers, name validation utilities. Zero external dependencies beyond `tokio`, `tokio_util`, `async-trait`, and platform crates (`toolkit`, `gts`, `types-registry-sdk`). Default backend implementations (`CasBasedLeaderElectionBackend`, `CasBasedDistributedLockBackend`) live in the cluster gear crate, not here.

#### cf-cluster wiring (follow-up change)

- [ ] `p1` - **ID**: `cpt-cf-clst-component-wiring`

Wiring library. Implements no `RunnableCapability` itself. Exposes `ClusterWiring::builder(config, hub).build_and_start() -> ClusterHandle`. The handle's `stop()` is the single shutdown entry point. A parent host gear owns the handle from inside its own `RunnableCapability::start`/`stop`.

#### Plugin crates (follow-up changes)

- [ ] `p1` - **ID**: `cpt-cf-clst-component-plugins`

Each plugin (Postgres, K8s, Redis, NATS, etcd, standalone) exposes a builder/handle pair (`MyCachePlugin::builder(...).build_and_start() -> MyCacheHandle`), with the handle's `stop()` cancelling internal `CancellationToken`s and joining background tasks (TTL reapers, renewal loops, watch fan-out). The wiring crate composes these into the cluster handle.

### 3.3 API Contracts

#### ClusterCacheV1 — Cache primitive

| Method | Signature | Contract |
|--------|-----------|----------|
| `resolver` | `fn resolver(hub: &ClientHub) -> CacheResolverBuilder<'_>` | Static entry point. Returns a fluent builder. |
| `consistency` | `fn consistency(&self) -> CacheConsistency` | Surfaces backend's declared consistency class. |
| `features` | `fn features(&self) -> CacheFeatures` | Surfaces backend's native capability flags. |
| `scoped` | `fn scoped(&self, prefix: &str) -> ClusterCacheV1` | Returns a scoped wrapper that prepends `prefix + "/"` on the write path and strips it on the read path. Validates prefix per the cluster name rule. |
| `get` | `async fn get(&self, key: &str) -> Result<Option<CacheEntry>, ClusterError>` | Returns versioned entry or `None`. Never errors for missing keys. |
| `put` | `async fn put(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), ClusterError>` | Stores value, increments version. Emits `Changed`. Overwrites if exists. |
| `delete` | `async fn delete(&self, key: &str) -> Result<bool, ClusterError>` | Removes entry. Emits `Deleted` if existed. Return MAY be `true` unconditionally if backend cannot determine prior existence. |
| `contains` | `async fn contains(&self, key: &str) -> Result<bool, ClusterError>` | Existence check. MAY be `get(key).is_some()`. |
| `put_if_absent` | `async fn put_if_absent(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<Option<CacheEntry>, ClusterError>` | Atomic. `Some(entry)` if created, `None` if key existed. Emits `Changed` on creation only. |
| `compare_and_swap` | `async fn compare_and_swap(&self, key: &str, expected_version: u64, new_value: &[u8], ttl: Option<Duration>) -> Result<CacheEntry, ClusterError>` | Atomic version-based CAS. Emits `Changed` on success. `CasConflict { key, current }` on mismatch — `current` SHOULD contain the entry if cheaply obtainable. |
| `watch` | `async fn watch(&self, key: &str) -> Result<CacheWatch, ClusterError>` | Yields `CacheWatchEvent` for exact key. Drop unsubscribes. |
| `watch_prefix` | `async fn watch_prefix(&self, prefix: &str) -> Result<CacheWatch, ClusterError>` | Yields `CacheWatchEvent` for matching keys. Backends declaring `features().prefix_watch == false` return `Err(Unsupported { feature: "prefix_watch" })`. Callers may polyfill via `PollingPrefixWatch`. |
| `CacheWatch::auto_restart` | `fn auto_restart(self, policy: RetryPolicy) -> RestartingWatch<CacheWatch>` | Wraps the watch with the SDK auto-restart combinator. See §3.9 for retryability classification and `RetryPolicy` defaults. `LeaderWatch::auto_restart` follows the same shape. |

> **Backend-trait-only — `compare_and_delete`.** `ClusterCacheBackend` additionally declares `async fn compare_and_delete(&self, key: &str, expected_value: &[u8]) -> Result<bool, ClusterError>`: an atomic value-guarded delete that removes `key` only if its current value equals `expected_value`. A value mismatch or an absent key returns `Ok(false)`, never an error. It is deliberately **not** exposed on `ClusterCacheV1` — the public CAS contract is version-based (`compare_and_swap`), while this is the value/owner-token-guarded counterpart used internally by SDK-default coordination backends (e.g. the leader elector's guarded release, which must survive a key's version resetting to 1 on delete+recreate, where a version guard would alias a successor's fresh claim). The trait's default impl is a best-effort, non-atomic `get`-then-`delete`; backends with an atomic store override it for a genuine compare-and-delete.

#### LeaderElectionV1 — Leader election primitive

| Method | Signature | Contract |
|--------|-----------|----------|
| `resolver` | `fn resolver(hub: &ClientHub) -> LeaderElectionResolverBuilder<'_>` | Static entry point. |
| `scoped` | `fn scoped(&self, prefix: &str) -> LeaderElectionV1` | Scopes election names. |
| `elect` | `async fn elect(&self, name: &str) -> Result<LeaderWatch, ClusterError>` | Join election. Auto-renews. `LeaderWatch` auto-reenrolls on `Status(Lost)`. |
| `elect_with_config` | `async fn elect_with_config(&self, name: &str, config: ElectionConfig) -> Result<LeaderWatch, ClusterError>` | Same with custom timing. |
| `LeaderWatch::changed` | `async fn changed(&mut self) -> LeaderWatchEvent` | Next watch event (`Status` / `Lagged` / `Reset` / `Closed`). Infallible at type level per ADR-003. Transient backend errors retried internally. Terminal errors arrive via `Closed(err)`. |
| `LeaderWatch::status` | `fn status(&self) -> LeaderStatus` | Cached snapshot from background task. Synchronous, no I/O. **Advisory** — see staleness bound. |
| `LeaderWatch::is_leader` | `fn is_leader(&self) -> bool` | `matches!(status(), Leader)`. Advisory — do NOT use for correctness-critical mutual exclusion. |
| `LeaderWatch::resign` | `async fn resign(self) -> Result<(), ClusterError>` | Explicit step-down. Triggers immediate re-election. |

**Staleness bound**: `is_leader() == true` at time T does NOT guarantee this node holds leadership at time T on the backend. The background task's state lags by up to one renewal interval plus a provider round-trip in steady state, and up to a full TTL under partition.

**Worst-case window with default config** (`ttl=30s`, `max_missed_renewals=2`, derived `renewal_interval=10s`): under network partition, renewal attempts fail at T+10s, T+20s, and T+30s; the third consecutive failure triggers `LeaderWatchEvent::Status(Lost)` emission. The backend revokes the lease at T+30s, after which a successor's `put_if_absent` may succeed. The consumer-perceived dual-leadership window is `TTL + observation_lag`, where `observation_lag` is the time between renewal-failure emission and the consumer's code reaching a watch-polling await point. A consumer with a 1s iteration cycle observes the transition ~30s after partition begins; one with a 60s synchronous compute block ~90s. Operators tune `ttl` and `max_missed_renewals` against this trade-off: shorter TTL shortens the window at the cost of more renewal traffic and lower tolerance for transient network jitter. Pattern C below (lock + CAS) eliminates the dual-write effect at the resource level regardless of window size.

**Profile 3 widens the bound by one transport hop, and does not change its shape.** With cluster deployed as its
own pod, `status()` is still a synchronous read of a cached snapshot — the remote handle keeps a local cache fed
by the event stream, exactly as the in-process watch keeps one fed by its channel — so the *cost* of the call is
unchanged. What changes is how the snapshot got there: leadership transitions are derived client-side from
`renew` results and a re-`join` on the renewal cadence (the server announces no leadership; see
§3.20.4), and each of those is now an RPC. So `observation_lag` gains one round trip in
steady state, and under partition the client's renew fails against an unreachable *cluster pod* rather than an
unreachable *backend* — which produces the same `Status(Lost)` after the same `max_missed_renewals`, because
renewal remains client-driven precisely so that it stays the liveness proxy (invariant I8). The worst-case window
is therefore `TTL + observation_lag + one_rpc`, and the three consumer patterns below apply unchanged: a
consumer that needs mutual exclusion still gets it from `try_lock` or a CAS failing, not from a timing argument.

Three consumer patterns are available, ordered by tolerance for transient dual-leadership:

- **Tolerant work — `is_leader()` gate, short jobs.** For workloads where brief dual-execution is acceptable or recoverable (idempotent rebalancing, periodic cleanup, log compaction, leader-coordinated metrics emission): gate each iteration on the cached `is_leader()` snapshot and bound the iteration's duration to a small fraction of the TTL. Optional: app-level guard (e.g., a row lock in the consumer's own database) on the actual write.
- **Reactive work — `changed()` + cancellation token.** For workloads where dual-execution should end as soon as leadership transitions: subscribe to `LeaderWatch::changed().await`, hold a `CancellationToken` per leader-only task, fire the token on `Status(Lost)`, and structure the task to observe cancellation at every await point. This pattern reduces the dual-leader window relative to the tolerant pattern (reactive vs. cached) but does not eliminate it: the window between backend lease revocation and the consumer's cancel-observation is bounded by `renewal_lag + consumer_poll_lag + cancellation_propagation`, never zero.
- **Mutually exclusive work — `DistributedLockV1` + cache CAS.** For workloads where two simultaneous writers would corrupt state: combine the reactive pattern with either (a) `DistributedLockV1::try_lock` around the write, or (b) `ClusterCacheV1::compare_and_swap` with `expected_version` drawn from a prior `get` on the protected key. A `LockContended`/`LockExpired` from (a) or a `CasConflict` from (b) is the authoritative "you are no longer the writer" signal — closes the residual window from the reactive pattern by failing the actual write rather than relying on cancellation timing.

#### DistributedLockV1 — Distributed lock primitive

| Method | Signature | Contract |
|--------|-----------|----------|
| `resolver` | `fn resolver(hub: &ClientHub) -> LockResolverBuilder<'_>` | Static entry point. |
| `scoped` | `fn scoped(&self, prefix: &str) -> DistributedLockV1` | Scopes lock names. |
| `try_lock` | `async fn try_lock(&self, name: &str, ttl: Duration) -> Result<LockGuard, ClusterError>` | Non-blocking. `LockContended { name }` if held. |
| `lock` | `async fn lock(&self, name: &str, ttl: Duration, timeout: Duration) -> Result<LockGuard, ClusterError>` | Blocking up to `timeout`. `LockTimeout { name, waited }` if not acquired. |
| `LockGuard::renew` | `async fn renew(&self, new_ttl: Duration) -> Result<(), ClusterError>` | Renews the lease (resets the TTL to `new_ttl` from now; does not add to the time left). `LockExpired { name }` if TTL elapsed. |
| `LockGuard::release` | `async fn release(self) -> Result<(), ClusterError>` | Explicit release. Consumers MUST call this. `Drop` is a no-op (no I/O in `Drop`). |

**Critical-section rule** (see §2 Constraints, ADR-002 as amended): Consumers MUST NOT make **non-cluster** remote I/O calls inside the critical section between `try_lock` / `lock` and `release`; bounded cluster-primitive round trips are permitted (§6). No fencing tokens — the no-remote-in-critical-section rule eliminates the stale-writer scenario fencing tokens protect against, and pattern C covers correctness-critical work, where a remote critical section widens the window in which a lease can lapse unnoticed.

### 3.4 Internal Dependencies

| Dependency | Direction | Purpose |
|-----------|-----------|---------|
| `toolkit` | SDK → toolkit | GTS registration, ClientHub wiring |
| `gts` / `gts-macros` | Wiring → gts | Plugin schema definitions (used by follow-up wiring crate) |
| `tokio` | SDK | Async runtime (watch channels, broadcast, TTL timers in stub backends) |
| `tokio_util` | SDK | `CancellationToken` for `PollingPrefixWatch` and (follow-up) plugin lifecycles |
| `async-trait` | SDK | `#[async_trait]` on the three backend traits |
| `types-registry-sdk` | Wiring → registry | GTS plugin-spec registration (used by follow-up wiring crate) |

### 3.5 External Dependencies

The cluster SDK has **no external dependencies** of its own. External backend libraries (`sqlx`, `kube`, `redis`, `async-nats`, `etcd-client`, `hazelcast`) belong to the follow-up plugin crates (`cf-postgres-cluster-plugin`, `cf-k8s-cluster-plugin`, `cf-cluster-redis`, `cf-cluster-nats`, `cf-cluster-etcd`, `cf-cluster-hazelcast`) and are NOT SDK dependencies.

| Plugin | External library | Purpose |
|---|---|---|
| Postgres plugin (shipped) | `sqlx` | Connection pool, prepared statements, LISTEN/NOTIFY |
| K8s plugin (shipped) | `kube` | API client, watch streams, `coordination.k8s.io/v1.Lease` types, `ClusterCacheEntry` CRD derive |
| Redis plugin | `fred` (or `redis`) | Connection management, Lua script execution, keyspace notifications |
| NATS plugin | `async-nats` | JetStream KV access, watch subscriptions |
| etcd plugin | `etcd-client` | KV access, native lease/lock/election APIs |
| Hazelcast plugin | `hazelcast-rust` (TBD) | CP Subsystem access |

### 3.6 Resolution Pattern

There is no root trait. Each primitive has its own public-API facade struct with a static `resolver(hub)` entry point returning a fluent builder.

**Consumer-side definition (one place per consumer crate)**:

```rust
#[derive(Clone, Copy)]
pub struct EventBrokerProfile;
impl ClusterProfile for EventBrokerProfile {
    const NAME: &'static str = "event-broker";
}
```

**Call site**:

```rust
let cache = ClusterCacheV1::resolver(&hub)
    .profile(EventBrokerProfile)
    .require(CacheCapability::Linearizable)
    .require(CacheCapability::PrefixWatch)
    .resolve()?;

let leader = LeaderElectionV1::resolver(&hub)
    .profile(EventBrokerProfile)
    .require(LeaderElectionCapability::Linearizable)
    .resolve()?;
```

**Resolver builder body** (cache; the other three are identical in shape). `resolve()` is `async` — the one SDK signature the deployable model changed (invariant I2) — and it resolves through the process's single `dyn ClusterClient` (§3.16) rather than reading a scoped backend straight out of the hub. It validates against the profile's **descriptor**, not against the backend object, so the same code path serves both the in-process and remote bindings:

```rust
impl<'a> CacheResolverBuilder<'a> {
    pub(crate) fn new(hub: &'a ClientHub) -> Self {
        Self { hub, profile_name: None, requirements: Vec::new() }
    }
    pub fn profile<P: ClusterProfile>(mut self, _: P) -> Self {
        self.profile_name = Some(P::NAME);
        self
    }
    pub fn require(mut self, cap: CacheCapability) -> Self {
        self.requirements.push(cap);
        self
    }
    pub async fn resolve(self) -> Result<ClusterCacheV1, ClusterError> {
        let profile = self.profile_name
            .ok_or(ClusterError::ProfileNotSpecified)?;
        validate_cluster_name(profile)?;
        let requirements = self.requirements;
        // Bind through the process's single `dyn ClusterClient`: locally the
        // real backend, remotely a handle; validation reads the profile's
        // descriptor, bounded by the SDK resolve timeout (§3.10.1, §3.16).
        let backend = binding::bind(
            self.hub,
            profile,
            "cache",
            |client| client.cache_backend(profile),
            || binding::unbound_cache(profile),
            move |descriptor| validate_cache_capabilities_from(&descriptor.cache, &requirements),
        )
        .await?;
        Ok(ClusterCacheV1::from_backend(backend))
    }
}
```

**Resolution flow**:
1. Consumer crate defines a `ClusterProfile` marker once. The `NAME` const is the only place the profile string appears on the consumer side.
2. Gear calls `*V1::resolver(hub).profile(P).require(Cap...).resolve().await` (in `start`, never `init`; §3.10.1).
3. The process holds exactly one `Arc<dyn ClusterClient>` — a `LocalClusterClient` over the gear's `ProfileRegistry` in Profile 1, a `RemoteClusterClient` over the gRPC channel in Profile 3 (§3.16). The resolver asks it for this profile's backend (sync and pure in both profiles) and awaits the profile's `ProfileDescriptor`, bounded by the SDK resolve timeout.
4. It validates the declared `*Capability` requirements against that **descriptor** (its `consistency`/`features` fields), and returns the wrapped facade. Mismatch → `CapabilityNotMet { primitive, capability, provider }` — where `provider` is the operator-facing provider name the descriptor declares. When the descriptor was not obtainable within the timeout (a cold start against an unreachable cluster), validation is deferred to the readiness contributor and `resolve()` returns `Ok` (§3.10.1). An unbound profile surfaces as `ProfileNotBound`; a process with no client wired at all returns `Ok` and binds lazily on first use (§3.16, §3.17.7).

Multiple resolutions of the same primitive on the same profile are cheap (`Arc`-clone-equivalent) and idempotent.

`profile_scope(name)` is an SDK helper that maps a profile name to a `ClientScope`. Convention: scope name `cluster:{profile}`. Validation: profile name MUST conform to `[a-zA-Z0-9_-]+`; reject invalid names at registration time.

### 3.7 Lifecycle Pattern (Builder/Handle)

> **Amendment (2026-06-16): collapsed to one gear crate.** As designed (this is follow-up work, not delivered in the SDK-only change that freezes this contract), the wiring library and the host gear are **the same crate** (`cf-gears-cluster`, gear name `cluster`), matching the platform's universal one-gear-per-domain layout (`<gear>-sdk` + `<gear>` + plugins). The crate will both (a) register the `cluster` gear — a `RunnableCapability` whose `start` builds the wiring from operator config and whose `stop` owns teardown — and (b) exports the builder/handle (`ClusterWiring`, `ClusterHandle`, `ClusterWiring::from_config`, `ProviderRegistry`) as `pub` library API, so a consumer gear may still embed the wiring directly without depending on the `cluster` gear. The separate non-gear wiring crate + separate host gear described below was rejected because it introduced a third core crate no other gear has; the genuinely reusable surface is `cluster-sdk` (already its own crate). The substance below holds — a `RunnableCapability` owns the handle, plugins remain builder/handle libraries composed by `ClusterHandle::stop()`, backends register under `cluster:{profile}` — only the crate boundary changed. The `ClusterCacheProvider` trait (a plugin implements it to build its cache backend from config options) lives in `cluster-sdk`, so plugins depend on the SDK only.

The `cluster` gear (`cf-gears-cluster`) is the single `RunnableCapability` that owns the cluster handle across its lifecycle; the same crate also exposes the wiring as a builder/handle pair (the outbox-style library API) for a consumer gear that prefers to embed it directly. Either way one `RunnableCapability` owns the `ClusterHandle` inside its own `start`/`stop`:

```rust
// In the cluster gear's RunnableCapability impl (or a consumer gear embedding the wiring):
impl RunnableCapability for ClusterGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let cluster_handle = ClusterWiring::builder(&self.config.cluster, &self.hub)
            .build_and_start()
            .await?;
        self.cluster_handle.set(cluster_handle).ok();
        Ok(())
    }

    async fn stop(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        if let Some(handle) = self.cluster_handle.take() {
            tokio::select! {
                () = handle.stop() => {} // graceful: deregister, cancel tokens, join
                () = cancel.cancelled() => {} // framework deadline
            }
        }
        Ok(())
    }
}
```

`ClusterHandle::stop().await` is the single entry point that:
1. Deregisters every registered backend from ClientHub via `deregister_*_backend` helpers (subsequent `*V1::resolver(...).resolve()` calls return `ProfileNotBound`).
2. Calls each plugin's internal stop sequence — cancels the plugin's `CancellationToken`, joins its background tasks (renewal loops, watch fan-out, TTL reapers).
3. Delivers `LeaderWatchEvent::Status(Lost)` then `LeaderWatchEvent::Closed(Shutdown)` to active leaders (two distinct events — `Status(Lost)` revokes confidence before the consumer can observe shutdown; `Closed(Shutdown)` ends the watch), and `CacheWatchEvent::Closed(Shutdown)` to active cache watches before returning.

**Why this shape**:
- Outbox is the codebase's production-mature long-running background-task pattern (`cluster/libs/toolkit-db/src/outbox/manager.rs:455–596`). Mini-chat owns its outbox via `Outbox::builder(...).start()` from inside its own `RunnableCapability::start`.
- Ordering is by code flow inside the parent gear's `start`, NOT framework declarations. The parent gear is registered as a `RunnableCapability` dependency of consumer gears (via existing ToolKit gear-dependency mechanism), so consumers can't try to resolve before cluster is up.
- Plugins are NOT separate `RunnableCapability` implementors. They expose builder/handle types like outbox does. The cluster wiring's builder calls each plugin's builder; the cluster handle owns each plugin's handle and stops them in reverse-start order.

**Post-shutdown behavior (narrowed best-effort `Ok`)**:
- `LockGuard::release(self)` / `LeaderWatch::resign(self)` MAY return `Ok(())` on a best-effort basis ONLY after the backend has observed `RunnableCapability::stop` (e.g., via an internal `AtomicBool::shutdown_observed`). Outside the shutdown window, real errors (`LockExpired`, foreign-holder release attempts, connection-lost mid-release) MUST propagate normally — silently masking them under the "best-effort" rule would hide real consumer bugs.

### 3.8 Per-primitive Scoping

Each public-API facade exposes `pub fn scoped(&self, prefix: &str) -> Self` returning a wrapped instance auto-prepending `prefix + "/"` on the write path and stripping it on the read path. Scoping composes: `cache.scoped("event-broker").scoped("shard-0")` produces effective prefix `"event-broker/shard-0/"`.

**Per-primitive scoping rules**:

| Primitive | Scoped argument(s) | Read-path strip | NOT scoped |
|---|---|---|---|
| `ClusterCacheV1` | `key` on `get`/`put`/`delete`/`contains`/`put_if_absent`/`compare_and_swap`/`watch`; `prefix` on `watch_prefix` | `CacheEvent::{Changed,Deleted,Expired}{key}` — strip prefix on the way back to the consumer | (none — cache has only keys) |
| `LeaderElectionV1` | `name` on `elect`/`elect_with_config` | n/a — `LeaderWatch` events don't carry names; the consumer already holds the watch handle | (none — election has only a name) |
| `DistributedLockV1` | `name` on `try_lock`/`lock` | n/a — `LockGuard` is opaque, consumer doesn't see backend names | (none — lock has only a name) |

**Examples**:

```rust
// Cache: keys
let cache = ClusterCacheV1::resolver(...).resolve()?.scoped("event-broker");
cache.put("shard-assignments", ...);          // backend sees "event-broker/shard-assignments"
cache.watch_prefix("");                        // backend sees "event-broker/"

// Leader election: election names
let leader = LeaderElectionV1::resolver(...).resolve()?.scoped("event-broker");
let watch = leader.elect("shard-leader").await?;  // backend sees "event-broker/shard-leader"

```

**Wrapper implementation**: each public-API struct's `scoped()` returns a new instance whose `inner: Arc<dyn _Backend>` is a `Scoped*Backend` wrapper that prepends/strips the prefix. The wrapper is internal to the SDK — consumers see only `ClusterCacheV1`, etc.

**Scope validation**: the `prefix` argument MUST conform to `[a-zA-Z0-9_/-]+`. Invalid prefixes fail at scope construction with `ClusterError::InvalidName { name, reason }`.

**Scoping across the process boundary is client-side and cooperative.** `Scoped*Backend` wraps *any* backend, including a remote one, so full prefixed keys travel over the wire and the cluster gear needs no scope concept at all — composition (`cache.scoped("event-broker").scoped("shard-0")`) and read-path prefix stripping on `CacheEvent` keys both keep working, including through the polyfill, which emits full backend keys precisely so the scoped wrapper can strip them. Because scoping is a wrapper the consumer opts into rather than a boundary the server enforces, it is **not an isolation boundary over a network**: a caller can simply not scope and read another gear's keys. Server-enforced namespacing derived from the authenticated caller identity is the hardening that would turn this cooperative convention into a platform guarantee, and it is tracked as an open question (§7, decision 9) alongside cluster's credential-proxy posture (§6).

### 3.9 Watch Event Shape

All three watches yield events via union enums of the same shape (per ADR-003).

```rust
enum CacheWatchEvent {
    Event(CacheEvent),                // a cache mutation; consumer calls cache.get(key) for value
    Lagged { dropped: u64 },          // watcher fell behind; treat watched keys as stale, re-read
    Reset,                            // subscription re-established (reconnect, compaction); re-read
    Closed(ClusterError),             // terminal — watch is no longer usable
}

enum LeaderWatchEvent {
    Status(LeaderStatus),             // leadership transition; Lost is transient (auto-reenroll)
    Lagged { dropped: u64 },
    Reset,
    Closed(ClusterError),
}

```

Both are `#[non_exhaustive]` and infallible at the type level — there is no `Result<_, _>`-returning `changed()` method on any watch. **Terminal errors arrive via `Closed(err)`. Transient backend errors (`ConnectionLost`, `Timeout`, `ResourceExhausted`) are retried internally by the watch's background task and do not surface as events.**

**Consumer obligations**:
- On `Lagged { dropped }` or `Reset`: treat current state as potentially stale and recover. Cache: re-read affected keys via `get`. Leader watch: wait for the next `Status` event before resuming leader-only work.
- After `Closed(err)`: the watch is no longer usable; no further events follow. Consumer MAY restart at the application level (call `elect()` / `watch()` again) once cluster is up.

**Shutdown sequence** for `LeaderWatch`: the wiring crate's `ClusterHandle::stop()` delivers `LeaderWatchEvent::Status(Lost)` synchronously to every active `LeaderWatch` currently in `Leader` state, immediately followed by `LeaderWatchEvent::Closed(ClusterError::Shutdown)` as the terminal event. Two distinct events at the type level — `Status(Lost)` revokes the leader's confidence before the consumer can observe shutdown; `Closed(Shutdown)` ends the watch.

**Auto-restart combinator** (`*Watch::auto_restart(policy: RetryPolicy)`): the SDK provides an opt-in wrapper that turns retryable terminal closes into transparent reconnection with backoff. Retryability classification:

| `Closed(err)` payload | Classification | Combinator action |
|---|---|---|
| `Provider { kind: ConnectionLost, .. }` | retryable | reconnect after backoff; emit `Reset` on success |
| `Provider { kind: Timeout, .. }` | retryable | same |
| `Provider { kind: ResourceExhausted, .. }` | retryable | same; backoff respects backend's signal where available |
| `Provider { kind: AuthFailure, .. }` | non-retryable | propagate `Closed(err)` to consumer |
| `Provider { kind: Other, .. }` | non-retryable | propagate |
| `Shutdown` | non-retryable | propagate; consumer ends loop |
| `CapabilityNotMet { .. }` | non-retryable | propagate (capability validation rejects re-resolution anyway) |
| `LockExpired`, `LockContended`, `LockTimeout` | non-retryable on `LeaderWatch`/`CacheWatch` | propagate (these are state-loss signals on the renewal-task path; see §"Watch task and renewal task: independent signal paths" in ADR-003) |

`RetryPolicy::default()` uses exponential backoff `1s → 30s` with full jitter (`jitter_factor: 1.0`) and no retry cap. Operators can override via `RetryPolicy { initial_backoff, max_backoff, jitter_factor, max_retries }` constructor. When `max_retries` is exhausted, the combinator propagates the most recent `Closed(err)` to the consumer unchanged.

ADR-003 captures the rationale for the union shape over `Result`/`?`-based signaling, applies to all three watches for consistency, and is the source of the auto-restart combinator's semantics.

### 3.10 Capability Validation

Each primitive declares its own `*Capability` enum carrying the requirements a consumer can demand at resolution time. Each variant maps to a concrete characteristic check against the profile's **descriptor** — the same input in both deployment profiles (§3.18.4), computed from the real backends in-process and fetched via `DescribeProfiles` remotely, so the diagnostic is byte-identical across profiles:

| Capability | Descriptor field | Check |
|---|---|---|
| `CacheCapability::Linearizable` | `descriptor.consistency` | `CacheConsistency::from(...) == Linearizable` |
| `CacheCapability::PrefixWatch` | `descriptor.features.prefix_watch` | `== true` |
| `LeaderElectionCapability::Linearizable` | `descriptor.features.linearizable` | `== true` |
| `LockCapability::Linearizable` | `descriptor.features.linearizable` | `== true` |

**Validation helpers** (one per primitive). They take the primitive's `*Descriptor` (not the backend object) and match the requirement set exhaustively — no catch-all — so a future `*Capability` variant fails to compile here rather than being silently treated as satisfied:

```rust
pub fn validate_cache_capabilities_from(
    descriptor: &CacheDescriptor,
    reqs: &[CacheCapability],
) -> Result<(), ClusterError> {
    for cap in reqs {
        match cap {
            CacheCapability::Linearizable => {
                if CacheConsistency::from(descriptor.consistency) != CacheConsistency::Linearizable {
                    return Err(ClusterError::CapabilityNotMet {
                        primitive: "ClusterCacheV1",
                        capability: "Linearizable",
                        provider: intern(&descriptor.provider),
                    });
                }
            }
            CacheCapability::PrefixWatch => {
                if !descriptor.features.prefix_watch {
                    return Err(ClusterError::CapabilityNotMet {
                        primitive: "ClusterCacheV1",
                        capability: "PrefixWatch",
                        provider: intern(&descriptor.provider),
                    });
                }
            }
        }
    }
    Ok(())
}
```

Same shape for `validate_leader_election_capabilities_from` and `validate_lock_capabilities_from`. The `provider` field is `intern(&descriptor.provider)` — the operator-facing provider name the profile's descriptor declares (e.g. `postgres`), interned to `&'static str` so `ClusterError` stays frozen (invariant I3) and provider names remain a bounded, config-derived set. The descriptor is the validation input in both profiles: computed from real backends in-process, fetched remotely via `DescribeProfiles` — so `CapabilityNotMet` names the provider the operator wrote rather than the Rust type behind it (§3.18.4).

**Why per-primitive (not bundled `CapabilityClass`)**: the prior bundled `CapabilityClass { Standalone, Durable, InMemory, Coordination }` collapsed three orthogonal axes (topology, persistence, consistency) into one fuzzy ordering. Per-primitive `*Capability` enums are type-safe (a cache resolver cannot accept `MetadataFiltering`) and grounded in concrete backend characteristic checks rather than coarse tier claims.

#### 3.10.1 `resolve()` is `async`, and where validation lands

`resolve()` on all three resolvers is `async fn`. **It is the only SDK signature the remote-backend model changes** — the facades, the typed-profile resolver, `scoped()`, the watch-event unions and `auto_restart` all keep their shapes.

The reason is validation, not the resolution itself: checking a declared capability needs the bound backend's `consistency()`/`features()`, and for a *remote* binding those come from a `ProfileDescriptor` that has to be fetched. A synchronous signature cannot await one. In-process there is nothing to await — the bound object **is** the real backend, so its characteristics are known immediately and validation is inline, exactly as the tables above describe.

Two rules for consumers follow, and both are cheap:

- **Resolve facades in `start`, never in `init`.** Both are already `async fn` on the gear traits, so the `.await` costs a consumer nothing structurally. `init` is the wrong phase regardless: backends are registered by the cluster gear's own `start`.
- **A consumer that branches on `CapabilityNotMet` is relying on the inline path.** That is always the in-process path. Against a remote cluster the same check can instead land on readiness — see the specification of the bounded descriptor await and the inline-vs-deferred split later in this subsection and in §3.17.7. The *guarantee* is identical either way (no consumer serves traffic against an unmet requirement) and so is the error text; only the delivery point moves.

**What `resolve()` actually does**, now that the seam is in place (ADR-011):

1. Takes the process's one `Arc<dyn ClusterClient>` from the `ClientHub`.
2. Asks it for this profile's backend. Synchronous and pure in both deployment profiles — the real backend locally, a remote handle remotely. A client that does not bind the profile is `Err(ProfileNotBound)` here, immediately.
3. **Awaits the profile's `ProfileDescriptor`, bounded** by an SDK constant (2 s). This is the only `await` on the path, and it waits on the descriptor — never on cluster becoming reachable.
4. Validates the declared requirements against that descriptor, or defers to readiness when it did not arrive in time.

Two consequences worth stating rather than leaving to be found:

- **Validation reads the descriptor, not the backend.** The tables above describe the check; its *input* is now what the profile's binding declares, which is what a remote consumer can obtain at all. In-process the descriptor is computed from the real backends, so the answer is identical — and the error text is byte-identical across deployment profiles, which is the property the equivalence gate asserts. One thing does change under operator config: `CapabilityNotMet { provider }` names the provider **the operator wrote** (`postgres`) rather than the Rust type behind it.
- **A process with no cluster client wired at all is not a resolve failure.** `resolve()` returns `Ok` and the facade reports `ProfileNotBound` on its first *call*, naming the profile; the distinguishing phrase (*no cluster client registered in this process*) is logged at `warn`, because `ClusterError` is frozen and cannot carry a second message. That tolerance is what lets a Profile 3 cold start proceed, and the readiness contributor is what stops it hiding a Profile 1 build mistake.

**Two obstacles, two answers.** The design reconciles cluster's `cpt-cf-clst-fr-validation-startup-fail` (a capability mismatch must fail loudly) with ADR-0005's eventual readiness (gears start immediately and never block on a dependency) by separating them:

| | Obstacle | Answer |
|---|---|---|
| **A** | Validating a declared capability needs a `ProfileDescriptor`, which is I/O when remote, and a sync `resolve()` cannot await one | `resolve()` is `async`; it awaits the descriptor, then runs the identical `validate_*_capabilities` call |
| **B** | Cluster may be unreachable at startup, and ADR-0005 forbids both blocking and failing startup on an unresolved dep | The await is **bounded** (an SDK constant, hundreds of ms to low seconds). `resolve()` waits on the descriptor, never on cluster becoming reachable; on timeout it defers validation to readiness |

The situations `resolve()` faces classify cleanly into transient (retry, never fail startup) and permanent (loud, one way or another):

| Situation | Classification | Behaviour |
|---|---|---|
| Cluster pod not up / DNS not resolving / connection refused | **Transient** | Framework background dep resolution retries; consumer `/readyz` reports `Starting`. No startup failure |
| Cluster reachable; requested profile not bound (`ProfileNotBound`) | **Permanent config error** | `Err` from `resolve().await` naming the profile |
| No `dyn ClusterClient` in the hub at all (cluster unlinked, forwarding feature off, config missing) | **Permanent build/config error**, indistinguishable at `resolve()` from a Profile 3 cold start | `resolve()` returns `Ok` and the facade binds lazily; enforcement is the readiness contributor (`Unhealthy` once the grace window lapses) and `ProfileNotBound` from any call that arrives first |
| Cluster reachable; profile bound but capability unmet (`CapabilityNotMet`) | **Permanent config error** | `Err` naming primitive, unmet capability, and the **server-side provider** |
| Cluster unreachable at startup; a requirement later proves unmet | **Permanent, discovered late** | `resolve()` returned `Ok`; the readiness contributor reports `Unhealthy` with the identical diagnostic once the descriptor lands |

The guarantee is preserved in every row — no consumer serves traffic against a primitive that fails its declared requirements (invariant I5). What varies is whether the failure arrives as a return value or a readiness verdict, and the deferred verdict re-runs the *same* validator closure the inline path would have, so the error text is identical by construction.

**A permanent error crash-loops rather than sitting not-ready.** After a bounded grace period — five minutes — a gear whose readiness is `Unhealthy` for a *permanent* reason (`ProfileNotBound`, `CapabilityNotMet`, nothing-wired-past-grace) exits the process, so the failure surfaces as `CrashLoopBackOff` with the diagnostic in the container's last log lines. A transient unresolved dependency never escalates, no matter how long it lasts. A never-ready pod is a quiet failure a rollout can sit behind indefinitely; a crash loop is loud and has a standard alert attached. This is recorded as a considered exception against ADR-0005, which discourages failing startup on a *dependency* but says nothing about a configuration that cannot become valid.

### 3.11 SDK Default Backends

> **Implementation location:** The three default backend implementations live in the **cluster gear** (`cf-gears-cluster`), not in the SDK. Consumer gears never import them directly; only the cluster gear's wiring layer instantiates them. The SDK retains only the backend *traits* and facades that consumers depend on.

The cluster gear ships three default backend implementations built on `Arc<dyn ClusterCacheBackend>`:

- `CasBasedLeaderElectionBackend` — `put_if_absent(election_key, node_id, ttl)` for candidacy, `watch(election_key)` for status changes, background renewal task at `ttl / (max_missed_renewals + 1)`, TTL expiry → `Status(Lost)` followed by auto-reenroll. `features()` returns `LeaderElectionFeatures { linearizable: cache.consistency() == Linearizable }` — derives from the underlying cache's consistency.
- `CasBasedDistributedLockBackend` — `put_if_absent(lock_key, holder_id, ttl)` for `try_lock`, `watch(lock_key)` to notify blocked waiters on release, background TTL reaper. Release via delete-if-still-holder using CAS (a foreign holder cannot release another's lock). No fencing tokens (the no-remote-in-critical-section rule eliminates the stale-writer scenario). `features()` returns `LockFeatures { linearizable: cache.consistency() == Linearizable }`.

**Constructor pair per default backend**:
- `new(cache: Arc<dyn ClusterCacheBackend>) -> Result<Self, ClusterError>` — returns `Err(ClusterError::InvalidConfig)` if `cache.consistency() == EventuallyConsistent`. Default-safe.
- `new_allow_weak_consistency(cache: Arc<dyn ClusterCacheBackend>) -> Self` — always succeeds. Caller acknowledges the safety implications. Construction emits a warning log at instantiation. Required by spec for use cases where the underlying cache is intentionally `EventuallyConsistent` (Redis Sentinel, NATS R=1, Postgres `synchronous_commit=off`) and the consumer accepts the split-brain risk.

**SDK-default selection at the wiring layer (omit-primitive auto-wrap)**: operator YAML uses **omission** to opt into SDK defaults. If a profile binds a `cache` provider but does not bind `leader_election` / `lock`, the wiring crate auto-wraps the bound cache backend in the corresponding SDK default backend and registers each under the same profile scope. Explicit binding always wins. If both `cache` and another primitive are omitted (no anchor to wrap), the wiring crate fails startup with `ClusterError::InvalidConfig`.

```yaml
cluster:
  profiles:
    # Single-backend profile via omission
    default:
      cache: { provider: postgres }
      # leader_election omitted → CasBasedLeaderElectionBackend over postgres cache
      # lock              omitted → CasBasedDistributedLockBackend  over postgres cache

    # Mixed: native LE + auto-wrapped lock
    in-memory:
      cache: { provider: redis }
      leader_election: { provider: k8s }
      # lock omitted → CasBasedDistributedLockBackend over redis cache
```

### 3.12 Polyfill

`PollingPrefixWatch` synthesizes `watch_prefix` semantics on backends that declare `features().prefix_watch == false`:

```rust
PollingPrefixWatch::spawn(
    cache: Arc<dyn ClusterCacheBackend>,
    prefix: &str,
    interval: Duration,
) -> CacheWatch
```

Periodically lists keys under the prefix, diffs against the previous list, and emits `CacheWatchEvent::Event(CacheEvent::Changed | Deleted)` for observed changes. Cost: N `get` calls per interval, no millisecond-level precision. Doc comments explicitly warn about the cost and recommend routing to a backend with native prefix watch at scale. Drop on the watch stops the polling task.

Enumeration is provided by `ClusterCacheBackend::scan_prefix(prefix) -> Vec<String>`, a defaulted (returns `Unsupported`) additive extension to the cache contract so existing backends keep compiling and opt in by override (see ADR-010). The polyfill lists keys via `scan_prefix`, then issues one `get` per key to read its version for change detection (the `N + 1` round-trips above); a `scan_prefix` error closes the synthesized watch with a terminal `Closed`. Because the polyfill emits full backend keys like a native `watch_prefix`, `ScopedCacheBackend` strips the scope prefix from them on the read path, so scoping composes with the polyfill.

### 3.13 Interactions & Sequences

#### Per-primitive Resolution

- [x] `p1` - **ID**: `cpt-cf-clst-seq-per-primitive-resolution`

```
  Consumer Gear                    SDK                         ClientHub
       │                              │                              │
       │  ClusterCacheV1::resolver(&hub)                              │
       │   .profile(EventBrokerProfile)                              │
       │   .require(CacheCapability::Linearizable)                   │
       │   .resolve()                 │                              │
       │ ────────────────────────────>│                              │
       │                              │  hub.get_scoped::<dyn        │
       │                              │     ClusterCacheBackend>(    │
       │                              │     profile_scope("event-broker"))│
       │                              │ ────────────────────────────>│
       │                              │  Arc<dyn ClusterCacheBackend>│
       │                              │ <────────────────────────────│
       │                              │  validate_cache_capabilities_from │
       │                              │     (consistency() check)    │
       │                              │  wrap in ClusterCacheV1      │
       │  ClusterCacheV1              │                              │
       │ <────────────────────────────│                              │
```

#### Lifecycle: Parent host gear → Cluster wiring → Plugins

- [ ] `p1` - **ID**: `cpt-cf-clst-seq-lifecycle-startup`

```
  Gear Host         Parent Gear               Cluster Wiring          Plugins
       │                   │                          │                      │
       │ start(cancel)     │                          │                      │
       │ ─────────────────>│                          │                      │
       │                   │ ClusterWiring::builder() │                      │
       │                   │  .build_and_start()      │                      │
       │                   │ ────────────────────────>│                      │
       │                   │                          │ read profile config  │
       │                   │                          │ (cache: redis,       │
       │                   │                          │  leader: k8s)        │
       │                   │                          │                      │
       │                   │                          │ Plugin::builder()    │
       │                   │                          │  .build_and_start()  │
       │                   │                          │ ────────────────────>│
       │                   │                          │                      │  spawn
       │                   │                          │                      │  CancellationToken
       │                   │                          │                      │  + JoinHandles
       │                   │                          │                      │
       │                   │                          │ register_*_backend   │
       │                   │                          │  (per profile per    │
       │                   │                          │   primitive in       │
       │                   │                          │   ClientHub)         │
       │                   │                          │                      │
       │                   │ ClusterHandle            │                      │
       │                   │ <────────────────────────│                      │
       │                   │ store handle             │                      │
       │ Ok                │                          │                      │
       │ <─────────────────│                          │                      │

  Consumer gears now resolve via *V1::resolver(...).profile(P).resolve()
```

#### Shutdown Sequence

- [ ] `p1` - **ID**: `cpt-cf-clst-seq-shutdown`

```
  Gear Host       Parent Gear        Cluster Handle         Active Watches
       │                 │                    │                        │
       │ stop(cancel)    │                    │                        │
       │ ───────────────>│                    │                        │
       │                 │ handle.stop()      │                        │
       │                 │ ──────────────────>│                        │
       │                 │                    │ revoke: deliver        │
       │                 │                    │  Status(Lost) to leaders│
       │                 │                    │ ──────────────────────>│ Status(Lost)
       │                 │                    │ revoke: Closed(Shutdown)│
       │                 │                    │  to leader/lock/SD      │
       │                 │                    │ ──────────────────────>│ Closed(Shutdown)
       │                 │                    │                        │
       │                 │                    │ deregister all backends│
       │                 │                    │  from ClientHub         │
       │                 │                    │                        │
       │                 │                    │ stop hooks: plugin      │
       │                 │                    │  cache.shutdown() →     │
       │                 │                    │ ──────────────────────>│ Closed(Shutdown)
       │                 │                    │  cancel sweeper, drop   │
       │                 │                    │                        │
       │                 │ Ok                 │                        │
       │                 │ <──────────────────│                        │
       │ Ok              │                    │                        │
       │ <───────────────│                    │                        │
```

**Implementation status (this change).** The lifecycle owner is the cluster gear crate itself (host collapsed in); `ClusterHandle::stop()` lives there, not in a separate wiring crate. The implementation now matches the sequence diagram above. It revokes in-flight coordination **first** for every wiring-created default backend: the leader-election backend latches `Status(Lost)` then `Closed(ClusterError::Shutdown)` to active leaders (awaiting those tasks); an in-flight blocking `lock()` waiter returns `Err(ClusterError::Shutdown)` (distinct from `LockTimeout`); and it then deregisters backends from the `ClientHub` and runs the plugin stop hooks in reverse-start order. Active **cache** watches now receive an explicit `Closed(ClusterError::Shutdown)` too — delivered via the standalone plugin's stop hook (`StandaloneCache::shutdown`), which closes every watcher before the sweeper stops and the cache is dropped. That cache-watch close lands one phase after the leader/lock revocation but still within `stop()` (the chosen simplest path). No remote release is performed; held claims and locks lapse via TTL (`cpt-cf-clst-fr-shutdown-ttl-cleanup`).

### 3.14 Database schemas & tables

N/A — the cluster SDK has no persistent database schemas. Cluster is an in-process library that delegates all storage to plugin-owned backends (Redis, Postgres, K8s API, NATS, etcd), each of which manages its own schema or storage layout independently. The SDK's only durable types are the wire-stable contract surfaces (facade methods, backend traits, error variants) documented in §3.3 and §3.1; those are Rust types, not database tables.

Per-backend storage layout (e.g., the Postgres plugin's `cluster_cache` and `cluster_cache_subscriber_lease` tables, the K8s plugin's `Lease` layout and its `ClusterCacheEntry` CRD) is documented in each plugin's own DESIGN, not here.

### 3.15 Deployment Topology

**Cluster has a deployment topology of its own, and it is mapped to the platform's deployment profiles.** This
section previously said the opposite — "an in-process Rust library SDK; it has no deployment topology of its
own" — which was true while the only shape was a library linked into a consumer's process. It is no longer:
`cf-gears-cluster` ships a `[[bin]] cluster-oop` and can be deployed as its own pod, with consumers reaching it
over gRPC. The consumer API is unchanged in both shapes (§1.4, invariant I1), so what varies is
the topology, not the code.

| Platform profile | Topology | What owns the backends | How a consumer gets a primitive |
|---|---|---|---|
| **Profile 1 — Embedded** | One process. `cluster` and its plugins are linked into the consumer's binary; the gear's `start` owns the `ClusterHandle`, or a consumer owns `ClusterWiring` directly (§3.7) | The consumer's own process | `resolve()` returns the real backend `Arc` through a `LocalClusterClient` — no wrapper on the request path, no network |
| **Profile 2 — Host + Workers** | **Not designed.** Out of scope for the first deployable version, and stated as a scope limit rather than a deferral: no endpoint-resolution mechanism exists for it, and its topology fork (one cluster process per *deployment* vs. per *host*) is unanswered — the second silently makes locks per-host rather than deployment-wide | — | — |
| **Profile 3 — K8s Native** | The `cluster-oop` binary in its own pod, serving the four coordination services on the gRPC port and the framework probes on the HTTP port. One replica by default, pending the cross-replica failover suite; store-owned leases (ADR-012) already make any replica able to serve any lease operation | The cluster pod | The framework's proxy-wiring phase registers a `RemoteClusterClient`; `resolve()` derives per-primitive remote handles from it and the profile rides on each request |

Within a profile, the shape that still matters operationally is the **profile × backend** matrix. §4.2
Recommended Deployment Combinations enumerates the supported shapes (single-instance dev/test, multi-instance
non-K8s, K8s-low-throughput, K8s + Redis production, Redis-only); each is realized by the deployment of whatever
process owns the wiring — a `cluster-oop` pod in Profile 3, the consumer's own pod, systemd unit or container in
Profile 1 — plus the backend bindings declared in operator YAML. The wiring instantiates each primitive's bound
provider independently and auto-fills only the primitives the operator omits with the SDK defaults over that
profile's cache, so the mixed-backend shapes in the matrix below are expressible in YAML today
(`cpt-cf-clst-fr-routing-per-primitive`) for whichever native providers the linked plugins ship.

**What `cluster-oop` does not contain** is worth stating, because the absence is the design: no directory
registration, no heartbeat, no backoff, no dependency retry and no drain logic. `/healthz`, `/readyz`, `/health`
and `/openapi.json` are bound and served **before** the gear's `start` runs, self-registration and the presence
loop run in the background, and the drain sequence and deregistration run on SIGTERM — all of it supplied by
`toolkit::bootstrap::oop::run_oop_with_options`, which the binary's `main` calls and otherwise does nothing
(ADR-0005). The binary is a `clap` CLI over that one call plus a `registered_gears.rs` naming the two gears the
process must link.

Cross-cluster / geo-distributed coordination is out of scope (§4.2 Out of Scope in PRD).

#### 3.15.1 Linking `cluster` requires linking `grpc-hub`

**Any process that links the `cluster` gear must also link `grpc-hub` and give it a `listen_addr`, or it fails at startup.** This is not confined to the deployable (out-of-process) shape — it applies to every in-process monolith too, and it is a hard failure rather than a degradation: the framework refuses to build a registry that has gRPC services and no hub, with `RegistryError::GrpcRequiresHub` (`libs/toolkit/src/runtime/host_runtime.rs:777-779`).

The cause is that the gear declares the `grpc` capability and exports the four coordination services (`cluster.{cache,lock,leader,profile}.v1`), which it does so that one profile-dispatch mechanism serves both an embedded and a remote consumer. Two consequences an operator has to plan for:

- **Once the hub is linked, cluster's four services are served on that process's hub port.** That is a network surface an embedded cluster never had, so an embedding process needs the same `NetworkPolicy` treatment as a dedicated cluster pod: the coordination port is platform-plane and must not be reachable from outside the platform namespaces.
- **The hub must bind a port the operator is willing to expose.** There is no "link the hub but serve nothing" mode today.

Gating the capability behind a `serve-grpc` feature *is* expressible — but only as two mutually exclusive `#[cfg_attr(..., toolkit::gear(...))]` attributes, since `#[toolkit::gear]` accepts a `#[cfg]` **inside** its `capabilities = [..]` list and then silently ignores it (measured against `toolkit-macros`; the capability is registered either way). It is not adopted, because the gear links `tonic` unconditionally regardless, so the feature would remove the hub requirement without removing the dependency — see §3.17.1.

### 3.16 Remote Backend Seam

This is the central architectural decision that lets one consumer source file work whether cluster is linked in-process or reached over a network (invariant I1). The SDK already had a clean two-layer split (ADR-005): a consumer-facing **facade** (`ClusterCacheV1`) over a plugin-facing **backend trait** (`Arc<dyn ClusterCacheBackend>`), with everything consumer-visible — resolver, capability validation, `scoped()`, polyfill, `RestartingWatch` — implemented above the backend trait in terms of it.

**The process boundary is cut exactly at the three backend traits** (ADR-011). The remote client is not a new kind of facade; it is four ordinary backend implementations that satisfy their trait by making a remote call. **One object is registered in the hub, under one trait, and it is a factory for those backends** — the platform's ubiquitous consumption shape (`hub.get::<dyn SomeClient>()`, one trait object per dependency, local impl winning when co-located), applied to cluster rather than reinvented:

```rust
// cluster-sdk — the TRAIT is unfeatured, because Profile 1 needs it too.
// Only the remote impl sits behind `grpc-client` (§3.16.1).

/// The one cluster object per process, registered under `dyn ClusterClient`.
/// A factory for the three backend traits — it answers "give me the backend for
/// this profile" and nothing else.
///
/// Profile 1: `LocalClusterClient`, registered by the cluster gear, dispatching
/// through the `ProfileRegistry` (§3.18.1).
/// Profile 3: `RemoteClusterClient`, registered by the SDK's consumer
/// registration (§3.17.7), holding the gRPC channel and the descriptor cache.
/// Local wins when both could apply.
#[async_trait]
pub trait ClusterClient: Send + Sync {
    fn cache_backend(&self, profile: &str) -> Result<Arc<dyn ClusterCacheBackend>, ClusterError>;
    fn lock_backend(&self, profile: &str) -> Result<Arc<dyn DistributedLockBackend>, ClusterError>;
    fn leader_election_backend(&self, profile: &str)
        -> Result<Arc<dyn LeaderElectionBackend>, ClusterError>;

    /// The only async member: the descriptor needs I/O when remote (§3.18.4).
    async fn descriptor(&self, profile: &str) -> Result<ProfileDescriptor, ClusterError>;
}
```

**The factory methods are sync and pure in both profiles**, which is what keeps `resolve()`'s only await the descriptor. Locally they are a `ProfileRegistry` snapshot read returning the *real* backend — no wrapper, no indirection on the hot path, so Profile 1 keeps today's exact cost (invariant I14). Remotely they construct a `Remote*Backend`, an `Arc` clone plus an interned profile name, whose sync accessors (`consistency()`, `features()`, `provider_name()`) read a `ProfileDescriptor` held in a process-wide cache and whose async methods carry `profile` on each RPC.

**The profile is a request parameter, not a wiring parameter.** Every RPC carries it, and the cluster gear resolves it to a bound backend on arrival (§3.18.1). The client never learns which provider serves a profile — that knowledge stays entirely server-side, which is what makes plugin linkage a cluster-gear concern only. Three consequences shape the rest:

- **Nothing profile-specific is wired.** The process registers **one** `Arc<dyn ClusterClient>`; per-profile backends are derived from it (§3.17.7).
- **Construction needs no round trip**, in either profile. The only thing `resolve()` may wait on is the profile *descriptor*, needed to validate declared capabilities, and it waits on a bounded timeout, never on cluster becoming reachable (§3.10.1).
- **The facade binds lazily, so wiring order stops mattering.** A resolved facade holds `Arc<ClientHub>`, its profile, its recorded requirements and a `OnceLock` for the backend rather than the backend itself. `resolve()` fills that slot eagerly whenever the client is already in the hub — always in Profile 1, normally in Profile 3 — and otherwise the first call fills it. Steady state is one atomic load.

Everything above the seam keeps working because nothing above it knows the difference: the typed profile + fluent resolver, capability validation (same `validate_*_capabilities` call, same `CapabilityNotMet` naming the server-side provider), `scoped()`, the `*WatchEvent` union, `RestartingWatch`, and the `LockGuard`/`LeaderWatch` command-channel handles all map onto the remote backend unchanged. Those pre-existing command-channel seams (`LockGuard::channel`, `ResignReceiver`) are why this is cheap rather than a rewrite: a remote backend is just another servicer of the same channels. Consumers never name a `Remote*Backend` — those types are `pub(crate)`/`#[doc(hidden)]`, constructed solely by `RemoteClusterClient`'s factory methods and handed out as `Arc<dyn _Backend>` (invariant I4).

**Who registers the client** is separate from where the boundary is, and it belongs to the framework's dependency-resolution loop rather than consumer code (§3.17.7). The Profile-2 topology fork is deliberately left undecided (§3.15): store-owned leases make *replica count* free but not *deployment scope*, so one cluster process per host silently makes leases per-host unless every host's process shares one backend configuration — a distinction that must be picked explicitly and enforced when Profile 2 is designed (§7, decision 12).

#### 3.16.1 Crate layout and the `grpc-client` feature

**No new crate.** The contract, its projections and the remote client all live in `cluster-sdk`, behind features, following the platform's reference layout (`examples/toolkit/api-contracts`), which puts the contract, DTOs, projections, generated stubs, error codec, `proto/` and `proto.lock.toml` in the SDK crate and gates every transport dependency behind a feature.

```
gears/system/cluster/
  cluster-sdk/            facades, backend traits, resolvers, scoping, restart — plus:
    src/contract.rs        [grpc-client] #[toolkit::contract] traits (§3.20.2)
    src/dto.rs             [grpc-client] serde/schemars DTOs + ProtoBridge
    src/grpc/              [grpc-client] #[toolkit::grpc_contract] projection + `stubs`
                           (tonic::include_proto!, BOTH *_client and *_server traits)
    src/convert.rs         [grpc-client] DTO ⇄ domain, ClusterError ⇄ CanonicalError codec
    src/client/mod.rs      UNFEATURED  the `ClusterClient` trait (§3.16) — Profile 1 needs it
    src/client/remote.rs   [grpc-client] RemoteClusterClient : ClusterClient
    src/descriptors.rs     [grpc-client] descriptor cache
    src/requirements.rs    UNFEATURED  requirement registry + readiness contributor (§3.10.1)
    src/client/backends/{cache,leader,lock}.rs  [grpc-client] Remote*Backend handles
    src/wiring.rs          [grpc-client] the ConsumerRegistration cluster submits
    proto/, proto.lock.toml, build.rs   GENERATED by toolkit-contract-protogen
  cluster/                 the gear — depends on cluster-sdk with `grpc-client` enabled
    src/main.rs, registered_gears.rs     the cluster-oop binary (§3.17.2)
    src/api/grpc/, api/rest/             hand-written service impls + admin routes (§3.20)
    src/domain/local_client.rs           LocalClusterClient : ClusterClient over ProfileRegistry
    src/domain/registry.rs               ProfileRegistry + BackendInstanceCache (§3.18)
    src/domain/health.rs                 composite readiness healthcheck (§3.17.3)
  plugins/                 unchanged — depend on cluster-sdk with no feature, so no tonic
```

The `ClusterClient` trait and `ProfileDescriptor` stay **ungated**: Profile 1 resolves through the same trait, and gating it would put a `#[cfg]` back in the resolve path. `grpc-client` gates `tonic`/`prost`, the contract traits, the `stubs` module (both `*_client` and `*_server`), `RemoteClusterClient`, the `Remote*Backend` handles and the `ConsumerRegistration`; it is enabled by a consuming gear crate's forwarding feature and by the `cluster` gear crate. There is no separate server feature — `tonic-prost-build` emits client and server traits into one module, so `grpc-client` gates both directions. Plugins enable neither feature and never compile tonic.

`LocalClusterClient` lives in the `cluster` crate, not the SDK, because it dispatches through the `ProfileRegistry` (gear state) — exactly where every other gear's local impl lives. The decisive argument against putting the remote client in the `cluster` crate instead: a consumer linking `cluster` would register the cluster gear in its own process (`#[toolkit::gear]` emits an `inventory::submit!`), silently becoming Profile 1 as a side effect of a crate dependency, and would compile both plugins and `sqlx` — reopening the credential-distribution problem §3.18.2 exists to close. The one real cost of the single-crate layout: the wire contract versions with `cluster-sdk` rather than independently (§3.20.9).

#### 3.16.2 Rejected alternatives

| Alternative | Why rejected |
|---|---|
| **Cut the boundary at the facades** — a `RemoteClusterCacheV1` consumers use instead of `ClusterCacheV1` | Doubles the consumer-facing API, breaks profile transparency (invariant I1), duplicates resolver/scoping/capability logic per transport |
| **Only the cache primitive goes remote**; consumers keep local SDK-default backends over it | Each consumer process then runs its own renewal loops and CAS spin over the network — more round trips, worse contention, a network hop *inside* the renewal loop — and splits the "which backend serves this primitive" decision between operator config and consumer code |
| **Pure REST for the primitives** | Structurally viable (§1.4); what stands against it is throughput, base64 of opaque byte values on the cache path, and the codegen's platform-plane REST restriction |
| **Pure gRPC, no HTTP server** | Forfeits ADR-0005 probes, ADR-0006 internal auth, self-registration and drain — the `oop_http` machinery every OoP gear must have |
| **Hand-written `.proto`** | Forks the platform's proto-from-contract-IR pipeline and its `proto.lock.toml` wire-stability guarantee (§3.20.1) |
| **Cluster as a standalone tonic server outside the gear framework** | Loses directory registration, config rendering, logging, lifecycle |

### 3.17 Making Cluster a Deployable Gear

The gear declares `capabilities = [stateful, system, grpc, rest]`, ships a `cluster-oop` binary, and serves the four coordination services (`cluster.{cache,lock,leader,profile}.v1`) over gRPC. The builder/handle library (§3.7) still exists and is still embeddable — that half is unchanged — but the same crate now also owns the profile registry, the composite readiness check, the local client and the deployable entry point.

#### 3.17.1 Capability set and gRPC serving

| Capability | Trait | Why |
|---|---|---|
| `stateful` | `RunnableCapability` | Owns `ClusterHandle`, plus the profile and session registries |
| `grpc` | `GrpcServiceCapability` | The coordination data plane. `get_grpc_services(&ctx)` returns one `RegisterGrpcServiceFn` per service; `grpc-hub` installs them into its tonic `RoutesBuilder` |
| `rest` | `RestApiCapability` | Two jobs: the `healthcheck()` hook (the framework reads readiness *only* from here) and the admin/diagnostic routes (§3.20.5). No primitive is exposed over REST |
| `system` | flag | Marks cluster platform-tier, so it initialises in the system phase ahead of application gears |

**Phase-order constraint (verified).** The framework's phase order is `pre-init → db → init → post-init → REST → gRPC registration → start → OoP spawn → … → stop`. So `get_grpc_services` and `healthcheck()` both run **before** `RunnableCapability::start` — but backends only exist *after* `start`, where `ClusterWiring::from_config` runs. Neither the service impls nor the healthcheck may capture backends; both capture `Arc<ProfileRegistry>` — created in `init`, *populated* in `start`. An RPC arriving before `start` completes gets `ProfileNotBound`, and `/readyz` reports `Starting` until the registry is populated. This is why the profile registry must be a mutable runtime object rather than a snapshot handed to the services at registration time (§3.18.1).

The `grpc` capability makes `grpc-hub` mandatory wherever `cluster` is linked, Profile 1 included — a hard failure (`RegistryError::GrpcRequiresHub`), not a degradation. §3.15.1 states this in full; the two operator consequences are that an embedding process needs the same `NetworkPolicy` treatment as a dedicated cluster pod, and its `grpc-hub` must bind a port the operator is willing to expose.

#### 3.17.2 Binary target and OoP bootstrap

`cluster/Cargo.toml` adds a `[[bin]] cluster-oop` over `src/main.rs`, depending on `cluster-sdk` with `grpc-client`, `tonic`, `grpc_hub`, `axum` and `clap`. `main.rs` is a `clap` CLI (`--config`, `-v`) feeding `OopRunOptions { gear_name: "cluster", .. }` into `toolkit::bootstrap::oop::run_oop_with_options`; `registered_gears.rs` links `cluster as _` and `grpc_hub as _`.

With `oop_http` present, the bootstrap supplies — none of it cluster's code to write (ADR-0005) — the Axum server with probes served **as soon as the listener binds** (before `start`), background self-registration with exponential backoff, background dependency resolution, the presence loop, the drain sequence and DirectoryService deregistration on shutdown. What `cluster-oop` does **not** contain is the design: no directory registration, no heartbeat, no backoff, no dependency retry and no drain logic. The binary is a CLI over that one bootstrap call plus a `registered_gears.rs` naming the two gears the process must link.

#### 3.17.3 Readiness — the four-state model

Cluster is the coordination dependency of nearly every gear, so its readiness is a fleet-wide gate. ADR-0005 defines four states; cluster's composite healthcheck drives the health dimension:

| State | HTTP | When |
|---|---|---|
| `Starting` | 503 | Before `start` completes, or while the registry has no profiles though config declares some |
| `Ready` | 200 | Every configured profile has three bound backends and every backend instance's probe passes |
| `Degraded` | 200 (`ready: true`) | ≥1 profile healthy, another profile's backend unreachable |
| `Draining` | 503 | Set by the SIGTERM handler before deregistration (§3.17.6) |

The bodies are the framework's `ReadinessReport` verbatim; cluster only supplies the health dimension through `healthcheck()`. Two rulings from ADR-0005 matter here: **`Degraded`, not `Unhealthy`, for one bad profile** — evicting the pod because one DSN is unreachable would take down coordination for every profile; and **DirectoryService registration is not a readiness signal** — a transient directory failure must not pull a healthy pod out of rotation.

**`Degraded` is right for the pod and wrong for the consumer, and the SDK closes that gap.** The framework's dep gate is gear-granular — it has no vocabulary for "profile `oagw` is unavailable" — so a `Degraded` cluster reports `ready: true`, the dep resolves, and a consumer starts taking traffic against a profile that cannot serve. Only the SDK knows which profiles this process resolved, so the SDK builds a per-profile gate on top of the per-gear one: `ProfileDescriptor` carries a per-profile health state (`serving`/`degraded`, §3.18.4); the requirement registry (§3.10.1) reports **not-ready** while any *recorded* profile is non-serving; and it re-reads descriptors on a bounded **10 s** poll so a profile that degrades mid-life pulls its consumers out of rotation and returns them without a restart. This `Unhealthy` is reversible and must not crash-loop — the escalation of §3.10.1 applies to *permanent* errors only.

Per-instance liveness needs a cheap non-mutating probe, which `ClusterCacheBackend` lacks. A defaulted `async fn probe(&self) -> Result<(), ClusterError>` returning `Ok(())` is added — additive and dyn-safe, the same shape as ADR-010's `scan_prefix` (invariant I11). The Postgres plugin implements it as `SELECT 1` on the pool. The framework caches healthcheck results for 2 s and bounds each check by `oop_http.healthcheck_timeout_ms` (default 500 ms), so the probe must be fast or report `Degraded` on timeout rather than hanging.

#### 3.17.4 Discovery and endpoint resolution

Name resolution (hostname to address) is the transport's job and is already solved: hand tonic `http://cluster.{namespace}.svc.cluster.local:{grpc.port}` and hyper's connector resolves it per connection attempt. Construction is pure (`Endpoint::parse` + `connect_lazy` touch no network, which is what lets registration run at any point and await nothing), and reconnects re-resolve, so a rescheduled cluster pod is picked up with no re-discovery step.

Finding the name is a string built from convention, ordered: (1) an explicit override via the platform's designated transport config; (2) k8s DNS by convention — the default in Profile 3, with the namespace from `POD_NAMESPACE`; (3) DirectoryService `resolve_grpc_service` for the non-derivable cases (Profile 2 UDS, dynamic ports, non-k8s deployments). The name is derivable but the **port convention is not** — a generic framework `DnsEndpointResolver` needs a platform-wide port convention that does not exist yet (§7, decision 21). Cluster is not blocked on it: it derives its own endpoint inside its hand-written registration, and the ADR-0004 static override (`gears.<owner>.config.consumer_wiring.<dep>`) covers a directory-less Profile 3 in the interim. Cluster still registers with DirectoryService (the bootstrap does it unconditionally), so platform tooling sees it; consumers simply need not depend on that path.

#### 3.17.5 Platform-plane authentication

Per ADR-0008 the plane is chosen by tenant-scoped vs non-tenant-scoped, not user vs system. Cluster coordination — cache keys, lock names, election names — is non-tenant-scoped platform infrastructure, so cluster is **platform plane**: calls carry an `InternalCredential`, the server resolves a `PlatformSecurityContext`, and no tenant AuthZ runs on the coordination path. The caller's `PlatformIdentity` (SA name, later SPIFFE) is the `ClientId` for lease ownership (§3.19.1) and session indexing (§3.18.3).

The gRPC data plane has **no viable SA-token phase**: `K8sTokenReviewAuthenticator` runs an uncached TokenReview per request, unusable at cluster's rate (§6). So the data plane gets **per-connection** authentication — mTLS + SPIFFE, not "later" — or ships behind a `NetworkPolicy` only; there is no intermediate SA-token step for it, which is why the inbound interceptor and pulling ADR-0006's mTLS phase forward are one work item (§7, decision 5). The REST lifecycle/admin plane uses the SA-token path. `x-secctx-bin` MUST NOT be used over either transport (ADR-0008).

**One case looks like an exception and is not: OAGW's per-tenant rate-limit counters.** Cluster stays platform-plane anyway, and tenant isolation of cluster *data* is the caller's responsibility, expressed through key scoping (§3.8). Cluster has no tenant model and should not acquire one — a key is an opaque string, and threading a `SecurityContext` through a 10k-ops/s path to run tenant AuthZ per operation is a latency cost §6 has no room for, in service of a boundary the caller is better placed to enforce. Two obligations follow, both belonging in consumer docs: a consumer putting tenant-derived data in cluster must scope its keys, and that makes server-enforced namespacing (§7, decision 9) the hardening that would turn the caller's responsibility into a platform guarantee.

#### 3.17.6 Shutdown and the reverse-dependency drain rule

Cluster is a dependency of nearly everything, so it drains **last** (the platform's reverse-dependency rule, an operator requirement enforced by preStop-hook ordering rather than at runtime). The ordering matters less than the rule implies, because leases are store-owned (§3.19): a cluster that drains early revokes no coordination state — it only interrupts in-flight calls and subscriptions. `ClusterHandle::stop`'s phases, with framework phases around them:

| Phase | Owner | Action |
|---|---|---|
| 1 | framework | `/readyz` → `Draining` (503); stop accepting new work |
| 2 | framework | Drain in-flight unary requests up to `drain_timeout_secs` (default 30 s) |
| 3 | cluster | **Close subscriptions, do not revoke leases.** Cache and leader watches get `Closed(Shutdown)`; a blocking `lock()` in progress returns `Err(Shutdown)`. Held locks and leader claims are deliberately untouched — they are store-owned, so they survive this process and are renewed by their holders against the next replica |
| 4 | cluster | Deregister backends from the local hub (`ProfileNotBound` for embedded consumers); clear the `ProfileRegistry` |
| 5 | cluster | Plugin stop hooks, reverse-start order |
| 6 | framework | Deregister from DirectoryService; stop presence and auth-refresh tasks; close listeners |

**Reconciliation with §3.13's embedded shutdown.** §3.13 describes the embedded path, where the process shutting down *is* the leader, so revoking its own `Status(Lost)` then `Closed(Shutdown)` before exit is the honest thing to do — and that path is unchanged for an in-process leader whose own process is stopping. The brokered path here is different in meaning, not in contradiction: when cluster is a broker, the process shutting down holds no lease of its own, so revoking on its way out would falsely unseat a live, working leader in another pod. Under store-owned leases the guarantee is re-stated as **"a leader that loses its lease observes the loss"**, and a cluster restart is not a lease loss. What still propagates end to end is the *subscription* close — `Closed(Shutdown)` becomes a stream message the `Remote*Backend` translates into `*WatchEvent::Closed(ClusterError::Shutdown)`, which `RestartingWatch` re-establishes against the next replica — while a client whose own process is already gone has its locks and claims lapse via TTL (`cpt-cf-clst-fr-shutdown-ttl-cleanup`), now the only path by which a lease ends other than an explicit release. A simultaneous SIGTERM blast to all pods breaks the ordering, so cluster's preStop hook needs a delay or the orchestrator must terminate cluster last, on pain of spurious `Closed(Shutdown)` storms during a rolling restart.

#### 3.17.7 Consumer-side wiring

**Profile resolution is server-side**; what must exist in the consumer's process is an object satisfying `Arc<dyn ClusterClient>`, from which the three backend handles are *derived, not registered*. The split:

| Registered once per process (§3.16) | Derived per `resolve()`, locally | Resolved per request, server-side |
|---|---|---|
| One `Arc<dyn ClusterClient>` — local wins | The three backend handles for a profile, via the client's factory methods | Which provider instance serves `(profile, primitive)` (§3.18.1) |

**The wiring is not hand-written per consumer** — that would contradict ADR-0005 ("gear developers write no registration code; `deps` is the only input"). It rides the framework's proxy-wiring phase, whose generated `wire` closure short-circuits when the hub already holds the impl (local wins), returns `WireOutcome::{Local, Remote}`, and runs once before `start` in both profiles. Cluster adds two things:

1. **Profiles self-register.** A `ClusterProfile` impl emits an `inventory` entry (the same mechanism GTS registration uses), so the wiring enumerates inventoried markers rather than reading a config list — keeping the profile name in its two legitimate places (invariant I10):

   ```rust
   #[derive(Clone, Copy)]
   pub struct EventBrokerProfile;
   impl ClusterProfile for EventBrokerProfile { const NAME: &'static str = "event-broker"; }
   cluster_sdk::register_cluster_profile!(EventBrokerProfile);
   ```

2. **An SDK-submitted `ConsumerRegistration`**, submitted by `cluster-sdk` rather than written per consumer. Cluster writes it by hand — rather than via `#[toolkit::consumes]` — only because that macro emits a REST resolving client and cluster's transport is gRPC; `ConsumerRegistration` is transport-agnostic, so the hand-written registration rides the same inventory-and-replay path and needs no rework if a generated gRPC client ever lands. Its `wire` closure checks the hub (local wins), else registers a lazily-connected `RemoteClusterClient`, then spawns the descriptor prefetch.

**A consumer must NOT declare `deps = [cluster]`.** `deps` is a hard topo-sort edge, and a Profile 3 consumer links no cluster gear at all — declaring it would fail the registry build (`RegistryError::UnknownDependency`) and break invariant I1. Both properties the edge would buy are supplied otherwise: **start ordering** comes from cluster's `system` tier (`run_start_phase` runs every `system` gear ahead of every application gear), and **readiness gating** comes from `ConsumerRegistration::dep_gear: "cluster"`, which the wiring phase registers with the `DependencyChecker` in both profiles. The one exception: a consumer that is *itself* `system`-tier shares cluster's priority group and needs the edge — but may write it only when cluster is co-located, making it a Profile 1 declaration outside the portable surface.

So the consumer's entire cluster-facing surface is the typed `ClusterProfile` marker plus `#[toolkit::gear(...)]` with no `deps = [cluster]`, no endpoint, no profile list, no timeouts — cluster defines **no client-side configuration block at all** (invariant I9), because every field such a block would carry is owned by the framework or the platform transport layer. What varies between profiles is a Cargo feature: a Profile 1 monolith leaves it off and links `cluster` + plugins; a Profile 3 image turns it on and links neither. The generated wiring is gated on `#[cfg(feature = "…")]` evaluated in the consuming gear crate, so each consuming gear declares one forwarding feature that the binary enables.

**Lazy binding keeps late registration correct, and the SDK builds its own client so inline validation stays the norm.** The OoP path today replays `ConsumerRegistration`s after `start`, while consumers resolve in `start` — so a design that waited for the phase would find an empty hub at every Profile 3 `resolve()` and defer validation unconditionally. Instead, when the hub holds no `dyn ClusterClient` and `grpc-client` is on, `resolve()` constructs the `RemoteClusterClient` itself; `connect_lazy` touches no network, so this cannot fail, block or race. (The ordering it works around is being aligned so wiring runs before `start` in both profiles; this branch stays as defence, at the cost of one lazily-built channel.) Two properties make that safe: local-wins is preserved by the crate graph, not by timing (self-construction compiles only under `grpc-client`, which a Profile 1 monolith does not link), and `ClientHub::register` is last-write-wins, so a race drops one redundant channel.

**What lazy binding must not hide.** Tolerating an empty hub makes the cold path work but would trade a loud startup failure for a quiet runtime one — in Profile 1, where an empty hub is always a build or config mistake. Three rules close it: a first call against a still-empty slot returns `ProfileNotBound { profile }` (distinguished by its message, no new error variant, so `ClusterError` stays frozen — invariant I3); the requirement registry doubles as the readiness contributor and lives in the SDK's **unfeatured** core (registered by the first `resolve()`, not by the `ConsumerRegistration`, because in Profile 1 the registration closure never runs); and consumers resolve in `start`, never `init` (phases are global, so no consumer's `init` can see the cluster gear's registrations). `resolve()`'s four steps are then: (1) take `dyn ClusterClient` from the hub, self-constructing under `grpc-client` if absent; (2) ask it for this profile's backend — sync, pure, no I/O; (3) await this profile's descriptor, bounded (§3.10.1); (4) record `(profile, primitive, requirements)`, then validate inline if a descriptor is in hand or leave it to the readiness contributor. There is no fallback branch and no mode flag — the local-wins check in the registration is the entire embedded/remote decision, which is what makes this one code path in both profiles.

#### 3.17.8 Deployment artifacts and operator config

The cluster pod's config carries `oop_http` (probe/drain/internal-auth settings), a `grpc-hub` block with `listen_addr`/`advertise_addr`, and the `cluster` profile matrix. An optional `fence_retention` (default 1h) sets how long a lease record outlives its lease so the fence stays monotonic across a lapse (§3.19.1); it must exceed the longest lease TTL in use. Helm values use the `toolkit-common` library chart with `replicaCount: 1` (a shipped default, not a constraint — §3.19.3), `strategy: RollingUpdate` (safe: leases are store-owned), `grpc: { enabled: true, port: 50051 }`, and `service: { type: ClusterIP }`. Also required: a `NetworkPolicy` restricting the coordination port to platform namespaces, and a preStop delay (§3.17.6). Neither `RollingUpdate` nor `ClusterIP` is load-bearing any more — that is the visible payoff of §3.19: a transiently-doubled pod is harmless, and `ClusterIP` is now a genuine load balancer rather than a pin.

**Schema migrations** stay plugin-owned. The Postgres plugin runs its own `sqlx` migrators inside `build_cache`/`build_lock` during gear `start`, guarded by a Postgres advisory lock and `_sqlx_migrations`, so re-runs are no-ops and concurrent replicas serialize. The framework's DB migration phase does not fit — it is tied to the `db` capability and assumes one database per gear, whereas cluster's DDL is owned by plugins across N distinct DSNs drawn from the profile matrix. Once cluster is a multi-replica pod, migration-on-startup costs blast radius (a failed migration crash-loops the fleet's coordination dependency), least privilege (the runtime DB user must retain DDL rights), and startup budget. The additive answer is a `cluster-oop migrate` subcommand run as a Helm pre-upgrade Job, a defaulted `migrate()` provider hook (no-op for plugins without DDL, invariant I11), and a per-binding `migrations: auto|verify|skip` mode with `auto` as the default so nothing changes for Profile 1.

### 3.18 Runtime Profile Management

Serving remote clients turns a profile from transient wiring state into queryable runtime state. The hub is a type-keyed map — it can return the cache backend for `cluster:event-broker` but cannot answer "what profiles exist", "which provider serves this one", or "is this DSN shared" — and the wire needs all of it: to enumerate/describe profiles, to dispatch a request's `profile` to the right backend, to identify and share provider instances, to track sessions, and to add/remove profiles without a restart.

#### 3.18.1 The `ProfileRegistry`

```rust
/// Runtime, queryable view of every bound profile. Created in `init`, populated
/// by `start`, read by every service impl on every request.
pub struct ProfileRegistry { inner: arc_swap::ArcSwap<RegistrySnapshot> }
struct RegistrySnapshot { generation: u64, profiles: BTreeMap<String, Arc<BoundProfile>> }
pub struct BoundProfile {
    pub name: String,
    pub cache: Arc<dyn ClusterCacheBackend>,
    pub leader_election: Arc<dyn LeaderElectionBackend>,
    pub lock: Arc<dyn DistributedLockBackend>,
    pub descriptor: ProfileDescriptor,     // shipped to clients (§3.18.4)
    pub instances: ProfileInstanceRefs,    // which shared instances (§3.18.2)
}
```

Reads are `ArcSwap::load()` — no lock on the request path, which matters at 10k ops/s under a 5 ms budget — and `generation` lets a client detect that the server's profile set changed (§3.18.5). The registry replaces nothing: the hub registrations stay, because the server's own SDK-default backends resolve through them; the registry is the additional index the wire needs. **It is also what `LocalClusterClient` dispatches through**, so one profile→backend dispatch mechanism serves both profiles: `cache_backend(profile)` is an `ArcSwap::load()` plus a `BTreeMap` lookup returning the real `Arc`, no wrapper interposed, so the embedded hot path is unchanged (invariant I14).

Because `ClusterError::ProfileNotBound { profile: &'static str }` cannot hold a name arriving in a request, profile names are **interned** at registration with `intern(&str) -> &'static str` (`Box::leak`, bounded because the profile set is config-bounded) rather than widening the frozen enum (invariant I3, §7 decision 14).

#### 3.18.2 Backend instance sharing — the N-connections problem

Two halves. **Connections move from consumers to the cluster gear, which shrinks the total.** Today each consumer *process* binding a Postgres profile opens its own pool (`replicas × gears × pool_max_size` plus LISTEN connections and reapers); in Profile 3 consumers hold zero backend connections — one channel each — and the total becomes `cluster_replicas × distinct_instances × (pool_max_size + listeners)`. A 10-replica event broker on a 5-connection pool goes 50 → 5 at one cluster replica. This also narrows the deferred credential problem to one deployment target.

**Distinct connection strings do mean distinct hot instances, so the job is not to multiply them needlessly.** A `BackendInstanceCache` keyed by `(primitive, provider, canonical_options_digest)` holds `Weak` refs while profiles hold strong `Arc`s, so an instance's `StopHook` runs exactly when the last profile releases it (which is what makes dynamic profile removal safe, §3.18.5). Design points: canonicalisation must be conservative — digest the options map with sorted keys *after* `${VAR}` expansion, and do **not** attempt DSN-semantic equivalence, because a false merge silently points a profile at the wrong database while a false split only costs a redundant pool; `secret_ref` participates in the key; **sharing is not merging profiles** (two profiles sharing one cache still have independent SDK-default LE/lock backends above it, and a consumer treating separate profiles as an *isolation* boundary was already mistaken — §6); and pool sizing must account for fan-in, since one instance now backs every profile bound to it and every remote client behind those profiles.

#### 3.18.3 Session tracking and the subscription sweep

The `profile` field is the routing key; the **session index** holds the per-client state that is genuinely connection-scoped — watch subscriptions. It carries a `ClientId { identity: PlatformIdentity, instance_id }` and, per subscription, its `profile`, `kind` and `target`. **The registry holds subscriptions and an index, not leases**: `Renew`/`Release`/`Resign` are conditional writes predicated on the token the client presents (§3.19.1), so the replica serving them needs no memory of the acquire, and reaping on client death is TTL-driven exactly as the contract specifies. Shutdown fan-out is watches only. Observability is gauges per `(profile, primitive)` — `profile` and `provider` are bounded and allowed as labels; lock/election **names** and cache **keys** are not (invariant I15, ADR-004). A per-replica lease *index* is deliberately not built: nothing in the lease path may read it (invariant I7), and the diagnostics it would serve read the **store** the moment there is more than one replica.

**The watch-subscription sweep.** Abandoned watch subscriptions — entries whose client will never return — are swept; leases need no equivalent because they expire in the store at their `deadline`. The key is `last_seen`, written on `open`, on stream `attach`, and on every sweep pass that observes a live reader (which is what makes one timestamp sufficient: a subscription whose client dies gets a full grace window measured from the last pass rather than from a `join` an hour earlier). A subscription is reaped when it has **no live reader** and has been that way for at least the grace window. The server's stream task must select on its outbound channel closing so a cancelled or crashed client's receiver is dropped promptly. Cadence is **5 s** (matching the plugins' lock-reaper default) and the grace multiplier is **3** (a 15 s window, above one cadence and far above a `join`→`await_change` round trip). It emits `cluster_subscriptions_reaped` (counter) and `cluster_subscriptions_active` (gauge), both per `(profile, primitive)`; an election name appears in neither. Nothing the sweep touches is a lease: reaping an entry revokes no leadership and fails no renewal (invariants I7/I8).

#### 3.18.4 Profile discovery and capability validation over the wire

The sync accessors (`consistency()`, `features()`, `provider_name()`) cannot make a call, so a remote backend reads a `ProfileDescriptor` supplied by `DescribeProfiles` — one call returning a descriptor per profile plus the server `generation`. It has two readers on two paths, which is what lets validation be inline in the normal case without ever blocking startup: a **prefetch** (one background `DescribeProfiles` per process covering every inventoried profile marker, populating an `Arc<OnceLock<ProfileDescriptor>>` per profile) and **`resolve()`**, which awaits this profile's entry bounded by the resolve timeout — normally a cache hit, otherwise its own `DescribeProfiles`. Validation uses the same `validate_*_capabilities` code and produces the same `CapabilityNotMet { primitive, capability, provider }` on either path, where `provider` is the **server-side** provider (e.g. `postgres`), not `remote` — the operator must see which real backend failed. Each descriptor also carries **per-profile health** (`ProfileHealth`), which the readiness contributor reads for every recorded profile so a degraded profile pulls *its* consumers out of rotation without touching consumers of healthy profiles (§3.17.3). Because descriptors are cached client-side, a server-side profile change can make a client's view stale; `generation` is the detector (§3.18.5).

#### 3.18.5 Dynamic profile reload

Full dynamic reconfiguration is not required for the first deployable version, but the mechanisms exist for it: phase A (initial) has profiles fixed at `start`, the registry runtime-queryable but immutable, `generation = 1`; phase B **adds** a profile without restart (build its backends reusing shared instances, register, swap a new snapshot with `generation + 1` — purely additive); phase C **removes/rebinds** a profile (swap the snapshot first so new resolves get `ProfileNotBound`, then close that profile's subscriptions with `Closed(Shutdown)`, then drop its instance refs — the `Weak`-keyed cache runs each `StopHook` only when the last referencing profile is gone). The trigger is an explicit admin endpoint (`POST /admin/profiles/reload`), not a config-file watch. On a `generation` mismatch a client re-issues `DescribeProfiles` and re-validates the refreshed descriptor against every requirement recorded for that profile — an unmet one flips the gear to `Unhealthy`. This is why the requirement registry is load-bearing even though validation is normally inline: a mid-life profile rebind has no `resolve()` call to fail, so readiness is the only enforcement point.

#### 3.18.6 Capacity, isolation and failure modes

| Concern | Mitigation |
|---|---|
| One instance now backs the whole fleet; `pool_max_size: 5` is fleet-wide | Revised sizing guidance per profile; `cluster_backend_pool_saturation` gauge; WARN on acquire-timeout. Exhaustion surfaces as `Provider{ResourceExhausted}` ⇒ retryable-with-backoff |
| One noisy client starves others on a shared instance | Per-`ClientId` concurrent-request and open-subscription caps (over-cap ⇒ `ResourceExhausted`). The cap is **per replica**, so a fleet-wide limit is `cap × replicas` unless accounting moves into the store — the right trade at single-digit replica counts |
| One unreachable profile must not fail the whole pod | Readiness `Degraded`, not `Unhealthy` (§3.17.3) |
| Cluster gear is a single point of failure for coordination | **Structurally removable** (§3.19): any replica serves any lease operation, so a lost replica costs subscribers one `RestartingWatch` cycle, not a lock. The shipped default is still one replica pending the failover suite (§3.19.3) |
| Consumer up before cluster | Framework background dep resolution + readiness gating (§3.10.1) — no startup failure |

### 3.19 Store-Owned Leases

A lease — a held lock or a leader claim — is a **record in the backing store, fenced by a token the client presents; no server-side session is required to interpret it** (ADR-012). Two requirements drive it: a cluster replica must be replaceable — restarted, upgraded, rescheduled — without revoking the fleet's locks, and coordination must scale past one process. Holding lease state in the replica that issued it satisfies neither: it makes every deploy a revocation event and every second replica a correctness hazard. This is the shape etcd (lease in the raft log), Consul (session in server state) and Kubernetes (`Lease` object) all converged on: any member serves any renew.

#### 3.19.1 The model

Every lease-bearing operation is a **conditional write predicated on state the store already holds**, so the replica handling the request needs no memory of the one that issued it:

| Element | Definition |
|---|---|
| **Lease record** | Lives in the backend row: `owner` (the `ClientId`), `deadline` (absolute, server-clock), `fence` (per lease name, monotonic within the retention window below) |
| **Lease token** | What the client holds and presents: `(name, owner, fence)`, opaque and server-issued — the whole of the authority, with no lookup table behind it |
| `renew(token, ttl)` | `UPDATE … SET deadline = now() + ttl WHERE name = $1 AND owner = $2 AND fence = $3 AND deadline > now()`. Zero rows ⇒ `LockExpired`, identical on every replica because it is a property of the row |
| `release(token)` | Same predicate, `DELETE`. Absence ⇒ `Ok` (idempotent by absence, §3.20.8) |
| `acquire(name, ttl)` | Insert-or-steal-if-expired, bumping `fence`. A stolen lease strictly increases the fence, so a stale holder's `renew` can never match again |
| **Liveness** | The client's own `renew` cadence. A holder that stops renewing lapses at `deadline` — the TTL safety net the contract already specifies |

Three properties follow, and they are the whole justification: **any replica serves any request** (nothing is affine, so a `ClusterIP` Service round-robining across replicas is correct); **a cluster restart revokes nothing** (no process vouches for a lease); and **stale holders are fenced, not trusted** (the previous holder's operations fail their predicate rather than succeeding against a lease someone else now owns).

**Where the fence comes from.** The fence must not restart while any stale token bearing the old value could still be presented, and the cache-backed defaults do not get that for free: `CacheEntry.version` is monotonic *per key while the key exists* (§3.1) and the TTL reaper deletes expired keys, so the next insert writes `version: 1`. The lease record therefore carries its own fence and outlives its lease: the fence lives in the value (the record is `{ owner, deadline, fence }` serialised into the cache value or the native lock row; `version`/`xmin` still drives the CAS, but authority is the `fence` field); acquisition CASes rather than delete-inserts, so the counter survives a change of owner; and the record's physical expiry is `deadline + fence_retention`, so the reaper skips records inside the retention window. `fence_retention` defaults to an hour — orders of magnitude above any lease TTL, one small row per lease *name*. **The guarantee, stated exactly**: a fence value is never reused for a given lease name within `fence_retention` of its lease *lapsing* (a voluntary `release` deletes the record, dropping the fence with it; lapsing is the case a stale holder can be in). The backends reject a **zero** retention window at startup; the `ttl >= fence_retention` case is caught at acquisition as a `warn!` naming both durations.

**This is why the fence stays internal.** Exposing it as `LockGuard::fence()` for external fencing would promise *global* monotonicity for the lifetime of the protected resource, which no backend here can honour across the retention window. External fencing, if a consumer needs it, is an additive `LockGuard` method backed by a source that can promise it (a Postgres sequence), and needs its own ADR (§7, decision 17b). **Renewal runs at the client's cadence** because the token it holds is the authority, which keeps renewal doubling as a consumer-liveness signal — a wedged holder stops renewing and loses its claim (invariant I8, §6).

#### 3.19.2 Restarts and upgrades revoke nothing

A cluster restart is not a lease event: rolling the pod, upgrading it, or losing it to a node failure leaves every held lock and leader claim exactly where it was, and consumers observe a dropped subscription and one `RestartingWatch` cycle, nothing more. **The deadline is the only liveness authority, in every profile.**

This retires the Postgres **liveness beacon** — the one piece of the shipped lock that assumed otherwise. That beacon (an advisory key drawn fresh per incarnation, held on a dedicated connection, whose disappearance from `pg_locks` published "the process that took this lock is gone") bought sub-TTL reclaim of a crashed holder's locks, and was sound precisely when the process holding the beacon was the process using the lock. Brokered, that stops being true: cluster's beacon would vouch for locks held by other, live consumers, so its restart would revoke the fleet's locks. The predicate becomes `expires_at <= now()` — one branch, no `pg_locks` scan, no beacon columns, no dedicated connection — and **it changes for everyone**, because keeping the beacon for in-process acquisitions and dropping it for brokered ones would give the same code and config two reclaim timings and a class of bug that reproduces in only one profile (invariant I1). The price, stated plainly: a crashed holder's lock now lingers until its TTL in every profile, where the embedded path previously reclaimed it early — the same bound every non-Postgres backend already has, and a reason to keep lock TTLs tight. The migration is a column drop (`holder_beacon_hi`/`holder_beacon_lo` and their check constraint), and the shutdown drain that deleted rows on the way out is removed — deleting rows on exit is the revocation this section exists to prevent.

#### 3.19.3 Replica count

`replicaCount: 1` is a shipped default, not a constraint — nothing in §3.19.1 breaks at two replicas, but "correct in principle" is not "the failover suite passes", and that suite is later work. Before conformance: `replicaCount: 1`, `RollingUpdate`, `ClusterIP`, and **no enforcement** — replica count is a capacity decision, so there is nothing to enforce in Helm, at startup, or anywhere else. After: `replicaCount: 2+`, `maxUnavailable: 0`, a PodDisruptionBudget (headless is also safe, just unnecessary). The gate is empirical: a renew issued against a *different* replica than the acquire succeeds; a rolling upgrade under held locks revokes nothing; a killed replica costs subscribers one `RestartingWatch` cycle and no lease; a stolen-after-expiry lease fences its predecessor.

### 3.20 Wire Contract & API Projection

#### 3.20.1 Contract-first, three projections

The platform mandates a contract-first pipeline: a `#[toolkit::contract]` Rust trait is the single source of truth, emitting a transport-neutral `ContractIr`; `#[toolkit::rest_contract]` and `#[toolkit::grpc_contract]` project it; `toolkit-contract-protogen` generates the `.proto` from the IR plus schemars schemas, with `proto.lock.toml` guaranteeing wire-stable field numbers. `openapi.json` is a published *output*, never a codegen input. Cluster follows this directly on the shipped macros — it hand-writes no `.proto`.

**What is generated, and what is not.** The `.proto` + `proto.lock.toml` (protogen), the prost messages and `*_client`/`*_server` traits (`tonic-prost-build` via `build.rs`), the consumer-side client implementing the contract trait (`#[toolkit::grpc_contract]`), and the `ClusterError` ⇄ `CanonicalError` codec (`#[derive(ContractError)]`) are all generated. **The gRPC service impls are hand-written, by design** — server codegen is explicitly out of scope for the platform's contract pipeline (the supported escape hatch for service authors), so cluster's four service impls are the sanctioned permanent pattern, not interim glue. Each does the same steps: proto → DTO, resolve the caller identity from metadata (§3.17.5), profile dispatch through the `ProfileRegistry` (§3.18.1), backend call, and `ClusterError` → `Status` (§3.20.7). Two disciplines keep the generated artefacts behind the `*Api` trait boundary: write the security-context first parameter into every trait method (even though the gRPC projection does not yet enforce it), and follow protogen's naming conventions. A contract change should never reach `cluster/src/api/grpc/`; if it does, the trait boundary leaked, and that is the finding.

#### 3.20.2 The contract traits

The wire mirrors the **backend traits**, not the facades: the facades' sync/local concerns (`resolver`, `scoped`, `status()`, `is_leader()`, `auto_restart`) stay client-side. Four contracts (`ClusterCacheApi`, `DistributedLockApi`, `LeaderElectionApi`, `ClusterProfileApi`), each with a security-plane context as the first parameter of every method and `#[idempotency(..)]` throughout, `#[streaming]` on the push-shaped ones:

```rust
#[toolkit::contract(gear = "cluster", version = "v1")]
pub trait ClusterCacheApi: Send + Sync {
    #[idempotency(SafeRead)]
    async fn get(&self, ctx: &PlatformSecurityContext, req: GetRequest)
        -> Result<GetResponse, CanonicalError>;
    #[idempotency(IdempotentWrite)]
    async fn put(&self, ctx: &PlatformSecurityContext, req: PutRequest) -> Result<(), CanonicalError>;
    #[idempotency(NonIdempotentWrite)]
    async fn compare_and_swap(&self, ctx: &PlatformSecurityContext, req: CasRequest)
        -> Result<CacheEntryDto, CanonicalError>;
    // … put_if_absent, delete, contains, compare_and_delete, scan_prefix
    #[idempotency(SafeRead)] #[streaming]
    fn watch(&self, ctx: &PlatformSecurityContext, req: WatchRequest)
        -> Result<CacheWatchEventDto, CanonicalError>;
}
```

The lease-bearing traits are the least obvious part of the contract: **every operation after the acquire is predicated on a token rather than addressed by a handle** (§3.19.1). `DistributedLockApi::try_lock`/`lock` return a `LeaseToken { name, owner, fence }`; `renew` and `release` take a `LeaseRef` (a conditional write on `(name, owner, fence, deadline > now())`, answerable by any replica). `LeaderElectionApi` is the same shape plus a subscription: `join` returns `{ lease_token, election_id, initial_status }`, `renew`/`resign` take a `LeaseRef`, and `await_change` is keyed by the `election_id` — the one piece of replica-local state, because it addresses a subscription rather than a lease. `ClusterProfileApi` carries `describe_profiles` alone. The error type is `CanonicalError` throughout; the `Remote*Backend` impls translate it back into `ClusterError` so the consumer-facing contract is unchanged.

The security context is required on the *contract* traits (a platform binding constraint), hard-enforced on the REST projection and a type-assertion on the gRPC one; cluster follows it either way. It costs nothing on the wire — it carries an IR `FieldRole` that filters it out of the generated schema, so the credential travels in gRPC metadata and resolves server-side (§3.17.5). **The requirement stops at the contract traits**: the three facades are not contracts and never become contracts, which is the §3.16 seam doing its job. A naming trap to avoid: `is_remote_capable()` returns true for both `Api` and `Backend` suffixes, so annotating the plugin-facing `*Backend` traits with a projection would push a security-context parameter onto the trait every plugin implements, breaking `cpt-cf-clst-nfr-plugin-stability` for no benefit. The two-trait split (`*Backend` local and serde-free; `*Api` carrying the wire contract) is load-bearing, not stylistic (invariant I11, ADR-011). Every method is unary or single-return-`#[streaming]`, both of which the IR already expresses, so cluster needs no IR extension.

#### 3.20.3 Facade → wire mapping

Of every public method across the three facades and two handle types, **only three are push-shaped** — `cache.watch`, `cache.watch_prefix` and `LeaderWatch::changed()`; everything else is either client-local or a plain unary call. `resolver`/`scoped`/`status()`/`is_leader()`/`auto_restart` and the `PollingPrefixWatch` are all client-local (no remote call). The cache mutations and reads are one unary contract method each; `compare_and_delete` **must** be on the wire even though the trait defaults it, because the default is a non-atomic `get`-then-`delete` (a real race over a network) and the CAS-based leader release depends on it. `elect`/`try_lock`/`lock` are unary and return a token; `renew`/`release`/`resign` are unary against that token. Nothing consumer-facing is unreachable, and nothing new is exposed to consumers.

#### 3.20.4 Cache, lock, and leader contracts

Semantics are inherited verbatim from §3.3 — this is a transport, not a redefinition — and every request carries `profile`. One deliberate divergence: **`scan_prefix` is paginated** on the wire (`page_token`, `limit`, `next_page_token`) where the in-process trait returns an unbounded `Vec<String>`; the `RemoteCacheBackend` loops pages and presents the flat `Vec` the trait requires.

**The lock primitive needs no streaming.** Its entire surface is four request/response operations (`try_lock`, `lock`, `renew`, `release`), and the client-death case is covered by the contract's TTL safety net. Server side there is **no lease table** — the lease is the backend row, and the gear translates a token into a predicate and executes it, which is what lets a `renew` land on a replica that never saw the acquire. `LockGuard::channel(name, 1)` builds the guard and spawns a pump translating `LockCommand::Renew`/`Release` into unary RPCs (so the guard cannot carry the lease id — it lives in the pump task's closure — which costs one client-side task per held lock). Consequences: liveness is TTL exactly as specified; `Drop` stays a no-op with no I/O; blocking `lock()` is a slow unary call whose server abandons the wait on client disconnect; no session-reaping machinery for locks.

**Leader election is not a streaming API either** — three unary operations (`join`, `renew` issued by the client's pump on the `renewal_interval()` cadence, `resign`) and one subscription (`changed()`). Renewal is **client-driven** against the lease token: a wedged consumer whose renewals stop must lose its claim, and renewing in the gear on its behalf would keep a dead consumer elected (§6, invariant I8). **A failed renewal is a status change, not a terminal close**: zero rows matched means this instance no longer holds the lease — stolen after expiry, or fenced by a newer holder — which is exactly the in-process meaning of losing leadership, so the pump emits `Status(Lost)` and keeps the subscription open (only a `Closed` from the server or an unrecoverable transport failure terminates the watch). The subscription is purely an observation channel — losing it costs a re-subscribe, not a leadership change.

#### 3.20.5 Profile / admin contract

`DescribeProfiles` runs over gRPC (§3.18.4). The admin surface — `GET /admin/profiles`, `GET /admin/sessions` ("who holds lock X" during an incident), `POST /admin/profiles/reload` — is REST because it is operator surface where `curl`-debuggability is the point, and it needs an authorization decision distinct from the data plane (`/admin/sessions` reveals lock and election names across all clients). These are **hand-written `OperationBuilder` routes, not a `#[rest_contract]` projection**, which sidesteps the platform-plane REST restriction entirely (that restriction is about generated clients sourcing an internal token, and these have no generated client). They are built `.authenticated()` and **without** `.exposed()`, so `OperationSpec.is_public` stays `false` — which keeps them off the gateway by the property of the spec they publish, under #4403's directory-driven route exposure, rather than by any call cluster declines to make.

#### 3.20.6 Watch stream semantics

The union shape (ADR-003) maps directly; the wire adds no new states. `Event`/`Status` is an ordinary value event; `Lagged { dropped }` is the server's per-stream buffer overflow *or* its own upstream watch reporting lag (both collapse to "you missed events, re-read"); `Reset` is the server's upstream re-established; `Closed(err)` is terminal. Two projections satisfy the contract identically — streaming (gRPC server-stream or SSE) and long-poll (`AwaitChange { subscription_id, since_cursor, timeout }`) — the choice being per §1.4; long-poll is measurably worse only for high-rate cache-prefix watches. Common rules: a **bounded per-subscription buffer, drop-then-`Lagged`** so one wedged consumer never stalls a shared watch; per-key ordering preserved (HTTP/2 delivers in order per stream, and one watch is one stream); at-most-once unchanged; client cancel = unsubscribe. Transient backend errors are not events (retried inside the server's watch task); a transient *transport* error surfaces as `Closed(Provider{ConnectionLost})`, which `RestartingWatch` classifies retryable and transparently resubscribes — the auto-restart combinator, built for in-process backends, turns out to be the load-bearing piece of the remote story.

#### 3.20.7 Error model over the wire

**The wire form is `CanonicalError`** (`toolkit-canonical-errors`), not a bespoke cluster DTO. `ClusterError` stays the frozen Rust-facing contract consumers match on (unchanged — no consumer edit); the client reconstructs it from the canonical variant plus a typed cluster detail context carrying the discriminant and payload, via `#[derive(ContractError)]` (which serialises each variant's payload into `context["data"]` and bounces unknown `(error_domain, error_code)` pairs back as the original `Problem` — the forward-compatibility rule §3.20.9 needs). The mapping is `InvalidName`/`InvalidConfig` → `InvalidArgument`, `ProfileNotBound` → `NotFound`, `CapabilityNotMet` → `FailedPrecondition`, `LockContended`/`CasConflict` → `Aborted`, `LockExpired` → `FailedPrecondition`, `LockTimeout` → `DeadlineExceeded`, `Shutdown` → `ServiceUnavailable`, and the `Provider{..}` kinds to `ServiceUnavailable`/`DeadlineExceeded`/`ResourceExhausted`/`Internal` by kind.

Four consequences to get right: **`ProviderErrorKind` must be carried explicitly, not inferred** — `Provider{ConnectionLost}` and `Shutdown` both map to `ServiceUnavailable` yet one is retryable and the other terminal, and `RestartingWatch` reads retryability from `ProviderErrorKind`, so the discriminant travels in the detail context; **`Provider{AuthFailure}` → `Internal`, not `Unauthenticated`** — the failure is the cluster gear's credentials against Postgres/Redis, not the caller's against cluster; **a transport failure with no canonical body is synthesised client-side as `Provider{ConnectionLost}`** — retryable, so an unreachable cluster behaves for consumers exactly like an unreachable Postgres; and **a lease-keyed operation that matches nothing** reconstructs as `LockExpired` for `renew` (the guard holds the name), `Ok` for `release`/`resign` (idempotent by absence), and `Closed(Shutdown)` for `AwaitChange` (a subscription can outlive the replica serving it — the one row that is new behaviour in Profile 3). `CasConflict` carrying a full `Vec<u8>` value is expressible but base64'd in JSON `context`, a cost on the hot CAS error path measured in phase 0a (§7, decision 17a); the version-only fallback stays contract-legal.

**Input validation is a server-side boundary check.** The `cluster-sdk` facades validate keys and coordination names before the RPC, but that runs in the consumer's process, so the four gRPC services re-run the **same** validators at the boundary — closing an invariant-I1 divergence where Profile 3 would otherwise accept keys and names Profile 1 rejects. **Lease durations are clamped server-side** to a ceiling drawn from the fence-retention default, so an unauthenticated caller cannot park a lock, or a waiting server task, for years.

#### 3.20.8 Idempotency and retry

The platform's `#[idempotency(..)]` annotation drives retry-aware generated clients. **Two annotation layers**: `#[idempotency(SafeRead | IdempotentWrite | NonIdempotentWrite)]` on the contract trait feeds the IR, while the gRPC projection carries its own `#[rpc(name)]`, `#[idempotency_level(..)]` and an opt-in `#[retryable]` marker that actually licenses retry — and the two must agree, which nothing cross-checks, so it belongs in review. Reads (`get`, `contains`, `scan_prefix`, `describe_profiles`) are `SafeRead`; `put`/`delete`/`compare_and_delete` are `IdempotentWrite`; `put_if_absent`/`compare_and_swap` are `NonIdempotentWrite` (**must not be auto-retried** — a lost response on a successful first attempt would make a retry read "someone else won"). **Lease acquisition** (`try_lock`, `lock`, `join`) is `NonIdempotentWrite` with **no `#[retryable]`**: the same false negative one layer up and worse, because the caller has no key to re-read — a retried `TryLock` reports `LockContended` (self-contention), a retried `Join` reports another leader when the caller *is* the leader. **Lease release** (`release`, `resign`) is `IdempotentWrite`, retry-safe only because the server makes it idempotent by absence (also what keeps tokens un-probeable). **Lease renewal** is `IdempotentWrite` but absence must **not** be `Ok` — the caller needs to learn it lost the lease, so an unknown/non-owned token is `LockExpired`. An optional `client_request_id` rides the mutating requests (unused in phase 1) so server-side dedup can land without a wire break — most valuable on the acquisitions, where dedup is the only thing that can turn a retried acquire from a wrong answer into the right one. Streams carry no RPC timeout (relying on HTTP/2 keepalive); an RPC timeout on a watch stream would sever every watch on a fixed interval.

#### 3.20.9 Versioning

Rust facades + backend traits version per-primitive `*V1`/`*V2` as today (ADR-005). The contract traits + wire (`cluster.v1`) are **additive-only within v1**: new optional fields, new enum values with `*_UNSPECIFIED = 0`, new methods; `proto.lock.toml` guarantees field numbers never move. A new Rust facade major does not force `cluster.v2` unless the wire shape changes, and vice versa — the wire mirrors the backend traits, which are more stable than the facades. Skew rules: a newer client tolerates missing optional fields and maps unknown enum values to `Provider{Other}`; an older client ignores unknown fields; both directions are tested because a rolling deployment produces both. Because the wire ships inside `cluster-sdk`, it carries no independent crate version — the `cluster.v1` / `proto.lock.toml` pair (`proto.lock.toml` for field *numbers*, the committed `.proto` diff for field *types*) is the wire-compatibility contract, and the crate version is not a wire signal (invariant I12).

## 4. Additional Context

### 4.1 Backend Feature Compatibility

**Sub-capability implementation strategy per backend:**

| Backend | Cache | Leader Election | Distributed Lock |
|---------|-------|----------------|-----------------|
| **Standalone** (in-process, shipped) | Native (HashMap + AtomicU64) | Native (watch channel) | Native (Mutex + Notify) |
| **Postgres** (shipped) | Native (table + LISTEN/NOTIFY) | SDK default (on PG cache) | Native (`cluster_lock` row, owner + fence) |
| **K8s** (shipped) | Native (`ClusterCacheEntry` CRD + `spec.version`) | Native (Lease API) | Native (Lease API) |
| **Redis** (follow-up) | Native (GET/SET/Lua) | SDK default (on Redis cache) | Native (SET NX EX + Lua) |
| **NATS KV** (follow-up) | Native (KV bucket + revision) | SDK default (on NATS cache) | SDK default (on NATS cache) |
| **etcd** (follow-up) | Native (KV + `mod_revision`) | Native (election API) | Native (lock API) |

**ProviderErrorKind mapping per backend:**

| ProviderErrorKind | Redis (fred) | Postgres (sqlx) | NATS (async-nats) | K8s (kube) | etcd (etcd-client) |
|---|---|---|---|---|---|
| `ConnectionLost` | `ErrorKind::IO` | `Error::Io` | `ConnectErrorKind::Io` | `HyperError` | `TransportError` |
| `Timeout` | `ErrorKind::Timeout` | `Error::PoolTimedOut` | `*ErrorKind::TimedOut` | hyper timeout | gRPC `DeadlineExceeded` |
| `AuthFailure` | `ErrorKind::Auth` | SQLSTATE `28xxx` | `Authentication` | HTTP `401`/`403` | gRPC `Unauthenticated` |
| `ResourceExhausted` | `ErrorKind::Backpressure` | — | — | HTTP `429` | gRPC `ResourceExhausted` |

### 4.2 Recommended Deployment Combinations

| Deployment | Config | Cache | LE | Lock | SD | Notes |
|-----------|--------|-------|----|----|----|----|
| Dev / single-instance | `provider: standalone` | Standalone | Standalone | Standalone | Standalone | Zero deps |
| Multi-instance, no K8s | `provider: postgres` | Postgres | SDK default | Postgres | SDK default | Zero new infra |
| K8s, low-throughput | `provider: k8s` | K8s CRD | K8s Lease | K8s Lease | K8s Lease (per instance) | Zero new infra; needs a one-time CRD install |
| K8s + Redis (recommended) | hybrid | Redis | K8s Lease | Redis | K8s Lease (per instance) | Best of both |
| Redis-only | `provider: redis` | Redis | SDK default | Redis | SDK default | Single infra dep |
| NATS stack | `provider: nats` | NATS KV | SDK default | SDK default | SDK default | Single infra dep |
| etcd available | `provider: etcd` | etcd | etcd (native) | etcd (native) | SDK default | Best coordination guarantees |

### 4.3 Existing Code Migration

The following existing code overlaps with cluster capabilities and will be migrated in **separate follow-up changes**:

| Existing Code | Location | Overlap | Migration Plan |
|------|----------|---------|---|
| `LeaderElector` trait + `K8sLeaseElector` | `mini-chat/src/infra/leader/` | Leader election (production-quality K8s Lease impl) | Extract into `cf-k8s-cluster-plugin`; mini-chat consumes via `LeaderElectionV1::resolver(&hub).profile(MiniChatProfile).resolve()` |
| File-based advisory locks | `libs/toolkit-db/src/advisory_locks.rs` | Distributed lock (single-host only, no fencing) | Not reusable — cluster provides true distributed locks via `DistributedLockV1`. Gears migrate on adoption. |

### 4.4 Consumer & Gear End to End

This is the concrete counterpart to §3.16's transparency claim: one consumer gear that coordinates through the facades, the cluster gear that serves it, and what differs between the deployment profiles.

**Consumer side.** The profile name is typed in exactly two places — the marker and the `.profile()` call (invariant I10):

```rust
// reservations/src/cluster_profile.rs
#[derive(Clone, Copy)]
pub struct ReservationsProfile;
impl ClusterProfile for ReservationsProfile { const NAME: &'static str = "reservations"; }
cluster_sdk::register_cluster_profile!(ReservationsProfile);      // inventory entry (§3.17.7)
```

Facades are resolved in `start` (never `init`, §3.10.1) and stored; nothing names a transport, and there is no `deps = [cluster]` (§3.17.7):

```rust
#[toolkit::gear(name = "reservations", capabilities = [rest, stateful])]
struct ReservationsGear { hub: OnceLock<Arc<ClientHub>>, service: OnceLock<Arc<Service>> }

#[async_trait]
impl RunnableCapability for ReservationsGear {
    async fn start(&self, _cancel: CancellationToken) -> anyhow::Result<()> {
        let hub = self.hub.get().expect("init ran first");
        let cache = ClusterCacheV1::resolver(hub)
            .profile(ReservationsProfile)
            .require(CacheCapability::Linearizable)     // enforced per §3.10.1
            .resolve().await?;
        let locks = DistributedLockV1::resolver(hub)
            .profile(ReservationsProfile).resolve().await?;
        let elections = LeaderElectionV1::resolver(hub)
            .profile(ReservationsProfile).resolve().await?;

        let svc = Arc::new(Service::new(cache.scoped("reservations")?, locks));
        let _ = self.service.set(Arc::clone(&svc));

        // Exactly one consumer replica runs the reconciler. The lease is store-owned,
        // so leadership survives a cluster restart; renewal is client-driven in both
        // profiles, which is what makes a wedged holder lose its claim (§3.19, §3.20.4).
        let watch = elections.elect("reservation-reconciler").await?;
        tokio::spawn(async move {
            watch.run_while_leader(Duration::from_secs(5), move || {
                let svc = Arc::clone(&svc);
                async move { svc.reconcile_expired_holds().await }
            }).await;
        });
        Ok(())
    }
}
```

What `resolve()` finds in this gear's hub is one `dyn ClusterClient`, in both profiles. That is the whole of the difference:

| | Profile 1 (monolith) | Profile 3 (own pod) |
|---|---|---|
| Who registered `dyn ClusterClient` | the co-located cluster gear's `start` — a `LocalClusterClient` over the `ProfileRegistry` | the framework's proxy-wiring phase (or `resolve()` itself) — a `RemoteClusterClient` over the gRPC channel |
| What `resolve()` does | `cache_backend("reservations")` → the **real** `PostgresCache`, no wrapper | the identical calls → a `RemoteCacheBackend`, an `Arc` clone plus an interned name |
| Descriptor for `require(Linearizable)` | intrinsic — the bound object *is* the real backend | from the prefetch cache, or one `DescribeProfiles` under the bounded timeout |

No cluster backend is ever registered into a *consumer's* hub in Profile 3, and none needs to be — the backend is *derived* from the client rather than registered, because a peer process cannot insert into another's hub. Two things make this one code path rather than two: the `ClusterClient` trait is unfeatured (no `#[cfg]`, no fallback branch in the resolve path), and the embedded/remote decision was already made once by the local-wins check in the registration. The facade holds `Arc<ClientHub>` + profile + `OnceLock` for the backend, so a `resolve()` that ran before the client was registered still succeeds and binds on first use; steady state is one atomic load.

The request path takes a lock and reads/writes coordination state:

```rust
pub async fn hold_seat(&self, seat_id: &str, who: &str) -> Result<HoldOutcome, ClusterError> {
    let guard = match self.locks.try_lock(&format!("seat/{seat_id}"), self.lock_ttl).await {
        Ok(g) => g,
        Err(ClusterError::LockContended) => return Ok(HoldOutcome::Busy),
        Err(e) => return Err(e),
    };
    let key = format!("hold/{seat_id}");
    let outcome = match self.cache.get(&key).await? {
        Some(_) => HoldOutcome::AlreadyHeld,
        None => { self.cache.put(&key, who.as_bytes(), self.hold_ttl).await?; HoldOutcome::Granted }
    };
    guard.release().await?;
    Ok(outcome)
}
```

Two planes meet in one request: the inbound call is **tenant-plane** REST carrying a JWT, while the coordination beneath it is **platform-plane** gRPC to cluster (§3.17.5); the handler never sees the second. Two caveats this example exposes deliberately: `hold_seat` makes cluster calls *inside* the critical section — local in Profile 1, two bounded remote round trips in Profile 3, which the amended ADR-002 permits (§6) but which does not make the section free (keep it short; use §3.3 pattern C where correctness depends on it); and `release()` runs on the happy path only, so an early `?` leaks the lock until TTL — contract-legal but worth a `Drop`-based wrapper in real code.

**Gear side.** The gear carries `capabilities = [stateful, system, grpc, rest]`, and the phase order dictates what its services may capture: `get_grpc_services` (phase 6) and `healthcheck()` (phase 5) both run before `start` (phase 7), so neither may capture a backend — both capture `Arc<ProfileRegistry>`, created in `init` and *populated* in `start` (§3.17.1). `start` calls `ClusterWiring::from_config`, which registers each profile's three backends in the hub under `cluster:{profile}` and returns the bound set for the registry; the gear then `publish`es the bound set (an `ArcSwap` swap, after which RPCs start succeeding) and registers `LocalClusterClient` under `dyn ClusterClient` unconditionally — the local-wins check finds it in Profile 1, and in Profile 3 the gear is alone in its pod so nothing resolves against it locally. Hub registration is cluster's own work, not the framework's; the framework only hands the gear its hub in `init`. A stateless service is four steps — authenticate, resolve the profile, dispatch, map the error — and a lease-bearing service owns **no** server-side lease state: the token the client presents is the whole authority, so the handler is a conditional write (§3.19.1).

**What actually differs between the profiles** is the binary and the config, never §4.4's consumer source:

| | Profile 1 (monolith) | Profile 3 (own pod) |
|---|---|---|
| Consumer gear crate | `cluster-sdk` + a forwarding feature left off | *identical crate*, forwarding feature enabled by the binary |
| Consumer **binary** crate | links `cluster` + plugins + **`grpc-hub`** (mandatory, §3.17.1) | enables the forwarding feature; links none of them |
| Consumer config | `gears.cluster.config.profiles.…` — DSNs here | none; endpoint from k8s DNS by convention (§3.17.4) |
| Who owns the Postgres pool | this process | the cluster pod only |
| `resolve()` returns | a facade over the real `PostgresCache` | a facade over a `RemoteCacheBackend` |
| `try_lock` cost | one Postgres round trip | one gRPC hop, then Postgres (§6) |
| Reconciler leadership | renewal loop in this process | same renewal loop, one hop further — the lease is store-owned, so a cluster restart does not unseat the leader (§3.19.2, §3.20.4) |

## 5. Traceability

DESIGN realizes the requirements stated in [PRD.md](./PRD.md) §5 (Functional Requirements) and §6 (Non-Functional Requirements). The inverse mapping (FR/NFR → realizing DESIGN section + supporting ADR) is the source of truth at PRD §14 Traceability. This section captures the forward direction: which decisions in DESIGN annotate which ADRs.

**ADR coverage of DESIGN decisions** (each cluster ADR annotates one or more DESIGN sections with rationale):

- **ADR-001** — annotates §3.11 SDK Default Backends (cache-CAS-universal model), §3.2 Component Model (per-backend characteristics drive component shape), §4.1 Backend Feature Compatibility, §4.2 Recommended Deployment Combinations.
- **ADR-002** — annotates §2.2 Constraints (no-remote-in-critical-section), §3.3 lock contract (no I/O in `Drop`, explicit async release).
- **ADR-003** — annotates §2.1 watch-union-shape principle, §2.1 lightweight-notifications principle, §3.9 Watch Event Shape, §3.13 Shutdown Sequence.
- **ADR-004** — annotates §3.3 telemetry expectations across all three primitives.
- **ADR-005** — annotates §1.1 Architectural Vision (facade-plus-backend-trait), §2.1 facade-plus-backend-trait principle, §3.1 Domain Model (eight types), §3.2 Component Model.
- **ADR-006** — annotates §3.7 Lifecycle Pattern (Builder/Handle), §3.11 SDK Default Backends (omit-primitive auto-wrap as wiring-crate behavior), §3.13 lifecycle/shutdown sequences.
- **ADR-007** — annotates §3.6 Resolution Pattern, §3.10 Capability Validation.
- **ADR-009** — annotates §3.11 SDK Default Backends (constructor pair `new` + `new_allow_weak_consistency`), §4.1 per-backend safety classification.
- **ADR-010** — annotates §3.12 Polyfill (`scan_prefix` as a defaulted, dyn-safe additive extension enabling `PollingPrefixWatch`).
- **ADR-011** — annotates §1.4 Platform OoP Alignment, §3.6 Resolution Pattern (async resolve through `dyn ClusterClient`), §3.10 / §3.10.1 Capability Validation (descriptor-based, inline-vs-deferred), §3.16 Remote Backend Seam (boundary at the three backend traits, one client per process, profile carried per request, lazy binding), §3.17.7 consumer-side wiring, §3.18.4 profile discovery. Records the `*Api` / `*Backend` two-trait split as load-bearing (§3.20.2).
- **ADR-012** — annotates §2.2 Constraints (store-owned-lease principle), §3.17.6 Shutdown (close-subscriptions, do-not-revoke), §3.19 Store-Owned Leases (fenced records, any replica serves any operation, client-driven renewal, the retired Postgres beacon, the fence's exact guarantee), §3.20.4 lock/leader contracts (unary against a store-owned lease).

**DESIGN component IDs** (from §3.2): `cpt-cf-clst-component-sdk`, `cpt-cf-clst-component-wiring`, `cpt-cf-clst-component-plugins`.

**DESIGN sequence IDs** (from §3.13): `cpt-cf-clst-seq-per-primitive-resolution`, `cpt-cf-clst-seq-lifecycle-startup`, `cpt-cf-clst-seq-shutdown`.

**DESIGN principle IDs** (from §2.1): `cpt-cf-clst-principle-cas-universal`, `cpt-cf-clst-principle-per-primitive-routing`, `cpt-cf-clst-principle-facade-plus-backend-trait`, `cpt-cf-clst-principle-lightweight-notifications`, `cpt-cf-clst-principle-version-based-cas`, `cpt-cf-clst-principle-watch-union-shape`.

**DESIGN constraint IDs** (from §2.2): `cpt-cf-clst-constraint-no-serde`, `cpt-cf-clst-constraint-no-remote-in-critical-section`, `cpt-cf-clst-constraint-dyn-compat`.

## 6. Risks / Trade-offs

**[Risk: Abstraction leakage]** Different backends have fundamentally different consistency guarantees (Redis RedLock is "probably correct", Postgres advisory locks are strictly serializable, Hazelcast IMap is CP or AP depending on config). Trait documentation must be explicit about minimum guarantees, and plugins must document their actual guarantees.
- Mitigation: Define minimum guarantees in trait docs (e.g., "at most one leader at any point per `LeaderElectionFeatures::linearizable == true` plus advisory staleness bound"). Plugin authors document their `*Features` declarations honestly. Capability requirements at the resolver site enforce honest characteristic claims at startup.

**[Risk: SDK contract verifies API shape, not distributed correctness]** Smoke tests against minimal in-process stubs verify that consumer code compiles against the SDK, handles the happy path, and exercises the error variants stubs emit (`Lagged`, `Closed(Shutdown)`, `CasConflict`, `CapabilityNotMet`). They do NOT verify behavior under network partition, clock skew, split-brain, message reordering across subscribers, or backend-specific failure semantics (Redis AOF loss, Postgres `synchronous_commit` windows, NATS JetStream sequence gaps, K8s API-server throttling). These failure modes cannot be faithfully simulated in-process — stubs have one state map, one clock, and one FIFO event channel.
- Mitigation: Each plugin follow-up change ships feature-gated integration tests against the real backend using CI infrastructure (Postgres containers for Phase 3, kind/minikube for Phase 4 K8s, future Redis/NATS/etcd containers). These tests are the authoritative source of distributed-correctness verification for each backend.
- Operator-facing partition behavior is concretely bounded: the consumer-perceived dual-leadership window under partition is `TTL + observation_lag`. See §3.3 staleness bound for the worst-case formula with default config and the operator-tuning trade-off.
- Future work (out of initial scope): Jepsen-style correctness harness exercising partition, clock skew, and process-kill scenarios against each plugin.

**[Trade-off: Per-primitive routing config complexity]** Per-primitive backend routing in operator YAML adds configuration surface. Operators could create confusing combinations (e.g., three different backends for three primitives).
- Mitigation: Documented recommended combinations in §4.2. Capability validation surfaces incompatible combinations at startup with clear error messages naming the bound backend. SDK-default omit-primitive auto-wrap simplifies single-backend profiles to a 1-line YAML config.

**[Trade-off: SDK-only this change ships without runnable cluster]** Until the wiring crate (`cf-cluster`) and at least one production plugin (`cf-standalone-cluster-plugin`) ship, the cluster is not deployable beyond SDK consumption — consumers can compile against the SDK but cannot run.
- Mitigation: Showcase example crates demonstrate consumer usage and plugin author shape (builder/handle pattern). Smoke tests prove the SDK contract works. Follow-up plugin changes can begin in parallel against the stable SDK contract.

**[Trade-off: remoting adds a hop to every operation — the primary consequence of the deployable model]** Making cluster a separate process is above all a performance change, and the cost is neither uniform nor uniformly negative. For every backend other than standalone, cluster already pays a network round trip today, so remoting adds one hop rather than converting memory access into network access: against a real Postgres backend `cache.get` roughly doubles to ~0.6 ms p50, comfortably inside `cpt-cf-nfr-oop-latency`'s 5 ms localhost / 10 ms intra-cluster budget; the alarming ratio (~300×) appears only against the in-memory standalone backend, which is the dev/test configuration and keeps Profile 1. The cost is concentrated, not spread: leader renewal is one hop every ~10 s; watches get *better* (the gear holds one upstream subscription and fans out, replacing N consumer-side `LISTEN` connections); the real exposure is read-modify-write under contention in effectively one consumer (OAGW's rate limiter), whose in-process ~1.2 ms flow becomes ~2.5 ms remoted — roughly 2×, still inside budget.
- Position: **that cost is accepted rather than engineered away** (invariant I13). The compound-operation lever (`increment` as one atomic `INSERT … ON CONFLICT`) would collapse the hot flow to one hop and beat today's in-process implementation, but it extends a frozen contract on speculation and is deliberately deferred until measurement shows the cost hurts. Two consequences are load-bearing: the read-modify-write loop stays (so `CasConflict` payload fidelity is a live concern, §7 decision 17a), and the ADR-002 critical-section conflict must be settled by amending ADR-002 rather than dissolved by a compound op.
- The binding constraint is **pool fan-in, not gRPC**: five Postgres connections serving the whole fleet break before HTTP/2 does, so revised `pool_max_size` guidance sized against fleet traffic, a `cluster_backend_pool_saturation` gauge, and per-`ClientId` concurrency caps (§3.18.6) matter more than protocol overhead. What gets better in return: backend connections (~50 → 5 for a 10-replica gear), duplicate TTL reapers collapse to one per instance per replica (reducing load on the backend, often the real ceiling), credential distribution narrows to one pod, and one shape impossible today becomes available — a single cluster pod with the standalone backend gives multi-instance coordination with zero infrastructure. Escape hatches (compound ops, Profile 1 for hot consumers, a node-local DaemonSet, client-side read caching) exist in order of preference but none is taken now.

**[Trade-off: ADR-002's critical-section rule is amended for the remote profile]** `cpt-cf-clst-fr-lock-no-remote` / ADR-002 forbid remote calls inside a lock's critical section, on the premise that cluster's own operations are local. In Profile 3 that premise does not hold — UC-002's `get` and CAS *are* remote calls — so the rule as written forbids the design's own reference flow.
- Decision: cluster-primitive round trips inside a critical section are **permitted and bounded** (by `rpc_timeout`, failing closed so the holder learns quickly); the ban on *non-cluster* remote I/O — database writes, HTTP to other services, unbounded and dependent on a third party's availability — stands unchanged, and the workspace lint is re-scoped to distinguish the two. §3.3's pattern C (treat the lock as advisory, make the protected write conditional) remains the answer for correctness-critical work and becomes more important, since a remote critical section has a wider window in which a lease can lapse unnoticed. The lint change is not cosmetic: left as-is it either fires on every correct Profile 3 consumer or gets suppressed wholesale, and a suppressed lint protects nothing.

**[Risk: cluster is a credential proxy across a network trust boundary — the cost of the connection-count win]** In-process, cluster's primitives are unauthenticated because in-process means one trust domain; a network boundary changes that materially — `cache.get(key)` against a shared cache can read any gear's coordination state, and `lock(name)` can block any gear's critical section. Concentrating backend credentials in one pod (§3.18.2) means cluster will use any configured backend on behalf of any authenticated caller, and no profile-level check restores the in-process limit, because **a profile is not a trust boundary** — it selects a backend, two profiles routinely share one DSN and one table, so a per-profile caller allow-list says nothing about two gears sharing a profile or a caller reaching the same rows through a profile it *is* allowed to name.
- Mitigation: the boundary is the **network and the credential** — a `NetworkPolicy` restricting the coordination port to platform namespaces (§3.17.8) plus platform-plane authentication (§3.17.5), both load-bearing rather than defence in depth. `scoped()` is cooperative and client-side (§3.8), so it is not an isolation boundary over a network at all. Server-enforced namespacing derived from the authenticated caller identity is the one piece of new security work that matters — the only mechanism that partitions one gear's coordination state from another's regardless of profile — and it is what must exist before cluster serves gears across trust boundaries (§7, decision 9). A deployment binding a profile to infrastructure at a genuinely different trust level needs a deployment-specific control at that layer, not a cluster config key.

**[Trade-off: leadership liveness tracks the consumer, not the transport]** Leader renewal doubles as a liveness proxy for the consumer — a wedged consumer's renewals stop, its claim lapses, a healthy peer takes over — and the remote path keeps that property by making renewal **client-driven** against the store-owned lease (§3.19.1, invariant I8). Renewing inside the gear on a consumer's behalf would sever the proxy, keeping a wedged consumer elected and silently weakening the guarantee to "cluster is up". Consequences: the proxy holds identically in both profiles; the transport owes no keepalive (a subscription is an observation channel and nothing more); `max_missed_renewals` governs only renewal-jitter tolerance; and §3.3's staleness bound gains one client→cluster hop on the observation side while renewal adds no server-side term. The cost is renewal traffic proportional to held elections (~one round trip per `renewal_interval()` per holder), negligible against the cache path and buying a correctness property no server-side bookkeeping can reconstruct. None of this changes leader election's advisory nature — pattern C remains the answer for correctness-critical work.

**[Note: observability spans the transport without breaking ADR-004]** ADR-004 makes span/metric/log names part of cluster's contract with a cardinality rule; the transport adds a layer to instrument without renaming anything. Client-side instrumentation is delegated to the platform's `PolicyStack` (trace-context propagation, RED metrics), with cluster supplying a `cluster.transport = "grpc" | "embedded"` attribute, the `profile`, and the caller `PlatformIdentity` on server spans so "which gear is hammering the cache" becomes answerable. New metrics stay label-bounded to `profile`, `provider`, `primitive`, `method`, `code` — never keys or names (invariant I15). The cardinality trap to call out explicitly: `/admin/sessions` and per-session logs contain lock and election names, which belong in logs and traces, never as metric labels.

## 7. Open Questions

| Question | Owner | Target Resolution |
|----------|-------|-------------------|
| Backend authentication and credential wiring (decision 13) — still the platform OoP credential design's call, now narrowed to one deployment target (§3.18.2) | Platform OOP deployment design | Resolved as part of the broader OOP design |
| Whether ADR-003 (cache watch backpressure) broadens to cover all three watches, or a new ADR captures the generalization | Cluster gear owner | Resolved during ADR audit — recommendation: broaden ADR-003 with a "Generalization to all three watches" section |
| **(5, blocking) Internal-auth granularity and the missing server-side gRPC path** (§3.17.5, §6). Validation is not cached (`K8sTokenReviewAuthenticator` runs a live TokenReview per call — at 10k ops/s, two orders of magnitude over budget), and no server-side inbound gRPC interceptor exists in the tree. The inbound platform-plane path and the mTLS+SPIFFE pull-forward are **one work item** — an interceptor over an uncached TokenReview is still unusable, so there is no viable SA-token phase for the gRPC data plane | OoP runtime / ADR-0006 two-plane work | Per-connection validation (mTLS+SPIFFE); the interceptor must exist by the deployable phase or the coordination port ships unauthenticated behind a `NetworkPolicy`. ADR-0006's "cached" claim is a defect in the ADR |
| **(9) Server-enforced namespacing from caller identity** (§3.8, §3.17.5, §6). Cluster derives a mandatory scope prefix from the authenticated `PlatformIdentity` and rejects keys outside it — the isolation boundary, now that profiles are ruled out as one. A **contract change** for consumers writing unscoped keys today | Cluster gear owner | Needs its own ADR and a migration path; hardening phase, before cluster serves gears across trust boundaries |
| **(12) Profile 2 (Host + Workers) is undesigned** (§3.15, §3.16). Three gaps: transport (UDS for single-host), endpoint resolution (none exists today), and topology — process count is no longer a correctness question (any process serves any lease, §3.19) but *backend scope* still is: one process per host is safe only if every host's process shares one backend configuration | Platform + cluster owner | Design P2 explicitly; make the shared-backend requirement checkable at startup |
| **(14) `ProfileNotBound { profile }` interning vs. widening the frozen enum** (§3.18.1) | Cluster gear owner | Recommend interning (`Box::leak`, config-bounded) — keeps `ClusterError` frozen (invariant I3) |
| **(17a) `CasConflict` payload fidelity** (§3.20.7) — a cost question, not a capability one, live now that the CAS loop stays. `#[derive(ContractError)]` can express a `Vec<u8>` value; the question is base64 overhead on the CAS error path | Cluster gear owner | Spike and measure; the version-only fallback is contract-legal (`current` is SHOULD) |
| **(17b) Should `LockGuard` expose a fencing token for external resources?** (§3.19.1) Cluster's internal `fence` is monotonic only within `fence_retention`, enough for its own predicates and not for a third-party store | Cluster gear owner | Additive if wanted — a `LockGuard::fence()` backed by a source that promises global monotonicity (a Postgres sequence); needs its own ADR |
| **(21, non-blocking) Where does the endpoint come from when no DirectoryService is deployed?** (§3.17.4) Two coupled asks on two owners: lift the OoP bootstrap's eager directory connect, and add a DNS fallback resolver whose real prerequisite is a **port convention in the platform PRD**. Cluster has a working path either way (own derivation + the ADR-0004 static override), so this is raised on the platform's behalf | Platform (OoP runtime + PRD) | The port convention is the actual decision; the `NullEndpointResolver`/503 wiring behaviour is deliberate and stays |
| **(16b, non-blocking) Does `inventory`-based profile self-registration belong in `cluster-sdk`?** (§3.17.7) Adoption of an existing pattern rather than invention | SDK owners | Confirm placement with the SDK owners |

## Appendix — Invariants

Properties no implementation may break. A change that appears to require breaking one of these is a change to *this document*, not an implementation choice. These were carried in the build plan while the deployable work was in flight; that plan has been retired now the work has landed, so they live here with the design they constrain, and the remediation review judged the implementation against this table, citing these by number.

| # | Invariant | §ref |
|---|---|---|
| **I1** | **Profile transparency.** One consumer source file compiles and behaves identically in Profile 1 and Profile 3. No `cfg`, no mode flag, no profile-specific consumer code | §1.4, §3.16, §4.4 |
| **I2** | **`resolve()` becoming `async` is the only SDK signature change.** The four facades, the typed-profile resolver, `scoped()`, the watch-event unions and `auto_restart` keep their shapes | §3.6, §3.16 |
| **I3** | **`ClusterError` is frozen.** No new variants, no widened fields. `ProfileNotBound` carries an interned `&'static str` | §3.18.1, §3.20.8, decision 14 |
| **I4** | **The boundary is the three backend traits**, and exactly one `Arc<dyn ClusterClient>` is registered per process, local winning over remote. No consumer names a `Remote*Backend` | §3.16, ADR-011 |
| **I5** | **No consumer serves traffic against an unmet requirement.** Whether that arrives as `Err(CapabilityNotMet)` from `resolve()` or as an `Unhealthy` readiness verdict varies; the guarantee and the error text do not — the deferred verdict re-runs the *same* validator closure the inline path ran | §3.10.1 |
| **I6** | **Startup never blocks on cluster reachability.** `resolve()` awaits only the descriptor, bounded; the facade binds lazily; registration touches no network | §3.10.1, §3.16, §3.17.7, ADR-0005 |
| **I7** | **A lease is a fenced record in the backing store.** No process's death — holder's or broker's — ends another's lease. Any replica serves any lease operation | §3.19.1, §3.19.2, ADR-012 |
| **I8** | **Renewal is client-driven**, so renewal remains the consumer-liveness proxy. A failed renew is `LockExpired` / `Status(Lost)`, never a terminal close | §3.20.4, §6 |
| **I9** | **No cluster-side client configuration exists** — no mode flag, no endpoint key, no timeouts, no profile list. Every field such a block would carry is owned elsewhere | §3.16, §3.17.7 |
| **I10** | **The profile name is typed in exactly two places** — the marker and the `.profile()` call. A config list would be a third | §3.17.7, `cpt-cf-clst-fr-validation-typed-profile` |
| **I11** | **The plugin-facing `*Backend` traits stay stable.** Extensions are defaulted and dyn-safe (`probe()`, `migrate()`), never required methods | §3.17.3, §3.17.8, `cpt-cf-clst-nfr-plugin-stability` |
| **I12** | **The wire is additive-only within `cluster.v1`.** `proto.lock.toml` — not the `cluster-sdk` crate version — is the wire-compatibility contract for field _numbers_; the committed `.proto` diff is the contract for field _types_. A reviewer checking a wire change must read the `.proto`, not the lock | §3.20.9 |
| **I13** | **No compound cache operations.** The frozen cache contract is not extended; the ~2× hot-path cost is accepted | §6 |
| **I14** | **Profile 1's hot path is unchanged.** `LocalClusterClient` returns the real backend `Arc` — no wrapper interposed on the request path | §3.16, §3.18.1 |
| **I15** | **Metric labels stay bounded** to `profile`, `provider`, `primitive`, `method`, `code`. Lock names, election names and cache keys live in traces and logs only | §3.18.3, §6, ADR-004 |
