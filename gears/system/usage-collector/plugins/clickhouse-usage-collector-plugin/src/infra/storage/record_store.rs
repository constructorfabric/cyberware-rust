//! `ClickHouse`-backed [`RecordStore`] over the `usage_records` table.
//!
//! All operations — `create` / `create_batch` / `get` / `list` / `aggregate` /
//! `deactivate` — are implemented against `ClickHouse` using the `clickhouse`
//! 0.15.x crate.
//!
//! ## Key design differences from the `TimescaleDB` reference plugin
//!
//! - **No `ON CONFLICT DO NOTHING`**: `ClickHouse` has no unique constraints.
//!   Dedup is performed explicitly: SELECT then INSERT.
//! - **No `FOR UPDATE`, no coordination lock**: `ClickHouse` has no row-level
//!   locks and this plugin uses no external mutex. Creates are not serialised
//!   against each other. Two concurrent creates for one dedup key can both
//!   pass the pre-read; the engine then drops the second *block* when both
//!   carry the same `insert_deduplication_token` ([`insert_dedup_token`],
//!   honoured on synchronous inserts against the table's
//!   `non_replicated_deduplication_window`) — **first-writer-wins** at the
//!   engine — and otherwise `ReplacingMergeTree(version)` collapses the twin
//!   rows at the next merge. Consequences: (a) the dedup pre-read is
//!   best-effort; (b) `IdempotencyConflict` is raised only when the earlier row
//!   is already visible at pre-read time; (c) `UsageTypeNotFound` comes from a
//!   catalog read with no ordering guarantee against a concurrent
//!   `create_usage_type`; (d) that same catalog read has no ordering guarantee
//!   against a concurrent `delete_usage_type` either — the delete's own
//!   post-delete sweep (see `catalog_store`) removes rows that land inside its
//!   window, but an insert that passed the check before the delete and commits
//!   after the sweep orphans a row. That residual is accepted by design; the
//!   insert-time catalog check is what bounds it, since a deleted type is
//!   refused here from the moment its catalog row is gone.
//! - **No `UPDATE`**: deactivation uses versioned marker rows (INSERT with
//!   `status = 'inactive'` and a higher `version`, same `id`) rather than
//!   `ALTER TABLE … UPDATE` (an async mutation unsuitable for the request path).
//! - **No `FINAL`, and no resolution on the range reads**: `get`, the
//!   deactivation cascade and the create-path dedup lookups resolve
//!   `ReplacingMergeTree` versions themselves over a tiny candidate set —
//!   `ORDER BY version DESC LIMIT 1 BY <sort key>`, see [`query::dedup`]. `list`
//!   and `aggregate` do not resolve at all: they scan raw rows and anti-join
//!   the ids that carry a deactivation marker (`dedup::resolved_survivors` /
//!   `dedup::active_survivors`), which is exact for deactivation before any
//!   merge and costs one hash probe per row instead of a sort or hash
//!   aggregation over the whole scan. Unmerged duplicate creates the engine
//!   did not catch are visible to those two reads until the merge; the module
//!   docs of `query::dedup` carry the full argument.
//!
//!   [`query::dedup`]: crate::infra::storage::query::dedup
//! - **`?` placeholders**: `ClickHouse` uses positional `?` (not `$N`).
//! - **`metadata['key']`**: map subscript (not `metadata ->> key`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use tracing::instrument;
use uuid::Uuid;

use toolkit_odata::filter::{FilterField, FilterNode, convert_expr_to_filter_node};
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, SortDir};

use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationResult, AggregationSpec, MetadataFilter,
    UsageCollectorPluginError, UsageRecord, UsageRecordFilterField, UsageTypeGtsId,
    is_keyset_safe_record_field,
};

use crate::domain::ports::RecordStore;
use crate::infra::metrics::{InsertMode, Metrics, OpDurationGuard, QueryKind, TimedOp};
use crate::infra::storage::entity::{EpochMicros, UsageRecordRow, UsageRecordStatusCode};
use crate::infra::storage::error::{tracked_ch_err, with_deadline};
use crate::infra::storage::mapper::{
    canonical_equal, current_merge_version, make_inactive_marker, record_row_key,
    version_higher_than,
};
use crate::infra::storage::pool::configure_insert;
use crate::infra::storage::query::aggregate::{
    agg_select_expr, aggregate_limit_clause, corrects_id_partition_clause, dimension_select_expr,
};
use crate::infra::storage::query::dedup::{
    RECORD_DEDUP_KEY, active_survivors, latest_by, resolved_survivors, split_version_invariant,
};
use crate::infra::storage::query::effective_page_size;
use crate::infra::storage::query::keyset::{
    encode_next_cursor, ensure_forward_cursor, keyset_predicate, render_order_by,
};
use crate::infra::storage::query::translate::{
    SqlBind, SqlCtx, bind_one, record_column, translate_record_filter,
};

/// Static column list for every `usage_records` SELECT, in [`UsageRecordRow`]
/// field order. A `'static` constant (never caller input), so decoding is
/// positional without SQL injection risk.
const RECORD_COLUMNS: &str = "id, tenant_id, gts_id, value, created_at, resource_id, \
     resource_type, subject_id, subject_type, idempotency_key, corrects_id, status, metadata, \
     ingested_at, version";

/// What a `usage_records` `INSERT` writes, for [`insert_dedup_token`].
///
/// A deactivation marker carries the *same* `id` as the row it supersedes, so
/// the token must tell the two apart: otherwise a marker set written inside the
/// engine's dedup window of the create that wrote those very ids would be
/// dropped as a retry of it, and the deactivation would silently not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertKind {
    /// Ordinary or compensation usage rows (single create or batch).
    Record,
    /// Deactivation markers written by the cascade.
    Marker,
}

impl InsertKind {
    /// Fixed `UUIDv5` namespace per kind — the discriminator described above.
    fn namespace(self) -> Uuid {
        match self {
            // Arbitrary fixed values; they only need to be distinct and stable.
            Self::Record => Uuid::from_u128(0x7a4c_1f0e_2b9d_4c63_8e5a_0d1b_9f2e_3c4d),
            Self::Marker => Uuid::from_u128(0x2e9b_6d3a_8f10_4b7c_a1d5_6e2f_0c8b_9a7e),
        }
    }
}

/// `insert_deduplication_token` for one `usage_records` `INSERT`.
///
/// `ClickHouse` deduplicates an inserted block against the last
/// `non_replicated_deduplication_window` blocks by this token (per partition
/// the block touches), so two racing statements writing the same rows must
/// produce the same token and any other pair must not. The token is the `UUIDv5`
/// of the rows' `id`s — sorted and deduplicated, so it is insensitive to row
/// order and to a repeated row — under a per-[`InsertKind`] namespace. A single
/// create and a batch containing only that record therefore produce the same
/// token, which is the intended behaviour: they are the same write.
///
/// `id` is itself the `UUIDv5` of the record's dedup tuple (plugin-spi.md), so
/// two statements share a token exactly when they carry the same set of
/// logical rows of the same kind.
pub(crate) fn insert_dedup_token(rows: &[UsageRecordRow], kind: InsertKind) -> String {
    let mut ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut buf = Vec::with_capacity(ids.len() * 16);
    for id in &ids {
        buf.extend_from_slice(id.as_bytes());
    }
    Uuid::new_v5(&kind.namespace(), &buf).to_string()
}

/// `ClickHouse`-backed implementation of [`RecordStore`] over `usage_records`.
#[derive(Clone)]
pub struct ChRecordStore {
    client: clickhouse::Client,
    metrics: Arc<Metrics>,
    request_timeout: Duration,
    /// Whether `usage_records` `INSERT`s carry `async_insert = 1` /
    /// `wait_for_async_insert = 1`
    /// ([`crate::config::ClickHousePluginConfig::async_insert`]).
    async_insert: bool,
}

impl ChRecordStore {
    /// Build a store from an existing `ClickHouse` client, metric inventory,
    /// and per-request client-side deadline.
    ///
    /// There is no coordination primitive to inject: every write path is a
    /// plain read-then-insert whose concurrency semantics are described in the
    /// module docs.
    ///
    /// `request_timeout` bounds every individual `ClickHouse` await; production
    /// wiring passes `ClickHousePluginConfig::client_deadline()`. It is an
    /// explicit parameter rather than a defaulted builder step so a future
    /// wiring change cannot silently leave a store on a default that disagrees
    /// with the configured budget.
    ///
    /// `async_insert` selects server-side asynchronous inserts for this
    /// store's `usage_records` writes; production wiring passes
    /// `ClickHousePluginConfig::async_insert`. Explicit for the same reason as
    /// `request_timeout` — a store silently defaulting to the opposite of the
    /// configured value would change durability and part-formation behaviour
    /// without anything failing.
    #[must_use]
    pub fn new(
        client: clickhouse::Client,
        metrics: Arc<Metrics>,
        request_timeout: Duration,
        async_insert: bool,
    ) -> Self {
        Self {
            client,
            metrics,
            request_timeout,
            async_insert,
        }
    }

    /// Execute a `SELECT … FROM usage_type_catalog WHERE gts_id = ?` catalog
    /// existence check for a single record.
    ///
    /// Returns `Ok(())` if the usage type exists; `UsageTypeNotFound`
    /// otherwise.
    ///
    /// A type seen here is **not** guaranteed to still exist when the `INSERT`
    /// that follows commits: `delete_usage_type` removes the catalog row with
    /// no mutual exclusion against this path. That is the accepted residual
    /// documented on
    /// [`ChCatalogStore::delete`](crate::infra::storage::catalog_store) — the
    /// delete's post-delete sweep removes rows that land inside its own
    /// window, but a check that passed *before* the delete and an `INSERT`
    /// that commits *after* the sweep still orphan a row. This check is what
    /// keeps that window narrow rather than unbounded: once the catalog row is
    /// gone, every subsequent insert for the `gts_id` is refused here.
    ///
    /// No version resolution: the question is only whether *any* row carries
    /// this `gts_id`. Existence is invariant across versions — every physical
    /// copy shares the `gts_id` that is the whole sort key, and no copy is a
    /// tombstone — so `LIMIT 1` on the first match is both correct and the
    /// cheapest form of the query.
    ///
    /// # Errors
    ///
    /// Returns `Transient` on connectivity errors, `Internal` on protocol
    /// errors, `UsageTypeNotFound` when absent.
    ///
    /// (`ClickHouse` errors are mapped via [`tracked_ch_err`].)
    #[instrument(skip_all, fields(gts_id = %gts_id.as_ref()))]
    async fn check_catalog_existence(
        &self,
        gts_id: &UsageTypeGtsId,
    ) -> Result<(), UsageCollectorPluginError> {
        let sql = "SELECT gts_id FROM usage_type_catalog WHERE gts_id = ? LIMIT 1";
        let found: Option<String> = {
            with_deadline(
                &self.metrics,
                self.request_timeout,
                self.client
                    .query(sql)
                    .bind(gts_id.as_ref())
                    .fetch_optional::<String>(),
            )
            .await?
        };
        if found.is_none() {
            return Err(UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: gts_id.clone(),
            });
        }
        Ok(())
    }

    /// Catalog existence check for a whole batch: one
    /// `SELECT gts_id FROM usage_type_catalog WHERE gts_id IN (…)` over the
    /// batch's distinct `gts_id`s, returning the subset that exists.
    ///
    /// The batch analogue of [`Self::check_catalog_existence`]; the caller
    /// maps every record whose `gts_id` is absent from the result to
    /// `UsageTypeNotFound`.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    #[instrument(skip_all, fields(type_count = gts_ids.len()))]
    async fn existing_usage_types(
        &self,
        gts_ids: &BTreeSet<&str>,
    ) -> Result<HashSet<String>, UsageCollectorPluginError> {
        if gts_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let (sql, ctx) = catalog_lookup_sql(gts_ids);
        let mut q = self.client.query(&sql);
        for b in &ctx.binds {
            q = bind_one(q, b);
        }
        let found: Vec<String> = {
            with_deadline(&self.metrics, self.request_timeout, q.fetch_all::<String>()).await?
        };
        Ok(found.into_iter().collect())
    }

    /// Dedup lookup on the canonical dedup tuple [`DedupKey`]:
    /// `WHERE tenant_id = ? AND gts_id = ? AND created_at = ? AND
    /// idempotency_key = ?`
    ///
    /// The three leading columns are the `ORDER BY` prefix, so this still
    /// prunes down to a primary-key point; `idempotency_key` is a residual
    /// filter over the handful of rows sharing that exact microsecond.
    ///
    /// Returns the stored row if found, `None` if no row exists for this key.
    /// A well-formed table holds at most one matching row, but `ClickHouse`
    /// cannot enforce that — see [`prefer_dedup_row`] for how a legacy
    /// mismatched-`id` twin is resolved. `ORDER BY id` only makes the candidate
    /// order stable; the choice itself is made by `prefer_dedup_row`.
    ///
    /// `LIMIT 1 BY id` — not the full sort key — is the version-resolution
    /// step: the `WHERE` already pins `gts_id`, `tenant_id` and `created_at`,
    /// so `id` is the only part of the sort key still free. Each surviving `id`
    /// is one logical row at its highest `version`, and `prefer_dedup_row` then
    /// chooses between distinct `id`s.
    ///
    /// NOTE — filtering on `idempotency_key` (not a sort-key column) *below*
    /// the resolution step is safe. `idempotency_key` is invariant across every
    /// version of a given sort key: `id` is the `UUIDv5` of the canonical
    /// tuple, and [`make_inactive_marker`] clones the source row wholesale, so a
    /// deactivation marker carries the same key. Filtering before and after
    /// resolution therefore selects the same sort keys — which is what keeps
    /// the "inactive stored row ⇒ conflict" path in
    /// [`Self::resolve_dedup_hit`] working: a deactivated key's surviving row
    /// is its marker, so the `status` the caller sees is `inactive`.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    #[instrument(skip_all, fields(gts_id = %record.gts_id.as_ref()))]
    async fn dedup_point_lookup(
        &self,
        record: &UsageRecord,
    ) -> Result<Option<UsageRecordRow>, UsageCollectorPluginError> {
        // Spelled out rather than built from `dedup::latest_by`: the candidate
        // order `prefer_dedup_row` relies on (`id` ascending) has to lead the
        // `ORDER BY`, with `version DESC` as the tie-break that makes
        // `LIMIT 1 BY id` keep each id's newest row.
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records \
             WHERE gts_id = ? AND tenant_id = ? \
             AND created_at = fromUnixTimestamp64Micro(?) AND idempotency_key = ? \
             ORDER BY id ASC, version DESC LIMIT 1 BY id"
        );
        let created_at_micros = EpochMicros::from(record.created_at).0;
        let rows: Vec<UsageRecordRow> = {
            with_deadline(
                &self.metrics,
                self.request_timeout,
                self.client
                    .query(&sql)
                    .bind(record.gts_id.as_ref())
                    .bind(record.tenant_id.to_string())
                    .bind(created_at_micros)
                    .bind(record.idempotency_key.as_str())
                    .fetch_all(),
            )
            .await?
        };
        Ok(rows.into_iter().fold(None, |current, candidate| {
            Some(prefer_dedup_row(current, candidate, record.id))
        }))
    }

    /// Insert a single row into `usage_records`.
    ///
    /// Tracks pool-acquire duration (time to get the `Insert` handle) via the
    /// metric inventory, and reports the single-row create latency measured
    /// from `op_start` — the caller's entry instant, so the catalog check and
    /// the dedup read are inside the observed window.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    async fn insert_record(
        &self,
        row: &UsageRecordRow,
        op_start: Instant,
    ) -> Result<(), UsageCollectorPluginError> {
        let pool_start = Instant::now();
        // Insert-time timeouts and settings both come from `configure_insert`,
        // so this site cannot drift from the batch and catalog ones.
        //
        // The dedup token makes a racing retry of this very row a no-op at the
        // engine — on a synchronous insert. With `async_insert = 1` on the
        // shipped non-replicated table the token is carried but not enforced
        // (async dedup is a `Replicated*` feature); twins that land in one
        // flush still collapse through `optimize_on_insert`, and the rest at
        // merge. See `config::ClickHousePluginConfig::async_insert`.
        let token = insert_dedup_token(std::slice::from_ref(row), InsertKind::Record);
        let mut insert: clickhouse::insert::Insert<UsageRecordRow> = configure_insert(
            {
                with_deadline(
                    &self.metrics,
                    self.request_timeout,
                    self.client.insert("usage_records"),
                )
                .await?
            },
            self.request_timeout,
            self.async_insert,
            Some(&token),
        );
        self.metrics
            .record_pool_acquire(pool_start.elapsed().as_secs_f64());

        {
            insert
                .write(row)
                .await
                .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        }
        {
            insert
                .end()
                .await
                .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        }
        self.metrics
            .record_insert(InsertMode::Single, op_start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Insert multiple rows into `usage_records` in a single INSERT statement.
    ///
    /// Always synchronous, regardless of the store's `async_insert` setting:
    /// both callers depend on the statement's rows becoming visible together,
    /// which the server-side async-insert buffer does not guarantee. The body
    /// carries the full argument.
    ///
    /// Atomicity is **per partition**, not per statement. `usage_records` is
    /// `PARTITION BY toYYYYMM(created_at)` and a `ClickHouse` INSERT commits one
    /// part per partition it touches, so rows landing in one month are visible
    /// all-or-nothing while a statement spanning two months is two commits — a
    /// version-resolving reader can observe one part without the other. The one
    /// caller that depends on the all-or-nothing reading, the deactivation
    /// cascade, states the resulting window at its own call site.
    ///
    /// A statement whose block spans more than `max_partitions_per_insert_block`
    /// partitions (server default 100, i.e. 100 distinct months) is rejected by
    /// `ClickHouse` outright; that surfaces as `Transient`/`Internal` like any
    /// other insert failure.
    ///
    /// Tracks pool-acquire duration via the metric inventory and reports the
    /// batch-write latency measured from `op_start`, which the caller sets to
    /// the point its observed window should begin.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    async fn insert_records(
        &self,
        rows: &[UsageRecordRow],
        op_start: Instant,
        kind: InsertKind,
    ) -> Result<(), UsageCollectorPluginError> {
        if rows.is_empty() {
            return Ok(());
        }
        let pool_start = Instant::now();
        // Synchronous, so the engine enforces this token: an identical racing
        // batch, or a repeated cascade for the same ids, is dropped as a block.
        // `kind` keeps a marker set from being mistaken for the create that
        // wrote the same ids (see `InsertKind`).
        let token = insert_dedup_token(rows, kind);
        // Deliberately **synchronous**, whatever `self.async_insert` says — the
        // one place this store departs from the configured setting, and the
        // reason is the per-statement atomicity both callers of this method
        // rely on.
        //
        // `async_insert = 1` hands the rows to a server-side buffer that
        // coalesces across statements and flushes on its own schedule, and one
        // statement's rows are not guaranteed to land in a single commit. That
        // is harmless for independent rows but not here:
        //
        // * the deactivation cascade writes one marker row per affected id and
        //   requires them to flip together (see the ATOMICITY NOTE on
        //   `deactivate`); async inserts made a concurrent resolved read
        //   observe a flipped compensation under a still-active target, which
        //   `ch_deactivation_cascade_is_atomic` catches; and
        // * `create_batch` is documented as a whole-batch write (DESIGN.md
        //   §3.6 Batch Ingest).
        //
        // Nothing is given up by staying synchronous: async inserts exist here
        // to stop the *one-INSERT-per-request* path writing a part per record,
        // and a batch statement already carries up to the SPI's batch cap of
        // rows in a single part. `insert_record` is the site that needs the
        // buffer, and it has no cross-row invariant to lose.
        let mut insert: clickhouse::insert::Insert<UsageRecordRow> = configure_insert(
            {
                with_deadline(
                    &self.metrics,
                    self.request_timeout,
                    self.client.insert("usage_records"),
                )
                .await?
            },
            self.request_timeout,
            false,
            Some(&token),
        );
        self.metrics
            .record_pool_acquire(pool_start.elapsed().as_secs_f64());
        // Row counts up to the batch cap (≤1000) fit exactly in f64's 52-bit mantissa.
        #[allow(
            clippy::cast_precision_loss,
            reason = "batch size is bounded by the SPI wire-level batch cap (<=1000 rows)"
        )]
        let batch_len = rows.len() as f64;
        self.metrics.record_batch_rows(batch_len);

        {
            for row in rows {
                insert
                    .write(row)
                    .await
                    .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
            }
        }
        {
            insert
                .end()
                .await
                .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        }
        self.metrics
            .record_insert(InsertMode::Batch, op_start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Batch dedup pre-check: SELECT all rows whose `(tenant_id, gts_id,
    /// created_at, idempotency_key)` canonical dedup tuple appears in the input
    /// list — written in sort-key order (`gts_id` first) so the tuple's first
    /// three components are the table's key prefix — the batch analogue of [`Self::dedup_point_lookup`], including its
    /// argument for filtering on `idempotency_key` below the resolution step.
    ///
    /// Returns a map from the 4-tuple key to the stored row. Two stored rows can
    /// share a key only through a legacy mismatched-`id` twin; the collision is
    /// resolved by [`prefer_dedup_row`] against the incoming record's `id`.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    #[instrument(skip_all, fields(record_count = records.len()))]
    async fn batch_dedup_lookup(
        &self,
        records: &[&UsageRecord],
    ) -> Result<HashMap<DedupKey, UsageRecordRow>, UsageCollectorPluginError> {
        if records.is_empty() {
            return Ok(HashMap::new());
        }
        // Build `(g, t, c, k) IN ((?, ?, fromUnixTimestamp64Micro(?), ?), ...)`
        // — tuple element order follows `ORDER BY (gts_id, tenant_id,
        // created_at, id)`, so the leading three elements pin the sort-key
        // prefix; `idempotency_key` is not in the key and is a residual filter.
        // A bare epoch-microsecond integer in a tuple comparison is coerced
        // through Decimal arithmetic by ClickHouse and can raise
        // DECIMAL_OVERFLOW before the query starts.
        let mut ctx = SqlCtx::new();
        let mut tuples = Vec::with_capacity(records.len());
        // The `id` each incoming record expects to find, so a collision between
        // two stored rows sharing a dedup key is resolved toward the caller's
        // own record rather than by part-read order.
        let mut expected_ids: HashMap<DedupKey, Uuid> = HashMap::with_capacity(records.len());
        for r in records {
            ctx.push(SqlBind::Str(r.gts_id.as_ref().to_owned()));
            ctx.push(SqlBind::Uuid(r.tenant_id));
            ctx.push(SqlBind::DateTime64Micros(EpochMicros::from(r.created_at).0));
            ctx.push(SqlBind::Str(r.idempotency_key.as_str().to_owned()));
            tuples.push("(?, ?, fromUnixTimestamp64Micro(?), ?)");
            expected_ids.insert(record_dedup_key(r), r.id);
        }
        let in_clause = tuples.join(", ");
        // Every column in the `IN` tuple is version-invariant, so the filter
        // may sit below the resolution step and still prune on the sort-key
        // prefix. Unlike the single-record path this cannot pin `created_at`
        // to one value, so the resolution keys on the whole sort key.
        let resolve_latest = latest_by(RECORD_DEDUP_KEY);
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records \
             WHERE (gts_id, tenant_id, created_at, idempotency_key) IN ({in_clause})\
             {resolve_latest}"
        );
        let mut q = self.client.query(&sql);
        for b in &ctx.binds {
            q = bind_one(q, b);
        }
        let rows: Vec<UsageRecordRow> = {
            with_deadline(&self.metrics, self.request_timeout, q.fetch_all()).await?
        };
        let mut out: HashMap<DedupKey, UsageRecordRow> = HashMap::with_capacity(rows.len());
        for row in rows {
            let key = row_dedup_key(&row);
            // A row the batch did not ask for cannot come back from the `IN`
            // filter; `Uuid::nil` is an unreachable fallback that simply makes
            // the lowest-`id` rule the tie-break.
            let expected_id = expected_ids.get(&key).copied().unwrap_or_else(Uuid::nil);
            let chosen = prefer_dedup_row(out.remove(&key), row, expected_id);
            out.insert(key, chosen);
        }
        Ok(out)
    }

    /// Append metadata side-channel filters as parameterised `WHERE` clauses.
    ///
    /// `metadata['?'] IN (?, ?)` — both key and each value are bound via `ctx`.
    fn push_metadata_filters(
        metadata_filter: &[MetadataFilter],
        ctx: &mut SqlCtx,
        clauses: &mut Vec<String>,
    ) {
        for mf in metadata_filter {
            if mf.values().is_empty() {
                clauses.push("FALSE".to_owned());
                continue;
            }
            ctx.push(SqlBind::Str(mf.key().as_str().to_owned()));
            let placeholders = mf
                .values()
                .iter()
                .map(|v| {
                    ctx.push(SqlBind::Str(v.clone()));
                    "?"
                })
                .collect::<Vec<_>>();
            clauses.push(format!("metadata[?] IN ({})", placeholders.join(", ")));
        }
    }

    /// Translate a caller `$filter` into its two halves: the version-invariant
    /// conjuncts join the scan predicate (which is also repeated inside the
    /// marker subquery, so they prune both), the `status`-naming rest is
    /// appended after the survivor predicate, where it reads the raw `status`
    /// that on every surviving row equals the resolved one.
    ///
    /// The split itself, and why it is sound for `OR`/`NOT`, is
    /// [`split_version_invariant`]. Two things this call order guarantees:
    ///
    /// - **Bind order.** `ClickHouse` `?` is positional left-to-right and the
    ///   scan predicate (twice) precedes the trailing conjuncts in the emitted
    ///   text, so the invariant half must be translated first — its binds are
    ///   applied first too.
    /// - **Clause order within the scan half.** Callers push the metadata
    ///   side-channel before calling this, so `inner_ctx` already holds those
    ///   binds and the appended conjuncts land after them in both the text and
    ///   the bind list.
    fn push_split_filter(
        node: &FilterNode<UsageRecordFilterField>,
        inner_ctx: &mut SqlCtx,
        inner_clauses: &mut Vec<String>,
        outer_ctx: &mut SqlCtx,
        outer_clauses: &mut Vec<String>,
    ) -> Result<(), UsageCollectorPluginError> {
        let (invariant, version_dependent) = split_version_invariant(node);
        for conjunct in invariant {
            let fragment = translate_record_filter(conjunct, inner_ctx)
                .map_err(UsageCollectorPluginError::internal)?;
            inner_clauses.push(fragment);
        }
        for conjunct in version_dependent {
            let fragment = translate_record_filter(conjunct, outer_ctx)
                .map_err(UsageCollectorPluginError::internal)?;
            outer_clauses.push(fragment);
        }
        Ok(())
    }

    /// Assemble the single-level aggregate query text.
    ///
    /// Split out of [`RecordStore::aggregate`] so the text is unit-testable
    /// without a live `ClickHouse`.
    ///
    /// ```text
    /// SELECT <aliased_select> FROM usage_records
    /// WHERE <scan> AND status = 'active' AND id NOT IN (SELECT id FROM usage_records WHERE <scan> AND status = 'inactive')
    ///   [AND <outer_clauses>]
    /// [GROUP BY 1, …] [LIMIT …]
    /// ```
    ///
    /// One `WHERE`, in three parts:
    ///
    /// 1. `scan` — `inner_clauses` joined, every version-invariant predicate
    ///    (`gts_id`, the `corrects_id` partition, metadata side-channel, the
    ///    invariant half of `$filter`, subject guards). It prunes the scan.
    /// 2. the survivor predicate (`dedup::active_survivors`), which embeds the
    ///    *same* `scan` text so the marker subquery prunes identically. That is
    ///    why the caller binds `gts_id` and the scan binds twice, and why the
    ///    aggregate no longer needs a version-resolving level at all — one
    ///    physical row per active logical row survives, and the only `GROUP BY`
    ///    is the caller's dimension grouping.
    /// 3. `outer_clauses` — the `status`-naming half of `$filter`, which is
    ///    sound on the raw column here because every survivor's raw `status`
    ///    equals its resolved status. Empty for the common call.
    ///
    /// The `WHERE` is never empty (`gts_id = ?` is always the first scan
    /// clause), so no branch omits it.
    fn build_aggregate_sql(
        aliased_select: &str,
        inner_clauses: &[String],
        outer_clauses: &[String],
        group_by: &str,
        limit_clause: &str,
    ) -> String {
        let scan = inner_clauses.join(" AND ");
        let mut where_parts = vec![scan.clone(), active_survivors(&scan)];
        where_parts.extend(outer_clauses.iter().cloned());
        format!(
            "SELECT {aliased_select} FROM usage_records WHERE {}{group_by}{limit_clause}",
            where_parts.join(" AND ")
        )
    }

    /// Assemble the single-level keyset-list query text.
    ///
    /// Split out of [`RecordStore::list`] for the same reason as
    /// [`Self::build_aggregate_sql`].
    ///
    /// ```text
    /// SELECT <RECORD_COLUMNS> FROM usage_records
    /// WHERE <scan> AND (status = 'inactive' OR id NOT IN (SELECT id FROM usage_records WHERE <scan> AND status = 'inactive'))
    ///   [AND <outer_clauses>]
    /// ORDER BY <order_sql> LIMIT <limit>
    /// ```
    ///
    /// Same three-part `WHERE` as the aggregate, with `dedup::resolved_survivors`
    /// as the survivor predicate because `list` returns inactive rows too: a
    /// marker survives as the resolved row it is, an active row survives iff
    /// no marker exists for its id. `outer_clauses` carries the `status`-naming
    /// half of `$filter` and the keyset tuple predicate (which may name
    /// `status`); both read the raw column, which on every survivor equals the
    /// resolved value. `limit` is the caller's look-ahead `LIMIT` (page size
    /// plus one) and applies after every predicate, so a filtered page is never
    /// short.
    fn build_list_sql(
        inner_clauses: &[String],
        outer_clauses: &[String],
        order_sql: &str,
        limit: u64,
    ) -> String {
        let scan = inner_clauses.join(" AND ");
        let mut where_parts = vec![scan.clone(), resolved_survivors(&scan)];
        where_parts.extend(outer_clauses.iter().cloned());
        format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records WHERE {} ORDER BY {order_sql} LIMIT {limit}",
            where_parts.join(" AND ")
        )
    }
}

/// The 4-tuple dedup identity for `usage_records`: `(tenant_id, gts_id,
/// created_at_micros, idempotency_key)` — the SPI's canonical dedup tuple
/// (plugin-spi.md §"Plugin-specific outputs"; ADR-0014).
///
/// The record `id` is deliberately **not** part of this key. `id` is a
/// deterministic `UUIDv5` projection *of* this tuple, so for well-formed data
/// keying on either is equivalent — but a stored row whose `id` disagrees with
/// its own canonical tuple would be missed by an `id`-keyed lookup and inserted
/// as new, silently binding an idempotency key that is already taken. Keying on
/// the tuple instead lets [`canonical_equal`] compare stored vs incoming `id`
/// and surface the disagreement as `IdempotencyConflict`.
///
/// `created_at` is stored as `i64` epoch-microseconds, so the dedup key is
/// already µs-normalised; no truncation is needed.
///
/// NOTE — the field order differs from the `TimescaleDB` sibling plugin's
/// `(tenant_id, gts_id, idempotency_key, created_at)`. That is deliberate:
/// `ClickHouse` can only prune an `IN`-tuple against the primary key when the
/// tuple's *leading* elements match the `ORDER BY` prefix, so `created_at` (a
/// key column) must stay ahead of `idempotency_key` (which is not in the sort
/// key). Postgres enforces its own order through a real `UNIQUE` index and has
/// no such constraint.
type DedupKey = (Uuid, String, i64, String);

/// Build the catalog existence query for a batch's distinct `gts_id`s:
/// `SELECT gts_id FROM usage_type_catalog WHERE gts_id IN (?, ?, …)`.
///
/// One `SqlBind::Str` per id, in `BTreeSet` order, so the placeholder and bind
/// sequences are deterministic and every caller-supplied identifier reaches the
/// server as a bound parameter rather than SQL text.
///
/// Like [`ChRecordStore::check_catalog_existence`] this needs no version
/// resolution — it asks only which ids are present, and unmerged duplicate
/// copies of one type are collapsed by the caller's `HashSet`.
fn catalog_lookup_sql(gts_ids: &BTreeSet<&str>) -> (String, SqlCtx) {
    let mut ctx = SqlCtx::new();
    let placeholders = gts_ids
        .iter()
        .map(|gts_id| {
            ctx.push(SqlBind::Str((*gts_id).to_owned()));
            "?"
        })
        .collect::<Vec<_>>()
        .join(", ");
    (
        format!("SELECT gts_id FROM usage_type_catalog WHERE gts_id IN ({placeholders})"),
        ctx,
    )
}

/// Apply the batch catalog check: every record whose `gts_id` is not in
/// `known` gets `UsageTypeNotFound` in its own slot; the input indices of the
/// rest are returned in input order for the dedup step.
fn split_by_catalog(
    records: &[UsageRecord],
    known: &HashSet<String>,
    outcomes: &mut [Option<Result<UsageRecord, UsageCollectorPluginError>>],
) -> Vec<usize> {
    let mut passed = Vec::with_capacity(records.len());
    for (idx, record) in records.iter().enumerate() {
        if known.contains(record.gts_id.as_ref()) {
            passed.push(idx);
        } else {
            outcomes[idx] = Some(Err(UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: record.gts_id.clone(),
            }));
        }
    }
    passed
}

/// Turn the per-slot outcome table into the SPI's positional result vector.
///
/// Every slot is filled by the time this runs; an empty one is an invariant
/// break and is reported as such rather than dropped or reordered.
fn finalize_outcomes(
    outcomes: Vec<Option<Result<UsageRecord, UsageCollectorPluginError>>>,
    records: &[UsageRecord],
) -> Vec<Result<UsageRecord, UsageCollectorPluginError>> {
    outcomes
        .into_iter()
        .enumerate()
        .map(|(idx, outcome)| {
            outcome.unwrap_or_else(|| {
                Err(dedup_invariant_break(
                    &records[idx],
                    "batch index unresolved after batch processing (invariant break)",
                ))
            })
        })
        .collect()
}

fn record_dedup_key(r: &UsageRecord) -> DedupKey {
    (
        r.tenant_id,
        r.gts_id.as_ref().to_owned(),
        EpochMicros::from(r.created_at).0,
        r.idempotency_key.as_str().to_owned(),
    )
}

fn row_dedup_key(r: &UsageRecordRow) -> DedupKey {
    (
        r.tenant_id,
        r.gts_id.clone(),
        r.created_at,
        r.idempotency_key.clone(),
    )
}

/// Pick between two stored rows that share a [`DedupKey`], preferring the one
/// whose `id` matches `expected_id`.
///
/// `ClickHouse` has no `UNIQUE` constraint, so rows written before the dedup
/// lookup was keyed on the canonical tuple can leave two rows sharing a tuple
/// with different `id`s. Both survive version resolution — distinct `id`s are
/// distinct sort keys, so neither `ReplacingMergeTree` nor a `LIMIT 1 BY` over
/// the sort key ever collapses them together.
///
/// The exact-`id` match wins so an honest retry is still absorbed when a
/// corrupt twin exists; otherwise the lowest `id` wins, which makes the choice
/// deterministic rather than dependent on which part `ClickHouse` reads first.
fn prefer_dedup_row(
    current: Option<UsageRecordRow>,
    candidate: UsageRecordRow,
    expected_id: Uuid,
) -> UsageRecordRow {
    let Some(current) = current else {
        return candidate;
    };
    if current.id == expected_id {
        return current;
    }
    if candidate.id == expected_id || candidate.id < current.id {
        return candidate;
    }
    current
}

/// Build an `Internal` error noting a dedup invariant break and log it at
/// `error` level so it is observable without exposing identifiers to callers.
fn dedup_invariant_break(record: &UsageRecord, msg: &'static str) -> UsageCollectorPluginError {
    tracing::error!(
        tenant_id = %record.tenant_id,
        gts_id = %record.gts_id.as_ref(),
        idempotency_key = %record.idempotency_key.as_str(),
        "{msg}"
    );
    UsageCollectorPluginError::internal(msg)
}

/// Build a fresh, independently-owned copy of an error for embedding into
/// every outcome slot it applies to.
///
/// [`UsageCollectorPluginError`] is intentionally not `Clone` (it is a
/// foundation-owned SPI contract type — `cpt-cf-usage-collector-dod-*
/// -plugin-contract-stability`), so [`ChRecordStore::create_batch`]
/// reconstructs an equivalent value per variant instead of cloning a shared
/// instance. Its producers — [`ChRecordStore::existing_usage_types`],
/// [`ChRecordStore::batch_dedup_lookup`] and [`ChRecordStore::insert_records`]
/// — only yield `Transient` / `Internal`; the `UsageTypeNotFound` arm is kept
/// so a caller-visible variant is never downgraded should a producer change,
/// and the fallback arm exists only because the enum is `#[non_exhaustive]`.
fn err_for_slot(err: &UsageCollectorPluginError) -> UsageCollectorPluginError {
    match err {
        UsageCollectorPluginError::Transient {
            detail,
            retry_after_seconds,
        } => UsageCollectorPluginError::Transient {
            detail: detail.clone(),
            retry_after_seconds: *retry_after_seconds,
        },
        UsageCollectorPluginError::UsageTypeNotFound { gts_id } => {
            UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: gts_id.clone(),
            }
        }
        UsageCollectorPluginError::Internal(msg) => {
            UsageCollectorPluginError::Internal(msg.clone())
        }
        other => UsageCollectorPluginError::internal(other.to_string()),
    }
}

/// Fan a failed batch write out across the outcome slots that depended on it.
///
/// Rows absorbed from storage keep whatever the dedup read decided for them —
/// a write that never landed cannot invalidate a row that was already there —
/// so only the slots backed by a composed row are rewritten.
fn apply_insert_failure(
    err: &UsageCollectorPluginError,
    row_slots: &[Vec<usize>],
    outcomes: &mut [Option<Result<UsageRecord, UsageCollectorPluginError>>],
) {
    for slots in row_slots {
        for &idx in slots {
            outcomes[idx] = Some(Err(err_for_slot(err)));
        }
    }
}

impl ChRecordStore {
    /// Resolve a dedup-key hit against an already-materialised row.
    ///
    /// A stored row is absorbed only when it is still `active` and every
    /// canonical field matches. An `inactive` stored row means the dedup key
    /// was created and then deactivated, so re-creating it must not resurrect
    /// the deactivated row as a silent absorb — the key is already bound to a
    /// record the caller cannot have back, which is exactly
    /// [`UsageCollectorPluginError::IdempotencyConflict`].
    ///
    /// The canonical fields include the record `id`. Because the lookup keys on
    /// the canonical tuple and not on `id` ([`DedupKey`]), that comparison is a
    /// load-bearing fail-closed guard rather than a tautology: a stored row
    /// whose `id` disagrees with its own dedup tuple is reported as a conflict
    /// instead of being missed and re-inserted under a key already in use.
    fn resolve_dedup_hit(
        &self,
        row: &UsageRecordRow,
        record: &UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if row.status == UsageRecordStatusCode::Inactive || !canonical_equal(row, record)? {
            self.metrics.inc_idempotency_conflict();
            return Err(UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key: record.idempotency_key.as_str().to_owned(),
                existing_id: row.id,
            });
        }
        self.metrics.inc_dedup_absorbed();
        UsageRecord::try_from(row.clone())
    }

    /// Compose the rows a batch has to write, deciding every passed record's
    /// outcome in input order.
    ///
    /// `passed` holds the input indices that survived the catalog check;
    /// `existing` is the dedup pre-read over exactly those records. A stored
    /// row for a record's dedup key decides its outcome via
    /// [`Self::resolve_dedup_hit`]. A row already composed in this batch for
    /// the same key is treated identically: since [`DedupKey`] is the
    /// canonical tuple rather than the `id`, two in-batch records sharing a
    /// tuple but carrying different payloads collide here and the second is a
    /// conflict instead of both being written under one idempotency key.
    /// Anything else becomes a new row versioned `base + <rows composed so
    /// far>`, which keeps the batch's versions distinct and increasing.
    ///
    /// Returns the rows to insert and, parallel to them, the input indices
    /// whose success depends on each row landing. Every `passed` slot in
    /// `outcomes` is filled on return. Performs no I/O.
    fn compose_batch(
        &self,
        records: &[UsageRecord],
        passed: &[usize],
        existing: &HashMap<DedupKey, UsageRecordRow>,
        base: u64,
        outcomes: &mut [Option<Result<UsageRecord, UsageCollectorPluginError>>],
    ) -> (Vec<UsageRecordRow>, Vec<Vec<usize>>) {
        let mut to_insert: Vec<UsageRecordRow> = Vec::new();
        // Row position in `to_insert` rather than a clone of the row itself.
        let mut insert_map: HashMap<DedupKey, usize> = HashMap::new();
        // Parallel to `to_insert`: the input positions whose success depends
        // on that row landing.
        let mut row_slots: Vec<Vec<usize>> = Vec::new();

        for &idx in passed {
            let record = &records[idx];
            let key = record_dedup_key(record);
            let outcome = if let Some(stored_row) = existing.get(&key) {
                self.resolve_dedup_hit(stored_row, record)
            } else if let Some(&row_idx) = insert_map.get(&key) {
                let resolved = self.resolve_dedup_hit(&to_insert[row_idx], record);
                if resolved.is_ok() {
                    row_slots[row_idx].push(idx);
                }
                resolved
            } else {
                let offset = u64::try_from(to_insert.len()).unwrap_or(u64::MAX);
                let row = UsageRecordRow::from((record, base.saturating_add(offset)));
                let row_idx = to_insert.len();
                to_insert.push(row);
                insert_map.insert(key, row_idx);
                row_slots.push(vec![idx]);
                if record.corrects_id.is_some() {
                    self.metrics.inc_compensation();
                }
                UsageRecord::try_from(to_insert[row_idx].clone())
            };
            outcomes[idx] = Some(outcome);
        }

        (to_insert, row_slots)
    }
}

#[async_trait]
impl RecordStore for ChRecordStore {
    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-ingest-dedup
    /// Create one record: catalog existence check, dedup pre-read, `INSERT`.
    ///
    /// The three statements are independent — nothing serialises two creates
    /// for the same dedup key, so both can pass the pre-read and both insert.
    /// They share a sort key (`id` is derived from the dedup tuple), so
    /// `ReplacingMergeTree(version)` converges them to the higher `version`;
    /// a differing payload inside that window is resolved last-writer-wins
    /// rather than reported as `IdempotencyConflict`. Once the earlier row is
    /// visible the pre-read sees it and the usual absorb / conflict rules
    /// apply.
    #[instrument(skip(self, record), fields(gts_id = %record.gts_id.as_ref()))]
    async fn create(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError> {
        let op_start = Instant::now();
        self.check_catalog_existence(&record.gts_id).await?;

        if let Some(row) = self.dedup_point_lookup(&record).await? {
            return self.resolve_dedup_hit(&row, &record);
        }

        if record.corrects_id.is_some() {
            self.metrics.inc_compensation();
        }

        let row = UsageRecordRow::from((&record, current_merge_version()));
        self.insert_record(&row, op_start).await?;
        UsageRecord::try_from(row)
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-ingest-batch
    /// Create a batch with three statements regardless of how many usage
    /// types it spans: one catalog existence query over the distinct
    /// `gts_id`s, one dedup pre-read over every record that passed it, one
    /// multi-row `INSERT` of the composed rows.
    ///
    /// Outcomes are positional. A failed catalog or dedup read is a failure
    /// for every slot that depended on it; a failed `INSERT` is reported only
    /// in the slots backed by a composed row, so rows absorbed from storage
    /// keep the outcome the pre-read decided. The single `INSERT` commits one
    /// part per `toYYYYMM(created_at)` partition it touches, so a reader sees
    /// all of the batch's rows for a given month or none of them; a batch
    /// spanning several months can be observed part-way through (see
    /// [`Self::insert_records`]).
    #[instrument(skip(self, records), fields(batch_size = records.len()))]
    async fn create_batch(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            tracing::warn!(
                "create_usage_records called with an empty batch (host-contract breach)"
            );
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }

        let op_start = Instant::now();
        let mut outcomes: Vec<Option<Result<UsageRecord, UsageCollectorPluginError>>> =
            (0..records.len()).map(|_| None).collect();

        // 1. One catalog existence query over the distinct usage types. A
        //    failed read decides nothing for anyone, so every slot carries it.
        let distinct: BTreeSet<&str> = records.iter().map(|r| r.gts_id.as_ref()).collect();
        let known = match self.existing_usage_types(&distinct).await {
            Ok(known) => known,
            Err(e) => return Ok(records.iter().map(|_| Err(err_for_slot(&e))).collect()),
        };
        let passed = split_by_catalog(&records, &known, &mut outcomes);

        // 2. One dedup pre-read over everything that passed the catalog check.
        let record_refs: Vec<&UsageRecord> = passed.iter().map(|&i| &records[i]).collect();
        let existing = match self.batch_dedup_lookup(&record_refs).await {
            Ok(existing) => existing,
            Err(e) => {
                for &idx in &passed {
                    outcomes[idx] = Some(Err(err_for_slot(&e)));
                }
                return Ok(finalize_outcomes(outcomes, &records));
            }
        };

        // 3. Resolve every passed record and compose the rows to write.
        let base = current_merge_version();
        let (to_insert, row_slots) =
            self.compose_batch(&records, &passed, &existing, base, &mut outcomes);

        // 4. One multi-row INSERT (a no-op when nothing was composed). A failed
        //    write does not invalidate the outcomes already decided for
        //    absorbed rows, so it is reported per backed slot rather than as a
        //    batch-level error that would discard them too.
        if let Err(e) = self
            .insert_records(&to_insert, op_start, InsertKind::Record)
            .await
        {
            tracing::warn!(error = %e, "create-batch insert failed; reporting per-record outcomes");
            apply_insert_failure(&e, &row_slots, &mut outcomes);
        }

        Ok(finalize_outcomes(outcomes, &records))
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-get
    #[instrument(skip_all, fields(record_id = %id))]
    async fn get(&self, id: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        // `LIMIT 1 BY` the whole sort key rather than a bare `LIMIT 1`: `id` is
        // normally the `UUIDv5` of the other three key columns, but a legacy
        // mismatched-`id` twin (the case `prefer_dedup_row` exists for) lets
        // one `id` span two sort keys. A global highest-`version` pick could
        // then return a row that is not its own key's resolved version.
        // Resolving per key and taking the first row is what `FINAL` plus
        // `fetch_optional` did.
        //
        // The `use_skip_indexes_if_final*` settings that used to accompany this
        // query are gone with `FINAL`: `idx_records_id` is consulted for this
        // non-sort-key-prefix `id` predicate by default once no merge-on-read
        // is involved.
        let resolve_latest = latest_by(RECORD_DEDUP_KEY);
        let sql =
            format!("SELECT {RECORD_COLUMNS} FROM usage_records WHERE id = ?{resolve_latest}");
        let row: Option<UsageRecordRow> = with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(&sql)
                .bind(id.to_string())
                .fetch_optional(),
        )
        .await?;

        match row {
            Some(row) => UsageRecord::try_from(row),
            None => Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
        }
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-list-keyset
    #[instrument(skip_all, fields(gts_id = %gts_id.as_ref()))]
    async fn list(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        let _timer =
            OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Query(QueryKind::Raw));
        self.metrics.inc_query_request(QueryKind::Raw);

        let limit =
            effective_page_size(query.limit, crate::infra::storage::query::DEFAULT_PAGE_SIZE);

        // The `WHERE` has two halves (see `Self::build_list_sql`), so the binds
        // are collected in two contexts and applied scan-then-trailing to
        // match the order the `?`s appear in the text.
        //
        // The scan half takes every predicate over version-invariant columns:
        // `gts_id`, the metadata side-channel, and the version-invariant half
        // of the caller `$filter` (`Self::push_split_filter`) — which is where
        // `tenant_id` and `created_at`, two thirds of the key prefix, arrive.
        // It is rendered twice — as the scan predicate and inside the marker
        // subquery — so its binds are applied twice.
        //
        // The trailing half takes the `status`-naming half of the `$filter`
        // and the whole keyset predicate, after the survivor predicate. Both
        // read the raw `status` column, which on every surviving row equals the
        // resolved status (`dedup::resolved_survivors`). The keyset predicate
        // stays whole — it is a lexicographic tuple over the sort order, not a
        // conjunction that can be split, and it may name `status`.
        //
        // `gts_id = ?` is bound separately and is the first `?` in each copy.
        let mut inner_ctx = SqlCtx::new();
        let mut inner_clauses: Vec<String> = vec!["gts_id = ?".to_owned()];
        let mut outer_ctx = SqlCtx::new();
        let mut outer_clauses: Vec<String> = Vec::new();

        // Metadata side-channel — pushed before `$filter` so the inner binds
        // precede the outer ones.
        Self::push_metadata_filters(metadata_filter, &mut inner_ctx, &mut inner_clauses);

        // `$filter` (validated AST → typed node → parameterised fragments),
        // split inner/outer on version-invariance.
        if let Some(expr) = query.filter() {
            let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
                .map_err(|e| UsageCollectorPluginError::internal(format!("invalid filter: {e}")))?;
            Self::push_split_filter(
                &node,
                &mut inner_ctx,
                &mut inner_clauses,
                &mut outer_ctx,
                &mut outer_clauses,
            )?;
        }

        // Keyset continuation (forward only).
        if let Some(cursor) = query.cursor.as_ref() {
            ensure_forward_cursor(cursor).map_err(UsageCollectorPluginError::internal)?;
            if cursor.f.as_deref() != query.filter_hash.as_deref() {
                return Err(UsageCollectorPluginError::internal(
                    "cursor filter hash mismatch",
                ));
            }
            if !query.order.equals_signed_tokens(&cursor.s) {
                return Err(UsageCollectorPluginError::internal(
                    "cursor sort order mismatch",
                ));
            }
            let order_pairs: Vec<(&str, bool)> = query
                .order
                .0
                .iter()
                .map(|key| (key.field.as_str(), matches!(key.dir, SortDir::Asc)))
                .collect();
            let predicate = keyset_predicate(
                &order_pairs,
                &cursor.k,
                record_column,
                |name| UsageRecordFilterField::from_name(name).map(|f| f.kind()),
                is_keyset_safe_record_field,
                &mut outer_ctx,
            )
            .map_err(UsageCollectorPluginError::internal)?;
            outer_clauses.push(predicate);
        }

        let order_sql = render_order_by(&query.order, record_column)
            .map_err(UsageCollectorPluginError::internal)?;

        let sql = Self::build_list_sql(
            &inner_clauses,
            &outer_clauses,
            &order_sql,
            limit.saturating_add(1),
        );

        // Scan binds twice (scan predicate, then the marker subquery), then the
        // trailing binds — the order the `?`s appear in `build_list_sql`.
        let mut q = self.client.query(&sql);
        for _ in 0..2 {
            q = q.bind(gts_id.as_ref());
            for b in &inner_ctx.binds {
                q = bind_one(q, b);
            }
        }
        for b in &outer_ctx.binds {
            q = bind_one(q, b);
        }

        let mut rows: Vec<UsageRecordRow> =
            with_deadline(&self.metrics, self.request_timeout, q.fetch_all()).await?;


        // Look-ahead row present → a next page exists.
        let has_next = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        if has_next {
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        let next_cursor = if has_next {
            let last = rows.last().ok_or_else(|| {
                UsageCollectorPluginError::internal("non-empty page lost its tail")
            })?;
            let keys = query
                .order
                .0
                .iter()
                .map(|key| {
                    record_row_key(last, &key.field).ok_or_else(|| {
                        UsageCollectorPluginError::internal(format!(
                            "order field `{}` has no cursor key on the row",
                            key.field
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let token = encode_next_cursor(&query.order, &keys, query.filter_hash.as_deref())
                .map_err(UsageCollectorPluginError::internal)?;
            Some(token)
        } else {
            None
        };

        let items = rows
            .into_iter()
            .map(UsageRecord::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ODataPage::new(
            items,
            PageInfo {
                next_cursor,
                prev_cursor: None,
                limit,
            },
        ))
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-query-aggregated
    #[instrument(skip_all, fields(gts_id = %gts_id.as_ref()))]
    async fn aggregate(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        spec: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let _timer = OpDurationGuard::start(
            Arc::clone(&self.metrics),
            TimedOp::Query(QueryKind::Aggregated),
        );
        self.metrics.inc_query_request(QueryKind::Aggregated);

        // Same two-half `WHERE` as `list` (see `Self::build_aggregate_sql`):
        // the version-invariant predicates form the scan half, rendered twice
        // (scan and marker subquery); the survivor predicate
        // (`dedup::active_survivors`) keeps one raw row per *active* logical
        // row, so no version-resolving level is needed and aggregating the
        // survivors directly counts neither a deactivated row nor its marker;
        // only the `status`-naming half of the caller `$filter` — which may be
        // an unsplittable `OR` / `NOT` over any allowlisted column — trails,
        // reading the raw `status` that on every survivor equals the resolved
        // one.
        //
        // Pushing the invariant half of the `$filter` into the scan half
        // matters most here: `tenant_id` and `created_at` reach this method
        // only through `$filter`, and with `gts_id` they are the whole key
        // prefix, so an aggregate over one tenant's time window is a key-range
        // scan (twice — the marker subquery prunes on the same range).
        //
        // Binds are collected in two contexts and applied scan-twice-then-
        // trailing to match the `?` order in the emitted text; the SELECT-list
        // metadata binds precede both (see below).
        let mut inner_ctx = SqlCtx::new();
        let mut inner_clauses: Vec<String> = vec!["gts_id = ?".to_owned()];
        let mut outer_ctx = SqlCtx::new();
        // Only the `status`-naming half of the caller `$filter` can land here,
        // so this vector is empty for the common call.
        let mut outer_clauses: Vec<String> = Vec::new();

        // `corrects_id` partition (plugin-spi.md §Method 3). `corrects_id` is
        // version-invariant, so this prunes the scan.
        if let Some(clause) = corrects_id_partition_clause(spec.op) {
            inner_clauses.push(clause.to_owned());
        }

        // Metadata side-channel — pushed before `$filter` so the inner binds
        // precede the outer ones.
        Self::push_metadata_filters(metadata_filter, &mut inner_ctx, &mut inner_clauses);

        // `$filter`, split inner/outer on version-invariance.
        if let Some(expr) = query.filter() {
            let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
                .map_err(|e| UsageCollectorPluginError::internal(format!("invalid filter: {e}")))?;
            Self::push_split_filter(
                &node,
                &mut inner_ctx,
                &mut inner_clauses,
                &mut outer_ctx,
                &mut outer_clauses,
            )?;
        }

        // Dimension SELECT exprs + subject-not-null guards.
        // SELECT-list binds (metadata keys) are collected separately from both
        // WHERE contexts: they appear first in the SQL text and must be applied
        // before `gts_id` and every WHERE `?`.
        let mut select_dims: Vec<String> = Vec::with_capacity(spec.group_by.len());
        let mut select_binds: Vec<SqlBind> = Vec::new();
        for dim in &spec.group_by {
            match dim {
                // Both guards are over version-invariant columns, so they
                // belong in the scan half with the rest of the pruning.
                AggregationDimension::SubjectId => {
                    inner_clauses.push("subject_id IS NOT NULL".to_owned());
                }
                AggregationDimension::SubjectType => {
                    inner_clauses.push("subject_type IS NOT NULL".to_owned());
                }
                _ => {}
            }
            let (expr, bind) = dimension_select_expr(dim);
            select_dims.push(expr);
            if let Some(b) = bind {
                select_binds.push(b);
            }
        }

        let dim_count = select_dims.len();
        let mut select_parts = select_dims;
        select_parts.push(agg_select_expr(spec.op).to_owned());
        let group_by = if dim_count == 0 {
            String::new()
        } else {
            let ordinals = (1..=dim_count)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!(" GROUP BY {ordinals}")
        };

        let limit_clause = aggregate_limit_clause(dim_count);

        // `JSONEachRow` lets us decode a result whose column count varies per call
        // without a fixed `Row` struct. Every column is aliased (`d0`…`dN`, `agg`)
        // so the JSON keys stay predictable whatever the dimension exprs are.
        let aliased_select = select_parts
            .iter()
            .enumerate()
            .map(|(i, expr)| {
                if i == dim_count {
                    format!("{expr} AS agg")
                } else {
                    format!("{expr} AS d{i}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");

        let sql = Self::build_aggregate_sql(
            &aliased_select,
            &inner_clauses,
            &outer_clauses,
            &group_by,
            &limit_clause,
        );

        // Bind order matches left-to-right `?` in `sql`: SELECT metadata keys,
        // then `gts_id` and the scan binds for the scan predicate, then
        // `gts_id` and the scan binds again for the marker subquery, then the
        // trailing (`$filter` status half) binds.
        let mut q = self.client.query(&sql);
        for b in &select_binds {
            q = bind_one(q, b);
        }
        for _ in 0..2 {
            q = q.bind(gts_id.as_ref());
            for b in &inner_ctx.binds {
                q = bind_one(q, b);
            }
        }
        for b in &outer_ctx.binds {
            q = bind_one(q, b);
        }

        // Column names for the JSON key lookup below, matching the aliases above.
        let dim_names: Vec<String> = (0..dim_count).map(|i| format!("d{i}")).collect();

        // Stream-parse `JSONEachRow` as chunks arrive. The server-side `LIMIT`
        // from `aggregate_limit_clause` still caps row count; streaming avoids
        // holding the full encoded body in memory in addition to the buckets.
        let mut parser = AggregateNdjsonParser::new(dim_names);
        let mut cursor = q
            .fetch_bytes("JSONEachRow")
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        // The deadline bounds each chunk read rather than the whole stream: a
        // large aggregate legitimately takes longer than one request budget to
        // drain, but a stall between chunks is the failure this guards.
        while let Some(chunk) =
            with_deadline(&self.metrics, self.request_timeout, cursor.next()).await?
        {
            parser.push_chunk(&chunk)?;
        }

        Ok(AggregationResult {
            buckets: parser.finish()?,
        })
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-deactivate-cascade
    #[instrument(skip_all, fields(record_id = %id))]
    async fn deactivate(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Deactivate);

        // No coordination anywhere in this plugin (DESIGN.md §3.6). For the
        // cascade specifically, the host prevents a concurrent compensation from
        // reaching create_usage_record while a deactivation is in flight
        // (plugin-spi.md Method 5 caller-side rule); a plain retry of the target
        // record racing this cascade is the documented resurrection window.

        // Step 1: Read the target + active depth-1 compensations.
        //
        // `status` is the one version-dependent column, so it must be filtered
        // *above* the resolution step. Pushing `status = 'active'` below it
        // would drop a compensation's own inactive marker while keeping the
        // superseded active row, and the resolution would then hand back that
        // stale row — an already-deactivated compensation would be cascaded a
        // second time, and the target's `AlreadyInactive` check below would
        // never fire.
        //
        // The inner query keeps `id` / `corrects_id` (both version-invariant),
        // so `idx_records_id` and `idx_records_corrects_id` still prune it.
        // Each `id` bind therefore appears twice in the text — inner then
        // outer — hence four binds for two placeholders' worth of values.
        let resolve_latest = latest_by(RECORD_DEDUP_KEY);
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM \
             (SELECT {RECORD_COLUMNS} FROM usage_records \
             WHERE id = ? OR corrects_id = ?{resolve_latest}) \
             WHERE id = ? OR (corrects_id = ? AND status = 'active')"
        );
        let rows: Vec<UsageRecordRow> = {
            with_deadline(
                &self.metrics,
                self.request_timeout,
                self.client
                    .query(&sql)
                    .bind(id.to_string())
                    .bind(id.to_string())
                    .bind(id.to_string())
                    .bind(id.to_string())
                    .fetch_all(),
            )
            .await?
        };

        // Step 2: Identify target and compensation rows.
        let target_row = rows.iter().find(|r| r.id == id);
        match target_row {
            None => return Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
            Some(r) if r.status == UsageRecordStatusCode::Inactive => {
                return Err(UsageCollectorPluginError::UsageRecordAlreadyInactive { id });
            }
            Some(_) => {}
        }

        // Step 3: Compose one versioned marker row per affected id (target +
        // active compensations). `version_higher_than` mints each marker's
        // version off the row it supersedes, so no batch-wide base version is
        // needed — the per-row offset only spaces markers whose source rows
        // already share a version.
        let markers: Vec<UsageRecordRow> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| make_inactive_marker(r, version_higher_than(r.version, i as u64), 0))
            .collect();

        // Step 4: One multi-row INSERT for all marker rows.
        //
        // ATOMICITY NOTE (DESIGN.md §3.6): a ClickHouse INSERT commits one part
        // per partition it touches, and `usage_records` is PARTITION BY
        // toYYYYMM(created_at). A marker clones its source row's `created_at` —
        // it must, because `created_at` is in the sort key and sharing it is
        // what makes the marker supersede the row — so all markers for one
        // month land in one part and flip together.
        //
        // The cascade is therefore atomic exactly when the target and its
        // depth-1 compensations fall in the same `toYYYYMM(created_at)`
        // partition, which is the common case: a compensation's event time
        // normally sits close after the record it corrects. When it does not —
        // correcting the 31st on the 1st — the markers commit as two parts and
        // a resolved read can see the target flipped while a compensation is
        // still active. The window is the gap between the two commits. It is
        // not retried or compensated for; it joins the races DESIGN.md §3.6
        // enumerates rather than being closed here, and it is the price of the
        // partition pruning and whole-partition TTL drops the partition key
        // buys (migrations/0001_init.sql).
        self.insert_records(&markers, Instant::now(), InsertKind::Marker)
            .await?;

        Ok(())
    }
}

/// Incremental `JSONEachRow` decoder for aggregate responses.
///
/// Keeps only an incomplete trailing line between chunks so peak memory is
/// dominated by the already-parsed [`AggregationBucket`]s (bounded by the
/// server-side bucket-cap `LIMIT`), not a second full copy of the body.
struct AggregateNdjsonParser {
    leftover: Vec<u8>,
    dim_names: Vec<String>,
    buckets: Vec<AggregationBucket>,
}

impl AggregateNdjsonParser {
    fn new(dim_names: Vec<String>) -> Self {
        Self {
            leftover: Vec::new(),
            dim_names,
            buckets: Vec::new(),
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> Result<(), UsageCollectorPluginError> {
        self.leftover.extend_from_slice(chunk);
        self.consume_complete_lines()
    }

    fn finish(mut self) -> Result<Vec<AggregationBucket>, UsageCollectorPluginError> {
        self.consume_complete_lines()?;
        if !self.leftover.is_empty() {
            let line = std::str::from_utf8(&self.leftover).map_err(|e| {
                UsageCollectorPluginError::internal(format!("aggregate response utf-8 error: {e}"))
            })?;
            if let Some(bucket) = parse_aggregate_line(line, &self.dim_names)? {
                self.buckets.push(bucket);
            }
            self.leftover.clear();
        }
        Ok(self.buckets)
    }

    fn consume_complete_lines(&mut self) -> Result<(), UsageCollectorPluginError> {
        let mut start = 0;
        while let Some(rel) = self.leftover[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            let line = std::str::from_utf8(&self.leftover[start..end]).map_err(|e| {
                UsageCollectorPluginError::internal(format!("aggregate response utf-8 error: {e}"))
            })?;
            if let Some(bucket) = parse_aggregate_line(line, &self.dim_names)? {
                self.buckets.push(bucket);
            }
            start = end + 1;
        }
        if start > 0 {
            self.leftover.drain(..start);
        }
        Ok(())
    }
}

/// Decode a complete `JSONEachRow` aggregate response body into
/// [`AggregationBucket`]s.
///
/// Test helper over [`AggregateNdjsonParser`]; production aggregate path
/// pushes chunks into the parser directly.
///
/// `dim_names` are the `d0`…`dN` column aliases the SELECT emitted, in
/// `group_by` order; a missing or non-string dimension decodes as an empty
/// key component. The `agg` column is accepted as a JSON string or number and
/// parsed into a [`BigDecimal`]; a JSON `null` (an empty `MIN`/`MAX`/`AVG`
/// group) becomes `None`.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when a line is not valid
/// JSON, carries an `agg` value of an unexpected JSON type, or holds an
/// unparseable decimal.
#[cfg(test)]
fn parse_aggregate_response(
    bytes: &[u8],
    dim_names: &[String],
) -> Result<Vec<AggregationBucket>, UsageCollectorPluginError> {
    let mut parser = AggregateNdjsonParser::new(dim_names.to_vec());
    parser.push_chunk(bytes)?;
    parser.finish()
}

/// Parse one NDJSON line into an optional bucket (`None` for blank lines).
fn parse_aggregate_line(
    line: &str,
    dim_names: &[String],
) -> Result<Option<AggregationBucket>, UsageCollectorPluginError> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let obj: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        UsageCollectorPluginError::internal(format!("aggregate JSON parse error: {e}"))
    })?;
    let key = dim_names
        .iter()
        .map(|dim_name| {
            obj.get(dim_name)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_default()
        })
        .collect();
    let value = match obj.get("agg") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                other => {
                    return Err(UsageCollectorPluginError::internal(format!(
                        "unexpected aggregate value type: {other}"
                    )));
                }
            };
            Some(s.parse::<BigDecimal>().map_err(|e| {
                UsageCollectorPluginError::internal(format!("aggregate value parse error: {e}"))
            })?)
        }
    };
    Ok(Some(AggregationBucket { key, value }))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "record_store_tests.rs"]
mod record_store_tests;
