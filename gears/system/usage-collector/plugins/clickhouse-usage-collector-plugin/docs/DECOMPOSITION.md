# Decomposition: ClickHouse Usage Collector Storage Plugin

**Overall implementation status:**

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-status-overall`

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Foundation: Bootstrap, Schema & SPI Wiring](#21-foundation-bootstrap-schema--spi-wiring)
  - [2.2 Record Persistence & Lifecycle](#22-record-persistence--lifecycle)
  - [2.3 Query & Aggregation](#23-query--aggregation)
  - [2.4 Usage-Type Catalog & Referential Integrity](#24-usage-type-catalog--referential-integrity)
  - [2.5 Data Retention](#25-data-retention)
  - [2.6 Backend Observability & Metrics](#26-backend-observability--metrics)
  - [2.7 Deliberate Omissions](#27-deliberate-omissions)
- [3. Feature Dependencies](#3-feature-dependencies)
- [4. Documentation Inventory](#4-documentation-inventory)

<!-- /toc -->

## 1. Overview

This decomposition mirrors the reference `timescaledb-usage-collector-plugin`'s six-capability shape (Foundation, Record Persistence & Lifecycle, Query & Aggregation, Usage-Type Catalog & Referential Integrity, Data Retention, Backend Observability & Metrics), so the two backends stay easy to compare feature-for-feature. Like the reference decomposition, this is a **brownfield** record: the plugin is implemented and merged, so each entry below describes the capability as it exists in the crate today and serves as the traceability map from PRD/DESIGN elements to shipped code, not as a forward execution plan. Where a documented element is deliberately not implemented, the entry says so explicitly (see [§2.6](#26-backend-observability--metrics) for the orphan-reconciliation worker and [§2.7](#27-deliberate-omissions)).

The load-bearing difference from the reference plugin's decomposition is that **Record Persistence & Lifecycle** depends on the `ReplacingMergeTree(version)` versioned-marker mechanism established by Foundation's schema, and **Usage-Type Catalog & Referential Integrity** depends on both that mechanism (for create) and a real lightweight-`DELETE` row removal (for delete) — both documented in DESIGN.md §3.6 — rather than relying on transactional or FK primitives the reference plugin's equivalent features use. This coupling is called out explicitly in each feature's scope below so no downstream phase re-derives a transactional design ClickHouse cannot support.

**Decomposition Strategy**: identical to the reference plugin's — cohesion by capability, loose coupling via explicit `Depends On`, 100% PRD/DESIGN element coverage, mutual exclusivity at the capability layer, and write/read plane separation. IDs use the `cpt-cf-uc-ch-plugin-*` namespace (distinct from the reference plugin's `cpt-cf-uc-plugin-*`) so both plugins' traceability graphs coexist without collision in the monorepo-wide `cpt` index.

## 2. Entries

### 2.1 Foundation: Bootstrap, Schema & SPI Wiring

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-feature-foundation`

- **Purpose**: Establish the plugin's runtime substrate and its single public surface. At `#[toolkit::gear]` `init`, the Plugin Module loads and validates the typed configuration, builds the `clickhouse` crate client, runs the embedded-SQL schema provisioning (idempotent `CREATE TABLE IF NOT EXISTS` DDL with a fixed 1-year TTL default, no external migration-tracking table) and reconciles `usage_records` TTL to `retention_period_secs` via `ensure_retention_ttl` — the retention semantics are owned by [§2.5](#25-data-retention)), and performs the GTS handshake identical in shape to the reference plugin's. The SPI Storage Adapter is the host's only entry point, delegating to the stores and owning ClickHouse-error-to-`UsageCollectorPluginError` classification.

- **Depends On**: None

- **Scope**:
  - Overall backend design node and tech stack (`toolkit::gear` + `types-registry-sdk` wiring, `usage-collector-sdk` domain types, `clickhouse`/`opentelemetry` infrastructure). No coordination backend: the plugin declares no `cluster` dependency.
  - Plugin Module lifecycle: config load, ClickHouse client construction (via `build_client` — parses `database_url` into a bare base URL plus separate user/password/database via `ParsedEndpoint`), schema migration invocation plus `ensure_retention_ttl` (DDL bakes a 1-year default; startup `ALTER TABLE … MODIFY TTL` when config differs) and `ensure_insert_dedup_window` (retrofits `non_replicated_deduplication_window` onto pre-existing tables), and GTS + ClientHub registration. Foundation owns the call sites; [§2.5](#25-data-retention) owns the retention semantics.
  - **Retired: Coordination Lock Manager, `CatalogLockPort`, `LockGuardPort`.** `infra/coordination/` no longer exists. Both stores are constructed from a ClickHouse client, the metric inventory and a deadline only; nothing in the plugin coordinates across processes.
  - SPI Storage Adapter: pure delegation, no business logic, owns backend-error classification (realizing `cpt-cf-uc-ch-plugin-fr-error-classification`) and keyset cursor encoding.
  - Schema Migration: the embedded `migrations/0001_init.sql` DDL runner (idempotent, re-runnable as a no-op; fixed 1-year TTL default) plus `ensure_retention_ttl`; `--` comment lines stripped and statements split while respecting single-quoted string literals before execution; no versioned-migration framework for non-TTL schema evolution (PRD.md §13 Open Questions).
  - TLS-defaulted, secret-wrapped DSN (`SecretFromEnv` with redacted `Debug`, no `Display`/`Serialize`).
  - Published (narrower, numerically-bounded) consistency profile per DESIGN.md §3.8.

- **Out of scope**:
  - Record insert/dedup/deactivate — [§2.2](#22-record-persistence--lifecycle).
  - Aggregation/list execution and query translation — [§2.3](#23-query--aggregation).
  - Usage-type CRUD and the delete-emulation protocol — [§2.4](#24-usage-type-catalog--referential-integrity).
  - `TTL` clause ownership, the `retention_period_secs` config field, and retention/key-reuse semantics — [§2.5](#25-data-retention) in full; Foundation owns the fixed 1-year DDL default and the `ensure_retention_ttl` call site that reconciles the live clause to config on every `init`.
  - The `uc_clickhouse_*` metric inventory — [§2.6](#26-backend-observability--metrics).
  - ClickHouse cluster topology, sizing, HA — operator deployment guide.

- **Requirements Covered**:
  - [x] `p1` - `cpt-cf-uc-ch-plugin-fr-schema-provisioning`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-nfr-spi-stability`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-nfr-transport-security`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-nfr-consistency-profile`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-fr-error-classification`

- **Design Principles Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-principle-pure-persistence`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-principle-spi-conformance`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-principle-honest-degradation`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-principle-one-mechanism-two-problems`

- **Design Constraints Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-no-transactions`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-vendor-isolation`

- **Domain Model Entities**:
  - `UsageCollectorPluginV1` (SPI trait), `UsageCollectorPluginError`, typed plugin configuration, ClickHouse client handle (plugin-local).

- **Design Components**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-module`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-adapter`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-migrations`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-lock-manager` — **retired**; no such component exists. The ID is retained so cross-artifact references stay resolvable (DESIGN.md §1.3).
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-catalog-lock-port` — **retired** with the lock manager; both stores are constructed from a client, metrics and a deadline only, so no lock seam trait is needed.

- **API**:
  - [x] `p1` - `cpt-cf-uc-ch-plugin-interface-storage-spi`
  - In-process async `UsageCollectorPluginV1` trait object, identical surface to the reference plugin. No REST or network-exposed surface.

- **Sequences**: None (bootstrap and registration expose no runtime SPI sequence).

- **Data**:
  - [x] `p3` - `cpt-cf-uc-ch-plugin-db-schema`

- **Contracts**:
  - [x] `p1` - `cpt-cf-uc-ch-plugin-contract-clickhouse`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-contract-coordination-lock` — **retired**; the plugin consumes no coordination backend and declares no `cluster` gear dependency.
  - [x] `p2` - `cpt-cf-uc-ch-plugin-contract-gts-registration`

### 2.2 Record Persistence & Lifecycle

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-feature-record-persistence`

- **Purpose**: Provide the backend write plane over `usage_records`, using the `ReplacingMergeTree(version)`-keyed-by-`id` mechanism as the dedup convergence backstop and the deactivation vehicle (DESIGN.md §3.6). Single/batch insert resolve via a read-before-insert check against the deterministic `id`; on a found row, canonical-field comparison yields silent absorb or `IdempotencyConflict`, best-effort, with racing identical writes dropped by the engine through a per-`INSERT` `insert_deduplication_token` and the residual concurrent-race deviation documented in DESIGN.md §3.6 and PRD.md §5/§11. Deactivation composes one multi-row `INSERT` of versioned marker rows for the target and its depth-1 active compensations, atomic as a single part write.

- **Depends On**: `cpt-cf-uc-ch-plugin-feature-foundation`

- **Scope**:
  - Plugin-owned pre-insert referential-integrity check against the catalog (DESIGN.md §3.6 Ingest sequence steps 2-3), rejecting a reference to an absent (including previously deleted) usage type with `UsageTypeNotFound`. Because both create and delete paths acquire the **same exclusive mutex name** per `gts_id` (DESIGN.md §3.5), concurrent creates for the same `gts_id` also serialize — this is this feature's half of `cpt-cf-uc-ch-plugin-fr-referential-integrity`; the catalog-side lock-protected verify-then-delete protocol is owned by [§2.4](#24-usage-type-catalog--referential-integrity). The lock usage (acquire/release call sites) is owned here; the Coordination Lock Manager's cluster lock manager is constructed by [§2.1](#21-foundation-bootstrap-schema--spi-wiring).
  - Single insert with read-before-insert dedup check and `ReplacingMergeTree` convergence backstop; `metadata` persisted verbatim into `Map(String, String)`; `status = 'active'` on first accept.
  - Batch insert as exactly three statements regardless of how many usage types the batch spans — one catalog existence query over the distinct `gts_id`s, one dedup pre-read, one multi-row `INSERT` — with per-record results in input order.
  - Compensation persistence: signed `value` + optional `corrects_id` on the ordinary insert path; no netting computed.
  - Depth-1 versioned-marker deactivation cascade: `UsageRecordNotFound` / `UsageRecordAlreadyInactive` / flip-via-single-INSERT, per DESIGN.md §3.6.
  - `get_usage_record` by `id`, version-resolved.
  - Batch-write-path throughput allocation (one multi-row `INSERT` for the whole batch, no per-row round-trip).

- **Out of scope**:
  - Reading records back for aggregation/keyset list — [§2.3](#23-query--aggregation).
  - Schema DDL (`usage_records` table, `TTL` clause) — created by [§2.1](#21-foundation-bootstrap-schema--spi-wiring); this feature is the row-writer.
  - `TTL` expiry of stored rows — [§2.5](#25-data-retention).

- **Requirements Covered**:
  - [x] `p1` - `cpt-cf-uc-ch-plugin-fr-idempotent-dedup`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-fr-deactivation`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-nfr-ingestion-throughput`
  - [x] `p1` - `cpt-cf-uc-ch-plugin-fr-referential-integrity` (the insert-time catalog existence check only; there is no delete-side half, because [§2.4](#24-usage-type-catalog--referential-integrity) does not implement `delete_usage_type` and usage types are never removed)

- **Design Principles Covered**: None (realizes principles owned by [§2.1](#21-foundation-bootstrap-schema--spi-wiring))

- **Design Constraints Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-dedup-race-window`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-no-in-place-update`

- **Domain Model Entities**: `UsageRecord`

- **Design Components**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-record-store`

- **API**:
  - `create_usage_record`, `create_usage_records`, `get_usage_record`, `deactivate_usage_record`.

- **Sequences**:
  - `p1` - `cpt-cf-uc-ch-plugin-seq-ingest-dedup`
  - `p1` - `cpt-cf-uc-ch-plugin-seq-ingest-batch`
  - `p1` - `cpt-cf-uc-ch-plugin-seq-deactivate-cascade`

- **Data**:
  - `p1` - `cpt-cf-uc-ch-plugin-dbtable-usage-records`

### 2.3 Query & Aggregation

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-feature-query-aggregation`

- **Purpose**: Provide the backend read plane, pushing aggregation into ClickHouse's vectorized execution and paginating raw reads via keyset seeking — both single-level scans that anti-join the deactivation-marker id set instead of resolving `ReplacingMergeTree` versions (DESIGN.md §3.8). This is the allocation target for the aggregation query-latency NFR and the workload-isolation NFR.

- **Depends On**: `cpt-cf-uc-ch-plugin-feature-foundation`, `cpt-cf-uc-ch-plugin-feature-record-persistence`

- **Scope**:
  - Pushed-down aggregation (SUM/COUNT/MIN/MAX/AVG with grouping) as a single-level scan over the marker-anti-joined active-row set, honoring the compensation-partition rule (SUM nets compensations; other ops exclude them), capped server-side to `MAX_AGGREGATION_BUCKETS + 1` (100,001) grouped rows via `LIMIT` (DESIGN.md §3.6).
  - Marker-anti-joined keyset-paginated raw list honoring the supplied order and cursor, one-row look-ahead, next-cursor encoding.
  - Injection-safe translation: bound parameters for values, allowlisted identifiers, adapted to the `clickhouse` crate's parameter API.
  - Aggregation query-latency NFR allocation through ClickHouse's columnar execution, explicitly measured **with** the marker anti-join included in the budget, not around it (DESIGN.md §3.8).
  - Workload-isolation NFR allocation: the documented, accepted shared-client contention point between ingestion and aggregation (DESIGN.md §3.5). There is deliberately **no** pool-size config field — the `clickhouse` crate exposes no pool bound one could drive (DESIGN.md §3.5) — so this feature owns the burst-query-vs-ingestion contention analysis and its README-documented **operational** mitigation guidance (server-side quotas, separate instances), not merely a DESIGN-only aside.

- **Out of scope**:
  - Writing, dedup, or deactivation — [§2.2](#22-record-persistence--lifecycle).
  - Catalog listing keyset pagination — reuses this pattern but owned by [§2.4](#24-usage-type-catalog--referential-integrity).
  - Client construction (there is no pool config field to define) — [§2.1](#21-foundation-bootstrap-schema--spi-wiring); this feature owns the isolation *analysis and behavior*, not the client/pool object's construction.

- **Requirements Covered**:
  - [x] `p1` - `cpt-cf-uc-ch-plugin-nfr-query-latency`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-nfr-workload-isolation`

- **Design Principles Covered**: None (realizes principles owned by [§2.1](#21-foundation-bootstrap-schema--spi-wiring))

- **Design Constraints Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-final-qualified-reads`
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-aggregation-bucket-cap`

- **Domain Model Entities**: `UsageRecord` (read), `AggregationSpec`, `AggregationResult`, `ODataQuery`, `CursorV1`, `Page`

- **Design Components**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-query-translator`

  (Query execution lives in the Record Store component owned by [§2.2](#22-record-persistence--lifecycle); the Query Translator owned by this feature provides the OData-to-ClickHouse SQL translation layer — `infra/storage/query/*`.)

- **API**:
  - `query_aggregated_usage_records`, `list_usage_records`.

- **Sequences**:
  - `p1` - `cpt-cf-uc-ch-plugin-seq-query-aggregated`
  - `p2` - `cpt-cf-uc-ch-plugin-seq-list-keyset`

- **Data**:
  - `cpt-cf-uc-ch-plugin-dbtable-usage-records` (reader; written by [§2.2](#22-record-persistence--lifecycle) — shared usage, not re-owned).

### 2.4 Usage-Type Catalog & Referential Integrity

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-feature-usage-type-catalog`

- **Purpose**: Own the sole store for the usage-type catalog. Referential integrity between records and types is application-emulated (ClickHouse has no native FK) and is carried entirely by the insert-time catalog existence check owned by [§2.2](#22-record-persistence--lifecycle): there is no delete-side half, because this backend does not implement `delete_usage_type` and usage types are therefore never removed. `create_usage_type` is a version-resolved pre-existence read followed by an `INSERT`, with no critical section around the pair.

- **Depends On**: `cpt-cf-uc-ch-plugin-feature-foundation`

- **Scope**:
  - Catalog create (version-resolved pre-existence read then `INSERT`, not one critical section: two concurrent same-`gts_id` creates can both pass the read, and `ReplacingMergeTree(version)` then converges them last-writer-wins; once the winner is visible a later create yields a silent absorb for an identical payload or `UsageTypeAlreadyExists` otherwise), storing `kind` and `metadata_fields` verbatim.
  - Catalog point read (version-resolved; absent → `UsageTypeNotFound`).
  - Catalog keyset-paginated list ordered by `gts_id`, version-resolved.
  - Catalog delete is **not implemented**: it returns `UsageCollectorPluginError::Internal` and issues no SQL. With no foreign keys and no mutual-exclusion primitive, a reference-count probe is only ever a snapshot and a `DELETE` could orphan records inserted between probe and removal (DESIGN.md §3.6).

- **Out of scope**:
  - Metadata-key validation, counter/gauge derivation — inherited pure-persistence posture owned by [§2.1](#21-foundation-bootstrap-schema--spi-wiring) and enforced upstream by the gear core.
  - `usage_records`' own schema — created by Foundation.
  - The insert-time catalog existence check — [§2.2](#22-record-persistence--lifecycle). This feature guarantees the invariant that check relies on (a type, once created, is never removed) but does not perform it.

- **Requirements Covered**:
  - [x] `p1` - `cpt-cf-uc-ch-plugin-fr-referential-integrity` (the append-only-catalog half: types are never removed, which is what makes [§2.2](#22-record-persistence--lifecycle)'s insert-time check sufficient)

- **Design Principles Covered**: None (realizes principles owned by [§2.1](#21-foundation-bootstrap-schema--spi-wiring))

- **Design Constraints Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-gts-lock-required` — **superseded**; the ID is retained so cross-artifact references stay resolvable (DESIGN.md §2.2).

- **Domain Model Entities**: `UsageType`

- **Design Components**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-component-catalog-store`

- **API**:
  - `create_usage_type`, `get_usage_type`, `list_usage_types`, `delete_usage_type`.

- **Sequences**:
  - `p1` - `cpt-cf-uc-ch-plugin-seq-create-type`
  - `p1` - `cpt-cf-uc-ch-plugin-seq-delete-type-fk`

- **Data**:
  - `p1` - `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

### 2.5 Data Retention

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-feature-retention`

- **Purpose**: Own `usage_records` storage-growth bounding via ClickHouse's native `TTL` clause — both the `retention_period_secs` config field and the mechanism by which it takes effect at runtime. Foundation's Schema Migration creates `usage_records` with a fixed 1-year TTL default; on every `init`, `ensure_retention_ttl` compares the live TTL to `retention_period_secs` and issues `ALTER TABLE … MODIFY TTL` when they differ. The `usage_type_catalog` is reference data and is never retention-bounded.

- **Depends On**: `cpt-cf-uc-ch-plugin-feature-foundation`

- **Scope**:
  - The `retention_period_secs` config field and its validation (range `(0, MAX_RETENTION_SECS]` where `MAX_RETENTION_SECS` = 100 years — guards against `DateTime64` overflow in the ClickHouse TTL expression).
  - `TTL created_at + INTERVAL <n> SECOND DELETE` on `usage_records` (`<n>` = the configured `retention_period_secs`) — this feature owns the clause's semantics, the config field, and the documentation of the TTL-coupling behavior (a TTL-dropped row's dedup identity becomes reusable after expiry).
  - Documentation of the retention-vs-dedup-preservation coupling, mirroring the reference plugin's already-accepted risk class, and of the open gear-level reconciliation this narrowing shares with the reference plugin (PRD.md §13).

- **Out of scope**:
  - Creation of the `usage_records` table — [§2.1](#21-foundation-bootstrap-schema--spi-wiring); Foundation's `apply_migrations` / `ensure_retention_ttl` own the DDL and reconcile call sites; this feature owns the config field and retention semantics.
  - The dedup-write behavior itself — [§2.2](#22-record-persistence--lifecycle) (coupled here via key-reuse-after-expiry).
  - A general versioned-migration framework for non-TTL schema evolution — not in v1 scope (PRD.md §13); TTL itself is reconciled on every `init` via `ensure_retention_ttl`.

- **Requirements Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-fr-retention`

- **Design Principles Covered**: None (realizes principles owned by [§2.1](#21-foundation-bootstrap-schema--spi-wiring))

- **Design Constraints Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-constraint-retention`

- **Domain Model Entities**: `UsageRecord` (TTL-expired subject; not re-owned)

- **Design Components**: None (Foundation's Schema Migration bakes a fixed 1-year TTL default at first provisioning and reconciles it to `retention_period_secs` on every `init` via `ensure_retention_ttl`; this feature owns the retention/key-reuse constraint, referencing those call sites rather than re-owning them).

- **API**: None (declarative backend policy; no SPI method).

- **Sequences**: None.

- **Data**:
  - `cpt-cf-uc-ch-plugin-dbtable-usage-records` (retention target; written by [§2.2](#22-record-persistence--lifecycle) — shared usage, not re-owned).

### 2.6 Backend Observability & Metrics

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-feature-observability`

- **Purpose**: Emit the backend-internal telemetry the gear cannot see, under the plugin's own `uc_clickhouse_*` OpenTelemetry sub-namespace (distinct from the host's and from the reference plugin's `uc_timescaledb_*`), covering performance, efficiency, reliability, and the dedup/deactivation-emulation-specific outcome counters this backend's mechanism requires that the reference plugin's does not (DESIGN.md §4).

- **Depends On**: `cpt-cf-uc-ch-plugin-feature-foundation`

- **Scope**:
  - The `uc_clickhouse_*` metric inventory (insert/query/deactivate/pool-acquire duration, backend-error classification, readiness gauge, catalog-size gauge, dedup-outcome counters) with bounded label cardinality.
  - **No coordination-lock instrument set**: there is no lock to instrument, so no acquire-duration histogram, contention counter or lock-manager-unavailable counter is registered.
  - The `uc_clickhouse_orphaned_reference_detected_total` defense-in-depth counter — specified as a safety net for an out-of-band `usage_type_catalog` deletion or a future code regression, not as a detector for an accepted race (the catalog is append-only, so the plugin itself cannot create an orphan). *
  - Recording each SPI dispatch's ClickHouse work under the host's ambient tracing span.

- **Out of scope**:
  - The request-path `usage_collector.*` signals and host-computed readiness gauge — owned by the gear core.
  - The operations being measured — owned by their respective features above; this feature instruments them cross-cuttingly.

- **Requirements Covered**:
  - [x] `p2` - `cpt-cf-uc-ch-plugin-nfr-operational-visibility`

- **Design Principles Covered**: None (realizes principles owned by [§2.1](#21-foundation-bootstrap-schema--spi-wiring))

- **Domain Model Entities**: None (OpenTelemetry instruments; no persisted entity).

- **Design Components**:
  - [x] `p3` - `cpt-cf-uc-ch-plugin-design-metric-inventory`

- **API**: None (push-based OTLP export; no SPI method).

- **Sequences**: None.

- **Data**: None.

### 2.7 Deliberate Omissions

- **Multi-shard distributed-table topology, ClickHouse's own replication-serving cluster/cluster lock coordination** — governed by the operator's ClickHouse deployment guide, not by plugin features (PRD.md §4.2). This is distinct from, and does not include, this plugin's own use of the cluster gear's `DistributedLockV1` as a coordination-lock facade for referential integrity, which **is** in scope and owned jointly by [§2.1](#21-foundation-bootstrap-schema--spi-wiring) (client construction and `CatalogLockPort`/`LockGuardPort` traits), [§2.2](#22-record-persistence--lifecycle) (exclusive lock usage on the create path), and [§2.4](#24-usage-type-catalog--referential-integrity) (exclusive lock usage on the delete path).
- **Product-level gear concerns** (authentication, PDP authorization, attribution/shape validation, idempotency-key presence, counter/gauge semantics, data classification) — owned by the parent Usage Collector gear, surfaced only as the pure-persistence boundary in [§2.1](#21-foundation-bootstrap-schema--spi-wiring).
- **DB-enforced serializable dedup** — structurally unavailable on ClickHouse; not a deferred feature, a permanent architectural constraint documented in DESIGN.md §2.2/§3.6/§3.8 rather than assigned to a feature to "complete" later. (Referential integrity is, by contrast, closed exactly via the `gts_id` coordination lock — see [§2.2](#22-record-persistence--lifecycle)/[§2.4](#24-usage-type-catalog--referential-integrity) — not merely bounded or accepted as unavailable.)
- **Read/write pool split for workload isolation** — noted as a possible future, additive revision in DESIGN.md §3.5 if production experience shows contention; not committed to v1 scope.
- **General schema-evolution / versioned-migration mechanism** — not designed in v1 (DESIGN.md §4 Deferred, PRD.md §13 Open Questions); Foundation ([§2.1](#21-foundation-bootstrap-schema--spi-wiring)) provisions only the initial schema shape.

## 3. Feature Dependencies

**Legend**:
- `↓` = build-order dependency (upstream must exist first)
- `├─→` = direct build-order dependency
- `└─→` = related dependency (also `← foundation` — reverse dependency on foundation)
- `⇢` = data-coupling (runtime data flow; not a build-order dependency)

```text
cpt-cf-uc-ch-plugin-feature-foundation
    ↓                                                                   [build-order: every feature depends on foundation]
    ├─→ cpt-cf-uc-ch-plugin-feature-record-persistence                  [build-order]
    │       └─→ cpt-cf-uc-ch-plugin-feature-query-aggregation           [build-order: records → query]      (also ← foundation)
    ├─→ cpt-cf-uc-ch-plugin-feature-usage-type-catalog                  [build-order]
    ├─→ cpt-cf-uc-ch-plugin-feature-retention                           [build-order; data-coupling ⇢ record-persistence: dedup-key reuse-after-expiry]
    └─→ cpt-cf-uc-ch-plugin-feature-observability                       [build-order; cross-cutting runtime]   (instruments record-persistence, query-aggregation, usage-type-catalog, retention)
```

**Dependency Rationale**: identical structural rationale to the reference plugin's ([`timescaledb-usage-collector-plugin/docs/DECOMPOSITION.md` §3](../../timescaledb-usage-collector-plugin/docs/DECOMPOSITION.md#3-feature-dependencies)) — Record Persistence requires Foundation's ClickHouse client and Coordination Lock Manager; Query & Aggregation requires both Foundation and Record Persistence (nothing to read until records exist); Usage-Type Catalog requires only Foundation (its FK-emulation protocol is self-contained at build-order level, coordinating with Record Persistence only at runtime through the shared `gts_id` coordination lock and the shared catalog-existence read both features perform, not through a build-order dependency); Retention and Observability each require only Foundation to exist, instrumenting/expiring the other features' data cross-cuttingly.

## 4. Documentation Inventory

| # | Document | Type | Purpose |
| --- | --- | --- | --- |
| 1 | `docs/DESIGN.md` | Technical Design | Architecture overview, component model, sequencing, schema, and consistency profile for the ClickHouse plugin. |
| 2 | `docs/PRD.md` | Product Requirements Document | Plugin-specific requirements, deviations from the reference plugin, NFRs, acceptance criteria, and open questions. |
| 3 | `docs/DECOMPOSITION.md` | Decomposition | Feature breakdown, scope boundaries, design element traceability, and this inventory. This file. |
| 4 | `docs/features/0001-cpt-cf-uc-ch-plugin-feature-foundation.md` | Feature Spec | Bootstrap, schema provisioning, SPI wiring, and security posture. |
| 5 | `docs/features/0002-cpt-cf-uc-ch-plugin-feature-record-persistence.md` | Feature Spec | Record write path, dedup, deactivation cascade, and batch ingest. |
| 6 | `docs/features/0003-cpt-cf-uc-ch-plugin-feature-query-aggregation.md` | Feature Spec | Pushed-down aggregation, keyset-paginated list, query translation, and workload-isolation analysis. |
| 7 | `docs/features/0004-cpt-cf-uc-ch-plugin-feature-usage-type-catalog.md` | Feature Spec | Usage-type create / get / list, and why delete is withheld. |
| 8 | `docs/features/0005-cpt-cf-uc-ch-plugin-feature-retention.md` | Feature Spec | `retention_period_secs` config field, TTL semantics, and dedup-key-reuse-after-expiry coupling. |
| 9 | `docs/features/0006-cpt-cf-uc-ch-plugin-feature-observability.md` | Feature Spec | `uc_clickhouse_*` metric inventory, lock instrument set, and deferred orphan-detection counter. |
| 10 | `README.md` | README | Operator-facing deployment guide, configuration reference, workload-isolation mitigation guidance, and consistency-profile caveats.
