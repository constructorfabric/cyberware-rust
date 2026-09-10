---
status: accepted
date: 2026-08-13
decision-makers: Graph Storage design review
---

# ADR-0002: One typed node model unifies owned entities and managed-object references

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A. Separate storage and APIs per family](#a-separate-storage-and-apis-per-family)
  - [B. Full replication of managed objects](#b-full-replication-of-managed-objects)
  - [C. Single typed node table with GTS family semantics](#c-single-typed-node-table-with-gts-family-semantics)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-graph-storage-adr-unified-node-model`

## Context and Problem Statement

The graph must hold two families of members. Owned entities — such as Finding nodes produced by analysis runs — originate in the graph; it is their system of record. Managed objects — commits, pull requests, review comments — are owned by other gears or external systems and only participate in the graph as relationship endpoints. The prototype stored only one family (artifacts). The gear must generalize: how are these families modeled so that every query capability works across both, producers stay decoupled, and the graph never competes with upstream systems of record?

## Decision Drivers

- `cpt-cf-graph-storage-fr-reference-nodes` requires search, traversal, projection, and analytics to treat both families uniformly.
- The platform value proposition is connecting new analysis entities to existing managed objects; a fragmented model fragments every query.
- Upstream gears (e.g., a repository mirror) already store managed objects completely; duplicating them creates unbounded storage growth and staleness.
- `cpt-cf-graph-storage-fr-edge-provenance` and `cpt-cf-graph-storage-fr-scope-replace` require deterministic re-sync that preserves analysis conclusions across source re-imports.
- GTS derivation already provides typed families with shared base schemas — the natural mechanism for expressing "kinds of nodes" without storage-level special cases.
- The prototype's phantom-node mechanism proved that out-of-order ingest across producers needs first-class dangling-reference handling.

## Considered Options

- A. Separate storage and APIs per family (entity tables for owned nodes, a link table for external references)
- B. Full replication: copy managed objects into the graph as complete records
- C. Single typed node table; family semantics expressed by GTS base types (owned vs. reference), reference payloads carrying canonical upstream identifiers plus a bounded queryable projection

## Decision Outcome

Chosen option: "C. Single typed node table with GTS-expressed family semantics", because it keeps one uniform storage and query model while the type system — not the storage engine — carries the semantic distinction. The gear publishes GTS base types: an owned-node base, a reference-node base, a phantom type, and a provenance attribute type for analysis-originated content. Producers derive their domain types (Finding from the owned base; commit, pull request, and comment from the reference base). The reference-node base schema requires a **source-qualified canonical identity** — the owning source (gear or external system identifier), the object kind, and the native identifier — because a native identifier alone is not collision-safe: two upstream systems can expose the same native ID within one tenant. Reference payloads hold only what queries need: that identity triple and a small projection of searchable and filterable attributes.

### Consequences

- The gear must define and publish the GTS base ontology (owned base, reference base, phantom, provenance) as versioned platform contracts; every producer derives from it.
- Reference-node projections duplicate a bounded slice of upstream data by design; staleness is accepted and bounded by producer re-sync cadence — the graph answers "what is connected", the upstream gear answers "what is the current full state".
- Consumers resolving a reference node's full record must call the owning gear using the canonical identifier; the graph API never proxies upstream reads.
- Edge semantics split along the same seam: static edges are recomputed on scope replacement, analysis edges carry provenance and survive it — so re-mirroring a repository never erases Finding conclusions.
- Phantom nodes make cross-producer ordering a non-problem: a Finding batch may reference a commit before the mirror producer has pushed it; the phantom is replaced in place later. That replacement is governed by an explicit atomic transition contract (DESIGN § Phantom Materialization Contract): identity and edges preserved, incident edges revalidated against the concrete type's endpoint constraints, batch-level rejection on violation, deterministic resolution under concurrent ingests.
- Uniqueness of node keys per tenant makes the node-key scheme a shared producer convention that DESIGN must specify. For reference nodes the key is derived deterministically from the full identity triple (source, object kind, native identifier), never from the native identifier alone. Collision behavior is therefore defined by construction: the same triple ingested by any producer converges onto the same node (idempotent upsert), while identical native identifiers from different sources remain distinct nodes.

- A source namespace is an ownership boundary, not a payload field. The identity triple makes two producers' references to the same upstream object converge deliberately, but it would equally let a producer submit another source's triple under a generic write permission and overwrite the projection its owner maintains. Namespaces are therefore bound to authenticated producer principals, owner provenance is immutable from first creation, every update and phantom materialization re-authorizes against that owner, and transfer is an explicit audited administrative flow (DESIGN § Authorization Model). An unclaimed namespace is claimed by its first writer, so single-producer deployments need no setup while the second writer remains an authorization decision rather than a silent merge.

### Confirmation

- Contract tests validate the published base types: a payload lacking the canonical identifier fails reference-node validation; provenance-free analysis edges are rejected.
- Integration tests run the Finding scenario end-to-end: ingest findings + references + analysis edges, re-sync the static scope, and assert findings and their edges survive (`cpt-cf-graph-storage-usecase-finding-ingest`).
- Search, traversal, and projection tests assert identical behavior over owned and reference nodes.

## Pros and Cons of the Options

### A. Separate storage and APIs per family

Owned entities in first-class tables; external references in a dedicated link/registry table with its own endpoints.

- Good, because each family gets a storage shape tuned to its needs.
- Good, because "the graph is not a replica" is structurally enforced.
- Bad, because every query capability (search, traversal, projection, analytics) must be implemented and maintained twice, then joined.
- Bad, because edges spanning families need cross-table polymorphism, the exact complexity a graph model exists to avoid.
- Bad, because adding a third family (e.g., external documents) would multiply API surface again.

### B. Full replication of managed objects

Copy complete upstream records into graph nodes so the graph is self-sufficient.

- Good, because consumers get full records from one API.
- Good, because queries never depend on upstream availability.
- Bad, because it turns the graph into a competing system of record with unbounded storage growth and permanent staleness reconciliation.
- Bad, because heavy upstream fields defeat the metadata-only payload discipline (see ADR-0003).
- Bad, because upstream schema changes cascade into graph ontology changes even when relationships did not change.

### C. Single typed node table with GTS family semantics

One node table; owned/reference distinction lives in the GTS type hierarchy; reference payloads carry canonical identifiers plus a bounded projection.

- Good, because every capability works over both families with no special cases.
- Good, because GTS derivation lets any producer add domain types without storage or API changes.
- Good, because bounded projections keep storage and staleness bounded while still serving search and filters.
- Good, because the type system documents family semantics as reviewable contracts rather than implementation convention.
- Neutral, because "what belongs in a projection" becomes an ontology-design judgment per reference type.
- Bad, because consumers needing full upstream records must make a second call to the owning gear.

## More Information

The prototype's ontology (18 node types, 15 edge types, abstract document base, phantom type, static/analysis edge split with provenance-gated replacement) is the working example this generalization is derived from; the prototype's `replace_scope` semantics carry into `cpt-cf-graph-storage-fr-scope-replace`.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses:

- `cpt-cf-graph-storage-fr-reference-nodes` — the unified model is this requirement's design
- `cpt-cf-graph-storage-fr-edge-provenance` — static/analysis split and provenance schema defined by the base ontology
- `cpt-cf-graph-storage-fr-scope-replace` — replacement semantics keyed to the same family distinction
- `cpt-cf-graph-storage-fr-phantom-nodes` — phantom type is part of the published base ontology
- `cpt-cf-graph-storage-contract-gts-ontology` — the published base types are the contract this ADR mandates
