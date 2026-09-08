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

Provide the backend write plane over `usage_records`. Inserts (single and batch) are an unsynchronised read-before-insert dedup check followed by an `INSERT`; racing duplicates are caught by the engine's `insert_deduplication_token` window, with `ReplacingMergeTree(version)` convergence as the backstop. Deactivation composes a single multi-row `INSERT` of versioned marker rows, atomically flipping the target and its depth-1 active compensations.

### 1.2 Purpose

Record Persistence owns the full lifecycle write path: referential-integrity check (create side), dedup resolution, compensation persistence, and depth-1 deactivation cascade — all without any ACID transaction and without any coordination primitive. Every status transition is a new versioned row; no `UPDATE` or `ALTER TABLE ... DELETE` is ever issued on the request path.

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
- Usage-type create / get / list, and why delete is withheld — Feature 4 (`cpt-cf-uc-ch-plugin-feature-usage-type-catalog`). This feature's insert-time catalog check relies on that feature's append-only guarantee.
- `TTL` expiry of stored rows — Feature 5 (`cpt-cf-uc-ch-plugin-feature-retention`).

## 2. Actor Flows (CDSL)

### Create Single Usage Record (with Dedup)

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-record-persistence-create-single`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- New record: dedup check finds no row, `INSERT` succeeds, new record returned.
- Duplicate (idempotent absorb): dedup check finds a row with identical canonical fields — silent absorb, stored record returned.

**Error Scenarios**:

- Usage type absent → return `UsageTypeNotFound`. (A type is never removed once created, so "previously deleted" cannot arise — Feature 4.)
- Duplicate with differing canonical fields → return `IdempotencyConflict`.
- ClickHouse error during check or insert → classify and return the error.

**Steps**:

1. [ ] - `p1` - Compute the deterministic record `id` (ADR-0013/ADR-0014 4-tuple: `tenant_id`, `gts_id`, `idempotency_key`, `created_at`) - `inst-ch-rec-create-1`
2. [ ] - ~~`p1` - Acquire the exclusive `gts_id` coordination lock~~ — **superseded**: there is no coordination primitive; the three statements below are independent and unordered against a concurrent create for the same dedup key (DESIGN.md §3.6) - `inst-ch-rec-create-2`
3. [ ] - `p1` - Plugin-owned referential-integrity check: `SELECT gts_id FROM usage_type_catalog WHERE gts_id = ? LIMIT 1` (existence is version-invariant, so no resolution is needed) — absent → **RETURN** `UsageTypeNotFound` - `inst-ch-rec-create-3`
4. [ ] - `p1` - Dedup point-lookup: `SELECT ... FROM usage_records WHERE tenant_id=? AND gts_id=? AND created_at=? AND idempotency_key=? ORDER BY id ASC, version DESC LIMIT 1 BY id` - `inst-ch-rec-create-4`
5. [ ] - `p1` - **IF** not found — proceed to insert - `inst-ch-rec-create-5`
   1. [ ] - ~~`p1` - Call `ClusterLockGuard::ensure_still_held()` (lease renew) immediately before the INSERT~~ — **superseded**: no lease exists to renew - `inst-ch-rec-create-5a`
   2. [ ] - `p1` - `INSERT` one row with `status='active'`, `version=<monotonic epoch_μs>`, carrying `insert_deduplication_token = UUIDv5(record namespace, id)` so a racing identical insert is dropped by the engine on a synchronous insert (`non_replicated_deduplication_window`; async inserts are deduplicated only on `Replicated*` engines) - `inst-ch-rec-create-5b`
   3. [ ] - `p1` - **RETURN** the new record - `inst-ch-rec-create-5c`
6. [ ] - `p1` - **IF** found, canonical fields equal — silent absorb: **RETURN** the stored record - `inst-ch-rec-create-6`
7. [ ] - `p1` - **IF** found, canonical fields differ — **RETURN** `IdempotencyConflict` - `inst-ch-rec-create-7`

### Create Batch of Usage Records

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-record-persistence-create-batch`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- All records inserted/absorbed through exactly three statements, however many usage types the batch spans: one catalog existence query, one dedup pre-read, one multi-row `INSERT`; per-record outcomes returned in input order.

**Error Scenarios**:

- A failed catalog read decides nothing for anyone, so every slot carries it. A failed dedup read fails only the slots that passed the catalog check. A failed `INSERT` fails only the slots backed by a composed row, so outcomes already decided from storage are kept.

**Steps**:

1. [ ] - `p1` - Collect the batch's distinct `gts_id`s into a sorted set, so the placeholder and bind sequences of step 2 are deterministic rather than hash-map-iteration dependent - `inst-ch-rec-batch-1`
2. [ ] - `p1` - One catalog existence query over that set: `SELECT gts_id FROM usage_type_catalog WHERE gts_id IN (?, …)`. Every record whose `gts_id` is absent from the result gets `UsageTypeNotFound` in its own slot; a failure of the read itself is written to every slot - `inst-ch-rec-batch-2`
3. [ ] - `p1` - One batched dedup pre-read over every record that passed step 2: `SELECT ... WHERE (gts_id, tenant_id, created_at, idempotency_key) IN (...) ORDER BY version DESC LIMIT 1 BY gts_id, tenant_id, created_at, id` - `inst-ch-rec-batch-3`
4. [ ] - `p1` - Compute each passed record's dedup outcome (new / absorb / conflict) in input order, versioning each composed row `current_merge_version() + <rows composed so far>` so the batch's versions are distinct and increasing. Within-batch dedup is global to the batch: the canonical dedup tuple keys the composition map, so two records sharing a tuple collide here and the second is a conflict - `inst-ch-rec-batch-4`
5. [ ] - `p1` - One multi-row `INSERT` of the composed rows — a no-op when nothing was composed — carrying `insert_deduplication_token = UUIDv5(record namespace, sorted row ids)` so an identical racing batch is dropped by the engine. Always synchronous, whatever `async_insert` says, because the statement's rows must become visible together - `inst-ch-rec-batch-5`
6. [ ] - `p1` - On `INSERT` failure, write the error into exactly the slots backed by a composed row; slots absorbed from storage keep the outcome step 4 decided - `inst-ch-rec-batch-6`
7. [ ] - `p1` - **RETURN** per-record outcome vector aligned to input order (an insert failure is reported per affected slot, not as a top-level error) - `inst-ch-rec-batch-7`

### Get Usage Record

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-record-persistence-get`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Steps**:

1. [ ] - `p1` - `SELECT ... FROM usage_records WHERE id = ? ORDER BY version DESC LIMIT 1 BY gts_id, tenant_id, created_at, id` (no lock required for a point read) - `inst-ch-rec-get-1`
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

1. [ ] - `p1` - `SELECT ... FROM (SELECT ... WHERE id=? OR corrects_id=? ORDER BY version DESC LIMIT 1 BY gts_id, tenant_id, created_at, id) WHERE id=? OR (corrects_id=? AND status='active')` — resolve target status and depth-1 active compensations in one unlocked read; the `status` half of the predicate has to sit above the resolution step - `inst-ch-rec-deact-1`
2. [ ] - `p1` - **IF** target not found → **RETURN** `UsageRecordNotFound` - `inst-ch-rec-deact-2`
3. [ ] - `p1` - **IF** target already `inactive` → **RETURN** `UsageRecordAlreadyInactive` - `inst-ch-rec-deact-3`
4. [ ] - `p1` - Compose one versioned marker row per affected `id` (target + compensations), each with `status='inactive'` and `version` strictly greater than the superseded row - `inst-ch-rec-deact-4`
5. [ ] - `p1` - Issue one multi-row `INSERT` for all marker rows (atomic single part write), carrying `insert_deduplication_token = UUIDv5(marker namespace, sorted marker ids)` — the marker namespace is what keeps the engine from dropping the cascade as a retry of the create that wrote the same ids - `inst-ch-rec-deact-5`
6. [ ] - `p1` - **RETURN** success - `inst-ch-rec-deact-6`

## 3. Processes / Business Logic (CDSL)

### Ingest with Dedup and Referential Integrity Check

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-record-persistence-ingest-dedup`

The create-side referential-integrity half of `cpt-cf-uc-ch-plugin-fr-referential-integrity`: the exclusive `gts_id` coordination lock (same exclusive mutex name as the delete path) is held around the catalog-existence check and the dedup check/insert. Because no concurrent create or delete for the same `gts_id` can hold the lock simultaneously, the catalog check and the eventual `INSERT` are ordered against every `delete_usage_type` call for this `gts_id` with no gap — this satisfies `plugin-spi.md` Method 9's "MUST NOT admit a window" without qualification. The `ReplacingMergeTree(version)` convergence backstop is a defense-in-depth layer, not a primary integrity mechanism.

### Batch Partition by gts_id

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-record-persistence-batch-partition`

The batch MUST be resolved with exactly three ClickHouse statements — one catalog existence query over the distinct `gts_id`s, one dedup pre-read over the records that passed it, one multi-row `INSERT` of the composed rows — regardless of how many usage types it spans. This replaces the former per-`gts_id` partition fan-out, which existed only so each partition could take its own coordination lock; with no lock there is nothing to partition for, and three statements is strictly fewer round-trips than one pipeline per distinct type. Within-batch dedup stays correct because the composition map is keyed on the canonical dedup tuple, which is global to the batch rather than partition-local. Failure granularity is per slot: a failed catalog read fails every slot, a failed dedup read fails the slots that passed the catalog check, and a failed `INSERT` fails only the slots backed by a composed row.

### Versioned-Marker Deactivation Cascade

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-record-persistence-deactivation-cascade`

Every `usage_records` status transition is a new versioned row — never an `UPDATE` or `ALTER TABLE ... DELETE`. The deactivation cascade composes one marker row per affected `id` (target + depth-1 active compensations), each with a `version` strictly greater than the row it supersedes, and issues them as a single multi-row `INSERT`. A version-resolving reader observes either the pre-cascade state or the fully-flipped state — never a partial cascade. There is no late-compensation race: the gateway rejects a compensation against a record being deactivated before dispatching it to the plugin (`plugin-spi.md` Method 5 caller-side concurrency rule).

## 4. States (CDSL)

### Usage Record Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-state-record-status`

| State | Description |
| --- | --- |
| `active` | Record is in scope for all reads, aggregations, and list queries. |
| `inactive` | Record has been deactivated; it is excluded from `status='active'` filter reads. It remains in storage until the TTL clause expires the row (Feature 5). |

**Transition**: `active → inactive` via `deactivate_usage_record`. Status is carried as a versioned column on the row; read-time resolution yields the highest-version row per sort key. `status` is therefore the one column a predicate may not be applied below the resolution step.

## 5. Definitions of Done

### Implement create_usage_record with dedup and referential integrity

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-create-single`

The system **MUST** implement `create_usage_record` as: run the catalog-existence check, run the dedup lookup on the canonical `(tenant_id, gts_id, created_at, idempotency_key)` tuple against `usage_records` (emitted in sorting-key order, `gts_id` first), and on a new record issue one `INSERT` with `status='active'` and a monotonic `version`, carrying an `insert_deduplication_token`. On absorb, return the stored record without inserting. On conflict, return `IdempotencyConflict`. The three statements are independent — no coordination primitive orders them against a concurrent create for the same dedup key, and the resulting window is stated in DESIGN.md §3.6.

**Implements**: `cpt-cf-uc-ch-plugin-algo-record-persistence-ingest-dedup`, `cpt-cf-uc-ch-plugin-flow-record-persistence-create-single`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-ingest-dedup`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-record-store`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-records`, `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

### Implement create_usage_records (batch)

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-create-batch`

The system **MUST** implement `create_usage_records` with `gts_id`-partitioned locking: one lock + catalog check per distinct `gts_id` partition, a single batched dedup pre-check `SELECT` per partition, and one multi-row `INSERT` of that partition's non-duplicate rows. Each partition's pipeline (acquire → catalog check → dedup pre-check → resolve → `ensure_still_held` → `INSERT` → release) **MUST** run concurrently with the other partitions', and its lock **MUST** be held for its own critical section only — acquired no earlier than that partition's own work and released as soon as its own `INSERT` completes, on every exit path. A partition **MUST NOT** hold more than one lock at a time (which is what precludes cross-batch deadlock). Per-record outcomes **MUST** be returned in input order. A partition failure **MUST NOT** affect other partitions.

**Implements**: `cpt-cf-uc-ch-plugin-algo-record-persistence-batch-partition`, `cpt-cf-uc-ch-plugin-flow-record-persistence-create-batch`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-ingest-batch`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-record-store`

### Use server-side asynchronous inserts for usage_records

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-async-insert`

The system **MUST** send every **single-row** `usage_records` `INSERT` with `async_insert = 1` and `wait_for_async_insert = 1` when the `async_insert` config field is enabled (the default), applied per statement rather than on the shared client so no read path is affected. This is what stops the one-`INSERT`-per-`create_usage_record` write path forming a part per record.

`wait_for_async_insert` **MUST NOT** be configurable or set to `0`: both the durability acknowledgement and `create_usage_record`'s dependence on its own dedup pre-read observing a prior insert rest on it.

Multi-row `usage_records` `INSERT`s — `create_usage_records` and the deactivation cascade — **MUST** remain synchronous regardless of the config value, because the server-side buffer does not guarantee that one statement's rows become visible in a single commit, and both depend on that. All `usage_type_catalog` writes **MUST** likewise remain synchronous.

Startup validation **MUST** reject a `request_timeout_secs` too small to absorb the server-side buffer-flush wait while `async_insert` is enabled.

Every `usage_records` `INSERT` — single, batch, or marker — **MUST** carry an `insert_deduplication_token` (`record_store::insert_dedup_token`: UUIDv5 of the sorted, deduplicated row ids under a per-insert-kind namespace), and asynchronous inserts **MUST** also carry `async_insert_deduplicate = 1`. The table **MUST** keep `non_replicated_deduplication_window = 10000`, reconciled on startup by `ensure_insert_dedup_window`. The documented consequence: on a synchronous insert a racing identical write is dropped by the engine (first-writer-wins); with `async_insert` enabled on a non-replicated table the single-record path is deduplicated by `optimize_on_insert` within one flush and by merge otherwise, so `list`/`aggregate` may see such a twin twice until the merge.

**Implements**: `cpt-cf-uc-ch-plugin-flow-record-persistence-create-single`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-ingest-dedup`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-record-store`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-records`

### Implement get_usage_record

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-get`

The system **MUST** implement `get_usage_record` as a version-resolved point read by `id`; absent → `UsageRecordNotFound`.

**Implements**: `cpt-cf-uc-ch-plugin-flow-record-persistence-get`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-record-store`

### Implement deactivate_usage_record cascade

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-record-persistence-deactivate`

The system **MUST** implement `deactivate_usage_record` as: one version-resolved read to resolve the target's current status plus all depth-1 active compensations, with the `status = 'active'` predicate applied above the resolution step, then one multi-row `INSERT` of versioned marker rows with `status='inactive'`. No lock is acquired (the cascade is unlocked). Target not found → `UsageRecordNotFound`; target already `inactive` → `UsageRecordAlreadyInactive`.

**Implements**: `cpt-cf-uc-ch-plugin-algo-record-persistence-deactivation-cascade`, `cpt-cf-uc-ch-plugin-flow-record-persistence-deactivate`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-deactivate-cascade`

**Touches**: Component: `cpt-cf-uc-ch-plugin-component-record-store`

## 6. Acceptance Criteria

- [ ] Single-row `usage_records` `INSERT`s carry `async_insert = 1` and `wait_for_async_insert = 1` (verifiable in `system.query_log`), while multi-row `INSERT`s and every `usage_type_catalog` write carry neither. Every `usage_records` `INSERT` carries a UUID `insert_deduplication_token`; on a synchronous store two racing identical single creates, or two racing identical batches, store exactly one physical row per id, and a deactivation marker is stored next to the row it supersedes rather than deduplicated against it. An exact create retry is still absorbed rather than inserting a second row, which is what proves the acknowledgement implies queryability.
- [x] `create_usage_record` takes no lock of any kind: catalog check, dedup pre-read and `INSERT` are three independent statements.
- [x] The catalog-existence check is `SELECT gts_id FROM usage_type_catalog WHERE gts_id = ? LIMIT 1`; an absent type returns `UsageTypeNotFound`. A type cannot be *previously deleted* — this backend never removes one.
- [x] The dedup lookup keys on the canonical `(tenant_id, gts_id, created_at, idempotency_key)` tuple — emitted in sorting-key order (`gts_id` first) so it leads with the three-column sort-key prefix, never on `id` — and resolves versions per `id` (`ORDER BY id ASC, version DESC LIMIT 1 BY id`).
- [x] An identical re-submission (same canonical fields) is absorbed silently; a re-submission with differing canonical fields returns `IdempotencyConflict`.
- [x] No lease-renew step exists on the insert path; the engine's `insert_deduplication_token` window, not a lock, is what makes a racing retry of the same row a no-op.
- [x] `create_usage_records` issues exactly three statements however many usage types the batch spans — one catalog existence query, one dedup pre-read, one multi-row `INSERT` — with per-record outcomes in input order.
- [x] A failed `INSERT` is reported only in the slots backed by a composed row; slots absorbed from storage keep the outcome the dedup pre-read decided.
- [x] `deactivate_usage_record` flips the target and all depth-1 active compensations in a single multi-row `INSERT`; no partial cascade is observable by a version-resolving reader.
- [x] `deactivate_usage_record` returns `UsageRecordNotFound` when the target does not exist and `UsageRecordAlreadyInactive` when it is already inactive.
- [x] No `UPDATE` or `ALTER TABLE ... DELETE` statement is issued on any request-path code path.
- [x] No lock is acquired anywhere on the write path, so no release path exists to get wrong.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — enforcement is upstream in the gear core; every SPI call arrives already authorized (`cpt-cf-uc-ch-plugin-principle-pure-persistence`).
- **Security — Audit Trail**: Not applicable — the plugin performs no auditable user actions.
- **Data Privacy / Compliance**: Not applicable — opaque identifiers and metadata passed through verbatim.
- **Usability (UX)**: Not applicable — no user interface.
- **Observability (OPS-FDESIGN-001)**: Insert-duration histograms, batch-row-count histogram, and dedup-outcome counters are allocated to Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`); this feature provides the write-path hot path they instrument.
- **Retention / TTL**: Not applicable here — TTL clause ownership and its runtime expiry behavior belong to Feature 5 (`cpt-cf-uc-ch-plugin-feature-retention`).
