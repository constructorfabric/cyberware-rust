---
status: accepted
date: 2026-08-24
decision-makers: Graph Storage design review
---

# ADR-0002: Whole-graph analytics ships as its own gear with a read-only connection to the graph schema

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Boundary Rules](#boundary-rules)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A. Analytics stays inside graph-storage](#a-analytics-stays-inside-graph-storage)
  - [B. Separate gear with a read-only connection to the graph schema](#b-separate-gear-with-a-read-only-connection-to-the-graph-schema)
  - [C. Separate gear reading topology over the SDK](#c-separate-gear-reading-topology-over-the-sdk)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-graph-analytics-adr-own-gear-boundary`

## Context and Problem Statement

ADR-0001 settled *what* analytics is — in-process Rust over a topology-only
projection, with per-metric determinism contracts and no NetworkX parity — and
placed it in the graph-storage runtime. That placement is a separate question,
and the review raised it: whole-graph analytics and interactive graph queries
have opposite resource profiles. A metrics job holds a million-node projection
in memory for minutes; an ingest batch or a search request wants a connection
and a few milliseconds. Sharing one runtime means one deployment's memory,
worker and connection budget must satisfy both, and the failure mode is
one-directional — a loop of accidental analytics jobs degrades ingest, while no
volume of ingest degrades analytics in the same way.

ADR-0001 acknowledged this only as an obligation on the implementation
("long computations must be cooperatively cancellable and must not starve
request handling"). An obligation is not an isolation boundary.

## Decision Drivers

- Operators need to bound analytics CPU, memory and connections independently of
  the interactive path, and today cannot: both live in one process.
- Deployments that never call analytics still carry its scheduler, memory pool
  and worker configuration, and still pay its readiness surface.
- `cpt-cf-graph-storage-nfr-analytics-memory` bounds analytics to a topology-only
  projection; that projection needs node keys and typed edge pairs and nothing
  else — no payloads, no vectors, no chunks. The read surface it needs is narrow
  and stable.
- The metrics cache is keyed by graph revision, so the coupling between the two
  gears is one integer plus a topology read, not a data-flow.
- ADR-0001 rejected option A (a Python sidecar) partly because "the full graph
  crosses a process boundary on every recomputation". That objection is about
  serializing the graph over an API, not about running in a separate process, and
  it therefore rules out option C below rather than this decision.
- ADR-0001's single-source-of-truth principle must survive: whatever reads the
  graph must not become a second writer of graph state.

## Considered Options

- A. Analytics stays inside graph-storage, isolated only by the global scheduler and memory pool
- B. A separate `graph-analytics` gear with a read-only database role on the graph-storage schema
- C. A separate gear that reads topology through the graph-storage SDK

## Decision Outcome

Chosen option: "B. A separate `graph-analytics` gear with a read-only database
role", because it gives operators real CPU, memory and connection isolation
without reintroducing the whole-graph-over-an-API cost that ADR-0001 rejected,
and because the read surface analytics needs — node keys and typed edge pairs —
is narrow enough to expose as a stable, read-only projection rather than as a
general database grant.

### Boundary Rules

1. **graph-storage owns all DDL.** The analytics gear never migrates the schema.
   It declares the schema version it requires and reports a version mismatch as
   a readiness failure, not as a runtime error on the first job.
2. **The read-only role can see topology and nothing else.** `SELECT` on `node`
   (tenant, id, node_key, type reference, tombstone), `edge` (tenant, id, type
   reference, endpoints, discriminator, tombstone) and `gts_type`. No payload,
   no `search_text`, no embeddings, no chunks. The grant is the enforcement of
   ADR-0001's topology-only rule, not a convention the code is trusted to follow.
3. **The metrics cache table is owned by the analytics gear.** It is the one
   table analytics writes. graph-storage reads it to annotate projections and
   never writes it, so the single-writer property holds per table and the
   read-only role stays read-only with respect to graph state.
4. **Analytics is unavailable when the graph is served by an external
   graph-engine plugin.** A plugin store has no PostgreSQL schema to read. This
   is a declared capability that reports unavailable, never a silent degradation
   to a stale or partial result.
5. **The whole-tenant analytics permission and the job ownership tuple move with
   the gear**, and the corresponding rows leave the graph-storage authorization
   matrix. The analytics gear runs the same shared-PEP pattern; nothing about the
   enforcement model changes, only which gear declares it.
6. **The graph revision remains graph-storage's.** Analytics reads it, keys its
   cache by it, and never writes it. A job superseded by a newer revision is
   cancelled cooperatively, exactly as before.

The determinism contracts, canonical input ordering, algorithm set,
`algorithm_contract_version` and cache identity defined by ADR-0001 are
unchanged and move with the component. This ADR narrows ADR-0001: "analytics in
Rust" stands, "in the graph-storage runtime" is superseded here.

### Consequences

- A new gear needs its own PRD, DESIGN and deployment unit, with its own
  timeout, worker-count, memory and connection-pool configuration. That is the
  cost of this decision and it is not small.
- The DoS coupling ADR-0001 accepted as an implementation obligation stops being
  one: analytics cannot exhaust the interactive path's connection pool or its
  memory, because it does not share either.
- Deployments that do not need analytics do not run it, and graph-storage's
  readiness surface loses the analytics-worker capability.
- Two gears are pinned to one physical schema. That is a real coupling and rule 1
  is what keeps it honest: a schema change is a coordinated release, and the
  version declaration makes an uncoordinated one fail closed at readiness rather
  than at the first job.
- Analytics loses the ability to read uncommitted or same-transaction state — it
  never used it, since it reads a committed snapshot at a known revision by
  construction.

### Confirmation

- The read-only role's grants are asserted by an integration test: an attempted
  write, and a `SELECT` of a payload or embedding column, both fail.
- A schema-version mismatch is asserted to surface as an unhealthy readiness
  report naming the expected and actual versions, with no job accepted.
- The graph-storage contract suite asserts that metric annotation degrades to
  "no annotations" rather than an error when the analytics gear is absent.
- The golden determinism tests of ADR-0001 run unchanged in the new gear,
  including the shuffled-input-order cases.

## Pros and Cons of the Options

### A. Analytics stays inside graph-storage

- Good, because there is one deployment unit and no schema-version coordination.
- Good, because it needs no new documentation set.
- Bad, because CPU, memory and connections cannot be bounded independently, so an
  accidental analytics loop degrades ingest and search.
- Bad, because every deployment carries the scheduler, memory pool and worker
  configuration whether or not it computes a single metric.
- Bad, because isolation rests on an obligation ("must not starve request
  handling") that nothing enforces.

### B. Separate gear with a read-only connection to the graph schema

- Good, because resource isolation is a property of the deployment, not of the
  code's good behaviour.
- Good, because the read-only grant enforces ADR-0001's topology-only rule
  mechanically.
- Good, because the topology never crosses a process boundary as serialized API
  data — it is read from the same PostgreSQL instance.
- Bad, because two gears are pinned to one schema and one release cadence.
- Bad, because it is unavailable when the store is an external plugin.

### C. Separate gear reading topology over the SDK

- Good, because there is no shared schema and no version coupling at all.
- Good, because it works regardless of which store backs graph-storage.
- Bad, because a million-node topology is serialized, transferred and
  deserialized on every recomputation — the exact objection ADR-0001 used to
  reject the Python sidecar, reappearing over HTTP.
- Bad, because pagination over a topology read has to hold a consistent snapshot
  across many requests, which the Read Consistency Contract does not offer to
  external callers.

## More Information

- ADR-0001 (`cpt-cf-graph-analytics-adr-rust-determinism`) — what analytics is; narrowed by this ADR on where it runs.
- graph-storage ADR-0001 (`cpt-cf-graph-storage-adr-single-postgres-store`) — single source of truth; rule 3 preserves single-writer-per-table.

## Traceability

- **PRD**: `cpt-cf-graph-storage-fr-analytics-topology`, `cpt-cf-graph-storage-fr-metric-annotation`, `cpt-cf-graph-storage-fr-revision-signal`, `cpt-cf-graph-storage-nfr-analytics-memory`. The metric-computation requirements this ADR moves out take `cpt-cf-graph-analytics-*` identifiers in the new gear's PRD.
- **DESIGN**: § 3.2 Component Model (Graph Analytics — moved), § Capacity and Admission Contract
