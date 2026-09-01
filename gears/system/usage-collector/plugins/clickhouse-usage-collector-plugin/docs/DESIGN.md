# Technical Design — ClickHouse Usage Collector Storage Plugin

<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers / NFR Allocation](#12-architecture-drivers--nfr-allocation)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions & Sequences](#36-interactions--sequences)
  - [3.7 Database Schema & Tables](#37-database-schema--tables)
  - [3.8 Consistency & Concurrency](#38-consistency--concurrency)
- [4. Additional Context](#4-additional-context)
  - [Non-Applicable Design Domains](#non-applicable-design-domains)
  - [Observability](#observability)
  - [Security](#security)
  - [Deferred (post-v1)](#deferred-post-v1)
  - [Testing Architecture](#testing-architecture)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This plugin is a second, independent realization of the Usage Collector's storage SPI (`UsageCollectorPluginV1`), targeting ClickHouse — a columnar OLAP database — instead of the reference plugin's row-oriented PostgreSQL/TimescaleDB. It is chosen for deployments that weight aggregation-query throughput and storage efficiency over ClickHouse's absence of transactions, row locks, and native foreign keys. The architecture's central engineering problem, and the subject of most of this document, is: **how does a plugin honor the SPI's dedup, deactivation, and referential-integrity contracts on a backend with no ACID transactions and no in-place row update?** The answer, decided in the Phase 1 design review gate and substantially redesigned in a second follow-up review after the first fencing-delay-based approach was found insufficient, is a single unifying mechanism for dedup/deactivation — `ReplacingMergeTree(version)` versioned rows, resolved at read time via `FINAL`/`argMax` — plus, for referential integrity, a **per-`gts_id` exclusive coordination lock** backed by the cluster gear's `DistributedLockV1` ([§3.5](#35-external-dependencies), [§3.6](#36-interactions--sequences)) that closes the concurrent-reference window described in [`plugin-spi.md` Method 9](../../../docs/plugin-spi.md#method-9--delete-usage-type) without qualification, rather than merely bounding it. A plugin-owned pre-insert catalog check, performed while holding the lock, mirrors the structural (not business-logic) role Postgres's native FK plays in the reference plugin.

### 1.2 Architecture Drivers / NFR Allocation

| Driver | Allocation |
| --- | --- |
| `cpt-cf-usage-collector-nfr-query-latency` (≤500ms p95, 30-day single-tenant aggregation) | ClickHouse's columnar storage and vectorized `GROUP BY`/aggregate execution ([§3.6](#36-interactions--sequences) aggregate sequence); an `ORDER BY (tenant_id, gts_id, created_at, id)` primary key on `usage_records` for time-range scan locality; the SPI's `MAX_AGGREGATION_BUCKETS` cap enforced server-side via `LIMIT`. |
| `cpt-cf-usage-collector-nfr-throughput` (≥10,000 records/sec) | Batch writes as one multi-row `INSERT` per distinct `gts_id` partition — exactly one `INSERT` for the common single-`gts_id` batch (ClickHouse's write path strongly favors large parts over many small ones); no per-row round-trip. **Lock-bound per-`gts_id` ceiling**: every create acquires the *same* per-`gts_id` exclusive coordination lock ([§3.5](#35-external-dependencies)), so creates for one `gts_id` execute one critical section at a time (lock acquire → catalog check → dedup pre-check → `INSERT`). Distinct `gts_id`s do not contend, and within one multi-`gts_id` batch each partition runs its *whole* critical section — read, renew, `INSERT` — concurrently under its own lock ([§3.6](#36-interactions--sequences) Batch Ingest step 2), so a contended `gts_id` delays only its own partition and this envelope is met by batching and/or by spreading load across `gts_id`s; a workload concentrated on a single `gts_id` is capped by that serialized critical-section rate, not by ClickHouse's write throughput, and single-record `create_usage_record` calls against one hot `gts_id` are the worst case. **Shared-pool exposure**: the same client/pool serves the query path, so query bursts can consume capacity this NFR depends on (see the workload-isolation row). |
| `cpt-cf-usage-collector-nfr-workload-isolation` | Documented as a **known limitation** rather than a solved allocation: the `sea-clickhouse` client (soft fork of `clickhouse-rs`) exposes one client over one internal connection pool, and that pool's sizing is **not** operator-tunable — the crate offers no pool-bound builder at all ([§3.5](#35-external-dependencies)), so there is no config field to expose. This plugin configures one such client for the request path. Consequence for the throughput NFR above: a burst of aggregation/list queries competes with ingest for the same pool and the same ClickHouse server, and nothing in the plugin reserves capacity for or prioritizes ingest — accepted for v1, mitigated **operationally** (server-side quotas/settings profiles, separate plugin instances) rather than by configuration, with monitoring and mitigation guidance in the plugin README. A future revision MAY split ingestion and query onto separate clients if contention is observed in practice (see [§3.5](#35-external-dependencies)). |
| `cpt-cf-usage-collector-adr-0012` (FK-equivalent referential integrity) | A per-`gts_id` exclusive lock backed by the cluster distributed lock ([§3.5](#35-external-dependencies)) makes the plugin-owned pre-insert catalog check (create side) and the verify-then-delete protocol (delete side, [§3.6](#36-interactions--sequences)) mutually exclusive per `gts_id`, closing the window `ON DELETE RESTRICT` would otherwise close natively. |
| `cpt-cf-usage-collector-fr-idempotency` | `ReplacingMergeTree(version)` + read-before-insert check ([§3.6](#36-interactions--sequences)), with a documented residual race narrower than but analogous in class to the reference plugin's already-accepted TOCTOU window. |

### 1.3 Architecture Layers

Follows the same three-layer shape as the reference plugin (`docs/toolkit_unified_system/02_gear_layout_and_sdk_pattern.md`):

- **Wiring layer** (`gear.rs`, `config.rs`): `#[toolkit::gear]` `init()`, typed config, the four-step GTS/`types-registry`/`ClientHub` registration handshake.
- **Domain layer** (`domain/ports.rs`, `domain/adapter.rs`): `RecordStore` / `CatalogStore` ports and the `StorageAdapter` implementing `UsageCollectorPluginV1` by pure delegation — dialect-agnostic, effectively unchanged from the reference plugin.
- **Infrastructure layer** (`infra/storage/*`): all ClickHouse-specific code — the client/pool, schema migration, entity/mapper types, the OData-to-ClickHouse-SQL query translator, and the `RecordStore`/`CatalogStore` implementations.

## 2. Principles & Constraints

### 2.1 Design Principles

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-principle-pure-persistence`

**Pure persistence, no business logic** (inherited from the reference plugin): the plugin performs no authentication, PDP authorization, attribution/shape validation, idempotency-key presence, or counter/gauge decisions. The one exception, called out explicitly rather than left implicit, is [§2.1](#21-design-principles)'s companion principle below: the plugin's own pre-insert catalog-existence check is a **structural storage-integrity mechanism**, not a re-executed business check (see the "Structural-integrity checks are not business-logic re-execution" note under `cpt-cf-uc-ch-plugin-principle-spi-conformance`).

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-principle-spi-conformance`

**SPI conformance is structural**: `StorageAdapter` implementing `UsageCollectorPluginV1` is a compile-time guarantee; a drift between the SPI and this backend is a build error, identical to the reference plugin's guarantee. **Structural-integrity checks are not business-logic re-execution**: `plugin-spi.md`'s "the plugin MUST NOT re-execute [gateway] checks" (Method 1) refers to authorization/business validation (PDP, attribution, idempotency-key presence, counter/gauge semantics, metadata shape, `corrects_id` preconditions) the gateway performs before dispatch. It does not prohibit a backend from implementing referential-integrity as a storage-level invariant on its own write path — that is exactly what Postgres's native `ON DELETE RESTRICT` FK does for the reference plugin, transparently and without any Rust code. Because ClickHouse has no such native primitive, this plugin's `create_usage_record`/`create_usage_records` perform their own catalog-existence check immediately before the record `INSERT` ([§3.6](#36-interactions--sequences)) — functionally the same role as the reference plugin's FK, not a re-validation of the gateway's authorization decision.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-principle-honest-degradation`

**Honest degradation over silent narrowing**: every place this backend cannot match the reference plugin's DB-enforced guarantee is either closed exactly (referential integrity, via the cluster exclusive lock, [§3.6](#36-interactions--sequences) — no residual window is accepted here) or implemented as the closest achievable approximation with the residual explicitly documented as a deviation (dedup atomicity, read-after-write for all readers) — never silently presented as equivalent to what a non-transactional backend cannot actually provide.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-principle-one-mechanism-two-problems`

**One mechanism, two problems**: dedup convergence and deactivation-without-`UPDATE` both reduce to "insert a new versioned row sharing the `ReplacingMergeTree` sorting key, resolve the latest version at read time" — this is a deliberate simplification so the plugin has one correctness mechanism to reason about, test, and instrument, not two.

### 2.2 Constraints

- **No multi-statement ACID transactions.** Every sequence in [§3.6](#36-interactions--sequences) that would be a single Postgres transaction in the reference plugin is instead composed of independently-committing ClickHouse statements; atomicity is claimed only where a single ClickHouse `INSERT` (which is atomic as one part write) suffices, or where an external mutual-exclusion primitive (the `gts_id` coordination lock, [§3.5](#35-external-dependencies)) makes a multi-statement sequence effectively atomic *with respect to the one other operation it must be ordered against*, and is explicitly disclaimed everywhere else (dedup).
- **No row-level locks in ClickHouse itself.** `SELECT ... FOR UPDATE` has no ClickHouse equivalent. For the dedup path this leaves a plain, unlocked read-then-write. For the referential-integrity path, the absence of a ClickHouse-native lock is compensated by an external one: a per-`gts_id` exclusive lock held via the cluster `DistributedLockV1` facade ([§3.5](#35-external-dependencies)) serializes every create against every delete for the same `gts_id` (and serializes concurrent creates with each other — both paths acquire the same exclusive mutex name), which is what actually closes that window rather than merely narrowing it.
- **No native `UNIQUE`/`FOREIGN KEY`/`ON CONFLICT`.** Uniqueness (dedup) is emulated at the application level; referential integrity is emulated at the application level *and* made race-free by the `gts_id` coordination lock ([§3.5](#35-external-dependencies)). ClickHouse's `ReplacingMergeTree` collapsing is an eventual-consistency backstop for dedup convergence, not a substitute for either constraint.
- **No `sqlx` driver.** Schema provisioning is a hand-rolled, idempotent DDL-statement runner (embedded SQL with a fixed 1-year TTL default, no external migration-tracking framework) plus `ensure_retention_ttl` to reconcile `retention_period_secs` on every `init` — see [§3.2](#32-component-model).
- **`UPDATE` and `ALTER TABLE ... DELETE` are asynchronous background mutations in ClickHouse**, unsuitable for any request-path operation with a latency budget. No request-path code path issues `ALTER TABLE ... UPDATE` or `ALTER TABLE ... DELETE`; every `usage_records` status transition is a new `INSERT`. The one exception is `usage_type_catalog` row deletion, which uses a lightweight `DELETE FROM ... WHERE ...` statement — a distinct ClickHouse primitive from `ALTER TABLE ... DELETE` — issued with `lightweight_deletes_sync = 2` set on the statement so that it returns only once the matching row is masked from every subsequent query on every replica (physical removal of the underlying data happens later, in background merges), making it suitable for the request path ([§3.6](#36-interactions--sequences) Delete Usage Type sequence). The synchronous masking is a property of that explicit setting, not of `DELETE FROM` itself: the server default is `2` only on self-managed deployments, is `1` on ClickHouse Cloud, and can be `0` via a settings profile — so the plugin states it rather than inheriting it. No tombstone flag or higher-version marker row is needed to represent "deleted" for this table as a result.
- **Cluster ADR-002 deviation: remote I/O while holding a cluster lock.** Cluster ADR-002 forbids issuing remote I/O while holding a cluster lock. This plugin must hold the `gts_id` exclusive coordination lock across ClickHouse SQL statements — that is the mechanism that closes the referential-integrity race. Call sites therefore call `ClusterLockGuard::ensure_still_held()` (lease renew) immediately before every mutating ClickHouse write and abort with `Transient` on `ClusterError::LockExpired`. Operators must size `lock_ttl_secs` above worst-case critical-section latency (i.e. above the ClickHouse round-trips performed while the lock is held); config validation enforces the floor that makes the renew meaningful — `lock_ttl_secs` must strictly exceed the client deadline (`request_timeout_secs + 5s`), so the single round-trip that follows a renew cannot outlive the lease. See [§3.5](#35-external-dependencies) and PRD.md §13 for the timeout-sizing guidance note.
- **Dependencies policy** (`guidelines/DEPENDENCIES.md`): any synchronous lock introduced by later phases uses `parking_lot::Mutex`/`RwLock`, not `std::sync`; any YAML use, if introduced, uses `serde-saphyr`.

**Constraint ID index** (used for cross-artifact traceability):

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-no-transactions`

No multi-statement ACID transactions; every multi-step sequence uses independently-committing ClickHouse statements.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-no-in-place-update`

No `UPDATE` or `ALTER TABLE ... DELETE` on the request path; every `usage_records` status transition is a new versioned `INSERT`.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-vendor-isolation`

All ClickHouse-specific SQL, schema, and client dependencies are confined to this crate; no dependency on the host `usage-collector` crate.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-dedup-race-window`

Residual dedup race is limited to the theoretical same-id/different-gts_id hash-collision case; the same-gts_id concurrent-create race is closed by the exclusive coordination lock.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-final-qualified-reads`

Every read path uses `FINAL` (or equivalent `argMax`-grouped rewrite) to force `ReplacingMergeTree` version resolution at query time.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-aggregation-bucket-cap`

The plugin caps aggregation output to `MAX_AGGREGATION_BUCKETS + 1` (100,001) rows server-side via `LIMIT` on every aggregation query.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-gts-lock-required`

The exclusive `gts_id` coordination lock is required on every create-record and delete-usage-type path; proceeding without the lock is not permitted (fail-closed `Transient` on unavailability).

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-retention`

The `usage_records` TTL clause is provisioned with a fixed 1-year default in `CREATE TABLE IF NOT EXISTS`, then reconciled on every `init` by `ensure_retention_ttl`: if the live TTL interval differs from `retention_period_secs`, the plugin issues `ALTER TABLE … MODIFY TTL`.

## 3. Technical Architecture

### 3.1 Domain Model

Reuses the SDK's domain types verbatim (`UsageRecord`, `UsageType`, `UsageKind`, `AggregationSpec`, `AggregationResult`, `MetadataFilter`, `UsageCollectorPluginError`) — this plugin introduces no new domain types at the SPI boundary. Infra-internal types (entity rows, the deactivation-marker shape) are plugin-local and never cross the SPI.

### 3.2 Component Model

| Component | Responsibility |
| --- | --- |
| Plugin Module (`gear.rs`) | `init()`: load/validate config, build the ClickHouse client (via `build_client` — parses `database_url` into a bare base URL plus separate user/password/database, applied via `with_user`/`with_password`/`with_database` so the `clickhouse::Client` URL path and userinfo are never mixed), build the Coordination Lock Manager's cluster lock facade, run initial schema provisioning (`apply_migrations`), reconcile `usage_records` TTL with `retention_period_secs` (`ensure_retention_ttl`), perform the four-step GTS/`types-registry`/`ClientHub` registration handshake. |
| Schema Migration (`infra/storage/pool.rs`, `migrations/0001_init.sql`) | A single embedded SQL file executed as a sequence of idempotent `CREATE TABLE IF NOT EXISTS` DDL statements at `init`. The `usage_records` DDL bakes a fixed 1-year TTL default (`INTERVAL 31536000 SECOND`). After migration, `ensure_retention_ttl` reads `system.tables.create_table_query`, compares the live TTL interval to `retention_period_secs`, and issues `ALTER TABLE usage_records MODIFY TTL …` when they differ. Comment lines (`--`) are stripped before statement splitting to avoid false statement boundaries from semicolons in prose comments; statements are split while respecting single-quoted string literals. No `schema_migrations` tracking table, no external framework. |
| SPI Storage Adapter (`domain/adapter.rs`) | The sole `UsageCollectorPluginV1` implementation; pure delegation to `RecordStore`/`CatalogStore`, identical in shape to the reference plugin's adapter. |
| Coordination Lock Manager (`infra/coordination/lock_manager.rs`) | Lazily resolves cluster `DistributedLockV1` for profile `usage-collector` and exposes a **single exclusive per-`gts_id` mutex** via `acquire_exclusive_for_create` / `acquire_exclusive_for_delete` — both paths acquire the same lock name, making create and delete mutually exclusive with each other and serializing concurrent creates for the same `gts_id`. Exposes `ClusterLockGuard` with `ensure_still_held` (lease renew before mutating write) and explicit `release` (drop is best-effort fallback only). Implements the `CatalogLockPort` seam trait for `ChCatalogStore` testability. |
| `CatalogLockPort` trait (`infra/coordination/lock_manager.rs`) | Testability-seam trait exposing `acquire_exclusive_for_create(gts_id)` and `acquire_exclusive_for_delete(gts_id)` → `Box<dyn LockGuardPort>` (both resolve to the same exclusive lock name; the two entry points exist only to label the `mode` metric). Both stores (`ChCatalogStore` and `ChRecordStore`) depend on `Arc<dyn CatalogLockPort>` rather than `Arc<LockManager>` directly, enabling unit tests with stub implementations. `LockManager` implements this trait in production. |
| `LockGuardPort` trait (`infra/coordination/lock_manager.rs`) | Testability-seam trait for lock guard operations: `ensure_still_held()` (lease renew) and `release()`. `ClusterLockGuard` is the production implementation; the catalog delete critical section calls through it so the path can be exercised in tests with stub guards. |
| Record Store (`infra/storage/record_store.rs`) | `usage_records` CRUD/query: insert with dedup and the lock-protected, plugin-owned catalog-existence check, batch insert, get, keyset list, pushed-down aggregation, and the versioned-marker deactivation cascade. |
| Catalog Store (`infra/storage/catalog_store.rs`) | `usage_type_catalog` CRUD: create (with pre-existence check and idempotency absorb for identical payloads — same `kind`+`metadata_fields` → silent absorb; different `kind`/`metadata_fields` → `UsageTypeAlreadyExists`), get, list (keyset-paginated, fixed `gts_id ASC` order, forward-only cursor), and the lock-protected verify-then-delete real row removal. Spawns a single background catalog-size refresh worker that coalesces mutation-triggered `count()` refreshes via a `tokio::sync::Notify` signal, racing each count against the gear cancellation token for prompt shutdown. |
| Query Translator (`infra/storage/query/*`) | OData `$filter`/`$orderby`/keyset-cursor → parameterized ClickHouse SQL via `sea-query` + `sea-query-clickhouse` (`ClickhouseSelect` for `FINAL`), reusing the reference plugin's allowlisted-identifier approach. Forward-only cursor enforcement (`ensure_forward_cursor`) is a v1 constraint; backward paging is not implemented. |
| Metrics (`infra/metrics.rs`) | `uc_clickhouse_*` OpenTelemetry instrument inventory (see [§4](#4-additional-context) Observability). |

**Component ID index** (used for cross-artifact traceability):

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-module`

Plugin Module (`gear.rs`): `init()` lifecycle, config, ClickHouse client, lock-manager bootstrap, schema migration, GTS/ClientHub registration.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-adapter`

SPI Storage Adapter (`domain/adapter.rs`): sole `UsageCollectorPluginV1` implementation, pure delegation, error classification, cursor encoding.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-migrations`

Schema Migration (`migrations/0001_init.sql` + runner): idempotent DDL runner with fixed 1-year TTL default; `ensure_retention_ttl` reconciles config on startup.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-lock-manager`

Coordination Lock Manager (`infra/coordination/lock_manager.rs`): single exclusive per-`gts_id` mutex, `acquire_exclusive_for_create`/`acquire_exclusive_for_delete`, `ClusterLockGuard`.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-catalog-lock-port`

`CatalogLockPort` and `LockGuardPort` testability-seam traits; `LockManager` implements both in production.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-record-store`

Record Store (`infra/storage/record_store.rs`): `usage_records` CRUD, dedup, batch insert, keyset list, pushed-down aggregation, deactivation cascade.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-catalog-store`

Catalog Store (`infra/storage/catalog_store.rs`): `usage_type_catalog` CRUD, lock-protected verify-then-delete, catalog-size background refresh.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-query-translator`

Query Translator (`infra/storage/query/*`): translates OData `$filter`/`$orderby`/keyset-cursor into parameterised ClickHouse SQL with bound parameters for caller-derived values and a closed allowlist for caller-influenced identifiers.

### 3.3 API Contracts

Identical to the reference plugin: the ten `UsageCollectorPluginV1` methods across the record group (`create_usage_record`, `create_usage_records`, `get_usage_record`, `query_aggregated_usage_records`, `list_usage_records`, `deactivate_usage_record`) and the catalog group (`create_usage_type`, `get_usage_type`, `list_usage_types`, `delete_usage_type`). No new methods, no widened signatures — the SPI is the same trait implemented by both backends. Error mapping is detailed in [§4](#4-additional-context)'s error-classification note (see PRD.md §5 "Typed Error Classification").

### 3.4 Internal Dependencies

- `usage-collector-sdk` — the SPI trait, domain models, error vocabulary, and GTS plugin spec (`UsageCollectorPluginSpecV1`, `type_id = "gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.plugin.v1~"`, reused verbatim — the same GTS type identifies every backend; instances are distinguished by their registered instance segment, not by type).
- `types-registry-sdk` / `types-registry` — plugin discovery and instance registration.
- `toolkit`, `toolkit-macros`, `toolkit-odata` — gear lifecycle, config macros, and the shared OData query-AST types the query translator consumes.

### 3.5 External Dependencies

| Dependency | Version | Rationale |
| --- | --- | --- |
| `sea-clickhouse` (imported as `clickhouse`) | `0.15.x` | SeaQL soft fork of the official `clickhouse-rs` HTTP client — same typed `serde`/`Row`-derive insert API and TLS/`lz4` features as upstream, plus `sea-ql` `DataRow` / `sea_query::Value` decoding for variable-column aggregates. Chosen over `clickhouse-native-client` (native TCP, lower maturity) and `klickhouse` (unofficial, less active). |
| `sea-query` + `sea-query-clickhouse` | `1.0.0-rc.34` / `0.1.4` | Typed SQL AST builder with ClickHouse dialect extensions (`ClickhouseSelect::final()`, PREWHERE, …). Runtime SELECT/DELETE statements are built here; DDL stays in `migrations/0001_init.sql`. Coexists with workspace `sea-orm`'s `sea-query` 0.32. |
| Cluster gear `DistributedLockV1` (profile `usage-collector`) | linearizable backend | Backs the per-`gts_id` exclusive coordination lock ([§3.6](#36-interactions--sequences)) that closes the referential-integrity race. Replaced the former ZooKeeper/Keeper reader/writer recipe; both create and delete paths acquire the same exclusive mutex name. |
| `cluster-sdk` | workspace | Resolves `DistributedLockV1` with `LockCapability::Linearizable`; lock names are `gts-{xxh3_64_hex_16}` (zero-padded 16-digit lowercase hex of the `gts_id` bytes) under a `usage-collector` scope prefix via `DistributedLockV1::scoped`. |

**Coordination lock is required independent of ClickHouse's own topology.** This plugin's lock requirement is driven by the number of **gear process instances** that may concurrently call this plugin against the same ClickHouse backend — not by whether ClickHouse itself is single-node or replicated. Operators MUST provision the cluster gear profile `usage-collector` (standalone cache is sufficient for single-node). This is a platform coordination dependency rather than a ZooKeeper/Keeper side-car, documented in PRD.md §3.1/§10.

**Lock semantics: single logical exclusive mutex, not reader/writer.** The implementation uses one exclusive lock name per `gts_id` — both `acquire_exclusive_for_create` and `acquire_exclusive_for_delete` acquire the same name, and all three mutating paths (`create_usage_record(s)`, `create_usage_type`, `delete_usage_type`) take it, making them mutually exclusive with each other and serializing concurrent creates for the same `gts_id`. This replaced the former reader/writer (shared-for-create, exclusive-for-delete) design; the result is simpler, and the additional serialization of concurrent creates is acceptable because the coordinator is a logical exclusive mutex via cluster `DistributedLockV1` (profile `usage-collector`) rather than a Keeper round-trip per create. With the standalone cache provider that mutex is in-process (sufficient for single-node operation and tests, with no Keeper); multi-node deployments bind whatever linearizable backend the profile uses — mutual-exclusion semantics are unchanged either way.

**Cluster ADR-002 deviation.** Cluster ADR-002 forbids issuing remote I/O while holding a cluster lock. This plugin must hold the exclusive lock across ClickHouse SQL statements (that is the referential-integrity closure mechanism). Call sites therefore invoke `ClusterLockGuard::ensure_still_held()` (lease renew) immediately before the mutating write and abort with `Transient` on `ClusterError::LockExpired`. Size `lock_ttl_secs` above worst-case critical-section latency (ClickHouse round-trips while the lock is held), and never at or below the client deadline (`request_timeout_secs + 5s`) — config validation rejects that at startup, because a lease a single round-trip can outlive makes the pre-write renew useless. `lock_timeout_secs` bounds the acquisition wait.

**Workload isolation allocation**: the `sea-clickhouse` `Client` (crate path `clickhouse`) is a lightweight, cheaply-cloneable handle over an internal `hyper` connection pool, and this plugin configures one such `Client` shared by both the ingestion and query paths for v1 — the accepted, documented contention point, not a silently-assumed-solved allocation, per `cpt-cf-usage-collector-nfr-workload-isolation`.

**The pool is not operator-tunable, by constraint of the client crate.** `sea-clickhouse` 0.15.x exposes no pool-bound knob a config field could drive: `Client` has no pool builder method; `with_setting`/`with_option` pass ClickHouse **server** settings, not client pool settings; and the one seam that could carry a pre-built pooled connector, `Client::with_http_client`, is unusable from another crate because the `HttpClient` trait lives in a private module and its bound names the private `clickhouse::request_body::RequestBody` type. Even reaching it would buy little — `hyper_util`'s legacy client builder offers only `pool_idle_timeout` and `pool_max_idle_per_host`, neither of which bounds concurrent in-flight connections. There is therefore **no** `pool_max_connections`-style field in `ClickHousePluginConfig`, and this design does not promise one. Mitigation for the contention below is consequently operational (server-side settings profiles/quotas, or separate plugin instances), not configuration. If a production deployment observes query bursts degrading ingestion latency, a future revision can still split ingestion and query onto two `Client` instances against different endpoints (additive, non-breaking to the SPI). Operators experiencing contention in the interim MAY mitigate by running two plugin instances (distinct GTS priorities) against read-replica vs. write-primary ClickHouse endpoints — an operational workaround documented in the plugin README, not a code-level split.

**Risk this poses to the ingestion-throughput NFR.** Because ingest and query share one client, one pool, and one ClickHouse server, a query burst is a direct threat to `cpt-cf-usage-collector-nfr-throughput` ([§1.2](#12-architecture-drivers-nfr-allocation)): the write path can wait on pool acquisition behind long-running aggregations, and the plugin has no reservation, priority, or admission control that protects it. This is accepted for v1 rather than designed away, and the mitigation is operational: operators correlate `uc_clickhouse_pool_acquire_duration_seconds` with `uc_clickhouse_query_requests_total{query_kind="aggregated"}` and `uc_clickhouse_insert_duration_seconds{mode="batch"}` to attribute ingest slowdowns to read contention, bound read cost server-side with ClickHouse settings profiles/quotas, and — where hard separation is required — run the two-instance split above (see the plugin README's "Workload isolation and pool contention").

### 3.6 Interactions & Sequences

#### Ingest with Dedup (`cpt-cf-uc-ch-plugin-seq-ingest-dedup`)

**ID**: `cpt-cf-uc-ch-plugin-seq-ingest-dedup`

1. Compute the record's deterministic `id` (ADR-0013/ADR-0014 4-tuple derivation — inherited unchanged from the reference plugin's identity scheme).
2. **Acquire the exclusive `gts_id` coordination lock** (`cpt-cf-uc-ch-plugin-seq-gts-lock-exclusive`, held via the Coordination Lock Manager over cluster `DistributedLockV1` — [§3.2](#32-component-model), [§3.5](#35-external-dependencies)) for the record's `gts_id`. Both create and delete paths acquire the same exclusive mutex name, so this acquisition blocks behind any in-flight create **or** delete for the *same* `gts_id`. The lock is held for the remainder of this sequence and released explicitly on every exit path (`ClusterLockGuard::release`; `Drop` spawns a best-effort release as a fallback only, since `LockGuard` drop is a no-op in the cluster SDK). **Lock-manager unavailable**: if the lock cannot be granted within `lock_timeout_secs`, return `Transient` rather than proceeding unlocked.
3. **Plugin-owned referential-integrity check**, performed *while holding the exclusive lock* (mirrors the reference plugin's reliance on a DB-native FK firing at insert time — see `cpt-cf-uc-ch-plugin-principle-spi-conformance`, [§2.1](#21-design-principles)): `SELECT ... FINAL FROM usage_type_catalog WHERE gts_id = ?`. Absent (including previously-deleted) → release the lock, return `UsageTypeNotFound { gts_id }` (mirrors the reference plugin's FK-violation mapping in `record_store.rs::map_insert_error`). Because no other exclusive lock for the same `gts_id` can be held concurrently, this check and the eventual `INSERT` (step 6) are ordered against every `delete_usage_type` call for this `gts_id` with no gap — this is the mechanism that satisfies [`plugin-spi.md`](../../../docs/plugin-spi.md#method-9--delete-usage-type) Method 9's "MUST NOT admit a window" without qualification.
4. `SELECT ... FINAL WHERE tenant_id = ? AND gts_id = ? AND created_at = ? AND idempotency_key = ?` — a lookup against the `usage_records` `ReplacingMergeTree` table on the SPI's canonical dedup tuple. The three leading columns are the `ORDER BY` prefix, so the read resolves to a primary-key point rather than a filtered scan, and `idempotency_key` applies as a residual filter (see [§3.7](#37-database-schema--tables)). It is deliberately **not** keyed on `id`: `id` is a projection of this same tuple, so an `id`-keyed lookup would miss a stored row whose `id` disagrees with its own tuple and re-insert it under an idempotency key already in use. Because the coordination lock is a **single logical exclusive mutex** per `gts_id` (not a reader/writer lock), concurrent creates for the same `gts_id` serialize at step 2 — only one create for a given `gts_id` executes this check at a time.
5. **Not found**: first call `ClusterLockGuard::ensure_still_held()` (lease renew) to confirm the lease survived steps 3-4's ClickHouse round-trips — on `ClusterError::LockExpired` increment `uc_clickhouse_lock_manager_unavailable_total{mode="create"}`, release the lock, and return `Transient` without writing (cluster ADR-002 deviation, [§3.5](#35-external-dependencies)). Only with the lease confirmed, `INSERT` one row with `status = 'active'`, `version = <monotonic>` (e.g. ingestion-time microseconds). Release the lock. Return the new record.
6. **Found, canonical fields equal**: silent absorb — release the lock, return the stored row without inserting.
7. **Found, canonical fields differ**: release the lock, return `IdempotencyConflict`.
8. **Residual dedup race** (documented, narrowed as far as achievable for the dedup path only — referential integrity is fully closed by the exclusive lock): the exclusive lock serializes creates for the same `gts_id`, but two callers with *different* `gts_id`s that happen to derive the same deterministic `id` (an extreme edge case) are not serialized. In practice, the race that remained documented under the former reader/writer design (concurrent creates for the same `gts_id` racing steps 4-5) no longer exists, since both callers now serialize at step 2. The residual race is now limited to the theoretical hash-collision case and is effectively eliminated in normal operation. The `ReplacingMergeTree` convergence backstop (collapsing on merge/`FINAL`) remains as a defense-in-depth layer.

#### Batch Ingest (`cpt-cf-uc-ch-plugin-seq-ingest-batch`)

**ID**: `cpt-cf-uc-ch-plugin-seq-ingest-batch`

1. **Partition the batch by `gts_id`**, preserving each record's input position so outcomes can be written back positionally, then **sort the partition keys**. The sort no longer carries a deadlock argument (step 2 removes hold-and-wait outright); it is what makes the per-partition `version` ranges of step 5 — and the log order — deterministic rather than hash-map-iteration dependent.
2. **Run every partition as its own pipeline, all started at once.** A partition acquires only *its own* exclusive `gts_id` coordination lock (same lock name as the single-record path, the catalog-create path, and the delete path), holds it for its own critical section (steps 3-7) and releases it there (step 8). A partition queued behind a contended `gts_id` therefore delays nothing but itself — no other partition's read, renew or `INSERT` waits on it. Because each pipeline holds at most one lock at any moment, two concurrent multi-type batches covering the same types in opposite orders can queue but can never cycle: hold-and-wait, the precondition for that deadlock, does not arise. On acquisition failure for one partition, write that partition's error to every one of its records' positions and leave the other partitions unaffected — no whole-batch abort.
3. **Per partition, while holding its lock: catalog-existence check** for that `gts_id`. Absent → every record in this partition gets `UsageTypeNotFound` at its own input position, the partition's lock is released, and the remaining partitions proceed unaffected.
4. **Per partition: one batched dedup pre-check** — a single `SELECT ... FINAL WHERE (tenant_id, gts_id, created_at, idempotency_key) IN (...)` over that partition's records, issued under that partition's own lock and therefore overlapping the other partitions' round-trips, never their critical sections' mutual exclusion.
5. **Per partition: resolve its records' outcomes in input order** — existing row with equal canonical fields → silent absorb; existing row with differing fields → `IdempotencyConflict`; a second row for the same dedup key inside this same batch is absorbed only if canonically identical, otherwise it is likewise a conflict; anything else joins this partition's pending insert set with a monotonic `version`. Within-batch dedup is partition-local, which is exact rather than an approximation: the canonical dedup tuple contains `gts_id`, so two records sharing a dedup key are always in the same partition. Each partition's `version` offsets come from a range reserved *before* any lock is taken (the batch's base merge version plus the record count of every preceding sorted partition), so concurrently composing partitions mint disjoint, reproducible versions without a shared counter.
6. **Per partition: `ensure_still_held()` (lease renew)** immediately before its own `INSERT`, so a lease lost during this partition's ClickHouse round-trips aborts the partition with `Transient` (incrementing `uc_clickhouse_lock_manager_unavailable_total{mode="create"}`) instead of writing under an expired lock — the same cluster ADR-002 deviation the single-record path applies ([§3.5](#35-external-dependencies)). A partition that composed no rows has no write to protect and skips the renew, so a lapsed lease cannot discard absorb/conflict outcomes that a retry could only re-derive.
7. **Per partition: one multi-row `INSERT` of that partition's non-duplicate rows**, issued inside that partition's lock. A batch spanning `N` distinct `gts_id`s issues `N` lock acquisitions and `N` `INSERT`s — exactly one of each for the common single-`gts_id` batch. Whole-batch write atomicity is deliberately *not* claimed: the SPI contract is per-record outcomes, and each `INSERT` is still one atomic part write covering exactly the partition its lock protects.
8. **Release the partition's lock as soon as its own `INSERT` completes** (including on the insert-failure path), rather than holding it across partitions it does not own.
9. **Return one outcome per input record in input order**, mirroring the reference plugin's positionally-aligned batch contract. A failed `INSERT` is reported per affected record slot of that partition rather than as a top-level error, so outcomes already decided for absorbed rows survive.

**Design-correction caveat (multi-`gts_id` batches).** The SPI does not restrict a batch to a single `gts_id` — `plugin-spi.md` Method 2 places no such constraint on `create_usage_records`, and the host resolves each record's `gts_id` independently (see `usage-emission.md`'s `catalog-existence-and-kind-lookup`). An earlier revision of this design incorrectly assumed a batch was always single-`gts_id` and amortized the exclusive lock + catalog check over the whole call using only `records[0].gts_id`; a mixed-`gts_id` batch would then let every non-first record's `gts_id` bypass its own referential-integrity check and lock, letting it race a concurrent `delete_usage_type` for that type into an orphaned row. The per-partition sequence above is the correction: it keeps the common single-`gts_id`-batch case's amortization ([§1.2](#12-architecture-drivers-nfr-allocation)) while closing the referential-integrity gap for the multi-`gts_id` case, and it attributes a referential-integrity rejection per `gts_id` rather than to the whole batch.

#### Deactivation Cascade (`cpt-cf-uc-ch-plugin-seq-deactivate-cascade`)

**ID**: `cpt-cf-uc-ch-plugin-seq-deactivate-cascade`

1. `SELECT ... FINAL WHERE id = ? OR (corrects_id = ? AND status = 'active')` — resolve the target's current status plus every active depth-1 compensation referencing it, in one unlocked read.
2. Target not found → `UsageRecordNotFound`. Target already `inactive` → `UsageRecordAlreadyInactive`.
3. Otherwise, compose one versioned marker row per affected `id` (target + compensations), each carrying `status = 'inactive'` and a `version` strictly higher than the row it supersedes.
4. Issue **one** multi-row `INSERT` for all marker rows. A single ClickHouse `INSERT` is applied as one atomic part write, so a reader querying with `FINAL` either sees the pre-cascade state (insert not yet visible) or the fully-flipped state (insert visible) — never a partial cascade.
5. **No late-compensation race exists here** (unlike an earlier draft of this design, which incorrectly hedged on one): per `plugin-spi.md` Method 5's caller-side concurrency rule, a compensation referencing the target while it is being deactivated is rejected by the gateway's own L1 "MUST be active" check **before** `create_usage_record` is ever dispatched for that compensation — the plugin never coordinates with an in-flight cascade because the host structurally prevents the racing write from reaching the SPI at all. The plugin's only genuine atomicity property to state here is the one already established: the cascade's flip is atomic as a single `INSERT` (step 4), so no reader ever observes a partial cascade.

#### Aggregated Query (`cpt-cf-uc-ch-plugin-seq-query-aggregated`)

**ID**: `cpt-cf-uc-ch-plugin-seq-query-aggregated`

Pushed-down `SELECT <dims>, <AGG> FROM usage_records FINAL WHERE gts_id = ? AND status = 'active' [AND <filter>] [AND <metadata>] [GROUP BY ...] LIMIT {MAX_AGGREGATION_BUCKETS + 1}`, honoring the same `SUM`-nets-compensations vs. other-ops-exclude-compensations partition rule as the reference plugin (`corrects_id IS NULL` for non-`SUM` ops). Per `plugin-spi.md` Method 3's pushdown obligation, the plugin **MUST** cap its own grouped result to `MAX_AGGREGATION_BUCKETS + 1` buckets (100,001) via the `LIMIT`, letting the gateway distinguish a result exactly at the cap from one over it before it applies the `400 AGGREGATION_RESULT_TOO_LARGE` rejection — the plugin never materializes an unbounded bucket set even transiently. The `FINAL` modifier is required here (unlike a plain `MergeTree` table) so a not-yet-merged duplicate or stale pre-deactivation row is never double-counted or mis-classified; this is the plugin's primary query-cost tradeoff for correctness (see [§3.8](#38-consistency--concurrency)).

#### Keyset List (`cpt-cf-uc-ch-plugin-seq-list-keyset`)

**ID**: `cpt-cf-uc-ch-plugin-seq-list-keyset`

`SELECT ... FROM usage_records FINAL WHERE gts_id = ? [AND <filter>] [AND <metadata>] [AND <keyset>] ORDER BY <order> LIMIT <n+1>`, reusing the reference plugin's look-ahead-row-then-truncate keyset pagination pattern, translated to ClickHouse's parameter-binding syntax. The wire-level cap (≤ 1,000 records) bounds `<n>`, independent of the aggregation bucket cap above. **The absence of a `status = 'active'` predicate here is deliberate, not an omission relative to the aggregated query above**: `plugin-spi.md` Method 4 is status-agnostic by contract, because Method 5 does not return the set of cascade-flipped ids and instead directs operators to enumerate it via a follow-up `list_usage_records` filtered on `status` / `corrects_id` — a predicate baked in here would make that enumeration impossible. A caller wanting active-only rows passes `status` in its own `$filter`. The reference plugin's list path behaves identically. The keyset predicate itself is a **strict** row-value comparison (`>` ascending, `<` descending); see `query/keyset.rs`. **`<order>` MUST be a total order**: it has to end in a stable globally-unique tie-breaker — `id` on this path, `gts_id` on the type-catalog list — and the cursor's key tuple spans that same full order. The host supplies it: per `plugin-spi.md` Method 4 the gateway normalizes every caller `$orderby` to end in the canonical `(created_at, id)` suffix (in the caller's sort direction) before dispatch, so `ORDER BY <order>` and the keyset predicate always agree on a unique row position. Without that tie-breaker the strict comparison skips every row that ties the last returned row on the non-unique key and did not fit on the page — silent data loss across the page boundary, not merely an unstable ordering.

#### Create Usage Type (`cpt-cf-uc-ch-plugin-seq-create-type`)

**ID**: `cpt-cf-uc-ch-plugin-seq-create-type`

1. **Acquire the exclusive `gts_id` coordination lock** — the same lock name `delete_usage_type` and the record-ingest paths take ([§3.5](#35-external-dependencies)), so the pre-existence check and the `INSERT` form one critical section and two concurrent creates for the same `gts_id` serialize instead of racing. **Lock-manager unavailable**: return `Transient` without touching the catalog.
2. `SELECT ... FINAL WHERE gts_id = ?` pre-existence check, under the lock, then:
   - **Already exists, identical payload** (`kind` and `metadata_fields` equal): silent absorb — return the stored type without inserting.
   - **Already exists, different payload** (`kind` or `metadata_fields` differ): `UsageTypeAlreadyExists`.
   - **Absent**: call `ClusterLockGuard::ensure_still_held()` (lease renew) immediately before the `INSERT` to confirm the lease survived the pre-existence check's ClickHouse round-trip — on `ClusterError::LockExpired` increment `uc_clickhouse_lock_manager_unavailable_total{mode="create"}`, release the lock, and return `Transient` without writing (cluster ADR-002 deviation — [§3.5](#35-external-dependencies)). Only with the lease confirmed, `INSERT` with `version = current_version()` (epoch microseconds) and signal the background catalog-size refresh worker.
3. **Release the lock** explicitly on every exit path (absorb, conflict, insert, error), as elsewhere — the cluster guard's `Drop` is a no-op.

Because the lock serializes same-`gts_id` creates, the loser of a race observes the winner's row and returns an absorb or `UsageTypeAlreadyExists` rather than inserting a second physical row. `ReplacingMergeTree(version)` resolution remains the convergence backstop (highest version wins on merge/`FINAL`) rather than the primary defense.

#### Delete Usage Type — Lock-Protected Verify-Then-Delete (`cpt-cf-uc-ch-plugin-seq-delete-type-fk`)

**ID**: `cpt-cf-uc-ch-plugin-seq-delete-type-fk`

This sequence was substantially redesigned in a second follow-up design review (from a fencing-delay approach that could not satisfy `plugin-spi.md` Method 9's "MUST NOT admit a window"), and subsequently simplified from a reader/writer lock design to a **single exclusive mutex** shared by both create and delete paths.

1. **Acquire the exclusive `gts_id` coordination lock** (via the Coordination Lock Manager over cluster `DistributedLockV1`). Because every mutating path acquires the same exclusive lock name, this acquisition waits for any in-flight `create_usage_record`/`create_usage_records`/`create_usage_type` call's lock for this `gts_id` to release, and once granted, no new create or delete for this `gts_id` can start until this delete releases it. **Lock-manager unavailable**: if the cluster cannot grant the lock within its configured timeout, return `Transient` rather than proceeding unlocked (step 7).
2. **While holding the exclusive lock**: `SELECT ... FINAL WHERE gts_id = ?` — confirm the type exists; absent → release the lock, return `UsageTypeNotFound`.
3. **Verify** (now authoritative, not probabilistic): probe `usage_records` for any row with this `gts_id` (bounded — capped scan via `LIMIT REF_COUNT_CAP`, mirroring the reference plugin's `REF_COUNT_CAP` pattern). Because step 1's exclusive lock excludes every concurrent create for this `gts_id`, any reference that existed before this lock was granted is visible to this probe, and no new reference can be created until the lock is released. This probe result is current with respect to every write that could ever reference this `gts_id`.
4. **References found**: release the lock without issuing the `DELETE`; return `UsageTypeReferenced { gts_id, sample_ref_count }`.
5. **No references found**: call `ensure_still_held()` (lease renew) on the lock guard to confirm the lease is still valid after the ClickHouse round-trip — returns `Transient` if the lease expired during the critical section (cluster ADR-002 deviation, [§3.5](#35-external-dependencies)). If renew succeeds, issue a lightweight `DELETE FROM usage_type_catalog WHERE gts_id = ?` with `lightweight_deletes_sync = 2` on the statement — a real row removal whose masking is complete by the time the statement returns, which is what makes a subsequent create's `FINAL` pre-existence check (§3.6 Create Usage Type) see the type as absent. Because the statement waits, its latency lands inside the critical section that `lock_ttl_secs` has to cover. Release the lock. Return success.
6. **No rollback step exists**: the `DELETE` is issued only *after* the verify step (step 3) has already run to completion while holding the exclusive lock — there is no possibility of a reference landing after the row is removed but being missed by the verify.
7. **No residual race**: the exclusive-mutex lock discipline makes this sequence's referential-integrity guarantee unconditional, satisfying `plugin-spi.md` Method 9 without qualification.
8. **Lock-manager unavailability (fail-closed)**: if the cluster connection cannot grant or release a lock within its configured timeout, both this sequence and the Ingest sequence's lock acquisition **MUST** return `Transient` rather than proceeding without the lock. Failing closed is the whole defense here: the orphan-detection counter once specified as a second line of defense against this class of failure is **not implemented** ([§4](#4-additional-context) Observability), so `uc_clickhouse_lock_manager_unavailable_total` and the resulting `Transient` errors are the only signals operators have for it.

### 3.7 Database Schema & Tables

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-db-schema`

#### Table: usage_type_catalog

**ID**: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

| Column | Type | Description |
| --- | --- | --- |
| `gts_id` | `String` | GTS usage-type identifier; sorting-key column (the closest ClickHouse analog of a primary key). |
| `kind` | `Enum8('counter'=1,'gauge'=2)` | Counter or gauge, stored verbatim. |
| `metadata_fields` | `Array(String)` | Closed list of allowed metadata key names, stored verbatim. |
| `version` | `UInt64` | `ReplacingMergeTree` version column; resolves the create sequence's own concurrent-insert race (two racing creates for the same `gts_id`) on merge/`FINAL`. |

**Sorting key (`ORDER BY`)**: `(gts_id)`. **Engine**: `ReplacingMergeTree(version)`. There is no native `PRIMARY KEY`/`UNIQUE` constraint; uniqueness-on-`gts_id` is an application-level invariant enforced by the create sequence's pre-existence check ([§3.6](#36-interactions--sequences)), not a schema-level guarantee. Deletion (`delete_usage_type`, [§3.6](#36-interactions--sequences)) is a real row removal via a lightweight `DELETE FROM` statement, not a tombstone-flag column or a higher-version marker row.

**Constraints**: none native (no FK target support); referenced only conceptually by `usage_records.gts_id` — the reference is enforced entirely in application code (steps in [§3.6](#36-interactions--sequences)), not by the schema.

#### Table: usage_records

**ID**: `cpt-cf-uc-ch-plugin-dbtable-usage-records`

| Column | Type | Description |
| --- | --- | --- |
| `id` | `UUID` | Deterministic gateway-derived record id (`UUIDv5` of the full 4-tuple dedup key including `created_at`, ADR-0013/ADR-0014); persisted verbatim. |
| `tenant_id` | `UUID` | Owning tenant. |
| `gts_id` | `String` | Usage type; application-enforced reference to `usage_type_catalog`. |
| `value` | `Decimal128(9)` | Signed delta. |
| `created_at` | `DateTime64(6)` | Event time; leading `ORDER BY` column after tenant/type. |
| `resource_id` / `resource_type` | `String` | Resource attribution. |
| `subject_id` / `subject_type` | `Nullable(String)` | Optional subject attribution. |
| `idempotency_key` | `String` | Caller idempotency key. |
| `corrects_id` | `Nullable(UUID)` | Set on a compensation row; references the offset row. |
| `status` | `Enum8('active'=1,'inactive'=2)` | Current lifecycle status; status transitions are new versioned rows, never an in-place `UPDATE`. |
| `metadata` | `Map(String, String)` | Caller metadata, stored verbatim. Chosen over a JSON-encoded string because `metadata_fields` is a closed, `String`-typed key set per ADR-0012 simplification 6, which `Map(String, String)` represents natively with efficient `metadata['key']` push-down and no JSON-parsing dependency at query time. |
| `ingested_at` | `DateTime64(6)` | Server insert time. |
| `version` | `UInt64` | `ReplacingMergeTree` version column; a higher value wins on merge/`FINAL` resolution. |

**Sorting key (`ORDER BY`)**: `(tenant_id, gts_id, created_at, id)`. **Engine**: `ReplacingMergeTree(version)`. **TTL**: `created_at + INTERVAL <n> SECOND DELETE` on the `DateTime64(6)` column (no `toDateTime` cast — that would saturate at 2106). The migration DDL bakes a fixed 1-year default (`n = 31536000`). On every `init`, `ensure_retention_ttl` reads the live clause from `system.tables` and, when the parsed interval differs from configured `retention_period_secs`, TTL is missing, or the live clause still wraps `created_at` in `toDateTime`, issues `ALTER TABLE usage_records MODIFY TTL …` so the effective window tracks config across restarts.

**Data-skipping indexes**: `INDEX idx_records_id id TYPE bloom_filter GRANULARITY 1` and `INDEX idx_records_corrects_id corrects_id TYPE bloom_filter GRANULARITY 1`, both declared inside the `CREATE TABLE IF NOT EXISTS`. They exist because two request-path predicates do not lead with the sorting-key prefix and would otherwise read every granule as the table grows: `get_usage_record` (`WHERE id = ?`, where `id` is only the *trailing* sort-key column) and the deactivation cascade (`WHERE id = ? OR (corrects_id = ? AND status = 'active')`, where `corrects_id` is not in the sort key at all). The `ORDER BY` is deliberately left unchanged — it is the dedup identity `ReplacingMergeTree` collapses on, so altering it would change which rows `FINAL` resolves together, whereas skip indexes only prune granules and never affect that resolution. Because they live in the idempotent `CREATE TABLE`, a deployment provisioned before they were added does not acquire them on restart; that needs an explicit `ALTER TABLE usage_records ADD INDEX …` plus `MATERIALIZE INDEX` for pre-existing parts (operator procedure in the plugin README), which is a follow-up migration rather than startup-path work — unlike the TTL clause, which `ensure_retention_ttl` reconciles on every `init`.

**Constraints**: none native (no `UNIQUE`, no `FOREIGN KEY`) — ClickHouse does not support either. Dedup identity and referential integrity are both emulated entirely in application code ([§3.6](#36-interactions--sequences)). The `(tenant_id, gts_id, created_at, id)` sorting key is chosen so that `create_usage_record`'s own dedup lookup — which supplies `tenant_id`/`gts_id`/`created_at` from the canonical dedup tuple — resolves against the three-column sort-key prefix rather than scanning; `idempotency_key`, the tuple's fourth component, is not in the sort key and applies as a residual filter over the handful of rows sharing that exact microsecond. The sort-key choice therefore optimizes both the dominant read pattern (tenant + type + time-range scans for aggregation/list) and the dedup lookup simultaneously.

Because the lookup keys on the canonical tuple and not on `id`, a stored row whose `id` disagrees with its own tuple — which no gateway dispatch can produce, since `id` is a derived projection, but which data corruption can — is surfaced by the canonical-field comparison as `IdempotencyConflict` rather than missed and re-inserted under an idempotency key already in use. ClickHouse cannot enforce uniqueness on the tuple, so when two such rows share one dedup key the lookup prefers the row whose `id` matches the incoming record and otherwise takes the lowest `id`, keeping the choice deterministic rather than dependent on part-read order.

**Additional info**: the exact DDL text (column defaults, `Enum8` value assignment, engine parameter syntax) is authored as literal SQL in the Phase 3 migration file (`migrations/0001_init.sql`), not reproduced verbatim here — this table is the schema's contract, not its implementation.

### 3.8 Consistency & Concurrency

The plugin's published consistency ceiling is **narrower than the reference plugin's** and MUST be stated with concrete, measurable bounds (`cpt-cf-uc-ch-plugin-nfr-consistency-profile`, PRD.md §6.1) rather than a vague "eventually consistent":

- **Single-node deployment: effectively immediate read-after-write for any reader.** A `FINAL`-qualified read observes any `INSERT` whose part has become locally visible, which on a single-node ClickHouse deployment happens synchronously with the `INSERT` call's return (typically sub-millisecond to low-single-digit milliseconds after acknowledgment). This is the default, recommended deployment topology for this plugin's v1 consistency claim.
- **Replicated deployment: bounded by ClickHouse replication lag, not by the plugin.** A read hitting a different replica than the one that served the write is bounded by ClickHouse's own replication lag (via ClickHouse Keeper), typically sub-second under healthy operation. **Measurement method and owner**: replication lag is measured with ClickHouse's own `system.replicas.absolute_delay`, which operators scrape from ClickHouse directly. It is **not** part of this plugin's metric surface — feature 0006 §1.5 lists ClickHouse server metrics as explicitly out of scope, since the lag is not this plugin's own state, so no plugin-emitted OTLP metric and no plugin-owned monitoring procedure exists for it. Operators running a replicated deployment MUST monitor it themselves and MUST configure ClickHouse's `insert_quorum` (a native ClickHouse write-quorum setting) if they require strict read-your-writes across replicas, at a documented write-latency cost proportional to the quorum size. This plugin's default configuration does not set `insert_quorum` (single-node/no-quorum default), and the plugin's own documentation MUST NOT claim a stronger cross-replica bound than "typically sub-second, operator-monitored" without quorum writes enabled.
- **Cross-writer convergence via `FINAL`, not a background-merge wait.** Dedup and deactivation both rely on `ReplacingMergeTree` version resolution; `FINAL` forces this resolution at query time against all currently-visible parts (it does not wait for or trigger an out-of-band background merge), which is why every read path in this plugin **MUST** use `FINAL` (or an equivalent `argMax`-grouped rewrite) rather than reading raw un-collapsed rows. This is the plugin's central query-cost/correctness tradeoff: `FINAL` is more expensive per query than a plain `MergeTree` scan, and the aggregation-latency NFR budget ([PRD.md §6.1](./PRD.md#61-gear-specific-nfrs)) must be met with `FINAL` included, not around it.
- **No cross-row transactional isolation inside ClickHouse itself.** The deactivation cascade is atomic as a *single* multi-row `INSERT`, and the delete sequence's row removal is likewise a single lightweight `DELETE` statement; neither ClickHouse operation is internally isolated from a concurrent, unrelated write. Referential integrity closes its race not via ClickHouse isolation but via an external mutual-exclusion primitive (the `gts_id` exclusive coordination lock, [§3.5](#35-external-dependencies)) that fully orders every create against every delete for the same `gts_id`; the same exclusive mutex also serializes concurrent creates for the same `gts_id`, substantially narrowing the dedup race relative to the former reader/writer design. Every race is enumerated explicitly in [§3.6](#36-interactions--sequences), never left implicit.

## 4. Additional Context

### Non-Applicable Design Domains

- **Security Architecture**: not applicable as a plugin concern beyond transport/injection security — authentication, PDP authorization, and attribution validation are enforced upstream by the gear core; the plugin receives only authorized, validated calls.
- **Deployment Topology**: not detailed here — the plugin is statically linked into the gear process; ClickHouse deployment (single-node vs. replicated, sizing, region) follows the operator's ClickHouse deployment guide, with the consistency-profile caveats in [§3.8](#38-consistency--concurrency) called out for the replicated case specifically. The cluster gear profile `usage-collector` backing the `gts_id` coordination lock ([§3.5](#35-external-dependencies)) is required regardless of ClickHouse topology. A standalone cache provider is sufficient for single-node operation and tests (no Keeper); multi-node deployments bind whatever linearizable lock backend the operator registers for that profile — sizing and operation of that backend are not designed here.

### Observability

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-design-metric-inventory`

All series in this plugin live under the `uc_clickhouse_*` metric namespace (distinct from the reference plugin's `uc_timescaledb_*`). All labels are bounded to enumerated value sets — no unbounded caller-supplied strings (`tenant_id`, `gts_id`, record `id`, `idempotency_key`, etc.) are ever used as metric dimensions; they belong in logs and traces.

**The full instrument inventory, label conventions, and histogram bucket boundaries are documented in [Feature 0006 §3](features/0006-cpt-cf-uc-ch-plugin-feature-observability.md#metric-instrument-inventory).** This section establishes only the namespace prefix and bounded-label policy.

### Security

- TLS is enforced, not merely advisory: `ClickHousePluginConfig::validate` rejects a plaintext `http://` `database_url` at startup unless the config explicitly sets `allow_insecure_http = true`, so a misconfigured DSN fails closed before any connection carrying credentials is attempted, rather than merely warning and proceeding. The gate reads the parsed URL's scheme, which is normalized to lowercase, so a mixed-case `HTTP://` DSN cannot slip past it; a scheme outside `http`/`https` (e.g. the native-protocol `clickhouse://`) is rejected at the same point irrespective of the override, since the client speaks only ClickHouse's HTTP interface. The override exists only for non-TLS development/test connections (e.g. a local Docker `ClickHouse` container) and is additionally logged via `tracing::warn!` on every connection it permits — mirroring the reference plugin's `sslmode` upgrade-with-warning pattern, but backed by a config-level gate rather than a log line alone.
- The connection DSN (embedding credentials) is wrapped in `SecretFromEnv` with no `Display`, `Serialize`, or `PartialEq`, and a `Debug` that emits `<redacted>`, so panic-formatter dumps and `tracing::debug!(?cfg)` traces never print the resolved URL. The raw URL is unwrapped from `SecretFromEnv` only at the connection-build boundary in `build_client`.
- **URL parsing (`ParsedEndpoint`)**: `build_client` parses `database_url` into a bare scheme+host+port base URL and separately extracts user, password, and database, applying them via `Client::with_user`/`with_password`/`with_database`. This is required because `clickhouse::Client::with_url` passes the URL path verbatim as the HTTP request path (ClickHouse's HTTP API only accepts `/`, not arbitrary paths) and silently ignores URL userinfo. Callers embedding credentials with URL-reserved characters in `database_url` must percent-encode them first.
- Every query is built with bound parameters for caller-derived values and a closed allowlist for caller-influenced identifiers — no string interpolation of untrusted input into query text, identical security posture to the reference plugin.
- **Tenant scoping is the host gear's responsibility, not this storage layer's (explicit assumption/boundary).** The read paths apply **no tenant clamp of their own**: `get_usage_record` is a point read by record `id` alone, and `list_usage_records` / `query_aggregated_usage_records` scope by `gts_id` plus whatever filter the host supplies. `tenant_id` is stored and is the leading `ORDER BY` column, but the plugin never injects a `tenant_id = <caller's tenant>` predicate — it has no notion of the calling tenant, since the SPI hands it already-authorized calls (`cpt-cf-uc-ch-plugin-principle-pure-persistence`). Consequence: if the gear core omits or mis-derives the tenant predicate, this plugin will faithfully return cross-tenant rows. Tenant isolation therefore lives entirely in the gear core's authorization and filter construction; this is a deliberate boundary, not an oversight, and any future tenant-clamping obligation would have to arrive as an SPI change carrying the caller's tenant explicitly.

### Deferred (post-v1)

- **Schema evolution beyond the v1 shape** (e.g. adding a column to `usage_records` or `usage_type_catalog` after initial release) is not designed in this document — Foundation's Schema Migration ([§3.2](#32-component-model)) provisions the v1 shape idempotently but has no versioned-migration-file mechanism for evolving it. A future revision requiring a schema change needs a dedicated migration-versioning design (e.g. a `schema_migrations` tracking table, DECOMPOSITION.md §2.1's deliberate-omission note) before it can ship; this is tracked as an open question in PRD.md §13, not silently assumed solved by the "provision idempotently" language.
- Multi-shard distributed-table topology and cross-replica read/write pool splitting remain deferred per PRD.md §4.2.
- **Orphan-reference reconciliation worker** (`uc_clickhouse_orphaned_reference_detected_total`): the periodic background scan that would increment this defense-in-depth counter was not built; the instrument is not registered. See [§4 Observability](#observability) and [Feature 0006 §5](features/0006-cpt-cf-uc-ch-plugin-feature-observability.md) for the deferred rationale.

### Testing Architecture

Integration tests run against a real ClickHouse **and** a cluster lock backend registered **in-process as a standalone cache provider** — no ZooKeeper or Keeper container is required — covering dedup outcomes, compensation persistence, the deactivation cascade, the lock-protected verify-then-delete row removal, keyset pagination, and aggregation correctness including the `MAX_AGGREGATION_BUCKETS + 1` cap. The crate compiles against the SDK trait, giving compile-time SPI conformance.

Unit tests for `ChCatalogStore` use `CatalogLockPort` and `LockGuardPort` stub implementations to exercise the delete critical section (existence check → reference probe → `ensure_still_held` → DELETE) offline, without a live ClickHouse or cluster lock backend.

Referential-integrity-specific coverage: a test that starts a `create_usage_record` call, holds it mid-sequence (after acquiring its exclusive lock, before its `INSERT` completes) via an injected delay, and asserts a concurrent `delete_usage_type` call for the same `gts_id` blocks on exclusive-lock acquisition until the create releases its exclusive lock and commits — the delete's verify step then deterministically observes the new reference and returns `UsageTypeReferenced`, proving the window is closed rather than merely narrowed under real concurrency; a symmetric test asserting a `create_usage_record` call blocks behind an in-flight `delete_usage_type`'s exclusive lock and, once the delete's row removal commits and releases the lock, deterministically observes the row's absence and returns `UsageTypeNotFound`; a cluster-unavailability test asserting both `create_usage_record` and `delete_usage_type` return `Transient` (never proceed unlocked) when the lock manager cannot reach the cluster backend within its timeout; a test confirming the plugin's own pre-insert catalog check rejects a reference to an already-deleted type; and a test confirming `ensure_still_held` failure (simulated via the `LockGuardPort` stub) aborts the delete with `Transient` before the `DELETE FROM` is issued.

## 5. Traceability

- **PRD**: [PRD.md](./PRD.md)
- **Decomposition**: [DECOMPOSITION.md](./DECOMPOSITION.md)
- **Parent Gear PRD/DESIGN**: [../../../docs/PRD.md](../../../docs/PRD.md), [../../../docs/DESIGN.md](../../../docs/DESIGN.md)
- **Plugin SPI reference**: [../../../docs/plugin-spi.md](../../../docs/plugin-spi.md) (Method 1, Method 3, Method 5, Method 9 are load-bearing for this design's dedup, aggregation-cap, deactivation, and FK-emulation sections respectively)
- **Reference plugin (structural template)**: [../../timescaledb-usage-collector-plugin/docs/DESIGN.md](../../timescaledb-usage-collector-plugin/docs/DESIGN.md)
- **ADRs**: [`0002-pluggable-storage`](../../../docs/ADR/0002-pluggable-storage.md), [`0012-unified-plugin-catalog-and-gts-id-reference`](../../../docs/ADR/0012-unified-plugin-catalog-and-gts-id-reference.md), [`0013-deterministic-usage-record-id`](../../../docs/ADR/0013-deterministic-usage-record-id.md), [`0014-created-at-in-dedup-identity`](../../../docs/ADR/0014-created-at-in-dedup-identity.md)
