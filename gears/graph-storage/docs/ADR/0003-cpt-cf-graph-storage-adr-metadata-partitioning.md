---
status: accepted
date: 2026-08-13
amended: 2026-08-24
decision-makers: Graph Storage design review
---

# ADR-0003: Node metadata splits into common columns, schema-declared indexed and vectorized attributes, and externalized heavy content

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A. Fixed platform-wide layout with opaque payloads](#a-fixed-platform-wide-layout-with-opaque-payloads)
  - [B. Index and vectorize everything automatically](#b-index-and-vectorize-everything-automatically)
  - [C. Schema-declared partitioning](#c-schema-declared-partitioning)
- [What payload filtering needs from the platform](#what-payload-filtering-needs-from-the-platform)
  - [1. A filterable field set resolved per request](#1-a-filterable-field-set-resolved-per-request)
  - [2. A field that maps to an expression, not only to a column](#2-a-field-that-maps-to-an-expression-not-only-to-a-column)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-graph-storage-adr-metadata-partitioning`

> **Amendment 2026-08-24.** The partitioning decision is unchanged. What changed is
> the carrier: indexing and vectorization declarations were specified as
> per-property extension keywords (`x-gts-indexed`, `x-gts-vectorized`) and are now
> GTS trait values on the gear's node base. The two reasons are recorded in the
> Decision Outcome and Consequences below — the inheritance model already exists,
> and the effective set becomes enumerable in one lookup. The change also splits
> "vectorizable" into `full_text_search` and `vector_search`, which the single
> annotation had conflated.

## Context and Problem Statement

A shared graph accumulates payloads from many producers. If every attribute is indexed and embedded, indexes bloat and ingest slows; if none are, filters and vector search stop working; if payloads carry article bodies or raw logs, the graph becomes a slow blob store. The gear needs a defined partitioning of node metadata — what lives in dedicated columns, what is indexed inside JSONB, what feeds embeddings, and what must leave the graph entirely — and a defined authority for those choices per type.

This ADR is `accepted` for the partitioning mechanism, which DESIGN builds on normatively (extension keywords, filter rejection for unannotated attributes, index provisioning, the payload ceiling) and which the prototype validates. The governance sub-question — who approves indexing and vectorization declarations — is deliberately **out of this ADR's scope**: it is an operational-policy decision, tracked in PRD § Open Questions with an owner (the platform steering committee), a binding interim policy (see Decision Outcome), and a blocking gate — it must be resolved before the v1 ontology-registration API freezes.

## Decision Drivers

- Meeting-established direction: split fields into common ones (tenant, timestamps, user) and per-type schema; mark indexable (JSONB) and vectorizable fields; keep heavy texts out of the database, referenced by key from S3-like storage.
- `cpt-cf-graph-storage-fr-tabular-projection` needs predictable attribute filters — which requires knowing which attributes are index-backed.
- `cpt-cf-graph-storage-fr-embedding-pipeline` composes search text from designated fields; embedding everything is costly and dilutes vectors.
- `cpt-cf-graph-storage-fr-heavy-content-offload` and `cpt-cf-graph-storage-nfr-ingest-throughput` cap payload size so writes and index maintenance stay fast.
- GTS schemas are the one artifact every producer already authors and versions — the natural carrier for per-type storage semantics.
- Unresolved: whether ontology authors alone decide indexing, or a platform administrator approves it (each new index costs shared write throughput and disk).

## Considered Options

- A. Fixed platform-wide layout: only gear-defined common fields are queryable; payloads are opaque
- B. Index and vectorize everything automatically
- C. Schema-declared partitioning: GTS schemas annotate attributes as indexable and vectorizable; the gear provisions indexes and composes embeddings accordingly; heavy content is banned from payloads by size ceiling and offloaded to file storage

## Decision Outcome

Chosen option: "C. Schema-declared partitioning", because the ontology author knows attribute semantics, the declaration lives in the same versioned, reviewable contract as the type itself, and the gear can enforce it mechanically. The layout:

1. **Common columns** (every node, gear-defined): tenant, node key, GTS type, display name, created/updated timestamps, creating actor, embedding, search vector.
2. **Indexed, full-text and vectorized paths are declared as GTS trait values**, not as per-property extension keywords. The type carries three independent JSON-pointer lists — `index`, `full_text_search`, `vector_search` — under the gear's trait schema on the node base (DESIGN § Base Ontology GTS Schemas). Paths in `index` are queryable in tabular projections and scope filters and the gear maintains the supporting index — one B-tree over each declared path's extraction expression, not a single GIN over the payload (see Consequences); paths in `full_text_search` compose the node's tsvector; paths in `vector_search` compose the embedding input.
3. **Full-text and vector declarations are separate lists.** They are different indexes with different costs, and a field worth putting in the tsvector is frequently the wrong field to embed — a single "vectorizable" annotation forced the two together.
4. **Heavy content**: payloads above the configured ceiling are rejected; long-form content goes to the file-storage gear, referenced from the payload by file identifier (and may still contribute a bounded excerpt to search text via the content field).

5. **Accepted declarations pass capacity admission before index intent is committed.** Authorization to register a type bounds *who* may declare an index, not *how much* index work the declaration creates. An administrator acting entirely within permission can publish type versions whose declared paths each commit durable intent, launch `CREATE INDEX CONCURRENTLY` and scan the whole table, starving the shared PostgreSQL instance for every tenant. Registration therefore checks per-tenant and deployment-wide caps on registered types and indexed paths, estimates the build footprint and reserves capacity **before** the intent row commits, and admits accepted intents into a bounded tenant-fair DDL queue running at background priority under a reserved interactive share. Exhausted budget produces backpressure — a rejection with a retry-after hint — never an unbounded backlog. The bounds are named configuration keys in DESIGN § Capacity and Admission Contract.

Until the steering committee resolves the governance question, the following **interim policy is binding**: annotations are authored by the ontology author and reviewed like any GTS contract change, and any type registration that provisions indexes or changes vectorization requires the ontology-administration permission (`cpt-cf-graph-storage-fr-access-control`), which keeps index-affecting registrations administratively gated. The steering-committee decision (owner of the open question; deadline: before the v1 ontology-registration API freeze) may replace this policy without reopening this ADR — the partitioning mechanism is unaffected by who approves declarations.

### Consequences

- The declarations are trait values, so their inheritance and merge semantics along the derivation chain are the GTS registry's, already specified and already implemented. The gear registers no extension keyword of its own, and a schema carrying an unknown one is still rejected.
- Filters over unannotated attributes are rejected with an error naming the indexed alternatives, keeping query performance honest (`cpt-cf-graph-storage-usecase-criteria-table` alternative flow).
- Changing annotations is a type-version change with a **durable index activation lifecycle**: `requested -> building/backfilling -> active` (or `failed`), and `retiring -> removed` for the old version. Type registration commits the index *intent* atomically; the privileged DDL runs afterwards in the gear's background lifecycle worker using `CREATE INDEX CONCURRENTLY` (which cannot participate in the registration transaction) with idempotent index naming, retries, and cleanup of failed/invalid builds. Filters over an annotated attribute are admitted **only while its index is `active`** — before that they are rejected exactly like filters on unannotated attributes, so accepting a type version never silently enables full-table scans. Readiness reports in-flight and failed builds; old-version indexes are retained until the version retires.
- The **annotation meta-schema this ADR previously owed is no longer needed**, and that is the main reason for the trait form. Canonical names, value shapes, allowed locations, inheritance along the chain and ancestor-versus-leaf override are all properties of `x-gts-traits-schema` / `x-gts-traits`: the schema is declared once on the base, the registry resolves the effective values right-to-left across the chain, and a value outside the declared shape is rejected by trait validation rather than by a gear-specific validator. What remains gear-specific is which paths are legal targets (they must resolve within `payload`, or be `/name` for full-text) and which changes require a new type version.
- The effective declaration set for a type is **enumerable in one lookup** rather than by walking a schema tree. The ontology catalog, `$filter` admissibility checking and the index-provisioning lifecycle all need exactly that, and the gear persists it as `gts_type.effective_traits`.
- Index declarations are now capacity-admitted as well as authorized, which makes an accepted declaration a promise the deployment can keep: an intent that cannot be built within the estimated footprint is rejected at registration rather than accepted and left queued indefinitely. The cost is that ontology evolution can be refused for capacity reasons, which is deliberate — the alternative is a shared instance whose write throughput degrades for every tenant because one ontology grew.
- **Found while building the prototype: a declared path is backed by a B-tree over its extraction expression, not by a GIN over the payload.** The distinction is easy to miss because both are "a JSONB index", and it decides whether the projection contract can be served at all. The tabular projection is not filter-only: it orders and paginates by keyset, so `$filter`, `$orderby` and the cursor all need a total order over the filtered field. A GIN index — the default `jsonb_ops` or the narrower `jsonb_path_ops` — answers containment and key existence. One such index covers equality over *every* path at once, and ordering over *none*. The prototype's single GIN over the whole payload is therefore not a cheaper form of this decision but a different and much weaker one: it would have admitted `eq`, silently left every ordered projection to a full scan and a sort, and given the operator no signal that it had. One expression index per declared path, partial on the soft-delete predicate like every other read-path index, is what this contract costs — and is the reason the paths are declared rather than inferred.

- **Found while building the prototype: a declared path resolves to a scalar type, and registration rejects it when it does not.** The `index` trait is a list of JSON pointers and carries no type of its own, which is right — the pointer already points into the type's own schema, and that schema types it. But the resolution has to be explicit, because four separate things need the answer and none of them can wait until query time: the index expression (`->>` yields `text`, so an ordered numeric or temporal path needs its cast written into both the index and every predicate, or the index is simply not used), comparison semantics (in `text`, `'10' < '9'`), the keyset cursor (a continuation token must be encoded and parsed under a known field kind), and the failure mode when one node's payload holds an object or an array where the path was declared scalar. Registration therefore resolves each declared pointer against the type's effective schema and rejects a path that does not land on a scalar of a supported kind — string, integer, number, boolean, date-time — rather than accepting a declaration whose index would be built and then never used.

- The payload ceiling makes producer-side offloading mandatory for document-like content from day one, preventing the graph from becoming the platform's accidental blob store.
- Until the governance question closes, deployments must treat index-affecting type registrations as administratively reviewed operations (the permission model in `cpt-cf-graph-storage-fr-access-control` already separates ontology administration from ingest).

### Confirmation

- Type-registration tests confirm that a type resolving no `family` trait is rejected, that a trait value outside its declared shape is rejected, and that a leaf's declarations override an ancestor's for the same key — all through the platform's trait validation, verified against `gts-rust` 0.12.0 / GTS spec v0.13.1.
- Projection tests confirm that paths declared in the `index` trait filter correctly and that filters over undeclared paths are rejected with the documented error naming the declared alternatives.
- Registration tests confirm that exceeding the per-tenant or global indexed-path caps is rejected with `resource_exhausted` naming the bound, that an intent whose estimated footprint exceeds the budget never commits, and that a saturated DDL queue applies backpressure instead of accepting work.
- Ingest benchmarks confirm the payload ceiling and trait-bounded indexing hold `cpt-cf-graph-storage-nfr-ingest-throughput`.
- The governance decision is confirmed when the approval flow is ratified by the platform steering committee (owner; blocking for the v1 ontology-registration API freeze) and recorded against the PRD open question; the interim permission-gated policy applies until then and its enforcement is covered by access-control tests.

## Pros and Cons of the Options

### A. Fixed platform-wide layout with opaque payloads

Only gear-defined common columns are queryable; payload JSON is stored but never indexed.

- Good, because storage behavior is fully predictable and index cost is constant.
- Good, because no governance question arises.
- Bad, because criteria-based tabular projection over domain attributes — a validated core scenario — becomes impossible.
- Bad, because producers would bypass it by stuffing filterable data into the name field or external side tables.

### B. Index and vectorize everything automatically

Every payload attribute gets JSONB path indexing; all string values feed embeddings.

- Good, because producers do nothing and every filter works.
- Bad, because index size and write amplification grow with payload shape, not with need, degrading shared ingest throughput.
- Bad, because embedding all strings (identifiers, URLs, enum values) dilutes vector quality for the fields that matter.
- Bad, because cost accrues invisibly — no one ever decides anything, so no one can ever prune.

### C. Schema-declared partitioning

Annotations in the GTS schema drive indexing and embedding; size ceiling forces heavy content out.

- Good, because the decision is explicit, versioned, and reviewable in the type contract.
- Good, because index and embedding cost is proportional to declared need.
- Good, because the same annotations document query capabilities to consumers.
- Neutral, because it introduces platform-specific GTS extension keywords that must be specified and maintained.
- Bad, because ontology authors can still over-declare; governance (the open question) is the backstop.
- Bad, because annotation changes ride the type-versioning process, making "just index this field" a heavier operation than a DBA one-liner.

## What payload filtering needs from the platform

**Found while building the prototype.** Everything above is a gear-side decision, and none of it reaches SQL through the platform's OData binding as that binding stands today. The projection serves `$filter` and `$orderby` over the four common columns and cannot serve a declared payload path at all — not because the index is missing (it is, but that is this gear's work) but because the binding has no shape for a field that is not a column known at compile time. Two things are missing. Both are additive: nothing built on the binding today changes behaviour if they land.

### 1. A filterable field set resolved per request

`FilterField` declares its members as `const FIELDS: &'static [Self]`. The set is therefore fixed when the gear is compiled, and the paths this ADR declares are not: they belong to a tenant's ontology and to a type version, and their admissibility depends further on whether the path's index has reached `active`. A gear cannot enumerate them in a `const`, so it cannot present them to the binding at all.

**What it would change.** `$filter` over a declared path becomes expressible in the platform binding rather than only in a dialect the gear would have to invent. The field kind must travel with the resolved set, because the cursor codec needs it — which is the other half of why the scalar type above has to be resolved at registration.

### 2. A field that maps to an expression, not only to a column

`FieldToColumn::map_field` returns a `Column`, and the predicate is assembled as `Expr::col(column)`. A declared path is an *expression over* a column, so there is nowhere to put it. An additive `map_field_expr(F) -> SimpleExpr`, defaulting to today's `Expr::col(map_field(f))`, would carry it; the same seam serves the continuation token, where `extract_cursor_value` would read the path out of the model's payload under the resolved field kind.

**What it would change.** This is the narrower of the two and is useful beyond payload filtering — any field whose storage shape is a computed expression rather than a stored column needs it.

**Status of these asks.** Not yet raised with the platform. The index-activation lifecycle this ADR specifies is unimplemented, so there is no consumer to measure a proposed signature against; these are recorded at the precision a reading of the binding supports, and are to be raised with a working prototype behind them — as ADR-0005's asks were — when the lifecycle lands.

**The alternative is refused.** A gear can parse `$filter` itself and emit its own `Condition` carrying a custom expression; that compiles today. It is the second query dialect the architecture lints exist to prevent (DE0802/DE0803), and it would forfeit the ordering and cursor integration that make the projection a contract rather than a query endpoint — solving a quarter of the problem at the cost of the rule that keeps every gear's `$filter` meaning the same thing.

## More Information

The prototype partially anticipated this: it indexed the whole payload with a single JSONB GIN index, composed search text by walking all string values to depth 4, and documented (but did not enforce) heavy-content exclusion. This ADR replaces those implicit behaviors with declared ones. The governance open question is tracked in PRD § 13.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses:

- `cpt-cf-graph-storage-fr-tabular-projection` — filterable attributes are exactly the annotated ones
- `cpt-cf-graph-storage-fr-embedding-pipeline` — search-text composition reads the vectorization annotations
- `cpt-cf-graph-storage-fr-heavy-content-offload` — the payload ceiling and file-storage reference pattern
- `cpt-cf-graph-storage-fr-type-registration` — extension keywords validated at registration
- `cpt-cf-graph-storage-nfr-ingest-throughput` — bounded indexing protects the write path
