# Technical Design — Kubernetes Cluster Plugin

> **Status: draft for review.** No code exists yet. This document and
> [TESTING.md](./TESTING.md) are the design deliverable of
> [#4372](https://github.com/constructorfabric/gears-rust/issues/4372); the crate is
> implemented against them afterwards. The scope decision the issue left open —
> whether to ship a cache in this change — is **decided: yes**. v1 ships all three
> primitives natively: leader election and distributed lock
> over `coordination.k8s.io/v1.Lease`, and the cache over a purpose-built
> `ClusterCacheEntry` custom resource (§6, §13 D1). The questions this draft
> opened are recorded in §13 with their decisions.

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Role in the Cluster Architecture](#11-role-in-the-cluster-architecture)
  - [1.2 Primitive Coverage](#12-primitive-coverage)
  - [1.3 What This Plugin Deliberately Is Not](#13-what-this-plugin-deliberately-is-not)
- [2. Domain Model](#2-domain-model)
  - [2.1 Two Primitives on a Lease, One on a Custom Resource](#21-two-primitives-on-a-lease-one-on-a-custom-resource)
  - [2.2 Object Naming: Cluster Names Are Not RFC 1123 Names](#22-object-naming-cluster-names-are-not-rfc-1123-names)
  - [2.3 Labels and Annotations](#23-labels-and-annotations)
  - [2.4 The Election Lease](#24-the-election-lease)
  - [2.5 The Lock Lease](#25-the-lock-lease)
  - [2.6 The `ClusterCacheEntry` Custom Resource](#26-the-clustercacheentry-custom-resource)
  - [2.7 `resourceVersion` Guards; `spec.version` Is the Version](#27-resourceversion-guards-specversion-is-the-version)
  - [2.8 Expiry Is Judged on the Observer's Monotonic Clock](#28-expiry-is-judged-on-the-observers-monotonic-clock)
  - [2.9 Sub-second TTLs Against an `i32`-Seconds Field](#29-sub-second-ttls-against-an-i32-seconds-field)
- [3. Component Model](#3-component-model)
  - [3.1 Crate Structure](#31-crate-structure)
  - [3.2 Builder / Handle Lifecycle](#32-builder--handle-lifecycle)
  - [3.3 Client and Connection Model](#33-client-and-connection-model)
  - [3.4 Startup Preflight](#34-startup-preflight)
  - [3.5 Three Independently Routable Providers](#35-three-independently-routable-providers)
  - [3.6 Identity and Namespace Discovery](#36-identity-and-namespace-discovery)
  - [3.7 The Consistency Declaration Is Unconditional](#37-the-consistency-declaration-is-unconditional)
- [4. Leader Election Implementation](#4-leader-election-implementation)
  - [4.1 Acquire](#41-acquire)
  - [4.2 Renew](#42-renew)
  - [4.3 Status Is Watch-Driven, Not Poll-Driven](#43-status-is-watch-driven-not-poll-driven)
  - [4.4 Resign](#44-resign)
- [5. Distributed Lock Implementation](#5-distributed-lock-implementation)
  - [5.1 The Holder Token](#51-the-holder-token)
  - [5.2 `try_lock`](#52-try_lock)
  - [5.3 Blocking `lock()`](#53-blocking-lock)
  - [5.4 Renew and Release Are Token-Fenced](#54-renew-and-release-are-token-fenced)
  - [5.5 Release Clears; It Does Not Delete](#55-release-clears-it-does-not-delete)
  - [5.6 Inspecting Locks (operators)](#56-inspecting-locks-operators)
- [6. Cache Implementation](#6-cache-implementation)
  - [6.1 Operation Contract per Method](#61-operation-contract-per-method)
  - [6.2 TTL: Enforced on Read, Reclaimed by a Deadline-Armed Sweeper](#62-ttl-enforced-on-read-reclaimed-by-a-deadline-armed-sweeper)
  - [6.3 Watch](#63-watch)
  - [6.4 `scan_prefix`](#64-scan_prefix)
  - [6.5 The Consistency Declaration Follows the Read Mode](#65-the-consistency-declaration-follows-the-read-mode)
  - [6.6 Value Size](#66-value-size)
  - [6.7 The CRD Is an Operator Prerequisite, Verified at Startup](#67-the-crd-is-an-operator-prerequisite-verified-at-startup)
  - [6.8 What the Cache Is Not For](#68-what-the-cache-is-not-for)
- [7. RBAC](#7-rbac)
- [8. Configuration](#8-configuration)
- [9. Observability](#9-observability)
- [10. ProviderErrorKind Mapping](#10-providererrorkind-mapping)
- [11. Shutdown Sequence](#11-shutdown-sequence)
- [12. Risks / Trade-offs](#12-risks--trade-offs)
- [13. Decisions (formerly Open Questions)](#13-decisions-formerly-open-questions)
  - [D1 — Ship a cache in this change, and on what resource?](#d1--ship-a-cache-in-this-change-and-on-what-resource)
  - [D2 — Native lock, when the issue's DoD did not ask for one](#d2--native-lock-when-the-issues-dod-did-not-ask-for-one)
  - [D3 — Guarded `replace` vs. server-side apply](#d3--guarded-replace-vs-server-side-apply)
  - [D4 — Monotonic `Observed` expiry vs. comparing `renewTime` to local wall time](#d4--monotonic-observed-expiry-vs-comparing-renewtime-to-local-wall-time)
  - [D5 — Cross-namespace coordination](#d5--cross-namespace-coordination)
  - [D6 — Credential resolution](#d6--credential-resolution)
  - [D7 — A shared K8s-access abstraction for the workspace](#d7--a-shared-k8s-access-abstraction-for-the-workspace)
- [14. Migration: mini-chat and chat-engine](#14-migration-mini-chat-and-chat-engine)

<!-- /toc -->

## 1. Overview

`cf-k8s-cluster-plugin` is the Kubernetes backend plugin for the cluster gear. It
provides all three cluster primitives **natively**, with no SDK default in the
picture. Two of them sit on the same built-in resource —
`coordination.k8s.io/v1.Lease`:

- a `LeaderElectionBackend` over one Lease per election, declaring **linearizable**
  election semantics;
- a `DistributedLockBackend` over one Lease per lock name, token-fenced, declaring
  **linearizable** mutual exclusion.

The third sits on a purpose-built namespaced custom resource:

- a `ClusterCacheBackend` over one `ClusterCacheEntry` per key, with a per-key
  `spec.version` counter for compare-and-swap, `resourceVersion`-guarded atomicity,
  native exact **and prefix** watches, and TTL enforced on the read path with a
  deadline-armed sweeper for reclamation (§6).

Shipping the cache makes this the only backend that declares **both**
`CacheConsistency::Linearizable` and `CacheFeatures { prefix_watch: true }` with no
configuration caveat — Redis gets native prefix watch only on a non-clustered
deployment and cannot claim linearizability in any HA shape; Postgres claims
linearizability but has no native prefix watch. It also makes the "K8s,
low-throughput" deployment shape (`docs/DESIGN.md` §4.2) expressible with a single
provider and zero new infrastructure, which was its whole point.

The word doing the work in that shape's name is **low-throughput**, and §6.8 and
§12 are where that is stated as a limit rather than a caveat.

The plugin is the one backend ADR-009's safety table rates **safe for leader
election with no configuration caveat at all**: "K8s Lease API (etcd-backed) —
Yes; failure mode only on etcd quorum loss." Every other candidate needs a
qualifier (Postgres needs `synchronous_commit=on` plus a synchronous standby,
NATS needs R≥3, Redis needs a single durable node and no replicas). That is what
makes this plugin the intended `leader_election` and `lock` binding for a
Redis-cache profile, whose cache is `EventuallyConsistent` and whose SDK-default
leader election therefore refuses to construct.

The counterweight is throughput. ADR-001 puts a K8s API operation at 2–10 ms and
etcd's practical sustained write ceiling at ~3 000–5 000 writes/sec *shared with
the entire control plane*, and cites K8s issue #47532 — 100 000+ Leases
destabilising etcd — as evidence that "many small Lease objects" is a known
anti-pattern. Every design choice below that looks conservative (watch-driven
status instead of polling, one Lease per lock name kept rather than churned,
`Reset` instead of a re-list storm) is that ceiling being respected.

### 1.1 Role in the Cluster Architecture

The plugin satisfies `cpt-cf-clst-component-plugins` for the Kubernetes backend.
It:

- Implements all three provider traits — `ClusterCacheProvider`,
  `ClusterLeaderElectionProvider`, and `ClusterLockProvider` — each returning
  `provider() -> "k8s"`, so an operator can bind any subset of the three primitives
  to `k8s` from YAML.
  Per-primitive routing (`cpt-cf-clst-fr-routing-per-primitive`, implemented in
  `cluster/src/domain/wiring.rs` via `ClusterWiring::from_config`, over the
  `ProviderRegistry` in `cluster/src/domain/provider.rs`) dispatches each primitive
  against its own registry, so
  binding `cache: { provider: k8s }` alongside `lock: { provider: redis }` is a
  supported mix, not a special case.
- Because all three are native, **nothing here rides an SDK default.** A profile
  that binds `cache: { provider: k8s }` and omits the other two still gets the
  SDK defaults over this cache (the wiring's omit-default behaviour), and §12
  records why that is a worse arrangement than binding the native two explicitly.
- Exposes a builder/handle pair per shape
  (`K8sClusterPlugin::builder(...).build_and_start() -> K8sClusterHandle`, plus
  three per-primitive shapes) following the outbox-style lifecycle pattern
  (`docs/DESIGN.md` §3.7, ADR-006). It is NOT a `RunnableCapability`; the cluster
  gear (`cf-gears-cluster`) owns its lifecycle.
- Returns a `StopHook` from each `build_*` that cancels the background tasks it
  owns (renewal loops, watcher tasks, the cache TTL sweeper, the stale
  lock-object reaper) and drops its `kube::Client`.

Registration is three lines in the wiring's provider registry
(`cluster/src/gear.rs`):

```rust
ProviderRegistry::new()
    // … existing providers …
    .with_cache_provider(Arc::new(k8s_cluster_plugin::K8sCacheProvider))
    .with_leader_election_provider(Arc::new(k8s_cluster_plugin::K8sLeaderElectionProvider))
    .with_lock_provider(Arc::new(k8s_cluster_plugin::K8sLockProvider))
```

### 1.2 Primitive Coverage

| Primitive | Implementation | Consistency / features |
|---|---|---|
| `LeaderElectionBackend` | Native — one Lease per election, `holderIdentity` + `resourceVersion`-guarded acquire/renew, watch-driven status | `LeaderElectionFeatures { linearizable: true }` — unconditional (§3.7) |
| `DistributedLockBackend` | Native — one Lease per lock name, per-acquisition holder token, token-fenced renew/release, watch-driven wake for blocking `lock()` | `LockFeatures { linearizable: true }` |
| `ClusterCacheBackend` | Native — one `ClusterCacheEntry` CR per key, `spec.version` for CAS, `resourceVersion`-guarded writes, native exact + prefix watch, read-path TTL with a deadline-armed sweeper (§6) | `CacheConsistency::Linearizable` under the default `reads: quorum`; `EventuallyConsistent` under `reads: cached` (§6.5). `CacheFeatures { prefix_watch: true }` unless watches are disabled (§6.3) |

### 1.3 What This Plugin Deliberately Is Not

- **Not a general-purpose or high-throughput cache.** §6.8 states the envelope
  explicitly: every operation is an etcd operation on the shared control-plane
  budget, so this cache is for low-volume coordination state (shard assignments,
  configuration, rate-limit windows at modest rates), not for the OAGW's
  10 000 counter-updates/sec. A deployment past the envelope binds `cache` to
  Redis or Postgres and keeps the other two primitives here.
- **Not ConfigMap-based.** §13 D1 records the comparison. A purpose-built CR
  carries a real typed schema instead of `binaryData` plus annotations, and — the
  deciding reason — confines the watch blast radius to its own resource type
  instead of delivering every cache mutation to every ConfigMap informer in the
  cluster.
- **Not a fencing-token issuer.** ADR-002's "no remote I/O inside the critical
  section" rule removes the failure mode fencing tokens exist for. `leaseTransitions`
  is maintained for interop and observability, not handed to consumers as a fence.
- **Not interoperable with a `client-go` elector on the same Lease object.**
  Sharing one Lease between this plugin and `k8s.io/client-go/tools/leaderelection`
  would work only by accident: `client-go` judges expiry by comparing its own
  wall clock against `renewTime` (§2.8 explains why this plugin does not), and it
  does not read this plugin's `ttl-ms` annotation (§2.9). Object naming (§2.2)
  makes accidental sharing essentially impossible; §14 gives the deliberate
  escape hatch for the one case where sharing a *name* is wanted.
- **Not a controller, and not a CRD installer.** It registers no webhook, needs no
  cluster-scoped RBAC at runtime, and reconciles nothing. Everything it does at
  runtime is namespace-scoped CRUD plus watch on two resources. The
  `ClusterCacheEntry` CRD is an **operator prerequisite** the plugin verifies and
  never installs (§6.7) — installing it would demand cluster-scoped write access
  to `customresourcedefinitions`, which is the one RBAC escalation this design
  refuses.

## 2. Domain Model

### 2.1 Two Primitives on a Lease, One on a Custom Resource

Two resource types make up the whole storage layer: the built-in
`coordination.k8s.io/v1.Lease` for the two coordination primitives, and a
namespaced `ClusterCacheEntry` custom resource for the cache (§2.6).

The split is not arbitrary. Both coordination primitives *are* "a holder
identity with a renewable expiry", which is exactly what `Lease` models — so they
get a built-in resource, no CRD install, and semantics an operator already
recognises in `kubectl`. A cache entry is "opaque bytes with a version and an
expiry", which `Lease` cannot express at all (its only string field is
`holderIdentity`, and values are bytes). Forcing the cache onto `Lease` would mean
abusing a field; giving it its own resource costs an install step and buys a typed
schema plus an isolated watch stream (§13 D1).

`Lease`'s spec is, verbatim:

```
spec:
  holderIdentity:         string    — who holds it
  leaseDurationSeconds:   int32     — how long a holder's claim is good for
  acquireTime:            MicroTime — when the current holder took it
  renewTime:              MicroTime — when the current holder last renewed
  leaseTransitions:       int32     — how many times the holder has changed
```

Both primitives map onto those five fields with no field left doing double duty:

| | `holderIdentity` | `leaseDurationSeconds` | `acquireTime` | `renewTime` | `leaseTransitions` |
|---|---|---|---|---|---|
| Election | candidate identity (§3.6) | `ElectionConfig::ttl()` | when this leader acquired | last renewal | leader changes — surfaced as a metric |
| Lock | `<identity>#<acquisition uuid>` (§5.1) | `ceil(ttl)`, ≥1 (§2.9) | when this holder acquired | last `renew` | holder changes |

Choosing the built-in `Lease` over a CRD is what makes this plugin installable
with a `Role` and a `RoleBinding` and nothing else — no cluster-admin step, no
CRD version skew, no `kubectl apply -f crds/` in a runbook. It is also why the
same object type serves both: `Lease` *is* "a holder identity with a
renewable expiry", which is precisely what both primitives are.

### 2.2 Object Naming: Cluster Names Are Not RFC 1123 Names

This is the single largest impedance mismatch in the plugin, and getting it wrong
is a correctness bug rather than a cosmetic one.

Cluster coordination names are scoped (`cpt-cf-clst-fr-namespacing-scoped`), and
the SDK's `ScopedLeaderElectionBackend` / `ScopedDistributedLockBackend`
compose prefixes with `/` separators, so a name
arriving at this backend routinely looks like
`event-broker/shard-7/worker-pool`. Kubernetes object names must be RFC 1123
subdomains: lowercase alphanumerics, `-` and `.`, first and last characters
alphanumeric, ≤253 characters. `/` is not permitted; uppercase is not permitted.

A lossy transform is not an option. If `a/b` and `a-b` both mapped to `a-b`, two
unrelated consumers would silently share one lock. The mapping must therefore be
**injective**, and it is made so by carrying a hash of the *original* name:

```rust
/// `<prefix>-<seg>-<slug>-<hash>`
///   prefix — operator config, validated as an RFC 1123 label, ≤ 40 chars
///   seg    — "el" | "lk" | "ca"
///   slug   — lowercased original with every [^a-z0-9-] run collapsed to one '-',
///            trimmed of leading/trailing '-', truncated to fit; readability only
///   hash   — first 16 lowercase hex chars of SHA-256(original name)
fn lease_name(prefix: &str, seg: Seg, name: &str) -> String;
```

Three properties, in the order they matter:

- **Injective up to a 64-bit collision.** The slug is decorative; the hash carries
  the identity. 16 hex characters is 64 bits — a collision needs ~5 × 10⁹ distinct
  coordination names in one namespace before reaching a 10⁻⁹ probability. 16
  characters out of a 253-character budget is cheap insurance for a mapping whose
  failure mode is two consumers sharing a lock, which is why the hash is not
  trimmed to 8 or 10.
- **Stable across processes and restarts.** SHA-256 of the name string, nothing
  else — no host, no PID, no time. Two instances computing a name for the same
  election arrive at the same Lease, which is the entire point.
- **Legible in `kubectl`.** The slug means
  `event-broker/shard-7/worker-pool` shows up as
  `cluster-el-event-broker-shard-7-worker-pool-9f1c3ab4e7d02185`, which an
  operator can recognise without a decoder ring. When truncation bites, the slug
  is cut and the hash is not.

Name **validation** is separate from name mapping and stays with the SDK: this
plugin never returns `InvalidName` for a name it can hash, and it can hash
anything. What it *does* validate is its own `lease_prefix` (an RFC 1123 label,
so the composed name cannot be illegal at the front) and the resulting total
length (a `debug_assert` plus an `InvalidName` at the boundary, since a
253-character overrun means the slug-truncation arithmetic is wrong — a plugin
bug, not an operator one).

### 2.3 Labels and Annotations

Every object this plugin creates carries a fixed label set. The prefix is a
compile-time constant, and — like the observability catalog (ADR-004) — renaming
it is a **breaking wire change**, because operator dashboards, `kubectl`
one-liners, and the reaper's own list selector all key on it.

| Label | Value | Why a label (server-side selectable) |
|---|---|---|
| `cluster.cf-gears.io/managed-by` | `cf-gears-cluster` | One selector finds everything this plugin owns, in any namespace. The reaper's list scope, and the operator's "what is cluster doing in here" query |
| `cluster.cf-gears.io/primitive` | `election` \| `lock` \| `cache` | Per-primitive listing without parsing names. Also the cache's **watch and `scan_prefix` selector** (§6.3, §6.4) — every cache object carries it, so one label selector scopes a stream or a list to the cache keyspace |

| Annotation | Value |
|---|---|
| `cluster.cf-gears.io/name` | The original, unmapped coordination name — the inverse of §2.2's mapping, so an operator reading an object never has to reverse a hash. Carried by cache entries too, where it is also the **`scan_prefix` match key** (§6.4) |
| `cluster.cf-gears.io/ttl-ms` | Locks only: the exact requested TTL in milliseconds (§2.9) |

### 2.4 The Election Lease

```yaml
apiVersion: coordination.k8s.io/v1
kind: Lease
metadata:
  name: cluster-el-worker-pool-9f1c3ab4e7d02185
  namespace: gears
  labels:
    cluster.cf-gears.io/managed-by: cf-gears-cluster
    cluster.cf-gears.io/primitive: election
  annotations:
    cluster.cf-gears.io/name: "event-broker/worker-pool"
spec:
  holderIdentity: broker-7d9f8b-x4k2p     # the pod, per §3.6
  leaseDurationSeconds: 30                # ElectionConfig::ttl()
  acquireTime: "2026-08-06T09:14:02.114Z"
  renewTime: "2026-08-06T09:14:22.301Z"
  leaseTransitions: 4
```

One Lease per election name, shared by every candidate. A candidate that is not
the holder writes nothing — it watches (§4.3). This matters for the etcd budget:
N candidates on one election produce **one** writer, not N.

### 2.5 The Lock Lease

```yaml
metadata:
  name: cluster-lk-tenant-42-rate-limit-3b7e1d94c05af628
  labels:
    cluster.cf-gears.io/managed-by: cf-gears-cluster
    cluster.cf-gears.io/primitive: lock
  annotations:
    cluster.cf-gears.io/name: "oagw/tenant-42/rate-limit"
    cluster.cf-gears.io/ttl-ms: "750"
spec:
  holderIdentity: oagw-5f4c7d-9pm2q#0f8a1c6e-3b2d-4e5f-9a01-77c3d5e91b42
  leaseDurationSeconds: 1                 # ceil(750ms), floored at 1 — §2.9
  renewTime: "2026-08-06T09:14:22.301Z"
```

The `holderIdentity` is deliberately compound: the pod identity before the `#`
so an operator reading `kubectl get lease` learns *which replica* is holding
the lock, and a per-acquisition UUID after it so renew and release are fenced
against a successor (§5.1). A free lock is a Lease that exists with
`holderIdentity: null`, not an absent object (§5.5).

### 2.6 The `ClusterCacheEntry` Custom Resource

One namespaced custom resource per cache key. The CRD is installed by the operator
(§6.7) and shipped in the crate as `deploy/crd.yaml`:

```yaml
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: clustercacheentries.cluster.cf-gears.io
spec:
  group: cluster.cf-gears.io
  scope: Namespaced
  names:
    kind: ClusterCacheEntry
    plural: clustercacheentries
    singular: clustercacheentry
    shortNames: ["cce"]
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        required: [spec]
        properties:
          spec:
            type: object
            required: [value, version]
            properties:
              value:
                type: string
                format: byte          # base64 on the wire, opaque bytes to consumers
              version:
                type: integer
                format: int64
                minimum: 1            # 0 is the SDK's reserved absent-sentinel
              expiresAt:
                type: string
                format: date-time     # absent ⇒ Ttl::Indefinite
    additionalPrinterColumns:
    - { name: Version,  type: integer, jsonPath: .spec.version }
    - { name: Expires,  type: string,  jsonPath: .spec.expiresAt }
    - { name: Key,      type: string,  jsonPath: .metadata.annotations.cluster\.cf-gears\.io/name }
```

An entry in flight:

```yaml
apiVersion: cluster.cf-gears.io/v1
kind: ClusterCacheEntry
metadata:
  name: cluster-ca-shard-t-42-b71e04c9a3f8562d
  namespace: gears
  labels:
    cluster.cf-gears.io/managed-by: cf-gears-cluster
    cluster.cf-gears.io/primitive: cache
  annotations:
    cluster.cf-gears.io/name: "event-broker/shard/t-42"
spec:
  value: "eyJvd25lciI6ImJyb2tlci03In0="
  version: 7
  expiresAt: "2026-08-06T09:14:52.114Z"
```

Four properties of this shape, each of which is a decision:

- **`spec.version` is the cluster cache version, and we maintain it.** 1 on create,
  `+1` on every successful write, absent once the object is deleted. §2.7 explains
  why `resourceVersion` cannot fill this role, and why maintaining our own costs
  nothing.
- **The value lives in `spec`, not `status`.** Semantically a cache value is data
  rather than desired state, so `status` has a claim — but nothing reconciles this
  resource, and putting the value in `spec` keeps one write path instead of two.
  There is deliberately no `status` subresource at all (§2.7 records the
  `metadata.generation` option it forecloses and why that is fine).
- **`expiresAt` is an absolute timestamp, not a duration.** A duration would need a
  start instant to be meaningful, and the only server-stamped one available
  (`creationTimestamp`) has one-second granularity and is not refreshed on update.
  An absolute deadline is directly comparable and survives an update that does not
  change the TTL. §6.2 covers whose clock writes it and what that costs.
- **The schema is a wire contract.** Like the label prefix (§2.3), the group, kind,
  and the three spec fields are part of this plugin's external integration surface
  (`docs/PRD.md` §7.2). Changing them is breaking and would need a second CRD
  version plus a conversion strategy, neither of which v1 builds. v1 commits to
  these three fields.

`additionalPrinterColumns` is not decoration: it makes
`kubectl get cce -n gears` a usable cache inspector without a `-o jsonpath`
incantation, which is the same reasoning behind the compound lock holder token
(§2.5, §5.6).

### 2.7 `resourceVersion` Guards; `spec.version` Is the Version

Two different jobs, two different fields, and conflating them is the single
subtlest correctness trap in this plugin. ADR-001's own reasoning conflates them —
see the note at the end of this section.

**`resourceVersion` guards.** Every mutating call this plugin makes is a
**guarded** update:

```rust
let mut lease = api.get(&name).await?;          // carries metadata.resourceVersion
mutate(&mut lease);                             // set holder / renewTime / labels
api.replace(&name, &PostParams::default(), &lease).await  // 409 on stale rV
```

`Api::replace` is a `PUT` that carries the `resourceVersion` from the read; the
API server rejects it with `409 Conflict` if anything changed in between. That is
native compare-and-swap, arbitrated by a Raft quorum, which is the whole reason
ADR-009 rates this backend safe with no qualifier.

**Why not `patch`.** A JSON-merge or strategic-merge patch without a
`resourceVersion` precondition is *unconditional*: two candidates patching
`holderIdentity` concurrently both succeed, last writer wins, and both believe
they lead. That is the split-brain bug this design exists to avoid, and it is an
easy one to introduce during a later "let's reduce the read" optimisation — which
is why `K8S-LEAD-004` (TESTING.md §4.3) asserts the 409 path explicitly rather
than only asserting the happy outcome.

Server-side apply (`Patch::Apply` with a field manager) is likewise not used: it
resolves concurrent writes by *merging field ownership*, which is the correct
semantic for controllers converging on desired state and the wrong one for
mutual exclusion, where the second writer must be told it lost.

Guarded **delete** uses the same mechanism through
`DeleteParams { preconditions: Some(Preconditions { resource_version, uid }), .. }`,
so neither the lock reaper (§5.5) nor the cache TTL sweeper (§6.2) can delete an object
that was revived between its read and its delete.

`resourceVersion` is treated as an **opaque string** throughout and never parsed as
an integer. The API contract says it is opaque and comparable only for equality; the
fact that etcd currently renders it as a decimal integer is an implementation detail
this plugin does not build on.

**`spec.version` is the version.** The cluster cache contract wants a `u64` that is
*monotonic per key*, starts at 1, and resets to 1 when a key is deleted and
re-created. `resourceVersion` fails all three: it is a **cluster-global etcd
revision**, so it starts at whatever the cluster is at, and a re-created object gets
a fresh higher revision rather than 1. The conformance suite is explicit —
`SC-CACHE-002` asserts `version == 1` on first write and `SC-CACHE-009` asserts a
delete-and-recreate resets to 1 — so exposing `resourceVersion` would fail the suite
outright, before any argument about API conventions.

Maintaining our own counter in `spec.version` costs **nothing**: it is written by
the same guarded replace we were already issuing. And it satisfies the awkward part
of the contract for free — a deleted object takes its `spec.version` with it, so the
reset-to-1 behaviour that other backends work around
(`[cluster-cache-version-reset-caveat]`) is simply what happens here.

One consequence worth naming, because it falls out of the split rather than being
designed in: a **failed** CAS issues no write, so neither `spec.version` nor
`resourceVersion` moves. The conformance model asserts exactly that ("a
non-mutating op leaves the version unchanged — so a backend that bumps a version on
a failed CAS is caught"), and this design cannot violate it.

> **Note on ADR-001.** ADR-001's "Why version-based, not value-based" section lists
> `K8s resourceVersion` among the per-backend versions the cache contract "just
> exposes", and two bullets later uses `expected_resourceVersion` correctly as the
> CAS precondition. The second use is right and is what this section implements; the
> first is not, for the reasons above. This plugin is the first backend to reach that
> claim, and the same objection likely applies to `etcd`'s `mod_revision`, which has
> the same cluster-global-revision shape — not audited here. A correction note has
> been added to ADR-001 rather than a rewrite, so the original reasoning stays
> legible.

### 2.8 Expiry Is Judged on the Observer's Monotonic Clock

`renewTime` is written by the *holder* and read by *observers*. The naive expiry
test —

```rust
// WRONG, and this is what mini-chat/chat-engine do today
renew_time + lease_duration < Timestamp::now()
```

— compares the holder's wall clock against the observer's. Under clock skew this
is unsound in both directions: an observer whose clock runs fast steals a live
lease from a healthy leader (split-brain), and one whose clock runs slow refuses
to take over from a dead one (an outage that outlasts the TTL). Kubernetes
guarantees nothing about node clock synchronisation, and `Lease` carries no
server-side expiry to lean on.

`client-go`'s elector solves this with an *observed record*, and this plugin does
the same:

```rust
struct Observed {
    /// The (holderIdentity, renewTime) pair last seen for this Lease.
    record: (Option<String>, Option<Timestamp>),
    /// When *this process* first saw that pair — `tokio::time::Instant`,
    /// monotonic, unaffected by wall-clock jumps.
    seen_at: Instant,
}
```

A lease is considered expired **only** when the observer has held an unchanged
`(holderIdentity, renewTime)` pair for longer than `leaseDurationSeconds` on its
own monotonic clock. Every observed change resets `seen_at`. Consequences worth
stating:

- **Skew is irrelevant.** No cross-machine clock comparison happens anywhere on
  the acquisition path. Two nodes an hour apart behave identically.
- **A fresh observer waits a full TTL before it may steal.** A pod that starts
  while the holder is already dead cannot know how long the lease has been stale,
  so it waits out one whole `leaseDurationSeconds` from *its* first observation.
  This lengthens worst-case failover after a simultaneous restart, and is the
  correct trade: the alternative is trusting a foreign clock.
- **Wall-clock timestamps are still written**, because `kubectl describe lease`,
  `client-go` tooling, and the operator all read them. They are output, never
  input.

The same rule governs the lock (§5.2).
It is one function, `Observed::is_expired(now: Instant, duration: Duration)`,
and it is the most heavily unit-tested thing in the crate (TESTING.md §2).

### 2.9 Sub-second TTLs Against an `i32`-Seconds Field

`leaseDurationSeconds` is an `int32` in **seconds**. Cluster TTLs are
`Duration`s, and the OAGW's rate-limiting pattern (`cpt-cf-clst-actor-oagw`)
holds locks for "sub-second windows". A 750 ms `try_lock` has no representation
in the field.

Rejecting sub-second TTLs would make the primitive unusable for its most cited
consumer. Rounding *down* would expire a live lock. So:

- `leaseDurationSeconds = max(1, ceil(ttl))` — the interop-facing value, always a
  safe **over**-estimate, so nothing that reads only the standard field can ever
  think the lock is free while this plugin thinks it is held.
- `cluster.cf-gears.io/ttl-ms` carries the exact requested TTL, and **this** is
  what §2.8's expiry test uses. Millisecond precision is preserved for every
  reader that is this plugin.
- TTLs above `i32::MAX` seconds (~68 years) are rejected at the call with
  `InvalidConfig`. A `Duration` can express it; a Lease cannot.

The asymmetry is deliberate and one-directional: a foreign reader is *too
conservative*, never too aggressive. That is the only direction in which
disagreement is safe.

**The cache does not have this problem.** `ClusterCacheEntry.spec.expiresAt` is an
RFC 3339 `date-time` with fractional seconds, so a `Ttl::Of(50ms)` is represented
exactly — no rounding, no companion annotation. It is a benefit of having designed
the resource rather than borrowed one, and it is what makes `SC-CACHE-010`'s 50 ms
TTL expressible at all (§6.2). The cache's precision limit is elsewhere: whose
clock writes the deadline (§6.2), not how many digits the field holds.

The same applies to `ElectionConfig::ttl()`, with one extra check:
`ElectionConfig::new` accepts any non-zero `Duration`, so a 500 ms election TTL
is constructible and would round to a 1 s Lease duration while the derived
renewal interval (`ttl / (max_missed + 1)`) is 167 ms — three writes per second
per election against the API server. The plugin therefore **rejects** an election
TTL below `min_election_ttl` (config, default 5 s) with `InvalidConfig` naming
the derived renewal rate, rather than silently becoming the load generator that
ADR-001's etcd ceiling warns about. Locks have no such floor: a lock's writes are
per-acquisition, not per-interval.

## 3. Component Model

### 3.1 Crate Structure

```
cf-k8s-cluster-plugin/
  src/
    lib.rs          — public API re-exports
    config.rs       — K8sClusterConfig + the three per-primitive configs (serde)
    naming.rs       — §2.2 name mapping, slug/hash, label + annotation constants
    observed.rs     — §2.8 Observed record and the expiry predicate
    client.rs       — kube::Client construction, namespace/identity discovery (§3.6)
    preflight.rs    — SelfSubjectAccessReview RBAC probe, version check, and the
                      CRD presence/schema canary (§3.4, §6.7)
    k8s_error.rs    — kube::Error → ClusterError / ProviderErrorKind (§10)
    plugin.rs       — K8sClusterPlugin, builder, combined handle
    provider.rs     — the three Cluster*Provider impls ("k8s")
    guarded.rs      — the shared read/mutate/guarded-replace helper every primitive
                      uses, over both resource types; the single place a 409 is
                      classified (§2.7, §10)
    lease.rs        — Lease spec encode/decode: holder identity, the i32-seconds
                      TTL and its ttl-ms companion (§2.9)
    crd/
      mod.rs        — the ClusterCacheEntry type (kube::CustomResource derive)
      cache_entry.rs— spec struct, projection to/from CacheEntry
    leader/
      mod.rs        — K8sLeaderElection (LeaderElectionBackend impl)
      renew.rs      — per-election acquire/renew task driving LeaderWatchSender
      watch.rs      — kube::runtime::watcher task → status transitions, Reset
    lock/
      mod.rs        — K8sLock (DistributedLockBackend impl); standalone plugin shape
      waiters.rs    — in-process release-waiter registry fed by the lock watcher
      reaper.rs     — stale lock-object prune (§5.5)
    cache/
      mod.rs        — K8sCache (ClusterCacheBackend impl); standalone plugin shape
      watch.rs      — one shared cache watcher + per-key/per-prefix registry (§6.3)
      sweeper.rs    — deadline-armed TTL sweeper over the watch-derived index (§6.2)
      scan.rs       — paginated scan_prefix (§6.4)
  deploy/
    crd.yaml        — the ClusterCacheEntry CRD (§2.6); an operator prerequisite
                      this plugin verifies and never installs (§6.7)
  docs/
    DESIGN.md       — this document
    TESTING.md
```

No `migrations/`. The `Lease`-backed primitives provision nothing —
`coordination.k8s.io/v1.Lease` is built in and the first write creates its own
object. The cache has exactly one prerequisite, the CRD, which the operator applies
and `build_and_start` verifies (§6.7).

**Why `kube`, and what it costs.** The workspace already depends on
`kube = "3.0"` and `k8s-openapi = "0.27"` (`libs/toolkit-k8s-auth`,
`gears/mini-chat`, `gears/chat-engine`), and `docs/DESIGN.md` §3.5 already names
`kube` for this plugin, so no new client library is introduced. Two `kube`
features today's consumers do not enable *are* needed: `runtime` and `derive`.
Consumers use only `client`, and `kube::runtime::watcher` — which §4.3, §5.3, and
§6.3 all depend on — lives behind `runtime` (`kube-runtime` is absent from
`Cargo.lock` today); the `ClusterCacheEntry` `#[derive(CustomResource)]` (§2.7)
lives behind `derive` (`kube-derive`), which also pulls in `schemars` for the
generated `JsonSchema`. Feature set:
`kube = { workspace = true, features = ["client", "runtime", "derive", "rustls-tls", "aws-lc-rs"] }`,
matching the TLS backend the existing consumers pin, and
`k8s-openapi = { workspace = true, features = ["latest"] }`. The workspace `kube`
entry must keep `default-features = false`; otherwise the default `ring` crypto
provider is compiled alongside `aws-lc-rs` and `rustls` panics at startup with two
registered providers.

`kube` is used directly and needs no lint exemption: `libs/toolkit-k8s-auth` is a
`TokenReview` authenticator, not a general K8s-access abstraction, and no
workspace rule routes API access through it (§13 D7).

### 3.2 Builder / Handle Lifecycle

```rust
pub struct K8sClusterPlugin;

impl K8sClusterPlugin {
    pub fn builder(config: K8sClusterConfig) -> K8sClusterBuilder;
}

impl K8sClusterBuilder {
    /// Optional: hand in an existing client instead of building one (§3.3).
    pub fn with_client(self, client: kube::Client) -> Self;
    pub async fn build_and_start(self) -> Result<K8sClusterHandle, ClusterError>;
}

pub struct K8sClusterHandle {
    cache:  Arc<K8sCache>,
    leader: Arc<K8sLeaderElection>,
    lock:   Arc<K8sLock>,
    /* client, cancellation token, background JoinHandles */
    /// Set by `stop` so the `Drop` guard can tell a graceful shutdown from a
    /// forgotten one (ADR-006 §Confirmation).
    stopped: bool,
}

impl K8sClusterHandle {
    pub fn cache(&self)             -> Arc<dyn ClusterCacheBackend>;
    pub fn leader_election(&self)   -> Arc<dyn LeaderElectionBackend>;
    pub fn lock(&self)              -> Arc<dyn DistributedLockBackend>;
    pub async fn stop(mut self);
}
```

`Drop` carries the ADR-006 diagnostic guard in full — `stopped` short-circuit,
`std::thread::panicking()` check so a handle dropped mid-unwind degrades to a
WARN instead of a double-panic abort, debug-build `panic!` / release-build
`tracing::warn!` otherwise, and cancellation of the shared token *before* the
diagnostic so a dropped `stop()` future still unwinds the background tasks. The
cancel-*then*-diagnose shape follows the **postgres plugin's**
`shutdown::cancel_and_diagnose_drop` (`postgres-cluster-plugin/src/shutdown.rs`),
which is the version that actually cancels its token on the way out — added there
specifically to stop a background-task and `PgPool` leak. The wiring crate's own
`ClusterHandle::drop` (`cluster/src/domain/wiring.rs`) carries the diagnostic but
does **not** cancel a token, so it is the wrong thing to copy: this plugin owns
background tasks (watchers, the sweeper, the reaper) that a forgotten `stop()`
must still tear down.

`build_and_start`:

1. Builds or adopts the `kube::Client` (§3.3) and resolves namespace and identity
   (§3.6). A missing namespace or identity fails here with `InvalidConfig`, not at
   first use.
2. Runs the startup preflight (§3.4): server version, one
   `SelfSubjectAccessReview` per (verb, resource) pair the enabled primitives
   need, and — when the cache is enabled — the CRD canary (§6.7). A missing verb
   fails startup with an error naming the verb, the resource, the namespace, and
   the service account; a missing or incompatible CRD fails naming
   `deploy/crd.yaml`.
3. Spawns the per-primitive background infrastructure that is not per-call: the
   **cache watcher and its TTL sweeper** (§6.2, §6.3), and, if enabled, the
   stale lock-object reaper (§5.5). Election and lock tasks are spawned per
   `elect()` / per blocking `lock()`, not here.
4. Returns the handle. By the time it resolves, the client is authenticated, RBAC
   is verified, the CRD is confirmed, and the cache watcher has completed its
   initial list — so the sweeper's expiry index is populated and a `watch()`
   established immediately afterwards cannot miss an event that was already in
   flight. A failure at any step tears down whatever the earlier steps started.

`stop`: §11.

The three standalone shapes (`K8sCachePlugin`, `K8sLeaderElectionPlugin`,
`K8sLockPlugin`) have the identical
builder/handle/`Drop` shape, each exposing only its own primitive and preflighting
only its own verbs (§3.5). Only `K8sCachePlugin` runs the CRD canary and owns a
watcher and sweeper at startup; the other two spawn nothing until their first
call.

### 3.3 Client and Connection Model

| Resource | Purpose | Count |
|---|---|---|
| `kube::Client` | Every request: get, create, replace, delete, list, watch | 1 per plugin handle |
| Watch streams | One per active election, one per contended lock name, and **exactly one for the entire cache keyspace** (§6.3) | Created on demand and dropped with the watch, except the cache's, which lives for the handle's lifetime |
| Background tasks | One renewal task per held election; one watcher task per active watch; the cache watcher and its sweeper; one reaper | Proportional to *active claims*, not to configured names |

A `kube::Client` is an HTTPS connection pool with a resolved auth strategy, not a
per-connection resource like a `PgPool` — so unlike the Postgres plugin, there is
no pool sizing to get right and no cost argument against constructing more than
one. That is what makes §3.5's "each provider builds its own" the easy answer
here where it is a real trade-off elsewhere.

Two properties follow from the table and both are load-bearing:

- **A held lock consumes no connection and no task.** A held lock is a Lease with
  our token in it; renewal is consumer-driven (`LockGuard::renew`), not a
  background loop. Holding 10 000 locks costs 10 000 Lease objects and zero
  in-process resources. A *blocked* `lock()` costs one watch stream for as long as
  it blocks (§5.3).
- **A watched election costs one watch stream per candidate**, which is the
  design's main etcd-side expense and the reason §4.3 argues for it anyway.
- **The cache costs exactly one watch stream, no matter how many keys are watched.**
  N `watch(key)` and M `watch_prefix(p)` subscribers all fan out in process from one
  label-selected stream (§6.3). The flip side is that the stream carries *every*
  cache mutation in the namespace to *every* instance, which is the cache's binding
  scalability limit and is stated as such in §12.

`with_client` exists because a gear that already has a `kube::Client` (mini-chat
does, `chat-engine` does) should not be made to authenticate twice; the wiring
path does not use it, because a provider receives only a serde options map.

### 3.4 Startup Preflight

`build_and_start` issues a bounded, read-only set of requests before returning,
and turns every predictable runtime failure into a startup failure.

| Request | Decides | If refused / unreadable |
|---|---|---|
| `GET /version` | Server version, recorded on the startup log line | WARN; proceed. Nothing in v1 is version-gated |
| `create SelfSubjectAccessReview` × (verb, resource) | Whether this service account may do what the enabled primitives need (§7) | See below |
| **Cache only:** create-then-delete a canary `ClusterCacheEntry` | That the CRD is installed **and** its schema accepts what this plugin writes (§6.7) | Fail `InvalidConfig` naming `deploy/crd.yaml` |

**The RBAC probe is the point of this section.** Without it, a missing
`update` verb surfaces as a `403` on the first renewal — minutes after startup,
inside a background task, to an operator who is now debugging a leadership flap
instead of reading a boot error. `SelfSubjectAccessReview` turns that into:

```
InvalidConfig: kubernetes RBAC insufficient for cluster leader_election:
  service account system:serviceaccount:gears:event-broker may not `update`
  `leases` in namespace `gears`. Grant the Role in
  plugins/k8s-cluster-plugin/docs/DESIGN.md §7.
```

`create` on `selfsubjectaccessreviews` is granted to every authenticated
principal by the built-in `system:basic-user` ClusterRole, so the probe needs no
grant of its own in any normal cluster. Where it is nonetheless denied (a
hardened cluster, an admission webhook), the plugin logs
`cluster.provider.rbac_unverified` (WARN, once, naming the denial) and proceeds:
refusing to start because a *diagnostic* is unavailable would make the plugin
unusable somewhere it would otherwise work fine. `skip_rbac_preflight: true` is
the explicit config escape hatch for that environment.

The probe set is scoped to the enabled primitives — the standalone lock plugin
never asks about `delete`, which it does not use, and never asks about
`clustercacheentries` at all — so an operator granting the minimum for one
primitive is not told to grant verbs for the others.

The **canary** is the cache's counterpart to the RBAC probe, and it earns its two
writes: it proves in one shot that the CRD exists, that its served schema accepts
the exact spec shape this plugin version writes, and that the verbs actually work
(as opposed to `SelfSubjectAccessReview` merely saying they should). A schema drift
between a deployed CRD and a newer plugin — the version-skew hazard a
cluster-scoped singleton resource invites — otherwise surfaces as a `422` on the
first real `put`, at which point a consumer's write has already failed. The canary
key is **per-instance** — `<prefix>-ca-preflight-<identity-hash16>`, keyed on the
resolved identity (§3.6) rather than a fixed constant — so two instances starting
at once do not collide on one object: each writes and validates *its own* schema
view (a shared constant key would leave every instance but the create-winner
reading a `409 AlreadyExists` and trusting a schema it never exercised). It carries
a 60 s TTL and is deleted before `build_and_start` returns; on a crash between
create and delete the leftover is reclaimed by the next cache-enabled instance's
sweeper (its `expiresAt` is already in the past, §6.2) — so "leaves nothing
permanent" holds as long as *some* cache instance runs afterward, and only a
whole-fleet crash-before-delete could strand it until the next start.

### 3.5 Three Independently Routable Providers

```rust
impl ClusterCacheProvider for K8sCacheProvider {
    fn provider(&self) -> &'static str { "k8s" }
    async fn build_cache(&self, options: &Map<String, Value>)
        -> Result<(Arc<dyn ClusterCacheBackend>, StopHook), ClusterError>;
}
// … ClusterLeaderElectionProvider and ClusterLockProvider
//   likewise, all returning "k8s".
```

Each builds a **fully independent** plugin instance with its own client, its own
config, and its own `StopHook`. The two non-cache providers additionally never
receive a cache backend, per the SDK provider contract's "non-cache providers do
not receive the cache backend" — and here that is not merely honoured but
*meaningful*: both are native, so there is nothing for a cache to serve them
even if one were passed.

Nothing is shared between the three even when all three are bound to `k8s` in one
profile, because sharing would require a lifecycle-ownership answer to "which
provider's `stop()` closes the client?" that the independent-`StopHook` contract
has no way to express. The cost is up to three HTTPS connection pools against one
API server, which §3.3 explains is not a cost worth coupling three providers to
avoid.

Operator YAML — the single-provider shape, which is what shipping the cache makes
possible and is the `docs/DESIGN.md` §4.2 "K8s, low-throughput" row:

```yaml
cluster:
  profiles:
    default:
      cache:             { provider: k8s, namespace: gears }
      leader_election:   { provider: k8s, namespace: gears }
      lock:              { provider: k8s, namespace: gears }
```

All three bound explicitly, rather than binding `cache` and letting the other two
ride the SDK defaults over it. That is deliberate and is the **recommended** shape:
the native two are clock-skew-immune (§2.8) and cost one write per held claim,
while the SDK defaults over this cache would reimplement the same primitives out of
cache CAS — more etcd writes for weaker properties, and a TTL safety net that
inherits the cache's clock-skew sensitivity (§6.2, §12). The omit-default path still
*works*; it is simply worse here than on any other backend, because here the native
alternatives exist.

And the recommended production shape from `docs/DESIGN.md` §4.2, with Redis serving
the high-throughput primitives and K8s serving leader election, where consistency
matters more than throughput:

```yaml
cluster:
  profiles:
    event-broker:
      cache:
        provider: redis
        url: "rediss://:${REDIS_PASSWORD}@redis-primary:6379/0"
        topology: sentinel
      lock:
        provider: redis                     # Redis native lock, high throughput
      leader_election:
        provider: k8s
        namespace: gears                    # or omitted → in-cluster (§3.6)
```

Mixing at any granularity works, because the wiring dispatches each primitive
against its own registry: `cache: k8s` with `lock: redis` is as valid as the
reverse. The choice is a throughput-versus-infrastructure trade, and §6.8 gives the
numbers to make it with.

### 3.6 Identity and Namespace Discovery

Both values are resolved once, at `build_and_start`, in a fixed order, and the
resolved values appear on the startup log line so an operator never has to guess
which source won.

**Namespace** — `config.namespace`, else `POD_NAMESPACE` env, else
`/var/run/secrets/kubernetes.io/serviceaccount/namespace`, else the current
kubeconfig context's namespace, else fail `InvalidConfig`. There is no `default`
fallback: silently coordinating in `default` because the downward API was not
wired is a cross-tenant collision waiting to happen.

**Identity** — `config.identity`, else `POD_NAME` env, else
`hostname`. Unlike namespace, identity has a safe fallback: a StatefulSet or
Deployment pod's hostname *is* its pod name, and outside K8s the hostname is as
good an instance discriminator as anything. What identity must be is **stable for
the process's lifetime and distinct across replicas**, which all three sources
satisfy.

Identity is used raw as `holderIdentity` (§2.4) — it is a Lease *spec* value, not
an object name, so RFC 1123 does not apply and no mapping is needed. It is
truncated at 512 characters with a WARN if some exotic source produces something
longer.

### 3.7 The Consistency Declaration Is Unconditional

```rust
impl LeaderElectionBackend for K8sLeaderElection {
    fn features(&self) -> LeaderElectionFeatures { LeaderElectionFeatures::new(true) }
}
impl DistributedLockBackend for K8sLock {
    fn features(&self) -> LockFeatures { LockFeatures::new(true) }
}
```

`true` for both, with no detection step, no operator hint, and no topology table.
The reason is that there is nothing to compute:
ADR-009 rates the K8s Lease API safe with the single failure mode "etcd quorum
loss", and every K8s cluster's API server is quorum-backed by construction. There
is no `appendfsync` equivalent to read, no `synchronous_commit` to enforce, no
replication mode to detect. An operator cannot configure this API server into
non-linearizability without breaking their own control plane first.

The **cache** is the one exception, and its declaration is computed — from one
operator config field rather than from a probe. A linearizable read of a custom
resource means a quorum read, and the API server will happily serve a cheaper
possibly-stale read from its watch cache instead if asked. Which one the plugin
issues is `cache.reads`, and `consistency()` follows it honestly:
`Linearizable` under the default `quorum`, `EventuallyConsistent` under `cached`
(§6.5). That is the only place in this plugin where an operator can move a
declaration, and moving it downgrades rather than upgrades — the safe direction.

Two consequences:

- **Under etcd quorum loss the primitive fails rather than degrades.** Writes
  return `503`/`500`, which §10 maps to `Provider { ResourceExhausted }` /
  `ConnectionLost` — retryable, surfaced, and *not* silently succeeding. The
  leader's renewals fail, its budget exhausts, and it observes `Status(Lost)`. A
  quorum-loss window produces *no* leader, never two.
- **The declaration is honest about the object, not the deployment.** It says
  "a guarded write to this Lease is linearised", which is true. It does not say
  "your election has zero-downtime failover" — §2.8's fresh-observer wait and
  §12's APF risk both bound that, and neither is a consistency claim.

## 4. Leader Election Implementation

`elect(name)` / `elect_with_config(name, config)` return a `LeaderWatch` and spawn
one task per call. That task owns the Lease, the `LeaderWatchSender`, the
`ResignReceiver`, and an `Observed` record (§2.8), and drives everything through
`LeaderWatchSender::send_status` so the cached snapshot and the event stream
cannot diverge — the SDK's stated renewal contract.

### 4.1 Acquire

```
loop {
    ensure the Lease exists                      (create; 409 AlreadyExists → fine)
    get the Lease                                → refresh Observed
    if holder is us, or holder is empty, or Observed says expired (§2.8):
        guarded replace: holderIdentity = us, renewTime = now,
                         leaseDurationSeconds = ttl,
                         acquireTime = now  (only if we were not already holder),
                         leaseTransitions += 1 (only if the holder changed)
        → Ok       ⇒ we lead; send_status(Leader); go to §4.2
        → 409      ⇒ someone beat us; fall through to watching
    send_status(Follower) (once, on entry to follower state)
    await the next relevant change (§4.3) or the expiry deadline, whichever first
}
```

Three details that are not obvious from the shape:

- **`ensure exists` is create-if-absent, and a 409 is success.** N candidates
  starting simultaneously all attempt the create; one wins, the rest read a 409
  and proceed to the guarded-replace race, which is where arbitration actually
  happens. No pre-created Lease object is required in the Helm chart (mini-chat's
  chart ships one today; this plugin does not need it, and §14 notes it can be
  dropped).
- **The wake is watch-driven with a deadline, not a poll.** A follower does not
  re-`get` on an interval; it holds a watch (§4.3) and additionally arms a timer
  for its own `Observed` expiry deadline, because "the holder stopped renewing"
  produces *no* watch event — the absence of a change is the signal, and only a
  timer can observe an absence.
- **Contention is jittered.** After a 409, the retry waits
  `equal_jitter(base)` with `base` doubling to a `max_acquire_backoff` (config,
  default 5 s). Without jitter, N candidates that all lost one race retry in
  lockstep forever, converting an election into a synchronised write storm against
  the API server — the exact APF-throttling risk §12 records. The existing
  `K8sLeaseElector` gets this right and its equal-jitter approach carries over.

### 4.2 Renew

A renewal every `ElectionConfig::renewal_interval()` (`ttl / (max_missed + 1)`;
10 s under the default 30 s TTL) does a guarded replace setting only `renewTime`.

- **Success** — reset the failure counter, refresh `Observed`.
- **Retryable failure** (`ConnectionLost`, `Timeout`, `ResourceExhausted`) —
  increment the counter and retry on the next tick. Per the SDK's renewal
  contract these are handled **internally** and never surface as transitions.
  Only after `max_missed_renewals` consecutive failures does the task
  `send_status(Lost)` and fall back into §4.1's loop — which re-enrolls with no
  consumer code, satisfying `cpt-cf-clst-fr-leader-observability`'s
  "loss is transient" requirement.
- **`409 Conflict`** — someone else wrote the Lease. This is *not* retryable and
  *not* counted against the budget: it means the claim is gone now. Re-read; if
  the holder is no longer us, `send_status(Lost)` immediately and re-enroll. A
  409 treated as a transient error would keep a displaced leader believing it
  leads for `max_missed_renewals` intervals, which is the longest-lived
  split-brain window this design could have and does not.
- **A renewal attempted after the local `Observed` deadline already passed** —
  treated as loss without issuing the write at all. If we cannot prove we were
  still inside our own lease, we must not act as though we were.

Every renewal is wrapped in a `request_timeout` (config, default 10 s) shorter
than the renewal interval, so a hung API server produces a *failed* renewal on
schedule rather than a renewal task that stops ticking. This is the bound §11's
`stop()` budget rests on.

### 4.3 Status Is Watch-Driven, Not Poll-Driven

Each active election runs a `kube::runtime::watcher` over the single Lease
(a field-selector on `metadata.name`, so the stream carries only this object).
The alternative — every candidate polling `get` on the renewal interval — costs
N reads per interval per election against an API server ADR-001 already flags as
the throughput-constrained backend; a watch costs one long-lived connection and
delivers changes in ~milliseconds instead of up to one interval late.

Mapping the watcher's stream to `LeaderWatchEvent`:

| Watcher signal | Plugin action |
|---|---|
| `Apply(lease)` where holder became us | `send_status(Leader)` (deduplicated against current status) |
| `Apply(lease)` where holder became someone else | `send_status(Follower)`; if it *was* us, `send_status(Lost)` first |
| `Apply(lease)` with only `renewTime` advanced | Refresh `Observed`; **no event** — a healthy holder renewing is not a transition |
| `Delete(lease)` | Treated as "free": re-enter §4.1 immediately. Someone deleted the object out from under us; the create path handles it |
| `Init` / `InitApply` / `InitDone` (a re-list, i.e. the watch was re-established — typically a `410 Gone` on an expired `resourceVersion`) | Emit `Reset`, re-read state from the `InitApply` payload, re-evaluate status. `ClusterMetrics::watch_reset("leader")` |
| Stream error, retryable | `kube`'s watcher backs off and re-lists internally; the consumer sees the resulting `Reset` and nothing else |
| Stream error, non-retryable (`401`/`403`) | `Closed(Provider { AuthFailure })` — terminal, per `docs/DESIGN.md` §3.9's retryability table |

**`Lagged` is never emitted by this plugin's leader watch.** The K8s watch
protocol has no "you fell behind by N" signal: a watcher that cannot keep up gets
a `410 Gone` and must re-list, which is semantically `Reset`, not
`Lagged { dropped }`. Fabricating a count would be worse than not emitting one.
`Lagged` remains reachable only through the SDK's own bounded watch channel when
a *consumer* stops draining, which the SDK handles uniformly for every backend.

### 4.4 Resign

`LeaderWatch::resign()` arrives on the `ResignReceiver`. The task does a guarded
replace clearing `holderIdentity` (leaving `leaseTransitions` alone — a resignation
is not a transition until a successor takes it), responds `Ok(())` through the
`ResignResponder`, emits `Status(Lost)`, and stops.

This is the **one** place the plugin makes a remote release call, and it is not a
contradiction of `cpt-cf-clst-fr-shutdown-ttl-cleanup`: that requirement forbids
*best-effort cleanup during shutdown*, whereas `resign` is an explicit, awaited
consumer request whose whole purpose is
`cpt-cf-clst-fr-leader-resign`'s "releasing the claim immediately so a successor
can be elected within a backend round-trip rather than waiting for TTL expiry".
The distinction is that `resign` reports its failure to the caller; shutdown
cleanup would have nobody to report to.

A 409 on the resign replace is reported as `Ok(())`: someone else already owns the
Lease, so the claim we were asked to release is already gone. The postcondition
the caller asked for holds.

## 5. Distributed Lock Implementation

### 5.1 The Holder Token

`holderIdentity = "<identity>#<uuid-v4>"`, one fresh UUID per acquisition (§2.5).

Both halves earn their place. The identity prefix means
`kubectl get lease -l cluster.cf-gears.io/primitive=lock` answers "which replica
is holding this?" without a lookup table — the question an operator actually has
during an incident. The UUID suffix means two acquisitions are never confusable,
which is what makes renew and release safe against a successor (§5.4) *and* makes
two concurrent acquisitions **within one process** arbitrate through the API
server exactly as two processes would. There is no in-process fast path; a
regression that added one would show up as `K8S-LOCK-007` failing.

Only the plugin parses the `#`; nothing else depends on the shape. An identity
containing a literal `#` is not a problem, because parsing splits on the **last**
`#` (the UUID cannot contain one).

### 5.2 `try_lock`

```
ensure the Lease exists                    (create with holderIdentity = us, ttl)
    → created            ⇒ acquired
    → 409 AlreadyExists  ⇒ get it, refresh Observed
        held and not expired (§2.8, using ttl-ms) ⇒ Err(LockContended)
        free or expired                            ⇒ guarded replace with our token
            → Ok   ⇒ acquired
            → 409  ⇒ Err(LockContended)     (someone beat us; contention, not error)
```

Acquisition costs one request in the common uncontended case (the create, or the
get when the object already exists and is free) and never more than three. A 409
is reported as `LockContended`, never as a `Provider` error: it means the lock is
held, which is a documented outcome the caller branches on, not a backend
malfunction.

On success the backend creates the `LockGuard` via `LockGuard::channel`, keeps the
`LockCommandReceiver`, and services `LockRequest::Renew` / `Release` from it. A
guard dropped without release yields `None` from `recv`; the plugin does nothing
and the claim lapses on its TTL, per the SDK's guard-channel contract.

### 5.3 Blocking `lock()`

```
deadline = now + timeout
loop {
    try_lock-style attempt
        acquired          ⇒ return the guard
        LockContended     ⇒ …
    if now >= deadline                    ⇒ Err(LockTimeout { waited })
    if the shutdown token is cancelled    ⇒ Err(ClusterError::Shutdown)
    await, whichever comes first:
        - a watch event on this Lease showing the holder cleared or changed
        - the current holder's Observed expiry deadline
        - the caller's remaining budget
}
```

The watch is established **before** the first attempt and held for the duration
of the wait, not created after the first `LockContended`. A release landing
between "we saw it held" and "we subscribed" would otherwise be missed, and the
resulting bug is invisible except as a `lock()` that waits out the holder's full
TTL instead of waking promptly — which reads as slowness, not as a lost
notification. `K8S-LOCK-003` measures exactly this, and asserts a wake well inside
the TTL rather than merely "eventually".

Three distinct failure returns, and the distinction is the point:

- **`LockTimeout`** — the budget elapsed with the lock genuinely held by someone.
  The caller may retry or shed load.
- **`Shutdown`** — the plugin is going down (§11). The caller must not retry.
- **`Provider { .. }`** — the API server could not be reached. Classified by §10
  and retryable per its kind.

Collapsing any of these into another leaves a caller unable to tell "someone else
holds it" from "the backend is gone", which is why the classification is a pure
function of `(elapsed, cancellation state, last error)` and is unit-tested as one
(TESTING.md §2).

### 5.4 Renew and Release Are Token-Fenced

Both are guarded replaces that first check `holderIdentity == our full token`:

| | Holder is our token | Holder is someone else, or empty, or our `Observed` deadline passed |
|---|---|---|
| `renew(new_ttl)` | Guarded replace: `renewTime = now`, `ttl-ms` and `leaseDurationSeconds` updated for the new TTL → `Ok(())` | `Err(LockExpired)` — the SDK's "extending an already-expired lock **MUST** return a specific error so the consumer knows it lost the lock and needs to abort" |
| `release()` | Guarded replace clearing `holderIdentity` → `Ok(())` | `Ok(())`, **no write issued** — a foreign holder's claim is left untouched |

A 409 on either is re-read once and re-classified, since the concurrent write may
well have been a successor taking over after our lapse.

The release row is the release-if-still-holder contract
(`cpt-cf-clst-algo-distributed-lock-release-if-holder`), and it is the single
assertion that distinguishes this implementation from the classic bug: an
unconditional `DELETE` on release would let a holder whose TTL lapsed delete its
*successor's* lock. `K8S-LOCK-006` is that test.

### 5.5 Release Clears; It Does Not Delete

A released lock is a Lease with `holderIdentity: null`, not an absent object.

Deleting on release is the obvious alternative and was rejected on two grounds.
The first is etcd cost: a hot lock name acquired and released 10 times a second
becomes 20 writes/sec of create-and-delete churn plus a tombstone per delete,
against a backend whose sustained ceiling ADR-001 puts at 3 000–5 000 writes/sec
*for the whole cluster*. Clearing halves the write count and produces no
tombstones. The second is that a stable object is a stable `Observed` subject:
the watch established by a blocked `lock()` (§5.3) stays valid across
acquisitions, where a delete-and-recreate cycle would deliver `Delete` then
`Apply` and force the waiter to reason about object identity.

The cost is real and accepted: **one empty Lease object accumulates per lock name
ever used, forever.** For a bounded name space (per-tenant, per-shard) that is a
handful of objects. For an *unbounded* one — a lock per request id — it is an etcd
leak of exactly the shape K8s issue #47532 describes. Three things bound it:

- The stale lock-object reaper (`reaper`, default enabled; `reaper_interval`,
  default 5 min). Every interval it does one paginated list of
  `managed-by=cf-gears-cluster` Leases in the namespace and a **guarded delete**
  (on `resourceVersion` *and* `uid`) of each `primitive=lock` Lease with an empty
  holder and a `renewTime` older than `lock_object_retention` (config, default
  24 h). Two properties keep it from becoming its own problem: it is **safe to run
  from every replica** — two reapers racing means one gets a 409 and moves on, so
  no leader election is needed — and the `uid` precondition means a reap can never
  land on a *different* lock object that reused the name after a delete-and-recreate.
- `cluster_k8s_lock_objects` (gauge, §9) makes the count visible, and
  `cluster.lock.name_cardinality_high` (WARN) fires past a configurable
  threshold — the same signal the Postgres plugin emits for the same reason.
- The documented guidance is unambiguous: **cluster lock names must be bounded.**
  A lock per request id is a misuse on every backend and an etcd incident on this
  one.

### 5.6 Inspecting Locks (operators)

```bash
# What is held right now, by whom, and for how long
kubectl get lease -n gears -l cluster.cf-gears.io/primitive=lock \
  -o custom-columns=\
'NAME:.metadata.annotations.cluster\.cf-gears\.io/name,'\
'HOLDER:.spec.holderIdentity,'\
'RENEWED:.spec.renewTime,'\
'TTLMS:.metadata.annotations.cluster\.cf-gears\.io/ttl-ms'

# Everything this plugin owns in a namespace
kubectl get lease -n gears -l cluster.cf-gears.io/managed-by=cf-gears-cluster
```

The holder column reads `oagw-5f4c7d-9pm2q#0f8a…`, so the pod is identifiable
directly from the output — that is what §5.1's compound token buys, and it is why
`cluster.lock.acquired` (DEBUG, §9) logs the same token: a token seen in
`kubectl` can be grepped back to the acquiring task's log line.

## 6. Cache Implementation

`K8sCache` implements `ClusterCacheBackend` over the `ClusterCacheEntry` custom
resource (§2.6), one object per key. `K` below is the mapped object name
`<prefix>-ca-<slug>-<hash16>` (§2.2); `rV` is `metadata.resourceVersion`.

The shape of the whole section follows one principle: **expiry is enforced on the
read path, and the sweeper exists for reclamation and notification, not for
correctness.** A sweeper that is late, throttled, or stopped entirely cannot cause
a stale read.

### 6.1 Operation Contract per Method

| Operation | Kubernetes | Round trips (uncontended) |
|---|---|---|
| `get(key)` | `GET K`; `404` → `None`; `spec.expiresAt` in the past → `None` (§6.2) | 1 |
| `contains(key)` | Same as `get`, discarding the value. There is no cheaper existence probe — no `HEAD` on a named object, and a metadata-only `GET` still costs a quorum read | 1 |
| `put(req)` | `CREATE K` with `version: 1` → on `409 AlreadyExists`, `GET` then guarded `PUT` with `version: prev + 1` | 1 on create, 2 on overwrite |
| `put_if_absent(req)` | `CREATE K` with `version: 1` → `Some(entry)`; `409 AlreadyExists` → `Ok(None)`, **no read** | 1 |
| `compare_and_swap(key, expected, value, ttl)` | `GET K`; `spec.version != expected` → `CasConflict { key, current: Some(entry) }` from that same read, **no write**; equal → guarded `PUT` with `version: expected + 1` | 2 |
| `compare_and_delete(key, expected_value)` | `GET K`; value mismatch or absent → `Ok(false)`, **no write**; match → `DELETE K` with `Preconditions { rV, uid }` | 2 |
| `delete(key)` | `DELETE K`; `404` → `Ok(false)`, else `Ok(true)` | 1 |
| `watch(key)` / `watch_prefix(prefix)` | Registration against the single shared cache watcher — **no request at all** (§6.3) | 0 |
| `scan_prefix(prefix)` | Paginated `LIST` with the cache label selector, prefix-matched client-side (§6.4) | ⌈n/500⌉ |

Five things in that table are worth drawing out, because each is a decision rather
than a transcription:

- **`put_if_absent` is one round trip and genuinely atomic.** `CREATE` either
  succeeds or returns `409 AlreadyExists`; there is no read-then-create window. It
  is also the only method that needs no `GET` on the contended path, since the
  contract only asks *whether* it existed, not what it held.
- **A conflicted CAS returns the live entry for free — on the read path.** When the
  `GET` finds `spec.version != expected`, it already carries the current value and
  version, so `CasConflict { current: Some(entry) }` is populated from data in hand —
  no second request (the 2 round trips in the table). There is a second, narrower
  conflict: `expected` *matched* at the `GET`, but the guarded `PUT` then lost a race
  to a concurrent writer and returned `409 Conflict`. That path **re-reads once** and
  returns `CasConflict { current: Some(fresh entry) }` from the re-read — a 3rd round
  trip, paid only under genuine write contention. Returning `current: None` there
  would type-check but violate `SC-CACHE-006`'s "the conflict carries the live
  version", and retrying the CAS internally would let a momentarily-stale value still
  win, so neither is done. `SC-CACHE-006` asserts the live version is present on both
  paths, and `cluster_k8s_conflicts_total{primitive="cache"}` counts both.
- **A conflicted CAS issues no write**, so neither `spec.version` nor `rV` moves.
  The conformance model checks exactly this ("a backend that bumps a version on a
  failed CAS is caught"), and this design cannot violate it because the write is
  never attempted.
- **`compare_and_delete` is atomic**, via a `resourceVersion` + `uid` precondition
  on the `DELETE`. The SDK's default implementation of this method is an explicitly
  best-effort `get`-then-`delete` that "narrows but does not close the read-to-delete
  window"; the trait doc asks backends with an atomic store to override it, and this
  is that override. The `uid` half matters as much as the `rV` half: it stops the
  delete from landing on a *different object that reused the name* after a
  delete-and-recreate.
- **`put` is the only method that can lose a race**, because it carries no expected
  version and must nonetheless overwrite. On `409 Conflict` from the guarded `PUT`
  it re-reads and retries with jittered backoff up to `put_max_retries` (default 3);
  exhaustion returns `Provider { ResourceExhausted }`, which is retryable, so a
  caller's own retry loop composes. Unbounded internal retry was rejected: a hot key
  under heavy contention would turn one `put` into an unbounded write storm against
  the API server, which is the failure mode §12 is most concerned with.

A `put` with a byte-identical value still bumps `spec.version`, because we always
write `version: prev + 1`. That matters: the API server treats an identical-spec
`PUT` as a no-op and leaves `rV` unchanged, so a design that leaned on `rV` or on
`metadata.generation` for the version would silently violate the contract's
"a mutating op strictly increases the version" invariant. Ours differs on every
write by construction.

### 6.2 TTL: Enforced on Read, Reclaimed by a Deadline-Armed Sweeper

Kubernetes has no object TTL for arbitrary resources — `ttlSecondsAfterFinished` is
Job-specific and the TTL-after-finished controller does not generalise. So TTL is
this plugin's to implement, in two independent halves.

**The read path is authoritative.** `get`, `contains`, and `scan_prefix` treat an
entry whose `spec.expiresAt` has passed as **absent**. Correctness therefore does
not depend on any sweep having run: an entry is expired the instant its deadline
passes, from every reader's point of view, whether or not the object still exists.

**The sweeper reclaims and notifies.** It maintains a min-heap of
`(deadline, key, rV, uid)` fed entirely by the shared cache watcher's stream
(§6.3) — so it costs **no reads of its own** — and arms a single timer at the
nearest deadline. When the timer fires it issues a guarded `DELETE` per due entry.
The deletion produces a watch event, which is how subscribers learn the entry
expired (§6.3 covers the `Expired`-vs-`Deleted` distinction).

Deadline-armed rather than fixed-interval, and that is a requirement rather than an
optimisation: `SC-CACHE-010` writes an entry with a **50 ms** TTL and expects an
`Expired` event shortly after a 100 ms wait. No polling interval an operator would
tolerate in production hits that; a timer armed at the deadline hits it exactly. The
fixed-interval fallback exists only for `cache.watch: false`, where there is no
stream to build an index from, and that mode documents its coarser expiry
promptness.

Two instances may both see a deadline pass and both attempt the `DELETE`. The
preconditions make that safe: one wins, the other gets `404` or `409` and drops the
entry from its heap. No leader election is needed for the sweeper, which is
fortunate — a `cache`-only profile has no election to borrow.

**Whose clock writes `expiresAt`.** The writer's. This is the one place the plugin
compares timestamps across machines, and it is a deliberate, bounded exception to
§2.8's rule rather than an oversight:

- The `Lease`-backed primitives cannot use wall clocks because skew there means
  split-brain — two leaders, two lock holders — which is a correctness failure with
  no bound on its damage.
- A cache TTL under skew expires an entry early or late by the skew. For the entries
  a cache TTL actually guards — rate-limit windows, token caches, configuration
  snapshots — that is a precision defect, not a correctness one.
- The alternatives were considered and are worse. A server-stamped anchor would
  need the API server's clock, available only via the HTTP `Date` response header at
  **one-second** granularity, which would destroy the millisecond precision §2.9
  buys and which `kube`'s typed `Api` methods do not surface. A start-up-measured
  offset has the same one-second floor plus a measurement error of RTT/2.
- `cluster.provider.clock_skew_observed` (§9) already fires when a Lease timestamp
  is implausibly far from local time, and it is the signal for this too.

**The exception this creates, stated plainly.** If an operator binds
`cache: { provider: k8s }` and *omits* `lock` or `leader_election`, the wiring wraps
this cache in `CasBasedDistributedLockBackend` / `CasBasedLeaderElectionBackend`
(the CAS-over-cache defaults, which live in the `cf-gears-cluster` wiring crate at
`cluster/src/defaults/`, not in `cluster-sdk` — "SDK default" throughout this doc
means this wiring-owned omit-default behaviour, not a `cluster_sdk::` type),
whose TTL safety net is then a cache TTL — and inherits its clock-skew sensitivity,
in a place where skew *is* a correctness concern. Our cache declares `Linearizable`,
so those strict constructors accept it and nothing fails at startup. The plugin
cannot prevent this: the wiring owns the auto-wrap and a provider has no say in it.
The mitigations are documentation and defaults — §3.5's recommended YAML binds all
four explicitly, and §12 records the risk — which is why the recommended shape is
recommended rather than merely shown.

`Ttl::Indefinite` omits `spec.expiresAt` entirely, so the entry is never swept and
never filtered. `SC-CACHE-011` advances an hour to assert exactly that.

### 6.3 Watch

**One watcher for the entire cache keyspace**, per plugin instance:
`kube::runtime::watcher` over `ClusterCacheEntry` with the label selector
`cluster.cf-gears.io/managed-by=cf-gears-cluster,cluster.cf-gears.io/primitive=cache`.
Every `watch(key)` and `watch_prefix(prefix)` is an in-process registration against
that one stream, which is why both cost zero requests (§6.1) and why N subscribers
cost one connection.

Event mapping, all keyed off the `cluster.cf-gears.io/name` annotation so
subscribers see original keys rather than mapped object names:

| Watcher signal | `CacheEvent` |
|---|---|
| `Apply(entry)`, `spec.version` advanced | `Changed { key }` |
| `Apply(entry)`, only labels/annotations changed | none — nothing a consumer can observe changed |
| `Delete(entry)`, last-seen `spec.expiresAt` **in the past** | `Expired { key }` |
| `Delete(entry)`, last-seen `spec.expiresAt` absent or in the future | `Deleted { key }` |
| `Init`/`InitApply`/`InitDone` (re-list, typically `410 Gone`) | `Reset` + `ClusterMetrics::watch_reset("cache")`, and the sweeper's index is rebuilt from the `InitApply` payloads |

The `Expired`-versus-`Deleted` split needs no extra state: a `Delete` event carries
the object as it last existed, `expiresAt` included, so the distinction is read
straight off the payload. That is a direct benefit of owning the schema — a
`Lease`-based or annotation-based encoding would have had to keep a side table of
which keys had deadlines.

Exactly one event per mutation, which `SC-CACHE-012` asserts. Our writes are single
`PUT`s or single `CREATE`s, so there is no multi-write path that could produce
duplicates — a single atomic object write yields a single watch event.

`features().prefix_watch` is **`true`** — the prefix match is applied in process to
a stream that already carries every cache key, so there is no partial-coverage
caveat: the one label-selected stream sees every cache object in the namespace. A consumer may declare
`CacheCapability::PrefixWatch` against this backend, and the SDK's
`PollingPrefixWatch` polyfill is never used. Under `cache.watch: false` the
declaration flips to `false`, `watch`/`watch_prefix` return
`Unsupported { feature: "prefix_watch" }`, and the SD-style polling polyfill becomes
the consumer's option.

**`Lagged` is never emitted**, for the same reason as §4.3: the K8s watch protocol
signals a fallen-behind watcher with `410 Gone` and a mandatory re-list, which is
semantically `Reset`. Fabricating a drop count would be worse than omitting one.
A consumer that stops draining its own bounded channel still gets `Lagged` from the
SDK's watch machinery, uniformly with every other backend.

### 6.4 `scan_prefix`

A paginated `LIST` (`ListParams::default().limit(500)`, every page followed) with
the same label selector as §6.3, then client-side: drop expired entries, match the
`cluster.cf-gears.io/name` annotation against the prefix, return the original keys.

It would be cheaper to answer from the watcher's in-memory index — zero requests
instead of ⌈n/500⌉ — and that is deliberately not done. The index is
watch-derived, so it lags the API server by the stream's delivery latency and is
briefly empty after a re-list; serving `scan_prefix` from it would make a
contract method eventually-consistent on a backend that declares `Linearizable`.
The index's one job is the sweeper's deadline heap (§6.2), where lag costs
promptness rather than correctness.

In practice `scan_prefix` has one caller — the SDK's `PollingPrefixWatch` — and
this backend never triggers it, since `prefix_watch` is `true` (§6.3). It is
implemented because the contract offers it and a consumer may call it directly, not
because anything here depends on it.

### 6.5 The Consistency Declaration Follows the Read Mode

```rust
fn consistency(&self) -> CacheConsistency {
    match self.reads {
        ReadMode::Quorum => CacheConsistency::Linearizable,
        ReadMode::Cached => CacheConsistency::EventuallyConsistent,
    }
}
```

Every write is a quorum-committed, `resourceVersion`-guarded operation, so the
write path is linearizable unconditionally. The **read** path is a choice:

- **`reads: quorum` (default)** — a plain `GET` with no `resourceVersion`
  parameter, which the API server serves from etcd with a quorum read. Reads cost
  what writes cost, ~2–10 ms, and `consistency()` is `Linearizable`.
- **`reads: cached`** — `GET` with `resourceVersion=0`, served from the API
  server's watch cache. Substantially cheaper and does not touch etcd, at the price
  of possibly-stale data, so `consistency()` is `EventuallyConsistent`. The typed
  client expresses this directly — `Api::get_with(name, &GetParams::any())` for a
  point read, `ListParams::default().match_any()` for `scan_prefix` — so no raw
  request is needed.

The second mode is offered because it is a genuine and large throughput lever, and
it is safe to offer *because the declaration moves with it*: a consumer requiring
`CacheCapability::Linearizable` against a `cached` profile fails at resolution with
`CapabilityNotMet`, and the wiring's strict `CasBasedLeaderElectionBackend::new`
refuses to auto-wrap it. An operator can trade consistency for throughput, and
cannot do so behind a consumer's back.

`cluster.provider.weak_consistency` (WARN, once at startup) fires under `cached`,
naming the mode — the same audit-trail discipline ADR-009 asks of the opt-in weak
constructors.

### 6.6 Value Size

The practical object ceiling is etcd's `--max-request-bytes` (1.5 MiB by default),
and `spec.value` is base64 on the wire, inflating raw bytes by 4/3. The plugin
therefore caps `value` at `max_value_bytes` (default **256 KiB**, ≈341 KiB encoded,
leaving the rest of the budget for metadata and the JSON envelope) and returns
`InvalidConfig` naming the limit and the actual size **before issuing a request**.

Failing locally rather than forwarding an oversized object matters: the API server's
rejection is a `413` or a `422` whose message is about request bodies rather than
about a cache value, and a consumer reading it would have no reason to suspect a
size limit. The cluster cache contract states no ceiling of its own, so declaring
one here is a documented per-plugin restriction — the same latitude the Postgres
plugin takes for its 2048-byte key limit.

### 6.7 The CRD Is an Operator Prerequisite, Verified at Startup

The plugin **never installs** the CRD. Installing it would require cluster-scoped
`create` on `customresourcedefinitions`, turning the plugin's RBAC ask from "a Role
in your own namespace" (§7) into cluster-wide schema write access — and it would be
racy across replicas, all of which would attempt the install on boot.

So: the operator applies `deploy/crd.yaml` once per cluster, and
`build_and_start` verifies it with the canary write of §3.4. The failure message
names the manifest:

```
InvalidConfig: kubernetes cache backend requires the ClusterCacheEntry CRD:
  clustercacheentries.cluster.cf-gears.io is not served by this cluster.
  Apply plugins/k8s-cluster-plugin/deploy/crd.yaml (cluster-admin, once per
  cluster), then restart.
```

Two consequences of the CRD being a **cluster-scoped singleton** while the entries
are namespaced:

- **Its schema is shared by every deployment in the cluster.** Two gears running
  different plugin versions in different namespaces share one CRD, so a schema
  change is a cluster-wide coordination event. This is why §2.6 commits v1 to its
  three spec fields and why the canary checks the *schema* and not merely presence:
  a plugin newer than the installed CRD fails at boot with an actionable message
  rather than on a consumer's first `put`.
- **Namespaces still isolate the data.** Entries are namespaced and every read is
  namespace-scoped, so two deployments sharing the CRD share no keys. `lease_prefix`
  isolates further within one namespace (§12).

### 6.8 What the Cache Is Not For

The envelope, stated as numbers so the trade is decidable rather than vibes-based.
Per ADR-001: an API operation is 2–10 ms, and etcd sustains ~3 000–5 000 writes/sec
**for the entire cluster, shared with the control plane**. Under `reads: quorum`,
reads draw on the same budget as writes.

| Use | Verdict |
|---|---|
| Shard-assignment state, leader-published configuration, feature flags, small coordination documents | **Yes.** Tens to hundreds of writes/sec, values in kilobytes, reactive prefix watch included. This is what the primitive is for |
| Rate-limit windows and counters at modest rates | **Qualified yes.** Every `compare_and_swap` is a quorum read plus a quorum write, so a per-tenant counter at 50/sec is fine and one at 5 000/sec is not |
| The OAGW's 10 000 counter-updates/sec (`cpt-cf-clst-actor-oagw`) | **No.** Two to three times the whole cluster's write budget. Bind `cache` to Redis |
| Session state, response caches, anything with values in the hundreds of KiB or churn in the thousands/sec | **No.** §6.6's size cap and the write budget both bite |
| A backing store for the SDK-default lock or leader election | **Works, but don't.** The native `Lease` implementations are cheaper and clock-skew-immune (§6.2, §12) |

An operator who is unsure should look at two numbers this plugin emits:
`cluster_k8s_api_requests_total` (is our request rate a meaningful fraction of the
control plane's?) and `cluster_k8s_throttled_total` (is APF already pushing back?).
Those, not a benchmark, are what say whether a deployment is inside the envelope.

## 7. RBAC

Namespaced, and the minimum differs per primitive so an operator binding one is
not asked to grant the others'.

| Verb | `leases` | `clustercacheentries` | Needed by |
|---|---|---|---|
| `get` | ✔ | ✔ | all three — every guarded write starts with a read |
| `create` | ✔ | ✔ | all three — create-if-absent on first use |
| `update` | ✔ | ✔ | leader election, lock (guarded replace); cache (`put`, `compare_and_swap`) |
| `list` | ✔ | ✔ | the lock reaper (§5.5); cache (`scan_prefix`, and the watcher's initial list) |
| `watch` | ✔ | ✔ | leader election (§4.3), blocking `lock()` (§5.3), the cache watcher (§6.3) |
| `delete` | ✔ | ✔ | the lock reaper (§5.5); cache (`delete`, `compare_and_delete`, the TTL sweeper) |

Per primitive: leader election needs `get`/`create`/`update`/`watch` on `leases`,
and lock needs those plus `list`/`delete` for its reaper (§5.5) — both need
**nothing** on `clustercacheentries`. The cache needs all six on
`clustercacheentries` and nothing on `leases`. A profile binding one primitive
should grant only that primitive's rows — §3.4's preflight probes exactly the set
it needs and no more, so an over-narrow grant fails at boot with the missing verb
named.

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: cf-gears-cluster
  namespace: gears
rules:
# Leader election and distributed lock.
- apiGroups: ["coordination.k8s.io"]
  resources: ["leases"]
  verbs: ["get", "create", "update", "list", "watch", "delete"]
# Cache. Omit this rule entirely if `cache` is bound to another provider.
- apiGroups: ["cluster.cf-gears.io"]
  resources: ["clustercacheentries"]
  verbs: ["get", "create", "update", "list", "watch", "delete"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: cf-gears-cluster
  namespace: gears
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: cf-gears-cluster
subjects:
- kind: ServiceAccount
  name: event-broker
  namespace: gears
```

Three things this deliberately does **not** need at runtime, each of which is a
question an operator or a security reviewer will ask:

- **No `ClusterRole`.** Every runtime rule is namespace-scoped. A profile
  coordinating across namespaces is not supported and is not planned (§13 D5).
- **No `patch`.** §2.7 explains why the plugin never patches. Granting `patch`
  would be harmless but is not required, and a minimal grant is the point.
- **No access to `customresourcedefinitions`.** The plugin reads and writes
  `clustercacheentries` (the custom **resource**), never the
  `CustomResourceDefinition` itself. Installing the CRD is a one-time
  cluster-admin action outside this Role (§6.7), and §3.4's canary verifies the
  install using only the namespaced verbs above — which is precisely why it is a
  canary write rather than a CRD read.

The one cluster-admin step, for completeness — run once per cluster, not per
deployment, and not by the gear's service account:

```bash
kubectl apply -f plugins/k8s-cluster-plugin/deploy/crd.yaml
```

The `create` on `selfsubjectaccessreviews` that §3.4's preflight uses comes from
the built-in `system:basic-user` ClusterRole, which is bound to
`system:authenticated` in every standard cluster. It is listed here for
completeness, not as something to grant.

Recommended downward-API wiring, since namespace and identity resolution depend
on it (§3.6):

```yaml
env:
- name: POD_NAME
  valueFrom: { fieldRef: { fieldPath: metadata.name } }
- name: POD_NAMESPACE
  valueFrom: { fieldRef: { fieldPath: metadata.namespace } }
```

## 8. Configuration

```rust
#[derive(Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct K8sClusterConfig {
    /// Namespace for every Lease this plugin creates. Omitted → resolved from
    /// POD_NAMESPACE, then the service-account file, then the kubeconfig
    /// context; no `default` fallback (§3.6).
    #[serde(default)]
    #[expand_vars]
    pub namespace: Option<String>,

    /// This instance's identity, written as `holderIdentity`. Omitted →
    /// POD_NAME, then hostname (§3.6).
    #[serde(default)]
    #[expand_vars]
    pub identity: Option<String>,

    /// Prefix for every Lease name (§2.2). Validated as an RFC 1123 label,
    /// ≤ 40 chars. Default: "cluster".
    #[serde(default = "default_lease_prefix")]
    pub lease_prefix: String,

    /// Per-request timeout, applied to every API call. Must be shorter than the
    /// shortest renewal interval in use, so a hung API server produces a failed
    /// renewal on schedule rather than a stalled task (§4.2). Default: 10s.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,

    /// Ceiling on the jittered acquire-contention backoff (§4.1). Default: 5s.
    #[serde(default = "default_max_acquire_backoff")]
    pub max_acquire_backoff_ms: u64,

    /// Floor on an election TTL. Rejects configurations whose derived renewal
    /// rate would be a write storm against the API server (§2.9). Default: 5s.
    #[serde(default = "default_min_election_ttl")]
    pub min_election_ttl_ms: u64,

    /// The stale lock-object reaper (§5.5). Default: enabled, 5 min, with 24h
    /// retention for released lock Leases.
    #[serde(default = "default_true")]
    pub reaper: bool,
    #[serde(default = "default_reaper_interval")]
    pub reaper_interval_ms: u64,
    #[serde(default = "default_lock_object_retention")]
    pub lock_object_retention_ms: u64,

    /// Warn past this many distinct lock names observed by the reaper (§5.5).
    /// Default: 1000.
    #[serde(default = "default_lock_name_warn")]
    pub lock_name_cardinality_warn_threshold: u64,

    // ── Cache (§6) ────────────────────────────────────────────────────────────

    /// Read mode. `quorum` (default) reads through etcd and declares
    /// `Linearizable`; `cached` reads the API server's watch cache
    /// (`resourceVersion=0`) and declares `EventuallyConsistent` (§6.5).
    #[serde(default)]
    pub cache_reads: ReadMode,

    /// Whether the cache maintains its shared watcher (§6.3). When false,
    /// `watch`/`watch_prefix` return `Unsupported`, `features().prefix_watch` is
    /// `false`, and the TTL sweeper falls back to a fixed interval with coarser
    /// expiry promptness (§6.2). Default: true.
    #[serde(default = "default_true")]
    pub cache_watch: bool,

    /// Sweeper interval used **only** when `cache_watch: false`; with the watcher
    /// running, the sweeper is deadline-armed and needs no interval (§6.2).
    /// Default: 5s.
    #[serde(default = "default_cache_sweep_interval")]
    pub cache_sweep_interval_ms: u64,

    /// Max raw `value` bytes accepted by a cache write (§6.6). Rejected locally
    /// with `InvalidConfig` before a request is issued. Default: 256 KiB.
    #[serde(default = "default_max_value_bytes")]
    pub max_value_bytes: usize,

    /// Bounded retry budget for an unconditional `put` losing a guarded-write
    /// race (§6.1). Exhaustion returns `Provider { ResourceExhausted }`.
    /// Default: 3.
    #[serde(default = "default_put_max_retries")]
    pub put_max_retries: u8,

    /// Skip the SelfSubjectAccessReview RBAC probe (§3.4) for a cluster that
    /// denies it. Default: false.
    #[serde(default)]
    pub skip_rbac_preflight: bool,

    /// Keep a pre-existing Lease name for a named election instead of applying
    /// §2.2's mapping — the rolling-migration escape hatch (§14). Empty by
    /// default. Values are validated as RFC 1123 subdomains.
    #[serde(default)]
    pub election_lease_names: BTreeMap<String, String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReadMode { #[default] Quorum, Cached }
```

`K8sCacheConfig`, `K8sLeaderElectionConfig`, and `K8sLockConfig` are the
per-primitive shapes each provider deserializes, carrying the shared subset plus
only their own fields (`min_election_ttl_ms` and `election_lease_names` are
election-only; `reaper*`, `lock_object_retention_ms`, and
`lock_name_cardinality_warn_threshold` are lock-only; `cache_*`, `max_value_bytes`,
and `put_max_retries` are cache-only). The shared subset is **duplicated** across
the four shapes rather than pulled from one inner struct via `#[serde(flatten)]`:
serde's `flatten` is incompatible with `#[serde(deny_unknown_fields)]` (it silently
swallows an unknown key instead of rejecting an operator typo), and the
typo-rejection is worth more than avoiding the duplication — the same trade the
shipped postgres plugin makes. The shared `default_*` functions are centralized so
the defaults cannot drift, and TESTING.md §2's drift guard asserts the four shapes
deserialize the shared keys identically.

Note the absence of a `cache_ttl` or `default_ttl` field: TTL is per write, carried
on `PutRequest::ttl`, and the plugin never substitutes a default for
`Ttl::Indefinite` (§6.2). An operator cannot impose expiry on entries a consumer
declared permanent.

No credential fields. `kube::Config::infer()` resolves the in-cluster service
account token (with automatic refresh of the projected token, which `kube`
handles) or a kubeconfig, and that is the entirety of the auth story — which is
why this plugin is unaffected by the open credential question in `docs/PRD.md`
§3 (§13 D6).

## 9. Observability

The plugin satisfies the versioned observability contract (ADR-004,
`docs/OBSERVABILITY.md`) verbatim and emits no catalog signal under a different
name. All signals carry `provider = "k8s"`.

**Leader election** — native, so it emits the leader signals directly at each
site: spans `cluster.leader.{elect,renew,resign}`, counter
`cluster_leader_transitions_total{provider,transition}` with `transition` in
`{acquired, lost, resigned}`, and the `cluster.leader.transition` INFO log with
`election` as a *field*.

**Lock** — native: spans `cluster.lock.{try_lock,lock,renew,release}`, counter
`cluster_lock_ops_total{provider,op,result}` with `result` in
`{ok, contended, timeout, expired}`, histogram
`cluster_lock_op_duration_seconds{provider,op}`, all emitted through the injected
`cluster_sdk::observability::ClusterMetrics` sink via `ClusterMetrics::lock_op(op,
result)` and `lock_op_duration(op, seconds)` — the same two hooks the wiring
crate's `record_lock` helper (`cluster/src/defaults/lock.rs`) drives, and that the
postgres plugin already mirrors.

**Cache** — `K8sCache` is wrapped in the SDK's
`cluster_sdk::observability::InstrumentedCache` decorator, the supported path for
the cache signal set, so the full set comes for free: spans
`cluster.cache.{get,put,delete,contains,put_if_absent,compare_and_swap,watch,watch_prefix}`,
counter `cluster_cache_ops_total{provider,op,result}` with `result` in
`{ok, conflict}`, histogram `cluster_cache_op_duration_seconds{provider,op}`. The
decorator also covers the backend-internal `compare_and_delete` and `scan_prefix`
`op` values the catalog lists. Watch re-lists call
`ClusterMetrics::watch_reset("cache")`.

**Shared** — every backend failure routes through
`cluster_sdk::observability::emit_provider_error`, incrementing
`cluster_provider_errors_total{provider,kind}` and logging
`cluster.provider.error` (ERROR) with `op`, `kind`, `message`, and the
`election`/`lock`/`name` resource as a *field*. Watch re-lists call
`ClusterMetrics::watch_reset(primitive)`, backing
`cluster_watch_resets_total{provider,primitive}` for `leader` and `cache`.

All three primitives are native, so **this plugin emits the entire ADR-004 catalog
itself** — no signal arrives via an SDK default backend. That is unique among the
shipped plugins and makes the observability integration test (`K8S-SPEC-016`)
correspondingly load-bearing: nothing else is emitting on this plugin's behalf.

**Plugin-local metrics** (outside the ADR-004 catalog; adding signals is
non-breaking per ADR-004):

| Metric | Type | Why |
|---|---|---|
| `cluster_k8s_api_requests_total{provider,verb,resource,code}` | counter | Per-verb request volume and status distribution. The panel that answers "are we the reason etcd is unhappy", which is this plugin's defining operational risk. `code` is the HTTP status class, a bounded label |
| `cluster_k8s_throttled_total{provider}` | counter | `429`s from API Priority and Fairness. Distinct from the generic error counter because APF throttling is an expected, self-correcting condition that nonetheless bounds failover latency (§12) |
| `cluster_k8s_conflicts_total{provider,primitive}` | counter | `409`s. Election contention and lock contention are visible here; a sustained rise on `election` means candidates are fighting rather than settling |
| `cluster_k8s_watch_relists_total{provider,primitive}` | counter | `410 Gone` → re-list cycles. Pairs with `cluster_watch_resets_total` to separate "one stream flapped" from "every subscriber reset" |
| `cluster_k8s_lease_objects{provider,primitive}` | gauge | Objects observed by the reaper, per primitive (`primitive` includes `cache`). The #47532 early-warning gauge, and the signal behind the cardinality WARN (§5.5) |
| `cluster_k8s_reaped_total{provider,primitive}` | counter | Objects the reaper deleted. A flat line while the gauge climbs means retention is too long or the reaper is failing |
| `cluster_k8s_cache_swept_total{provider}` | counter | Entries the TTL sweeper deleted (§6.2). Distinct from the reaper's counter: a sweep is expiry working as designed, a reap is cleanup after something failed to tidy up |
| `cluster_k8s_cache_sweep_backlog{provider}` | gauge | Entries in the sweeper's deadline heap that are already past due. Non-zero means expiry deletions are lagging — under APF throttling, most likely — and since expiry is enforced on the read path (§6.2), this is a storage-and-notification-latency signal rather than a correctness one. Worth a dashboard panel and not a page |

Election names, lock names, and holder tokens are
**never** metric label values (`METRIC_LABEL_ALLOWLIST`); they appear only as span
attributes and log fields.

**Plugin-local log events**, all following `cluster.{primitive}.{event}`:

| Event | Level | Meaning |
|---|---|---|
| `cluster.provider.started` | INFO, once | Namespace, identity, server version, enabled primitives, and which source supplied namespace and identity (§3.6). The line that answers "is it coordinating where I think it is" |
| `cluster.provider.rbac_unverified` | WARN, once at startup | The `SelfSubjectAccessReview` probe was itself denied, or was skipped by config (§3.4). A later `403` is then unexplained by anything at boot, so this line is the trail |
| `cluster.provider.throttled` | WARN, rate-limited | A `429` with its `Retry-After`. Not an error — APF working as designed — but sustained throttling bounds failover latency and must be visible |
| `cluster.provider.clock_skew_observed` | WARN, rate-limited | A Lease's `renewTime` is more than one lease duration in the *future* relative to this observer. Harmless to this plugin (§2.8 never reads foreign clocks) but a strong signal that node clocks are wrong, which will hurt something else |
| `cluster.leader.transition` | INFO | Catalog event: acquired / lost / resigned, with `election` and `transition` |
| `cluster.lock.acquired` | DEBUG | Lock name and full holder token — the line that makes a token read out of `kubectl` traceable to a task (§5.6) |
| `cluster.lock.name_cardinality_high` | WARN, rate-limited | Distinct lock names past the threshold (§5.5). The unbounded-lock-name misuse, before it becomes an etcd incident |
| `cluster.watch.reset` | WARN | Catalog event; emitted on each watcher re-list |

## 10. ProviderErrorKind Mapping

Matches the platform mapping table (`docs/DESIGN.md` §4.1, K8s/`kube` column) and
extends it with the HTTP statuses that column does not enumerate.

| `kube` error / HTTP status | `ClusterError` / `ProviderErrorKind` |
|---|---|
| `Error::HyperError`, `Error::Service` (transport) | `ConnectionLost` |
| request timeout (`request_timeout_ms` elapsed), `504` | `Timeout` |
| `401 Unauthorized`, `403 Forbidden` | `AuthFailure` — **not retryable**. A `403` mid-flight means RBAC changed under a running gear; retrying cannot fix it |
| `429 Too Many Requests` | `ResourceExhausted` — retryable, and the backoff **honours `Retry-After`** when present rather than using its own schedule. APF publishes the wait it wants; ignoring it is how a throttled client becomes a throttling *cause* |
| `500`, `503` (API server or etcd unavailable, quorum loss) | `ResourceExhausted` — retryable with backoff. §3.7's "fails rather than degrades" |
| `409 Conflict` | **Not a `Provider` error.** Classified per primitive: `LockContended` on lock acquisition (§5.2), an internal re-read-and-retry on renewal (§4.2) and on `put` (§6.1, bounded), `CasConflict` on a cache `compare_and_swap` whose guarded write lost the race, `Ok(())` on resign and guarded release (§4.4, §5.4), `Ok(false)` on a guarded delete (§6.1). The single place a 409 is classified is `guarded.rs`, so no call site can get it wrong independently |
| `409 AlreadyExists` on a create | Not an error, and distinct from `409 Conflict` despite sharing a status code — the `reason` field separates them. `put_if_absent` → `Ok(None)`; every other create path falls through to its guarded-replace branch |
| `404 Not Found` on a get | Not an error: "no claim exists" / "no entry", handled by each primitive's create path. On a cache `delete`, `Ok(false)` / `Ok(())` |
| `410 Gone` on a watch | Not surfaced as an error: `kube`'s watcher re-lists and the consumer observes `Reset` (§4.3, §6.3) |
| `404` naming the **resource** rather than an object, on a cache call | `InvalidConfig` naming `deploy/crd.yaml` — the CRD is not installed or not served (§6.7). Normally unreachable, since the startup canary catches it, but a CRD deleted under a running plugin lands here and must not read as "the entry is missing" |
| `422 Unprocessable Entity` on a cache write | `InvalidConfig` naming `deploy/crd.yaml` — the installed CRD's schema rejects what this plugin version writes, i.e. version skew on a cluster-scoped singleton (§6.7). Deliberately **not** `Other`: the fix is an operator action on a manifest, not a bug report |
| `422` elsewhere, `413 Payload Too Large` | `Other` — the object we sent was rejected as invalid, which is a plugin bug (a malformed name, an oversized annotation, a value that §6.6's local cap should have refused), not an operator one |
| Malformed kubeconfig, unresolvable namespace, invalid `lease_prefix`, cache value over `max_value_bytes` | `InvalidConfig` — **not** wrapped as `Provider`, and for the value cap **not even sent** (§6.6). An operator reading it should be looking at their YAML, their pod spec, or their own write, not at their cluster |
| Any other `kube::Error` | `Other` |

The `403` row is worth dwelling on, because it is the one place this plugin
diverges from a "retry the retryable" instinct: a `403` on a background renewal
is classified non-retryable, so the renewal budget exhausts, `Status(Lost)` is
emitted, and the consumer stops acting as leader. Retrying a `403` would keep a
leader that has *lost the right to renew* believing it leads until something else
noticed.

## 11. Shutdown Sequence

`K8sClusterHandle::stop()` follows `docs/DESIGN.md` §3.13:

1. Cancel the `CancellationToken` shared by every background task. Cancellation
   unparks every blocked `lock()` waiter, which returns
   `ClusterError::Shutdown` rather than `LockTimeout` (§5.3).
2. For every active election currently in `Leader` state, deliver
   `LeaderWatchEvent::Status(Lost)` and then the terminal
   `Closed(ClusterError::Shutdown)`, awaiting the per-election tasks so every
   leader has observed loss before `stop()` returns
   (`cpt-cf-clst-fr-shutdown-revoke`). Followers get the terminal `Closed` alone.
3. Deliver `Closed(ClusterError::Shutdown)` to every active **cache** watch
   (exact and prefix), dispatched directly against the registry before the
   watcher tasks are awaited, so every subscriber observes it before `stop()`
   returns.
4. Await every remaining background task's `JoinHandle` under a bounded
   `TASK_JOIN_TIMEOUT` (10 s). Every request the plugin issues is bounded by
   `request_timeout_ms` (§8), so no in-flight call can hold a join open past that
   — which is what makes this budget a property rather than a hope.
5. Drop the `kube::Client`, closing its connection pool.
6. Set `self.stopped = true` last, so the ADR-006 `Drop` guard does not fire.

**No remote cleanup**, per `cpt-cf-clst-fr-shutdown-ttl-cleanup`: held leader
claims and held locks are not released or deleted on the way out.

That requirement needs one clarification specific to this backend, because the
usual phrasing ("they lapse via TTL") is only half true here. What lapses is the
**claim**: within one lease duration, every other participant's `Observed` record
(§2.8) declares the lease expired and a successor may take it, exactly as
intended. What does *not* lapse is the **object** — Kubernetes has no object TTL,
so the Lease sits there with a stale `holderIdentity` until someone overwrites it
(the next acquirer, which is the normal case) or the reaper prunes it (§5.5). The
distinction matters for anyone reading `kubectl get lease` after a shutdown and
concluding the lock is still held: the `renewTime` column, not the presence of the
object, is what says whether a claim is live.

A consumer that wants the claim released within a round-trip rather than within a
TTL calls `resign()` (§4.4) or `release()` (§5.4) before shutdown — which the
explicit-release contract asks of it anyway.

**Cache entries are unaffected by shutdown either way.** The TTL sweeper stops with
everything else, so an entry whose deadline passes after `stop()` is not deleted by
this instance — but expiry is enforced on the read path (§6.2), so it still reads as
absent to every reader immediately, including a fresh instance that never saw it
live. The object is reclaimed by whichever instance next holds the deadline in its
heap, or by the reaper. This is the one place where "expiry is a read-path property"
pays off directly at shutdown: there is no drain step, and nothing to get wrong.

## 12. Risks / Trade-offs

**[Risk: every operation lands on the shared control-plane budget, and the cache is
the part that can actually exhaust it]** ADR-001 puts etcd's practical sustained
ceiling at ~3 000–5 000 writes/sec *for the whole cluster*, and K8s issue #47532
documents Lease-count-driven etcd instability. The two coordination primitives
have a **bounded, calculable** steady-state rate:
`elections / renewal_interval` plus per-acquisition lock writes — so 100 elections
on a 10 s renewal interval is 10 writes/sec before any lock, and the lock term is
proportional to acquisitions, not to holders (a held lock writes nothing). A
few percent of the budget at defaults.

The **cache** has no such bound: its rate is whatever consumers do, and under
`reads: quorum` its *reads* draw on the same budget. That asymmetry is the
important part of this risk and is why §6.8 exists as a table of verdicts rather
than a caveat — a single consumer treating this cache like Redis can consume the
control plane's entire write budget on its own, and no plugin-side setting prevents
it. **This is the top operational risk of running this plugin, and shipping the
cache is what raises it from "measurable" to "unbounded".**

Mitigations, all in the design and all visible: `cluster_k8s_api_requests_total`
and `cluster_k8s_throttled_total` make the load and the pushback measurable
(§6.8 names them as the two numbers to look at); watch-driven status (§4.3) removes
the N-readers-per-interval term; the single shared cache watcher (§6.3) removes the
N-watchers-per-key term; clearing rather than deleting on release (§5.5) halves lock
churn; `reads: cached` (§6.5) removes the read term entirely at a declared
consistency cost. The unmitigable
part is that a cluster whose control plane is already near its limit will feel this
plugin, and the answer there is Redis or Postgres for `cache` and `lock` while
keeping `leader_election` here.

**[Risk: the cache watch stream carries every mutation to every instance]** §6.3's
single shared watcher is the right trade at low volume — one connection regardless
of subscriber count — but it is unfiltered by key, so an N-instance deployment
writing W cache mutations/sec delivers `N × W` events/sec off the API server. At
`N=20` and `W=200` that is 4 000 events/sec of watch traffic, which is API-server
egress and CPU rather than etcd writes, but it is real. Not mitigated in v1:
narrowing per instance would need a server-side selector on *keys*, and keys are
arbitrary strings that cannot be labels.
`cache.watch: false` turns it off entirely at the cost of `prefix_watch`. Recorded
in TESTING.md §7 as the scaling limit no test currently measures.

**[Risk: the SDK defaults over this cache are strictly worse than the native
primitives, and nothing prevents choosing them]** §6.2. Binding
`cache: { provider: k8s }` while omitting `lock` or `leader_election` gets
`CasBasedDistributedLockBackend` / `CasBasedLeaderElectionBackend` over this cache:
more etcd writes than the native `Lease` implementations, and a TTL safety net that
inherits the cache's writer-clock skew sensitivity in a context where skew means
split-brain. Our cache declares `Linearizable`, so the strict constructors accept it
and startup succeeds. The plugin has no say — the wiring owns the auto-wrap. The
mitigations are §3.5's recommended YAML binding all three explicitly and this
paragraph; `K8S-SPEC-017` asserts the arrangement is at least *reachable and
functional* so the risk is documented rather than latent.

**[Risk: API Priority and Fairness can throttle coordination traffic]** A `429`
during a leadership renewal is a *missed* renewal; enough consecutive ones expire
the claim and trigger an unnecessary failover. This is the one way a healthy
leader on a healthy network loses leadership. Mitigations:
`Retry-After`-honouring backoff (§10), `max_missed_renewals` (whose entire
purpose is tolerating exactly this), and `cluster.provider.throttled` /
`cluster_k8s_throttled_total` so the cause is identifiable rather than looking
like a network fault. Operators running coordination-heavy workloads should
consider a dedicated `FlowSchema` for the gear's service account — worth
documenting in deployment guidance, not something the plugin can do for itself.

**[Risk: the CRD is a cluster-scoped singleton shared across deployments]** §6.7.
Two gears on different plugin versions in different namespaces share one
`ClusterCacheEntry` schema, so a schema change is a cluster-wide coordination event
and v1 therefore commits to its three spec fields (§2.6). The startup canary turns
version skew into a boot-time `InvalidConfig` naming the manifest rather than a
`422` on a consumer's first write, which is the best available mitigation but not a
fix: a cluster running two plugin versions with incompatible schemas has one of them
unable to start. A second CRD version plus a conversion strategy is the real answer
and is not built.

**[Risk: unbounded lock or cache key names leak etcd objects]** §5.5, and — for
the cache — the TTL sweeper, which only reclaims entries that
*have* a TTL. `Ttl::Indefinite` entries live until explicitly deleted, by design
(`SC-CACHE-011` asserts it), so an unbounded key space of permanent entries grows
without limit. The lock reaper, `cluster_k8s_lease_objects`, and the
cardinality WARN bound the lock case; none of them *prevents* a consumer from
caching under a request id. Documented as a misuse, alerted on, not preventable.

**[Trade-off: a fresh observer waits a full lease duration before it may steal]**
§2.8. The monotonic-`Observed` rule makes the plugin immune to clock skew and
costs worst-case failover time after a simultaneous restart — a pod that starts
while the previous holder is already dead cannot distinguish "died a second ago"
from "died an hour ago" without trusting a foreign clock, so it waits. Accepted:
the alternative trades a bounded, predictable delay for an unbounded, invisible
split-brain risk.

**[Trade-off: sub-second TTLs are approximated for foreign readers]** §2.9.
`leaseDurationSeconds` is a rounded-**up** over-estimate while `ttl-ms` carries
the truth. Anything that is not this plugin sees a lock held slightly longer than
it is — safe in the only direction that matters, and invisible in practice because
nothing else reads these objects (§1.3).

**[Trade-off: three providers means up to three clients]** §3.5. An all-K8s profile
builds three `kube::Client`s where one would do. A client is an HTTPS connection pool,
not a database pool, so the cost is small; the coupling avoided (shared-client
lifecycle ownership across three independent `StopHook`s) is not.

**[Trade-off: cache TTL uses the writer's wall clock]** §6.2. The only place in the
plugin that compares timestamps across machines, and a deliberate exception: skew on
a cache TTL costs precision, whereas skew on a lease costs correctness. The
alternatives (a server-stamped `Date` header anchor, or a startup-measured offset)
both have a one-second floor that would destroy the millisecond precision §2.9's
`expiresAt` buys and that `SC-CACHE-010`'s 50 ms TTL depends on. The exception has
one sharp edge, recorded above as its own risk: the SDK-default lock and leader
election over this cache inherit the skew sensitivity in a place where it *is*
correctness.

**[Trade-off: `scan_prefix` costs a real list rather than reading the local index]**
§6.4. Answering from the watcher's in-memory index would be free but would make a
contract method eventually-consistent on a backend declaring `Linearizable`. Since
`prefix_watch` is `true`, the SDK's polling polyfill never calls it, so the cost is
paid only by a consumer calling it directly.

**[Property: every request is bounded client-side]** `request_timeout_ms` (§8,
default 10 s) is applied to every call, and it must be shorter than the shortest
renewal interval in use — validated at construction. A frozen API server
therefore produces failed renewals on schedule rather than tasks that stop
ticking, which is what lets §11 promise a bounded `stop()` at all. It is stated as
an explicit property rather than left implicit because it is load-bearing for two
other sections (§4.2's renewal and §11's shutdown budget).

**[Property: no coordination primitive writes when it is not the holder]** A follower
writes nothing (§4.1); a blocked `lock()` writes only on its acquisition attempts.
The *coordination* write rate is therefore
proportional to **held claims**, not to participants, which is what makes the first
half of §12's top risk calculable and the reason the watch-driven design is not
merely a latency optimisation. The cache is the exception and does not have this
property — a cache write rate is proportional to *consumer behaviour*, which is
exactly why §6.8 has to be a table of verdicts.

**[Property: cache expiry is a read-path property, not a sweeper property]** §6.2.
No sweep having run — because the sweeper is throttled, stopped, or the whole
instance is shutting down (§11) — can cause a stale read. Stated as a property
because three sections lean on it: §6.2's tolerance of a lagging sweeper, §11's
absence of a cache drain step, and the decision to let two instances race a delete
without coordination.

## 13. Decisions (formerly Open Questions)

### D1 — Ship a cache in this change, and on what resource?

**Decided: yes, and on a purpose-built `ClusterCacheEntry` CRD.** v1 ships all three
provider traits (§1.1). The issue left this open ("evaluate whether it's worth
shipping in this same change or deferring further, given K8s custom resources are a
weak fit for high-churn cache writes"), and the evaluation turned on one question
that had been answered wrongly.

**What changed the answer.** The original objection was that the cache contract's
version semantics do not fit Kubernetes, because `resourceVersion` is a
cluster-global opaque revision rather than a monotonic per-key `u64`. That is true
of `resourceVersion` and irrelevant to the conclusion: on a resource we define, the
version is a **field we own**, incremented inside the guarded write we were already
issuing (§2.7). It costs nothing, and it satisfies the awkward part of the contract
— `SC-CACHE-009`'s reset-to-1 on delete-and-recreate — for free, because a deleted
object takes its `spec.version` with it. With that resolved, the remaining objection
is cost, and cost is a documented envelope (§6.8) rather than an impossibility.

**Why CRD rather than ConfigMap.** ConfigMap was the leading alternative and its one
real advantage is needing no install step. Three things decided against it:

| | ConfigMap | `ClusterCacheEntry` CRD |
|---|---|---|
| Schema | `binaryData` for the value plus annotations for version and expiry — a typed field pressed into service as a key-value bag | A declared OpenAPI schema with `value`, `version`, `expiresAt` as real fields, validated server-side, with printer columns (§2.6) |
| Watch blast radius | Every cache mutation is delivered to **every** ConfigMap informer in the cluster whose selector matches — other operators, other controllers. Our churn becomes their problem | Confined to watchers of our own resource type, which is only us |
| Install | None | One `kubectl apply` per cluster, cluster-admin, verified at startup (§6.7) |

The blast-radius row is the deciding one. A cache is the highest-churn primitive
cluster has, and putting that churn on a resource type the rest of the cluster
watches is an externality no amount of documentation fixes.

**What v1 commits to.** The group, kind, and three spec fields are a wire contract
(§2.6); the CRD is an operator prerequisite the plugin verifies and never installs
(§6.7); and the cache is declared honestly as `Linearizable` only under
`reads: quorum` (§6.5).

**What is *not* claimed.** This is not a high-throughput cache and §6.8 says so in
numbers rather than adjectives. ADR-001's judgement that "K8s-only deployments have
a clear ceiling for cache/lock workloads … recommend adding Redis when workloads
exceed this" stands unchanged and is quoted approvingly — shipping the cache makes
the low-throughput shape *possible*, not the high-throughput one *advisable*.

**Consequential doc changes**, all in the parent `docs/DESIGN.md`, in scope for this
change:

| Location | Change |
|---|---|
| §4.2, "K8s, low-throughput" row | Restored to a single-provider shape: `Config: provider: k8s`, `Cache: K8s CRD`, notes naming the CRD prerequisite and the throughput envelope |
| §4.2, after the table | A paragraph on what the two K8s rows mean now: three native primitives, the CRD install step, and the envelope that separates the two rows |
| §4.1, `**K8s**` row, Cache cell | `Native (CRD + resourceVersion)` → `Native (CRD + spec.version)` — the one-field correction this decision turns on |
| §3.14 | Restored to name the K8s plugin's CRD alongside its `Lease` layout |
| §3.5, external-dependency table | `kube`'s purpose covers `Lease` **and** the `ClusterCacheEntry` CRD |
| §1.3 layer diagram, §3.11 YAML example, §3.13 sequence diagram | The k8s box reads `(Lease+CRD)`; the provider name is `k8s`, not `k8s-lease`, matching this plugin's `provider()` (§1.1) |

**ADR-001 gets a correction note**, not a rewrite. Its "Why version-based, not
value-based" section lists `K8s resourceVersion` among the versions the contract
"just exposes", and that specific claim is wrong (§2.7) — this plugin is the first
backend to reach it, and the same objection likely applies to `etcd`'s
`mod_revision`. An additive note preserves the original reasoning, which the rest of
this design relies on, while stopping the error from propagating to the next plugin
author. `docs/PRD.md` §3.1's "custom resources for cache" needs no change: with this
decision it is accurate again.

Reopened by: nothing about the shape. If the throughput envelope proves too tight in
practice, the answer is not a different resource — it is binding `cache` to Redis
and keeping the other two here, which per-primitive routing already supports.

### D2 — Native lock, when the issue's DoD did not ask for one

**Decided: yes, ship it.** The DoD names leader election;
the lock is added because the marginal cost is small and the marginal value is
not. The machinery — guarded Lease writes, the `Observed` expiry rule, the
watch-driven wake — is built for leader election regardless, so the lock
reuses it rather than adding new infrastructure. And it is the only way
a K8s deployment gets a *linearizable* cross-instance lock without Redis:
`CasBasedDistributedLockBackend::new` refuses an `EventuallyConsistent` cache per
ADR-009, so a Redis-cache profile that omits `lock` fails startup, and a
Postgres-cache profile gets Postgres' native lock but a K8s-plus-Redis one
otherwise has nothing linearizable to bind. `docs/DESIGN.md` §4.1 already lists
K8s lock as "Native (Lease API)", so this closes a documented gap rather than
opening new scope.

### D3 — Guarded `replace` vs. server-side apply

**Decided: guarded `replace` (`PUT` with `resourceVersion`) everywhere; no
`patch`, no `Patch::Apply`.** §2.7. Optimistic concurrency with a 409 is what
makes the primitive linearizable; both patch flavours resolve concurrent writes
by merging instead of rejecting, which is correct for controllers converging on
desired state and wrong for mutual exclusion. The cost is a read before every
write — one extra round trip, ~2–10 ms — which is why §5.2's acquisition path
uses create-if-absent to skip the read in the common case rather than "optimising"
the guard away.

Reopened by: nothing plausible. A future K8s API offering a conditional patch
would be a mechanism change, not a decision change.

### D4 — Monotonic `Observed` expiry vs. comparing `renewTime` to local wall time

**Decided: monotonic `Observed` (§2.8), matching `client-go`'s elector.** The
naive wall-clock comparison — which mini-chat and chat-engine both use today — is
unsound under node clock skew in both directions, and Kubernetes guarantees
nothing about clock synchronisation. This commits v1 to a fresh observer waiting
one full lease duration before it may steal a lease it has never seen renewed
(§12), which is the price of never trusting a foreign clock.

### D5 — Cross-namespace coordination

**Decided: no. The namespace is resolved once and every Lease lives in it (§3.6),
and the RBAC is a namespaced `Role` (§7).** A profile coordinating across
namespaces would need a `ClusterRole`, which changes the plugin's security posture
from "grant a Role in your own namespace" to "grant cluster-wide access to
leases" — a materially harder review for a capability no consumer has asked for.
Consumers in different namespaces that must coordinate share a third namespace, or
use a backend that has no namespace concept.

### D6 — Credential resolution

**Decided: `kube::Config::infer()`, and nothing else. No `secret_ref`, no
credential fields (§8).** In-cluster service-account tokens (including projected
token refresh) and kubeconfig are what `kube` already resolves, and they are the
only two ways anything authenticates to an API server. This plugin is therefore
*unaffected* by the open credential question in `docs/PRD.md` §3 and
`docs/DESIGN.md` §3's credential note — worth stating explicitly, because every other plugin's
answer there is "deferred to the OOP design" and this one's is "not applicable".

### D7 — A shared K8s-access abstraction for the workspace

**Decided: no. This plugin uses `kube` directly.** `libs/toolkit-k8s-auth` is a
`TokenReview`-based authenticator, not a general API-access layer, and there is no
workspace rule routing K8s access through anything. The workspace's other `kube`
consumers (mini-chat, chat-engine) use it directly too. Building an abstraction
now would mean designing it against three consumers whose needs barely overlap —
a `TokenReview` call, a Lease elector, and this plugin's Lease CRUD-plus-watch.

Reopened by: §14's migration landing. Once mini-chat and chat-engine consume
`LeaderElectionV1` instead of their own electors, the workspace's direct `kube`
consumers drop to this plugin plus `toolkit-k8s-auth`, and the question becomes
whether *that* pair is worth an abstraction — which is a much easier question to
answer with the duplication already deleted.

## 14. Migration: mini-chat and chat-engine

`docs/DESIGN.md` §4.3 lists `LeaderElector` + `K8sLeaseElector`
(`mini-chat/src/infra/leader/`, duplicated in `chat-engine/src/infra/leader/`) as
code this plugin supersedes, with the migration itself a separate per-gear change.
Two things this design owes that change:

**What the plugin does better, so the migration is not a lateral move.** The
existing electors are production-quality and their structure carried directly into
§4 — the create-or-acquire loop, the `resourceVersion`-guarded replace, the
equal-jitter backoff, the release-on-clean-shutdown. Three differences are
substantive:

- **Clock skew.** Both electors compare `renewTime` against their own
  `Timestamp::now()`. §2.8 replaces that with the monotonic `Observed` rule. This
  is a latent correctness bug in the current code, and it is the strongest single
  argument for the migration.
- **Polling vs. watching.** Both electors re-`get` the Lease every
  `renew_period` (2 s by default) from every replica. §4.3 watches instead: on a
  3-replica deployment that is 1.5 reads/sec replaced by one watch stream, and
  failover detection improves from "up to one poll interval" to "as fast as the
  watch delivers".
- **`run_while_leader` already exists in the SDK.** The electors' `LeaderWorkFn`
  plumbing — spawn work on acquire, cancel on loss, bounded stop timeout — is
  `LeaderWatch::run_while_leader(stop_timeout, work)` in `cluster-sdk`. The
  migration deletes it rather than porting it.

**The rolling-upgrade hazard, and the escape hatch.** mini-chat's Lease is named
`{lease_prefix}-{role}` (and pre-created by its Helm chart's `lease.yaml`). This
plugin's name for the same election is §2.2's mapped name, which is different. A
rolling upgrade would therefore have old pods contending on the old Lease and new
pods on the new one — **two leaders, for the duration of the rollout**, which for
an outbox worker means duplicate delivery.

Three ways through it, in the order a migrating gear should consider them:

1. **`election_lease_names` (§8)** — pin the plugin to the literal existing Lease
   name for that election, so old and new pods contend on the same object
   throughout. The holder-identity semantics are compatible (both write a pod name
   into `holderIdentity`), and the expiry rules differ only in *which* clock each
   side trusts, so the worst case during the overlap is a slightly conservative
   takeover rather than a double one. This is why the field exists, and it is the
   recommended path for a gear that cannot take a restart.
2. **A full stop-then-start**, accepting a leadership gap of one lease duration.
   Simplest, correct, and fine for anything whose leader work tolerates a 30 s
   gap.
3. **Rename the election deliberately** and accept the overlap, if the leader work
   is idempotent. Rarely the right answer; listed because for some workloads it
   genuinely is.

The Helm chart's pre-created `lease.yaml` can be deleted in either case: §4.1's
create-if-absent needs no pre-provisioned object, and `create` is already in §7's
verb set.
