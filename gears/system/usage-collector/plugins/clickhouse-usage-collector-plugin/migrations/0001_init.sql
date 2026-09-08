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
-- Likewise, SETTINGS non_replicated_deduplication_window on usage_records is
-- applied to pre-existing tables by ensure_insert_dedup_window in pool.rs
-- (ALTER TABLE … MODIFY SETTING), since CREATE TABLE IF NOT EXISTS cannot
-- retrofit a setting onto a table that already exists.
--
-- This file is pasteable as-is into clickhouse-client / DBeaver / play.html.

-- Table: usage_type_catalog
--
-- Engine: ReplacingMergeTree(version) — the `version` column is the
-- ReplacingMergeTree resolution key; the row with the highest version wins
-- on merge, and on the read-time resolution every SELECT applies.  This
-- resolves the create sequence's own race window
-- (two concurrent `create_usage_type` calls for the same `gts_id` may both
-- pass the pre-existence check and INSERT; convergence collapses the
-- duplicate physical rows, keeping whichever insert's version is higher).
--
-- Rows are never deleted by the plugin: `delete_usage_type` is not
-- implemented by this backend (it returns an Internal "not implemented"
-- error without issuing SQL), so the catalog is append-only and needs no
-- tombstone flag or versioned "deleted" marker (DESIGN.md §3.6).
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
-- Engine: ReplacingMergeTree(version) — same merge-time collapse as above.
-- A new versioned row with `status = inactive`, a higher version and the SAME
-- id emulates deactivation (no in-place UPDATE; DESIGN.md §3.6 Deactivation
-- Cascade).  Read-time handling differs by read shape (query/dedup.rs):
--   * point reads (get, the cascade's own read, the create-path dedup
--     lookups) resolve the highest version explicitly with
--     ORDER BY version DESC LIMIT 1 BY <sort key> over a bloom-filter-pruned
--     candidate set;
--   * range reads (list, aggregate) do NOT resolve versions: they scan raw
--     rows and exclude the ids that carry an inactive marker
--     (`id NOT IN (SELECT id … WHERE … AND status = 'inactive')`), which is
--     exact for deactivation before any merge and costs one hash probe per
--     row instead of a sort or hash aggregation over the whole scan.
--
-- Duplicate creates are prevented at the engine rather than collapsed at read
-- time: every usage_records INSERT carries an insert_deduplication_token
-- (record_store.rs `insert_dedup_token`) and SETTINGS
-- non_replicated_deduplication_window below keeps the last 10000 inserted
-- blocks' tokens, so a racing retry of the same row(s) is dropped before it
-- becomes a part.  ClickHouse enforces the token on synchronous inserts; on
-- asynchronous inserts only Replicated* engines do, so with async_insert on
-- (the plugin default) a duplicate single-record create that lands in a
-- different flush than its twin is visible to list/aggregate until the merge.
--
-- ORDER BY (gts_id, tenant_id, created_at, id): `id` is the deterministic
-- UUIDv5 projection of the canonical dedup tuple (ADR-0013 / ADR-0014), so
-- this sort key is one-to-one with that tuple.  The column order is chosen
-- so that:
--   (a) gts_id leads, because every request-path read pins it: it is a typed
--       SPI parameter on both list and aggregate, whereas tenant_id and
--       created_at arrive only through the optional OData $filter.  Behind a
--       leading high-cardinality tenant_id, a `gts_id = ?` read falls back to
--       ClickHouse's generic exclusion search, which prunes effectively only
--       when the preceding key column has LOW cardinality; against a UUID it
--       read every granule.  gts_id is also the low-cardinality column of the
--       four, so leading with it compresses the primary index better.
--   (b) the dominant read pattern (type + tenant + time-range scans for
--       aggregation / list) is a sort-key-aligned range scan over the
--       (gts_id, tenant_id, created_at) prefix, and
--   (c) the dedup lookup resolves against that same three-column prefix as a
--       primary-key range rather than a full scan.
--
-- Permuting these four columns does NOT change what ReplacingMergeTree
-- collapses on.  The engine's row identity is the sort key as a *set* of
-- columns, the LIMIT 1 BY resolution fragment this plugin emits
-- (query/dedup.rs) is order-insensitive too, and the marker anti-join keys on
-- `id` alone; only adding or removing a column would change which rows
-- resolve together.
--
-- Neither ORDER BY nor PARTITION BY can be ALTERed in place, so a deployment
-- provisioned before this file changed either one keeps what it has until the
-- table is rebuilt (CREATE new + INSERT SELECT + EXCHANGE TABLES).  Every
-- variant is correct; the older ones are only slower.
--
-- PARTITION BY toYYYYMM(created_at): monthly partitions, which buy two things
-- on a table that is written in event-time order and expired by TTL.
--   * Time-range reads prune whole partitions before the primary index is
--     consulted at all.  This composes with, rather than replaces, the
--     sort-key prefix: it prunes a created_at range even when the read pins no
--     gts_id or tenant_id, and it needs the created_at predicate to reach the
--     scan, which is what the $filter split in query/dedup.rs is for.
--   * TTL expiry becomes a metadata-only partition drop rather than a merge
--     that rewrites every column of a part to remove its expired rows.
--     SETTINGS ttl_only_drop_parts = 1 below makes that the only mode: a part
--     is dropped once all of its rows have expired, and is never partially
--     rewritten.  The cost is that a row outlives the configured window by up
--     to the span of its own partition; ensure_retention_ttl in pool.rs warns
--     when retention_period_secs is short enough for that overshoot to matter.
--
-- Monthly is chosen against the 1-year default retention: ~13 live partitions,
-- which is the right order of magnitude (a MergeTree starts paying for part
-- count in the hundreds of partitions, and a single partition would give TTL
-- nothing to drop).  A much shorter configured retention would prefer a finer
-- key, but PARTITION BY is fixed at CREATE while retention is config, so the
-- DDL commits to the default's scale.
--
-- Partitioning weakens INSERT atomicity, which the deactivation cascade
-- depends on: ClickHouse commits one part per partition an INSERT touches, so
-- a statement spanning two months is two commits.  See the ATOMICITY NOTE on
-- deactivate in record_store.rs for the exact window this opens.
--
-- The dedup lookup itself keys on the canonical tuple
-- (tenant_id, gts_id, created_at, idempotency_key), NOT on id — see
-- record_store.rs `DedupKey`.  idempotency_key is deliberately absent from
-- the sort key: adding it would change what ReplacingMergeTree collapses on,
-- and the three-column prefix already prunes to a handful of rows, over which
-- idempotency_key applies as a cheap residual filter.  Keying the lookup on
-- id instead would miss a stored row whose id disagrees with its own tuple
-- and re-insert it under an idempotency key already in use.
--
-- No FOREIGN KEY on gts_id — ClickHouse has no FK support.  gts_id is a soft
-- reference checked in application code at insert time (an existence read of
-- usage_type_catalog, which needs no version resolution); since usage types
-- are never deleted, no delete-side check or coordination is needed
-- (DESIGN.md §3.6).
--
-- No UNIQUE constraint, no ON CONFLICT — ClickHouse has neither.  Dedup is
-- emulated at the application level (SELECT then INSERT), the engine's
-- insert_deduplication_token window catches the racing retries the pre-read
-- misses, and ReplacingMergeTree convergence is the backstop (DESIGN.md §3.6
-- Ingest Dedup step 8).
--
-- Data-skipping indexes: two request-path predicates do not lead with the
-- ORDER BY prefix and would otherwise scan every granule as the table grows:
--   * get_usage_record   -> WHERE id = ?
--   * deactivate cascade -> WHERE id = ? OR (corrects_id = ? AND status = ...)
-- `id` is the trailing sort-key column and `corrects_id` is not in the sort
-- key at all, so both get a bloom_filter index instead.  Neither predicate is
-- served by reordering the sort key: skip indexes prune granules for reads
-- that cannot use the key prefix at all, and they never affect which rows
-- resolve together.
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
-- retention_period_secs after migration.  Expiry is whole-partition only
-- (ttl_only_drop_parts = 1), so the configured window is a lower bound on a
-- row's lifetime rather than an exact deadline; see the PARTITION BY note.
CREATE TABLE IF NOT EXISTS usage_records
(
    id              UUID                        COMMENT 'Deterministic gateway-derived record id (UUIDv5 of the 4-tuple dedup key); ADR-0013 / ADR-0014',
    tenant_id       UUID                        COMMENT 'Owning tenant; second ORDER BY column',
    gts_id          String                      COMMENT 'Usage type; leading ORDER BY column, pinned by every request-path read; application-enforced reference to usage_type_catalog (no FK in ClickHouse)',
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
PARTITION BY toYYYYMM(created_at)
ORDER BY (gts_id, tenant_id, created_at, id)
TTL created_at + INTERVAL 31536000 SECOND DELETE
SETTINGS ttl_only_drop_parts = 1, non_replicated_deduplication_window = 10000;
