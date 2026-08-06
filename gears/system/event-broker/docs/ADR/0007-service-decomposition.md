# ADR-0007: Service Decomposition — Process Shape, Cluster-Primitive Mapping, and Standalone Dispatcher

<!-- toc -->

- [Status](#status)
- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Single Binary, Multi-Mode](#single-binary-multi-mode)
  - [Three-Binary Split](#three-binary-split)
- [More Information](#more-information)

<!-- /toc -->

**ID**: `cpt-cf-evbk-adr-service-decomposition`

## Status

Accepted

## Context and Problem Statement

`DESIGN.md:49-56` defines three roles — Ingest, Delivery, Dispatcher — as domain traits inside a single `event_broker` crate, gated per deployment mode by a `ClusterCapabilities` platform abstraction (pub/sub, leader election, distributed locks). This was written before the platform's actual coordination layer existed. Two things needed resolving before any code could be scaffolded:

1. **Process/binary shape.** Does standalone-vs-cluster mean one binary with mode-selected wiring, or three separately-built binaries (ingest/delivery/dispatcher)?
2. **`ClusterCapabilities` reality.** The platform now ships a real `cluster` gear (`gears/system/cluster/`) exposing `ClusterCacheV1`, `LeaderElectionV1`, `DistributedLockV1` via `cluster-sdk`, with `standalone-cluster-plugin` and `postgres-cluster-plugin` providers already merged. `DESIGN.md:739-768` already documents the mapping from `ClusterCapabilities` prose to these real primitives in detail — this ADR does not re-derive that mapping, it adopts it as the resolution strategy for `domain/cluster.rs`.

A third, narrower question fell out of scaffolding the module tree: does the dispatcher get constructed at all in standalone mode, and does the planned `infra/workers/` module tree (`cleaner.rs`, `retention.rs`, `reaper.rs`) match the broker's own no-cleaner/no-retention invariant (`DESIGN.md` §3.7 Key Invariants)?

## Decision Drivers

- Every other implemented Gears module in this repo is one-gear-per-crate with a single entry point; introducing a new build/packaging shape (three binaries) needs a real operational justification, not just historical phrasing in the design doc.
- `DESIGN.md:739-768` already fully specifies how the broker should use the cluster gear's primitives — this ADR should adopt that mapping, not invent a parallel one.
- `DESIGN.md` §3.7 Key Invariants is an explicit, deliberate statement ("the broker has no Cleaner / Retention workers") and takes precedence over an older module-tree listing that predates it.
- Downstream tickets (#4345 dispatcher routing, #4346 REST API, #4347 standalone runtime) need this decided once so they don't each re-derive it.

## Considered Options

- Single binary, multi-mode: one `event-broker` crate/binary; `EventBrokerConfig.mode` selects which services/routes `module.rs` constructs at startup.
- Three-binary split: separate `event-broker-ingest`, `event-broker-delivery`, `event-broker-dispatcher` binaries.
- For cluster-primitive mapping: wrap the real `cluster` gear directly, vs. bypass it in standalone mode with a hand-rolled in-process implementation.
- For standalone dispatcher: construct a pass-through dispatcher unconditionally, vs. never construct one in standalone mode.

## Decision Outcome

**Single binary, multi-mode.** The `event-broker` crate ships as one binary/gear. `EventBrokerConfig.mode` (`standalone` | `cluster_ingest` | `cluster_delivery` | `cluster_dispatcher`) selects which services `module.rs` constructs and which routes it mounts at startup, per `DESIGN.md:2224`'s Deployment Modes table. Deploying a "cluster_ingest-only process" means running this same binary N times with that mode config, not compiling a separate ingest-only artifact.

**`domain/cluster.rs` wraps the real `cluster` gear, not a new abstraction.** It resolves `ClusterCacheV1` / `LeaderElectionV1` / `DistributedLockV1` via `ClientHub`, scoped under a fixed `"evbk"` prefix (matching `DESIGN.md:756-764`'s `evbk.*` cache-key table). Both standalone and cluster modes resolve the same facade types — standalone is backed by the zero-dependency `standalone` cluster-gear provider instead of a network-backed one, per `DESIGN.md:2208`'s framing that they are "variants of the same module wiring."

**No dispatcher is constructed in standalone mode.** `module.rs` never constructs a dispatcher instance or mounts a dispatcher route/proxy layer when `mode = standalone`; REST handlers call Ingest/Delivery in-process directly. This resolves the open question raised in the tracking issue in favor of `DESIGN.md:1391,2521`'s framing (dispatcher is cluster-only). It does not conflict with #4345's plan for a pass-through dispatcher *stub* used by that ticket's own tests — that stub exercises the dispatcher's routing code in isolation; it is not a claim that production standalone deployments run a dispatcher process.

**The module tree drops `infra/workers/cleaner.rs` and `retention.rs`.** `DESIGN.md` §3.7 Key Invariants states the storage backend owns all event deletion — "the broker has no Cleaner / Retention workers" — directly contradicting the older module-tree listing. Only `reaper.rs` (expired subscriptions + idempotency-key cleanup, both broker-owned per §3.7 and ADR-0004) remains under `infra/workers/`.

### Consequences

**Removed:**
- Three-binary-split option (not pursued).
- `infra/workers/cleaner.rs`, `infra/workers/retention.rs` from the module tree and the example config's `workers:` block (`DESIGN.md:579-612`, `2361-2391`).
- The "future ADR (service-decomposition)" placeholder at `DESIGN.md:81` (now a real reference to this file).

**Added:**
- `docs/ADR/0007-service-decomposition.md` (this file).
- `event-broker` crate skeleton (`gears/system/event-broker/event-broker/`) matching the corrected module tree — structure only, `todo!()`/`unimplemented!()` bodies.
- `DeploymentMode::{ingest,delivery,dispatcher,reaper}_active()` predicates: report which of {`IngestService`, `DeliveryService`, dispatcher, `reaper`} *should* be active for the configured mode, matching `DESIGN.md:2224` exactly. These are mode predicates only - `module.rs` does not yet construct services or gate real routes/lifecycle work on them; that lands with #4345/#4346/#4347.

**Unchanged:**
- `DESIGN.md:739-768`'s `ClusterCapabilities`-to-`cluster-sdk` mapping (adopted as-is, not modified).
- The four deployment modes and their semantics (`DESIGN.md` §4.1).

**Not decided here (explicitly out of scope):**
- Which cluster-gear *profile name* `event-broker` binds to in cluster mode — operator-config surface, deferred to whichever ticket first wires real cluster-mode config.
- Whether `evbk.*` notification cache keys carry a payload or are pure version-bump signals — inherits `DESIGN.md` §4.7's still-open long-poll cache/backfill question.
- Handler bodies (#4346), the dispatcher's pass-through forwarding (#4345, delivered) and its
  real per-instance routing algorithm (#4438), and any real backend (#4347/#4348/#4349/#4350).

### Confirmation

Confirm by checking that: (1) `cargo build -p cf-gears-event-broker` produces one binary/library target, not three; (2) `domain/cluster.rs` imports `cluster_sdk` types directly with no locally-defined `ClusterCapabilities` trait; (3) a unit test booting `EventBrokerModule` in each of the four modes shows no dispatcher active in `standalone`; (4) `infra/workers/` contains only `reaper.rs`.

## Pros and Cons of the Options

### Single Binary, Multi-Mode

Pros:

- Matches every other implemented Gears module's one-gear-per-crate shape.
- Independent process *scaling* is achieved the same way (N processes, different `mode` config) without new build machinery.
- Simpler skeleton: one crate, one set of dependencies.

Cons:

- Cannot ship an independently-versioned ingest-only binary artifact — only independently-*configured* processes of the same binary.

### Three-Binary Split

Pros:

- Would allow independent binary versioning per role, if that ever becomes a real requirement.

Cons:

- No precedent in this repo's build/packaging tooling.
- No current requirement justifies the added complexity — independent process scaling doesn't need independent binaries.

## More Information

- `DESIGN.md:49-56` (role definitions), §3.8 Deployment Topology, §4.1 Deployment Modes (`DESIGN.md:2204-2391`).
- `DESIGN.md:739-768` — `ClusterCapabilities` (Platform Dependency): the adopted mapping to `cluster-sdk`.
- `DESIGN.md` §3.7 Key Invariants (`DESIGN.md:2195-2202`) — the no-Cleaner/no-Retention invariant this ADR reconciles the module tree against.
- Related tracking: gears-rust#4343 (this ADR + skeleton), #4345 (dispatcher routing), #4346 (REST API), #4347 (standalone runtime).
- Platform decision for how the dispatcher routes to specific ingest/delivery instances: ToolKit OoP ADR-0009 *Instance-Addressable Discovery* (see `docs/arch/toolkit-oop/ADR/`).
