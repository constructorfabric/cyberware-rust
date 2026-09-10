---
status: accepted
date: 2026-09-03
---

# Coordination: consume the platform `cluster` gear's leader election

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Why leader election and not the distributed lock](#why-leader-election-and-not-the-distributed-lock)
  - [Advisory semantics and what QE requires](#advisory-semantics-and-what-qe-requires)
  - [Startup validation replaces the bootstrap probe](#startup-validation-replaces-the-bootstrap-probe)
  - [Default deployment shape](#default-deployment-shape)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Consume the platform `cluster` gear's leader election](#consume-the-platform-cluster-gears-leader-election)
  - [Consume the platform `cluster` gear's distributed lock](#consume-the-platform-cluster-gears-distributed-lock)
  - [Separate QE-owned `CoordinationPluginV1` trait](#separate-qe-owned-coordinationpluginv1-trait)
  - [Bundle coordination methods into `QuotaEnforcementStoragePluginV1`](#bundle-coordination-methods-into-quotaenforcementstoragepluginv1)
  - [No in-process abstraction, external orchestration only](#no-in-process-abstraction-external-orchestration-only)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-quota-enforcement-adr-coordination-plugin`

> **Revision history.** The first version of this record (2026-05-07) chose a QE-owned `CoordinationPluginV1` trait
> with a QE-owned default plugin on the storage database. The platform `cluster` design (PRD, DESIGN, ADRs) already
> existed at that time (2026-03-31) but was not considered. Its SDK and gear implementation landed on 2026-06-18, and
> the gear became deployable out of process on 2026-08-25. This revision (2026-09-03) replaces the QE-owned contract
> with the platform primitive. The record keeps its ID; the design components and features that reference it keep
> their references.

## Context and Problem Statement

Two QE background tasks, `LeaseSweeper` and `RetentionSweeper`, must run as cluster-wide singletons. Only one replica
should sweep expired leases or reclaim retention rows at a time; otherwise every replica does the same work. The
singleton property also underpins `cpt-cf-quota-enforcement-nfr-recovery` (RTO ≤ 15 min): when the active replica
dies, a survivor must take over within a bounded time.

The natural realisation is a TTL-bounded claim: every replica competes for a per-scope claim, the winner runs the
task and renews the claim, and the claim expires on crash. The mechanism behind the claim (Postgres, Redis, etcd,
Kubernetes Lease, in-memory for tests) is operationally diverse and only weakly tied to the storage backend choice.

The 2026-05-07 version of this record asked where such a primitive should live in the QE contract surface, and
answered with a QE-owned trait plus a QE-owned default plugin. It did not consider the platform `cluster` design,
which existed at the time. That design is now implemented: the `cluster` gear
([PRD](../../../cluster/docs/PRD.md), [DESIGN](../../../cluster/docs/DESIGN.md)) provides exactly this primitive to
every gear: named leader election with automatic renewal, TTL-bounded distributed locks, per-profile backend selection
in operator YAML, and startup validation of declared capabilities. Its PRD names per-gear coordination code as the
duplication it exists to remove (cluster PRD §1.3).

Question: should QE keep its own coordination contract, or consume the platform primitive, and if so which one?

## Decision Drivers

- **Sweeper singleton coordination** for `LeaseSweeper` and `RetentionSweeper`. The `NotificationDispatcher` is fenced
  by the `toolkit-db` Outbox leased-handler model and needs no election (DESIGN §3.2).
- **NFR recovery (RTO ≤ 15 min)**: a crashed leader's claim MUST become acquirable by a survivor within a bounded time
  (`cpt-cf-quota-enforcement-nfr-recovery`).
- **Storage plugin contract minimality**: the storage trait carries 13 invariants and about 20 methods; coordination
  semantics do not belong in it.
- **Independent operational evolution**: the coordination backend evolves on a different axis than the storage
  backend; an operator with Postgres storage may want Redis, etcd, or Kubernetes for coordination.
- **Default-deployment ergonomics**: small deployments must not need a second infrastructure dependency to run the
  sweepers.
- **Platform reuse**: the platform owns one coordination gear with one contract, one conformance suite, and one
  operator configuration model. A second, QE-owned coordination contract duplicates all three.

## Considered Options

- **Consume the platform `cluster` gear's leader election** (chosen): one named election per sweeper scope through
  the `cluster-sdk` leader-election facade, wrapped in a thin QE port and adapter.
- **Consume the platform `cluster` gear's distributed lock**: one TTL-bounded lock per sweeper scope through the
  `cluster-sdk` lock facade, renewed by the sweeper.
- **Separate QE-owned `CoordinationPluginV1` trait** (the 2026-05-07 decision): a QE plugin contract with
  `try_lock`, `renew`, `release`, and a QE-owned default plugin on the storage database.
- **Bundle coordination methods into `QuotaEnforcementStoragePluginV1`**: add lock methods to the storage trait.
- **No in-process abstraction, external orchestration only**: Kubernetes Lease, a single-replica StatefulSet, or a
  Nomad job constraint, with no QE-side abstraction.

## Decision Outcome

Chosen option: **Consume the platform `cluster` gear's leader election**, because it (a) removes a QE-owned contract,
a QE-owned plugin crate, and a QE-owned vendor lookup, (b) gives the operator backend selection per deployment in the
cluster profile YAML with no QE code change, (c) validates the backend guarantee QE needs at startup instead of at the
first failover, and (d) is the primitive the cluster gear designs for singleton background work.

The shape on the QE side:

- The `quota-enforcement` gear depends on `cluster-sdk` only and declares no `deps = [cluster]` edge (cluster DESIGN
  §3.17.7): a deployed consumer links no cluster gear, so the edge would fail the registry build. Start ordering
  comes from the cluster gear's `system` tier, and readiness gating from the SDK-submitted consumer registration. The
  binary decides the profile: an embedded binary links the `cluster` gear, a provider plugin, and the mandatory
  `grpc-hub`; a remote image enables QE's forwarding Cargo feature and links none of them. QE source is the same in
  both.
- QE defines one typed cluster profile marker, `QuotaEnforcementProfile`, with the profile name `quota-enforcement`.
  The name appears in exactly two places: that marker and the operator's YAML.
- In its lifecycle `start`, QE resolves the leader-election facade for that profile with the `Linearizable`
  capability requirement and scopes it under the `qe` prefix.
- QE keeps a closed scope enum, `SingletonScope`, with the variants `LeaseSweeper` and `RetentionSweeper`. Each
  variant maps to one election name (`lease-sweeper`, `retention-sweeper`). Free-form names never reach the cluster
  facade from QE code.
- A thin domain port, `SingletonCoordinator`, exposes one operation: run a unit of work while this replica is the
  leader of a scope. Its single infrastructure adapter, the `CoordinationAdapter` component
  (`cpt-cf-quota-enforcement-component-coordination-plugin`), implements the port over the cluster facade. The port
  exists for the domain-layer dependency rule; it is not a plugin extension point.
- The adapter drives the election watch itself, with the same reactive pattern the cluster SDK's run loop
  implements: the sweep body starts when this replica becomes leader and receives a child cancellation token; the
  resolved cluster backend renews the claim on its own cadence; the token is cancelled when leadership is lost, and
  the body is aborted after a stop timeout; the body restarts on re-election. The adapter keeps ownership of the watch so that on
  graceful shutdown it can cancel the body and then call `resign`, which elects a successor without waiting for the
  TTL. The SDK's own `run_while_leader` cannot do this: it consumes the watch, and a dropped watch performs no resign
  I/O.
- The election TTL and the missed-renewal budget are QE operator configuration, with the cluster defaults as the
  defaults.

### Why leader election and not the distributed lock

The cluster lock forbids non-cluster remote I/O inside the critical section (cluster PRD §5.3, "No Remote I/O Inside
the Critical Section", with a planned workspace lint). A sweeper writes to the storage backend for the whole
duration of its cycle. That is exactly the shape the rule forbids. Leader election is the cluster primitive for
"which replica runs this workload" (cluster DESIGN §3.3, consumer patterns), and it carries the renewal loop, the
observation model, and the graceful step-down that the 2026-05-07 record specified by hand.

### Advisory semantics and what QE requires

Cluster leader election is advisory (cluster PRD §5.2, "Advisory Semantics, Not Mutual Exclusion"): two replicas can
both observe themselves as leader for a window bounded by the election TTL plus observation lag. Both QE sweepers tolerate that window:

- `LeaseSweeper` reclaims leases that are already expired by timestamp. Lazy semantic release (I4) keeps accounting
  correct regardless of who reclaims, and a duplicate reclamation is a no-op on rows already transitioned.
- `RetentionSweeper` deletes rows past their retention window. A duplicate delete finds nothing.

So the singleton property QE needs is "at most one active sweeper in steady state, and a bounded takeover time", not
strict mutual exclusion. QE still requires the `Linearizable` capability at resolve time: an eventually consistent
backend can elect two leaders on every failover (cluster ADR-009), which would turn the bounded window into a steady
state.

### Startup validation replaces the bootstrap probe

The 2026-05-07 record required a `try_lock` plus `release` probe per scope at bootstrap, with fail-fast abort. That
probe goes away. The cluster resolver validates the declared capability against the bound backend (cluster PRD §5.5,
"Capability Mismatch Fails Startup, Not Production"): in the embedded profile a mismatch or an unbound profile returns an error
from `resolve()` and QE fails startup; in the deployed profile the same verdict arrives through the readiness gate.
The QE health check names `cluster` as the failed dependency, the same way it names the storage plugin.

### Default deployment shape

The operator binds the `quota-enforcement` profile in the cluster section of the deployment YAML. With the
`standalone` provider the election runs in-process, which is correct for a single-process deployment and for tests.
With the `postgres` provider the election runs on a Postgres cache table; this may be the same Postgres server the QE
storage plugin uses, so a default multi-instance deployment still adds no infrastructure dependency. Kubernetes Lease,
Redis, etcd, and NATS backends are cluster plugins on the cluster roadmap; QE gains them by configuration.

An operator who needs a decorrelated failure domain binds the QE profile to a different backend than the storage
plugin uses. QE code does not change.

### Consequences

- No `quota-enforcement-coordination-plugin` crate, no `CoordinationPluginV1` trait in `quota-enforcement-sdk`, no
  `coordination_vendor` configuration, and no coordination GTS plugin spec. The gear depends on `cluster-sdk`; the
  embedded binary, not QE, links the `cluster` gear, a provider plugin, and `grpc-hub`.
- Four traceability IDs retire with this revision: the coordination-plugin contract entry in PRD §7.2 (replaced by
  `cpt-cf-quota-enforcement-contract-cluster-coordination`, a consumed contract), the lock-primitive algorithm and
  the lock state machine in the foundation feature, and the default-implementation definition of done (replaced by
  `cpt-cf-quota-enforcement-dod-coordination-adapter`). The component ID and the interface ID stay and are re-scoped
  to the adapter.
- The sweeper features consume the adapter's run-while-leader semantics and no longer specify renewal cadence,
  follower fallback, or jittered re-acquisition; the resolved cluster backend owns those.
- The bootstrap probe step is removed from DESIGN §3.7 and from the foundation feature; the resolve step in `start`
  replaces it.
- The cluster gear versions the leader-election primitive per its own policy (cluster PRD §7.1). QE tracks the
  `cluster-sdk` major version like any other platform SDK.
- QE is the first gear in the workspace to wire cluster leader election in production code. Integration defects
  surface in the foundation feature; the tests below run against both shipped cluster backends.
- Multi-process deployments on a SQLite storage backend have no cluster backend today (the standalone provider is
  in-process). QE does not target that shape; the record states it so an operator does not discover it in
  production.

### Confirmation

Confirmed by: a resolve-time test that binds the `quota-enforcement` profile to an eventually consistent cache double
and asserts `CapabilityNotMet` at startup; a handover test with two sweeper participants over one standalone backend
that asserts one leader at a time and a successor after `resign`; the same handover test over the Postgres cluster
backend; the chaos drill that kills the elected replica and verifies a survivor runs the sweep within one election
TTL plus observation lag (RTO ≤ 15 min per `cpt-cf-quota-enforcement-nfr-recovery`); and two forced-overlap tests
that run two sweep bodies at once against the same rows. Concurrent lease sweeps must produce one state transition,
one capacity reversal, and one outbox event per lease. Concurrent retention sweeps must complete without error, with
each expired row deleted once. The overlap tests are the evidence for the advisory-semantics argument above; the
uniqueness and handover tests alone do not cover it.

## Pros and Cons of the Options

### Consume the platform `cluster` gear's leader election

- Good, because QE ships no coordination contract, plugin crate, or conformance suite; the platform owns them once.
- Good, because the operator selects the backend per deployment in YAML, with no QE recompilation.
- Good, because the renewal loop, follower fallback, re-enrolment, and graceful step-down are cluster code, not
  sweeper code.
- Good, because the linearizable requirement is validated at startup, not discovered at the first failover.
- Bad, because the semantics are advisory; QE must state, and its sweepers must keep, idempotent sweep bodies.
- Bad, because the SDK's run loop consumes the watch and cannot resign; the adapter owns its own watch loop to keep
  the immediate handover on graceful shutdown.
- Bad, because QE is the first production consumer of cluster leader election in the workspace and carries the
  integration risk.
- Bad, because a multi-process deployment on SQLite storage has no cluster backend.

### Consume the platform `cluster` gear's distributed lock

- Good, because a lock is the closest shape to the 2026-05-07 record.
- Bad, because the cluster lock forbids non-cluster remote I/O inside the critical section, and a sweep cycle is
  storage I/O from start to end. The workspace lint planned for this rule would flag every sweeper.
- Bad, because the sweeper would own the renewal loop again.

### Separate QE-owned `CoordinationPluginV1` trait

- Good, because QE controls the contract and its closed error type.
- Good, because the default plugin on the storage database needed no second infrastructure dependency.
- Bad, because it reimplements a platform primitive, against the cluster PRD goal of zero per-gear coordination code.
- Bad, because operators would swap backends by replacing a QE crate instead of editing one YAML block.
- Bad, because QE would carry its own probe, versioning, vendor lookup, and conformance tests for coordination.

### Bundle coordination methods into `QuotaEnforcementStoragePluginV1`

- Good, because a single plugin crate is conceptually simpler for first-time readers.
- Bad, because coordination becomes hostage to the storage backend choice.
- Bad, because the storage trait already encodes 13 invariants and about 20 methods.
- Bad, because a coordination-only change would force a storage-plugin major bump.

### No in-process abstraction, external orchestration only

- Good, because zero QE-side code for coordination.
- Bad, because it imposes an external orchestrator on every deployment, including single-process ones.
- Bad, because leader and follower state is not observable in-process for readiness and telemetry.
- Bad, because the cluster gear already offers this shape as one plugin among others (Kubernetes Lease), so the
  option adds nothing the chosen option lacks.

## More Information

- Cluster [PRD](../../../cluster/docs/PRD.md): §1.3 goals, §3.1 deployment shapes and profiles, §5.2 leader election,
  §5.3 distributed locks and the no-remote-I/O rule, §5.5 consumer requirements and startup validation.
- Cluster [DESIGN](../../../cluster/docs/DESIGN.md) §3.3: the three consumer patterns for singleton work and the
  staleness bound of the leadership signal.
- Cluster [ADR-009](../../../cluster/docs/ADR/009-leader-election-backend-safety.md): per-backend safety of
  CAS-based leader election; why QE requires `Linearizable`.
- Cluster [ADR-012](../../../cluster/docs/ADR/012-store-owned-leases.md): store-owned leases; why a cluster replica
  restart does not revoke QE's claims.
- Cluster [feature 003](../../../cluster/docs/features/003-leader-election.md): the leader-election primitive.
- DESIGN §3.2 "Component model": the `CoordinationAdapter` component.
- DESIGN §3.3 "Cluster Coordination": what QE requires from cluster and the shape of the QE port.
- DESIGN §3.6 "Sequences": the sweeper election flow.
- Sibling ADR `cpt-cf-quota-enforcement-adr-storage-backend`: the pluggable storage contract, which stays free of
  coordination semantics.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses:

- `cpt-cf-quota-enforcement-fr-pluggable-storage`: keeps coordination out of the storage-pluggable contract; the two
  evolve independently.
- `cpt-cf-quota-enforcement-nfr-recovery`: a crashed leader's claim lapses within the election TTL, and the cluster
  gear elects a survivor without QE-side re-acquisition code.
- `cpt-cf-quota-enforcement-fr-lease-timeout`: `LeaseSweeper` runs under the `lease-sweeper` election.
- `cpt-cf-quota-enforcement-fr-notification-plugin`: the `NotificationDispatcher` is fenced by the `toolkit-db`
  Outbox lease and joins no election; retained here for the history of the singleton outbox draining.
- `cpt-cf-quota-enforcement-contract-cluster-coordination`: the consumed cluster contract this decision introduces.
- Sibling ADR `cpt-cf-quota-enforcement-adr-storage-backend`: storage and coordination stay separate contracts.
