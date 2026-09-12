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

This plugin is a second, independent realization of the Usage Collector's storage SPI (`UsageCollectorPluginV1`), targeting ClickHouse — a columnar OLAP database — instead of the reference plugin's row-oriented PostgreSQL/TimescaleDB. It is chosen for deployments that weight aggregation-query throughput and storage efficiency over ClickHouse's absence of transactions, row locks, and native foreign keys. The architecture's central engineering problem, and the subject of most of this document, is: **how does a plugin honor the SPI's dedup, deactivation, and referential-integrity contracts on a backend with no ACID transactions and no in-place row update?** The answer is `ReplacingMergeTree(version)` versioned rows for dedup/deactivation — point reads resolve the latest version in SQL (`ORDER BY version DESC LIMIT 1 BY <sort key>`), while the range reads (`list`, `aggregate`) scan raw rows and anti-join the ids that carry a deactivation marker, with duplicate creates prevented at the engine by `insert_deduplication_token` rather than collapsed at read time — and, for referential integrity, a two-sided application-level emulation: a plugin-owned pre-insert catalog check mirrors the structural (not business-logic) role Postgres's native FK plays on the create side, and `delete_usage_type` pairs a capped pre-delete reference probe with a post-delete orphan sweep on the delete side. The delete side is the one place this backend knowingly falls short of [`plugin-spi.md` Method 9](../../../docs/plugin-spi.md#method-9--delete-usage-type): without a coordination primitive the probe cannot be ordered against a concurrent insert, so a narrow orphaning window stays open and is documented as a deviation rather than papered over ([§3.6](#36-interactions--sequences)). No external lock is used anywhere: every write path is a plain read-then-insert whose concurrency semantics are stated explicitly, per race, in [§3.6](#36-interactions--sequences).

### 1.2 Architecture Drivers / NFR Allocation

| Driver | Allocation |
| --- | --- |
| `cpt-cf-usage-collector-nfr-query-latency` (≤500ms p95, 30-day single-tenant aggregation) | ClickHouse's columnar storage and vectorized `GROUP BY`/aggregate execution ([§3.6](#36-interactions--sequences) aggregate sequence); an `ORDER BY (gts_id, tenant_id, created_at, id)` primary key on `usage_records` for time-range scan locality, with the version-invariant half of the caller `$filter` pushed into the scan (and repeated inside the marker anti-join subquery) so `tenant_id` and `created_at` actually prune it, and no version-resolving `GROUP BY` or sort over the scanned rows; the SPI's `MAX_AGGREGATION_BUCKETS` cap enforced server-side via `LIMIT`. |
| `cpt-cf-usage-collector-nfr-throughput` (≥10,000 records/sec) | Batch writes as exactly **three statements per batch** regardless of how many usage types it spans: one catalog existence `IN` query, one dedup pre-read `IN` query, one multi-row `INSERT` (ClickHouse's write path strongly favors large parts over many small ones); no per-row round-trip and no per-type fan-out. **No coordination ceiling**: there is no lock, so creates never queue behind one another; concurrent creates for one `gts_id` run fully in parallel and converge through `ReplacingMergeTree(version)` ([§3.6](#36-interactions--sequences)). The envelope is met by batching; ClickHouse's own write throughput is the only bound. **Shared-pool exposure**: the same client/pool serves the query path, so query bursts can consume capacity this NFR depends on (see the workload-isolation row). |
| `cpt-cf-usage-collector-nfr-workload-isolation` | Documented as a **known limitation** rather than a solved allocation: the `sea-clickhouse` client (soft fork of `clickhouse-rs`) exposes one client over one internal connection pool, and that pool's sizing is **not** operator-tunable — the crate offers no pool-bound builder at all ([§3.5](#35-external-dependencies)), so there is no config field to expose. This plugin configures one such client for the request path. Consequence for the throughput NFR above: a burst of aggregation/list queries competes with ingest for the same pool and the same ClickHouse server, and nothing in the plugin reserves capacity for or prioritizes ingest — accepted for v1, mitigated **operationally** (server-side quotas/settings profiles, separate plugin instances) rather than by configuration, with monitoring and mitigation guidance in the plugin README. A future revision MAY split ingestion and query onto separate clients if contention is observed in practice (see [§3.5](#35-external-dependencies)). |
| `cpt-cf-usage-collector-adr-0012` (FK-equivalent referential integrity) | Create side: the plugin-owned pre-insert catalog check ([§3.6](#36-interactions--sequences)) rejects a reference to an unregistered type with `UsageTypeNotFound`. Delete side: emulated — `delete_usage_type` refuses a referenced type with `UsageTypeReferenced` after a capped probe, then sweeps records that landed inside its own probe→delete window ([§3.6](#36-interactions--sequences)). The window `ON DELETE RESTRICT` closes natively in Postgres is narrowed here, not closed; the residual is recorded as a deviation in PRD.md §5. |
| `cpt-cf-usage-collector-fr-idempotency` | `ReplacingMergeTree(version)` + read-before-insert check ([§3.6](#36-interactions--sequences)). The check is best-effort: two concurrent creates for one dedup key can both pass it and both write; the engine then drops the second block when both carry the same `insert_deduplication_token` (synchronous inserts, **first-writer-wins**, both callers `Ok`), and `ReplacingMergeTree(version)` collapses anything it missed at merge. `IdempotencyConflict` is raised whenever the earlier row is already visible at pre-read time — the documented deviation from the reference plugin's `UNIQUE`-backed guarantee. |

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

**Honest degradation over silent narrowing**: every place this backend cannot match the reference plugin's DB-enforced guarantee is implemented as the closest achievable approximation with the residual explicitly documented as a deviation (dedup atomicity is last-writer-wins inside the concurrent window; read-after-write holds per node, not across replicas; delete-side referential integrity narrows the orphaning window to the probe→delete span instead of closing it, [§3.6](#36-interactions--sequences)) — and the deviation is stated wherever the guarantee is claimed, never only in a footnote — never silently presented as equivalent to what a non-transactional backend cannot actually provide.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-principle-one-mechanism-two-problems`

**One write mechanism, two problems**: dedup convergence and deactivation-without-`UPDATE` both reduce to "insert a new versioned row sharing the `ReplacingMergeTree` sorting key" — this is a deliberate simplification so the plugin has one write-side mechanism to reason about, test, and instrument. On the read side the two are handled differently by design: point reads resolve the latest version explicitly, range reads anti-join the deactivation markers and rely on engine-side insert deduplication for duplicate creates ([§3.8](#38-consistency--concurrency)).

### 2.2 Constraints

- **No multi-statement ACID transactions.** Every sequence in [§3.6](#36-interactions--sequences) that would be a single Postgres transaction in the reference plugin is instead composed of independently-committing ClickHouse statements; atomicity is claimed only where a single ClickHouse `INSERT` (which is atomic as one part write) suffices, and is explicitly disclaimed everywhere else (dedup, catalog create).
- **No row-level locks, and no external lock either.** `SELECT ... FOR UPDATE` has no ClickHouse equivalent, and this plugin deliberately uses no cluster-side mutex to stand in for it. Every write path is therefore a plain, unlocked read-then-write whose concurrent outcome is resolved by the engine's `insert_deduplication_token` window (a racing identical write is dropped; first-writer-wins) and, past that window, by `ReplacingMergeTree(version)` (highest version wins on merge and on point-read resolution). The one operation that could not be made safe under that model — deleting a usage type while records may be referencing it — is not offered.
- **No native `UNIQUE`/`FOREIGN KEY`/`ON CONFLICT`.** Uniqueness (dedup) is emulated at the application level with a best-effort pre-read; referential integrity is emulated on both sides at the application level — a catalog existence check on insert, a reference probe plus orphan sweep on delete — and the delete side narrows its race rather than closing it ([§3.6](#36-interactions--sequences)). ClickHouse's `ReplacingMergeTree` collapsing is the convergence mechanism for duplicate physical rows, not a substitute for either constraint.
- **No `sqlx` driver.** Schema provisioning is a hand-rolled, idempotent DDL-statement runner (embedded SQL with a fixed 1-year TTL default, no external migration-tracking framework) plus `ensure_retention_ttl` to reconcile `retention_period_secs` on every `init` — see [§3.2](#32-component-model).
- **`UPDATE` and `ALTER TABLE ... DELETE` are asynchronous background mutations in ClickHouse**, unsuitable for any request-path operation with a latency budget. No request-path code path issues `ALTER TABLE ... UPDATE`, `ALTER TABLE ... DELETE`, or any `DELETE` at all; every `usage_records` status transition is a new `INSERT`, and `usage_type_catalog` rows are never removed by the plugin ([§3.6](#36-interactions--sequences) Delete Usage Type). No tombstone flag or higher-version marker row is needed to represent "deleted" for the catalog as a result.
- **No cluster dependency.** The plugin takes no cluster lock and declares no dependency on the `cluster` gear; its only gear dependency is `types-registry`. Cluster ADR-002 (no remote I/O while holding a cluster lock) is therefore satisfied trivially rather than deviated from.
- **Dependencies policy** (`guidelines/DEPENDENCIES.md`): any synchronous lock introduced by later phases uses `parking_lot::Mutex`/`RwLock`, not `std::sync`; any YAML use, if introduced, uses `serde-saphyr`.

**Constraint ID index** (used for cross-artifact traceability):

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-no-transactions`

No multi-statement ACID transactions; every multi-step sequence uses independently-committing ClickHouse statements.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-no-in-place-update`

No `UPDATE` or `ALTER TABLE ... DELETE` on the request path; every `usage_records` status transition is a new versioned `INSERT`.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-vendor-isolation`

All ClickHouse-specific SQL, schema, and client dependencies are confined to this crate; no dependency on the host `usage-collector` crate.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-dedup-race-window`

The dedup pre-read is best-effort: two concurrent creates for the same dedup key can both pass it and both write. Every `usage_records` `INSERT` carries an `insert_deduplication_token` derived from its row ids (`record_store::insert_dedup_token`) and the table keeps a `non_replicated_deduplication_window` (reconciled at startup by `ensure_insert_dedup_window`), so on a synchronous insert the engine drops the second block — first-writer-wins, no `IdempotencyConflict`. The residual cases converge at merge through `ReplacingMergeTree(version)`: an asynchronous single-record insert whose twin landed in a different flush (async dedup is a `Replicated*` feature; twins in one flush collapse through `optimize_on_insert`), and two non-identical batches that overlap. Until that merge, `list` shows such a duplicate twice and `aggregate` counts it twice; point reads never do. `IdempotencyConflict` is raised only when the earlier row is already visible at pre-read time.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-final-qualified-reads`

No read uses the `FINAL` modifier, and the range reads use no version resolution at all. Point reads whose candidate set is already tiny — `get`, the deactivation cascade's own read, the create-path dedup lookups, the catalog — resolve `ReplacingMergeTree` versions as ordinary SQL, `ORDER BY version DESC LIMIT 1 BY <sort key>`. `list` and `aggregate` instead scan raw rows and anti-join the deactivation markers: `id NOT IN (SELECT id FROM usage_records WHERE <same scan predicate> AND status = 'inactive')`. A marker keeps its source row's `id` and `status` never transitions back, so a logical row is inactive exactly when some physical row with its `id` is — the anti-join is exact before any merge, costs one hash probe per row against a set that is tiny or empty, and needs neither `version` nor a `GROUP BY`/sort over the scanned rows (which `FINAL`, `LIMIT 1 BY` and an `argMax`-grouped rewrite all did, and which was the dominant query cost).

Because a deactivation marker differs from the row it supersedes only in `status` and `version`, one rule governs the point reads that still resolve: a predicate on `status` **MUST NOT** be applied to the scan below the resolution step, and every other predicate **MAY** be (where it still prunes the scan). Applying a `status` predicate below the resolution step would retain a superseded active row while discarding that row's own marker, so the resolution would then return the stale row — a deactivated record read back as active. The deactivation cascade applies its `status = 'active'` above the step for exactly this reason.

On the range reads the same fact is used the other way round: after the marker anti-join, every surviving physical row's raw `status` **equals** its resolved status (a marker survives as the resolved inactive row in `list`; an unmarked active row survives as itself; the superseded active twin is the one thing dropped). The caller's `status`-naming `$filter` conjuncts, the keyset predicate and the `ORDER BY` therefore read the raw column in the one and only `WHERE`, and the aggregate's own `status = 'active'` is simply the first half of its survivor predicate (`dedup::active_survivors`; `list` uses `dedup::resolved_survivors`). `status` is both `$filter`-able and keyset-safe, so this is reachable from caller input; see `infra/storage/query/dedup.rs`.

The version-invariant/`status`-naming split is still exercised: `list` and the aggregate split the caller `$filter` on its top-level `AND` spine (`dedup::split_version_invariant`), render the version-invariant conjuncts as the scan predicate **and** repeat them verbatim inside the marker subquery (so both prune on the same key range, and the scan binds are applied twice), and append the `status`-naming conjuncts after the survivor predicate. This is load-bearing for the query-latency NFR, because `tenant_id` and `created_at` reach the plugin *only* through `$filter` — the gateway expresses the `[from, to)` window as `created_at ge … and created_at lt …` — and with `gts_id` they are the whole sorting-key prefix. A subtree that is not a conjunct of that spine (an `OR` or `NOT` naming `status` beside other columns) cannot be split and trails whole; a `status`-naming conjunct must never enter the marker subquery, which carries its own `status = 'inactive'`.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-aggregation-bucket-cap`

The plugin caps aggregation output to `MAX_AGGREGATION_BUCKETS + 1` (100,001) rows server-side via `LIMIT` on every aggregation query.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-gts-lock-required`

**Superseded.** No coordination lock exists in this plugin. The ID is retained so cross-artifact references stay resolvable; the constraint it once stated (an exclusive per-`gts_id` lock on every create-record and delete-usage-type path) no longer applies — creates are unlocked, and `delete_usage_type` is offered with its probe→delete race documented rather than serialized ([§3.6](#36-interactions--sequences)).

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-constraint-retention`

The `usage_records` TTL clause is provisioned with a fixed 1-year default in `CREATE TABLE IF NOT EXISTS`, then reconciled on every `init` by `ensure_retention_ttl`: if the live TTL interval differs from `retention_period_secs`, the plugin issues `ALTER TABLE … MODIFY TTL`.

## 3. Technical Architecture

### 3.1 Domain Model

Reuses the SDK's domain types verbatim (`UsageRecord`, `UsageType`, `UsageKind`, `AggregationSpec`, `AggregationResult`, `MetadataFilter`, `UsageCollectorPluginError`) — this plugin introduces no new domain types at the SPI boundary. Infra-internal types (entity rows, the deactivation-marker shape) are plugin-local and never cross the SPI.

### 3.2 Component Model

| Component | Responsibility |
| --- | --- |
| Plugin Module (`gear.rs`) | `init()`: load/validate config, build the ClickHouse client (via `build_client` — parses `database_url` into a bare base URL plus separate user/password/database, applied via `with_user`/`with_password`/`with_database` so the `clickhouse::Client` URL path and userinfo are never mixed), run initial schema provisioning (`apply_migrations`), reconcile `usage_records` TTL with `retention_period_secs` (`ensure_retention_ttl`), build the two stores, perform the four-step GTS/`types-registry`/`ClientHub` registration handshake. |
| Schema Migration (`infra/storage/pool.rs`, `migrations/0001_init.sql`) | A single embedded SQL file executed as a sequence of idempotent `CREATE TABLE IF NOT EXISTS` DDL statements at `init`. The `usage_records` DDL bakes a fixed 1-year TTL default (`INTERVAL 31536000 SECOND`). After migration, `ensure_retention_ttl` reads `system.tables.create_table_query`, compares the live TTL interval to `retention_period_secs`, and issues `ALTER TABLE usage_records MODIFY TTL …` when they differ. Comment lines (`--`) are stripped before statement splitting to avoid false statement boundaries from semicolons in prose comments; statements are split while respecting single-quoted string literals. No `schema_migrations` tracking table, no external framework. |
| SPI Storage Adapter (`domain/adapter.rs`) | The sole `UsageCollectorPluginV1` implementation; pure delegation to `RecordStore`/`CatalogStore`, identical in shape to the reference plugin's adapter. |
| Coordination Lock Manager — **retired** | Formerly `infra/coordination/lock_manager.rs`. Removed together with the cluster dependency; nothing in the plugin coordinates writers. The component IDs below are kept only so cross-artifact references stay resolvable. |
| Record Store (`infra/storage/record_store.rs`) | `usage_records` CRUD/query: insert with a best-effort dedup pre-read and the plugin-owned catalog-existence check, whole-batch insert (one catalog `IN` query, one dedup `IN` query, one multi-row `INSERT`), get, keyset list, pushed-down aggregation, and the versioned-marker deactivation cascade. |
| Catalog Store (`infra/storage/catalog_store.rs`) | `usage_type_catalog` create / get / list / delete: create (with pre-existence check and idempotency absorb for identical payloads — same `kind`+`metadata_fields` → silent absorb; different `kind`/`metadata_fields` → `UsageTypeAlreadyExists`), get, list (keyset-paginated, fixed `gts_id ASC` order, forward-only cursor). `delete` runs an existence read, a capped reference probe, an `ALTER TABLE … DELETE` of the catalog row under `mutations_sync = 1`, then a re-probe-gated sweep of orphaned records. Spawns a single background catalog-size refresh worker that coalesces mutation-triggered `count()` refreshes via a `tokio::sync::Notify` signal, racing each count against the gear cancellation token for prompt shutdown. |
| Query Translator (`infra/storage/query/*`) | OData `$filter`/`$orderby`/keyset-cursor → parameterized ClickHouse SQL, reusing the reference plugin's allowlisted-identifier approach: every identifier comes from a closed allowlist (`record_column` / `usage_type_column`) and every value is a positional `?` bind, so no query builder is needed and none is used — the stores assemble the statements with `format!` over `'static` fragments. Version-resolution fragments (point reads) and the marker anti-join fragments (range reads) live in `query/dedup.rs`. Forward-only cursor enforcement (`ensure_forward_cursor`) is a v1 constraint; backward paging is not implemented. |
| Metrics (`infra/metrics.rs`) | `uc_clickhouse_*` OpenTelemetry instrument inventory (see [§4](#4-additional-context) Observability). |

**Component ID index** (used for cross-artifact traceability):

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-module`

Plugin Module (`gear.rs`): `init()` lifecycle, config, ClickHouse client, schema migration, store wiring, GTS/ClientHub registration.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-adapter`

SPI Storage Adapter (`domain/adapter.rs`): sole `UsageCollectorPluginV1` implementation, pure delegation, error classification, cursor encoding.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-migrations`

Schema Migration (`migrations/0001_init.sql` + runner): idempotent DDL runner with fixed 1-year TTL default; `ensure_retention_ttl` reconciles config on startup.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-lock-manager`

**Retired.** Coordination Lock Manager — removed with the cluster dependency; no such component exists.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-catalog-lock-port`

**Retired.** `CatalogLockPort` / `LockGuardPort` seam traits — removed with the lock manager; both stores are constructed from a client, metrics, and a deadline only.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-record-store`

Record Store (`infra/storage/record_store.rs`): `usage_records` CRUD, dedup, batch insert, keyset list, pushed-down aggregation, deactivation cascade.

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-component-catalog-store`

Catalog Store (`infra/storage/catalog_store.rs`): `usage_type_catalog` create / get / list, the unimplemented delete refusal, catalog-size background refresh.

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

**No coordination dependency.** The plugin uses no cluster lock and declares no dependency on the `cluster` gear. Any number of gear process instances may call it against the same ClickHouse backend; concurrent writers are reconciled by `ReplacingMergeTree(version)` rather than serialized, with the consequences enumerated per sequence in [§3.6](#36-interactions--sequences). An earlier revision of this design carried a per-`gts_id` exclusive lock over cluster `DistributedLockV1` to make `delete_usage_type` safe; that operation now documents its race instead of serializing it, which removed the lock's only justification along with the cluster dependency, two config knobs (`lock_ttl_secs`, `lock_timeout_secs`), and the `uc_clickhouse_lock_*` metric series.

**Workload isolation allocation**: the `sea-clickhouse` `Client` (crate path `clickhouse`) is a lightweight, cheaply-cloneable handle over an internal `hyper` connection pool, and this plugin configures one such `Client` shared by both the ingestion and query paths for v1 — the accepted, documented contention point, not a silently-assumed-solved allocation, per `cpt-cf-usage-collector-nfr-workload-isolation`.

**The pool is not operator-tunable, by constraint of the client crate.** `sea-clickhouse` 0.15.x exposes no pool-bound knob a config field could drive: `Client` has no pool builder method; `with_setting`/`with_option` pass ClickHouse **server** settings, not client pool settings; and the one seam that could carry a pre-built pooled connector, `Client::with_http_client`, is unusable from another crate because the `HttpClient` trait lives in a private module and its bound names the private `clickhouse::request_body::RequestBody` type. Even reaching it would buy little — `hyper_util`'s legacy client builder offers only `pool_idle_timeout` and `pool_max_idle_per_host`, neither of which bounds concurrent in-flight connections. There is therefore **no** `pool_max_connections`-style field in `ClickHousePluginConfig`, and this design does not promise one. Mitigation for the contention below is consequently operational (server-side settings profiles/quotas, or separate plugin instances), not configuration. If a production deployment observes query bursts degrading ingestion latency, a future revision can still split ingestion and query onto two `Client` instances against different endpoints (additive, non-breaking to the SPI). Operators experiencing contention in the interim MAY mitigate by running two plugin instances (distinct GTS priorities) against read-replica vs. write-primary ClickHouse endpoints — an operational workaround documented in the plugin README, not a code-level split.

**Risk this poses to the ingestion-throughput NFR.** Because ingest and query share one client, one pool, and one ClickHouse server, a query burst is a direct threat to `cpt-cf-usage-collector-nfr-throughput` ([§1.2](#12-architecture-drivers-nfr-allocation)): the write path can wait on pool acquisition behind long-running aggregations, and the plugin has no reservation, priority, or admission control that protects it. This is accepted for v1 rather than designed away, and the mitigation is operational: operators correlate `uc_clickhouse_pool_acquire_duration_seconds` with `uc_clickhouse_query_requests_total{query_kind="aggregated"}` and `uc_clickhouse_insert_duration_seconds{mode="batch"}` to attribute ingest slowdowns to read contention, bound read cost server-side with ClickHouse settings profiles/quotas, and — where hard separation is required — run the two-instance split above (see the plugin README's "Workload isolation and pool contention").

### 3.6 Interactions & Sequences

#### Ingest with Dedup (`cpt-cf-uc-ch-plugin-seq-ingest-dedup`)

**ID**: `cpt-cf-uc-ch-plugin-seq-ingest-dedup`

1. Compute the record's deterministic `id` (ADR-0013/ADR-0014 4-tuple derivation — inherited unchanged from the reference plugin's identity scheme).
2. **Plugin-owned referential-integrity check** (mirrors the reference plugin's reliance on a DB-native FK firing at insert time — see `cpt-cf-uc-ch-plugin-principle-spi-conformance`, [§2.1](#21-design-principles)): `SELECT gts_id FROM usage_type_catalog WHERE gts_id = ? LIMIT 1` (existence only — invariant across versions, so no resolution is needed). Absent → return `UsageTypeNotFound { gts_id }` (mirrors the reference plugin's FK-violation mapping in `record_store.rs::map_insert_error`). A type seen here is **not** guaranteed to still exist when step 4's `INSERT` commits: nothing orders this read against a concurrent [Delete Usage Type](#delete-usage-type--probe-delete-sweep-cpt-cf-uc-ch-plugin-seq-delete-type-fk). That is the deviation from [`plugin-spi.md`](../../../docs/plugin-spi.md#method-9--delete-usage-type) Method 9's "MUST NOT admit a window", and it is accepted rather than closed. This check is nonetheless what *bounds* the window: once the delete has removed the catalog row, every subsequent insert for the `gts_id` is refused here, so the delete's own post-delete sweep only has to cover the records that landed inside its probe→delete span.
3. `SELECT ... WHERE tenant_id = ? AND gts_id = ? AND created_at = ? AND idempotency_key = ? ORDER BY id ASC, version DESC LIMIT 1 BY id` — a lookup against the `usage_records` `ReplacingMergeTree` table on the SPI's canonical dedup tuple. The three leading columns are the `ORDER BY` prefix, so the read resolves to a primary-key point rather than a filtered scan, and `idempotency_key` applies as a residual filter (see [§3.7](#37-database-schema--tables)). It is deliberately **not** keyed on `id`: `id` is a projection of this same tuple, so an `id`-keyed lookup would miss a stored row whose `id` disagrees with its own tuple and re-insert it under an idempotency key already in use.
4. **Not found**: `INSERT` one row with `status = 'active'`, `version = <monotonic>` (ingestion-time epoch microseconds). Return the new record.
5. **Found, canonical fields equal (and `status = 'active'`)**: silent absorb — return the stored row without inserting.
6. **Found, canonical fields differ, or stored row inactive**: return `IdempotencyConflict`.
7. **Concurrent-window semantics (explicit, not residual)**: nothing serializes two creates for the same dedup key. Both may execute step 3 before either's `INSERT` is visible, and both then insert. Because `id` is a projection of the dedup tuple, both `INSERT`s carry the same `insert_deduplication_token` (`insert_dedup_token`: UUIDv5 of the sorted row ids), so on a synchronous insert the engine drops the second block outright; with `async_insert` on (the default) the token is carried but only `Replicated*` engines enforce it for async inserts, so the twins collapse through `optimize_on_insert` when they land in one flush and otherwise share the sort key `(gts_id, tenant_id, created_at, id)` until `ReplacingMergeTree(version)` collapses them at merge (visible twice to `list`/`aggregate` in between; `get` resolves either way). Consequences: (a) identical payloads converge to one visible row and both callers get `Ok` — benign; (b) **differing payloads converge first-writer-wins when the engine drops the block, last-writer-wins when the merge collapses them, and both callers still get `Ok`** — no `IdempotencyConflict` is raised inside the window, and one caller believes a payload is stored that is not; (c) a retry of a record whose deactivation marker is concurrently being written, and whose pre-read missed the original row, can re-insert the record as `active` with a version above the marker's — the "resurrection" case, which the host's Method 5 rule does not cover because it protects compensations, not retries. Outside the window — once the earlier row is visible at pre-read — steps 5-6 apply and the reference plugin's outcomes are reproduced. On a replicated ClickHouse deployment the window also spans replication lag ([§3.8](#38-consistency--concurrency)).

#### Batch Ingest (`cpt-cf-uc-ch-plugin-seq-ingest-batch`)

**ID**: `cpt-cf-uc-ch-plugin-seq-ingest-batch`

Exactly three statements per batch, however many usage types it spans; no partitioning, no per-type fan-out.

1. **One catalog existence query** over the batch's distinct `gts_id`s: `SELECT gts_id FROM usage_type_catalog WHERE gts_id IN (?, ?, …)` (an existence read: no version resolution, since existence is invariant across versions and duplicate copies of one type collapse in the caller's `HashSet`) (one bound parameter per distinct id, in sorted order). Every record whose `gts_id` is absent from the result gets `UsageTypeNotFound` at its own input position; the rest pass. A failed read is written to every slot — it decided nothing for anyone.
2. **One batched dedup pre-read** over every record that passed — a single `SELECT ... WHERE (gts_id, tenant_id, created_at, idempotency_key) IN (...) ORDER BY version DESC LIMIT 1 BY gts_id, tenant_id, created_at, id`. A failed read is written to every passed slot; the `UsageTypeNotFound` outcomes from step 1 stand.
3. **Resolve outcomes in input order** — existing row with equal canonical fields and `active` → silent absorb; existing row with differing fields or `inactive` → `IdempotencyConflict`; a second row for the same dedup key inside this same batch is absorbed onto its twin's composed row only if canonically identical, otherwise it is likewise a conflict; anything else composes a new row with `version = <batch base epoch µs> + <rows composed so far>`, so the batch's versions are distinct and increasing. Within-batch dedup keys on the full canonical tuple (which contains `gts_id`), so records of different types can never collide.
4. **One multi-row `INSERT` of the composed rows** (skipped when nothing was composed). `usage_records` has no `PARTITION BY` and the SPI caps a batch at 1,000 rows (far below `max_insert_block_size`), so the statement is one part write: a reader sees all of the batch's new rows or none of them. **Whole-batch write atomicity therefore holds** for the composed rows; the SPI contract nonetheless remains per-record outcomes, because absorbed and rejected records are decided by the reads, not the write.
5. **Return one outcome per input record in input order**, mirroring the reference plugin's positionally-aligned batch contract. A failed `INSERT` is reported per slot backed by a composed row rather than as a top-level error, so outcomes already decided for absorbed rows survive.

The concurrent-window semantics of the single-record sequence (step 7 above) apply unchanged to each composed row.

**Multi-`gts_id` batches.** The SPI does not restrict a batch to a single `gts_id` — `plugin-spi.md` Method 2 places no such constraint on `create_usage_records`, and the host resolves each record's `gts_id` independently (see `usage-emission.md`'s `catalog-existence-and-kind-lookup`). Step 1's `IN` query covers every distinct type in one round-trip and attributes a referential-integrity rejection per `gts_id` rather than to the whole batch; an earlier revision that checked only `records[0].gts_id` would have let every non-first type bypass its own check.

#### Deactivation Cascade (`cpt-cf-uc-ch-plugin-seq-deactivate-cascade`)

**ID**: `cpt-cf-uc-ch-plugin-seq-deactivate-cascade`

1. `SELECT ... FROM (SELECT ... WHERE id = ? OR corrects_id = ? ORDER BY version DESC LIMIT 1 BY gts_id, tenant_id, created_at, id) WHERE id = ? OR (corrects_id = ? AND status = 'active')` — the `status` half of the predicate has to sit above the resolution step, so `id` is bound four times — resolve the target's current status plus every active depth-1 compensation referencing it, in one unlocked read.
2. Target not found → `UsageRecordNotFound`. Target already `inactive` → `UsageRecordAlreadyInactive`.
3. Otherwise, compose one versioned marker row per affected `id` (target + compensations), each carrying `status = 'inactive'` and a `version` strictly higher than the row it supersedes.
4. Issue **one** multi-row `INSERT` for all marker rows, carrying a marker-namespaced `insert_deduplication_token` (`InsertKind::Marker`): a marker shares its source row's `id`, so without the namespace the engine would drop the cascade as a retry of the create that wrote those ids. A single ClickHouse `INSERT` is applied as one atomic part write, so a reader either sees the pre-cascade state (insert not yet visible) or the fully-flipped state (insert visible) — never a partial cascade.
5. **No late-compensation race exists here** (unlike an earlier draft of this design, which incorrectly hedged on one): per `plugin-spi.md` Method 5's caller-side concurrency rule, a compensation referencing the target while it is being deactivated is rejected by the gateway's own L1 "MUST be active" check **before** `create_usage_record` is ever dispatched for that compensation — the plugin never coordinates with an in-flight cascade because the host structurally prevents the racing write from reaching the SPI at all. The plugin's only genuine atomicity property to state here is the one already established: the cascade's flip is atomic as a single `INSERT` (step 4), so no reader ever observes a partial cascade.

#### Aggregated Query (`cpt-cf-uc-ch-plugin-seq-query-aggregated`)

**ID**: `cpt-cf-uc-ch-plugin-seq-query-aggregated`

Pushed down as a single-level scan over raw rows:

```sql
SELECT <dims>, <AGG> FROM usage_records
WHERE <scan>
  AND status = 'active'
  AND id NOT IN (SELECT id FROM usage_records WHERE <scan> AND status = 'inactive')
  [AND <status-dependent filter conjuncts>]
[GROUP BY ...] LIMIT {MAX_AGGREGATION_BUCKETS + 1}

-- <scan> = gts_id = ? [AND corrects_id IS NULL] [AND <metadata>]
--          [AND <version-invariant filter conjuncts>] [AND <subject guards>]
```

There is no version-resolving level. The survivor predicate (`dedup::active_survivors`) keeps exactly one physical row per active logical row: raw `status = 'active'` drops the deactivation markers themselves, and the `NOT IN` anti-join drops the active rows those markers superseded (a marker keeps its source row's `id`; `status` never transitions back). The marker set is tiny or empty, so the anti-join is one hash probe per scanned row, and the only `GROUP BY` in the text is the caller's dimension grouping — where the previous shape hash-grouped every scanned row on the full sort key with `any()` over seven columns and `argMax(status, version)`, and the shape before that sorted them under `FINAL`. `EXPLAIN PIPELINE` on ClickHouse 25.6 shows `CreatingSets → Limit → Aggregating ×1 → ReadFromMergeTree` with primary-key pruning on `(gts_id, tenant_id, created_at)` intact. The caller `$filter` is still split by `dedup::split_version_invariant`: the version-invariant conjuncts form `<scan>`, which is rendered twice — as the scan predicate and inside the marker subquery, so both prune on the same key range and the store binds `gts_id` and the scan binds twice — and the `status`-naming conjuncts trail the survivor predicate, where they read the raw `status` that on every survivor equals the resolved one. That split is what makes a tenant-scoped time-window aggregate a sorting-key range scan: `tenant_id` and `created_at` arrive solely through `$filter`. Because the query is one level, every alias is `d<i>`/`agg` and cannot shadow a filtered column — the alias-shadowing constraint that forced the old shape to nest a third `SELECT` is gone with it. What the aggregate no longer collapses is a duplicate *create* the engine did not catch (an async single-record insert whose twin landed in a different flush, or two non-identical overlapping batches): it is counted twice until the merge; see [§2.2](#22-constraints) `constraint-dedup-race-window`.

The query honors the same `SUM`-nets-compensations vs. other-ops-exclude-compensations partition rule as the reference plugin (`corrects_id IS NULL` for non-`SUM` ops). Per `plugin-spi.md` Method 3's pushdown obligation, the plugin **MUST** cap its own grouped result to `MAX_AGGREGATION_BUCKETS + 1` buckets (100,001) via the `LIMIT`, letting the gateway distinguish a result exactly at the cap from one over it before it applies the `400 AGGREGATION_RESULT_TOO_LARGE` rejection — the plugin never materializes an unbounded bucket set even transiently. The marker anti-join is what keeps a not-yet-merged pre-deactivation row from being counted; engine-side insert deduplication is what keeps a not-yet-merged duplicate create from being double-counted; see [§3.8](#38-consistency--concurrency).

#### Keyset List (`cpt-cf-uc-ch-plugin-seq-list-keyset`)

**ID**: `cpt-cf-uc-ch-plugin-seq-list-keyset`

```sql
SELECT <cols> FROM usage_records
WHERE <scan>
  AND (status = 'inactive'
       OR id NOT IN (SELECT id FROM usage_records WHERE <scan> AND status = 'inactive'))
  [AND <status-dependent filter conjuncts>] [AND <keyset>]
ORDER BY <order> LIMIT <n+1>

-- <scan> = gts_id = ? [AND <metadata>] [AND <version-invariant filter conjuncts>]
```

This reuses the reference plugin's look-ahead-row-then-truncate keyset pagination pattern, translated to ClickHouse's parameter-binding syntax. The survivor predicate (`dedup::resolved_survivors`) differs from the aggregate's because `list` returns inactive rows too: a marker survives on its own — its raw `status` *is* the resolved status and it carries every other column of the row it marks — and an active row survives iff no marker exists for its id, so exactly one physical row per logical row remains, at its resolved status, before any merge. There is no version sort and no `LIMIT 1 BY`; the caller's `ORDER BY … LIMIT <n+1>` is the only sort, bounded by the limit, and streams in sort-key order when `<order>` is a key prefix (`EXPLAIN PIPELINE`: `CreatingSets → Limit → Sorting ×1 → ReadFromMergeTree`). The keyset predicate and the `status`-naming `$filter` conjuncts trail the survivor predicate and read the raw `status` column, which on every survivor equals the resolved value ([§2.2](#22-constraints)); the version-invariant `$filter` conjuncts and the metadata side-channel form `<scan>`, rendered twice so the marker subquery prunes on the same key range. The look-ahead `LIMIT` applies after every predicate, so a filtered list is never short-paged. `list` accumulates scan and trailing binds in separate `SqlCtx`s and applies the scan binds twice, then the trailing ones. The wire-level cap (≤ 1,000 records) bounds `<n>`, independent of the aggregation bucket cap above. **The absence of a `status = 'active'` predicate here is deliberate, not an omission relative to the aggregated query above**: `plugin-spi.md` Method 4 is status-agnostic by contract, because Method 5 does not return the set of cascade-flipped ids and instead directs operators to enumerate it via a follow-up `list_usage_records` filtered on `status` / `corrects_id` — a predicate baked in here would make that enumeration impossible. A caller wanting active-only rows passes `status` in its own `$filter`. The reference plugin's list path behaves identically. The keyset predicate itself is a **strict** row-value comparison (`>` ascending, `<` descending), led by the non-strict bound it implies on the first ordering column (`created_at >= ? AND (created_at, id) > (?, ?)`): ClickHouse's key analysis derives no range from a tuple comparison, so the redundant bound is what lets a deep page prune the granules before the cursor instead of rescanning the window from its start; see `query/keyset.rs`. Relatedly, the translator renders a one-element `IN` as an equality (`tenant_id = ?`, not `tenant_id IN (?)`): the gateway's authorization scope arrives as a one-tenant `in`, and only an equality lets ClickHouse treat the sorting-key prefix as *fixed*, which is what makes this read stream in key order and stop at `LIMIT <n+1>` (`MergeTreeSelect(algorithm: InOrder)`) rather than sort the whole tenant — measured at ~11x fewer rows read on a paged list. **`<order>` MUST be a total order**: it has to end in a stable globally-unique tie-breaker — `id` on this path, `gts_id` on the type-catalog list — and the cursor's key tuple spans that same full order. The host supplies it: per `plugin-spi.md` Method 4 the gateway normalizes every caller `$orderby` to end in the canonical `(created_at, id)` suffix (in the caller's sort direction) before dispatch, so `ORDER BY <order>` and the keyset predicate always agree on a unique row position. Without that tie-breaker the strict comparison skips every row that ties the last returned row on the non-unique key and did not fit on the page — silent data loss across the page boundary, not merely an unstable ordering.

#### Create Usage Type (`cpt-cf-uc-ch-plugin-seq-create-type`)

**ID**: `cpt-cf-uc-ch-plugin-seq-create-type`

1. `SELECT ... WHERE gts_id = ? ORDER BY version DESC LIMIT 1` pre-existence check, then:
   - **Already exists, identical payload** (`kind` and `metadata_fields` equal): silent absorb — return the stored type without inserting.
   - **Already exists, different payload** (`kind` or `metadata_fields` differ): `UsageTypeAlreadyExists`.
   - **Absent**: `INSERT` with `version = current_version()` (epoch microseconds) and signal the background catalog-size refresh worker.
2. **Concurrent-window semantics (explicit)**: nothing serializes two creates for the same `gts_id`. Both may pass step 1's read before either's `INSERT` is visible, and both then insert; `ReplacingMergeTree(version)` keeps the higher `version` on merge and on read-time resolution, so **the catalog is last-writer-wins inside that window and both callers get `Ok` even when their payloads differ** — `UsageTypeAlreadyExists` is not raised for the loser. Once the winner's row is visible, a later create sees it and returns an absorb or `UsageTypeAlreadyExists`. A re-create after a `delete_usage_type` has no earlier row to outrank either — the delete removes the physical rows rather than leaving a tombstone — so the version scheme only has to order racing inserts against one another.

#### Delete Usage Type — Probe, Delete, Sweep (`cpt-cf-uc-ch-plugin-seq-delete-type-fk`)

**ID**: `cpt-cf-uc-ch-plugin-seq-delete-type-fk`

`delete_usage_type` emulates the reference plugin's `ON DELETE RESTRICT` without a foreign key and without a mutual-exclusion primitive. Four steps, each bounded by the client-side deadline:

1. **Existence read** — `SELECT gts_id FROM usage_type_catalog WHERE gts_id = ? LIMIT 1`. Absent → `UsageTypeNotFound` (HTTP 404), so the gateway can distinguish "already gone" from "deleted now". No version resolution: `gts_id` is the whole sort key and no copy is a tombstone, so existence is version-invariant.
2. **Capped reference probe** — `SELECT count() FROM (SELECT 1 FROM usage_records WHERE gts_id = ? LIMIT 1000)`. Any row → `UsageTypeReferenced { gts_id, sample_ref_count }` (HTTP 409) and the catalog row is left untouched; `uc_clickhouse_usage_type_referenced_total` is incremented. `gts_id` leads the `usage_records` sorting key, so this is a primary-key range read; the inner `LIMIT` (`REF_COUNT_CAP`) keeps `sample_ref_count` a bounded diagnostic rather than a full count. Rows of **every** `status` count, `inactive` deactivation markers included — that is what the reference plugin's FK counts, and a type whose only records are deactivated still has rows that removing it would orphan.
3. **Delete the catalog row** — `ALTER TABLE usage_type_catalog DELETE WHERE gts_id = ?` with `mutations_sync = 1`. A heavyweight mutation rather than a lightweight `DELETE FROM`: it removes the physical rows, so a later `create_usage_type` for the same `gts_id` has no surviving copy to outrank (a marker-insert delete on a `ReplacingMergeTree` would). The table is unpartitioned and tiny, so the part rewrite is trivial. `mutations_sync = 1` is what makes the removal visible to the caller's next read.
4. **Orphan sweep** — re-run step 2's probe. Still non-zero → increment `uc_clickhouse_orphaned_reference_detected_total`, log at `warn`, and issue `ALTER TABLE usage_records DELETE WHERE gts_id = ?`. The re-probe *gates* the mutation, so the common path (nothing landed) issues no write against the large table at all. A failure in this step is logged at `error` and **not** propagated: the type is deleted, so reporting failure would misstate the outcome, and a retry could only ever answer `UsageTypeNotFound`.

**Step order is load-bearing.** The catalog row is removed *before* the sweep, not after. From the moment step 3 returns, the record store's insert-time catalog existence check refuses new records for the `gts_id` on its own — that check, not the sweep, is what keeps new orphans out. Sweeping first would leave a strictly wider window.

**Concurrent-window semantics (explicit).** This sequence does **not** satisfy `plugin-spi.md` Method 9's "MUST NOT admit a window" clause, which requires a transactionally serializable read-before-delete this backend cannot express. The residuals, all accepted:

- An insert whose own catalog check passed before step 3 can commit after step 4 and orphan a row. The window is the span between steps 2 and 3, not unbounded.
- With `async_insert` on (the plugin default) a record can sit in a server-side buffer past the sweep, widening that window from microseconds to the flush interval.
- Two concurrent deletes for one `gts_id` both pass step 1 and both mutate; the second is a no-op, so both return `Ok(())` and neither sees `UsageTypeNotFound`.
- `mutations_sync = 1` waits only for the receiving server. On a replicated deployment another gear process can briefly still resolve the type and accept a record for it. `mutations_sync = 2` (wait for all replicas) is deliberately not used: the shipped engine is non-replicated, so there is no replica to wait for, and paying for one on every request to narrow a window that is open anyway is not a trade this design makes.
- A step-3 or step-4 timeout abandons the client await while the server keeps applying the mutation, leaving the operation half-applied.

An earlier revision of this design closed the step-2/step-3 window with a per-`gts_id` exclusive cluster lock and withheld the operation entirely once that lock was removed ([§3.5](#35-external-dependencies)). The operation is now offered with the race documented rather than withheld: an append-only catalog left operators no way to remove a mis-registered type except hand-written SQL, which admits the same orphaning window with none of the guard rails. `uc_clickhouse_orphaned_reference_detected_total` is the signal that the window was actually hit; a non-zero rate means deletes are being run against types with live ingest, which is the operational rule this design asks for and cannot enforce. Consequence elsewhere: the catalog-size gauge is no longer monotone.

### 3.7 Database Schema & Tables

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-db-schema`

#### Table: usage_type_catalog

**ID**: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

| Column | Type | Description |
| --- | --- | --- |
| `gts_id` | `String` | GTS usage-type identifier; sorting-key column (the closest ClickHouse analog of a primary key). |
| `kind` | `Enum8('counter'=1,'gauge'=2)` | Counter or gauge, stored verbatim. |
| `metadata_fields` | `Array(String)` | Closed list of allowed metadata key names, stored verbatim. |
| `version` | `UInt64` | `ReplacingMergeTree` version column; resolves the create sequence's concurrent-insert race (two racing creates for the same `gts_id`) last-writer-wins on merge and on read-time resolution. |

**Sorting key (`ORDER BY`)**: `(gts_id)`. **Engine**: `ReplacingMergeTree(version)`. There is no native `PRIMARY KEY`/`UNIQUE` constraint; uniqueness-on-`gts_id` is an application-level invariant enforced best-effort by the create sequence's pre-existence check ([§3.6](#36-interactions--sequences)) and made eventual by version resolution, not a schema-level guarantee. `delete_usage_type` removes rows from this table with `ALTER TABLE … DELETE` ([§3.6](#36-interactions--sequences)) — a real mutation, so no tombstone-flag column or higher-version marker row exists and a re-create after a delete has no surviving copy to outrank.

**Constraints**: none native (no FK target support); referenced only conceptually by `usage_records.gts_id` — the reference is checked in application code at insert time only ([§3.6](#36-interactions--sequences)), which is sufficient because rows are never removed.

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
| `version` | `UInt64` | `ReplacingMergeTree` version column; a higher value wins on merge and on read-time resolution. |

**Sorting key (`ORDER BY`)**: `(gts_id, tenant_id, created_at, id)`. **Engine**: `ReplacingMergeTree(version)`. **TTL**: `created_at + INTERVAL <n> SECOND DELETE` on the `DateTime64(6)` column (no `toDateTime` cast — that would saturate at 2106). The migration DDL bakes a fixed 1-year default (`n = 31536000`). On every `init`, `ensure_retention_ttl` reads the live clause from `system.tables` and, when the parsed interval differs from configured `retention_period_secs`, TTL is missing, or the live clause still wraps `created_at` in `toDateTime`, issues `ALTER TABLE usage_records MODIFY TTL …` so the effective window tracks config across restarts.

**Data-skipping indexes**: `INDEX idx_records_id id TYPE bloom_filter GRANULARITY 1` and `INDEX idx_records_corrects_id corrects_id TYPE bloom_filter GRANULARITY 1`, both declared inside the `CREATE TABLE IF NOT EXISTS`. They exist because two request-path predicates do not lead with the sorting-key prefix and would otherwise read every granule as the table grows: `get_usage_record` (`WHERE id = ?`, where `id` is only the *trailing* sort-key column) and the deactivation cascade (`WHERE id = ? OR (corrects_id = ? AND status = 'active')`, where `corrects_id` is not in the sort key at all). Neither predicate is reachable by choosing a different sorting key, which is why they need indexes: skip indexes prune granules for reads that cannot use the key prefix at all, and never affect which rows resolve together. Note that *permuting* the sorting key is not an alteration of the dedup identity — `ReplacingMergeTree` collapses on the key as a set of columns, the `LIMIT 1 BY` resolution fragment (see `query/dedup.rs`) is order-insensitive, and the range reads' marker anti-join keys on `id` alone — so the column order is free to follow the request-path access pattern; only adding or removing a column would change which rows resolve together. Both indexes apply on their own now that reads no longer carry `FINAL`; the `use_skip_indexes_if_final` / `use_skip_indexes_if_final_exact_mode` settings the `get` and cascade reads used to attach existed only to make skip indexes usable *under* `FINAL`, and are gone with it. Because they live in the idempotent `CREATE TABLE`, a deployment provisioned before they were added does not acquire them on restart; that needs an explicit `ALTER TABLE usage_records ADD INDEX …` plus `MATERIALIZE INDEX` for pre-existing parts (operator procedure in the plugin README), which is a follow-up migration rather than startup-path work — unlike the TTL clause, which `ensure_retention_ttl` reconciles on every `init`. **Table settings**: `ttl_only_drop_parts = 1` (whole-partition TTL, above) and `non_replicated_deduplication_window = 10000`, the engine-side dedup window `insert_deduplication_token` is checked against ([§3.6](#36-interactions--sequences) ingest step 7). The window setting *is* retrofitted on startup: `ensure_insert_dedup_window` reads the live `create_table_query` and issues `ALTER TABLE usage_records MODIFY SETTING …` when the value differs, since `MODIFY SETTING` is metadata-only and safe to repeat. On a table an operator provisioned as `Replicated*` the setting is accepted and inert (`replicated_deduplication_window` governs, and async inserts are deduplicated too).

**Constraints**: none native (no `UNIQUE`, no `FOREIGN KEY`) — ClickHouse does not support either. Dedup identity and referential integrity are both emulated entirely in application code ([§3.6](#36-interactions--sequences)). The `(gts_id, tenant_id, created_at, id)` sorting key is chosen so that `create_usage_record`'s own dedup lookup — which supplies `gts_id`/`tenant_id`/`created_at` from the canonical dedup tuple — resolves against the three-column sort-key prefix rather than scanning; `idempotency_key`, the tuple's fourth component, is not in the sort key and applies as a residual filter over the handful of rows sharing that exact microsecond. The sort-key choice therefore optimizes both the dominant read pattern (type + tenant + time-range scans for aggregation/list) and the dedup lookup simultaneously.

`gts_id` leads rather than `tenant_id` because it is the one column every request-path read pins: it is a typed SPI parameter on `list_usage_records` and `query_aggregated_usage_records`, whereas `tenant_id` and `created_at` are optional `$filter` fields. Behind a leading high-cardinality `tenant_id`, a `gts_id = ?` read cannot use the primary index as a range and falls back to ClickHouse's generic exclusion search, which prunes effectively only when the *preceding* key column has low cardinality — against a UUID it read every granule, so every list and aggregate scanned the table. `gts_id` is also the low-cardinality column of the four, so leading with it compresses the primary index better. `ORDER BY` cannot be `ALTER`ed in place: a deployment provisioned under the earlier `(tenant_id, gts_id, created_at, id)` order keeps it until the table is rebuilt (`CREATE` new + `INSERT SELECT` + `EXCHANGE TABLES`), and both orders are correct — the earlier one is only slower.

Because the lookup keys on the canonical tuple and not on `id`, a stored row whose `id` disagrees with its own tuple — which no gateway dispatch can produce, since `id` is a derived projection, but which data corruption can — is surfaced by the canonical-field comparison as `IdempotencyConflict` rather than missed and re-inserted under an idempotency key already in use. ClickHouse cannot enforce uniqueness on the tuple, so when two such rows share one dedup key the lookup prefers the row whose `id` matches the incoming record and otherwise takes the lowest `id`, keeping the choice deterministic rather than dependent on part-read order.

**Additional info**: the exact DDL text (column defaults, `Enum8` value assignment, engine parameter syntax) is authored as literal SQL in the Phase 3 migration file (`migrations/0001_init.sql`), not reproduced verbatim here — this table is the schema's contract, not its implementation.

### 3.8 Consistency & Concurrency

The plugin's published consistency ceiling is **narrower than the reference plugin's** and MUST be stated with concrete, measurable bounds (`cpt-cf-uc-ch-plugin-nfr-consistency-profile`, PRD.md §6.1) rather than a vague "eventually consistent":

- **Single-node deployment: effectively immediate read-after-write for any reader.** A read observes any `INSERT` whose part has become locally visible, which on a single-node ClickHouse deployment happens synchronously with the `INSERT` call's return (typically sub-millisecond to low-single-digit milliseconds after acknowledgment). This is the default, recommended deployment topology for this plugin's v1 consistency claim.
- **Replicated deployment: bounded by ClickHouse replication lag, not by the plugin.** A read hitting a different replica than the one that served the write is bounded by ClickHouse's own replication lag (via ClickHouse Keeper), typically sub-second under healthy operation. **Measurement method and owner**: replication lag is measured with ClickHouse's own `system.replicas.absolute_delay`, which operators scrape from ClickHouse directly. It is **not** part of this plugin's metric surface — feature 0006 §1.5 lists ClickHouse server metrics as explicitly out of scope, since the lag is not this plugin's own state, so no plugin-emitted OTLP metric and no plugin-owned monitoring procedure exists for it. Operators running a replicated deployment MUST monitor it themselves and MUST configure ClickHouse's `insert_quorum` (a native ClickHouse write-quorum setting) if they require strict read-your-writes across replicas, at a documented write-latency cost proportional to the quorum size. This plugin's default configuration does not set `insert_quorum` (single-node/no-quorum default), and the plugin's own documentation MUST NOT claim a stronger cross-replica bound than "typically sub-second, operator-monitored" without quorum writes enabled.
- **Deactivation is exact at read time without a background-merge wait; duplicate creates are stopped at the engine.** Neither read path waits for or triggers an out-of-band merge, and none uses the `FINAL` modifier (which merges parts on the read path and was the plugin's dominant query cost).

  *Point reads* (`get`, the deactivation cascade's read, the create-path dedup lookups, the catalog) resolve `ReplacingMergeTree` versions as ordinary SQL — `ORDER BY version DESC LIMIT 1 BY <sort key>` — over a bloom-filter- or key-pruned candidate set that is already tiny. One rule constrains them: a deactivation marker differs from the row it supersedes only in `status` and `version`, so a predicate on `status` **MUST** be applied above the resolution step; below it, the predicate would retain a superseded active row while discarding that row's own marker, and the resolution would return the stale row.

  *Range reads* (`list`, `aggregate`) do not resolve versions. They scan raw rows and anti-join the ids that carry a deactivation marker (`dedup::resolved_survivors` / `dedup::active_survivors`, [§3.6](#36-interactions--sequences)). This is exact for deactivation against all currently-visible parts — a logical row is inactive iff some physical row with its `id` is — and it costs one hash probe per row against a tiny set instead of a sort or hash aggregation over every scanned row. The aggregation-latency NFR budget ([PRD.md §6.1](./PRD.md#61-gear-specific-nfrs)) is met with the anti-join included.

  What the range reads do **not** do is collapse duplicate *creates*. Those are prevented before they are stored: every `usage_records` `INSERT` carries an `insert_deduplication_token` (UUIDv5 of its sorted row ids, namespaced by insert kind) and the table keeps `non_replicated_deduplication_window = 10000`, so the engine drops a racing identical block on a synchronous insert. The residual — an asynchronous single-record insert whose twin landed in a different flush (async-insert dedup is a `Replicated*` feature; twins in one flush collapse through `optimize_on_insert`), or two non-identical overlapping batches — is visible twice to `list` and counted twice by `aggregate` until `ReplacingMergeTree(version)` collapses it at merge. Point reads resolve it immediately. Running the table as `ReplicatedReplacingMergeTree` closes the async gap, since `async_insert_deduplicate = 1` is already sent.
- **No cross-row transactional isolation inside ClickHouse itself, and no external mutex.** The deactivation cascade and each batch insert are atomic as a *single* multi-row `INSERT`; no ClickHouse operation is internally isolated from a concurrent, unrelated write, and this plugin adds no coordination primitive of its own. Concurrent writers to the same sort key are reconciled by the engine's insert dedup window when their tokens match (first-writer-wins) and by `ReplacingMergeTree(version)` at merge otherwise (last-writer-wins); the referential-integrity race is avoided rather than closed, by never deleting usage types. Every race — concurrent same-payload and differing-payload creates, retry-vs-deactivation resurrection, same-microsecond version ties, and replica lag — is enumerated explicitly in [§3.6](#36-interactions--sequences), never left implicit.
- **Version ties.** `version` is epoch microseconds minted by the writing process. Two instances can mint the same value for the same sort key; `ReplacingMergeTree` then keeps an arbitrary one. This is harmless when payloads match and is subsumed by the last-writer-wins case when they differ. Within one batch, per-row offsets keep the batch's own versions distinct.

## 4. Additional Context

### Non-Applicable Design Domains

- **Security Architecture**: not applicable as a plugin concern beyond transport/injection security — authentication, PDP authorization, and attribution validation are enforced upstream by the gear core; the plugin receives only authorized, validated calls.
- **Deployment Topology**: not detailed here — the plugin is statically linked into the gear process; ClickHouse deployment (single-node vs. replicated, sizing, region) follows the operator's ClickHouse deployment guide, with the consistency-profile caveats in [§3.8](#38-consistency--concurrency) called out for the replicated case specifically. No coordination backend is required for any topology ([§3.5](#35-external-dependencies)).

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

Integration tests run against a real ClickHouse container (Docker) — no coordination backend of any kind — covering dedup outcomes, compensation persistence, the deactivation cascade, the unimplemented-delete refusal (the row is left in place), keyset pagination, and aggregation correctness including the `MAX_AGGREGATION_BUCKETS + 1` cap. The crate compiles against the SDK trait, giving compile-time SPI conformance.

Unit tests for `ChCatalogStore` exercise the refresh-worker cancellation and coalescing behaviour, the client-side deadline, and the delete refusal offline: the delete test points the store at a socket that accepts and never answers, so returning immediately proves no SQL was issued. Unit tests for `ChRecordStore` drive the batch pipeline's pure parts directly — the catalog `IN` query builder (one bound parameter per distinct type, nothing inlined), the catalog-miss slot marking, and row composition (contiguous versions, absorbed rows compose nothing, in-batch twins share a row) — without I/O.

Referential-integrity-specific coverage is now limited to the create side: a mixed-`gts_id` batch test asserting each record is validated against its **own** type and an unregistered type's row is never written, and a single-record test asserting an unregistered `gts_id` returns `UsageTypeNotFound`. The former lock-ordering tests (create blocks delete, delete blocks create, lease-loss abort, release failure) are gone with the mechanism they proved. The dedup-convergence race test remains and asserts convergence only — both callers may get `Ok`; at most one row is visible to a version-resolved point read. Sibling tests on a synchronous store assert the engine-side guarantee directly: two racing identical single creates, or two racing identical batches, store exactly one physical row per id, and a deactivation marker is *not* deduplicated against the create that wrote its id. An `EXPLAIN PIPELINE` test asserts neither range read carries a `LimitBy` or version-sort step.

## 5. Traceability

- **PRD**: [PRD.md](./PRD.md)
- **Decomposition**: [DECOMPOSITION.md](./DECOMPOSITION.md)
- **Parent Gear PRD/DESIGN**: [../../../docs/PRD.md](../../../docs/PRD.md), [../../../docs/DESIGN.md](../../../docs/DESIGN.md)
- **Plugin SPI reference**: [../../../docs/plugin-spi.md](../../../docs/plugin-spi.md) (Method 1, Method 3, Method 5, Method 9 are load-bearing for this design's dedup, aggregation-cap, deactivation, and FK-emulation sections respectively)
- **Reference plugin (structural template)**: [../../timescaledb-usage-collector-plugin/docs/DESIGN.md](../../timescaledb-usage-collector-plugin/docs/DESIGN.md)
- **ADRs**: [`0002-pluggable-storage`](../../../docs/ADR/0002-pluggable-storage.md), [`0012-unified-plugin-catalog-and-gts-id-reference`](../../../docs/ADR/0012-unified-plugin-catalog-and-gts-id-reference.md), [`0013-deterministic-usage-record-id`](../../../docs/ADR/0013-deterministic-usage-record-id.md), [`0014-created-at-in-dedup-identity`](../../../docs/ADR/0014-created-at-in-dedup-identity.md)
