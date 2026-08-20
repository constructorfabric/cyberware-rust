# Feature: Record Persistence & Lifecycle

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Out of Scope](#15-out-of-scope)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Single Usage Record (with Dedup)](#create-single-usage-record-with-dedup)
  - [Create Batch of Usage Records](#create-batch-of-usage-records)
  - [Get Usage Record](#get-usage-record)
  - [Deactivate Usage Record (Cascade)](#deactivate-usage-record-cascade)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Ingest with Dedup and Referential Integrity Check](#ingest-with-dedup-and-referential-integrity-check)
  - [Batch Partition by gts_id](#batch-partition-by-gts_id)
  - [Versioned-Marker Deactivation Cascade](#versioned-marker-deactivation-cascade)
- [4. States (CDSL)](#4-states-cdsl)
  - [Usage Record Lifecycle](#usage-record-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement create_usage_record with dedup and referential integrity](#implement-create_usage_record-with-dedup-and-referential-integrity)
  - [Implement create_usage_records (batch)](#implement-create_usage_records-batch)
  - [Implement get_usage_record](#implement-get_usage_record)
  - [Implement deactivate_usage_record cascade](#implement-deactivate_usage_record-cascade)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-record-persistence-implemented`

<!-- reference to DECOMPOSITION entry -->

- [x] `p1` - `cpt-cf-uc-ch-plugin-feature-record-persistence`

## 1. Feature Context

### 1.1 Overview

Provide the backend write plane over `usage_records`. Inserts (single and batch) use an exclusive `gts_id` coordination-lock + read-before-insert dedup check, with `ReplacingMergeTree(version)` convergence as a defense-in-depth backstop. Deactivation composes a single multi-row `INSERT` of versioned marker rows, atomically flipping the target and its depth-1 active compensations.

### 1.2 Purpose

Record Persistence owns the full lifecycle write path: locking, referential-integrity check (create side), dedup resolution, compensation persistence, and depth-1 deactivation cascade — all without any ACID transaction. Every status transition is a new versioned row; no `UPDATE` or `ALTER TABLE ... DELETE` is ever issued on the request path.

**Requirements**: `cpt-cf-uc-ch-plugin-fr-idempotent-dedup`, `cpt-cf-uc-ch-plugin-fr-deactivation`, `cpt-cf-uc-ch-plugin-nfr-ingestion-throughput`, `cpt-cf-uc-ch-plugin-fr-referential-integrity` (create-side half)

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-dedup-race-window`, `cpt-cf-uc-ch-plugin-constraint-no-in-place-update`, `cpt-cf-uc-ch-plugin-constraint-no-transactions`

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-plugin-host` | Dispatches `create_usage_record`, `create_usage_records`, `get_usage_record`, and `deactivate_usage_record` through the SPI. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §5 (Typed Error Classification, Idempotent Deduplication FR)
- **Design**: [DESIGN.md](../DESIGN.md) — §3.5 (External Dependencies), §3.6 (Ingest/Batch/Deactivation sequences), §3.7 (usage_records table), §3.8 (Consistency & Concurrency)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-record-persistence`
- **Depends on**: `cpt-cf-uc-ch-plugin-feature-foundation`
- **Sequences**: `cpt-cf-uc-ch-plugin-seq-ingest-dedup`, `cpt-cf-uc-ch-plugin-seq-ingest-batch`, `cpt-cf-uc-ch-plugin-seq-deactivate-cascade`
- **DB Table**: `cpt-cf-uc-ch-plugin-dbtable-usage-records`
- **Component**: `cpt-cf-uc-ch-plugin-component-record-store`

### 1.5 Out of Scope

- Schema DDL (`usage_records` table, `TTL` clause) — created by Feature 1 (`cpt-cf-uc-ch-plugin-feature-foundation`); this feature is the row-writer.
- Aggregation, keyset list, and pushed-down `GROUP BY` execution — Feature 3 (`cpt-cf-uc-ch-plugin-feature-query-aggregation`).
- Catalog-side lock-protected verify-then-delete protocol — Feature 4 (`cpt-cf-uc-ch-plugin-feature-usage-type-catalog`).
- `TTL` expiry of stored rows — Feature 5 (`cpt-cf-uc-ch-plugin-feature-retention`).
- Coordination Lock Manager construction — Feature 1 (`cpt-cf-uc-ch-plugin-feature-foundation`); this feature only uses the lock manager's acquire/release API.

## 2. Actor Flows (CDSL)

### Create Single Usage Record (with Dedup)

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-record-persistence-create-single`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- New record: dedup check finds no row, `INSERT` succeeds, lock released, new record returned.
- Duplicate (idempotent absorb): dedup check finds a row with identical canonical fields — silent absorb, lock released, stored record returned.

**Error Scenarios**:

- Usage type absent or previously deleted → release lock, return `UsageTypeNotFound`.
- Duplicate with differing canonical fields → release lock, return `IdempotencyConflict`.
- Lock-manager unavailable or `lock_timeout_secs` exceeded → return `Transient` without locking.
- ClickHouse error during check or insert → release lock, classify and return error.

**Steps**:

1. [ ] - `p1` - Compute the deterministic record `id` (ADR-0013/ADR-0014 4-tuple: `tenant_id`, `gts_id`, `idempotency_key`, `created_at`) - `inst-ch-rec-create-1`
2. [ ] - `p1` - Acquire the exclusive `gts_id` coordination lock via `LockManager::acquire_exclusive_for_create(gts_id)` — timeout → `Transient` - `inst-ch-rec-create-2`
3. [ ] - `p1` - Plugin-owned referential-integrity check: `SELECT 1 FINAL FROM usage_type_catalog WHERE gts_id = ?` — absent → release lock, **RETURN** `UsageTypeNotFound` - `inst-ch-rec-create-3`
4. [ ] - `p1` - Dedup point-lookup: `SELECT ... FINAL FROM usage_records WHERE tenant_id=? AND gts_id=? AND created_at=? AND id=?` - `inst-ch-rec-create-4`
5. [ ] - `p1` - **IF** not found — proceed to insert - `inst-ch-rec-create-5`
   1. [ ] - `p1` - Call `ClusterLockGuard::ensure_still_held()` (lease renew) immediately before the INSERT; on `ClusterError::LockExpired` → release the lock, **RETURN** `Transient` (cluster ADR-002 deviation) - `inst-ch-rec-create-5a`
   2. [ ] - `p1` - `INSERT` one row with `status='active'`, `version=<monotonic epoch_μs>` - `inst-ch-rec-create-5b`
   3. [ ] - `p1` - Release the lock; **RETURN** the new record - `inst-ch-rec-create-5c`
6. [ ] - `p1` - **IF** found, canonical fields equal — silent absorb: release lock, **RETURN** the stored record - `inst-ch-rec-create-6`
7. [ ] - `p1` - **IF** found, canonical fields differ — release lock, **RETURN** `IdempotencyConflict` - `inst-ch-rec-create-7`

### Create Batch of Usage Records

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-record-persistence-create-batch`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- All records inserted/absorbed in a single multi-row `INSERT`; per-record outcomes returned in input order.

**Error Scenarios**:

- A partition whose lock acquisition or catalog check fails contributes only `Err` outcomes for its own records; every other partition is unaffected.

**Steps**:

1. [ ] - `p1` - Partition the input batch by `gts_id`, then sort the partition keys so every caller acquires multi-`gts_id` locks in one global order (two concurrent mixed batches cannot deadlock on opposite acquisition orders) - `inst-ch-rec-batch-1`
2. [ ] - `p1` - Acquire every partition's exclusive `gts_id` lock up front, sequentially in the sorted order; on failure for a partition mark every record in it with the appropriate error and continue with the rest - `inst-ch-rec-batch-2`
3. [ ] - `p1` - For each locked partition, concurrently (every partition already holds its own lock): run the catalog existence check and a single batched dedup pre-check `SELECT ... FINAL WHERE (tenant_id, gts_id, created_at, idempotency_key) IN (...)` for that partition's records - `inst-ch-rec-batch-3`
4. [ ] - `p1` - Compute per-record dedup outcome (new / absorb / conflict) sequentially, partition by partition, in input order from the pre-check results, so `version` offsets and within-batch dedup stay deterministic - `inst-ch-rec-batch-4`
5. [ ] - `p1` - Renew every surviving partition's lease (`ensure_still_held`) and drop an expired partition's composed rows, then pool all remaining non-duplicate `INSERT`-ready rows into one multi-row `INSERT` and execute it - `inst-ch-rec-batch-5`
6. [ ] - `p1` - Release all held partition locks, including on the insert-failure path - `inst-ch-rec-batch-6`
7. [ ] - `p1` - **RETURN** per-record outcome vector aligned to input order (an insert failure is reported per affected slot, not as a top-level error) - `inst-ch-rec-batch-7`

### Get Usage Record

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-record-persistence-get`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p1` - `SELECT ... FINAL FROM usage_records WHERE id = ?` (no lock required for a point read) - `inst-ch-rec-get-1`
2. [ ] - `p1` - **IF** not found — **RETURN** `UsageRecordNotFound` - `inst-ch-rec-get-2`
3. [ ] - `p1` - **RETURN** the found record - `inst-ch-rec-get-3`

### Deactivate Usage Record (Cascade)

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-record-persistence-deactivate`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- Target and all depth-1 active compensations are flipped to `inactive` atomically in a single `INSERT`.

**Error Scenarios**:

- Target not found → `UsageRecordNotFound`.
- Target already inactive → `UsageRecordAlreadyInactive`.

**Steps**:

1. [ ] - `p1` - `SELECT ... FINAL WHERE id=? OR (corrects_id=? AND status='active')` — resolve target status and depth-1 active compensations in one unlocked read - `inst-ch-rec-deact-1`
2. [ ] - `p1` - **IF** target not found → **RETURN** `UsageRecordNotFound` - `inst-ch-rec-deact-2`
3. [ ] - `p1` - **IF** target already `inactive` → **RETURN** `UsageRecordAlreadyInactive` - `inst-ch-rec-deact-3`
4. [ ] - `p1` - Compose one versioned marker row per affected `id` (target + compensations), each with `status='inactive'` and `version` strictly greater than the superseded row - `inst-ch-rec-deact-4`
5. [ ] - `p1` - Issue one multi-row `INSERT` for all marker rows (atomic single part write) - `inst-ch-rec-deact-5`
6. [ ] - `p1` - **RETURN** success - `inst-ch-rec-deact-6`

## 3. Processes / Business Logic (CDSL)

### Ingest with Dedup and Referential Integrity Check

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-record-persistence-ingest-dedup`

The create-side referential-integrity half of `cpt-cf-uc-ch-plugin-fr-referential-integrity`: the exclusive `gts_id` coordination lock (same exclusive mutex name as the delete path) is held around the catalog-existence check and the dedup check/insert. Because no concurrent create or delete for the same `gts_id` can hold the lock simultaneously, the catalog check and the eventual `INSERT` are ordered against every `delete_usage_type` call for this `gts_id` with no gap — this satisfies `plugin-spi.md` Method 9's "MUST NOT admit a window" without qualification. The `ReplacingMergeTree(version)` convergence backstop is a defense-in-depth layer, not a primary integrity mechanism.

### Batch Partition by gts_id

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-record-persistence-batch-partition`

The batch MUST be partitioned by `gts_id` before any lock acquisition. Acquiring only `records[0].gts_id`'s lock for the whole batch would expose every non-first `gts_id` to a concurrent `delete_usage_type` referential-integrity race. Partition keys MUST be **sorted** before acquiring their locks: with unordered acquisition, two concurrent batches covering the same two `gts_id`s in opposite orders can hold one lock each and block on the other until both leases lapse, so one global order is what rules that deadlock out. Each distinct `gts_id` partition runs its own acquire → catalog-check → dedup-pre-check sequence; because the locks are all taken before any read, the per-partition read phases are independent and run concurrently, while outcome resolution stays sequential to keep `version` offsets and within-batch dedup deterministic. Only the physical write is batched: all non-duplicate rows from all passing partitions are collected into one multi-row `INSERT`. Each partition's lock is held until after the combined `INSERT` completes.

### Versioned-Marker Deactivation Cascade

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-record-persistence-deactivation-cascade`

Every `usage_records` status transition is a new versioned row — never an `UPDATE` or `ALTER TABLE ... DELETE`. The deactivation cascade composes one marker row per affected `id` (target + depth-1 active compensations), each with a `version` strictly greater than the row it supersedes, and issues them as a single multi-row `INSERT`. A reader using `FINAL` observes either the pre-cascade state or the fully-flipped state — never a partial cascade. There is no late-compensation race: the gateway rejects a compensation against a record being deactivated before dispatching it to the plugin (`plugin-spi.md` Method 5 caller-side concurrency rule).

## 4. States (CDSL)

### Usage Record Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-state-record-status`

| State | Description |
| --- | --- |
| `active` | Record is in scope for all reads, aggregations, and list queries. |
| `inactive` | Record has been deactivated; it is excluded from `status='active'` filter reads. It remains in storage until the TTL clause expires the row (Feature 5). |

**Transition**: `active → inactive` via `deactivate_usage_record`. Status is carried as a versioned column on the row; `FINAL` resolution yields the highest-version row per sort key.

## 5. Definitions of Done

### Implement create_usage_record with dedup and referential integrity

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-create-single`

The system **MUST** implement `create_usage_record` as: acquire the exclusive `gts_id` coordination lock, run the catalog-existence check, run the dedup lookup on the canonical `(tenant_id, gts_id, created_at, idempotency_key)` tuple against `usage_records`, and on a new record **MUST** call `ClusterLockGuard::ensure_still_held()` (lease renew) immediately before the INSERT (on `ClusterError::LockExpired` → release the lock, return `Transient`), then issue one `INSERT` with `status='active'` and a monotonic `version`. On absorb, return the stored record without inserting. On conflict, return `IdempotencyConflict`. On lock-manager unavailability or timeout, return `Transient`. Lock **MUST** be released on every exit path.

**Implements**: `cpt-cf-uc-ch-plugin-algo-record-persistence-ingest-dedup`, `cpt-cf-uc-ch-plugin-flow-record-persistence-create-single`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-ingest-dedup`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-record-store`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-records`, `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

### Implement create_usage_records (batch)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-create-batch`

The system **MUST** implement `create_usage_records` with `gts_id`-partitioned locking: one lock + catalog check per distinct `gts_id` partition, locks acquired in sorted partition-key order to preclude cross-batch deadlock, a single batched dedup pre-check `SELECT` per partition (partition read phases MAY run concurrently once all locks are held), and exactly one multi-row `INSERT` for all non-duplicate rows across passing partitions. Per-record outcomes **MUST** be returned in input order. A partition failure **MUST NOT** affect other partitions. All partition locks **MUST** be held until the combined `INSERT` completes and then released.

**Implements**: `cpt-cf-uc-ch-plugin-algo-record-persistence-batch-partition`, `cpt-cf-uc-ch-plugin-flow-record-persistence-create-batch`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-ingest-batch`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-record-store`

### Implement get_usage_record

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-get`

The system **MUST** implement `get_usage_record` as a `FINAL`-qualified point read by `id`; absent → `UsageRecordNotFound`.

**Implements**: `cpt-cf-uc-ch-plugin-flow-record-persistence-get`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-record-store`

### Implement deactivate_usage_record cascade

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-deactivate`

The system **MUST** implement `deactivate_usage_record` as: one `FINAL`-qualified read to resolve the target's current status plus all depth-1 active compensations, then one multi-row `INSERT` of versioned marker rows with `status='inactive'`. No lock is acquired (the cascade is unlocked). Target not found → `UsageRecordNotFound`; target already `inactive` → `UsageRecordAlreadyInactive`.

**Implements**: `cpt-cf-uc-ch-plugin-algo-record-persistence-deactivation-cascade`, `cpt-cf-uc-ch-plugin-flow-record-persistence-deactivate`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-deactivate-cascade`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-record-store`

## 6. Acceptance Criteria

- [x] `create_usage_record` acquires the exclusive `gts_id` coordination lock before the catalog check; lock-manager unavailability returns `Transient`.
- [x] The catalog-existence check runs while holding the exclusive lock; an absent or previously-deleted type returns `UsageTypeNotFound` with the lock released.
- [x] The dedup lookup keys on the canonical `(tenant_id, gts_id, created_at, idempotency_key)` tuple — leading with the three-column sort-key prefix, never on `id` — and is `FINAL`-qualified.
- [x] An identical re-submission (same canonical fields) is absorbed silently; a re-submission with differing canonical fields returns `IdempotencyConflict`.
- [x] `create_usage_record` calls `ClusterLockGuard::ensure_still_held()` (lease renew) immediately before the INSERT; on `ClusterError::LockExpired` the lock is released and `Transient` is returned without inserting.
- [x] `create_usage_records` partitions the batch by `gts_id` and acquires the partition locks in sorted key order; each distinct `gts_id` partition performs its own lock + catalog-check + dedup-pre-check; only one multi-row `INSERT` is issued across all passing partitions; per-record outcomes are in input order.
- [x] A partition-level lock or catalog failure does not affect outcomes for records in other partitions.
- [x] `deactivate_usage_record` flips the target and all depth-1 active compensations in a single multi-row `INSERT`; no partial cascade is observable by a `FINAL`-qualified reader.
- [x] `deactivate_usage_record` returns `UsageRecordNotFound` when the target does not exist and `UsageRecordAlreadyInactive` when it is already inactive.
- [x] No `UPDATE` or `ALTER TABLE ... DELETE` statement is issued on any request-path code path.
- [x] All partition locks are released on every exit path (success, error, panic via `Drop` best-effort).

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — enforcement is upstream in the gear core; every SPI call arrives already authorized (`cpt-cf-uc-ch-plugin-principle-pure-persistence`).
- **Security — Audit Trail**: Not applicable — the plugin performs no auditable user actions.
- **Data Privacy / Compliance**: Not applicable — opaque identifiers and metadata passed through verbatim.
- **Usability (UX)**: Not applicable — no user interface.
- **Observability (OPS-FDESIGN-001)**: Insert-duration histograms, batch-row-count histogram, and dedup-outcome counters are allocated to Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`); this feature provides the write-path hot path they instrument.
- **Retention / TTL**: Not applicable here — TTL clause ownership and its runtime expiry behavior belong to Feature 5 (`cpt-cf-uc-ch-plugin-feature-retention`).
