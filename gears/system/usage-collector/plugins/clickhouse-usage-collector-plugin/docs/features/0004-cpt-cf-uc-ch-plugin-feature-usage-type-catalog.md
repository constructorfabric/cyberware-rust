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
  - [Delete Usage Type — Lock-Protected Verify-Then-Delete](#delete-usage-type--lock-protected-verify-then-delete)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Create Pre-Existence Check and Idempotency Absorb](#create-pre-existence-check-and-idempotency-absorb)
  - [Delete-Side Lock-Protected Verify-Then-Delete Protocol](#delete-side-lock-protected-verify-then-delete-protocol)
  - [Catalog Size Background Refresh](#catalog-size-background-refresh)
- [4. States (CDSL)](#4-states-cdsl)
  - [Usage Type Existence](#usage-type-existence)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement create_usage_type](#implement-create_usage_type)
  - [Implement get_usage_type](#implement-get_usage_type)
  - [Implement list_usage_types with keyset pagination](#implement-list_usage_types-with-keyset-pagination)
  - [Implement delete_usage_type with lock-protected verify-then-delete](#implement-delete_usage_type-with-lock-protected-verify-then-delete)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-usage-type-catalog-implemented`

<!-- reference to DECOMPOSITION entry -->

- [x] `p1` - `cpt-cf-uc-ch-plugin-feature-usage-type-catalog`

## 1. Feature Context

### 1.1 Overview

Own the sole store for the `usage_type_catalog` table and the delete-side half of the application-emulated referential integrity between records and types. `create_usage_type` pre-checks then inserts, both under the exclusive `gts_id` coordination lock; `get`/`list` are `FINAL`-qualified; `delete_usage_type` acquires the same exclusive lock and runs the lock-protected verify-then-delete protocol — a real row removal via a lightweight `DELETE FROM` statement.

### 1.2 Purpose

This feature owns the create-idempotency absorb, the catalog point-read and keyset list, and — critically — the delete-side exclusive-lock verify-then-delete protocol that satisfies `plugin-spi.md` Method 9's "MUST NOT admit a window" requirement. Since ClickHouse has no native FK, both halves of referential integrity (create-side, owned by Feature 2; delete-side, owned here) are application-level invariants enforced under the same exclusive coordination lock per `gts_id`.

**Requirements**: `cpt-cf-uc-ch-plugin-fr-referential-integrity` (delete-side half)

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-gts-lock-required`, `cpt-cf-uc-ch-plugin-constraint-no-transactions`

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-plugin-host` | Dispatches `create_usage_type`, `get_usage_type`, `list_usage_types`, and `delete_usage_type` through the SPI. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §5 (Typed Error Classification, In-Backend Referential Integrity FR — `cpt-cf-uc-ch-plugin-fr-referential-integrity`)
- **Design**: [DESIGN.md](../DESIGN.md) — §2.2 (Cluster ADR-002 deviation), §3.5 (External Dependencies — lock semantics), §3.6 (Create Type, Delete Type sequences), §3.7 (usage_type_catalog table)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-usage-type-catalog`
- **Depends on**: `cpt-cf-uc-ch-plugin-feature-foundation`
- **Sequences**: `cpt-cf-uc-ch-plugin-seq-create-type`, `cpt-cf-uc-ch-plugin-seq-delete-type-fk`
- **DB Table**: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`
- **Component**: `cpt-cf-uc-ch-plugin-component-catalog-store`

### 1.5 Out of Scope

- Metadata-key validation, counter/gauge derivation — inherited pure-persistence posture; enforced upstream by the gear core.
- `usage_records` schema — Feature 1 (`cpt-cf-uc-ch-plugin-feature-foundation`).
- The create-side pre-insert catalog check and its exclusive create-path lock acquisition — Feature 2 (`cpt-cf-uc-ch-plugin-feature-record-persistence`). This feature's delete protocol assumes that check and lock usage exist but does not implement them.
- Construction of the Coordination Lock Manager — Feature 1; this feature only calls `CatalogLockPort::acquire_exclusive_for_create(gts_id)` and `acquire_exclusive_for_delete(gts_id)`.

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

1. [ ] - `p1` - Acquire the exclusive `gts_id` coordination lock via `CatalogLockPort::acquire_exclusive_for_create(gts_id)` — the same lock name as `create_usage_record`, `create_usage_records`, and `delete_usage_type` (all three mutating paths share the exclusive mutex per `gts_id`); timeout or unavailability → `Transient` - `inst-ch-cat-create-0`
2. [ ] - `p1` - `SELECT ... FINAL FROM usage_type_catalog WHERE gts_id = ?` — pre-existence check, under the lock - `inst-ch-cat-create-1`
3. [ ] - `p1` - **IF** found, `kind` and `metadata_fields` equal → silent absorb, **RETURN** the stored type - `inst-ch-cat-create-2`
4. [ ] - `p1` - **IF** found, `kind` or `metadata_fields` differ → **RETURN** `UsageTypeAlreadyExists` - `inst-ch-cat-create-3`
5. [ ] - `p1` - **IF** absent — proceed to insert - `inst-ch-cat-create-4`
   1. [ ] - `p1` - Call `LockGuardPort::ensure_still_held()` (lease renew) immediately before the INSERT; on `ClusterError::LockExpired` → release the lock, **RETURN** `Transient` - `inst-ch-cat-create-4a`
   2. [ ] - `p1` - `INSERT` with `version = current_epoch_μs()` - `inst-ch-cat-create-4b`
6. [ ] - `p1` - Signal the background catalog-size refresh worker via `tokio::sync::Notify` - `inst-ch-cat-create-5`
7. [ ] - `p1` - Release the lock explicitly on every exit path (absorb, conflict, insert, error) and **RETURN** the newly created type - `inst-ch-cat-create-6`

### Get Usage Type

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-catalog-get-type`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p1` - `SELECT ... FINAL FROM usage_type_catalog WHERE gts_id = ?` - `inst-ch-cat-get-1`
2. [ ] - `p1` - **IF** not found → **RETURN** `UsageTypeNotFound` - `inst-ch-cat-get-2`
3. [ ] - `p1` - **RETURN** the found type - `inst-ch-cat-get-3`

### List Usage Types (Keyset Paginated)

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-catalog-list-types`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p1` - `SELECT ... FINAL FROM usage_type_catalog [WHERE gts_id > ?] ORDER BY gts_id ASC LIMIT <n+1>` - `inst-ch-cat-list-1`
2. [ ] - `p1` - **IF** result contains `n+1` rows — truncate to `n`, encode the `n+1`-th row's `gts_id` as the next-cursor - `inst-ch-cat-list-2`
3. [ ] - `p1` - **RETURN** a `Page` of at most `n` types plus an optional next-cursor - `inst-ch-cat-list-3`

### Delete Usage Type — Lock-Protected Verify-Then-Delete

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-catalog-delete-type`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- Type exists, no references: type row is removed (`DELETE FROM`), lock released, success returned.

**Error Scenarios**:

- Lock-manager unavailable or `lock_timeout_secs` exceeded → return `Transient`.
- Type absent → release lock, return `UsageTypeNotFound`.
- References found → release lock without `DELETE`, return `UsageTypeReferenced`.
- `ensure_still_held` fails (lock lease expired during critical section) → abort with `Transient` before issuing `DELETE`.

**Steps**:

1. [ ] - `p1` - Acquire the exclusive `gts_id` coordination lock via `CatalogLockPort::acquire_exclusive_for_delete(gts_id)` — timeout → `Transient` - `inst-ch-cat-del-1`
2. [ ] - `p1` - **While holding the exclusive lock**: `SELECT ... FINAL FROM usage_type_catalog WHERE gts_id = ?` — absent → release lock, **RETURN** `UsageTypeNotFound` - `inst-ch-cat-del-2`
3. [ ] - `p1` - Bounded reference probe: `SELECT 1 FROM usage_records FINAL WHERE gts_id = ? LIMIT REF_COUNT_CAP` — because the exclusive lock excludes every concurrent create for this `gts_id`, any reference visible at this point is authoritative (not probabilistic) - `inst-ch-cat-del-3`
4. [ ] - `p1` - **IF** references found → release lock, **RETURN** `UsageTypeReferenced { gts_id, sample_ref_count }` - `inst-ch-cat-del-4`
5. [ ] - `p1` - **No references found**: call `LockGuardPort::ensure_still_held()` (lease renew) — **IF** lease expired → **RETURN** `Transient` (abort before issuing `DELETE`) - `inst-ch-cat-del-5`
6. [ ] - `p1` - Issue `DELETE FROM usage_type_catalog WHERE gts_id = ?` (lightweight real row removal; row masked from every subsequent query synchronously with statement return) - `inst-ch-cat-del-6`
7. [ ] - `p1` - Release the lock via `LockGuardPort::release()` - `inst-ch-cat-del-7`
8. [ ] - `p1` - **RETURN** success - `inst-ch-cat-del-8`

## 3. Processes / Business Logic (CDSL)

### Create Pre-Existence Check and Idempotency Absorb

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-catalog-create-idempotency`

`create_usage_type` holds the exclusive `gts_id` coordination lock across a `FINAL`-qualified pre-existence check and the `INSERT`, so the two form one critical section. If the type already exists with an identical payload (`kind` + `metadata_fields`), the call is absorbed silently — consistent with the reference plugin's upsert-identical semantics. If the payload differs, `UsageTypeAlreadyExists` is returned. This is **not** a re-execution of a business rule: it is the structural pre-existence check this backend requires because ClickHouse has no native `UNIQUE` constraint or `ON CONFLICT` clause. Because the lock is the same exclusive mutex the delete path and record ingest take, two concurrent creates for the same `gts_id` serialize and the loser observes the winner's row (absorb or `UsageTypeAlreadyExists`) instead of inserting a second physical row; `version = current_epoch_μs()` and `ReplacingMergeTree(version)` resolution remain the convergence backstop rather than the primary defense.

### Delete-Side Lock-Protected Verify-Then-Delete Protocol

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-catalog-delete-fk`

The exclusive-mutex coordination lock per `gts_id` — the same lock name as the create path — makes this sequence's referential-integrity guarantee unconditional:

- Once the exclusive lock is granted, no new `create_usage_record`/`create_usage_records` call for this `gts_id` can start (they all acquire the same exclusive mutex name).
- The bounded reference probe (`LIMIT REF_COUNT_CAP` scan) is therefore authoritative: any reference that exists is visible; no new reference can be created while the lock is held.
- `ensure_still_held()` (lease renew) guards against lock-lease expiry between the probe and the `DELETE FROM` (cluster ADR-002 deviation — holding a cluster lock across ClickHouse remote I/O); if the lease expired, the call aborts with `Transient` before the `DELETE` is issued.
- The `DELETE FROM usage_type_catalog WHERE gts_id = ?` is a lightweight ClickHouse primitive (distinct from `ALTER TABLE ... DELETE`): the row is masked from every subsequent query synchronously with the statement's return; physical removal happens in background merges.
- No rollback step exists: the `DELETE` is issued only after the verify step has run to completion under the exclusive lock; there is no possibility of a reference landing after row removal is missed by the verify.

### Catalog Size Background Refresh

- [ ] `p3` - **ID**: `cpt-cf-uc-ch-plugin-algo-catalog-size-refresh`

`ChCatalogStore` spawns a single background `tokio` worker that coalesces mutation-triggered `COUNT(*)` refresh requests via a `tokio::sync::Notify` signal. The worker races each `count()` query against the gear cancellation token for prompt shutdown. The refreshed count is cached for the `uc_clickhouse_usage_type_catalog_size` gauge (Feature 6). Coalescing means that a burst of `create_usage_type` calls triggers at most one `COUNT(*)` round-trip per worker-wake, not one per create.

## 4. States (CDSL)

### Usage Type Existence

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-state-usage-type-existence`

| State | Description |
| --- | --- |
| Present | The `gts_id` row exists in `usage_type_catalog` (visible via `FINAL`-qualified read). `create_usage_record` catalog-existence check accepts this `gts_id`. |
| Absent | The row never existed, or was removed via `delete_usage_type`. `create_usage_record` rejects this `gts_id` with `UsageTypeNotFound`. |

**Transition**: Absent → Present via `create_usage_type`; Present → Absent via `delete_usage_type` (only when no `usage_records` references exist). The deletion is a real lightweight row removal — no tombstone row. A re-`create` after deletion creates a fresh row.

## 5. Definitions of Done

### Implement create_usage_type

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-create-type`

The system **MUST** implement `create_usage_type` as: acquire the exclusive `gts_id` lock via `CatalogLockPort::acquire_exclusive_for_create` (unavailability → `Transient`) → `FINAL`-qualified pre-existence check under the lock → identical-payload silent absorb, different-payload `UsageTypeAlreadyExists`, or absent → call `LockGuardPort::ensure_still_held()` (lease renew) immediately before the INSERT (on `ClusterError::LockExpired` → release the lock, return `Transient`) → `INSERT` with `version = current_epoch_μs()` + notify the background catalog-size refresh worker → release the lock explicitly on every exit path.

**Implements**: `cpt-cf-uc-ch-plugin-algo-catalog-create-idempotency`, `cpt-cf-uc-ch-plugin-flow-catalog-create-type`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-create-type`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-catalog-store`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

### Implement get_usage_type

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-get-type`

The system **MUST** implement `get_usage_type` as a `FINAL`-qualified point read by `gts_id`; absent → `UsageTypeNotFound`.

**Implements**: `cpt-cf-uc-ch-plugin-flow-catalog-get-type`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-catalog-store`

### Implement list_usage_types with keyset pagination

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-list-types`

The system **MUST** implement `list_usage_types` as a `FINAL`-qualified keyset-paginated list ordered by `gts_id ASC` (forward-only, fixed order). A `n+1` look-ahead determines whether a next-cursor exists. No backward paging is supported in v1.

**Implements**: `cpt-cf-uc-ch-plugin-flow-catalog-list-types`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-catalog-store`

### Implement delete_usage_type with lock-protected verify-then-delete

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-catalog-delete-type`

The system **MUST** implement `delete_usage_type` as the lock-protected verify-then-delete protocol: acquire exclusive `gts_id` lock via `CatalogLockPort::acquire_exclusive_for_delete`, run existence check, run bounded reference probe (`LIMIT REF_COUNT_CAP`), call `LockGuardPort::ensure_still_held()` before the `DELETE FROM`, issue the lightweight `DELETE FROM`, release the lock. Lock **MUST** be released on every exit path (including lock-expired abort). Lock-manager unavailability **MUST** return `Transient`. `ChCatalogStore` **MUST** depend on `Arc<dyn CatalogLockPort>` to support offline unit testing of the critical section via stub implementations.

**Implements**: `cpt-cf-uc-ch-plugin-algo-catalog-delete-fk`, `cpt-cf-uc-ch-plugin-flow-catalog-delete-type`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-delete-type-fk`

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-gts-lock-required`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-catalog-store`
- DB Tables: `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`, `cpt-cf-uc-ch-plugin-dbtable-usage-records`

## 6. Acceptance Criteria

- [x] `create_usage_type` absorbs silently on identical re-submission; returns `UsageTypeAlreadyExists` on a payload mismatch; inserts with `version = current_epoch_μs()` on first create — all under the exclusive `gts_id` lock, so two concurrent creates for the same `gts_id` serialize and lock-manager unavailability returns `Transient`.
- [x] `get_usage_type` is `FINAL`-qualified; absent `gts_id` returns `UsageTypeNotFound`.
- [x] `list_usage_types` is `FINAL`-qualified, returns pages ordered by `gts_id ASC`, uses `n+1` look-ahead cursor pattern.
- [x] `delete_usage_type` acquires the exclusive `gts_id` coordination lock before any read; lock-manager unavailability returns `Transient`.
- [x] The reference probe uses `LIMIT REF_COUNT_CAP`; with the exclusive lock held, the probe result is authoritative — no new reference can be created while the probe and `DELETE FROM` execute.
- [x] `ensure_still_held()` is called immediately before the `DELETE FROM`; if the lease has expired, the call returns `Transient` without issuing the `DELETE`.
- [x] `DELETE FROM usage_type_catalog WHERE gts_id = ?` is a lightweight real row removal — not a tombstone flag, not a higher-version marker row.
- [x] Lock is released on every exit path (success, type-not-found, referenced, lease-expired, ClickHouse error).
- [x] `ChCatalogStore` depends on `Arc<dyn CatalogLockPort>` and can be unit-tested with a stub without a live ClickHouse or cluster backend.
- [x] A type deleted via `delete_usage_type` is subsequently invisible to `create_usage_record`'s catalog-existence check (returns `UsageTypeNotFound`).

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — enforcement is upstream; this feature's security obligation is correct lock discipline and injection-safe queries (bound parameters).
- **Security — Audit Trail**: Not applicable.
- **Data Privacy / Compliance**: Not applicable — `kind` and `metadata_fields` are opaque strings passed through from callers; no classification is performed here.
- **Usability (UX)**: Not applicable — no user interface.
- **Observability (OPS-FDESIGN-001)**: the lock-acquire-duration histogram and the `uc_clickhouse_usage_type_catalog_size` gauge are allocated to Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`); this feature provides the catalog write path they instrument. The once-specified `uc_clickhouse_orphaned_reference_detected_total` counter is **deferred and not registered** (Feature 6 §5), so nothing in this feature emits it.
- **Retention / TTL**: Not applicable — `usage_type_catalog` is reference data and is never retention-bounded (Feature 5 scope covers `usage_records` only).
