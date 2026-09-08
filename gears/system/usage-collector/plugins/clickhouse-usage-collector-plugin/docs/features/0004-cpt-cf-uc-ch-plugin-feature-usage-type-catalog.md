# Feature: Usage-Type Catalog & Referential Integrity

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Out of Scope](#15-out-of-scope)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Usage Type](#create-usage-type)
  - [Get Usage Type](#get-usage-type)
  - [List Usage Types (Keyset Paginated)](#list-usage-types-keyset-paginated)
  - [Delete Usage Type — Probe, Delete, Sweep](#delete-usage-type--probe-delete-sweep)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Create Pre-Existence Check and Idempotency Absorb](#create-pre-existence-check-and-idempotency-absorb)
  - [Delete-Side Protocol](#delete-side-protocol)
  - [Catalog Size Background Refresh](#catalog-size-background-refresh)
- [4. States (CDSL)](#4-states-cdsl)
  - [Usage Type Existence](#usage-type-existence)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement create_usage_type](#implement-create_usage_type)
  - [Implement get_usage_type](#implement-get_usage_type)
  - [Implement list_usage_types with keyset pagination](#implement-list_usage_types-with-keyset-pagination)
  - [Implement delete_usage_type as probe-delete-sweep](#implement-delete_usage_type-as-probe-delete-sweep)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-usage-type-catalog-implemented`

<!-- reference to DECOMPOSITION entry -->

- [x] `p1` - `cpt-cf-uc-ch-plugin-feature-usage-type-catalog`

## 1. Feature Context

### 1.1 Overview

Own the sole store for the `usage_type_catalog` table. `create_usage_type` pre-checks then inserts with no coordination primitive between the two statements; `get`/`list` resolve versions in SQL; `delete_usage_type` probes for references, removes the catalog row with `ALTER TABLE … DELETE`, then sweeps any record that landed in between — narrowing, not closing, the orphaning window.

### 1.2 Purpose

This feature owns the create-idempotency absorb, the catalog point-read and keyset list, and the delete-side half of referential integrity. ClickHouse has no native FK and this plugin has no mutual-exclusion primitive, so a delete cannot be ordered against concurrent `create_usage_record(s)` calls referencing the same `gts_id`. The delete therefore pairs a capped pre-delete reference probe with a post-delete orphan sweep and removes the catalog row between them, so that Feature 2's insert-time check — the create-side half — starts refusing new records for the `gts_id` and bounds the window the probe cannot close. The residual is accepted and documented, not eliminated.

**Requirements**: `cpt-cf-uc-ch-plugin-fr-referential-integrity` (the delete-side half; the create-side half is owned by Feature 2)

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-no-transactions`. `cpt-cf-uc-ch-plugin-constraint-gts-lock-required` is superseded — no coordination lock exists in this plugin (DESIGN.md §2.2).

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-plugin-host` | Dispatches `create_usage_type`, `get_usage_type`, `list_usage_types`, and `delete_usage_type` through the SPI. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §5 (Typed Error Classification, In-Backend Referential Integrity FR — `cpt-cf-uc-ch-plugin-fr-referential-integrity`)
- **Design**: [DESIGN.md](../DESIGN.md) — §2.2 (Constraints), §3.6 (Create Type, Delete Type sequences), §3.7 (usage_type_catalog table), §3.8 (Consistency & Concurrency)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-usage-type-catalog`
- **Depends on**: `cpt-cf-uc-ch-plugin-feature-foundation`
- **Sequences**: `cpt-cf-uc-ch-plugin-seq-create-type`, `cpt-cf-uc-ch-plugin-seq-delete-type-fk`
- **DB Table**: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`
- **Component**: `cpt-cf-uc-ch-plugin-component-catalog-store`

### 1.5 Out of Scope

- Metadata-key validation, counter/gauge derivation — inherited pure-persistence posture; enforced upstream by the gear core.
- `usage_records` schema — Feature 1 (`cpt-cf-uc-ch-plugin-feature-foundation`).
- The create-side pre-insert catalog check — Feature 2 (`cpt-cf-uc-ch-plugin-feature-record-persistence`).
- Any coordination lock — none exists in this plugin.

## 2. Actor Flows (CDSL)

### Create Usage Type

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-catalog-create-type`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- Type absent: `INSERT` succeeds; catalog-size refresh signal sent.
- Type present, identical payload (`kind` + `metadata_fields` equal): silent absorb — return stored type.

**Error Scenarios**:

- Type present, different payload — return `UsageTypeAlreadyExists`.
- ClickHouse error — classify and return.

**Steps**:

1. [ ] - `p1` - `SELECT ... FROM usage_type_catalog WHERE gts_id = ? ORDER BY version DESC LIMIT 1` — pre-existence check - `inst-ch-cat-create-1`
2. [ ] - `p1` - **IF** found, `kind` and `metadata_fields` equal → silent absorb, **RETURN** the stored type - `inst-ch-cat-create-2`
3. [ ] - `p1` - **IF** found, `kind` or `metadata_fields` differ → **RETURN** `UsageTypeAlreadyExists` - `inst-ch-cat-create-3`
4. [ ] - `p1` - **IF** absent — `INSERT` with `version = current_epoch_μs()` - `inst-ch-cat-create-4`
5. [ ] - `p1` - Signal the background catalog-size refresh worker via `tokio::sync::Notify` - `inst-ch-cat-create-5`
6. [ ] - `p1` - **RETURN** the newly created type - `inst-ch-cat-create-6`

### Get Usage Type

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-catalog-get-type`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p1` - `SELECT ... FROM usage_type_catalog WHERE gts_id = ? ORDER BY version DESC LIMIT 1` - `inst-ch-cat-get-1`
2. [ ] - `p1` - **IF** not found → **RETURN** `UsageTypeNotFound` - `inst-ch-cat-get-2`
3. [ ] - `p1` - **RETURN** the found type - `inst-ch-cat-get-3`

### List Usage Types (Keyset Paginated)

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-catalog-list-types`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p1` - `SELECT ... FROM (SELECT ... FROM usage_type_catalog ORDER BY version DESC LIMIT 1 BY gts_id) [WHERE gts_id > ?] ORDER BY gts_id ASC LIMIT <n+1>` - `inst-ch-cat-list-1`
2. [ ] - `p1` - **IF** result contains `n+1` rows — truncate to `n`, encode the `n+1`-th row's `gts_id` as the next-cursor - `inst-ch-cat-list-2`
3. [ ] - `p1` - **RETURN** a `Page` of at most `n` types plus an optional next-cursor - `inst-ch-cat-list-3`

### Delete Usage Type — Probe, Delete, Sweep

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-catalog-delete-type`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- The `gts_id` exists and no `usage_records` row references it → the catalog row is removed and `Ok(())` returned. The removal is visible to the next read (`mutations_sync = 1`).
- Same, plus records landed inside the probe→delete window → those records are swept, `uc_clickhouse_orphaned_reference_detected_total` is incremented, and `Ok(())` is still returned.

**Error Scenarios**:

- No row carries the `gts_id` → `UsageTypeNotFound { gts_id }` (HTTP 404). Absence is an error, not a silent success, so the gateway can distinguish "already gone" from "deleted now".
- Any `usage_records` row references the `gts_id` → `UsageTypeReferenced { gts_id, sample_ref_count }` (HTTP 409), `uc_clickhouse_usage_type_referenced_total` incremented, catalog row untouched.
- A backend failure on the existence read or the probe → `Transient` / `Internal` per the usual classification. It **MUST NOT** be reported as `UsageTypeNotFound`: a read that never happened is not evidence of absence.
- A backend failure on the post-delete probe or sweep → logged at `error`, **not** propagated. The type is deleted; reporting failure would misstate the outcome and a retry could only answer `UsageTypeNotFound`.

**Steps**:

1. [ ] - `p1` - `SELECT gts_id FROM usage_type_catalog WHERE gts_id = ? LIMIT 1`; **IF** no row → **RETURN** `UsageTypeNotFound` - `inst-ch-cat-del-1`
2. [ ] - `p1` - `SELECT count() FROM (SELECT 1 FROM usage_records WHERE gts_id = ? LIMIT REF_COUNT_CAP)`; **IF** non-zero → increment `uc_clickhouse_usage_type_referenced_total` and **RETURN** `UsageTypeReferenced { gts_id, sample_ref_count }` - `inst-ch-cat-del-2`
3. [ ] - `p1` - `ALTER TABLE usage_type_catalog DELETE WHERE gts_id = ?` with `mutations_sync = 1` - `inst-ch-cat-del-3`
4. [ ] - `p1` - Re-run step 2's probe; **IF** non-zero → increment `uc_clickhouse_orphaned_reference_detected_total`, log at `warn`, and issue `ALTER TABLE usage_records DELETE WHERE gts_id = ?` with `mutations_sync = 1` - `inst-ch-cat-del-4`
5. [ ] - `p1` - Signal the catalog-size refresh worker and **RETURN** `Ok(())` - `inst-ch-cat-del-5`

## 3. Processes / Business Logic (CDSL)

### Create Pre-Existence Check and Idempotency Absorb

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-catalog-create-idempotency`

`create_usage_type` runs a version-resolved pre-existence check followed by an `INSERT`, as two independent statements. If the type already exists with an identical payload (`kind` + `metadata_fields`), the call is absorbed silently — consistent with the reference plugin's upsert-identical semantics. If the payload differs, `UsageTypeAlreadyExists` is returned. This is **not** a re-execution of a business rule: it is the structural pre-existence check this backend requires because ClickHouse has no native `UNIQUE` constraint or `ON CONFLICT` clause.

Nothing serializes two concurrent creates for the same `gts_id`. Both may pass the pre-existence check and both may insert; `version = current_epoch_μs()` and `ReplacingMergeTree(version)` resolution then converge the physical rows to the one with the highest version, so inside that window the catalog is **last-writer-wins** and both callers observe `Ok` even when their payloads differ. Once the winner's row is visible, a later create sees it and returns an absorb or `UsageTypeAlreadyExists`.

### Delete-Side Protocol

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-catalog-delete-fk`

`delete_usage_type` emulates `ON DELETE RESTRICT` with a probe rather than a constraint, because ClickHouse has neither a foreign key nor (in this plugin) a mutual-exclusion primitive to make the probe authoritative.

**Step order is the whole design.** The catalog row is removed *before* the orphan sweep. From that moment the record store's insert-time catalog existence check (Feature 2) refuses new records for the `gts_id` on its own, so the sweep has only the probe→delete window's own arrivals to clean up. Sweeping first would leave a strictly wider window, since new records could keep arriving until the catalog row went away.

**The residual race.** The probe is a snapshot. A `create_usage_record` whose own catalog check passed before step 3 can commit after step 4 and orphan a row; with `async_insert` on (the default) it can sit in a server-side buffer, widening the window from microseconds to the flush interval. `plugin-spi.md` Method 9's "MUST NOT admit a window" clause is therefore **not** met by this backend — the window is bounded and instrumented, not closed. An earlier revision of this plugin used a per-`gts_id` exclusive cluster lock to close it and, once that lock was removed, withheld the operation entirely. Offering it with the race documented was chosen over withholding it: an append-only catalog left operators no way to remove a mis-registered type except hand-written SQL, which admits the same window with none of the probe, the sweep, the 409, or the counter.

The mutation is a heavyweight `ALTER TABLE … DELETE`, not a lightweight `DELETE FROM`: it removes the physical rows, so a later `create_usage_type` for the same `gts_id` has no surviving `ReplacingMergeTree` copy to outrank.

### Catalog Size Background Refresh

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-algo-catalog-size-refresh`

`ChCatalogStore` spawns a single background `tokio` worker that coalesces mutation-triggered refresh requests via a `tokio::sync::Notify` signal. Each refresh issues `SELECT uniqExact(gts_id) FROM usage_type_catalog` (`gts_id` is the whole sort key, so counting distinct ids reflects live types rather than unmerged duplicate copies, without paying for `FINAL`), raced against the gear cancellation token for prompt shutdown. The refreshed count is cached for the `uc_clickhouse_usage_type_catalog_size` gauge (Feature 6). Coalescing means that a burst of `create_usage_type` calls triggers at most one `count()` round-trip per worker-wake, not one per create. `delete_usage_type` signals the same worker, so the gauge is **not** monotone — it falls as types are removed.

## 4. States (CDSL)

### Usage Type Existence

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-state-usage-type-existence`

| State | Description |
| --- | --- |
| Present | The `gts_id` row exists in `usage_type_catalog` (visible to a version-resolved read). `create_usage_record` catalog-existence check accepts this `gts_id`. |
| Absent | The row does not exist — never created, or removed by `delete_usage_type`. `create_usage_record` rejects this `gts_id` with `UsageTypeNotFound`, and so does `delete_usage_type`. |

**Transitions**: Absent → Present via `create_usage_type`; Present → Absent via `delete_usage_type`, but only for a type no `usage_records` row references (otherwise the delete is refused with `UsageTypeReferenced` and the state does not change). Absent → Present again is permitted: the delete removes the physical rows, so a re-create is an ordinary create with no earlier row to outrank. The Present → Absent transition is not ordered against a concurrent `create_usage_record`; see the Delete-Side Protocol above for the residual.

## 5. Definitions of Done

### Implement create_usage_type

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-create-type`

The system **MUST** implement `create_usage_type` as: version-resolved pre-existence check → identical-payload silent absorb, different-payload `UsageTypeAlreadyExists`, or absent → `INSERT` with `version = current_epoch_μs()` + notify the background catalog-size refresh worker. No coordination primitive is taken; concurrent same-`gts_id` creates converge last-writer-wins via `ReplacingMergeTree(version)`.

**Implements**: `cpt-cf-uc-ch-plugin-algo-catalog-create-idempotency`, `cpt-cf-uc-ch-plugin-flow-catalog-create-type`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-create-type`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-catalog-store`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

### Implement get_usage_type

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-get-type`

The system **MUST** implement `get_usage_type` as a version-resolved point read by `gts_id`; absent → `UsageTypeNotFound`.

**Implements**: `cpt-cf-uc-ch-plugin-flow-catalog-get-type`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-catalog-store`

### Implement list_usage_types with keyset pagination

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-list-types`

The system **MUST** implement `list_usage_types` as a version-resolved keyset-paginated list ordered by `gts_id ASC` (forward-only, fixed order). A `n+1` look-ahead determines whether a next-cursor exists. No backward paging is supported in v1.

**Implements**: `cpt-cf-uc-ch-plugin-flow-catalog-list-types`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-catalog-store`

### Implement delete_usage_type as probe-delete-sweep

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-delete-type`

The system **MUST** implement `delete_usage_type` as: an existence read (absent → `UsageTypeNotFound { gts_id }`, never a silent success); a capped reference probe over `usage_records` counting rows of every `status` (non-zero → `uc_clickhouse_usage_type_referenced_total` incremented and `UsageTypeReferenced { gts_id, sample_ref_count }` returned with the catalog row untouched); `ALTER TABLE usage_type_catalog DELETE WHERE gts_id = ?` under `mutations_sync = 1`; and a re-probe-gated `ALTER TABLE usage_records DELETE WHERE gts_id = ?` sweep that increments `uc_clickhouse_orphaned_reference_detected_total` when it fires.

The catalog row **MUST** be removed before the sweep, so Feature 2's insert-time check bounds the window. A post-delete probe or sweep failure **MUST** be logged at `error` and **MUST NOT** be propagated — the type is deleted. A failure on the existence read or the pre-delete probe **MUST** surface as a backend error and **MUST NOT** be reported as `UsageTypeNotFound`. The catalog-size refresh worker **MUST** be signalled on success. `ChCatalogStore` **MUST** remain constructible without any coordination dependency (`new(client, cancel, metrics, request_timeout)`).

**Implements**: `cpt-cf-uc-ch-plugin-flow-catalog-delete-type`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-delete-type-fk`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-catalog-store`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

## 6. Acceptance Criteria

- [x] `create_usage_type` absorbs silently on identical re-submission; returns `UsageTypeAlreadyExists` on a payload mismatch once the earlier row is visible; inserts with `version = current_epoch_μs()` on first create. No lock is taken; two concurrent creates for the same `gts_id` may both succeed and converge last-writer-wins.
- [x] `get_usage_type` resolves versions (`ORDER BY version DESC LIMIT 1`); absent `gts_id` returns `UsageTypeNotFound`.
- [x] `list_usage_types` resolves versions in an inner subquery (`LIMIT 1 BY gts_id`), returns pages ordered by `gts_id ASC`, uses `n+1` look-ahead cursor pattern.
- [x] `delete_usage_type` removes an unreferenced type, and `get_usage_type` reports `UsageTypeNotFound` on the very next read (the mutation is synchronous).
- [x] `delete_usage_type` on an absent `gts_id` returns `UsageTypeNotFound`, not a silent success.
- [x] `delete_usage_type` on a type with any referencing record returns `UsageTypeReferenced` with `sample_ref_count >= 1`, and `get_usage_type` still returns the type afterwards.
- [x] A type deleted and then re-created is readable, with no surviving earlier row to outrank.
- [x] Once a type is deleted, `create_usage_record` for its `gts_id` returns `UsageTypeNotFound` — the property that bounds the delete's race window.
- [x] Against an unreachable backend, `delete_usage_type` surfaces the failure and does **not** report `UsageTypeNotFound`.
- [x] `ChCatalogStore` can be unit-tested offline without a live ClickHouse or any coordination backend.
- [x] The `uc_clickhouse_usage_type_referenced_total` counter and every `uc_clickhouse_lock_*` series are not registered — nothing could increment them.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — enforcement is upstream; this feature's security obligation is injection-safe queries (bound parameters).
- **Security — Audit Trail**: Not applicable.
- **Data Privacy / Compliance**: Not applicable — `kind` and `metadata_fields` are opaque strings passed through from callers; no classification is performed here.
- **Usability (UX)**: Not applicable — no user interface.
- **Observability (OPS-FDESIGN-001)**: the `uc_clickhouse_usage_type_catalog_size` gauge is allocated to Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`); this feature provides the catalog write path it instruments. The once-specified lock-acquire histogram and `uc_clickhouse_orphaned_reference_detected_total` counter are **not registered**, so nothing in this feature emits them.
- **Retention / TTL**: Not applicable — `usage_type_catalog` is reference data and is never retention-bounded (Feature 5 scope covers `usage_records` only).
