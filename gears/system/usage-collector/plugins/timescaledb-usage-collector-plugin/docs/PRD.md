Created:  2026-07-21 by Virtuozzo International GmbH
Updated:  2026-07-21 by Virtuozzo International GmbH

# PRD — TimescaleDB Usage Collector Storage Plugin

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
  - [5.1 Record Persistence](#51-record-persistence)
  - [5.2 Query & Aggregation](#52-query--aggregation)
  - [5.3 Usage-Type Catalog](#53-usage-type-catalog)
  - [5.4 Data Lifecycle](#54-data-lifecycle)
  - [5.5 Plugin Integration & Error Contract](#55-plugin-integration--error-contract)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [Ingest a Usage Record with Idempotent Dedup](#ingest-a-usage-record-with-idempotent-dedup)
  - [Delete a Referenced Usage Type](#delete-a-referenced-usage-type)
  - [Bind the Backend at Host Startup](#bind-the-backend-at-host-startup)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

> **Abbreviations**: SPI = **Service Provider Interface**; GTS = **Global Type System**. This PRD describes a **storage backend plugin** for the Usage Collector gear.

## 1. Overview

### 1.1 Purpose

The TimescaleDB Usage Collector Storage Plugin (`timescaledb-usage-collector-plugin`) is a storage backend for the Usage Collector gear. It implements the Usage Collector storage SPI (Plugin SPI, `cpt-cf-usage-collector-interface-plugin`) on top of PostgreSQL with the TimescaleDB extension, and is the durable system of record for both usage records and the usage-type catalog.

This PRD specifies **only plugin-specific requirements** for the TimescaleDB backend. All product-level requirements — ingestion semantics, the idempotency contract, counter/gauge semantics, attribution, tenant isolation, authorization, the query/aggregation product surface, correction primitives, usage-type lifecycle, and data classification — are defined in the parent gear PRD and are **inherited** by this plugin:

- **Parent PRD (authoritative)**: [../../../docs/PRD.md](../../../docs/PRD.md)

Under the gear + plugin split, the Usage Collector core owns authentication, PDP authorization, attribution and shape validation, idempotency-key presence, and counter/gauge decisions; the plugin is pure persistence and query and receives only already-authorized, structurally-validated calls.

### 1.2 Background / Problem Statement

The Usage Collector requires at least one deployed storage plugin to reach readiness. Its workload is append-heavy time-series ingestion combined with time-windowed analytical reads at a high throughput envelope (sustained ≥ 10,000 records/sec, per `cpt-cf-usage-collector-nfr-throughput-profile`). A time-series-optimized backend keeps inserts and time-range scans efficient at that envelope while providing the transactional guarantees the correction and referential-integrity requirements need.

TimescaleDB — a PostgreSQL extension — is selected because it provides native time-partitioning (hypertables) and declarative retention for the append-heavy time-series workload while retaining PostgreSQL's ACID transactions and native referential integrity, which the depth-1 deactivation cascade and the usage-type integrity requirements rely on. Co-locating usage records and the usage-type catalog in one database lets integrity between them be enforced by the backend rather than by gear-side coordination.

### 1.3 Goals (Business Outcomes)

- Provide a production-grade time-series storage backend that satisfies the parent gear's query-latency and ingestion-throughput NFRs without a separate downstream aggregation layer. **Verification**: load tests against a bound backend within the parent throughput profile (`cpt-cf-usage-collector-nfr-throughput-profile`).
- Enforce usage-type referential integrity and idempotent ingestion natively in the backend, so correctness does not depend on gear-side coordination. **Verification**: tests asserting a referenced usage type cannot be deleted and a retry never admits a duplicate.
- Keep all TimescaleDB-specific storage logic, schema, and dependencies isolated to this crate so the backend can evolve and be licensed independently of the host gear. **Verification**: conformance to the SDK SPI and a dependency check that the crate does not depend on the host gear crate.

All other business and product goals are defined by the parent Usage Collector PRD.

### 1.4 Glossary

The parent gear glossary is the primary source of truth. The terms below are plugin-specific.

| Term                | Definition                                                                                                                                                                                       |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Hypertable          | A TimescaleDB time-partitioned table; usage records are stored in a hypertable partitioned on event time.                                                                                        |
| Chunk               | A time-bounded partition of a hypertable; retention and dedup uniqueness ride the chunk lifecycle.                                                                                               |
| Retention policy    | A declarative policy that drops chunks whose time window lies wholly outside the configured retention period.                                                                                    |
| Dedup key           | The `(tenant_id, gts_id, idempotency_key, created_at)` 4-tuple on which usage-record uniqueness is enforced (ADR-0014).                                                                          |
| Silent absorb       | An idempotent-retry outcome: a same-dedup-key submission whose canonical fields match the stored record returns the stored record instead of creating a duplicate.                               |
| Keyset pagination   | Seek-based pagination that walks records in a stable order by a cursor rather than by numeric offset.                                                                                            |
| Consistency profile | The read-visibility ceiling a backend deployment advertises above the gear's eventual-consistency floor (per `cpt-cf-usage-collector-nfr-query-freshness`).                                      |
| SPI                 | Service Provider Interface — the Usage Collector storage-plugin contract (`cpt-cf-usage-collector-interface-plugin`) this plugin implements; distinct from the gear SDK client and the REST API. |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level. The plugin's product-facing actors (usage sources, usage consumers, tenant administrators) interact with the **gear**, not the plugin, and are documented in the gear PRD (`cpt-cf-usage-collector-actor-*`).

### 2.1 Human Actors

This plugin has no direct human actors. Platform operators, developers, and tenant administrators interact only with the Usage Collector gear, never with the plugin directly; operator configuration (database connection, pool bounds, retention window, vendor/priority) reaches the plugin through the gear's configuration surface.

### 2.2 System Actors

#### Usage Collector Core (Plugin Host)

**ID**: `cpt-cf-uc-plugin-actor-plugin-host`

- **Role**: The Usage Collector gear core. It invokes this plugin through the storage SPI for all persistence and query operations, performing authentication, PDP authorization, attribution and shape validation, and semantics decisions before every call; the plugin performs storage only. The core is the sole caller of the SPI and owns which plugin is bound as the active backend.

## 3. Operational Concept & Environment

This plugin operates within the standard Gears ToolKit lifecycle. At startup it creates its database connection pool, provisions its schema idempotently, and registers itself as a scoped SPI client under a GTS instance identifier so the gear's plugin selection can discover and bind it. It opens no network listener and exposes no REST surface. Foundational runtime, lifecycle, and integration patterns are inherited from the parent gear ([../../../docs/PRD.md](../../../docs/PRD.md)) and the platform; only plugin-specific constraints are recorded here.

### 3.1 Gear-Specific Environment Constraints

- Requires a PostgreSQL database with the TimescaleDB extension available; the plugin provisions its time-partitioned storage and a declarative retention policy at startup.
- Requires a TLS-capable database endpoint; the plugin refuses non-TLS connections (see `cpt-cf-uc-plugin-nfr-transport-security`).
- Usage records and the usage-type catalog reside in the same database, so referential integrity between them is enforced atomically.
- The plugin is statically linked into the Usage Collector gear process; database deployment topology (HA, sizing, region) follows the operator's TimescaleDB deployment guide.

## 4. Scope

### 4.1 In Scope

- Full implementation of the Usage Collector storage SPI (`cpt-cf-usage-collector-interface-plugin`): single and batch record persistence, point read, keyset-paginated raw list, pushed-down aggregation, event deactivation with a depth-1 cascade, and the full usage-type catalog lifecycle (create, get, list, delete).
- Durable system-of-record storage for usage records in a time-partitioned table keyed on event time.
- In-backend deduplication keyed on the `(tenant_id, gts_id, idempotency_key, created_at)` dedup key (ADR-0014).
- Append-only compensation entries and a one-way, depth-1 deactivation cascade within a single transaction.
- In-backend referential integrity between usage records and the usage-type catalog (a delete of a referenced usage type is rejected).
- Server-side aggregation (SUM / COUNT / MIN / MAX / AVG with grouping) and keyset pagination pushed into the backend.
- A declarative, event-time-based retention policy for usage records.
- Injection-safe translation of the host-supplied filter, aggregation, and pagination into backend queries.
- Publication of the backend's consistency profile as required by the parent query-freshness contract.
- Push-based OpenTelemetry metrics for the plugin's backend-internal operation.
- Runtime discovery/registration and operator configuration of the connection, pool bounds, retention window, and GTS instance selection (vendor, priority).

### 4.2 Out of Scope

- Any product-level behavior owned by the gear core — authentication, PDP authorization, attribution and shape validation, idempotency-key presence enforcement, counter/gauge semantics, and metadata closed-shape validation. These are inherited from the parent gear, not re-implemented here.
- Columnar compression of aging chunks and continuous-aggregate (rollup) tables — deferred post-v1; additive and non-breaking to the SPI.
- Permanent (unbounded) idempotency-key preservation — this backend preserves dedup keys only for the configured retention window (see `cpt-cf-uc-plugin-fr-idempotent-dedup`, [§12](#12-risks), [§13](#13-open-questions)).
- Multi-region replication and cross-region topology — governed by the operator's TimescaleDB deployment and the parent gear's deferred multi-region item.
- Storage backends other than PostgreSQL/TimescaleDB.
- Any REST or network-exposed surface — the plugin exposes only the in-process SPI.

## 5. Functional Requirements

> **Testing strategy**: All requirements are verified via automated tests (unit and integration) unless otherwise specified. Document a verification method only where a non-test approach (analysis, inspection, demonstration) applies. Each requirement lists the gear-level requirement it realizes; there is no plugin-level UPSTREAM_REQS document, so no `Covers` field is used.

### 5.1 Record Persistence

#### Record Persistence (Single and Batch)

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-record-persistence`

The plugin **MUST** persist usage records supplied by the host, both singly and as a batch. A batch **MUST** return one result per input record, positionally aligned to input order, and a conflict or rejection on one record **MUST NOT** fail the others. Caller- and gateway-supplied values (including `metadata`) **MUST** be stored verbatim, without transformation or interpretation.

- **Rationale**: Faithful, order-preserving persistence is the backend's core responsibility and the foundation of every downstream query and aggregate.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-record-metadata`

#### Idempotent Deduplication

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-idempotent-dedup`

The plugin **MUST** deduplicate records in the backend on the dedup key `(tenant_id, gts_id, idempotency_key, created_at)`. On a duplicate dedup key whose caller-supplied canonical fields are identical, the plugin **MUST** return the stored record (silent absorb); on a duplicate dedup key whose canonical fields differ, the plugin **MUST** return an idempotency-conflict error. Concurrent same-dedup-key submissions **MUST** be serialized so exactly one record is created. Idempotency-key presence is enforced upstream by the gear core and **MUST NOT** be re-validated here.

- **Rationale**: At-least-once emission from callers requires the storage boundary to be the exactly-once authority. Anchoring dedup on the 4-tuple that includes `created_at` is required by the time-partitioned backend and adopted as the canonical identity (ADR-0014); a same-key submission with a different `created_at` is therefore a distinct record, consistent with the parent contract.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-idempotency`
- **Note**: Preservation of the dedup key is **retention-bounded**, not permanent — a deliberate narrowing of the parent's unbounded-window obligation (`cpt-cf-usage-collector-fr-idempotency`). See [§12](#12-risks) and [§13](#13-open-questions).

#### Compensation Persistence

- [x] `p2` - **ID**: `cpt-cf-uc-plugin-fr-compensation-persistence`

The plugin **MUST** persist a compensation entry — a caller-supplied signed value with a reference to the corrected record — verbatim through the same path used for ordinary records, with no dedicated compensate operation. The plugin **MUST NOT** compute or validate netting.

- **Rationale**: Corrections are append-only (ADR-0008); the backend stores what the caller supplies and never derives business deltas, so no dedicated storage operation is required.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-usage-compensation`

#### Event Deactivation (Depth-1, Atomic)

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-deactivation`

The plugin **MUST** deactivate a record as a one-way transition of its status from active to inactive, flipping the target record and its depth-1 active compensations in a single atomic transaction and modifying no other field. Deactivating a missing record **MUST** return a not-found error; deactivating an already-inactive record **MUST** return an already-inactive error.

- **Rationale**: Deactivation is a monotonic status change (ADR-0005); the single-transaction cascade preserves the post-correction sum invariant and never leaves a record and its offsets in a split state.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-event-deactivation`

### 5.2 Query & Aggregation

#### Pushed-Down Aggregation

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-aggregated-query`

The plugin **MUST** execute aggregation (SUM, COUNT, MIN, MAX, AVG) with grouping over the requested dimensions inside the backend, applying the host-supplied filter and scope, and return the aggregated result. Aggregation **MUST** operate over the active-row set as defined by the gear's aggregation contract, and the plugin **MUST NOT** return raw rows to the host for client-side aggregation.

- **Rationale**: Pushing aggregation into the backend is how the plugin meets the parent query-latency NFR (`cpt-cf-usage-collector-nfr-query-latency`) over large time ranges without a downstream aggregation layer.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-query-aggregation`

#### Keyset-Paginated Raw Query

- [x] `p2` - **ID**: `cpt-cf-uc-plugin-fr-raw-query`

The plugin **MUST** return raw usage records as keyset-paginated pages that honor the host-supplied order and cursor, indicating whether a further page exists and providing a cursor for it. The same keyset pagination **MUST** apply to usage-type catalog listing. The plugin **MUST NOT** widen the host-supplied filter, and **MUST** reject an order key on a field that may be absent, so that no matching record is silently dropped.

- **Rationale**: Realizes the persistence side of `cpt-cf-usage-collector-fr-query-raw`; keyset pagination bounds query cost at the throughput envelope, and preserving the host filter keeps tenant scoping intact.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-query-raw`

### 5.3 Usage-Type Catalog

#### Usage-Type Catalog Storage

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-usage-type-catalog`

The plugin **MUST** be the sole store for the usage-type catalog, implementing create (rejecting a duplicate identifier with an already-exists error), point read (absent returns a not-found error), keyset-paginated list, and delete. The plugin **MUST** store the usage type's kind and declared metadata-key set verbatim and **MUST NOT** derive or validate semantics.

- **Rationale**: The plugin backend holds the sole usage-type catalog (ADR-0012); records reference types by identifier, so the catalog must be authoritative and consistently addressable, and co-locating it with usage records lets referential integrity be enforced natively.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-usage-type-registration`, `cpt-cf-usage-collector-fr-usage-type-existence-and-semantics`

#### In-Backend Referential Integrity

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-referential-integrity`

The plugin **MUST** enforce, in the backend, that a usage type referenced by any usage record cannot be deleted: the rejection **MUST** be atomic with the delete attempt and surfaced as a usage-type-referenced error, and orphaned usage records **MUST** be structurally impossible.

- **Rationale**: Enforcing the invariant in the backend — rather than by a gear-side pre-read — closes the check-then-act race and makes the guarantee unconditional even for a caller bypassing the gateway (ADR-0012).
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-usage-type-deletion`, `cpt-cf-usage-collector-fr-usage-type-existence-and-semantics`

### 5.4 Data Lifecycle

#### Time-Based Retention

- [x] `p2` - **ID**: `cpt-cf-uc-plugin-fr-retention`

The plugin **MUST** register, idempotently at startup, a declarative retention policy that drops usage-record chunks whose event-time window lies wholly outside the configured retention window (default 365 days), evaluated on event time. The plugin **MUST NOT** issue row-level deletes for expiry. The usage-type catalog is reference data and **MUST NOT** be retention-bounded.

- **Rationale**: Time-based retention bounds storage growth for the append-heavy time-series workload without a gear-side delete path. The parent gear defers gear-level retention; this plugin provides retention as a backend capability. Retention interacts with idempotency-key preservation — see `cpt-cf-uc-plugin-fr-idempotent-dedup`, [§12](#12-risks), and [§13](#13-open-questions).
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`, `cpt-cf-usage-collector-actor-platform-operator`
- **Note**: Operators MUST size the retention window to exceed the maximum client replay and backfill horizon, since dedup-key preservation is bounded by it.

#### Self-Provisioned Schema

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-schema-provisioning`

The plugin **MUST** provision and evolve its own schema idempotently at startup, before serving traffic, so deployment requires no manual database setup and a restart re-runs provisioning as a no-op.

- **Rationale**: A backend must self-provision deterministically so deployment is turnkey and restarts are safe.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-pluggable-storage`
- **Note**: The Usage Collector is pre-release with no existing installations or data; this is forward schema setup only and carries no obligation to migrate data from a prior release.

### 5.5 Plugin Integration & Error Contract

#### GTS-Scoped Backend Registration

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-registration`

At startup the plugin **MUST** create its connection pool, provision its schema, and register itself as a scoped SPI client under a GTS instance identifier via the platform registry, carrying its configured vendor and priority, so the Usage Collector's plugin selection can discover and bind it. The plugin **MUST NOT** decide whether it is the active backend — selection is host-side.

- **Rationale**: Realizes the discovery half of `cpt-cf-usage-collector-fr-pluggable-storage` and the parent registry contract (`cpt-cf-usage-collector-contract-gts-registry`); operator configuration binds the active backend without code changes.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`, `cpt-cf-usage-collector-actor-platform-operator`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-pluggable-storage`

#### Typed Error Classification

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-fr-error-classification`

The plugin **MUST** return the SPI's error vocabulary and classify every backend error as transient (retryable) or internal (non-retryable), plus the typed domain variants defined by the SPI, so the host applies retry and fail-closed behavior without backend-specific parsing. A malformed or unauthorized call reaching the SPI is a host-contract breach and **MUST** surface as internal.

- **Rationale**: A stable, classified error vocabulary lets the host make retry and failure decisions uniformly across any backend, decoupling host behavior from backend-specific errors.
- **Actors**: `cpt-cf-uc-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-nfr-plugin-contract-stability`

## 6. Non-Functional Requirements

> **Global baselines**: Project- and gear-wide NFRs are defined at those levels — see the gear PRD ([../../../docs/PRD.md](../../../docs/PRD.md)) and gear DESIGN ([../../../docs/DESIGN.md](../../../docs/DESIGN.md)). Only plugin-specific NFRs — those that realize a gear NFR at the storage tier or that are standalone to this backend — appear below. Architecture allocation for each is in the plugin DESIGN.md.

### 6.1 Gear-Specific NFRs

#### Aggregation Query Latency

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-nfr-query-latency`

Aggregation queries over a 30-day range for a single tenant **MUST** complete within 500ms at p95, measured against the bound backend under the parent gear's load envelope (`cpt-cf-usage-collector-nfr-throughput-profile`).

- **Threshold**: p95 ≤ 500ms for a 30-day single-tenant aggregation within the parent throughput-profile envelope.
- **Rationale**: This plugin is the allocation target for the parent's query-latency NFR (`cpt-cf-usage-collector-nfr-query-latency`).
- **Architecture Allocation**: See DESIGN.md §1.2 (NFR Allocation) and §4 (Observability, SLO Summary).

#### Ingestion Throughput

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-nfr-ingestion-throughput`

The plugin **MUST** sustain the parent gear's ingestion envelope — ≥ 10,000 records/sec sustained, with the burst headroom defined by `cpt-cf-usage-collector-nfr-throughput-profile` — through the batch write path, without the storage tier breaching the parent ingestion-latency budget (`cpt-cf-usage-collector-nfr-ingestion-latency`).

- **Threshold**: ≥ 10,000 records/sec sustained through the batch write path within the parent throughput-profile envelope.
- **Rationale**: Allocation target for `cpt-cf-usage-collector-nfr-throughput`.
- **Architecture Allocation**: See DESIGN.md §1.2 (NFR Allocation).

#### SPI Conformance & Contract Stability

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-nfr-spi-stability`

The plugin **MUST** implement the storage SPI exactly as declared by the SDK, with changes additive-only within a major version. Conformance **MUST** be verifiable at build time, so a drift between the SPI and this backend is caught before release.

- **Threshold**: Build-time conformance to the SDK SPI; no breaking change within a major SPI version.
- **Rationale**: Allocation target for `cpt-cf-usage-collector-nfr-plugin-contract-stability`; the host binds the backend by contract, so the contract must not silently drift.
- **Architecture Allocation**: See DESIGN.md §2.1 (Design Principles — SPI Conformance).

#### Transport & Query Security

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-nfr-transport-security`

Database connections **MUST** require TLS, and the database connection string — which embeds credentials — **MUST NOT** appear in logs, error messages, or debug output. Translation of the host-supplied query into the backend **MUST** be injection-safe: no caller-supplied string is admitted into query text — comparison values are passed as bound parameters, and any caller-influenced identifier is resolved through a closed allowlist and rejected if unrecognized.

- **Threshold**: Zero non-TLS connections; zero credential disclosures in emitted diagnostics; no caller-supplied string reaches query text as a literal or identifier.
- **Rationale**: The plugin is the only component in the gear + plugin split that holds a database credential, opens a connection to the store, and translates untrusted query shapes; transport confidentiality, credential non-disclosure, and injection safety are its security obligations. Authentication and authorization of callers remain gear-core concerns (see [§6.2](#62-nfr-exclusions)).
- **Architecture Allocation**: See DESIGN.md §2.2 (Injection-Safe Query Translation) and §4 (Non-Applicable Design Domains — Security).

#### Backend Consistency Profile

- [x] `p1` - **ID**: `cpt-cf-uc-plugin-nfr-consistency-profile`

The plugin **MUST** publish its consistency profile as required by the parent gear's query-freshness contract (`cpt-cf-usage-collector-nfr-query-freshness`). On a single-node PostgreSQL/TimescaleDB deployment the plugin **MUST** provide read-after-write visibility of a committed record through the query surfaces (a stronger ceiling than the gear's eventual-consistency floor); a deployment that introduces read replicas **MUST** document the resulting staleness bound.

- **Threshold**: A published consistency profile per deployment topology; single-node ceiling equals read-after-write.
- **Rationale**: The parent floor is "eventually consistent, no upper bound"; `cpt-cf-usage-collector-nfr-query-freshness` obliges each plugin to publish its actual ceiling so consumers couple to it consciously.
- **Architecture Allocation**: See the parent gear DESIGN.md §3.10 (Consistency Contract) and ADR-0011; the published profile is carried in the plugin's deployment guide.

#### Operational Visibility

- [x] `p2` - **ID**: `cpt-cf-uc-plugin-nfr-operational-visibility`

The plugin **MUST** emit push-based OpenTelemetry metrics for its backend-internal operation — at minimum ingestion latency, deduplication outcomes, query latency, connection-pool saturation, backend error rate by classification, and backend readiness — under its own metric sub-namespace, distinct from the gear's request-path signals. Unbounded identifiers **MUST NOT** be used as metric labels.

- **Threshold**: The metric set enumerated in DESIGN.md Observability is emitted, with bounded label cardinality, so operator dashboards and alerts can be built from it.
- **Rationale**: Allocation target for `cpt-cf-usage-collector-nfr-operational-visibility`; the plugin owns the backend-internal series the gear cannot see.
- **Architecture Allocation**: See DESIGN.md §4 (Observability).

### 6.2 NFR Exclusions

- **Authentication, authorization, and attribution enforcement**: Not applicable as a plugin concern — the Usage Collector core enforces authentication, PDP authorization, tenant/subject/resource attribution, and shape validation before every SPI call (cross-reference `cpt-cf-usage-collector-fr-ingestion-authorization`, `cpt-cf-usage-collector-fr-tenant-isolation`). The plugin's only security obligations are transport security and injection safety ([§6.1](#61-gear-specific-nfrs)).
- **Data classification**: Not applicable at the plugin — it stores caller-supplied metadata verbatim and performs no classification; `cpt-cf-usage-collector-fr-data-classification` is gear-owned.
- **End-to-end ingestion latency and availability**: Not owned at plugin level. `cpt-cf-usage-collector-nfr-ingestion-latency` and `cpt-cf-usage-collector-nfr-availability` are gear-level, end-to-end NFRs realized jointly by the gear and the active backend; the plugin's contribution is bounded by its throughput and query-latency allocations.
- **Permanent idempotency-key preservation**: Explicitly not provided — dedup-key uniqueness is retention-bounded, not unbounded (see [§5.1](#51-record-persistence), [§12](#12-risks), [§13](#13-open-questions)).
- **Safety, UI accessibility/usability, internationalization, and privacy/regulatory conformance as standalone obligations**: Not applicable — inherited from the parent gear's identical exclusions (parent PRD [§6.2](../../../docs/PRD.md)). The plugin is server-side infrastructure with no UI, holding only opaque identifiers and opaque metadata passed through from the gear.
- **Disaster recovery (RPO/RTO) and backup/restore**: Not applicable as standalone plugin requirements — governed by the operator's TimescaleDB/PostgreSQL deployment posture.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Storage SPI Implementation

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-interface-storage-spi`

- **Type**: In-process async Rust trait implementation of the storage SPI (`UsageCollectorPluginV1`).
- **Stability**: stable (V1).
- **Description**: The plugin's sole public surface — record persistence (single and batch), point read, keyset-paginated raw list, pushed-down aggregation, event deactivation with a depth-1 cascade, and the usage-type catalog lifecycle. Registered as a scoped client under a GTS instance identifier and consumed in-process by the Usage Collector core; there is no REST or network-exposed surface. Realizes the gear's Plugin SPI (`cpt-cf-usage-collector-interface-plugin` and its `cpt-cf-usage-collector-contract-storage-plugin`); the technical realization is defined in DESIGN.md (`cpt-cf-uc-plugin-interface-spi`).
- **Breaking Change Policy**: Follows the SPI's versioning (`cpt-cf-usage-collector-nfr-plugin-contract-stability`) — additive within a major version; breaking changes are coordinated through the SDK crate.

### 7.2 External Integration Contracts

#### PostgreSQL / TimescaleDB Backend Contract

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-contract-timescaledb`

- **Direction**: required from external system (the operator-provisioned database).
- **Protocol/Format**: PostgreSQL wire protocol over a TLS-required connection; requires the TimescaleDB extension (hypertables, declarative retention).
- **Compatibility**: The plugin provisions and evolves its own schema idempotently at startup; it requires a PostgreSQL version compatible with the TimescaleDB features it uses.

#### GTS Registration Contract

- [ ] `p2` - **ID**: `cpt-cf-uc-plugin-contract-gts-registration`

- **Direction**: provided by library (to the platform registry).
- **Protocol/Format**: Registers a scoped SPI client under a GTS instance identifier, carrying `vendor` and `priority` selection metadata.
- **Compatibility**: Realizes the gear's `cpt-cf-usage-collector-contract-gts-registry`; the GTS spec identity is fixed by the SDK.

## 8. Use Cases

### Ingest a Usage Record with Idempotent Dedup

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-usecase-ingest-dedup`

**Actor**: `cpt-cf-uc-plugin-actor-plugin-host`

**Preconditions**:

- The plugin is the bound backend and its schema is provisioned; the referenced usage type exists.
- The call arrives already authorized and structurally validated, carrying the gateway-derived record id and idempotency key.

**Main Flow**:

1. The core calls the SPI to persist a usage record.
2. The plugin attempts an insert keyed on the dedup identity.
3. On no conflict, the record is stored and returned.

**Postconditions**:

- The record is durably stored and visible to subsequent dedup checks within the retention window.

**Alternative Flows**:

- **Exact-equality retry**: the dedup key already exists with identical canonical fields — the stored record is returned (silent absorb).
- **Canonical mismatch**: the dedup key exists with differing canonical fields — an idempotency-conflict error is returned.
- **Same key, different `created_at`**: a distinct dedup key — a new record is created (4-tuple identity, ADR-0014).
- **Transient backend error**: returned to the host classified as retryable.

### Delete a Referenced Usage Type

- [ ] `p2` - **ID**: `cpt-cf-uc-plugin-usecase-delete-referenced-type`

**Actor**: `cpt-cf-uc-plugin-actor-plugin-host`

**Preconditions**:

- A usage type exists in the catalog and the call is authorized.

**Main Flow**:

1. The core calls the SPI to delete a usage type by identifier.
2. The plugin attempts the delete.
3. If no usage record references the type, it is deleted.

**Postconditions**:

- An unreferenced type is removed; its identifier becomes available for re-registration.

**Alternative Flows**:

- **Referenced type**: the backend rejects the delete because records still reference the type; the plugin returns a usage-type-referenced error and the type remains.
- **Missing type**: a not-found error is returned.

### Bind the Backend at Host Startup

- [ ] `p2` - **ID**: `cpt-cf-uc-plugin-usecase-bind-startup`

**Actor**: `cpt-cf-uc-plugin-actor-plugin-host`

**Preconditions**:

- Valid plugin configuration (connection, pool bounds, retention, vendor/priority) is provided; the database is reachable.

**Main Flow**:

1. The plugin loads and validates config and creates its connection pool.
2. The plugin provisions its schema idempotently.
3. The plugin registers itself as a scoped SPI client under a GTS instance identifier, carrying vendor/priority.
4. The host discovers and binds the backend by vendor/priority.

**Postconditions**:

- The backend is bound and ready; the backend-readiness signal is set.

**Alternative Flows**:

- **Invalid config / unreachable database / missing TimescaleDB extension**: startup fails fast; the plugin does not register and the host does not bind it.

## 9. Acceptance Criteria

- [ ] The plugin implements every storage SPI method and conforms to the SDK SPI at build time, and does not depend on the host gear crate.
- [ ] A usage record is persisted and retrievable; a second submission with the same dedup key and identical canonical fields yields a single stored record (silent absorb).
- [ ] A submission with the same dedup key but differing canonical fields is rejected with an idempotency-conflict error.
- [ ] A submission with the same idempotency key but a different `created_at` is stored as a distinct record (4-tuple identity, ADR-0014).
- [ ] Batch ingestion returns one outcome per input record in input order; a conflict on one record does not fail the others.
- [ ] A compensation entry (signed value plus reference to the corrected record) is persisted verbatim through the ingestion path without mutating the corrected record.
- [ ] Deactivating an active record flips it and its depth-1 active compensations to inactive in a single transaction with no other field changed; a missing target returns not-found and an already-inactive target returns already-inactive.
- [ ] Aggregation (SUM/COUNT/MIN/MAX/AVG) with grouping is computed in the backend over the active-row set and honors the host filter and scope.
- [ ] Raw list and catalog list return keyset-paginated pages honoring the supplied order and cursor.
- [ ] Deleting a usage type referenced by any usage record is rejected atomically with a usage-type-referenced error; deleting an unreferenced type succeeds; creating a usage type whose identifier already exists returns an already-exists error.
- [ ] Usage-record chunks whose event-time window lies wholly outside the configured retention window are dropped by the declarative policy; the catalog is not retention-bounded; no row-level expiry deletes are issued.
- [ ] Database connections require TLS; the connection string and credentials never appear in logs, errors, or debug output; no caller-supplied string reaches query text as a literal or identifier.
- [ ] Aggregation over a 30-day single-tenant range completes within 500ms at p95 within the parent throughput-profile envelope, and the batch write path sustains ≥ 10,000 records/sec.
- [ ] The plugin publishes its consistency profile per deployment topology; a single-node deployment provides read-after-write visibility of a committed record.
- [ ] The plugin emits the enumerated OpenTelemetry metrics under its sub-namespace, including a backend-readiness signal.
- [ ] The plugin registers under a GTS instance identifier with its configured vendor and priority and does not self-select as the active backend.

## 10. Dependencies

| Dependency                                                                         | Description                                                                                                  | Criticality |
| ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ----------- |
| usage-collector-sdk                                                                | Storage SPI trait, domain models, error vocabulary, and GTS plugin spec — the contract the plugin implements | p1          |
| PostgreSQL + TimescaleDB extension                                                 | Durable system of record; provides time-partitioning (hypertables) and declarative retention                 | p1          |
| types-registry (+ ClientHub)                                                       | Publishes the plugin's GTS instance for host discovery and scoped binding                                    | p1          |
| Platform registry / orchestration (`cpt-cf-usage-collector-contract-gts-registry`) | Operator-driven active-backend selection                                                                     | p1          |

## 11. Assumptions

- The Usage Collector core performs all authentication, PDP authorization, attribution and shape validation, and semantics decisions before every SPI call; the plugin trusts each call as authorized and structurally valid.
- The gateway derives each record's id and idempotency key; the plugin stores them verbatim and does not mint identity.
- The operator provisions a PostgreSQL database with the TimescaleDB extension and a TLS-capable endpoint, sized for the deployment's throughput and retention.
- Usage records and the usage-type catalog reside in the same database, so referential integrity between them is enforced atomically.
- The operator sizes the retention window to exceed the maximum client replay and backfill horizon (see [§12](#12-risks)).

## 12. Risks

| Risk                                                                                     | Impact                                                                                                                                              | Mitigation                                                                                                                                                                                                       |
| ---------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Retention window shorter than the maximum client replay/backfill horizon                 | A dedup key whose chunk was dropped is accepted as a fresh insert, admitting a duplicate the parent's unbounded-window contract would have absorbed | Operators size the retention window above the maximum replay/backfill horizon; the divergence is documented ([§5.1](#51-record-persistence)) and tracked for upstream reconciliation ([§13](#13-open-questions)) |
| Single-node PostgreSQL write ceiling below the ingestion envelope on undersized hardware | Ingestion-throughput NFR missed                                                                                                                     | Batch write path and time-partitioning; operator sizing per the deployment guide; high-volume routing at the gear                                                                                                |
| High-cardinality aggregation exceeds the 500ms p95 budget                                | Slow dashboard and billing queries                                                                                                                  | Time-partitioned indexing; deferred continuous-aggregate rollups can be added additively                                                                                                                         |
| Read-replica deployments introduce query staleness                                       | Consumers coupled to read-after-write observe stale reads                                                                                           | Publish the per-topology consistency profile (`cpt-cf-uc-plugin-nfr-consistency-profile`); consumers couple only to the published ceiling                                                                        |

## 13. Open Questions

- Reconcile this backend's **retention-bounded** dedup-key preservation with the parent gear's **unbounded** idempotency-key obligation — the `cpt-cf-usage-collector-fr-idempotency` window and the `cpt-cf-usage-collector-nfr-query-freshness` floor's permanently-visible dedup-tuple clause, plus ADR-0004 and ADR-0011. Resolution is either to narrow the gear contract to "retention-bounded" for time-series backends, or to require this plugin to preserve dedup keys beyond chunk retention. Tracked in DESIGN.md §2.2; not resolved in this PRD.

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **Parent Gear PRD (authoritative)**: [../../../docs/PRD.md](../../../docs/PRD.md)
- **Parent Gear DESIGN**: [../../../docs/DESIGN.md](../../../docs/DESIGN.md)
- **Plugin SPI reference**: [../../../docs/plugin-spi.md](../../../docs/plugin-spi.md)
- **Domain model**: [../../../docs/domain-model.md](../../../docs/domain-model.md)
- **ADRs (gear-level)**: [../../../docs/ADR/](../../../docs/ADR/) — notably [`0004-mandatory-idempotency`](../../../docs/ADR/0004-mandatory-idempotency.md), [`0005-monotonic-deactivation`](../../../docs/ADR/0005-monotonic-deactivation.md), [`0008-usage-compensation`](../../../docs/ADR/0008-usage-compensation.md), [`0011-consistency-contract`](../../../docs/ADR/0011-consistency-contract.md), [`0012-unified-plugin-catalog-and-gts-id-reference`](../../../docs/ADR/0012-unified-plugin-catalog-and-gts-id-reference.md), [`0013-deterministic-usage-record-id`](../../../docs/ADR/0013-deterministic-usage-record-id.md), [`0014-created-at-in-dedup-identity`](../../../docs/ADR/0014-created-at-in-dedup-identity.md)
