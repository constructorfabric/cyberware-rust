-- ClickHouse Usage Collector Plugin — initial schema provisioning (v1).
--
-- Uses idempotent CREATE TABLE IF NOT EXISTS so this file is safe to re-run
-- on concurrent replica startup.  ClickHouse has no pg_advisory_lock
-- equivalent; unlike the reference plugin (TimescaleDB), no advisory-lock
-- serialises concurrent init runs here — idempotent DDL alone is sufficient
-- because CREATE TABLE IF NOT EXISTS is internally atomic in ClickHouse.
--
-- The usage_records TTL clause below uses a fixed 1-year default
-- (INTERVAL 31536000 SECOND).  Config-driven retention is applied after
-- migration by ensure_retention_ttl in pool.rs, which compares the live
-- table TTL to retention_period_secs and issues ALTER TABLE … MODIFY TTL
-- when they differ (DECOMPOSITION.md §2.5).
--
-- This file is pasteable as-is into clickhouse-client / DBeaver / play.html.

-- Table: usage_type_catalog
--
-- Engine: ReplacingMergeTree(version) — the `version` column is the
-- ReplacingMergeTree resolution key; the row with the highest version wins
-- on merge / FINAL.  This resolves the create sequence's own race window
-- (two concurrent `create_usage_type` calls for the same `gts_id` may both
-- pass the pre-existence check and INSERT; convergence collapses the
-- duplicate physical rows, keeping whichever insert's version is higher).
--
-- Deletion is a real row removal via ClickHouse's lightweight `DELETE FROM
-- ... WHERE gts_id = ?` — never `ALTER TABLE ... DELETE`, which is an
-- asynchronous background mutation unsuitable for the request path. A
-- lightweight `DELETE` masks the matching row from every subsequent query
-- synchronously with the statement's return, so `delete_usage_type` can
-- rely on the row being immediately absent; no tombstone flag or
-- FINAL-resolved versioned marker is needed to represent "deleted" for this
-- table (DESIGN.md §3.6).
--
-- ORDER BY (gts_id): single-column sort key for point lookups on gts_id.
-- There is no native PRIMARY KEY / UNIQUE constraint in ClickHouse; uniqueness
-- on gts_id is enforced by the application-level pre-existence check in the
-- create sequence (DESIGN.md §3.6).
CREATE TABLE IF NOT EXISTS usage_type_catalog
(
    gts_id          String                      COMMENT 'GTS usage-type identifier; sorting-key column (closest ClickHouse analog of a primary key)',
    kind            Enum8('counter' = 1, 'gauge' = 2)
                                                COMMENT 'Counter or gauge classification; stored verbatim',
    metadata_fields Array(String)              COMMENT 'Closed list of allowed metadata key names; stored verbatim',
    version         UInt64                      COMMENT 'ReplacingMergeTree version column; higher value wins on merge / FINAL resolution'
)
ENGINE = ReplacingMergeTree(version)
ORDER BY (gts_id);

-- Table: usage_records
--
-- Engine: ReplacingMergeTree(version) — same resolution mechanism as above.
-- A new versioned row with `status = inactive` and a higher version emulates
-- deactivation (no in-place UPDATE; DESIGN.md §3.6 Deactivation Cascade).
--
-- ORDER BY (tenant_id, gts_id, created_at, id): the 4-tuple that is also
-- the deterministic dedup key (ADR-0013 / ADR-0014).  This sort key is
-- chosen so that:
--   (a) the dominant read pattern (tenant + type + time-range scans for
--       aggregation / list) is a sort-key-aligned range scan, and
--   (b) the dedup point-lookup (always supplies all four columns) is a
--       genuine primary-key point lookup rather than a filtered scan.
--
-- No FOREIGN KEY on gts_id — ClickHouse has no FK support; referential
-- integrity is enforced in application code via the cluster exclusive per-gts_id
-- coordination lock (single exclusive mutex — DESIGN.md §3.5, §3.6).
--
-- No UNIQUE constraint, no ON CONFLICT — ClickHouse has neither.  Dedup is
-- emulated at the application level; ReplacingMergeTree convergence is the
-- backstop (DESIGN.md §3.6 Ingest Dedup step 8).
--
-- Data-skipping indexes: two request-path predicates do not lead with the
-- ORDER BY prefix and would otherwise scan every granule as the table grows:
--   * get_usage_record   -> WHERE id = ?
--   * deactivate cascade -> WHERE id = ? OR (corrects_id = ? AND status = ...)
-- `id` is the trailing sort-key column and `corrects_id` is not in the sort
-- key at all, so both get a bloom_filter index instead.  The ORDER BY itself
-- is deliberately left untouched: it is the dedup identity that
-- ReplacingMergeTree collapses on, so changing it would change which rows
-- FINAL resolves together.  Skip indexes only prune granules and never affect
-- that resolution.
--
-- Adding an index to CREATE TABLE IF NOT EXISTS has no effect on a table that
-- already exists; deployments provisioned before this change need an explicit
-- `ALTER TABLE usage_records ADD INDEX ...` (plus `MATERIALIZE INDEX` for
-- pre-existing parts), which is a follow-up migration rather than something
-- the startup path should run.
--
-- TTL: `created_at` is DateTime64(6); the clause uses the column directly
-- (`created_at + INTERVAL <n> SECOND DELETE`) so expiry stays in the
-- DateTime64 range. Do not wrap with toDateTime — that casts to 32-bit
-- DateTime (saturates in 2106) and can expire rows the moment they are
-- written. The default window is 1 year (31536000 seconds);
-- ensure_retention_ttl in pool.rs reconciles this with
-- retention_period_secs after migration.
CREATE TABLE IF NOT EXISTS usage_records
(
    id              UUID                        COMMENT 'Deterministic gateway-derived record id (UUIDv5 of the 4-tuple dedup key); ADR-0013 / ADR-0014',
    tenant_id       UUID                        COMMENT 'Owning tenant',
    gts_id          String                      COMMENT 'Usage type; application-enforced reference to usage_type_catalog (no FK in ClickHouse)',
    value           Decimal128(9)               COMMENT 'Signed delta',
    created_at      DateTime64(6)               COMMENT 'Event time; third ORDER BY column for time-range scan locality',
    resource_id     String                      COMMENT 'Resource instance identifier',
    resource_type   String                      COMMENT 'Resource type discriminator',
    subject_id      Nullable(String)            COMMENT 'Optional subject identifier',
    subject_type    Nullable(String)            COMMENT 'Optional subject type discriminator',
    idempotency_key String                      COMMENT 'Caller idempotency key',
    corrects_id     Nullable(UUID)              COMMENT 'Set on a compensation row; references the corrected ordinary-usage row',
    status          Enum8('active' = 1, 'inactive' = 2)
                                                COMMENT 'Lifecycle status; transitions are new versioned rows, never in-place UPDATE',
    metadata        Map(String, String)         COMMENT 'Caller metadata; Map(String, String) chosen over JSON for efficient metadata[key] push-down at query time',
    ingested_at     DateTime64(6)               COMMENT 'Server insert timestamp',
    version         UInt64                      COMMENT 'ReplacingMergeTree version column; higher value wins on merge / FINAL resolution',

    INDEX idx_records_id id TYPE bloom_filter GRANULARITY 1,
    INDEX idx_records_corrects_id corrects_id TYPE bloom_filter GRANULARITY 1
)
ENGINE = ReplacingMergeTree(version)
ORDER BY (tenant_id, gts_id, created_at, id)
TTL created_at + INTERVAL 31536000 SECOND DELETE;
