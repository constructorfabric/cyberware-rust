Created: 2026-08-03

# PRD — ClickHouse Usage Collector Storage Plugin

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
- [8. Use Cases](#8-use-cases)
  - [Ingest a Usage Record Referencing a Concurrently-Deleted Usage Type](#ingest-a-usage-record-referencing-a-concurrently-deleted-usage-type)
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

The ClickHouse Usage Collector Storage Plugin (`cf-gears-clickhouse-usage-collector-plugin`) is a storage backend for the Usage Collector gear. It implements the Usage Collector storage SPI (`cpt-cf-usage-collector-interface-plugin`) on top of ClickHouse, a columnar OLAP database, and is a durable system of record for both usage records and the usage-type catalog — a second SPI-conformant backend alongside `timescaledb-usage-collector-plugin`.

This PRD specifies **only plugin-specific requirements and deviations** for the ClickHouse backend, at the level of **behavior and constraint**, not implementation mechanism — the concrete mechanisms (storage engine choice, SQL shapes, client crate) that satisfy these requirements are DESIGN.md's responsibility, cross-referenced from each requirement below. All product-level requirements — ingestion semantics, the idempotency contract, counter/gauge semantics, attribution, tenant isolation, authorization, the query/aggregation product surface, correction primitives, usage-type lifecycle, and data classification — are defined in the parent gear PRD and are **inherited** by this plugin:

- **Parent PRD (authoritative)**: [../../../docs/PRD.md](../../../docs/PRD.md)

Under the gear + plugin split, the Usage Collector core owns authentication, PDP authorization, attribution and shape validation, idempotency-key presence, and counter/gauge decisions; the plugin is pure persistence and query and receives only already-authorized, structurally-validated calls.

### 1.2 Background / Problem Statement

ADR-0002 (`cpt-cf-usage-collector-adr-pluggable-storage`) requires the Usage Collector to remain backend-agnostic so operators can select the storage technology that fits their workload profile without a coordinated core release. ClickHouse is a columnar OLAP engine whose vectorized execution and native aggregate functions target exactly the read-side NFR this plugin must meet (30-day single-tenant aggregation ≤ 500ms p95, `cpt-cf-usage-collector-nfr-query-latency`) at the platform's ingestion envelope (≥ 10,000 records/sec, `cpt-cf-usage-collector-nfr-throughput`).

ClickHouse trades away the properties the reference TimescaleDB/PostgreSQL plugin relies on for correctness: it has **no multi-statement ACID transactions**, **no row-level locks**, **no native foreign keys**, and **no `INSERT ... ON CONFLICT`**. Every correctness mechanism the reference plugin implements with a single ACID SQL statement — dedup, the depth-1 deactivation cascade, and FK-enforced referential integrity — must be redesigned for a backend whose consistency model is "eventually converges via background merges," not "immediately serializable." This PRD states the resulting behavioral requirements and their honestly-scoped deviations from the reference plugin's guarantees; DESIGN.md documents the concrete mechanisms (engineered to close each gap as far as a non-transactional backend allows) that satisfy them.

### 1.3 Goals (Business Outcomes)

- Provide a production-grade columnar storage backend that satisfies the parent gear's query-latency NFR without a separate downstream aggregation layer, exploiting ClickHouse's vectorized `GROUP BY`/aggregate-function execution. **Verification**: load tests against a bound backend within the parent throughput profile.
- Preserve the SPI's idempotency and referential-integrity contracts to the fullest extent ClickHouse's consistency model allows, with every residual deviation from the reference plugin's DB-enforced guarantees engineered to be as small as achievable and explicitly documented rather than silently narrowed. **Verification**: conformance tests plus documented deviation review.
- Keep all ClickHouse-specific storage logic, schema, and client-library code isolated to this crate so the backend can evolve independently of the host gear and of the TimescaleDB plugin. **Verification**: conformance to the SDK SPI and a dependency check that the crate does not depend on the host gear crate.

All other business and product goals are defined by the parent Usage Collector PRD.

### 1.4 Glossary

The parent gear glossary and the TimescaleDB plugin's glossary are the primary sources of truth for shared terms. The terms below are specific to this backend; see DESIGN.md §3.6/§3.7 for the mechanisms they name.

| Term | Definition |
| --- | --- |
| Versioned row | A backend-internal mechanism (DESIGN.md §3.1/§3.7) by which a status transition or dedup-convergence outcome is represented as a new row rather than an in-place update. |
| Coordination lock | A per-`gts_id` exclusive cluster mutex held via the cluster gear `DistributedLockV1` (DESIGN.md §3.5/§3.6) that serializes `create_usage_record`/`create_usage_records`, `create_usage_type`, and `delete_usage_type` for the same `gts_id` against each other, eliminating (not merely bounding) the concurrent-reference race. |
| Bucket cap | The SPI's `MAX_AGGREGATION_BUCKETS` memory guard (plugin-spi.md Method 3) that every aggregation-capable backend, including this one, must enforce server-side. |

## 2. Actors

This plugin has no direct human actors and shares the TimescaleDB plugin's single system actor:

#### Usage Collector Core (Plugin Host)

**ID**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

- **Role**: The Usage Collector gear core. It invokes this plugin through the storage SPI for all persistence and query operations, performing authentication, PDP authorization, attribution and shape validation, and semantics decisions before every call; the plugin performs storage only.

#### Operator

**ID**: `cpt-cf-uc-ch-plugin-actor-operator`

- **Role**: The platform operator who deploys and configures the plugin (sets `retention_period_secs`, `allow_insecure_http`, `lock_ttl_secs`, etc.) and is responsible for ClickHouse deployment, cluster lock ensemble provisioning, and monitoring the plugin's operational metrics.

#### ClickHouse Server

**ID**: `cpt-cf-uc-ch-plugin-actor-clickhouse`

- **Role**: The external ClickHouse database server that executes the plugin's DDL and DML statements, applies `ReplacingMergeTree` background merges, and enforces the `TTL` retention clause asynchronously.

## 3. Operational Concept & Environment

This plugin operates within the standard Gears ToolKit lifecycle. At startup it creates its ClickHouse client/connection pool, provisions its schema idempotently, and registers itself as a scoped SPI client under a GTS instance identifier so the gear's plugin selection can discover and bind it. It opens no network listener and exposes no REST surface. Foundational runtime, lifecycle, and integration patterns are inherited from the parent gear ([../../../docs/PRD.md](../../../docs/PRD.md)) and the platform; only plugin-specific constraints are recorded here.

### 3.1 Gear-Specific Environment Constraints

- Requires a reachable ClickHouse server (self-hosted or ClickHouse Cloud) reachable over its HTTP interface; the plugin provisions its tables at startup.
- Requires the cluster gear with profile `usage-collector` (typically standalone cache for single-node) to back the per-`gts_id` exclusive coordination lock ([§5](#5-functional-requirements)). This lock is a single exclusive mutex shared by both the create and delete paths, serializing all creates and deletes for the same `gts_id` against each other. It coordinates across concurrently-running **gear process instances**, not across ClickHouse nodes.
- Requires a TLS-capable ClickHouse endpoint for production deployments; the plugin rejects a plaintext `database_url` at startup by default and requires an explicit, logged config override (`allow_insecure_http = true`) to permit one, for development/test only.
- The `database_url` config field embeds the ClickHouse HTTP endpoint, user, password, and database name as a single URL (e.g. `https://user:pass@host:8443/db`). Credentials with URL-reserved characters must be percent-encoded in the URL.
- The plugin exposes two lock-related config fields: `lock_ttl_secs` (the cluster lock lease TTL — must be sized above worst-case critical-section latency, i.e. above the ClickHouse round-trips performed while the exclusive lock is held; the lease is renewed via `ensure_still_held()` immediately before every mutating write) and `lock_timeout_secs` (the maximum wait to acquire the lock before failing closed with `Transient`).
- Usage records and the usage-type catalog reside in the same ClickHouse database, so the referential-integrity emulation ([§5](#5-functional-requirements)) operates entirely within one backend's reach.
- The plugin is statically linked into the Usage Collector gear process; ClickHouse cluster topology (replication, sharding, sizing) follows the operator's ClickHouse deployment guide. This plugin targets a single-shard (optionally replicated) deployment for v1; multi-shard distributed-table topology is out of scope ([§4.2](#42-out-of-scope)). The consistency guarantee this plugin can offer differs materially between the single-node and replicated case — see DESIGN.md §3.8 for the concrete, numeric bounds and [§6.1](#61-gear-specific-nfrs) for the NFR statement.

## 4. Scope

### 4.1 In Scope

- Full implementation of the Usage Collector storage SPI: single and batch record persistence, point read, keyset-paginated raw list, pushed-down aggregation (with the SPI's mandatory result-bucket cap enforced), event deactivation with a depth-1 cascade, and the full usage-type catalog lifecycle (create, get, list, delete).
- Durable system-of-record storage for usage records, structured for efficient dedup point-lookups and time-range scans (DESIGN.md §3.7).
- Application-level deduplication keyed on the deterministic record `id` (itself derived from the `(tenant_id, gts_id, idempotency_key, created_at)` 4-tuple, ADR-0014).
- Append-only compensation entries and a depth-1 deactivation cascade applied as a single atomic write, since ClickHouse has no in-place `UPDATE` suitable for the request path.
- Application-level referential-integrity emulation between usage records and the usage-type catalog, using a per-`gts_id` coordination lock (DESIGN.md §3.5/§3.6) that closes the concurrent-reference window entirely rather than merely bounding it, since ClickHouse has no native foreign key.
- Server-side aggregation (SUM / COUNT / MIN / MAX / AVG with grouping) and keyset pagination pushed into ClickHouse's vectorized execution engine, with the SPI's `MAX_AGGREGATION_BUCKETS` cap enforced server-side.
- A native ClickHouse expiry mechanism providing time-based retention for usage records: a fixed one-year TTL default in the initial DDL, reconciled on every startup to the operator-configured `retention_period_secs`.
- Injection-safe translation of the host-supplied filter, aggregation, and pagination into parameterized ClickHouse queries.
- Push-based OpenTelemetry metrics for the plugin's backend-internal operation, under a distinct `uc_clickhouse_*` sub-namespace, including a detection-backstop signal for the residual referential-integrity race ([§5](#5-functional-requirements)).
- Typed classification of every backend error into the SDK's `UsageCollectorPluginError` vocabulary (Transient vs. Internal, plus the typed domain variants).
- Runtime discovery/registration and operator configuration of the connection, request and lock timeouts, retention window, and GTS instance selection (vendor, priority). Connection-pool sizing is **not** configurable — see [§6.1](#61-gear-specific-nfrs).

### 4.2 Out of Scope

- Any product-level behavior owned by the gear core — authentication, PDP authorization, attribution and shape validation, idempotency-key presence enforcement, counter/gauge semantics, and metadata closed-shape validation. These are inherited from the parent gear, not re-implemented here.
- Multi-shard distributed-table topology, cross-cluster replication configuration, and cluster distributed lock/ZooKeeper coordination for ClickHouse's own replication — governed by the operator's ClickHouse deployment guide, not by this plugin. **Not included in this exclusion**: this plugin's own use of cluster distributed lock as a coordination-lock backend for referential integrity ([§5](#5-functional-requirements)) — that usage is in scope and owned by this plugin, distinct from and unrelated to ClickHouse's own replication coordination.
- Strict, DB-enforced serializability for the dedup path — ClickHouse structurally cannot provide this; the plugin provides the closest achievable approximation and documents the residual race explicitly (see [§5](#5-functional-requirements), DESIGN.md §2.2, §3.6, §3.8). The referential-integrity path, by contrast, achieves the equivalent of DB-enforced serializability via the coordination lock — see [§5](#5-functional-requirements).
- Permanent (unbounded) idempotency-key preservation beyond the configured retention window — the same narrowing the reference plugin already documents (and leaves as an open question) for a time-partitioned backend; see [§13](#13-open-questions).
- Schema evolution beyond the v1 shape (adding/changing columns post-release) — not designed in v1; see [§13](#13-open-questions) and DESIGN.md §4 Deferred.
- Storage backends other than ClickHouse.
- Any REST or network-exposed surface — the plugin exposes only the in-process SPI.

## 5. Functional Requirements

> This PRD documents only requirements that **deviate** from, or add backend-specific detail to, the parent gear's functional requirements and the reference (TimescaleDB) plugin's PRD. Where this plugin's behavior is identical to the reference plugin's stated FR text (e.g. record persistence verbatim-storage, compensation persistence, catalog verbatim storage), that FR is inherited unchanged and is not restated here — see the parent PRD and `timescaledb-usage-collector-plugin/docs/PRD.md` §5 for the full requirement text this plugin also satisfies structurally. Every FR below states the **behavioral requirement and its residual deviation**; the concrete backend mechanism that satisfies it (storage engine choice, exact SQL shapes, client-library calls) is cross-referenced to DESIGN.md rather than restated here, so this document stays at the requirements level.

#### Idempotent Deduplication (ClickHouse deviation)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-fr-idempotent-dedup`

The plugin **MUST** deduplicate records on the deterministic record `id` (derived from the `(tenant_id, gts_id, idempotency_key, created_at)` 4-tuple, ADR-0014) using an application-level mechanism appropriate for a backend without native uniqueness constraints (concrete mechanism: DESIGN.md §3.6 Ingest sequence). On an exact-equality retry the plugin **MUST** return the stored record (silent absorb); on a canonical-field mismatch under the same `id` it **MUST** return an idempotency-conflict error. Unlike the reference plugin, this backend **MUST NOT** claim *DB-enforced* atomic serialization of concurrent same-key submissions: the serialization comes from the per-`gts_id` exclusive coordination lock ([§5](#5-functional-requirements) referential-integrity FR, DESIGN.md §3.5/§3.6), not from ClickHouse. Because every `create_usage_record`/`create_usage_records` call for a `gts_id` acquires that one exclusive lock, two concurrent submissions sharing a dedup identity (which necessarily share a `gts_id`) **MUST** be ordered against each other, so the read-before-insert check is authoritative and a conflicting-field submission is caught as `IdempotencyConflict` rather than lost to a race. The only residual, theoretical exposure is a **hash collision** across *different* `gts_id`s — two callers whose distinct `gts_id`s derive the same record `id`, or whose hashed lock-name leaves collide — which the lock does not order; this narrow residual **MUST** be documented in the plugin's README rather than presented as equivalent to the reference plugin's DB-enforced guarantee.

- **Rationale**: ClickHouse has no `INSERT ... ON CONFLICT` and no row-level locking, so mutual exclusion has to come from outside ClickHouse; the exclusive coordination lock supplies it, and `ReplacingMergeTree` convergence remains a defense-in-depth backstop. Chosen and reviewed in Phase 1's design gate and narrowed further when the coordination lock became a single exclusive mutex per `gts_id` (see DESIGN.md §2.2, §3.6).
- **Actors**: `cpt-cf-uc-ch-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-idempotency`, narrowed per the above and per [§13](#13-open-questions).

#### Event Deactivation (Depth-1, Atomic-as-Single-Write)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-fr-deactivation`

The plugin **MUST** deactivate a record as a one-way transition from active to inactive, flipping the target and every active depth-1 compensation referencing it as a single atomic write (rather than an in-place `UPDATE`, which ClickHouse only supports as an asynchronous background mutation unsuitable for the request path — concrete mechanism: DESIGN.md §3.6 Deactivation sequence). No reader **MUST** ever observe a partially-flipped cascade. Deactivating a missing record **MUST** return a not-found error; deactivating an already-inactive record **MUST** return an already-inactive error. Per the SPI's Method 5 caller-side concurrency rule, the host prevents any new compensation from targeting a record while it is being deactivated before that write ever reaches this plugin, so the plugin's cascade sequence does **not** need to coordinate with an in-flight compensation write — this plugin introduces no additional race here beyond the reference plugin's own depth-1, snapshot-at-read-time cascade scope.

- **Rationale**: Preserves the reference plugin's depth-1, single-write-boundary cascade semantics to the extent ClickHouse's storage model allows; chosen and reviewed in Phase 1's design gate and hardened in a follow-up design review (see DESIGN.md §3.6).
- **Actors**: `cpt-cf-uc-ch-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-event-deactivation`

#### In-Backend Referential Integrity (Application-Emulated, Lock-Protected, Zero-Window)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-fr-referential-integrity`

The plugin **MUST** emulate, at the application level, that a usage type referenced by any usage record cannot be permanently deleted, and **MUST NOT** admit any window in which a concurrent record write can reference a `gts_id` being deleted, per `plugin-spi.md` Method 9's requirement — using a per-`gts_id` **exclusive coordination lock** (a single exclusive mutex shared by all three mutating paths — DESIGN.md §3.5/§3.6) and the three complementary write-path obligations it protects, stated separately below.

**(a) Create-path obligation.** The plugin's own record-write path **MUST** acquire the exclusive coordination lock on the record's `gts_id` and, while holding it, perform its own referential-integrity check immediately before persisting a record, rejecting a reference to an absent or deleted usage type — mirroring the structural role the reference plugin's native foreign key plays at insert time (this is a storage-layer integrity mechanism, not a re-execution of the gateway's business/authorization checks, which remain solely the gateway's responsibility). The create critical section **MUST** call `ensure_still_held()` (lease renew) immediately before the record `INSERT` to guard against lease expiry during the ClickHouse round-trips, aborting with `Transient` on expiry (cluster ADR-002 deviation — see DESIGN.md §2.2).

**(a-ii) Catalog-create obligation.** `create_usage_type` **MUST** acquire the same exclusive per-`gts_id` coordination lock before its pre-existence check and `INSERT` into `usage_type_catalog` — ClickHouse has no native `UNIQUE` constraint, so application-level exclusion via the same mutex is required to prevent concurrent-create races for the same `gts_id` and to ensure that a concurrent `delete_usage_type` cannot race a `create_usage_type`'s INSERT window. **Lock-manager unavailable**: return `Transient` without touching the catalog (fail-closed, same rule as the record-write and delete paths).

**(b) Delete-path obligation.** `delete_usage_type` **MUST** acquire the exclusive coordination lock on the same `gts_id` — which cannot be granted while any other holder of that lock for the same `gts_id` (record create, catalog create, or delete) exists, and which blocks any new create or delete for that `gts_id` from starting until it is released — and only then verify no reference exists before deleting the catalog row (a real row removal), returning a usage-type-referenced error (with no row deleted) if a reference is found. The delete critical section **MUST** likewise call `ensure_still_held()` (lease renew) immediately before the `DELETE FROM` statement, aborting with `Transient` on expiry (cluster ADR-002 deviation — see DESIGN.md §2.2).

Because the lock makes (a) and (b) mutually exclusive for the same `gts_id`, the verify step in (b) is authoritative, not a snapshot-in-time approximation, and **no rollback is needed**: this construction **MUST** eliminate the concurrent-reference race, not merely bound it. If the lock cannot be acquired because the coordination-lock backend is unavailable, the plugin **MUST** fail closed — returning `Transient` from both the create and delete paths — rather than proceeding without the lock.

- **Rationale**: ClickHouse has no native foreign key, `ON DELETE RESTRICT`, or any other primitive that could close this race from within ClickHouse alone; a fixed delay (an earlier design iteration) cannot satisfy `plugin-spi.md` Method 9's "MUST NOT admit a window" because it bounds a probability rather than eliminating the race between two independently-issued operations. A real mutual-exclusion primitive is structurally required, and cluster distributed lock — a coordination service most ClickHouse deployments already run or can run alongside ClickHouse — is the mechanism that provides it (see DESIGN.md §3.5 for the crate/dependency choice and rationale). Chosen in Phase 1's design gate, hardened in a first follow-up review (fencing delay, since superseded), and redesigned in a second follow-up review to close the window exactly (see DESIGN.md §3.6).
- **Actors**: `cpt-cf-uc-ch-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-usage-type-deletion`

#### Time-Based Retention (Native Backend Expiry)

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-fr-retention`

The plugin **MUST** bound `usage_records` storage growth via a native ClickHouse expiry mechanism (concrete mechanism: DESIGN.md §3.7, a `TTL` clause on `usage_records`). Schema provisioning bakes a fixed one-year TTL default into the `CREATE TABLE IF NOT EXISTS` DDL; on every `init`, `ensure_retention_ttl` compares the live TTL interval to the operator-supplied `retention_period_secs` and issues `ALTER TABLE … MODIFY TTL` when they differ (or when TTL is missing). The usage-type catalog **MUST NOT** be retention-bounded.

- **Rationale**: A native, backend-provided expiry mechanism is the direct analog of the reference plugin's declarative TimescaleDB retention policy. The fixed DDL default keeps first provisioning simple and idempotent; startup reconciliation applies config changes across restarts without a manual operator `ALTER` or table recreate (non-TTL schema evolution remains an open question — see [§13](#13-open-questions)).
- **Actors**: `cpt-cf-uc-ch-plugin-actor-plugin-host`, `cpt-cf-usage-collector-actor-platform-operator`

#### Self-Provisioned Initial Schema (No External Migration Framework)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-fr-schema-provisioning`

The plugin **MUST** provision its **initial** schema idempotently at startup, without depending on `sqlx`'s migration framework (ClickHouse has no `sqlx` driver and thus no `sqlx::migrate!` equivalent — concrete mechanism: DESIGN.md §3.2, an embedded SQL file executed as idempotent DDL statements), before serving traffic, so deployment requires no manual database setup and a restart re-runs provisioning as a no-op. This FR covers **initial provisioning only** — the plugin does **not** provide a general schema-evolution (versioned migration) mechanism in v1; see [§13](#13-open-questions).

- **Rationale**: Deployment must remain turnkey despite the absence of a `sqlx`-based migration framework for ClickHouse; chosen and reviewed in Phase 1's design gate (see DESIGN.md §3.2).
- **Actors**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

#### Typed Error Classification

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-fr-error-classification`

The plugin **MUST** classify every backend failure into the SDK's `UsageCollectorPluginError` vocabulary exactly as declared — `Transient` (retryable: connection reset, timeout, transient ClickHouse server error), `Internal` (non-retryable: unclassified or invariant-violating failure), and the typed domain variants `UsageTypeNotFound`, `UsageTypeAlreadyExists`, `UsageTypeReferenced { sample_ref_count }`, `IdempotencyConflict { existing_id }`, `UsageRecordNotFound`, and `UsageRecordAlreadyInactive` — so the host applies retry and fail-closed behavior without ClickHouse-specific parsing. The plugin **MUST NOT** invent new top-level error types. A malformed or unauthorized call reaching the SPI (a host-contract breach) **MUST** surface as `Internal`, never re-validated or silently accepted.

- **Rationale**: Inherited directly from `plugin-spi.md`'s per-method error-taxonomy contract (each method's error variants are tied back to `cpt-cf-usage-collector-fr-pluggable-storage`); a stable, classified error vocabulary lets the host make retry/failure decisions uniformly across backends. See DESIGN.md §4 Observability for the corresponding `uc_clickhouse_backend_errors_total{error_category}` metric.
- **Actors**: `cpt-cf-uc-ch-plugin-actor-plugin-host`
- **Realizes (gear)**: `cpt-cf-usage-collector-fr-pluggable-storage`

## 6. Non-Functional Requirements

> Global baselines are defined at the gear/project level — see the gear PRD ([../../../docs/PRD.md](../../../docs/PRD.md)) and gear DESIGN. Only plugin-specific NFRs, or NFRs whose ClickHouse-specific realization differs materially from the reference plugin's, appear below.

### 6.1 Gear-Specific NFRs

#### Aggregation Query Latency (Columnar Acceleration, Bucket-Capped)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-nfr-query-latency`

Aggregation queries over a 30-day range for a single tenant **MUST** complete within 500ms at p95, measured against the bound backend under the parent gear's load envelope. ClickHouse's columnar storage and vectorized `GROUP BY`/aggregate-function execution are the mechanism relied upon to meet this budget without a separate downstream aggregation layer. Per the SPI's Method 3 pushdown obligation, the plugin **MUST** bound its own grouped result to `MAX_AGGREGATION_BUCKETS + 1` (100,001) buckets server-side and **MUST NOT** materialize an unbounded bucket set even transiently (concrete mechanism: DESIGN.md §3.6, a `LIMIT` clause on the grouped query).

- **Threshold**: p95 ≤ 500ms for a 30-day single-tenant aggregation; result set capped at 100,001 rows.
- **Architecture Allocation**: See DESIGN.md §1.2 (NFR Allocation) and §3.6 (Aggregated Query sequence).

#### Ingestion Throughput (Batch-Amortized Columnar Writes)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-nfr-ingestion-throughput`

The plugin **MUST** sustain the parent gear's ingestion envelope (≥ 10,000 records/sec sustained) through the batch write path. ClickHouse's part-oriented write model favors large batched inserts over many small single-row inserts; the plugin's batch path **MUST** issue one multi-row `INSERT` per batch rather than N single-row inserts, regardless of how many distinct `gts_id`s the batch spans. The referential-integrity coordination lock ([§5](#5-functional-requirements)) **MUST** be acquired once per distinct `gts_id` present in the batch, not once per record, so its coordination-service round-trip is amortized across every record sharing a `gts_id` rather than added to this NFR's per-record budget; a batch is not restricted to a single `gts_id` (concrete mechanism: DESIGN.md §3.6 Batch Ingest sequence).

- **Threshold**: ≥ 10,000 records/sec sustained through the batch write path.
- **Architecture Allocation**: See DESIGN.md §1.2 (NFR Allocation).

#### Workload Isolation

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-nfr-workload-isolation`

The plugin's v1 design uses a single shared client/pool for both ingestion and query (concrete mechanism and contention analysis: DESIGN.md §3.5), and the client crate exposes **no** pool-size bound the plugin could surface as a config field, so this backend's workload isolation is not a configurable property. What the plugin **MUST** do instead is document the resulting starvation risk — a burst of aggregation queries competing with the ingestion write path for the same pool and server — as a known, accepted contention point with an operator-facing mitigation path (server-side settings profiles/quotas, or separate plugin instances against read-replica vs. write-primary endpoints), per `cpt-cf-usage-collector-nfr-workload-isolation`, rather than silently assuming it away.

- **Threshold**: The shared-pool starvation risk is documented with an operator-facing mitigation path; no claim of solved workload isolation, and no promise of a pool-size config field the client crate cannot support.
- **Architecture Allocation**: See DESIGN.md §1.2 (NFR Allocation) and §3.5 (External Dependencies).

#### SPI Conformance & Contract Stability

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-nfr-spi-stability`

The plugin **MUST** implement the storage SPI exactly as declared by the SDK, verifiable at build time.

- **Architecture Allocation**: See DESIGN.md §2.1.

#### Transport & Query Security

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-nfr-transport-security`

ClickHouse connections **MUST** default to requiring TLS in production: a plaintext (`http://`) `database_url` **MUST** be rejected at config-validation time (before any connection is attempted), with an explicit `allow_insecure_http` config override required to permit one for non-TLS development/test use only, logged via `tracing::warn!` on every connection it permits; the connection DSN/credentials **MUST NOT** appear in logs, error messages, or debug output. Translation of the host-supplied query into ClickHouse SQL **MUST** be injection-safe: no caller-supplied string is admitted into query text — comparison values are passed as bound parameters, and any caller-influenced identifier is resolved through a closed allowlist and rejected if unrecognized.

- **Architecture Allocation**: See DESIGN.md §2.2 and the Security subsection of §4 (Additional Context).

#### Backend Consistency Profile (Narrower Than the Reference Plugin, Numerically Bounded)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-nfr-consistency-profile`

The plugin **MUST** publish its consistency profile per the parent gear's query-freshness contract, stated with concrete numeric bounds rather than a vague "eventually consistent": on a **single-node deployment**, the plugin **MUST** provide effectively-immediate read-after-write visibility for any reader; on a **replicated deployment**, the ceiling is bounded by ClickHouse's own replication lag (typically sub-second under healthy operation, operator-monitored via `system.replicas.absolute_delay`), and operators requiring a stronger cross-replica bound **MUST** be directed to configure ClickHouse's native `insert_quorum` write-quorum setting, at a documented throughput cost. This profile **MUST** be documented in the plugin's README and DESIGN.md §3.8, not silently presented as equivalent to the reference plugin's unconditional read-after-write.

- **Architecture Allocation**: See DESIGN.md §3.8 (Consistency & Concurrency).

#### Operational Visibility

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-nfr-operational-visibility`

The plugin **MUST** emit push-based OpenTelemetry metrics for its backend-internal operation under a `uc_clickhouse_*` sub-namespace, distinct from both the gear's request-path signals and the TimescaleDB plugin's `uc_timescaledb_*` series, including a backend-error classification counter (realizing [§5](#5-functional-requirements)'s error-classification FR). (The orphan-detection defense-in-depth counter is **deferred** — see [§9](#9-acceptance-criteria)). Unbounded identifiers **MUST NOT** be used as metric labels.

- **Architecture Allocation**: See DESIGN.md §4 (Observability, Additional Context).

### 6.2 NFR Exclusions

- **Authentication, authorization, and attribution enforcement**, **data classification**, **end-to-end ingestion latency and availability**, and **disaster recovery / backup / restore**: excluded for the same reasons the reference plugin's PRD §6.2 excludes them — these remain gear-level or operator-level concerns, not plugin-level obligations. See `timescaledb-usage-collector-plugin/docs/PRD.md` §6.2 for the full inherited exclusion text.
- **Permanent idempotency-key preservation**: explicitly not provided — dedup-key uniqueness is retention-bounded, not unbounded, matching the reference plugin's own already-accepted narrowing of the parent's obligation; tracked as an open gear-level reconciliation in [§13](#13-open-questions), not treated here as a plugin-level defect.
- **DB-enforced serializable dedup**: explicitly and structurally excluded for this backend (not merely deferred) — ClickHouse cannot provide this primitive. The plugin's approximation and residual deviation are documented in [§5](#5-functional-requirements) above and DESIGN.md, not silently treated as equivalent. (Referential integrity, by contrast, is engineered toward — not merely excluded from — a bounded approximation; see [§5](#5-functional-requirements).)
- **General schema evolution (post-v1 migrations)**: not provided in v1 — see [§13](#13-open-questions).

## 7. Public Library Interfaces

#### Storage SPI Implementation

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-interface-storage-spi`

- **Type**: In-process async Rust trait implementation of the storage SPI (`UsageCollectorPluginV1`).
- **Stability**: stable (V1), identical trait to the reference plugin's implementation.
- **Description**: The plugin's sole public surface. Registered as a scoped client under a GTS instance identifier and consumed in-process by the Usage Collector core; there is no REST or network-exposed surface.

#### ClickHouse Backend Contract

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-contract-clickhouse`

- **Direction**: required from external system (the operator-provisioned ClickHouse server/cluster).
- **Protocol/Format**: ClickHouse HTTP interface (via the `sea-clickhouse` Rust client, crate path `clickhouse`), TLS-preferred.
- **Compatibility**: The plugin provisions its initial schema idempotently at startup (see [§5](#5-functional-requirements) and [§13](#13-open-questions) for the schema-evolution limitation); it requires a ClickHouse version supporting the storage engine, expiry, and map-typed-column features this plugin's schema depends on (DESIGN.md §3.7) — all long-stable ClickHouse capabilities.

#### Coordination Lock Backend Contract

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-contract-coordination-lock`

- **Direction**: required from external system (an operator-provisioned cluster distributed lock ensemble or compatible ZooKeeper cluster).
- **Protocol/Format**: cluster-sdk `DistributedLockV1` over the operator-selected cluster lock backend; used exclusively for the per-`gts_id` exclusive coordination lock — one lock name per `gts_id`, acquired identically by the create and delete paths ([§5](#5-functional-requirements), DESIGN.md §3.5/§3.6) — no application data is stored here.
- **Compatibility**: Any ClickHouse-Keeper-protocol-compatible or standard ZooKeeper ensemble reachable over the network; independent of the ClickHouse server's own version or topology.

#### GTS Registration Contract

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-contract-gts-registration`

- **Direction**: plugin → types-registry / ClientHub.
- **Protocol/Format**: `PluginV1<UsageCollectorPluginSpecV1>` published to `types-registry`, then the `StorageAdapter` registered as a scoped `UsageCollectorPluginV1` client via ClientHub under the GTS instance scope, carrying the configured vendor and priority so the host's plugin selection can discover and bind it.

## 8. Use Cases

### Ingest a Usage Record Referencing a Concurrently-Deleted Usage Type

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-usecase-ingest-vs-concurrent-delete`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Preconditions**: The plugin is the bound backend; a usage type exists in the catalog; a `delete_usage_type` call for that type is in flight or about to start.

**Main Flow**:

1. The core calls the SPI to persist a usage record referencing the usage type.
2. The plugin acquires the exclusive coordination lock on the `gts_id`; if another call (create or delete) for the same `gts_id` currently holds the lock, this call blocks until it releases.
3. Once the exclusive lock is held, the plugin's own referential-integrity check finds the type live (not yet deleted) and proceeds.
4. The record is stored, the lock is released, and the record is returned.

**Postconditions**: The record is durably stored; a subsequent `delete_usage_type` for that type cannot acquire its exclusive lock until this call's exclusive lock is released (in which case its verify step deterministically finds this reference and is rejected as `UsageTypeReferenced`), or, if the delete's exclusive lock was already held when this call started, this call blocks at step 2 until the delete completes and observes the delete's outcome directly (row deleted → `UsageTypeNotFound`) — there is no window in which the delete's verify step can miss this reference.

**Alternative Flows**:

- **Type already deleted**: the plugin's referential-integrity check finds the type absent and rejects the record with `UsageTypeNotFound`, mirroring the reference plugin's FK-violation mapping.
- **Coordination-lock backend unavailable**: the exclusive-lock acquisition in step 2 cannot complete within its configured timeout; the plugin fails closed and returns `Transient` rather than proceeding without the lock.

### Ingest a Usage Record with Idempotent Dedup

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-usecase-ingest-dedup`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Preconditions**: The plugin is the bound backend and its schema is provisioned; the referenced usage type exists. The call arrives already authorized and structurally validated, carrying the gateway-derived record id and idempotency key.

**Main Flow**:

1. The core calls the SPI to persist a usage record.
2. The plugin's referential-integrity check passes; the plugin's dedup point-lookup finds no existing row for the record's deterministic id.
3. The record is stored and returned.

**Postconditions**: The record is durably stored and visible to subsequent dedup checks within the retention window.

**Alternative Flows**:

- **Exact-equality retry**: the dedup identity already exists with identical canonical fields — the stored record is returned (silent absorb).
- **Canonical mismatch**: the dedup identity exists with differing canonical fields — an idempotency-conflict error is returned, outside the documented narrow concurrent-race window ([§5](#5-functional-requirements)).
- **Transient backend error**: returned to the host classified as retryable (`Transient`).

### Delete a Referenced Usage Type

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-usecase-delete-referenced-type`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Preconditions**: A usage type exists in the catalog and the call is authorized; at least one usage record references it.

**Main Flow**:

1. The core calls the SPI to delete a usage type by identifier.
2. The plugin acquires the exclusive coordination lock on the `gts_id`, waiting for any in-flight `create_usage_record`/`create_usage_records` call's lock on the same `gts_id` to release (both paths share the same exclusive mutex name).
3. Once the exclusive lock is held (no concurrent create for this `gts_id` can be in flight), the plugin confirms the type exists, then probes for referencing records.
4. The probe finds a reference; the plugin releases the lock without deleting the row.

**Postconditions**: The type remains in the catalog and available; no reference was ever at risk of being silently orphaned, since no concurrent create could have raced this delete for the same `gts_id`.

**Alternative Flows**:

- **Unreferenced type**: no reference is found; the row is deleted (a real row removal, not a tombstone marker) and the lock released; the type is gone; its identifier becomes available for re-registration.
- **Missing type**: a not-found error is returned immediately, before the lock is released and before any row is deleted.
- **Coordination-lock backend unavailable**: the exclusive-lock acquisition in step 2 cannot complete within its configured timeout; the plugin fails closed and returns `Transient` rather than proceeding without the lock.

### Bind the Backend at Host Startup

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-usecase-bind-startup`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Preconditions**: Valid plugin configuration (connection, request timeout, retention, coordination-lock TTL/timeout, vendor/priority) is provided; ClickHouse is reachable. The cluster `usage-collector` lock profile is expected to be provisioned for later use, but is **not** probed during `init` (cluster backends register in `start`, after this plugin's `init`).

**Main Flow**:

1. The plugin loads and validates config, creates its ClickHouse client, and constructs its Coordination Lock Manager (`LockManager`) for lazy `DistributedLockV1` resolution on first acquire — no Keeper client and no lock-backend probe at startup.
2. The plugin provisions its initial schema idempotently (`CREATE TABLE IF NOT EXISTS` with a fixed one-year TTL default on `usage_records`), then reconciles the live TTL to `retention_period_secs` via `ensure_retention_ttl` (`ALTER TABLE … MODIFY TTL` when the interval differs or TTL is missing).
3. The plugin registers itself as a scoped SPI client under a GTS instance identifier, carrying vendor/priority.
4. The host discovers and binds the backend by vendor/priority.

**Postconditions**: The backend is bound and ready; the backend-readiness signal is set.

**Alternative Flows**:

- **Invalid config / unreachable ClickHouse / schema provisioning failure**: startup fails fast; the plugin does not register and the host does not bind it.
- **Unbound or unavailable `usage-collector` lock profile**: does not fail startup; the first create/delete lock acquire fails closed with `Transient`.

## 9. Acceptance Criteria

- [x] The plugin implements every storage SPI method and conforms to the SDK SPI at build time, and does not depend on the host gear crate.
- [x] A usage record is persisted and retrievable; a second submission with the same dedup identity and identical canonical fields yields a single stored record (silent absorb).
- [x] A submission with the same dedup identity but differing canonical fields is rejected with an idempotency-conflict error outside the documented narrow race window.
- [x] Batch ingestion returns one outcome per input record in input order.
- [x] A record submitted against a usage type absent from (including previously deleted from) the catalog is rejected with `UsageTypeNotFound` at the plugin's own referential-integrity check, without depending on the gateway's earlier `get_usage_type` call.
- [x] Deactivating an active record flips it and its depth-1 active compensations to inactive as a single atomic write; a missing target returns not-found and an already-inactive target returns already-inactive; no test observes a partially-flipped cascade under concurrent load.
- [x] Aggregation (SUM/COUNT/MIN/MAX/AVG) with grouping is computed in ClickHouse over the active-row set, honors the host filter and scope, and never returns more than `MAX_AGGREGATION_BUCKETS + 1` (100,001) grouped rows even when the underlying data would produce more.
- [x] Deleting a usage type referenced by any usage record is rejected via the lock-protected verify-then-delete mechanism with a usage-type-referenced error; deleting an unreferenced type succeeds and removes the row for real (no tombstone flag or marker row); a reference-creation attempt that starts concurrently with an in-flight delete for the same `gts_id` is deterministically ordered (never silently missed) by the single exclusive coordination lock (both paths share the same mutex name, so concurrent creates also serialize), proven under injected concurrency, not merely by inspection.
- [x] Concurrent `create_usage_type` and `delete_usage_type` calls for the same `gts_id` do not race: the second acquires the lock only after the first releases it; a `create_usage_type` that races an in-flight `delete_usage_type` for the same `gts_id` is blocked at lock acquisition until the delete completes, and vice versa.
- [x] When the coordination-lock backend (cluster distributed lock) is unavailable, both record creation and usage-type deletion fail closed with `Transient` rather than proceeding without the lock.
- [x] `usage_records` rows older than the configured retention window are dropped by the native expiry mechanism; the catalog is not retention-bounded.
- [x] ClickHouse connections default to TLS in production; the connection string and credentials never appear in logs, errors, or debug output; no caller-supplied string reaches query text as a literal or identifier.
- [x] Aggregation over a 30-day single-tenant range completes within 500ms at p95, and the batch write path sustains ≥ 10,000 records/sec.
- [x] The plugin publishes its numerically-bounded consistency profile (single-node vs. replicated) explicitly, not as equivalent to the reference plugin's.
- [x] The plugin emits the `uc_clickhouse_*` OpenTelemetry metrics, including a backend-readiness signal, a backend-error classification counter, and the coordination-lock instrument set. (The orphaned-reference detection-backstop counter is **deferred**: its reconciliation worker was not built, so the instrument is not registered — see DESIGN.md §4 Observability and feature 0006 §5.)
- [x] Every backend failure surfaces as one of the SDK's declared `UsageCollectorPluginError` variants; no new top-level error type is introduced; a host-contract breach surfaces as `Internal`.
- [x] The plugin registers under a GTS instance identifier with its configured vendor and priority and does not self-select as the active backend.

## 10. Dependencies

| Dependency | Description | Criticality |
| --- | --- | --- |
| usage-collector-sdk | Storage SPI trait, domain models, error vocabulary, and GTS plugin spec — the contract the plugin implements | p1 |
| ClickHouse server/cluster | Durable system of record; provides columnar storage and the engine/expiry features this plugin's schema depends on | p1 |
| `sea-clickhouse` (crate path `clickhouse`) | Async Rust HTTP client (SeaQL soft fork of clickhouse-rs); typed Row inserts + DataRow reads | p1 |
| `sea-query` + `sea-query-clickhouse` | Typed ClickHouse SQL builders for runtime SELECT/DELETE | p1 |
| Cluster gear (`usage-collector` profile) | Backs the per-`gts_id` exclusive referential-integrity coordination lock | p1 |
| `cluster-sdk` | Resolves `DistributedLockV1` for the exclusive per-`gts_id` coordination lock | p1 |
| types-registry (+ ClientHub) | Publishes the plugin's GTS instance for host discovery and scoped binding | p1 |
| Platform registry / orchestration | Operator-driven active-backend selection | p1 |

## 11. Assumptions

- The Usage Collector core performs all authentication, PDP authorization, attribution and shape validation, and semantics decisions before every SPI call; the plugin trusts each call as authorized and structurally valid, while still performing its own referential-integrity check as a storage-layer (not business-logic) obligation ([§5](#5-functional-requirements)).
- The gateway derives each record's id and idempotency key; the plugin stores them verbatim and does not mint identity.
- The operator provisions a reachable ClickHouse server/cluster, TLS-capable for production, sized for the deployment's throughput and retention, and a reachable cluster gear `DistributedLockV1` backend under profile `usage-collector` to back the referential-integrity coordination lock, independent of whether ClickHouse itself is single-node or replicated ([§3.1](#31-gear-specific-environment-constraints)).
- Operators accept the narrower consistency and dedup-atomicity guarantees documented in [§5](#5-functional-requirements)/[§6](#6-non-functional-requirements) as the tradeoff for ClickHouse's columnar query performance — this is a conscious backend choice, not a silent regression from the reference plugin.

## 12. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Hash collision across different `gts_id`s (two distinct `gts_id`s deriving the same record `id`, or colliding on the hashed coordination-lock name) | Two writes that the exclusive lock does not order against each other; theoretical only, and no same-`gts_id` dedup race remains | Concurrent same-`gts_id` creates are serialized by the per-`gts_id` exclusive coordination lock ([§5](#5-functional-requirements)), so this is the only residual case; `FINAL`-qualified reads plus `ReplacingMergeTree` convergence remain as a backstop; documented in README and DESIGN.md §3.6 rather than claimed away |
| cluster distributed lock (coordination-lock backend) becomes unreachable or degraded | Both `create_usage_record`/`create_usage_records` and `delete_usage_type` fail closed with `Transient` for the affected `gts_id`s until Keeper recovers | Deliberate availability/consistency trade-off ([§5](#5-functional-requirements)); a new operational dependency versus the reference plugin, documented in the deployment README with Keeper-availability monitoring guidance |
| ClickHouse client/connection pool contention between ingestion and aggregation bursts | Aggregation-query latency NFR miss under heavy simultaneous ingestion | Operational mitigation guidance in the deployment README (pool sizing is not configurable — server-side quotas/settings profiles, or separate plugin instances); documented as a known, accepted contention point for v1 ([§6.1](#61-gear-specific-nfrs)) |
| Operator misconfigures the retention window shorter than the maximum client replay/backfill horizon | A dedup identity whose row was expired is accepted as a fresh insert, admitting a duplicate | Same mitigation as the reference plugin: operators size retention above the maximum replay/backfill horizon; tracked as a shared open question ([§13](#13-open-questions)) |

## 13. Open Questions

- **Reconcile this backend's retention-bounded dedup-key preservation with the parent gear's unbounded idempotency-key obligation** (`cpt-cf-usage-collector-fr-idempotency`) — the same open question the reference (accepted, production) TimescaleDB plugin's PRD §13 already carries unresolved for a time-partitioned backend. This plugin inherits, rather than introduces, this tension; resolution is a gear-level decision (narrow the gear contract to "retention-bounded" for time-series/columnar backends, or require every such plugin to preserve dedup keys beyond expiry) tracked at the gear level, not resolved in this PRD.
- **Schema evolution mechanism**: this plugin has no versioned-migration-file mechanism for evolving its schema after initial release (unlike `sqlx::migrate!`-based backends). A future column addition or type change requires a dedicated design (e.g. a `schema_migrations` tracking table plus a compatibility/rollout plan) before it can ship. Not resolved in this PRD or in Phase 1; tracked for a future phase or a follow-up ADR if/when a concrete schema change is needed.
- **Coordination-lock timeout sizing methodology**: the lock-acquisition timeout that triggers the fail-closed `Transient` path ([§5](#5-functional-requirements)) is a starting point pending real Keeper-latency data from load testing (Phase 7); the exact default and the guidance formula for operators sizing it against their own Keeper deployment's observed latency should be finalized once that data exists, not treated as final in this PRD.

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](./DECOMPOSITION.md)
- **Parent Gear PRD (authoritative)**: [../../../docs/PRD.md](../../../docs/PRD.md)
- **Parent Gear DESIGN**: [../../../docs/DESIGN.md](../../../docs/DESIGN.md)
- **Plugin SPI reference**: [../../../docs/plugin-spi.md](../../../docs/plugin-spi.md)
- **Domain model**: [../../../docs/domain-model.md](../../../docs/domain-model.md)
- **Reference plugin (structural template)**: [../../timescaledb-usage-collector-plugin/docs/PRD.md](../../timescaledb-usage-collector-plugin/docs/PRD.md)
- **ADRs (gear-level)**: notably [`0002-pluggable-storage`](../../../docs/ADR/0002-pluggable-storage.md), [`0012-unified-plugin-catalog-and-gts-id-reference`](../../../docs/ADR/0012-unified-plugin-catalog-and-gts-id-reference.md), [`0013-deterministic-usage-record-id`](../../../docs/ADR/0013-deterministic-usage-record-id.md), [`0014-created-at-in-dedup-identity`](../../../docs/ADR/0014-created-at-in-dedup-identity.md)
