# Product Requirements — Graph Analytics

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Topology Access](#51-topology-access)
  - [5.2 Metric Computation](#52-metric-computation)
  - [5.3 Job Lifecycle](#53-job-lifecycle)
  - [5.4 Caching](#54-caching)
  - [5.5 Multi-Tenancy and Access Control](#55-multi-tenancy-and-access-control)
  - [5.6 API Surfaces](#56-api-surfaces)
  - [5.7 Observability and Readiness](#57-observability-and-readiness)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

**Graph Analytics** computes whole-graph structural metrics — degree, PageRank,
connected components, betweenness centrality and community detection — over the
graph owned by the graph-storage gear, and publishes them into a revision-keyed
cache that graph-storage reads to annotate projections. It is a separate
deployment unit with its own worker, memory and connection budget, so a long or
runaway computation cannot degrade ingest or interactive queries.

### 1.2 Background / Problem Statement

These metrics were originally a component inside graph-storage. Whole-graph
analytics and interactive graph queries have opposite resource profiles: a
metrics job holds a million-node topology in memory for minutes, while an ingest
batch or a search request wants a connection and a few milliseconds. Sharing one
runtime meant one deployment's memory, worker and connection budget had to
satisfy both, and the failure was one-directional — a loop of accidental
analytics jobs degrades ingest, while no volume of ingest degrades analytics the
same way. Isolation rested on an obligation on the code ("must not starve request
handling"), which is not a boundary.

The decision to move the computation here (ADR-0002)
also fixed the seam: a read-only database role over topology columns only,
graph-storage owning all DDL and the graph revision, and this gear owning the
metrics cache and the job state machine.

### 1.3 Goals (Business Outcomes)

- Analysts get structural rankings and clusterings over a shared graph without
  the cost of those computations being paid by every interactive request.
- Operators bound analytics CPU, memory and connections independently, and can
  decline to deploy analytics at all without losing any graph capability.
- Results are reproducible: the same graph state and parameters produce the same
  numbers, so a metric can be cited, compared over time, and cached safely.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Topology | Node keys with their interned type plus typed edge pairs — the whole of what analytics reads; never payloads, vectors or chunks |
| Graph Revision | graph-storage's monotonic counter, incremented on any change to stored state; the cache key and the staleness signal |
| Algorithm Contract Version | An immutable version of a metric's output-affecting semantics; part of cache identity, so an old result is never served under new semantics |
| Job | One admitted computation with a durable state machine, an owner, a deadline and a lease |
| Lease | A worker's time-bounded claim on a running job, with a fencing epoch so a reclaimed job's late writes are rejected |
| Determinism Class | Per metric: exact, seeded-deterministic, or ordering-stable |
| Canonical Ordering | The fixed input ordering applied before any seeded algorithm runs; determinism comes from ordered inputs plus the seed, never from incidental iteration order |

## 2. Actors

### 2.1 Human Actors

#### Data Analyst

**ID**: `cpt-cf-graph-analytics-actor-data-analyst`

- **Description**: Requests metrics over a tenant's graph and consumes the results directly or through a UI.
- **Needs**: Deterministic numbers they can cite and compare across runs, an honest statement of which metrics are exact and which are sampled, and a job contract that survives a computation outliving an HTTP timeout.

#### Platform Administrator

**ID**: `cpt-cf-graph-analytics-actor-platform-admin`

- **Description**: Deploys, sizes and monitors the gear; grants the analytics permission.
- **Needs**: Explicit worker, memory, queue and deadline configuration; saturation telemetry before pressure becomes an incident; readiness that names the failing capability.

### 2.2 System Actors

#### Graph Storage Gear

**ID**: `cpt-cf-graph-analytics-actor-graph-storage`

- **Description**: Owns the graph, its schema and its revision. Provides the read-only topology surface this gear reads, and consumes the metrics cache this gear publishes.
- **Interaction**: Read-only database role for topology; metrics cache table written here and read there; schema version declared there and checked here.

#### AuthZ Resolver Gear

**ID**: `cpt-cf-graph-analytics-actor-authz-resolver`

- **Description**: The policy decision point for the analytics permission and job ownership.
- **Interaction**: PDP decision per request through the shared PolicyEnforcer.

#### Consumer Gear

**ID**: `cpt-cf-graph-analytics-actor-consumer-gear`

- **Description**: Another gear submitting jobs or reading results through the SDK client in ClientHub.
- **Interaction**: Same operations, same permissions and same admission limits as the REST path.

## 3. Operational Concept & Environment

### 3.1 Gear-Specific Environment Constraints

- Requires a PostgreSQL role on the graph-storage schema with `SELECT` on topology columns only and write access to the metrics cache and job tables it owns. It never migrates the graph schema.
- Requires memory budgeted for the configured topology ceiling. The gear reserves each job's estimated peak from a process-wide pool before starting it, so the budget is a real bound rather than a hope.
- Requires the graph-storage schema version it declares support for; a mismatch is a readiness failure, not a runtime error on the first job.
- Is optional in a deployment. Without it, graph-storage serves projections without metric annotations and every other capability is unaffected.
- Is unavailable when graph-storage is backed by an external store plugin, because there is no PostgreSQL schema to read. This is a declared capability, never a silent degradation.

## 4. Scope

### 4.1 In Scope

- Degree (total, in, out), PageRank and connected components over a topology-only projection
- Betweenness centrality (exact below a node-count threshold, sampled above it) and community detection with stable ordering
- Edge-type exclusion on every metric
- Per-metric determinism contracts with an immutable algorithm contract version
- Asynchronous job lifecycle: submit, status, result, cancel, with durable state and lease recovery
- Revision-keyed metrics cache with conditional single-flight publication and bounded retention
- Whole-tenant analytics permission and per-job ownership authorization
- Versioned REST API and a typed SDK client registered in ClientHub
- Structured observability and per-capability readiness

### 4.2 Out of Scope

- Storing or mutating graph data of any kind — this gear reads topology and writes only its own cache and job tables
- Resource-scoped analytics over an induced subgraph; v1 is whole-tenant only, and a constrained scope is rejected rather than widened
- Shortest-path and other traversal queries — those are graph-storage's traversal surface
- Streaming or incremental metric maintenance; a metric is recomputed for a revision, not updated in place
- Numeric parity with any prior NetworkX implementation
- Serving analytics for a graph-storage instance backed by a non-PostgreSQL store plugin

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e). Coverage has one normative threshold — the enforced floor of `cpt-cf-graph-analytics-nfr-code-coverage` (>= 85% line coverage, gated in CI). Document verification method only for non-test approaches.

### 5.1 Topology Access

#### Topology Projection Load

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-topology-load`

The system **MUST** load a tenant's topology — node keys with their interned type, and typed edge pairs with discriminator — through the read-only role, excluding tombstoned rows, and **MUST NOT** read payloads, composed search text, embeddings or chunk contents. The load **MUST** observe one snapshot together with the graph revision it reports, so a computation can never mix a topology from one revision with a revision number from another. The load **MUST** be refused before it begins when the tenant's node count, edge count or estimated byte size exceeds a configured ceiling, with an error naming the ceiling, the configured bound and the observed value.

- **Rationale**: Topology is the whole of what these algorithms need, and reading no more than that is what makes a million-node graph affordable. Refusing before loading is what keeps an oversized tenant from being discovered by an out-of-memory kill.
- **Actors**: `cpt-cf-graph-analytics-actor-graph-storage`

#### Schema Version Guard

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-schema-guard`

The system **MUST** read the graph schema version graph-storage publishes and compare it against the version it supports at startup and on reconnect. A mismatch **MUST** be reported as an unhealthy readiness state naming the expected and observed versions, and **MUST** cause job submissions to be rejected — never a best-effort read of a schema the gear does not understand.

- **Rationale**: Two gears pinned to one physical schema is the accepted cost of this design; the version check is what stops an uncoordinated migration from turning into wrong numbers instead of a stopped service.
- **Actors**: `cpt-cf-graph-analytics-actor-graph-storage`, `cpt-cf-graph-analytics-actor-platform-admin`

### 5.2 Metric Computation

#### Core Graph Metrics

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-core-metrics`

The system **MUST** compute per-node degree (total, in, out), PageRank and connected components over the loaded topology, with an option to exclude named edge types from the computation. Results **MUST** be deterministic for a given graph state, parameters and algorithm contract version.

- **Rationale**: Degree ordering drives graph-storage's projection truncation, and centrality gives analysts a structural ranking of entities; edge-type exclusion keeps hub types from dominating both.
- **Actors**: `cpt-cf-graph-analytics-actor-data-analyst`

#### Extended Graph Metrics

- [ ] `p2` - **ID**: `cpt-cf-graph-analytics-fr-extended-metrics`

The system **MUST** additionally provide betweenness centrality — exact below a configured node-count threshold, seeded and sampled above it — and community detection with community ordering stable across recomputation of the same graph. Numeric parity with any prior NetworkX implementation is explicitly not required; each algorithm's determinism class and normative contract are stated per metric.

- **Rationale**: Brokerage metrics and computed clusterings support deeper structural analysis, and clients may group a view by the community a node landed in. Community detection is an analytics *output*, not a stored grouping concept — grouping as a first-class entity is 1:N and deliberately absent from the platform model, because the same node is routinely interesting in several cuts at once; user-driven grouping is graph-storage's label system (`cpt-cf-graph-storage-fr-labels`).
- **Actors**: `cpt-cf-graph-analytics-actor-data-analyst`

#### Determinism Contracts

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-determinism`

Every metric **MUST** carry a normative contract covering the semantics that change its output — edge multiplicity and self-loop treatment, direction handling, PageRank damping, dangling-node redistribution, convergence tolerance and iteration cap, Brandes normalization and endpoint inclusion, the sampling rule above the exact threshold, and community-detection graph construction, weighting and resolution — together with its determinism class (exact, seeded-deterministic, or ordering-stable). Inputs **MUST** be canonicalized before any seeded algorithm runs: nodes ordered by key, edges by (type, source key, target key, discriminator), adjacency sorted by neighbour key, and every tie-break defined on node keys. Each contract **MUST** carry an immutable `algorithm_contract_version` that participates in cache identity, and that version **MUST** be incremented whenever an output-affecting semantic, default, sampling rule or implementation contract changes.

- **Rationale**: A seed alone does not make an algorithm repeatable when row order, hash-map iteration or adjacency layout vary between runs. And without the contract version in cache identity, a semantics change silently reinterprets every previously cached result.
- **Actors**: `cpt-cf-graph-analytics-actor-data-analyst`

### 5.3 Job Lifecycle

#### Asynchronous Job Contract

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-async-jobs`

A computation that can outlive a gateway timeout **MUST** be answered with `202 Accepted` plus a job identifier, with status, result and cancel operations against it. Job state **MUST** be durable: an accepted identifier survives process restart, and terminal transitions — including the race between cancellation and cache publication — **MUST** be single atomic updates. A running job's worker **MUST** hold a lease with a fencing epoch; an expired lease **MUST** be reclaimable, and a late write from the superseded attempt **MUST** be rejected by the epoch rather than overwrite the reclaiming worker's result. Cancellation **MUST** be cooperative and **MUST** release the job's reserved memory.

- **Rationale**: Long computations plus process restarts plus retries make the interesting failures concurrent ones; a durable state machine with fencing is what keeps two workers from publishing contradictory results for one job.
- **Actors**: `cpt-cf-graph-analytics-actor-data-analyst`, `cpt-cf-graph-analytics-actor-consumer-gear`

#### Admission and Fair Scheduling

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-scheduling`

Job admission **MUST** enforce, in order: the caller's permission, the topology ceilings, the per-tenant running and queued limits, and a reservation of the job's estimated peak memory from a process-wide pool. A job that cannot reserve **MUST** queue rather than start, and a full queue **MUST** be rejected with `resource_exhausted` and a retry hint. Reserved memory **MUST** be released on success, failure, cancellation and lease expiry alike. Jobs **MUST** deduplicate on (tenant, graph revision, metric, parameters, authorization-scope identity, contract version): a duplicate submission joins the in-flight job rather than starting a second one, and a job superseded by a newer revision **MUST** be cancelled cooperatively.

- **Rationale**: Per-tenant concurrency alone cannot bound the sum across tenants, and an unbounded queue converts a load spike into an out-of-memory kill instead of a rejection. Deduplication matters because the natural client behaviour — poll, time out, resubmit — otherwise multiplies the most expensive operation the platform has.
- **Actors**: `cpt-cf-graph-analytics-actor-platform-admin`, `cpt-cf-graph-analytics-actor-data-analyst`

### 5.4 Caching

#### Revision-Keyed Metrics Cache

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-metrics-cache`

The system **MUST** cache computed metrics keyed by tenant, graph revision, metric, canonicalized parameters and algorithm contract version; **MUST** serve a cached result while that key still matches; and **MUST** report per metric whether it was served from cache or computed. Publication **MUST** be conditional and single-flight: a result **MUST NOT** be published if the graph revision moved during computation, and concurrent identical computations **MUST** collapse to one publication. Cache growth **MUST** be bounded by configured retention — entry size, entries per tenant, retained revisions, and parameter variants per metric — enforced by publication checks and a background cleanup that never removes an in-flight publication.

- **Rationale**: Whole-graph analytics is expensive, and revision keying makes correctness trivial rather than heuristic — an entry is either for the current graph or it is not. The conditional publish is what stops a long job from writing a result for a graph that no longer exists.
- **Actors**: `cpt-cf-graph-analytics-actor-data-analyst`, `cpt-cf-graph-analytics-actor-graph-storage`

### 5.5 Multi-Tenancy and Access Control

#### Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-tenant-isolation`

Every topology read, cache read, cache write and job row **MUST** be tenant-scoped at the database query layer through the platform's secure ORM, and no in-memory topology **MUST** ever contain nodes or edges from more than one tenant. There **MUST** be no unscoped query path in the codebase.

- **Rationale**: An in-memory graph is the one place where a missing predicate produces a merged result instead of an error, so scoping has to be enforced at the query layer and asserted adversarially rather than reviewed.
- **Actors**: `cpt-cf-graph-analytics-actor-graph-storage`

#### Operation-Level Access Control

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-access-control`

Every operation **MUST** be authenticated and authorized through the platform policy decision point, resolved per request and never short-circuited by local state. Whole-graph computation **MUST** require the dedicated whole-tenant analytics permission: a caller whose access scope is constrained to a subset of resources **MUST** be rejected rather than served tenant-wide numbers. Status, result and cancel **MUST** re-authorize against the job's ownership tuple (tenant, principal) on every call, and an unknown job identifier **MUST** be indistinguishable from one the caller may not see. Enforcement **MUST** be identical for the REST and in-process paths through a shared policy-enforcement layer.

- **Rationale**: A whole-graph metric aggregates every node in the tenant, so serving it to a caller scoped to part of the graph leaks structure they cannot read. Making unknown and unauthorized indistinguishable is what stops job identifiers from becoming an enumeration oracle.
- **Actors**: `cpt-cf-graph-analytics-actor-authz-resolver`

### 5.6 API Surfaces

#### Versioned REST API

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-rest-api`

The system **MUST** expose a versioned REST surface under `/api/graph-analytics/v1` covering job submission, status, result, cancellation, metric metadata (determinism class and contract version per metric) and readiness, with OpenAPI schemas, RFC-9457 problem details, and every enforced limit documented on the operation that enforces it.

- **Rationale**: The determinism class and contract version are part of what a consumer needs to interpret a number, so they belong in the API rather than in prose.
- **Actors**: `cpt-cf-graph-analytics-actor-data-analyst`

#### Typed SDK Client

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-sdk-client`

The system **MUST** ship an SDK crate with a `GraphAnalyticsClientV1` trait registered in ClientHub, mirroring the REST operations with transport-agnostic models and canonical platform errors. Behaviour **MUST** be identical to REST — same permission checks, same admission limits, same job contract — enforced in the shared domain layer rather than duplicated per adapter.

- **Rationale**: graph-storage reads the cache directly, but any other consumer submitting work needs a typed path, and a second adapter is where limits historically diverge.
- **Actors**: `cpt-cf-graph-analytics-actor-consumer-gear`

### 5.7 Observability and Readiness

#### Structured Observability

- [ ] `p2` - **ID**: `cpt-cf-graph-analytics-fr-observability`

The system **MUST** emit structured tracing for topology load, computation and publication (node and edge counts, load duration, iteration counts, computation duration, cache hit or miss, memory reserved and peak) and expose operational metrics including a saturation counter for every enforced limit and a high-watermark gauge for the memory pool and queue. Telemetry **MUST** be deny-by-default for content: node keys, type identifiers and metric values **MUST NOT** appear in logs, spans, metrics or error attributes — only counts, sizes, durations, bounded enums, graph revision and opaque correlation identifiers.

- **Rationale**: Capacity pressure on a shared memory pool has to be visible before it becomes an incident, and node keys are tenant data even when the metric over them is not.
- **Actors**: `cpt-cf-graph-analytics-actor-platform-admin`

#### Readiness Reporting

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-fr-readiness`

The system **MUST** report readiness per capability — database connectivity, graph schema version, policy registry, worker pool and lease recovery — as healthy, degraded or unhealthy with named problems. Recovery of expired running leases **MUST** complete before workers report ready. A degraded capability **MUST** reject exactly the affected operations with canonical errors while unrelated operations continue, and **MUST NOT** silently widen behaviour.

- **Rationale**: A worker pool that accepts jobs before reclaiming abandoned leases will run two attempts of the same job, which the fencing epoch then has to resolve; reporting ready only after recovery avoids creating the race in the first place.
- **Actors**: `cpt-cf-graph-analytics-actor-platform-admin`

## 6. Non-Functional Requirements

### 6.1 Gear-Specific NFRs

#### Analytics Memory Bound

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-nfr-analytics-memory`

Computation **MUST** operate within configurable node, edge and memory-budget ceilings, refusing beyond any of them with a clear error, and **MUST** hold at most the graph topology — keys and typed edge pairs, never payloads or vectors — in memory. A node count alone does not bound memory on dense graphs, so all three ceilings are enforced independently.

- **Threshold**: Defaults 1,000,000 nodes / 10,000,000 edges / 2 GiB estimated topology budget per job; process-wide pool default 4 GiB. Topology-only footprint verified by profiling tests
- **Rationale**: An in-memory graph over an unbounded tenant is the dominant memory risk of this gear, and the estimate-and-reserve model only works if the estimate is bounded on every axis that drives it.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Interactive Path Isolation

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-nfr-interactive-isolation`

Analytics **MUST NOT** be able to exhaust graph-storage's connection pool, memory or CPU. The gear runs as its own process with its own connection pool and memory budget, and its database role **MUST** be unable to write any graph table or read any non-topology column.

- **Threshold**: Verified structurally, not by load test: separate deployment unit and connection pool; an integration test asserts that an attempted write and a `SELECT` of a payload or embedding column both fail on the analytics role
- **Rationale**: This is the requirement the whole gear exists for. Expressing it as a grant and a process boundary makes it hold by construction, where the previous in-process design could only ask the code to behave.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Zero Cross-Tenant Leakage

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-nfr-tenant-zero-leak`

No topology load, cache read or job query **MUST** ever return data from a tenant other than the caller's, including under concurrent multi-tenant load and under lease reclaim.

- **Threshold**: Adversarial multi-tenant integration tests on every read path, plus a test that a reclaimed job cannot publish into another tenant's cache
- **Rationale**: The in-memory projection is the one structure in the platform that could merge two tenants' graphs without any query failing.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Code Coverage

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-nfr-code-coverage`

The gear **MUST** maintain at least 85% line coverage across its library crates.

- **Threshold**: >= 85% line coverage, enforced in CI
- **Rationale**: The determinism contracts and the job state machine carry the correctness risk of this gear and must stay tested as they evolve.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

### 6.2 NFR Exclusions

- Wall-clock latency targets for computation: a job's duration is a function of tenant graph size and the chosen metric, not of the gear's design, and the job contract exists precisely because it cannot be bounded usefully. What is bounded and enforced is the deadline, beyond which the job terminates rather than running longer.
- High-availability clustering of the gear itself: the gear is stateless above PostgreSQL, and lease recovery already covers the restart case; multi-instance scheduling fairness is deferred until platform guidance requires it.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Graph Analytics REST API

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-interface-rest-api`

- **Type**: REST API
- **Stability**: unstable (v1 during incubation)
- **Description**: Versioned HTTP surface for job submission, status, result, cancellation, metric metadata and readiness. The endpoint table and versioning policy are normative in DESIGN § 3.3.
- **Breaking Change Policy**: Path-versioned; breaking changes require a new version prefix. A change to a metric's semantics is not an API break — it is an `algorithm_contract_version` increment, which consumers can detect on the result.

#### Graph Analytics SDK Client

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-interface-sdk-client`

- **Type**: Rust trait (ClientHub client) in the SDK crate
- **Stability**: unstable (v1 during incubation)
- **Description**: Typed async client trait mirroring the REST capabilities for in-process gear-to-gear calls, with transport-agnostic models and canonical errors. Behavioural parity with REST is a contract requirement asserted in the suite, not a convention.
- **Breaking Change Policy**: Versioned trait names (`...ClientV1`); breaking changes introduce a new trait version.

### 7.2 External Integration Contracts

#### Graph Topology Read Contract

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-contract-topology-read`

- **Direction**: required from graph-storage
- **Protocol/Format**: A PostgreSQL role on the graph-storage schema with `SELECT` limited to node keys with their interned type, typed edge pairs with discriminator, and the type table — all excluding tombstoned rows — plus the tenant's graph revision and the declared schema version. No write access to any graph table.
- **Compatibility**: The schema version is declared by graph-storage and checked here; a mismatch stops the gear rather than degrading it. The contract is unavailable when graph-storage runs on a non-PostgreSQL store plugin, and this gear reports that capability as unavailable rather than approximating it.

#### Metrics Cache Publication Contract

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-contract-metrics-cache`

- **Direction**: provided to graph-storage
- **Protocol/Format**: A cache table keyed by tenant, graph revision, metric, canonicalized parameters and algorithm contract version, carrying per-node values and the computation timestamp. This gear is its only writer; graph-storage reads it to annotate projections.
- **Compatibility**: An annotation is served only from an entry matching the revision the reading query observed. A missing entry is not an error for the reader — graph-storage returns the projection unannotated and says so — so adding a metric or bumping a contract version never breaks a consumer, it only widens or empties what is available.

## 8. Use Cases

#### Rank Entities by Structural Importance

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-usecase-rank-entities`

**Actor**: `cpt-cf-graph-analytics-actor-data-analyst`

**Main flow**: The analyst submits PageRank and degree over the tenant graph, excluding one hub edge type. The request is authorized, admitted against the ceilings, and accepted with `202` and a job identifier. A worker loads the topology at revision R, canonicalizes it, computes both metrics, and publishes them keyed by R. The analyst polls the job, receives the result, and each metric reports whether it was computed or served from cache.

**Alternative flows**: A second identical submission while the first is running joins it rather than starting a second computation. If the graph changes to R+1 mid-computation, publication is skipped and the job reports superseded rather than writing a result for a graph that no longer exists.

#### Annotate a Neighborhood View

- [ ] `p1` - **ID**: `cpt-cf-graph-analytics-usecase-annotate-projection`

**Actor**: `cpt-cf-graph-analytics-actor-graph-storage`

**Main flow**: graph-storage serves a neighborhood projection and reads degree values from the cache for the revision its own query observed, returning them as per-node annotations.

**Alternative flows**: No entry at that revision, or this gear not deployed at all — the projection is returned without annotations and says so. It is never an error, and a stale-revision annotation is never substituted.

#### Cluster a Graph for a Visual Cut

- [ ] `p2` - **ID**: `cpt-cf-graph-analytics-usecase-communities`

**Actor**: `cpt-cf-graph-analytics-actor-data-analyst`

**Main flow**: The analyst requests community detection. The result is a computed clustering with stable community ordering, which a client may render as a grouping of its own choosing. Recomputing over the same graph yields the same communities in the same order.

**Alternative flows**: The tenant graph exceeds the node ceiling — the job is refused at admission with the ceiling, bound and observed value, rather than accepted and later killed.

## 9. Acceptance Criteria

- Golden tests on fixed small graphs assert exact values for degree, components and PageRank (within the stated tolerance) and stability for betweenness and communities, including runs with deliberately shuffled input row order that must produce identical output.
- An integration test asserts the analytics database role cannot write any graph table and cannot select a payload or embedding column.
- A schema-version mismatch produces an unhealthy readiness report naming both versions, and no job is accepted.
- Profiling tests confirm the in-memory footprint is topology-only and that every ceiling refuses before allocation rather than during it.
- Concurrency tests cover the durable job machine: restart mid-computation, lease expiry and reclaim, a fenced late write from the superseded attempt, and the cancellation-versus-publication race.
- A job whose graph revision moves mid-computation publishes nothing and reports superseded.
- Adversarial multi-tenant tests find no cross-tenant data on any read path.
- REST and SDK paths are asserted to enforce identical permissions and identical admission limits.

## 10. Dependencies

| Dependency | Purpose | Criticality |
|---|---|---|
| graph-storage gear | Owns the graph, its schema, its revision and the DDL; provides the topology role | Blocking — the gear has nothing to read without it |
| PostgreSQL | The graph-storage schema, read through the analytics role | Blocking |
| authz-resolver gear | PDP decisions for the analytics permission and job ownership | Blocking |
| types-registry gear | Permission instances registered as GTS instances | Blocking at startup |
| Rust graph and algorithm crates (petgraph-family) | Algorithm implementations behind the metric contracts | Blocking for extended metrics |
| Task Engine gear ([PR #4545](https://github.com/constructorfabric/gears-rust/pull/4545)) | Target owner of the job control plane — lifecycle, claim/lease, queue dispatch, retry, history and the management API. This gear ships its own job table first and migrates once the Task Engine lands and its fencing and cross-store terminal-transition contracts are settled (ADR-0003 § Migration to the platform Task Engine) | Not blocking — adoption path, not a prerequisite |

## 11. Assumptions

- Tenant graphs of interest fit the configured ceilings; a graph beyond them forgoes analytics while keeping every graph-storage capability.
- The graph revision is a reliable staleness signal — graph-storage increments it on any change to stored state and only then, which its own requirement guarantees.
- Deployments that run this gear also run graph-storage on PostgreSQL rather than on an external store plugin.
- Recomputation on change is acceptable; no consumer needs incrementally maintained metrics in v1.

## 12. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| An uncoordinated graph-storage schema migration | Wrong numbers, or a crash mid-job | Declared schema version checked at readiness and on reconnect; job submissions rejected on mismatch |
| A tenant graph grows past the ceilings | Analytics silently stops being available for that tenant | Refusal names the ceiling and the observed value; saturation counters make the approach visible before the wall |
| Estimated peak memory underestimates actual | Pool over-commits and the process is killed | Estimate from node and edge counts plus key sizes, reserved before start; allocation tracking during the run terminates the job rather than the process |
| A metric's semantics change without a version bump | Old cached results reinterpreted under new meaning | `algorithm_contract_version` is part of cache identity, and every version is covered by golden fixtures |
| Client polls, times out and resubmits | The most expensive operation in the platform multiplied | Deduplication on the full job identity; a duplicate joins the in-flight job |

## 13. Open Questions

- When does the job control plane move to the Task Engine, and does its lease carry a fencing token that rejects a superseded worker's write? A terminal transition that must commit together with a metrics-cache row in this gear's database becomes a two-store commit once the task row lives elsewhere; the likely shape is a local terminal record as the publication authority reporting outcome to the Task Engine, but it needs agreeing. Owner: Task Engine and this gear jointly. **Does not block implementation** — the local job table is the v1 authority and the migration is additive.

- Should resource-scoped analytics over an induced authorized subgraph be supported, and if so what goes into the cache identity? A normalized scope fingerprint is the obvious candidate, but two scopes that differ textually and coincide semantically would then miss each other's cache entries. Owner: this gear plus authorization; not blocking v1, which rejects constrained scopes outright.
- Do any consumers need incrementally maintained metrics rather than recomputation per revision? Incremental PageRank and community maintenance are well studied but change the determinism story completely, so this needs a concrete consumer before it is considered.
- Which crate implements community detection, and does the platform want Leiden rather than Louvain? The API contract names guarantees rather than libraries, so this is contained, but the choice fixes the first `algorithm_contract_version` and changing it later invalidates every cached community result.
- Should the gear support more than one instance per deployment, and if so how is the process-wide memory pool coordinated across them? Single-instance is assumed in v1.

## 14. Traceability

Links to related specification artifacts.

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/)
- **Gear boundary**: [ADR-0002](./ADR/0002-cpt-cf-graph-analytics-adr-own-gear-boundary.md), which makes this computation its own deployment unit
