# PRD — Graph Storage

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
  - [5.1 Graph Type Management](#51-graph-type-management)
  - [5.2 Node and Edge Ingest](#52-node-and-edge-ingest)
  - [5.3 Content Handling](#53-content-handling)
  - [5.4 Vectorization](#54-vectorization)
  - [5.5 Search](#55-search)
  - [5.6 Graph Traversal and Projection](#56-graph-traversal-and-projection)
  - [5.7 Graph Analytics](#57-graph-analytics)
  - [5.8 Multi-Tenancy and Access Control](#58-multi-tenancy-and-access-control)
  - [5.9 API Surfaces](#59-api-surfaces)
  - [5.10 Observability and Readiness](#510-observability-and-readiness)
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

**Graph Storage** is a platform gear that stores, indexes, and serves a typed, multi-tenant knowledge graph. Producer gears push typed nodes and edges into the graph; consumer gears and user interfaces query it through lexical search, vector similarity search, hybrid fusion, depth-limited graph traversal, tabular projections, and graph analytics. Every node, edge, and payload is typed and validated through GTS (Global Type System) contracts, so independently developed gears can share one graph without schema drift.

The gear generalizes the `studio-graph-storage` prototype into a reusable platform component: it is not specific to Constructor Studio artifacts, code findings, or any single producer. Any domain that can express its entities as typed nodes and relationships — findings over a codebase, artifacts and their traceability, git objects such as commits, pull requests, and comments — is stored and queried the same way.

### 1.2 Background / Problem Statement

Several platform initiatives need to persist and query relationships between heterogeneous entities:

1. **Analysis pipelines produce graph-shaped results.** Code-analysis flows create Finding records that reference commits, pull requests, files, and each other. Today each pipeline would have to invent its own relationship storage, its own search index, and its own traversal queries.

2. **Entities live in different systems of record.** Some graph members are owned by the graph (a Finding created by an analysis run exists nowhere else), while others — commits, pull requests, review comments — are managed objects owned by other gears such as GitHub Mirror. A shared graph must represent both without duplicating the upstream stores.

3. **One search mode is never enough.** Practical exploration scenarios established during prototyping require all three retrieval modes over the same data: full-text search, vector similarity over embeddings, and structural graph traversal — plus their combination (narrow candidates with hybrid search, then expand structurally).

4. **The prototype is not multi-tenant and not a gear.** The `studio-graph-storage` prototype (Python, FastAPI, PostgreSQL with pgvector and Apache AGE, NetworkX) proved the data model, the GTS-typed ontology, hybrid retrieval, and depth-limited traversal, but has no tenancy, no access control, no pagination, unbatched writes, and Python-only dependencies. This PRD defines the productized Rust gear that replaces it.

### 1.3 Goals (Business Outcomes)

- Provide one reusable graph storage service for the platform so that new graph-shaped features (findings, traceability, dependency maps) do not each build bespoke relationship storage
- Let independently developed producer gears share a single typed graph safely, with GTS contracts validating every node and edge payload at the storage boundary
- Serve the retrieval scenarios validated in prototyping — hybrid text/vector narrowing, criteria-based tabular projection, bulk traversal with filtering, and bounded neighborhood exploration from a UI — from a single API
- Keep the graph lean and fast by storing only searchable, indexable, and vectorizable metadata in the graph while heavy content lives in external blob storage referenced by identifier
- Meet platform standards for multi-tenancy, access control, observability, and contract validation so the gear can be operated like any other CF/Gears component

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Node | A typed graph vertex with a stable producer-supplied key, a GTS-validated JSON payload, optional searchable text, and optional embedding |
| Edge | A typed, directed relationship between two nodes with a GTS-validated payload and a deterministic key |
| Node Key | A producer-supplied stable identity string for a node (unique per tenant); repeated ingests of the same key update the same node |
| Owned Node | A node whose content originates in the graph itself — the graph is its system of record (e.g., a Finding produced by an analysis run) |
| Reference Node | A node that represents a managed object owned by another system of record (e.g., a commit or pull request mirrored by another gear); its payload carries the canonical identifier and a queryable projection, never the full upstream record |
| Finding | An analysis result node type created by producer gears (e.g., a code-review conclusion); the canonical example of an owned node |
| Managed Object | An entity whose lifecycle is owned by another gear or external system (commits, pull requests, comments); appears in the graph as a reference node |
| Phantom Node | A placeholder node materialized when an ingested edge references a node key that no producer has defined yet |
| Static Edge | An edge derived deterministically from source data; recomputed and replaced on re-ingest of its scope |
| Analysis Edge | An edge produced by an analysis process, carrying provenance metadata; preserved across re-ingest of its scope |
| Provenance | Metadata on analysis-originated content: origin, creating actor, method, model, and confidence |
| Chunk | A deterministic fragment of long node content, individually indexed and embedded for retrieval |
| Hybrid Search | Fusion of lexical and vector search results into a single ranked list |
| RRF | Reciprocal Rank Fusion — a rank-based algorithm for merging result lists from multiple retrieval arms |
| Projection | A bounded, filterable tabular or subgraph view over graph data |
| Graph Revision | A monotonic counter bumped by every change to stored state — ingest, delete, and label attach/detach alike; used to invalidate analytics caches |
| Idempotency Key | A tenant- and producer-scoped identifier of one logical ingest request; its recorded outcome makes retries after lost responses safe |
| Scope Generation | A monotonic source revision carried by every scope-replacement snapshot; stale generations are rejected (fencing) |
| GTS | Global Type System — the platform's contract system of versioned, derivable JSON Schema types with `gts.` identifiers |
| Ontology | The set of registered GTS node, edge, and attribute types that describe one domain's graph shape |
| Heavy Content | Payload data too large or too opaque to index (article bodies, binaries, raw logs); stored in external blob storage and referenced from the graph by identifier |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by the steering committee. This section documents actors (users, systems) that interact with this gear.

### 2.1 Human Actors

#### Graph Explorer

**ID**: `cpt-cf-graph-storage-actor-graph-explorer`

- **Role**: A user who opens an entity in a UI and explores its relationships — "show me everything connected to this object within 3 hops".
- **Needs**: Fast bounded neighborhood queries, human-readable node names and types, labels they can attach and group by, and truncation that keeps the structurally important nodes.

#### Data Analyst

**ID**: `cpt-cf-graph-storage-actor-data-analyst`

- **Role**: A user who searches and slices the graph: finds candidates by text or semantic similarity, projects nodes matching criteria into tables, and reads graph metrics.
- **Needs**: Hybrid search with relevance provenance (which arm matched, which fragment), criteria-based tabular projections with filtering and pagination, and precomputed graph metrics.

#### Ontology Author

**ID**: `cpt-cf-graph-storage-actor-ontology-author`

- **Role**: A developer who designs a domain's graph ontology: node types, edge types, endpoint constraints, and which payload fields are indexed and vectorized.
- **Needs**: A GTS type registration contract with clear validation errors, schema evolution through versioning, and declarative control over indexing and vectorization behavior.

#### Platform Administrator

**ID**: `cpt-cf-graph-storage-actor-platform-admin`

- **Role**: An operator who runs the gear in a multi-tenant environment: monitors health and capacity, reviews registered ontologies, and manages tenant configuration.
- **Needs**: Readiness and health reporting, observability of ingest and query behavior, and guardrails that keep single tenants from exhausting shared resources.

### 2.2 System Actors

#### Producer Gear

**ID**: `cpt-cf-graph-storage-actor-producer-gear`

- **Role**: Any gear that pushes nodes and edges into the graph: an analysis gear creating Finding nodes, an importer publishing artifact traceability, or a mirror gear projecting managed objects (commits, pull requests, comments) as reference nodes. Producers own their ingest scopes and re-sync them idempotently.

#### Consumer Gear

**ID**: `cpt-cf-graph-storage-actor-consumer-gear`

- **Role**: Any gear that queries the graph through the SDK client or REST API: search, traversal, projections, and metrics. Consumers never write.

#### PostgreSQL with pgvector

**ID**: `cpt-cf-graph-storage-actor-postgres`

- **Role**: The single storage backend: relational tables as the source of truth, full-text search indexes, JSONB attribute indexes, and vector indexes via the pgvector extension.

#### File Storage Gear

**ID**: `cpt-cf-graph-storage-actor-file-storage`

- **Role**: The platform blob store holding heavy content (full documents, article bodies, large raw payloads) referenced from graph nodes by file identifier.

#### Types Registry Gear

**ID**: `cpt-cf-graph-storage-actor-types-registry`

- **Role**: The platform GTS registry that validates and serves compile-time-known GTS schemas and instances; the graph ontology's base types are published through it.

#### AuthZ Resolver Gear

**ID**: `cpt-cf-graph-storage-actor-authz-resolver`

- **Role**: The platform policy decision point consulted for every authenticated operation; supplies the access scope that confines queries to permitted tenants and resources.

#### Embedding Provider

**ID**: `cpt-cf-graph-storage-actor-embedding-provider`

- **Role**: The pluggable component that turns text into fixed-dimension vectors during ingest and query embedding; either an in-process model runtime or a remote inference service, selected by deployment configuration.

## 3. Operational Concept & Environment

> **Note**: Runtime, OS, architecture, lifecycle policy, and gear integration patterns are defined in this repository's foundational documents — the [architecture manifest](../../../docs/ARCHITECTURE_MANIFEST.md) and [guidelines/](../../../guidelines/). This section captures only this gear's deviations.

### 3.1 Gear-Specific Environment Constraints

- Requires PostgreSQL 16 or later with the `pgvector` extension and permission to create extensions in the gear's database; no other PostgreSQL extension is required. The SQL/PGQ graph-query backend additionally requires PostgreSQL 19: on an earlier server the gear starts on its iterative-CTE and two-query backends and reports SQL/PGQ as an unavailable capability rather than failing readiness (see ADR-0001). A deployment that wants SQL/PGQ before PostgreSQL 19 GA (expected September/October 2026) runs a pinned PG19 beta image with pgvector built from a pinned source revision
- Requires an embedding provider: either an in-process ONNX model runtime bundled with the gear or network access to a remote embedding inference endpoint, per deployment configuration
- Whole-graph analytics is a separate deployment unit (`graph-analytics`, graph-analytics ADR-0002) that reads this gear's schema over a read-only role; a deployment that installs it must budget memory for that gear's topology ceiling, and one that does not gets projections without metric annotations
- Depends on the file-storage gear when heavy-content offloading is enabled; the graph gear itself never stores blobs

## 4. Scope

### 4.1 In Scope

- Runtime registration of GTS graph ontologies: node types, edge types, and attribute types as draft-07 JSON Schemas with GTS identifiers, abstract types, and edge endpoint constraints
- Typed node and edge storage with stable producer-supplied keys, idempotent bulk upsert, batch atomicity, and GTS payload validation across the full type derivation chain
- A unified node model covering owned nodes (e.g., Finding) and reference nodes for managed objects (e.g., commits, pull requests, comments), distinguished by type metadata rather than separate storage
- Phantom node materialization for edges that reference undefined node keys
- Static versus analysis edge semantics: producer-scoped replacement re-sync that preserves analysis edges and their provenance
- Deterministic chunking of long node content; per-chunk indexing and embedding
- Heavy-content offloading to the file-storage gear with graph-side references
- Embedding pipeline with pluggable provider, embedding dimension verification, and per-request opt-out
- Lexical full-text search, vector similarity search, and hybrid search with reciprocal rank fusion; GTS type-family filtering on all search modes
- Depth-limited graph traversal from seed nodes (explicit keys or search hits) with per-hop edge-type filtering
- Bounded neighborhood projection for UI exploration with degree-ordered truncation
- Tabular projection of nodes by criteria and identifier lists using the platform OData binding, with pagination
- Annotation of projections with graph metrics read from the `graph-analytics` gear's revision-keyed cache
- Soft deletion of nodes and edges, with incident edges following the node and every read path excluding tombstoned rows
- A per-tenant label registry with runtime attach/detach on nodes and edges, label filtering in search, projection and traversal, and labels in read responses
- Optional CREATE/UPDATE/DELETE change events, off by default, declared per type and overridable by deployment configuration
- Multi-tenancy with tenant-scoped storage and queries, and platform access control on every operation
- A `GraphStoreV1` plugin surface over the whole data plane, with the built-in PostgreSQL store as the default implementation and a conformance suite that an in-memory fake also passes
- Versioned REST API and a typed Rust SDK client registered in ClientHub
- Structured logging, metrics, and readiness reporting

### 4.2 Out of Scope

- Parsing source repositories or documents into nodes and edges — producers parse; the gear only stores what is pushed to it
- Storing heavy content (article bodies, binaries, raw logs) inside the graph database
- Serving as the system of record for managed objects owned by other gears; reference nodes carry projections and canonical identifiers only
- Event-driven ingestion (subscribing to platform events to auto-sync managed objects) — ingest is push-only in v1; event-driven sync is a future consideration
- Cross-tenant or cross-graph federation queries
- A bundled visualization UI — consumers build UIs on the projection API
- Bitemporal versioning and node-level history — the graph reflects the latest ingested state; history is a future consideration
- Undelete, and a retention job that hard-deletes tombstoned rows past a configurable window — both p2; hard delete additionally has to settle cascade ordering, key reuse after purge, and vector/full-text index reconciliation, none of which need to block v1
- Whole-graph analytics computation — degree, PageRank, components, betweenness and community detection move to the `graph-analytics` gear (its ADR-0002); this gear stores the graph they read and annotates projections from their cache
- Embedding model training or fine-tuning

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e). Coverage has one normative threshold — the enforced floor of `cpt-cf-graph-storage-nfr-code-coverage` (>= 85% line coverage, gated in CI). Document verification method only for non-test approaches.

### 5.1 Graph Type Management

#### Ontology Type Registration

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-type-registration`

The system **MUST** accept runtime registration of GTS types with kind `node`, `edge`, or `attribute`, each carrying a GTS identifier and a draft-07 JSON Schema. Registration **MUST** be idempotent for byte-identical schemas, **MUST** reject re-registration of an existing identifier with a different schema (directing the caller to publish a new GTS version), and **MUST** apply each registration batch atomically. The system **MUST** derive and store the deterministic UUIDv5 for every registered GTS identifier using the platform GTS derivation so identifiers are interoperable with other gears.

- **Rationale**: Producers evolve independently; the type registry is the contract boundary that keeps one shared graph consistent across them.
- **Actors**: `cpt-cf-graph-storage-actor-ontology-author`, `cpt-cf-graph-storage-actor-producer-gear`

#### Type Semantics and Endpoint Constraints

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-type-constraints`

The system **MUST** support abstract types that cannot be instantiated directly, and edge types constrained to declared source and target node-type patterns (exact or GTS family patterns). Payload validation **MUST** walk the full GTS derivation chain: a payload is valid only if it satisfies the schema of every registered ancestor type plus the leaf type. Validation failures **MUST** report the offending type, JSON pointer path, and message for every error in the batch.

- **Rationale**: Derivation-chain validation is what makes derived types substitutable, and endpoint constraints keep edges structurally meaningful.
- **Actors**: `cpt-cf-graph-storage-actor-ontology-author`, `cpt-cf-graph-storage-actor-producer-gear`

#### Type Catalog

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-type-catalog`

The system **MUST** expose the registered ontology for inspection: list types filtered by kind and retrieve a single type with its schema, abstractness, endpoint constraints, and derived UUID.

- **Rationale**: Ontology authors and operators need to see what is registered to evolve it safely.
- **Actors**: `cpt-cf-graph-storage-actor-ontology-author`, `cpt-cf-graph-storage-actor-platform-admin`

#### Index Capacity Admission

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-index-admission`

Type registrations whose declared `index`, `full_text_search` or `vector_search` traits create index work **MUST** pass capacity admission before index intent is committed: per-tenant and deployment-wide caps on registered types and indexed paths, an estimated build footprint reserved before the intent row commits, bounded retention of old-version indexes, and a bounded tenant-fair queue for pending and running DDL and backfill work. Exhausted budget **MUST** produce a retryable rejection naming the bound rather than an unbounded backlog, and index builds **MUST** run below a reserved share of capacity kept for interactive requests.

- **Rationale**: Authorization bounds who may declare an index, not how much shared PostgreSQL work the declaration creates; an administrator acting within permission can otherwise starve every tenant on the instance.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`, `cpt-cf-graph-storage-actor-producer-gear`

### 5.2 Node and Edge Ingest

#### Bulk Idempotent Ingest

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-bulk-ingest`

The system **MUST** accept batches of nodes and edges in one ingest request, validate every payload against its GTS type before writing, and apply the batch atomically — either all valid writes commit or the batch is rejected with per-item errors. Writes **MUST** use batched database statements. Repeating an identical ingest **MUST** be a no-op that converges to the same stored state. The tenant's graph revision **MUST** be incremented in the same transaction if and only if stored state actually changed — a converging replay leaves the revision untouched, so retries do not invalidate metric caches.

Convergence **MUST** hold under retries with unknown commit outcomes: every ingest request carries a tenant- and producer-scoped idempotency key, and the system persists that key with a canonical request hash, the committed graph revision, and the response atomically with the batch. An identical retry **MUST** return the recorded outcome without touching graph state; reuse of a key with a different request **MUST** be rejected as a conflict (see DESIGN § Concurrent Ingest Protocol).

- **Rationale**: Producers re-run pipelines; idempotent atomic batches make re-runs safe and cheap, and the prototype's row-at-a-time writes were a measured bottleneck.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`

#### Stable Identity and Parallel Edges

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-stable-identity`

Nodes **MUST** be identified by a producer-supplied stable node key, unique per tenant; ingesting an existing key updates that node. Edge identity **MUST** be derived deterministically from edge type, source key, target key, and an optional producer-supplied discriminator, so that parallel edges of the same type between the same nodes are representable and re-ingest updates rather than duplicates.

An upsert **replaces** the mutable state of the row wholesale: `payload`, `name` and the content field are set to exactly what the request carries, and a field the request omits is cleared rather than preserved. There is no merge on the ingest path, and therefore no attribute that a producer is unable to remove; a future `PATCH` is defined as the merge operation, which keeps the two cleanly distinguishable. Edge payloads follow the same rule. This is the same contract chunked content already states — supplied content is an exact replacement set.

A concrete node's GTS type is immutable under ordinary upsert: a same-key ingest declaring a different type **MUST** be rejected as a conflict — the only permitted type transition is phantom materialization, which locks the node and revalidates incident edges atomically. Producers **MAY** pass an expected version with an update (compare-and-set); a mismatch **MUST** reject the batch. Endpoint-constraint validation **MUST** execute inside the ingest transaction under locks on the referenced endpoint nodes, so concurrently validated batches cannot commit edges against node types they never observed (see DESIGN § Concurrent Ingest Protocol).

- **Rationale**: Deterministic identity is the foundation of idempotent re-sync and of cross-producer references to the same entities.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`

#### Unified Owned and Reference Nodes

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-reference-nodes`

The system **MUST** store owned nodes (entities whose system of record is the graph, such as Finding nodes) and reference nodes (projections of managed objects owned elsewhere, such as commits, pull requests, and comments) in one unified node model: both are GTS-typed nodes distinguished by their type's metadata, not by separate storage or APIs. Reference node payloads **MUST** carry a source-qualified canonical identity — the owning source (gear or external system), the object kind, and the native identifier — so that identical native identifiers from different sources remain distinct within a tenant, and reference-node keys **MUST** derive from that full identity. All query capabilities (search, traversal, projection, analytics) **MUST** treat both families uniformly.

- **Rationale**: The platform value of the graph is connecting new analysis entities (Findings) to existing managed objects; a split model would fragment every query path.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Phantom Node Materialization

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-phantom-nodes`

When an ingested edge references a node key that does not exist, the system **MUST** (by default, and controllable per request) materialize a phantom node of a dedicated phantom type recording the referencing edge type, so the dangling reference stays visible instead of the edge being dropped. A later ingest of the real node under the same key **MUST** replace the phantom in place as one atomic transition: node identity and attached edges are preserved, the concrete payload is validated, every incident edge is revalidated against the concrete type's endpoint constraints, and a violation **MUST** reject the batch without mutation. Concurrent phantom creation and materialization **MUST** resolve deterministically (see DESIGN § Phantom Materialization Contract).

- **Rationale**: Producers ingest incrementally and out of order; silently dropped edges are much harder to diagnose than visible phantoms.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`

#### Edge Provenance and Analysis Preservation

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-edge-provenance`

The system **MUST** distinguish static edges (derived deterministically from source data) from analysis edges (produced by an analysis process). Analysis edges and analysis-originated nodes **MUST** carry provenance metadata: origin, creating actor, method, and optionally model and confidence. Scope replacement (see `cpt-cf-graph-storage-fr-scope-replace`) **MUST NOT** delete analysis-originated content.

- **Rationale**: Re-importing source data must not destroy conclusions computed on top of it; provenance is also required to audit machine-generated edges.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-data-analyst`

#### Scope Replacement Re-Sync

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-scope-replace`

The system **MUST** support declarative scope replacement on ingest: the producer names a scope (an indexed payload attribute and value, e.g., a source repository), and the system deletes previously ingested static nodes and edges of that scope that are absent from the new batch, in the same transaction as the upserts. Analysis-originated content in the scope **MUST** survive replacement.

A scope **MUST** have a canonical identity (tenant, owning producer, scope attribute and value). Replacements of one scope **MUST** serialize on that identity through a lock held to commit, and ordinary ingests writing static content into an owned scope **MUST** participate in the same locking protocol. Every replacement snapshot **MUST** carry a monotonic source generation: the system persists the highest accepted generation per scope and compares-and-updates it atomically under the scope lock; an older generation **MUST** be rejected as stale, an equal-generation identical retry **MUST** return the recorded outcome, and an equal-generation different-content snapshot **MUST** be rejected as a conflict (see DESIGN § Concurrent Ingest Protocol).

- **Rationale**: Producers re-sync whole sources; replacement semantics keep the graph consistent with upstream without full wipes or tombstone bookkeeping.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`

#### Node Read with Adjacency

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-node-read`

The system **MUST** return a single node by key with its type, payload, labels, embedding presence, chunk inventory, and adjacent edges in both directions (with edge types and neighbor keys). Adjacency **MUST** be bounded by a named request parameter defaulting to a configured maximum, and a truncated response **MUST** say so.

- **Rationale**: The entity detail view is the entry point of the UI exploration scenario.
- **Actors**: `cpt-cf-graph-storage-actor-graph-explorer`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Soft Delete

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-soft-delete`

The system **MUST** support deleting a node or an edge by setting a tombstone rather than removing the row. Node read, every search arm, chunk folding, traversal, projections and analytics topology loading **MUST** exclude tombstoned rows. Deleting a node **MUST** tombstone its incident edges in the same transaction, analysis edges included, because the endpoint foreign keys are `ON DELETE RESTRICT` and a node tombstoned without its edges would be unreachable yet still referenced; provenance is retained with the edge that carries it. A delete **MUST** increment the tenant's graph revision, and deleting an already-tombstoned row **MUST** be a no-op that leaves the revision untouched. A tombstoned node key **MUST NOT** be reusable before purge: re-ingesting it is a conflict, so an identifier consumers still hold cannot silently come back with different content.

- **Rationale**: Garbage arrives on day one, not in year two — a producer run pointed at the wrong target, a bad ontology during bring-up, tests against a shared environment. Without deletion the only remedy is re-submitting an entire scope, and for an object that belongs to no scope there is none, so the graph stays permanently dirty and the garbage pollutes search results and analytics. A tombstone is reversible and cheap, and it defers cascade ordering, key reuse and index reconciliation to purge instead of blocking v1 on them.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-platform-admin`

#### Labels

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-labels`

The system **MUST** provide a per-tenant label registry — name, description, display style, and whether the label applies to nodes, edges or both — and **MUST** allow labels to be attached to and detached from existing nodes and edges at runtime, without re-ingesting the object and without any GTS type change. Labels **MUST** be filterable in search, in tabular projection, and as a per-hop restriction in traversal, and **MUST** be returned on nodes and edges in read and projection responses.

Registry administration **MUST** require the ontology-administration permission. Attach and detach **MUST** be authorized as an action distinct from ingest write, on the target object rather than on the label. Attach and detach **MUST** increment the tenant's graph revision, so two reads at one revision can never observe different labels. Scope replacement **MUST NOT** drop labels attached out of band, the same way it preserves analysis edges and their provenance.

- **Rationale**: Labelling is N:N and covers what grouping cannot — the same node is routinely interesting in several cuts at once — and it is the mechanism users already expect from issue trackers. Labels cannot be modelled as payload attributes: filters are admissible only over paths the type declares, types are authored ahead of time, and a label is per-object runtime state.
- **Actors**: `cpt-cf-graph-storage-actor-graph-explorer`, `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-platform-admin`

#### Change Events

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-change-events`

The system **MUST** be able to publish CREATE, UPDATE and DELETE events for nodes and edges to the platform event-broker, and emission **MUST** be off by default. Whether a type emits **MUST** be declared by the `emit_events` trait on the gear's node and edge bases, so a base type sets the default and a derived type overrides only what it needs. Deployment configuration **MUST** be able to override the trait per GTS type pattern so that a vendor can enable or suppress emission without editing GTS type definitions in gear code; configuration wins over the trait, and the more specific pattern wins over the broader one.

Events **MUST** be published through the transactional outbox in the same transaction as the change, so a committed change always produces its event and a rolled-back batch produces none. An ingest that converges without changing stored state **MUST NOT** emit — the predicate that governs the revision bump governs emission. Every event **MUST** carry the tenant, the object key, the GTS type, the operation and the graph revision it committed at, so consumers can order and deduplicate; payload contents **MUST NOT** be included beyond those identity fields.

- **Rationale**: Consumers otherwise poll to notice change, and a per-type, configuration-overridable switch is what lets a deployment run the gear without provisioning a broker topic for types nobody subscribes to.
- **Actors**: `cpt-cf-graph-storage-actor-consumer-gear`, `cpt-cf-graph-storage-actor-platform-admin`

### 5.3 Content Handling

#### Deterministic Content Chunking

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-content-chunking`

When a node is ingested with long-form text content, the system **MUST** split it into deterministic chunks with stable chunk identifiers encoding location (section and offsets, never content), preserve exact character offsets into the raw text, index each chunk for lexical search, and embed each chunk when embedding is requested (see the embedding pipeline for the per-branch vector state). Re-ingesting unchanged content **MUST** produce identical chunks. Supplied content is an exact replacement set: in the same transaction the system **MUST** delete previous chunks absent from the newly computed set, so removed content can never remain searchable.

- **Rationale**: Retrieval quality over long documents requires passage-level granularity; deterministic chunking keeps re-ingest idempotent.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-data-analyst`

#### Heavy Content Offloading

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-heavy-content-offload`

The system **MUST** enforce a configurable payload size ceiling per node and reject payloads above it with an error directing producers to offload heavy content. Node payloads **MUST** be able to reference offloaded content held in the file-storage gear by file identifier, and node reads **MUST** return such references as-is without dereferencing them.

- **Rationale**: The graph stays fast only if it stores searchable metadata; blobs belong in blob storage (per the platform's storage split), referenced by identifier.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-file-storage`

### 5.4 Vectorization

#### Embedding Pipeline

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-embedding-pipeline`

When embedding is requested, the system **MUST** compose a searchable text per node (name plus string payload attributes designated as vectorizable plus a bounded content prefix), embed it and every content chunk through the configured embedding provider, store the vectors for similarity search, and batch the provider calls across the ingest request. Requests **MAY** opt out of embedding; the mandatory-embedding rule applies only to requests that ask for it.

The system **MUST** persist a canonical hash of each embedding input, and the vector state after an upsert **MUST** be one of: embedded and current (embedding requested); absent (new node or chunk with embedding skipped); preserved (embedding skipped and the input hash unchanged); stale (embedding skipped and the input hash changed); or removed together with its row (chunk deleted by exact-set reconciliation). Similarity search **MUST** consider only current vectors — never absent or stale ones — so a stored vector can never rank content that is no longer stored.

- **Rationale**: Vector search is a first-class retrieval arm; controlled skipping supports cheap metadata-only re-syncs.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-embedding-provider`

#### Embedding Identity and Dimension Guard

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-embedding-dim-guard`

The system **MUST** verify at readiness that the configured embedding dimension matches the database vector column definition, and **MUST** reject ingest batches whose produced vectors do not match the configured dimension. The system **MUST** also record the embedding-space identity (model artifact, tokenizer, preprocessing and pooling configuration) under which stored vectors were produced, verify the active provider against that record at readiness, and on mismatch **MUST** fail readiness and block vector search until re-embedding completes. Readiness reporting **MUST** state the active provider identity and dimension.

- **Rationale**: A silent dimension mismatch corrupts similarity ranking, and a same-dimension model swap corrupts it invisibly; the prototype documented the dimension case as a real failure mode, and identity verification closes the remaining gap.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`, `cpt-cf-graph-storage-actor-embedding-provider`

### 5.5 Search

#### Lexical Search

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-lexical-search`

The system **MUST** provide full-text search over node search text and chunk content using web-style query syntax, ranked by lexical relevance, returning matched nodes with highlighted snippets. Chunk hits **MUST** fold up to their parent node, keeping the best-scoring chunk as match provenance.

- **Rationale**: Exact-term retrieval is the baseline entry point into the graph and complements vector recall.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Vector Search

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-vector-search`

The system **MUST** provide vector similarity search: the query text is embedded with the same provider used at ingest and matched against node and chunk vectors by cosine similarity using approximate nearest neighbor indexes, with chunk hits folded to parent nodes.

- **Rationale**: Semantic similarity finds related entities that share no vocabulary with the query.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Hybrid Search

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-hybrid-search`

The system **MUST** provide hybrid search that runs the lexical and vector arms independently and fuses their rankings with reciprocal rank fusion, reporting per-hit which arms matched and each arm's rank.

- **Rationale**: The prototype demonstrated that neither arm alone is sufficient; rank-based fusion is robust without score calibration.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Type-Family Filtering

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-type-filtering`

All search modes **MUST** support filtering results by node type, accepting exact GTS identifiers and GTS family patterns. Pattern semantics **MUST** be the platform's, evaluated by the shared GTS implementation rather than reimplemented here — including the implicit derived-type coverage that makes a bare base identifier match its whole chain. A pattern **MUST** be resolved to a set of registered types and applied as set membership on the interned type reference; it **MUST NOT** be compiled into SQL text, so no identifier ever reaches a `LIKE` pattern and there is no punctuation-escaping surface to get right.

The caller's type filter and the GTS pattern of the permission that authorized the request **MUST** resolve through that same implementation, and the effective type set of a request **MUST** be their intersection. Admission is the one exception and is explicit: a permission over a pattern has to authorize registering a type that does not exist yet, so type registration and ingest **MUST** match the pattern against the requested identifier directly rather than against the resolved set.

#### Consistent Compound Reads

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-read-consistency`

Every compound read — hybrid search (multiple arms plus hydration), traversal with hydration, and projections — **MUST** observe one consistent graph state: all statements of one request execute against a single repeatable-read snapshot (or an equivalent revision-pinned protocol). Responses **MUST** report the observed graph revision, and pagination continuation tokens **MUST** be bound to it, so a continued read never silently mixes revisions.

- **Rationale**: Individually atomic statements can still compose a response describing a graph state that never existed when a concurrent ingest commits mid-request.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-consumer-gear`

- **Rationale**: Consumers usually search within a type family ("all findings", "all documents"), and GTS derivation makes family filters the natural unit.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-consumer-gear`

### 5.6 Graph Traversal and Projection

#### Depth-Limited Traversal

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-graph-traversal`

The system **MUST** expand a subgraph from seed nodes — given as explicit node keys, as hybrid-search hits for a query, or both — by breadth-first traversal up to a requested depth bounded by a system maximum, treating edges as undirected for reachability, with optional per-hop edge-type restriction and node-type filtering of returned nodes. Responses **MUST** include the traversed nodes, edges, seeds, and truncation status; seeds always survive truncation. Because seeds are exempt from truncation, the seed set **MUST** be bounded before expansion: after authorization and deduplication, a request whose distinct authorized seeds exceed the effective node budget **MUST** be rejected rather than served beyond the budget, and seed ordering and admitted-seed metadata **MUST** be deterministic.

- **Rationale**: "Search then expand" and "traverse many nodes and filter" were the primary scenarios that motivated the gear.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Bounded Neighborhood Projection

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-neighborhood-projection`

The system **MUST** serve a UI-oriented neighborhood projection: given one entity, return its connected subgraph up to a requested depth (the reference scenario is depth 3 or less) within a node budget, ordering retained nodes by degree so truncation keeps the structural core, with a toggle to exclude phantom nodes and optional per-node metric annotations.

- **Rationale**: The "open an object, see its relationships" experience needs predictable latency and readable truncation on dense graphs.
- **Actors**: `cpt-cf-graph-storage-actor-graph-explorer`

#### Tabular Projection

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-tabular-projection`

The system **MUST** project nodes matching criteria into tabular results: selection by explicit node-key or identifier lists, by type family, by label, and by filters over indexed payload attributes. Filtering, ordering and pagination **MUST** use the platform OData binding — exactly the five accepted system query options (`$filter`, `$orderby`, `$select`, `$top`, `$skiptoken`, with `cursor` as the alias for `$skiptoken`) — and any other option **MUST** be rejected rather than ignored. `$filter` **MUST** be admitted only over payload paths the type declares in its `index` trait, addressed by the same path in OData syntax (`payload/severity`), and a filter over an undeclared path **MUST** be rejected with an error naming the path and the declared alternatives. Responses **MUST** return stable pages suitable for table rendering, with continuation tokens carried in the platform `CursorV1` rather than in a second token format.

> **Found while building the prototype.** An earlier wording had the token be `CursorV1` *extended with the observed graph revision*. A gear cannot extend it: `CursorV1` has no revision member, and the platform page envelope carries only the items and the cursors. The revision rides on each row's audit envelope instead (`cpt-cf-graph-storage-fr-audit-envelope`, DESIGN § API element envelope): the platform wrapper constrains the *page*, not the *items*, and the items are this gear's own DTO. So this path reports `(source_epoch, graph_revision)` like every other one, per element rather than once per response — an earlier draft of this note concluded a platform slot was needed for it, and none is.

- **Rationale**: "Show me all objects matching these criteria as a table" is a validated scenario and the standard list contract for platform UIs.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-consumer-gear`

### 5.7 Graph Analytics

Whole-graph analytics — degree, PageRank, connected components, betweenness
centrality and community detection — is computed by the separate
`graph-analytics` gear (its ADR-0002), which reads this gear's topology over a
read-only role and owns the revision-keyed metrics cache. The requirements below
are what **this** gear owes that boundary; the algorithms, their determinism
contracts and the asynchronous job surface belong to the analytics gear's own
PRD and DESIGN.

#### Analytics Topology Surface

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-analytics-topology`

The system **MUST** expose a topology-only read surface — node keys with their
interned type, and typed edge pairs with discriminator, both excluding
tombstoned rows — through a database role granted `SELECT` on those columns and
nothing else. Payload, composed search text, embeddings and chunk contents
**MUST NOT** be readable through that role, and it **MUST NOT** be able to write
any graph table. The system **MUST** publish the schema version the surface
conforms to, so a consumer can fail closed on a mismatch instead of on its first
query.

- **Rationale**: The topology-only bound of `cpt-cf-graph-storage-nfr-analytics-memory` becomes a grant the database enforces rather than a rule the reading code is trusted to respect.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`, `cpt-cf-graph-storage-actor-platform-admin`

#### Metric Annotation from Cache

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-metric-annotation`

The system **MUST** be able to annotate neighborhood and tabular projections with
per-node metrics read from the analytics gear's revision-keyed cache, and
**MUST** annotate only from an entry matching the graph revision the read
observed. When the analytics gear is absent, or holds no entry for that revision,
the system **MUST** return the projection without annotations rather than
failing, and **MUST** say in the response that annotations were unavailable.

- **Rationale**: Degree ordering already drives projection truncation, and a projection that fails because an optional gear is not deployed would make analytics a hard dependency of the UI path.
- **Actors**: `cpt-cf-graph-storage-actor-graph-explorer`, `cpt-cf-graph-storage-actor-data-analyst`

#### Revision Signal for Cache Invalidation

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-revision-signal`

The system **MUST** expose the tenant's current graph revision and **MUST**
increment it in the same transaction as any change to stored state — ingest that
actually changed a row, delete, and label attach or detach — and only then. The
revision **MUST** be readable both through the topology surface and through the
API, so cache keying and staleness detection need no second mechanism.

- **Rationale**: The revision is the entire coupling between the two gears; if it moves when nothing changed, every cached metric is discarded needlessly, and if it fails to move when something did, a stale metric is served as current.
- **Actors**: `cpt-cf-graph-storage-actor-data-analyst`

### 5.8 Multi-Tenancy and Access Control

#### Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-tenant-isolation`

All graph data — types registered per tenant scope, nodes, edges, chunks, revisions, and metric caches — **MUST** be tenant-scoped, and every read and write path (including traversal recursion, search arms, and analytics graph loading) **MUST** apply tenant scoping at the database query layer through the platform's secure ORM. Node-key uniqueness is per tenant.

- **Rationale**: The graph is a platform component; traversal and search are novel query shapes that must not become cross-tenant side channels.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`, `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Operation-Level Access Control

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-access-control`

Every API operation **MUST** be authenticated and authorized through the platform policy decision point, with separate permissions for ontology administration, ingest, query, delete, and label attach/detach, declared as GTS permission instances. Authorization is resource-level, not tenant-level only: the PDP-derived access scope **MUST** confine every read path — search arms before ranking, traversal expansion (the caller-authorized induced subgraph), projections, and hydration — per the authorization matrix in DESIGN, with identical enforcement for the REST and in-process paths through a shared policy-enforcement layer. Denied resources **MUST** be indistinguishable from nonexistent ones in results, counts, truncation flags, and budget consumption.

- **Rationale**: Producers, consumers, and administrators have different privileges; write access to a shared graph must be explicitly granted.
- **Actors**: `cpt-cf-graph-storage-actor-authz-resolver`, `cpt-cf-graph-storage-actor-platform-admin`

#### Source Namespace Ownership

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-source-ownership`

Source namespaces for reference nodes **MUST** be an enforced ownership boundary: a namespace is bound to authenticated producer principals within a tenant, owner provenance is recorded immutably when a node is first created under it, and every subsequent update or phantom materialization **MUST** be authorized against that owner in addition to the tenant. An unclaimed namespace is claimed by its first writer; ownership transfer and reconciliation **MUST** be an explicit administrative flow under the ontology-administration permission and **MUST** be audited. A `source` value appearing in a validly typed payload **MUST NOT** by itself authorize writing under that namespace.

- **Rationale**: The identity triple that makes two producers converge on the same upstream object would otherwise let a generic write permission overwrite another source's searchable projection.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-authz-resolver`

#### Tenant Offboarding

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-tenant-offboarding`

The system **MUST** implement its part of the platform tenant-offboarding protocol: accept an authoritative tenant deletion generation, fence new tenant work before removing anything, cancel that tenant's in-flight ingest, index builds, backfills, re-embedding and analytics jobs, delete all tenant-keyed local and derived state, and acknowledge completion to the lifecycle owner. Before reporting ready — and unconditionally after a source-epoch rotation — the system **MUST** reconcile each tenant's applied deletion generation against the authoritative ledger and **MUST** quarantine any tenant whose local generation is behind rather than serving its data.

- **Rationale**: All state lives in one PostgreSQL database, so restoring a backup taken before offboarding resurrects a deleted tenant's data; deletion must be monotonic across restore, and the authority must live outside the restored database.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`

### 5.9 API Surfaces

#### Versioned REST API

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-rest-api`

The system **MUST** expose all capabilities over a versioned REST API following platform conventions: OpenAPI-documented operations, RFC-9457 problem responses, and platform authentication middleware. Request limits (batch sizes, result limits, depth bounds) **MUST** be validated and documented in the API schema.

- **Rationale**: The REST surface is how UIs and non-Rust consumers integrate.
- **Actors**: `cpt-cf-graph-storage-actor-consumer-gear`, `cpt-cf-graph-storage-actor-graph-explorer`, `cpt-cf-graph-storage-actor-data-analyst`

#### Element Audit Envelope

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-audit-envelope`

Every node and edge returned by any read surface **MUST** carry a gear-assigned envelope: tenant, key, creation and last-update timestamps, the soft-delete tombstone, and the subject behind each of those three verbs. The acting party **MUST** be expressed as a platform subject (`subject_id` plus optional `subject_type`) rather than as a user identifier, so an automation, a service integration and a person are all representable. The envelope **MUST** be read-only on every write surface and **MUST NOT** be declared by the GTS types producers register or derive from, which describe only producer-authored fields. Per-element change history is out of scope: the current state is stored, and history is reconstructed from emitted change events and `ingest_audit`.

- **Rationale**: Who wrote an element and when is the first question asked of any stored record, and the answer has to exist before it is needed rather than be added after. Keeping it out of the GTS type keeps a type usable as a static registry instance, where runtime timestamps have no value to carry.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`, `cpt-cf-graph-storage-actor-consumer-gear`, `cpt-cf-graph-storage-actor-data-analyst`

#### Typed SDK Client

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-sdk-client`

The system **MUST** ship a transport-agnostic SDK crate with a typed client trait covering type registration, ingest, search, traversal, projection, and metrics, registered in ClientHub for in-process consumption by other gears, with canonical platform error types. The in-process path **MUST** be subject to the same admission limits as the REST surface — resource bounds are enforced in the shared service layer, not at the HTTP edge only.

- **Rationale**: Producer and consumer gears integrate in-process; the SDK trait is the platform's inter-gear contract pattern.
- **Actors**: `cpt-cf-graph-storage-actor-producer-gear`, `cpt-cf-graph-storage-actor-consumer-gear`

### 5.10 Observability and Readiness

#### Structured Observability

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-fr-observability`

The system **MUST** emit structured tracing for ingest, search, traversal, and analytics operations (batch sizes, arm timings, traversal depth and frontier sizes, cache hits) and expose operational metrics through the platform telemetry stack, including saturation counters for every enforced admission limit. Telemetry is deny-by-default for content: raw or truncated query text, payloads, chunk or snippet text, composed embedding input, vectors, schema instances, and provider request/response bodies **MUST NOT** appear in logs, spans, metrics, or error attributes — only structural fields from the explicit allowlist (counts, sizes, durations, bounded enums, graph revision, opaque correlation identifiers) are permitted (see DESIGN § Telemetry and Audit Contract).

- **Rationale**: Query-shape problems (dense hubs, oversized batches) are diagnosable only with structural telemetry; payloads may hold sensitive content.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`

#### Snapshot Identity Across Restore

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-snapshot-identity`

Every revision-bound identity — pagination and continuation tokens, metric-cache keys and result provenance, analytics job and single-flight identity, graph-engine plugin cursors, and idempotency receipts — **MUST** carry a non-reusable source epoch alongside the graph revision. The epoch **MUST** be rotated by operator action before the gear reports ready after any point-in-time restore or store replacement, and identities minted under an earlier epoch **MUST** be rejected, invalidated or quarantined rather than served. A retry whose idempotency receipt is absent because a restore removed it **MUST** be treated as outcome-unknown and **MUST NOT** be executed automatically.

- **Rationale**: The graph revision is a counter and a restore rewinds it, so revision numbers are reissued to describe a different graph; a revision-only comparison accepts tokens, caches and cursors minted on an abandoned timeline.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`, `cpt-cf-graph-storage-actor-consumer-gear`

#### Readiness Reporting

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-fr-readiness`

The system **MUST** report readiness per capability — database and migrations, server major version and the SQL/PGQ backend's availability on it, policy and type registries, embedding provider and embedding-space identity, property graph and graph-engine plugins, and dynamic indexes — each as healthy, degraded, or unhealthy with named problems. Aggregate readiness **MUST** fail only when a component is unhealthy; a degraded capability **MUST** reject exactly the affected operations with canonical errors (or fall back where a fallback exists) while unrelated operations continue, and **MUST NOT** silently widen behavior. The readiness matrix in DESIGN is normative.

- **Rationale**: A single global boolean either takes healthy lexical and ingest paths offline for an unrelated fault, or keeps admitting a capability already known to be unsafe.
- **Actors**: `cpt-cf-graph-storage-actor-platform-admin`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs are defined in the [architecture manifest](../../../docs/ARCHITECTURE_MANIFEST.md) and [guidelines/](../../../guidelines/). Only gear-specific NFRs are documented here.
>
> **Testing strategy**: NFRs verified via automated benchmarks, security scans, and monitoring unless otherwise specified.

### 6.1 Gear-Specific NFRs

#### Ingest Throughput

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-nfr-ingest-throughput`

The system **MUST** ingest a batch of 10,000 nodes and 20,000 edges (validation and storage, embedding disabled) in 60 seconds or less on the reference deployment configuration.

- **Threshold**: 10,000 nodes + 20,000 edges in <= 60 s, embedding excluded, single tenant, reference hardware profile defined in the benchmark suite
- **Rationale**: Producers re-sync whole repositories; the prototype's row-at-a-time writes made large syncs impractical and batching is the required fix.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Search Latency

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-nfr-search-latency`

Hybrid search **MUST** answer within 500 ms at p95 (query embedding time excluded) on a tenant graph of 100,000 nodes and 500,000 edges with default limits.

- **Threshold**: p95 <= 500 ms, 100k nodes / 500k edges / 300k chunks, arm limit 50, warm indexes
- **Rationale**: Search is interactive; it fronts every exploration scenario.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Traversal Latency

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-nfr-traversal-latency`

Depth-3 neighborhood projection **MUST** answer within 1 second at p95 on a tenant graph of 100,000 nodes and 500,000 edges with a 1,000-node budget.

- **Threshold**: p95 <= 1 s, depth 3, node budget 1,000, same reference graph as search latency
- **Rationale**: The UI neighborhood scenario is interactive and hits dense regions of the graph.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Analytics Topology Bound

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-nfr-analytics-memory`

The topology read surface **MUST** expose at most node keys with their interned type and typed edge pairs — never payloads, composed search text, embeddings or chunk contents — and the grant backing it **MUST** make the wider columns unreadable rather than merely unused. The ceilings that bound a computation's memory move with the computation to the `graph-analytics` gear (its ADR-0002).

- **Threshold**: Configurable ceilings, defaults 1,000,000 nodes / 10,000,000 edges / 2 GiB estimated topology budget; topology-only memory footprint verified by profiling tests
- **Rationale**: Keeping analytics topology-only is what makes reading a million-node graph affordable at all; expressing it as a database grant means a future change to the reading code cannot quietly widen it.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Bounded Response Size

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-nfr-response-bound`

Every response **MUST** be bounded in aggregate, not only per item: cumulative hydrated payload bytes, returned edge count, snippet/chunk-provenance/annotation bytes, and total serialized bytes each have a configured ceiling enforced in the domain layer **before** hydration, with deterministic truncation or pagination at the ordering the query already established and explicit truncation metadata in the response. REST and the in-process client are bound by the same values.

- **Rationale**: Per-item ceilings do not compose — ten thousand individually valid nodes are hundreds of megabytes before adjacency, snippets and annotations are counted, and discovering the limit while serializing is already too late.
- **Verification**: Benchmarks at the maximum admissible cardinality assert the response stays under the configured ceilings and reports truncation.

#### Tenant Fairness Under Load

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-nfr-tenant-fairness`

Sustained load from one tenant **MUST NOT** indefinitely block another. Admission **MUST** hold global caps alongside per-tenant caps, queue per tenant with bounded depth and tenant-fair dispatch, and reserve a configured share of connection capacity for interactive requests so background work — index builds, backfills, re-embedding, cleanup — cannot starve user-facing reads.

- **Rationale**: Per-tenant limits bound what a tenant may start, not what it may hold; admitted work from every tenant competes again in the same shared database, provider and worker pools.
- **Verification**: A saturation test with one heavy tenant and one light tenant asserts the light tenant's p95 latency stays within its uncontended budget by a documented factor.

#### Zero Cross-Tenant Leakage

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-nfr-tenant-zero-leak`

No API operation, under any combination of filters, seeds, traversal depths, or pagination, **MUST** return or count data belonging to another tenant.

- **Threshold**: Zero occurrences in adversarial integration tests seeding multiple tenants with colliding node keys and shared type identifiers
- **Rationale**: Traversal recursion and rank fusion are custom query paths outside the CRUD patterns the platform's secure ORM is normally exercised on.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

#### Code Coverage

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-nfr-code-coverage`

The gear **MUST** maintain at least 85% line coverage across its library crates.

- **Threshold**: >= 85% line coverage, enforced in CI
- **Rationale**: Validation, fusion, and traversal logic carry the correctness risk of the gear and must stay tested as they evolve.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation

### 6.2 NFR Exclusions

- High-availability clustering of the gear itself: the gear is stateless above PostgreSQL in v1; availability follows the platform's standard single-writer database posture, and gear-level clustering is deferred until platform guidance requires it.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Graph Storage REST API

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-interface-rest-api`

- **Type**: REST API
- **Stability**: unstable (v1 during incubation)
- **Description**: Versioned HTTP surface covering type management, ingest, node reads, soft delete, labels, search (lexical, vector, hybrid), traversal, projections, and readiness. The endpoint table, OData binding and versioning policy are normative in DESIGN § 3.3.
- **Breaking Change Policy**: Path-versioned; breaking changes require a new version prefix.

#### Graph Storage SDK Client

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-interface-sdk-client`

- **Type**: Rust trait (ClientHub client) in the SDK crate
- **Stability**: unstable (v1 during incubation)
- **Description**: Typed async client trait mirroring the REST capabilities for in-process gear-to-gear calls, with transport-agnostic models and canonical errors. Behavioural parity with REST is a contract requirement, not a convention: identical permission checks and identical admission limits, both enforced in the shared domain layer.
- **Breaking Change Policy**: Versioned trait names (`...ClientV1`); breaking changes introduce a new trait version.

### 7.2 External Integration Contracts

#### Graph Ontology GTS Base Types

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-contract-gts-ontology`

- **Direction**: provided by library
- **Protocol/Format**: GTS type identifiers with draft-07 JSON Schemas
- **Compatibility**: The three abstract bases (node, edge, attribute) and six family types are versioned GTS types, fully specified in DESIGN § 3.1 (Base Ontology GTS Schemas). Producers derive domain types from a family type, never from a base directly; the required `family` trait is what enforces this. New majors are additive, existing majors immutable, and the phantom type is `x-gts-final` so nothing derives from it.

#### Embedding Provider Contract

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-contract-embedding-provider`

- **Direction**: required from client (plugin implementations)
- **Protocol/Format**: Rust plugin trait — batch text-to-vector with a declared embedding-space identity (model artifact name plus version or hash, tokenizer artifact, preprocessing and pooling configuration) and dimension
- **Compatibility**: Providers declare the full embedding-space identity, not only a dimension; a deployment pins one provider configuration per vector column lifetime, and changing it requires re-embedding.

#### Graph Engine Plugin Contract

- [ ] `p3` - **ID**: `cpt-cf-graph-storage-contract-graph-engine-plugin`

- **Direction**: required from client (plugin implementations)
- **Protocol/Format**: Rust plugin trait behind the traversal port — graph-query execution with declared capabilities (neighborhood expansion, bounded traversal, shortest path, pattern queries, in-engine analytics), including which authorization predicates the plugin can enforce; operations outside a plugin's declared capabilities are answered with a typed not-implemented error, never approximated
- **Compatibility**: The built-in PostgreSQL engine (SQL/PGQ, iterative CTE, and the entity-query hop they fall back to) is the default plugin and defines the baseline capability set. External-engine plugins are additive: they serve capabilities the baseline lacks over a rebuildable projection of the relational source of truth, must uphold the gear's tenant-isolation obligations and authorization equivalence (the gear remains the policy-enforcement point and passes a non-forgeable authorization envelope; a plugin that cannot enforce the full scope fails closed or is bypassed for the built-in engine), and must report their applied (source epoch, graph revision) cursor — the epoch is a non-reusable timeline identifier so a projection surviving a point-in-time restore of the source database is detected and rebuilt rather than served. This contract covers traversal only; the store beneath it is a separate, wider contract (`cpt-cf-graph-storage-contract-graph-store-plugin`), and a store plugin may delegate traversal to a graph engine through this one.

#### Graph Store Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-graph-storage-contract-graph-store-plugin`

- **Direction**: required from client (plugin implementations)
- **Protocol/Format**: Rust plugin trait (`GraphStoreV1`) covering the whole data plane behind the domain services — ingest with batch atomicity, node and edge reads, soft-delete tombstoning, the four search arms and their fusion inputs, tabular projection, label assignment, and the topology surface. Search is inside the contract rather than beside it because a store that served only writes and key reads could not answer the search API at all. Traversal stays in the narrower `cpt-cf-graph-storage-contract-graph-engine-plugin`, which a store may delegate to.
- **Compatibility**: An implementation **MUST** provide, as obligations rather than as PostgreSQL mechanics: batch atomicity across nodes, edges and the idempotency record; single-writer serialization per scope identity held until the batch is durable; monotonic source-generation fencing evaluated under that serialization; refusal to remove a node while a live edge references it; and one consistent snapshot across every arm of a single read. An implementation that cannot provide one of them **MUST** declare that capability unsupported rather than approximate it — a silently weakened guarantee is worse than an absent capability. Conformance is demonstrated by passing the shared suite, which the built-in PostgreSQL store and an in-memory fake both run in v1; the fake is the second implementation that keeps the contract from encoding PostgreSQL. Whole-graph analytics and the SQL/PGQ traversal backend are declared capabilities that a non-PostgreSQL store does not provide, and a deployment without them is supported rather than degraded.

## 8. Use Cases

#### Narrow Candidates with Hybrid Search

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-usecase-hybrid-narrowing`

**Actor**: `cpt-cf-graph-storage-actor-data-analyst`

**Preconditions**:
- Producers have ingested typed nodes with embedded search text

**Main Flow**:
1. The analyst submits a natural-language query with a node-type family filter
2. The system runs lexical and vector arms, fuses them with RRF, and returns ranked nodes with matched-arm and snippet provenance
3. The analyst selects promising hits as seeds for traversal or projection

**Postconditions**:
- A small, relevant candidate set exists for structural expansion

**Alternative Flows**:
- **No lexical matches**: vector arm results still surface semantically similar nodes; the response marks hits as vector-only

#### Project Entities by Criteria into a Table

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-usecase-criteria-table`

**Actor**: `cpt-cf-graph-storage-actor-data-analyst`

**Preconditions**:
- Nodes with indexed payload attributes exist

**Main Flow**:
1. The analyst requests all nodes of a type family matching attribute filters, or supplies an explicit identifier list
2. The system returns a paginated tabular projection ordered by the requested attribute

**Postconditions**:
- The consumer renders a stable, pageable table of matching entities

**Alternative Flows**:
- **Filter on an unindexed attribute**: the system rejects the filter with an error naming the attribute and the indexed alternatives

#### Traverse and Filter a Region of the Graph

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-usecase-traverse-filter`

**Actor**: `cpt-cf-graph-storage-actor-consumer-gear`

**Preconditions**:
- Seed node keys are known (e.g., from a prior search)

**Main Flow**:
1. The consumer requests traversal from the seeds to a bounded depth restricted to named edge types
2. The system expands breadth-first, applies node-type filters to the output, and returns nodes, edges, and truncation status
3. The consumer post-processes the bounded subgraph

**Postconditions**:
- The consumer holds a bounded, typed subgraph for downstream logic

**Alternative Flows**:
- **Expansion exceeds the node budget**: the system truncates, keeps all seeds, and sets the truncation flag

#### Explore an Entity's Neighborhood in the UI

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-usecase-ui-neighborhood`

**Actor**: `cpt-cf-graph-storage-actor-graph-explorer`

**Preconditions**:
- The UI displays an entity backed by a graph node

**Main Flow**:
1. The user opens the entity's relationship view
2. The UI requests the neighborhood projection at depth 3 or less with a node budget
3. The system returns the degree-ordered neighborhood subgraph with metric annotations
4. The UI renders the subgraph with important nodes retained under truncation

**Postconditions**:
- The user sees the entity's relationships up to the requested depth

**Alternative Flows**:
- **Dense hub entity**: truncation retains the highest-degree neighbors and marks the response truncated

#### Ingest Findings Linked to Managed Objects

- [ ] `p2` - **ID**: `cpt-cf-graph-storage-usecase-finding-ingest`

**Actor**: `cpt-cf-graph-storage-actor-producer-gear`

**Preconditions**:
- The producer registered its Finding node type (derived from the owned-node base) and its edge types
- Reference node types for commits, pull requests, and comments are registered

**Main Flow**:
1. An analysis run produces Findings referencing commits and pull requests
2. The producer ingests Finding nodes (owned), reference nodes for the managed objects with canonical upstream identifiers, and analysis edges with provenance in one batch
3. The system validates every payload against its GTS chain, upserts idempotently, and links Findings to the referenced objects

**Postconditions**:
- Findings are searchable and traversable alongside the managed objects they concern

**Alternative Flows**:
- **A referenced managed object was not ingested**: the edge materializes a phantom node that a later mirror sync replaces in place

## 9. Acceptance Criteria

- [ ] A producer can register an ontology, ingest a batch containing owned nodes, reference nodes, and both edge families, and re-run the identical ingest with a byte-identical resulting graph state
- [ ] Scope replacement removes stale static content and demonstrably preserves analysis edges and their provenance
- [ ] All four retrieval scenarios (hybrid narrowing, criteria table, bounded traversal with filtering, depth-3 UI neighborhood) succeed against a seeded reference graph within the latency thresholds of § 6.1
- [ ] Payloads violating their GTS derivation chain, edges violating endpoint constraints, and vectors violating the configured dimension are rejected with structured, per-item errors
- [ ] Adversarial multi-tenant tests observe zero cross-tenant data in every endpoint
- [ ] `cfs validate` passes for this gear's documentation set, and the gear's CI meets the coverage threshold

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| PostgreSQL 16+ with pgvector | Single storage backend: relational source of truth, full-text, JSONB and vector indexes | p1 |
| PostgreSQL 19+ with the property graph (pinned beta image until GA) | Optional: enables the SQL/PGQ traversal backend; traversal is served by the CTE backends without it | p2 |
| Platform tenant-deletion ledger | Authoritative, restore-independent tenant deletion generations for offboarding reconciliation (`cpt-cf-graph-storage-fr-tenant-offboarding`) | p2 |
| ToolKit framework | Gear lifecycle, REST OperationBuilder, SecureORM, ClientHub, canonical errors | p1 |
| AuthZ Resolver gear | Policy decisions and access scopes for every operation | p1 |
| Types Registry gear | Platform registration of the gear's GTS base types and permission instances | p1 |
| Embedding provider | In-process ONNX runtime or remote inference endpoint producing fixed-dimension vectors | p1 |
| File Storage gear | Blob storage for heavy content referenced from node payloads | p2 |
| ToolKit `toolkit-db` safe-CTE API | Secure execution path for single-statement traversal (scoped CTE, `GRAPH_TABLE`) under a compiled access scope. Not required for correctness — bounded traversal ships as two scoped queries per hop — but required for single-statement composition of vector, graph and full-text retrieval. Delivered in two halves: the scoped-CTE half merged as `toolkit-db` PR #4584 (secure-orm ADR-0001), the SQL/PGQ half in review as PR #4639 (secure-orm ADR-0002, `SecureGraphSelect`). The gear's hop has been rebuilt against both and renders as one scoped statement | p2 |

## 11. Assumptions

- Producers can express their entities as typed nodes and edges and are responsible for parsing source material; the gear never crawls upstream systems
- Managed-object producers (e.g., a mirror gear) push reference-node projections; the graph does not subscribe to upstream change feeds in v1
- One embedding provider configuration (model and dimension) is active per deployment at a time; changing it implies re-embedding
- Tenant graphs fit the analytics gear's configured ceiling; graphs beyond it forgo whole-graph analytics but keep all other capabilities
- The platform provides tenant resolution and authentication in front of the gear's API

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Dense hub nodes make traversal and projection slow or unreadable | Interactive scenarios miss latency targets | Node budgets, per-hop edge-type filters, degree-ordered truncation, edge-type exclusion in analytics |
| JSONB attribute indexing degrades as payloads grow | Filter queries slow down; index bloat | Payload size ceiling, indexable-attribute discipline in ontology design, heavy-content offloading |
| Embedding model change invalidates stored vectors | Vector search quality silently degrades | Provider identity and dimension pinned in configuration; readiness identity guard blocks vector search on mismatch; operator-triggered resumable re-embedding lifecycle with checkpoints and atomic identity cutover (ADR-0004) |
| Community detection and sampled betweenness differ from prototype outputs | Consumers expecting NetworkX-identical numbers are surprised | PRD explicitly waives numeric parity; determinism and ordering guarantees are documented per algorithm |
| A single tenant's ingest load starves others | Platform-wide latency degradation | Batch size limits, per-tenant concurrency gates, operation-level permissions, observability of per-tenant load |
| Analytics load starves the interactive path | Ingest and search miss latency targets | Analytics runs as its own gear with its own CPU, memory and connection budget (graph-analytics ADR-0002), so the two cannot share a pool |
| Shared ontologies evolve incompatibly across producers | Ingest failures or semantic drift between producers | Immutable schemas per GTS version, conflict-rejecting registration, family patterns that keep older derived types valid |
| PostgreSQL 19 GA slips, or a PG19 beta regression hits the pinned stack | The gear ships on a beta database longer than planned | The stack is pinned (beta image + pgvector revision) and validated by the PG19 spike and the prototype's full test suite; the iterative-CTE backend can serve the whole fixed-depth API if a PGQ-specific regression appears; re-pin to stock at GA |
| SQL/PGQ variable-length paths arrive later than PG20 | The CTE backend carries variable-depth expansion longer | The traversal port isolates the split; consumers see no API difference; a dedicated traversal mirror remains the measured-bottleneck contingency (ADR-0001) |
| The `toolkit-db` scoped single-statement API is not delivered | Single-statement traversal and single-statement hybrid composition stay unavailable; each hop costs an extra database round trip | Bounded traversal is implemented and verified without it (two scoped queries per hop, p95 0.37 ms per hop at reference scale), so delivery affects performance and expressiveness rather than viability. Measured against the candidate implementation, the single statement buys tail latency on wide frontiers — depth-3 p95 30.0 ms against 50.5 ms end to end — not correctness. Largely retired: the CTE half is merged (PR #4584) and the SQL/PGQ half is in review (PR #4639). What remains of the risk is the SQL/PGQ half not landing, which leaves the CTE backend serving traversal in full |

## 13. Open Questions

- Who decides which payload paths a type declares in its `index`, `full_text_search` and `vector_search` traits — the ontology author, the platform administrator via deployment configuration, or both with an approval step? Owner: platform steering committee; deadline: before the v1 ontology-registration API freeze. Until resolved, the binding interim policy from ADR-0003 applies: the declarations are authored by the ontology author, and index-affecting registrations require the ontology-administration permission.
- Which embedding model does the platform standardize on, who owns model upgrades, and is re-embedding on model change automatic or operator-triggered?
- Do managed-object reference nodes eventually sync through platform events (event-broker) instead of producer pushes, and if so, which component owns the subscription?
- Are edge payload attributes worth indexing in v1, or do edge filters remain type-only until a concrete consumer needs attribute-level edge filtering?
- What is the retention policy for phantom nodes that are never replaced by real nodes, or whose last referencing edge is removed by scope replacement — permanent visibility, TTL-based cleanup, or producer-triggered pruning? (See DESIGN § Phantom Materialization Contract for the transition rules that stop at this question.)
- Does the gear expose a graph export format (such as the prototype's cfs-map document) in v1, and who are its consumers?
- Does the gear expose a consumer-facing bounded graph-pattern query endpoint (a declarative graph-query DSL, e.g., derived from SQL/PGQ patterns) in a later version? Raw query languages cannot be exposed in a multi-tenant platform, so the shape and bounds of such a DSL — and which consumers need it — remain to be defined.
- Where does GTS pattern-to-type-set resolution live, and does the authorization side use the same semantics? Both this gear's type filter and a permission's `resource_type` pattern need it, and their results are intersected on one request, so a divergence would silently widen or narrow a result set instead of failing. `GtsIdPattern` in `gts-id` is the only implementation of the semantics today, and the platform PEP does not currently interpret `resource_type` as a pattern at all. Agreeing the semantics and the home of the resolution helper — GTS SDK versus each gear — before this gear or the next one implements its own is the point. Owner: authorization (PDP) and GTS SDK owners jointly. **Does not block implementation.** The caller's type filter resolves entirely inside this gear, and authorization works today with the concrete `resource_type` the PEP already accepts, so v1 is buildable and correct without the answer. What keeps deferral cheap is one rule the implementation must hold from the first commit: `GtsIdPattern` is the only matcher, never a local wildcard dialect and never SQL `LIKE`. Held to that, either outcome is additive — the PDP resolving patterns into an `In` predicate needs no change here, and a `Pattern` predicate reaching the gear is one arm where constraints are consumed.
- When does cross-request PDP decision caching become necessary, and what invalidation signal backs it? DESIGN § Authorization Model records why v1 has none: the platform PEP publishes no revocation signal, so a TTL-only cache buys throughput at the price of a window in which a revoked permission still works. Resolving this needs a revocation epoch or decision version from the authorization side. Owner: authz-resolver. **Does not block implementation.** No cache is the strictest and safest v1 answer, fully specified and dependent on nothing; the question is whether a later optimization is admissible, not whether v1 is. Deferral stays cheap because resolution happens at one seam — the shared PolicyEnforcer-backed application service — so a cache is inserted at a single call site rather than threaded through the read paths. If the answer never comes, the gear ships without one and pays one PDP call per `(ResourceType, action)` per request.
- Should an external graph engine be validated as the first third-party graph-engine plugin? The candidate experiment is an ArcadeDB plugin serving shortest-path queries over a rebuildable projection of the edge table (PG stays the system of record) — it would exercise the plugin contract end to end and feed the engine re-evaluation scheduled for Q1 2027 (ADR-0001) with first-hand data.

## 14. Traceability

Links to related specification artifacts.

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/)
