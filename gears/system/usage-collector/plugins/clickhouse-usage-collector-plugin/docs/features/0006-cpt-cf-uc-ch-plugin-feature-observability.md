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
  - [Implement coordination-lock metric instruments](#implement-coordination-lock-metric-instruments)
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

Implement the `uc_clickhouse_*` OpenTelemetry instrument inventory for the ClickHouse storage backend — distinct from the reference plugin's `uc_timescaledb_*` sub-namespace. This is the allocation target for per-backend telemetry across all five capabilities: write-path insert/dedup/deactivation, read-path aggregation/list, catalog CRUD, coordination-lock behavior, and backend-health gauges.

### 1.2 Purpose

Backend Observability codifies the metric contract for this plugin: the instrument names, label keys, bucket boundaries, and the conventions (no unbounded-identifier labels, explicit bucket boundaries). It adds separate dedup-outcome counters specific to this backend's `ReplacingMergeTree`-based dedup emulation (`uc_clickhouse_dedup_absorbed_total`, `uc_clickhouse_idempotency_conflicts_total`, `uc_clickhouse_compensations_total` — one counter per outcome rather than a single counter with an `outcome` label), coordination-lock instruments (acquire duration, contention, lock-manager unavailability, each labelled by call path), and — specified but **deferred** — the `uc_clickhouse_orphaned_reference_detected_total` defense-in-depth counter, whose reconciliation worker was never built, so neither the worker nor the instrument exists in the crate. It also instruments the catalog-size background refresh worker and the readiness gauge.

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
2. [ ] - `p3` - At SPI call exit (success or error): record the elapsed duration to the appropriate histogram (duration histograms are recorded via a drop guard, so error returns are captured too); increment the request-count counter on the query paths; increment `uc_clickhouse_backend_errors_total` if applicable, labelled by `error_category` with the two implemented values `transient` / `internal` (typed domain outcomes are not error-category values — they have their own counters where instrumented, e.g. `uc_clickhouse_idempotency_conflicts_total`, `uc_clickhouse_usage_type_referenced_total`) - `inst-ch-obs-req-2`
3. [ ] - `p3` - At lock acquisition entry/exit: record acquire duration to the lock-acquire histogram, labelled by `mode` (`create` / `delete`); on a blocked wait, increment the lock-contention counter; on lock-manager unavailability **and** on a failed lease renew (`ensure_still_held`), increment the lock-manager-unavailable counter — all three carry the same `mode` label - `inst-ch-obs-req-3`
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
| `uc_clickhouse_usage_type_catalog_size` | Gauge | — | Current live `usage_type_catalog` row count (refreshed by the background worker). |
| `uc_clickhouse_usage_type_referenced_total` | Counter | — | Delete rejections because live usage records still reference the type. |

**Coordination lock**:

| Instrument | Kind | Labels | Description |
| --- | --- | --- | --- |
| `uc_clickhouse_lock_acquire_duration_seconds` | Histogram | `mode: create \| delete` | Exclusive-lock acquisition wait duration, by the call path that acquired it (`create` covers record ingest and `create_usage_type`; `delete` covers `delete_usage_type`). |
| `uc_clickhouse_lock_contention_total` | Counter | `mode: create \| delete` | Incremented when a lock acquisition had to wait for a conflicting holder of the same `gts_id` lock. |
| `uc_clickhouse_lock_manager_unavailable_total` | Counter | `mode: create \| delete` | Incremented when the cluster lock cannot be granted/released **and** when a lease-renew check (`ensure_still_held`) fails, so session loss is observable on the same series. |

**Backend health**:

| Instrument | Kind | Labels | Description |
| --- | --- | --- | --- |
| `uc_clickhouse_ready` | Gauge | — | `0` recorded at `init` entry, `1` once after successful registration, back to `0` when the gear cancellation token fires (see [§4](#4-states-cdsl)). |
| `uc_clickhouse_backend_errors_total` | Counter | `error_category: transient \| internal` | `ClickHouse`/backend errors by the SPI transient-vs-internal classification only. |
| `uc_clickhouse_migration_failures_total` | Counter | — | Schema-migration failures at plugin startup (the signal that pairs with a readiness gauge stuck at `0`). |

**Defense-in-depth**: none. `uc_clickhouse_orphaned_reference_detected_total` is **not implemented and not registered** — the reconciliation scan that would be its only legitimate incrementer is deferred, so the instrument is omitted rather than exported as a permanently-zero series; it is added back together with that worker (see [Orphaned Reference Reconciliation](#orphaned-reference-reconciliation-defense-in-depth)).

**Deliberately not implemented** (each with a backend-specific reason recorded in `src/infra/metrics.rs`): a `dedup_stale` counter (ClickHouse has no server-side dedup and thus no MVCC-window analogue), a batch-retry counter (no deadlock-victim retries on this backend), a TLS-handshake-failure counter (the HTTP client surfaces TLS errors as generic network errors), pool-size gauges (the `clickhouse` 0.15.x crate exposes no counters for `reqwest`'s internal pool), and `uc_clickhouse_orphaned_reference_detected_total` (deferred worker, above).

### Label and Bucket Conventions

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-algo-observability-conventions`

1. **No unbounded identifier labels**: `tenant_id`, `gts_id`, record `id`, or any other unbounded caller-supplied string is never a metric label key or value.
2. **Explicit histogram bucket boundaries**: every histogram **MUST** declare explicit bucket boundaries (not the OpenTelemetry SDK default). Every duration histogram shares one seconds-valued layout — `0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0` — chosen to bracket the §1.2 p95 budgets with finer low-end resolution (pool acquire, single insert) while still covering the 500ms aggregation budget; `uc_clickhouse_batch_rows` uses the row-count layout `1, 5, 10, 50, 100, 500, 1000`.
3. **`error_category` label values** for `uc_clickhouse_backend_errors_total` are exactly the two SPI classifications: `transient` and `internal`. Typed domain outcomes (`IdempotencyConflict`, `UsageTypeReferenced`, `UsageRecordNotFound`, `UsageTypeAlreadyExists`, …) are **not** `error_category` values — they are ordinary SPI results, and the ones worth counting have their own dedicated counters (`uc_clickhouse_idempotency_conflicts_total`, `uc_clickhouse_usage_type_referenced_total`). Each label value is backed by a closed Rust enum (`ErrorClass`), so an out-of-set value is unrepresentable at a call site.
4. **`mode` label values**: `single` / `batch` on `uc_clickhouse_insert_duration_seconds` (write shape), and `create` / `delete` on all three coordination-lock instruments — including `uc_clickhouse_lock_manager_unavailable_total`, which carries `mode` as well. On the lock instruments `mode` names the **call path** that took the lock, not a lock mode: the lock is exclusive-only, so there is no `shared` value. `create` covers every create-side acquisition (`create_usage_record(s)` and `create_usage_type`), `delete` covers `delete_usage_type`. `single`/`batch`, `create`/`delete`, and `raw`/`aggregated` are each backed by a closed Rust enum (`InsertMode`, `LockMode`, `QueryKind`).
5. All instruments are registered through the `opentelemetry` crate's SDK-agnostic API (meter obtained from the global `MeterProvider`); the plugin does not depend on a specific OTLP exporter.

### Orphaned Reference Reconciliation (Defense-in-Depth)

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-algo-observability-orphan-reconciliation`

> **DEFERRED — NOT IMPLEMENTED.** No reconciliation worker exists in the crate: nothing scans for orphaned references, no reconciliation interval is configurable (there is no such config field), and the `uc_clickhouse_orphaned_reference_detected_total` instrument is not registered at all — `src/infra/metrics.rs` records the omission and its reason instead of exporting a permanently-zero series. Operators therefore have **no** orphan signal today, and absence of the series **MUST NOT** be read as evidence that no orphan exists. The paragraph below is the retained specification for if/when the worker is built, not a description of current behavior.

Specification (deferred): a periodic background reconciliation job scans `usage_records` for rows whose `gts_id` is absent from `usage_type_catalog` (a LEFT JOIN / NOT IN subquery, bounded to avoid full-table scans). Each detected orphan increments `uc_clickhouse_orphaned_reference_detected_total`. Under the correct lock discipline this counter would remain at zero in production; a nonzero value would be an operator signal to investigate lock-manager health or a code regression. The reconciliation interval would be configurable and default to a low-frequency background schedule (e.g. every 5 minutes) to avoid contending with the ingestion or query paths.

Why it is safe to defer: the referential-integrity race this would backstop is closed by the per-`gts_id` exclusive coordination lock, and both the create and delete paths fail closed with `Transient` when the lock cannot be granted (DESIGN.md §3.6), so there is no accepted race for the scan to detect — it is defense-in-depth only.

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

The system **MUST** implement `uc_clickhouse_insert_duration_seconds` (Histogram, label `mode: single|batch`), `uc_clickhouse_batch_rows` (Histogram), the three dedup-outcome counters `uc_clickhouse_dedup_absorbed_total` / `uc_clickhouse_idempotency_conflicts_total` / `uc_clickhouse_compensations_total`, `uc_clickhouse_deactivate_duration_seconds` (Histogram), and `uc_clickhouse_pool_acquire_duration_seconds` (Histogram). Each instrument **MUST** be recorded at the appropriate call site in `ChRecordStore`. All histograms **MUST** declare explicit bucket boundaries.

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (write-path instruments), `cpt-cf-uc-ch-plugin-flow-observability-request-path`

**Touches**: `infra/metrics.rs`; `ChRecordStore` call sites

### Implement read-path metric instruments

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-read-path`

The system **MUST** implement `uc_clickhouse_query_duration_seconds` (Histogram, label `query_kind: raw|aggregated`) and `uc_clickhouse_query_requests_total` (Counter, same label). Each instrument **MUST** be recorded at the appropriate call site in `ChRecordStore`. Histograms **MUST** declare explicit bucket boundaries covering the 500ms aggregation latency budget.

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (read-path instruments), `cpt-cf-uc-ch-plugin-flow-observability-request-path`

**Touches**: `infra/metrics.rs`; `ChRecordStore` query call sites

### Implement coordination-lock metric instruments

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-lock-instruments`

The system **MUST** implement `uc_clickhouse_lock_acquire_duration_seconds` (Histogram, label `mode: create|delete`), `uc_clickhouse_lock_contention_total` (Counter, same label), and `uc_clickhouse_lock_manager_unavailable_total` (Counter, same label). These **MUST** be recorded in `LockManager::acquire_exclusive_for_create` and `acquire_exclusive_for_delete`; the unavailable counter **MUST** additionally be incremented by the guard's failed `ensure_still_held` (lease-renew) checks on both the create and delete critical sections.

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (lock instruments), `cpt-cf-uc-ch-plugin-flow-observability-request-path`

**Touches**: `infra/coordination/lock_manager.rs`; `infra/metrics.rs`

### Implement backend-readiness and catalog-size gauges

- [x] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-gauges`

The system **MUST** implement `uc_clickhouse_ready` (Gauge, recorded `0` at `init` entry, `1` exactly once after a successful `init()`, and `0` again when the cancellation token fires — see [§4](#4-states-cdsl)), `uc_clickhouse_backend_errors_total` (Counter, label `error_category` with the two values `transient` / `internal`), `uc_clickhouse_migration_failures_total` (Counter, incremented when startup schema provisioning fails), and `uc_clickhouse_usage_type_catalog_size` (Gauge, updated by the background catalog-size refresh worker in `ChCatalogStore`).

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-inventory` (health/catalog instruments), `cpt-cf-uc-ch-plugin-flow-observability-readiness`

**Touches**: `infra/metrics.rs`; `gear.rs`; `ChCatalogStore` background worker

### Implement orphaned-reference detection counter

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-dod-observability-orphan-counter`

> **DEFERRED — intentionally unchecked.** Neither the counter nor the reconciliation worker specified below exists: `src/infra/metrics.rs` documents the instrument's omission rather than registering it. This DoD is not scheduled for v1; it is defense-in-depth over a race the coordination lock already closes (see [§3 Orphaned Reference Reconciliation](#orphaned-reference-reconciliation-defense-in-depth)). Closing it requires new code, not a documentation change.

The system **MUST** implement `uc_clickhouse_orphaned_reference_detected_total` (Counter) and a periodic background reconciliation worker that increments it for each orphaned `usage_records` row whose `gts_id` is absent from `usage_type_catalog`. The worker **MUST** be bounded (use `LIMIT` to avoid full-table scans), race against the gear cancellation token for prompt shutdown, and default to a low-frequency schedule (configurable, defaulting to 5 minutes).

**Implements**: `cpt-cf-uc-ch-plugin-algo-observability-orphan-reconciliation`

**Touches**: `infra/metrics.rs`; `infra/storage/` (reconciliation worker); `gear.rs` (spawn/cancel)

## 6. Acceptance Criteria

- [x] All `uc_clickhouse_*` instruments (except the explicitly deferred orphan counter) are registered and recorded at the appropriate call sites in Features 1–5.
- [x] No `tenant_id`, `gts_id`, record `id`, or any other unbounded caller-supplied string is used as a metric label.
- [x] All histograms declare explicit bucket boundaries.
- [x] `uc_clickhouse_ready` is `0` from `init` entry, `1` exactly once after successful registration, and `0` again once the cancellation token fires; it is never re-armed to `1` by the catalog-size refresh worker.
- [x] `uc_clickhouse_dedup_absorbed_total`, `uc_clickhouse_idempotency_conflicts_total`, and `uc_clickhouse_compensations_total` are incremented for their respective outcomes on both the single and batch insert paths.
- [x] `uc_clickhouse_lock_acquire_duration_seconds`, `uc_clickhouse_lock_contention_total`, and `uc_clickhouse_lock_manager_unavailable_total` are recorded by `LockManager` on every lock acquisition attempt, each labelled by `mode`; a failed `ensure_still_held` also increments the unavailable counter.
- [ ] **Deferred, not asserted**: `uc_clickhouse_orphaned_reference_detected_total` and its reconciliation worker are not implemented, so no acceptance test asserts the instrument's existence or its value (see the DoD note in [§5](#5-definitions-of-done)).
- [x] `uc_clickhouse_usage_type_catalog_size` is updated by the catalog-size background refresh worker after each `create_usage_type` insertion.
- [x] All instruments use the `opentelemetry` SDK-agnostic API (global meter); the plugin does not hard-depend on a specific OTLP exporter.
- [x] Unit tests use the `opentelemetry_sdk` `InMemoryMetricExporter` (gated behind the `testing` feature) to assert metric recordings without a live OTLP endpoint.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — metrics are read-only telemetry; access control is the operator's concern.
- **Security — Audit Trail**: Not applicable.
- **Data Privacy / Compliance**: No PII or business-sensitive data is recorded as a metric label or value. All label cardinality is bounded and operator-controlled.
- **Usability (UX)**: Not applicable — metrics are consumed by operator tooling (Prometheus, Grafana, etc.), not end users.
- **Deactivation / Retention**: Not applicable for the metric infrastructure itself; it instruments those features but has no own data lifecycle.
