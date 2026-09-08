# Feature: Backend Observability & Metrics

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Out of Scope](#15-out-of-scope)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Backend Readiness Gauge Lifecycle](#backend-readiness-gauge-lifecycle)
  - [Metric Recording on Request Path](#metric-recording-on-request-path)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Metric Instrument Inventory](#metric-instrument-inventory)
  - [Label and Bucket Conventions](#label-and-bucket-conventions)
  - [Orphaned Reference Reconciliation (Defense-in-Depth)](#orphaned-reference-reconciliation-defense-in-depth)
- [4. States (CDSL)](#4-states-cdsl)
  - [Backend Readiness State](#backend-readiness-state)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement write-path metric instruments](#implement-write-path-metric-instruments)
  - [Implement read-path metric instruments](#implement-read-path-metric-instruments)
  - [Register no coordination-lock metric instruments](#register-no-coordination-lock-metric-instruments)
  - [Implement backend-readiness and catalog-size gauges](#implement-backend-readiness-and-catalog-size-gauges)
  - [Implement orphaned-reference detection counter](#implement-orphaned-reference-detection-counter)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-observability-implemented`

> Implemented in `src/infra/metrics.rs` with one exception: the background orphan-reconciliation worker (`cpt-cf-uc-ch-plugin-dod-observability-orphan-counter`) was never built and is explicitly deferred — see [§3 Orphaned Reference Reconciliation](#orphaned-reference-reconciliation-defense-in-depth) and its DoD in [§5](#5-definitions-of-done). This document describes the instrument inventory **as implemented**.

<!-- reference to DECOMPOSITION entry -->

- [x] `p3` - `cpt-cf-uc-ch-plugin-feature-observability`

## 1. Feature Context

### 1.1 Overview

Implement the `uc_clickhouse_*` OpenTelemetry instrument inventory for the ClickHouse storage backend — distinct from the reference plugin's `uc_timescaledb_*` sub-namespace. This is the allocation target for per-backend telemetry across all five capabilities: write-path insert/dedup/deactivation, read-path aggregation/list, catalog CRUD, and backend-health gauges.

### 1.2 Purpose

Backend Observability codifies the metric contract for this plugin: the instrument names, label keys, bucket boundaries, and the conventions (no unbounded-identifier labels, explicit bucket boundaries). It adds separate dedup-outcome counters specific to this backend's `ReplacingMergeTree`-based dedup emulation (`uc_clickhouse_dedup_absorbed_total`, `uc_clickhouse_idempotency_conflicts_total`, `uc_clickhouse_compensations_total` — one counter per outcome rather than a single counter with an `outcome` label), no coordination-lock instruments (there is no lock to instrument), and — specified but **deferred** — the `uc_clickhouse_orphaned_reference_detected_total` defense-in-depth counter, whose reconciliation worker was never built, so neither the worker nor the instrument exists in the crate. It also instruments the catalog-size background refresh worker and the readiness gauge.

**Constraints**: Following the existing `uc_timescaledb_*` pattern: bounded labels, explicit histogram bucket boundaries, no tenant/GTS identifier labels.

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-operator` | Consumes the `uc_clickhouse_*` metrics via the operator's telemetry pipeline (Prometheus scrape, OTLP export). |
| `cpt-cf-uc-ch-plugin-actor-plugin-host` | All SPI call sites record metrics on their hot paths. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §5 (Typed Error Classification: `error_category` label values), §6.1 (NFR: Operational Visibility — `cpt-cf-uc-ch-plugin-nfr-operational-visibility`)
- **Design**: [DESIGN.md](../DESIGN.md) — §4 Observability (metric inventory contract, naming, label conventions, and the deferred `uc_clickhouse_orphaned_reference_detected_total` rationale)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-observability`
- **Depends on**: `cpt-cf-uc-ch-plugin-feature-foundation` — the metric instruments are wired into the same start/request-path lifecycle points that Foundation provides; individual code paths being instrumented are co-located with each feature but do not establish a hard dependency on that feature's existence
- **Design element**: `cpt-cf-uc-ch-plugin-design-metric-inventory`

### 1.5 Out of Scope

- `uc_timescaledb_*` metrics — the reference plugin's metric sub-namespace; this feature owns only the `uc_clickhouse_*` namespace.
- ClickHouse server metrics (`system.replicas.absolute_delay`, part counts, merge rates) — these are ClickHouse-internal and operator-scraped from ClickHouse's own HTTP metrics endpoint, not OTLP metrics emitted by this plugin.
- Retention-specific TTL metrics — not defined for v1 (see Feature 5 §7 Non-Applicable Concerns).
- Per-tenant or per-`gts_id` metric breakdown — explicitly excluded; no unbounded identifier is ever a metric label.

## 2. Actor Flows (CDSL)

### Backend Readiness Gauge Lifecycle

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-flow-observability-readiness`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p3` - At `init` entry, after config validation and before any startup I/O: record `uc_clickhouse_ready = 0`, so a plugin whose `init` never completes is distinguishable from a gear that never started at all (no series) - `inst-ch-obs-ready-1`
2. [ ] - `p3` - After successful schema provisioning and registration: set `uc_clickhouse_ready` gauge to `1`, exactly once, at the end of `init` - `inst-ch-obs-ready-2`
3. [ ] - `p3` - On shutdown: a watcher task spawned after the `1` is recorded awaits the gear cancellation token and records `0`, so a drained replica does not report ready forever; a failed `init` (which reports `uc_clickhouse_migration_failures_total` and aborts registration) leaves the gauge at the startup `0`, and the catalog-size refresh worker must never re-arm it to `1` - `inst-ch-obs-ready-3`

### Metric Recording on Request Path

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-flow-observability-request-path`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p3` - At each SPI call entry: start a timer for the operation-duration histogram - `inst-ch-obs-req-1`
2. [ ] - `p3` - At SPI call exit (success or error): record the elapsed duration to the appropriate histogram (duration histograms are recorded via a drop guard, so error returns are captured too); increment the request-count counter on the query paths; increment `uc_clickhouse_backend_errors_total` if applicable, labelled by `error_category` with the two implemented values `transient` / `internal` (typed domain outcomes are not error-category values — they have their own counters where instrumented, e.g. `uc_clickhouse_idempotency_conflicts_total`) - `inst-ch-obs-req-2`
3. [ ] - `p3` - There is no lock-acquisition step to instrument: the plugin holds no coordination lock, so no lock-acquire histogram, contention counter or unavailability counter is recorded anywhere on the request path - `inst-ch-obs-req-3`
4. [ ] - `p3` - At insert: record the batch row count to `uc_clickhouse_batch_rows` (batch path), the insert duration to `uc_clickhouse_insert_duration_seconds{mode}`, and the dedup outcome to the counter for that outcome — `uc_clickhouse_dedup_absorbed_total` (exact-equality absorb) or `uc_clickhouse_idempotency_conflicts_total` (canonical mismatch); a fresh insert increments no dedup counter, and a `corrects_id`-carrying insert additionally increments `uc_clickhouse_compensations_total` - `inst-ch-obs-req-4`

## 3. Processes / Business Logic (CDSL)

### Metric Instrument Inventory

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-algo-observability-inventory`

All instruments live under the `uc_clickhouse_` prefix, registered on the `uc.clickhouse` instrumentation scope. Names are the **full literal** Prometheus names (no `.with_unit(...)` hint), matching `src/infra/metrics.rs`, which is the canonical inventory. The tables below list the instruments **as implemented**:

**Write path**:

| Instrument | Kind | Labels | Description |
| --- | --- | --- | --- |
| `uc_clickhouse_insert_duration_seconds` | Histogram | `mode: single \| batch` | Duration of the record `INSERT` for a single-row vs. multi-row write. |
| `uc_clickhouse_batch_rows` | Histogram | — | Row count per batch write (bucket boundaries `1, 5, 10, 50, 100, 500, 1000`). |
| `uc_clickhouse_dedup_absorbed_total` | Counter | — | Exact-equality retries silently absorbed on the dedup key (single and batch paths). |
| `uc_clickhouse_idempotency_conflicts_total` | Counter | — | Canonical-field-mismatch idempotency conflicts on the dedup key. |
| `uc_clickhouse_compensations_total` | Counter | — | Inserts carrying a `corrects_id` (compensating records). |
| `uc_clickhouse_deactivate_duration_seconds` | Histogram | — | `deactivate_usage_record` cascade duration. |
| `uc_clickhouse_pool_acquire_duration_seconds` | Histogram | — | Time to acquire an HTTP connection from the `ClickHouse` client pool (recorded on both write and catalog paths). |

There is **no** `uc_clickhouse_dedup_outcomes_total` and no `outcome` label: the three dedup outcomes are separate counters as listed above, and a plain fresh insert increments none of them (it is observable as `uc_clickhouse_insert_duration_seconds` recordings minus the absorb/conflict counters).

**Read path**:

| Instrument | Kind | Labels | Description |
| --- | --- | --- | --- |
| `uc_clickhouse_query_duration_seconds` | Histogram | `query_kind: raw \| aggregated` | `list_usage_records` (`raw`) / `query_aggregated_usage_records` (`aggregated`) duration. |
| `uc_clickhouse_query_requests_total` | Counter | `query_kind: raw \| aggregated` | Request count by query kind (workload mix observable). |

**Catalog**:

| Instrument | Kind | Labels | Description |
| --- | --- | --- | --- |
| `uc_clickhouse_usage_type_catalog_size` | Gauge | — | Current distinct `usage_type_catalog` `gts_id` count (refreshed by the background worker). **Not** monotone: `delete_usage_type` removes rows and signals the same worker. |

There are **no coordination-lock instruments**: the plugin uses no lock (DESIGN.md §3.5), so nothing could increment them. `uc_clickhouse_usage_type_referenced_total` and `uc_clickhouse_orphaned_reference_detected_total` **are** registered — `delete_usage_type` increments the first when it refuses a referenced type and the second when its post-delete sweep finds records that landed inside the probe→delete window (DESIGN.md §3.6).

**Backend health**:

| Instrument | Kind | Labels | Description |
| --- | --- | --- | --- |
| `uc_clickhouse_ready` | Gauge | — | `0` recorded at `init` entry, `1` once after successful registration, back to `0` when the gear cancellation token fires (see [§4](#4-states-cdsl)). |
| `uc_clickhouse_backend_errors_total` | Counter | `error_category: transient \| internal` | `ClickHouse`/backend errors by the SPI transient-vs-internal classification only. |
| `uc_clickhouse_migration_failures_total` | Counter | — | Schema-migration failures at plugin startup (the signal that pairs with a readiness gauge stuck at `0`). |
| `uc_clickhouse_usage_type_referenced_total` | Counter | — | `delete_usage_type` calls refused because the pre-delete probe found referencing rows (the 409 path). |
| `uc_clickhouse_orphaned_reference_detected_total` | Counter | — | `delete_usage_type` calls whose post-delete sweep found records that landed inside the probe→delete window. A nonzero rate means deletes are being run against types with live ingest. |

**Defense-in-depth**: `uc_clickhouse_orphaned_reference_detected_total` is registered and incremented by the delete sweep, but the periodic reconciliation *scan* that would catch orphans the delete path never saw is still deferred — so a zero value is not proof that no orphan exists (see [Orphaned Reference Reconciliation](#orphaned-reference-reconciliation-defense-in-depth)).

**Deliberately not implemented** (each with a backend-specific reason recorded in `src/infra/metrics.rs`): a `dedup_stale` counter (ClickHouse has no server-side dedup and thus no MVCC-window analogue), a batch-retry counter (no deadlock-victim retries on this backend), a TLS-handshake-failure counter (the HTTP client surfaces TLS errors as generic network errors), and pool-size gauges (the `clickhouse` 0.15.x crate exposes no counters for `reqwest`'s internal pool).

### Label and Bucket Conventions

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-algo-observability-conventions`

1. **No unbounded identifier labels**: `tenant_id`, `gts_id`, record `id`, or any other unbounded caller-supplied string is never a metric label key or value.
2. **Explicit histogram bucket boundaries**: every histogram **MUST** declare explicit bucket boundaries (not the OpenTelemetry SDK default). Every duration histogram shares one seconds-valued layout — `0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0` — chosen to bracket the §1.2 p95 budgets with finer low-end resolution (pool acquire, single insert) while still covering the 500ms aggregation budget; `uc_clickhouse_batch_rows` uses the row-count layout `1, 5, 10, 50, 100, 500, 1000`.
3. **`error_category` label values** for `uc_clickhouse_backend_errors_total` are exactly the two SPI classifications: `transient` and `internal`. Typed domain outcomes (`IdempotencyConflict`, `UsageTypeReferenced`, `UsageRecordNotFound`, `UsageTypeAlreadyExists`, …) are **not** `error_category` values — they are ordinary SPI results, and the ones worth counting have their own dedicated counters (`uc_clickhouse_idempotency_conflicts_total`). Each label value is backed by a closed Rust enum (`ErrorClass`), so an out-of-set value is unrepresentable at a call site.
4. **`mode` label values**: `single` / `batch` on `uc_clickhouse_insert_duration_seconds` (write shape) — the only `mode`-labelled instrument, now that there are no coordination-lock series. `single`/`batch` and `raw`/`aggregated` are each backed by a closed Rust enum (`InsertMode`, `QueryKind`).
5. All instruments are registered through the `opentelemetry` crate's SDK-agnostic API (meter obtained from the global `MeterProvider`); the plugin does not depend on a specific OTLP exporter.

### Orphaned Reference Reconciliation (Defense-in-Depth)

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-algo-observability-orphan-reconciliation`

> **PARTIALLY DEFERRED.** No reconciliation *worker* exists in the crate: nothing periodically scans for orphaned references and no reconciliation interval is configurable (there is no such config field). The `uc_clickhouse_orphaned_reference_detected_total` instrument **is** registered, and `delete_usage_type`'s post-delete sweep increments it (DESIGN.md §3.6) — so operators do have an orphan signal, but only for orphans the delete path itself observed. Absence of a nonzero value **MUST NOT** be read as evidence that no orphan exists: one created by an out-of-band `DELETE`, or by an insert that committed after the sweep, is invisible until a worker scans for it. The paragraph below is the retained specification for that worker.

Specification (deferred): a periodic background reconciliation job scans `usage_records` for rows whose `gts_id` is absent from `usage_type_catalog` (a LEFT JOIN / NOT IN subquery, bounded to avoid full-table scans). Each detected orphan increments `uc_clickhouse_orphaned_reference_detected_total` — the same instrument the delete sweep already uses, so the worker adds a second incrementer rather than a new series. The reconciliation interval would be configurable and default to a low-frequency background schedule (e.g. every 5 minutes) to avoid contending with the ingestion or query paths.

Why the worker is still safe to defer: the two ways an orphan can arise are both already narrow and both already leave a trace. `delete_usage_type` removes the catalog row *before* it sweeps, so the insert-time existence check refuses new records from that moment and the sweep cleans the window's own arrivals — and when it fires, it increments the counter. The remaining cases are an insert that committed after the sweep (the accepted residual, DESIGN.md §3.6) and an out-of-band `DELETE` run by an operator. Both are real and neither is detected today; the worker is what would close that gap, which is why it stays on the roadmap rather than being dropped.

## 4. States (CDSL)

### Backend Readiness State

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-state-backend-readiness`

| State | `uc_clickhouse_ready` value | Description |
| --- | --- | --- |
| Not started | _(no series)_ | The gear never reached `init`; no series exists at all. |
| Initializing | `0` | Recorded at `init` entry, before any startup I/O, so a stuck `init` is distinguishable from a process that never started. |
| Ready | `1` | `init` complete; backend is registered and serving SPI calls. Recorded exactly once. |
| Failed `init` | `0` (unchanged) | Provisioning or registration failed; the gauge stays at the startup `0` and `uc_clickhouse_migration_failures_total` carries the failure signal. |
| Shutting down | `0` | A watcher task spawned at the end of `init` awaits the gear cancellation token and records `0`, so a drained replica stops reporting ready; the catalog-size refresh worker **MUST NOT** re-arm it to `1` afterwards. |

## 5. Definitions of Done

### Implement write-path metric instruments

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-write-path`

The system **MUST** implement `uc_clickhouse_insert_duration_seconds` (Histogram, label `mode: single|batch`), `uc_clickhouse_batch_rows` (Histogram), the three dedup-outcome counters `uc_clickhouse_dedup_absorbed_total` / `uc_clickhouse_idempotency_conflicts_total` / `uc_clickhouse_compensations_total`, `uc_clickhouse_deactivate_duration_seconds` (Histogram), and `uc_clickhouse_pool_acquire_duration_seconds` (Histogram). Each instrument **MUST** be recorded at the appropriate call site in `ChRecordStore`, **except** `uc_clickhouse_pool_acquire_duration_seconds`, which is not write-path-exclusive: the pool is shared, so it **MUST** additionally be recorded on the catalog path in `ChCatalogStore`, matching its inventory row in [§3](#3-processes--business-logic-cdsl) ("recorded on both write and catalog paths"). All histograms **MUST** declare explicit bucket boundaries.

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (write-path instruments), `cpt-cf-uc-ch-plugin-flow-observability-request-path`

**Touches**: `infra/metrics.rs`; `ChRecordStore` call sites; `ChCatalogStore` call sites (pool-acquire only)

### Implement read-path metric instruments

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-read-path`

The system **MUST** implement `uc_clickhouse_query_duration_seconds` (Histogram, label `query_kind: raw|aggregated`) and `uc_clickhouse_query_requests_total` (Counter, same label). Each instrument **MUST** be recorded at the appropriate call site in `ChRecordStore`. Histograms **MUST** declare explicit bucket boundaries covering the 500ms aggregation latency budget.

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (read-path instruments), `cpt-cf-uc-ch-plugin-flow-observability-request-path`

**Touches**: `infra/metrics.rs`; `ChRecordStore` query call sites

### Register no coordination-lock metric instruments

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-lock-instruments`

The system **MUST NOT** register `uc_clickhouse_lock_acquire_duration_seconds`, `uc_clickhouse_lock_contention_total`, or `uc_clickhouse_lock_manager_unavailable_total`. The plugin has no coordination lock, so nothing could ever increment them; registering an instrument that stays at zero is indistinguishable from a healthy one and is worse than its absence. The system **MUST** register `uc_clickhouse_usage_type_referenced_total` and `uc_clickhouse_orphaned_reference_detected_total`, both of which `delete_usage_type` increments.

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (lock instruments), `cpt-cf-uc-ch-plugin-flow-observability-request-path`

**Touches**: `infra/metrics.rs`

### Implement backend-readiness and catalog-size gauges

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-gauges`

The system **MUST** implement `uc_clickhouse_ready` (Gauge, recorded `0` at `init` entry, `1` exactly once after a successful `init()`, and `0` again when the cancellation token fires — see [§4](#4-states-cdsl)), `uc_clickhouse_backend_errors_total` (Counter, label `error_category` with the two values `transient` / `internal`), `uc_clickhouse_migration_failures_total` (Counter, incremented when startup schema provisioning fails), `uc_clickhouse_usage_type_catalog_size` (Gauge, updated by the background catalog-size refresh worker in `ChCatalogStore`, and **not** monotone since `delete_usage_type` removes rows), `uc_clickhouse_usage_type_referenced_total` (Counter, incremented when a delete is refused for a referenced type), and `uc_clickhouse_orphaned_reference_detected_total` (Counter, incremented when a delete's post-delete sweep finds orphans).

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (health/catalog instruments), `cpt-cf-uc-ch-plugin-flow-observability-readiness`

**Touches**: `infra/metrics.rs`; `gear.rs`; `ChCatalogStore` background worker

### Implement orphaned-reference detection counter

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-orphan-counter`

> **PARTIALLY DEFERRED — intentionally unchecked.** The counter exists and `delete_usage_type`'s post-delete sweep increments it; the periodic reconciliation *worker* specified below does not. The worker is not scheduled for v1, but it is no longer merely defense-in-depth over a closed race: this plugin has no coordination lock, so the delete admits a residual orphaning window (DESIGN.md §3.6) that only a scan can detect after the fact (see [§3 Orphaned Reference Reconciliation](#orphaned-reference-reconciliation-defense-in-depth)). Closing it requires new code, not a documentation change.

The system **MUST** implement `uc_clickhouse_orphaned_reference_detected_total` (Counter) and a periodic background reconciliation worker that increments it for each orphaned `usage_records` row whose `gts_id` is absent from `usage_type_catalog`. The worker **MUST** be bounded (use `LIMIT` to avoid full-table scans), race against the gear cancellation token for prompt shutdown, and default to a low-frequency schedule (configurable, defaulting to 5 minutes).

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-orphan-reconciliation`

**Touches**: `infra/metrics.rs`; `infra/storage/` (reconciliation worker); `gear.rs` (spawn/cancel)

## 6. Acceptance Criteria

- [x] All `uc_clickhouse_*` instruments (except the explicitly deferred orphan counter) are registered and recorded at the appropriate call sites in Features 1–5.
- [x] No `tenant_id`, `gts_id`, record `id`, or any other unbounded caller-supplied string is used as a metric label.
- [x] All histograms declare explicit bucket boundaries.
- [x] `uc_clickhouse_ready` is `0` from `init` entry, `1` exactly once after successful registration, and `0` again once the cancellation token fires; it is never re-armed to `1` by the catalog-size refresh worker.
- [x] `uc_clickhouse_dedup_absorbed_total`, `uc_clickhouse_idempotency_conflicts_total`, and `uc_clickhouse_compensations_total` are incremented for their respective outcomes on both the single and batch insert paths.
- [x] No `uc_clickhouse_lock_*` series is registered: the plugin has no coordination lock.
- [x] `uc_clickhouse_usage_type_referenced_total` and `uc_clickhouse_orphaned_reference_detected_total` are registered, and `delete_usage_type` increments them on the refused-delete and swept-orphan paths respectively.
- [ ] **Deferred, not asserted**: `uc_clickhouse_orphaned_reference_detected_total` and its reconciliation worker are not implemented, so no acceptance test asserts the instrument's existence or its value (see the DoD note in [§5](#5-definitions-of-done)).
- [x] `uc_clickhouse_usage_type_catalog_size` is refreshed **asynchronously and eventually**, not synchronously per mutation: a `create_usage_type` signals the background worker via `tokio::sync::Notify`, and the worker coalesces a burst into at most one `SELECT uniqExact(gts_id) FROM usage_type_catalog` per wake (feature 0004 `cpt-cf-uc-ch-plugin-algo-catalog-size-refresh`). The gauge therefore lags a mutation briefly and a burst of *n* creates does not produce *n* gauge updates.
- [x] All instruments use the `opentelemetry` SDK-agnostic API (global meter); the plugin does not hard-depend on a specific OTLP exporter.
- [x] Unit tests use the `opentelemetry_sdk` `InMemoryMetricExporter` (gated behind the `testing` feature) to assert metric recordings without a live OTLP endpoint.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — metrics are read-only telemetry; access control is the operator's concern.
- **Security — Audit Trail**: Not applicable.
- **Data Privacy / Compliance**: No PII or business-sensitive data is recorded as a metric label or value. All label cardinality is bounded and operator-controlled.
- **Usability (UX)**: Not applicable — metrics are consumed by operator tooling (Prometheus, Grafana, etc.), not end users.
- **Deactivation / Retention**: Not applicable for the metric infrastructure itself; it instruments those features but has no own data lifecycle.
