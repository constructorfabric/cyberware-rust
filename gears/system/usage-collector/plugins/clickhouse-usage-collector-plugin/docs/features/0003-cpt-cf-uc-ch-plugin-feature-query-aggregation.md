# Feature: Query & Aggregation

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Out of Scope](#15-out-of-scope)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Query Aggregated Usage Records](#query-aggregated-usage-records)
  - [List Usage Records (Keyset Paginated)](#list-usage-records-keyset-paginated)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [OData-to-ClickHouse Query Translation](#odata-to-clickhouse-query-translation)
  - [Aggregation Result Capping](#aggregation-result-capping)
  - [Keyset Cursor Encoding and Forward-Only Enforcement](#keyset-cursor-encoding-and-forward-only-enforcement)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement query_aggregated_usage_records](#implement-query_aggregated_usage_records)
  - [Implement list_usage_records with keyset pagination](#implement-list_usage_records-with-keyset-pagination)
  - [Implement OData-to-ClickHouse query translator](#implement-odata-to-clickhouse-query-translator)
  - [Document workload isolation posture](#document-workload-isolation-posture)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-query-aggregation-implemented`

<!-- reference to DECOMPOSITION entry -->

- [x] `p1` - `cpt-cf-uc-ch-plugin-feature-query-aggregation`

## 1. Feature Context

### 1.1 Overview

Provide the backend read plane, pushing aggregation into ClickHouse's vectorized execution and paginating raw reads via keyset seeking — both as single-level scans over raw rows that anti-join the deactivation-marker id set instead of resolving `ReplacingMergeTree` versions, so deactivation is exact before any merge at one hash probe per row. This is the allocation target for the aggregation query-latency NFR and the workload-isolation NFR.

### 1.2 Purpose

Query & Aggregation owns: OData `$filter`/`$orderby`/cursor → parameterized ClickHouse SQL translation (injection-safe); pushed-down single-level aggregate over the marker-anti-joined active rows with server-side `LIMIT MAX_AGGREGATION_BUCKETS + 1` cap; single-level keyset-paginated raw list over the marker-anti-joined rows with one-row look-ahead and forward-only cursor enforcement; and the operator-facing workload-isolation posture documentation (shared client/pool, the absence of any pool-size config knob, accepted contention analysis, and operational mitigation guidance).

**Requirements**: `cpt-cf-uc-ch-plugin-nfr-query-latency`, `cpt-cf-uc-ch-plugin-nfr-workload-isolation`

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-final-qualified-reads`, `cpt-cf-uc-ch-plugin-constraint-aggregation-bucket-cap`

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-plugin-host` | Dispatches `query_aggregated_usage_records` and `list_usage_records` through the SPI. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §6.1 (NFR: Query Latency, NFR: Workload Isolation — `cpt-cf-uc-ch-plugin-nfr-workload-isolation`, Aggregation Query Latency NFR — bucket-cap obligation)
- **Design**: [DESIGN.md](../DESIGN.md) — §3.5 (Workload isolation allocation), §3.6 (Aggregated Query, Keyset List sequences), §3.8 (Consistency & Concurrency — version-resolution cost tradeoff)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-query-aggregation`
- **Depends on**: `cpt-cf-uc-ch-plugin-feature-foundation`, `cpt-cf-uc-ch-plugin-feature-record-persistence`
- **Sequences**: `cpt-cf-uc-ch-plugin-seq-query-aggregated`, `cpt-cf-uc-ch-plugin-seq-list-keyset`
- **DB Table**: `cpt-cf-uc-ch-plugin-dbtable-usage-records` (reader; written by Feature 2)
- **Component**: Query Translator (`infra/storage/query/*`); query execution lives in the Record Store component (`cpt-cf-uc-ch-plugin-component-record-store`)

### 1.5 Out of Scope

- Writing, dedup, or deactivation — Feature 2 (`cpt-cf-uc-ch-plugin-feature-record-persistence`).
- Catalog listing keyset pagination — reuses this pattern but owned by Feature 4 (`cpt-cf-uc-ch-plugin-feature-usage-type-catalog`).
- ClickHouse client and pool construction — Feature 1 (`cpt-cf-uc-ch-plugin-feature-foundation`); this feature owns the isolation analysis and read-path behavior, not the client object.
- Metric instrumentation for query latency — Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`).

## 2. Actor Flows (CDSL)

### Query Aggregated Usage Records

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-query-aggregation-aggregate`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- Query executes pushed-down aggregate and returns grouped results within `MAX_AGGREGATION_BUCKETS` (100,000).

**Error Scenarios**:

- Over-bucket result: gateway detects `result.len() > MAX_AGGREGATION_BUCKETS` from the `+1` look-ahead row; plugin returns the full capped `MAX_AGGREGATION_BUCKETS + 1` row set for the gateway to apply `400 AGGREGATION_RESULT_TOO_LARGE`.
- ClickHouse error — classify and return.

**Steps**:

1. [ ] - `p1` - Translate the `AggregationSpec` (dimensions, operations, filters, metadata filters) to a parameterized ClickHouse `SELECT` using the Query Translator (`cpt-cf-uc-ch-plugin-algo-query-translator`) - `inst-ch-agg-1`
2. [ ] - `p1` - Apply `SUM`-nets-compensations rule (`corrects_id IS NOT NULL` excluded for non-SUM ops); apply the survivor predicate `status = 'active' AND id NOT IN (SELECT id FROM usage_records WHERE <scan> AND status = 'inactive')` (`dedup::active_survivors`), so markers and the active rows they supersede never reach the aggregate - `inst-ch-agg-2`
3. [ ] - `p1` - Append `LIMIT {MAX_AGGREGATION_BUCKETS + 1}` (100,001) to the generated SQL to cap server-side materialization - `inst-ch-agg-3`
4. [ ] - `p1` - Execute as a single-level query over raw `usage_records` rows: every version-invariant `$filter` conjunct joins the scan predicate, which is rendered twice — as the scan `WHERE` and verbatim inside the marker subquery — so `tenant_id` and `created_at` prune both on the sorting-key prefix; no version-resolving `GROUP BY`, `LIMIT 1 BY` or `FINAL` - `inst-ch-agg-4`
   1. [ ] - `p1` - Append the `$filter`'s `status`-naming conjuncts after the survivor predicate in the same `WHERE`; they read the raw `status`, which on every surviving row equals the resolved status - `inst-ch-agg-4a`
   2. [ ] - `p1` - Bind `gts_id` and the scan binds twice (once per copy of the scan text), then the trailing binds, matching the left-to-right `?` order - `inst-ch-agg-4b`
5. [ ] - `p1` - **RETURN** the result rows (caller — the gateway — inspects `len() > MAX_AGGREGATION_BUCKETS` to apply the over-bucket error) - `inst-ch-agg-5`

### List Usage Records (Keyset Paginated)

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-flow-query-aggregation-list`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- Page of up to `n` records returned with an encoded next-cursor; no next-cursor on last page.

**Error Scenarios**:

- Backward-cursor supplied → `InvalidCursor` (forward-only enforcement).
- ClickHouse error — classify and return.

**Steps**:

1. [ ] - `p1` - Translate the OData `$filter`, `$orderby`, and keyset cursor to parameterized ClickHouse `WHERE ... ORDER BY ... LIMIT <n+1>` via the Query Translator - `inst-ch-list-1`
2. [ ] - `p1` - Enforce forward-only cursor (`ensure_forward_cursor`): reject a cursor whose position is before the current sort-key anchor - `inst-ch-list-2`
3. [ ] - `p1` - Execute as a single-level query over raw rows with the survivor predicate `(status = 'inactive' OR id NOT IN (SELECT id … WHERE <scan> AND status = 'inactive'))` (`dedup::resolved_survivors`): every version-invariant `$filter` conjunct joins the scan predicate (rendered twice), the keyset predicate and the `$filter`'s `status`-naming conjuncts trail it, and the caller's `ORDER BY … LIMIT n+1` is the only sort - `inst-ch-list-3`
4. [ ] - `p1` - **IF** result contains `n+1` rows — truncate to `n`, encode the `n+1`-th row as the next-cursor - `inst-ch-list-4`
5. [ ] - `p1` - **RETURN** a `Page` of at most `n` records plus an optional next-cursor - `inst-ch-list-5`

## 3. Processes / Business Logic (CDSL)

### OData-to-ClickHouse Query Translation

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-query-translator`

**Input**: An `ODataQuery` or `AggregationSpec` AST plus a keyset cursor.

**Output**: A parameterized ClickHouse SQL fragment with bound parameter slots; no caller-controlled string is ever interpolated into the query text.

**Steps**:

1. [ ] - `p2` - For each `$filter` expression: bind caller-supplied values as parameters; validate column names against a closed allowlist of schema columns — reject any unlisted identifier; render a one-element `IN` list as `column = ?` so a sorting-key prefix column pinned by the gateway's one-tenant authorization scope counts as fixed for in-order reads and binary-search key pruning - `inst-ch-trans-1`
2. [ ] - `p2` - For each `$orderby` clause: validate column names against the same allowlist. The effective keyset order **MUST** be a **total order** — it **MUST** end in a stable globally-unique tie-breaker (`id` for `usage_records`, `gts_id` for the type catalog). Per `plugin-spi.md` Method 4 the host guarantees this by normalizing every caller `$orderby` to end in the canonical `(created_at, id)` suffix (`ODataOrderBy::ensure_tiebreaker`, in the caller's sort direction); an order whose final key is not unique **MUST NOT** be executed as a keyset query, because rows sharing the boundary value that did not fit on the previous page are silently dropped at the page break - `inst-ch-trans-2`
3. [ ] - `p2` - For a keyset cursor: decode the cursor bytes, generate a **strict** row-value predicate — `WHERE col1 >= ? AND (col1, col2, ...) > (?, ?, ...)` (or `<=` / `<` for a descending sort) — with bound parameters matching the decoded position (the first key is bound twice, in text order); the cursor names the last row already returned, so a bare `>=` tuple would re-emit it, while the leading non-strict bound on `col1` alone is implied by the tuple and exists only so the primary index can prune the granules before the cursor. The predicate's column tuple and the cursor's key tuple **MUST** span the full total order of step 2, tie-breaker included — a cursor arity that does not match the effective order is rejected - `inst-ch-trans-3`
4. [ ] - `p2` - For an `AggregationSpec`: generate `GROUP BY <dims> SELECT <agg_exprs>` honoring the `SUM`-nets-compensations / other-ops-exclude-compensations rule - `inst-ch-trans-4`
5. [ ] - `p2` - Adapt to the `clickhouse` crate's bind API (named `?` parameter slots or positional, per crate convention) - `inst-ch-trans-5`
6. [ ] - `p2` - **RETURN** the parameterized SQL fragment and parameter bindings - `inst-ch-trans-6`

### Aggregation Result Capping

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-aggregation-cap`

Per `plugin-spi.md` Method 3's pushdown obligation, the plugin **MUST** add `LIMIT {MAX_AGGREGATION_BUCKETS + 1}` (100,001) to every aggregation query before execution. The gateway's over-bucket detection logic relies on the plugin returning exactly the capped `MAX_AGGREGATION_BUCKETS + 1` rows when the true result exceeds the cap — never fewer (the gateway would then incorrectly pass the result through) and never more (the plugin must not materialize an unbounded result set). This cap is a hard contract between the plugin and the gateway.

### Keyset Cursor Encoding and Forward-Only Enforcement

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-keyset-cursor`

Cursors encode the sort-key values of the last row of the returned page, opaque to callers. The encoded key tuple covers the **whole** effective order including its unique final tie-breaker (`id` for records, `gts_id` for the catalog), so the keyset sequence is a total order and no two distinct rows share a cursor position. Decoding yields a typed position that is injected into the `WHERE` clause as bound parameters. The forward-only cursor constraint (`ensure_forward_cursor`) is a v1 constraint: a cursor whose decoded position is prior to the current page's first row is rejected with `InvalidCursor` (backward paging is not supported in v1).

## 4. States (CDSL)

Not applicable — this feature introduces no entity lifecycle state machine. All read operations are stateless queries over the `usage_records` table maintained by Feature 2.

## 5. Definitions of Done

### Implement query_aggregated_usage_records

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-query-aggregation-aggregate`

The system **MUST** implement `query_aggregated_usage_records` as a pushed-down single-level aggregate query over raw rows that:
- Applies the `SUM`-nets-compensations rule (`corrects_id IS NOT NULL` excluded from non-`SUM` operations).
- Applies the survivor predicate `status = 'active' AND id NOT IN (<marker ids within the same scan predicate>)` so neither a deactivation marker nor the row it supersedes is aggregated, and appends the `$filter`'s `status`-naming conjuncts after it in the same `WHERE`.
- Appends `LIMIT {MAX_AGGREGATION_BUCKETS + 1}` (100,001) server-side.
- Uses only bound parameters for caller-derived values and an allowlisted identifier set for column names.
- Returns the result rows in order for the gateway to inspect the cap.
- The aggregation-latency NFR (`≤500ms p95`, 30-day single-tenant aggregation) **MUST** be measured with the marker anti-join included.

**Implements**: `cpt-cf-uc-ch-plugin-algo-aggregation-cap`, `cpt-cf-uc-ch-plugin-flow-query-aggregation-aggregate`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-query-aggregated`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-record-store`, Query Translator
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-records`

### Implement list_usage_records with keyset pagination

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-query-aggregation-list`

The system **MUST** implement `list_usage_records` as a single-level keyset-paginated list over the marker-anti-joined rows (one physical row per logical row, at its resolved status, before any merge) that:
- Enforces forward-only cursor (`ensure_forward_cursor`).
- Uses a `n+1` look-ahead row to determine if a next-cursor exists.
- Returns at most `n` records (wire-level cap ≤ 1,000) with an opaque next-cursor when available.
- All filters and order-by columns are translated via the injection-safe Query Translator.

**Implements**: `cpt-cf-uc-ch-plugin-algo-keyset-cursor`, `cpt-cf-uc-ch-plugin-flow-query-aggregation-list`

**Sequences**: `cpt-cf-uc-ch-plugin-seq-list-keyset`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-record-store`, Query Translator

### Implement OData-to-ClickHouse query translator

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-query-aggregation-translator`

The system **MUST** implement the Query Translator (`infra/storage/query/*`) that converts OData `$filter`/`$orderby`/keyset-cursor ASTs into parameterized ClickHouse SQL. The translator **MUST** bind all caller-derived values as parameters and validate all caller-influenced identifiers against a closed allowlist — no string interpolation of untrusted input into query text is permitted on any code path.

**Implements**: `cpt-cf-uc-ch-plugin-algo-query-translator`

**Touches**: Query Translator component

### Document workload isolation posture

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-query-aggregation-workload-isolation`

The system **MUST** document in the plugin README that v1 uses one `clickhouse::Client` shared by both the ingestion and query paths (the accepted contention point), that the client's pool behavior is **not** operator-tunable because the `clickhouse` crate exposes no pool bound a config field could drive, and that operators experiencing query-burst degradation of ingestion latency MAY mitigate operationally — server-side settings profiles/quotas, or two plugin instances against read-replica vs. write-primary ClickHouse endpoints. The documentation **MUST NOT** claim workload isolation is solved for v1, and **MUST NOT** promise a pool-size config field.

**Constraints**: `cpt-cf-uc-ch-plugin-nfr-workload-isolation`

## 6. Acceptance Criteria

- [x] `query_aggregated_usage_records` is a single-level scan: it applies `status = 'active' AND id NOT IN (<marker ids>)` with the `$filter`'s version-invariant conjuncts in both the scan and the marker subquery and its `status`-naming conjuncts trailing, carries no version-resolving `GROUP BY`/`LIMIT 1 BY`/`FINAL` (`EXPLAIN PIPELINE` shows at most the caller's own `Aggregating` step), applies `LIMIT 100001` server-side, and correctly applies the `SUM`-nets-compensations vs. other-ops-exclude-compensations rule. Every aggregation op (`SUM`, `COUNT`, `MIN`, `MAX`, `AVG`) excludes a deactivated record, and a group whose only record is deactivated disappears rather than surfacing as a zero-valued bucket.
- [x] The aggregation-latency NFR budget is verified with the marker anti-join included (not measured around it).
- [x] `list_usage_records` is a single-level scan with the `(status = 'inactive' OR id NOT IN (<marker ids>))` survivor predicate: a deactivated record appears exactly once, as inactive, before any merge; the keyset predicate and the `$filter`'s `status`-naming conjuncts trail the survivor predicate and the rest of the `$filter` prunes both scan copies; the caller's `ORDER BY … LIMIT` is the only sort (`EXPLAIN PIPELINE` shows one `Sorting` step and no `LimitBy`); uses the `n+1` look-ahead cursor pattern, and enforces forward-only cursor — a backward cursor returns `InvalidCursor`.
- [x] The Query Translator uses bound parameters for all caller-derived values; no caller-controlled identifier is interpolated into query text; only allowlisted column names are accepted.
- [x] Aggregation results contain only rows with `status='active'` (deactivated records are excluded from active aggregation). Raw `list_usage_records` results are deliberately **status-agnostic**: `plugin-spi.md` Method 5 directs callers to enumerate a deactivation cascade through a follow-up list filtered on `status` / `corrects_id`, so the list path applies no `status` predicate of its own (matching the reference plugin).
- [x] The plugin README documents the shared-client workload-isolation posture and the two-instance operational mitigation.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — enforcement is upstream; this feature's security obligation is injection safety (bound parameters, identifier allowlist), which is addressed in the Acceptance Criteria.
- **Security — Audit Trail**: Not applicable.
- **Data Privacy / Compliance**: Not applicable — opaque identifiers and metadata passed through verbatim.
- **Usability (UX)**: Not applicable — no user interface.
- **Observability (OPS-FDESIGN-001)**: Query-duration histograms and request-count metrics are allocated to Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`); this feature provides the read-path hot path they instrument.
- **Write operations**: Not applicable — this feature owns no write path; writing belongs to Feature 2.
