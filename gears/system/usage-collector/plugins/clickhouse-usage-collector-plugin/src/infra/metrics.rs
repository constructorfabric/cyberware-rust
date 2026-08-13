//! `OpenTelemetry` metric inventory for the `ClickHouse` storage plugin.
//!
//! Realizes design ID `cpt-cf-uc-ch-plugin-design-metric-inventory`: every
//! backend-internal series this plugin owns under the `uc_clickhouse_*`
//! sub-namespace. Instrument names are the **full literal** Prometheus names
//! (`snake_case`, `_total` on counters, `_seconds` on duration histograms) with
//! **no** `.with_unit(...)` hint — matching the parent gateway and the
//! `uc_timescaledb_*` reference plugin. Histogram bucket layouts bracket the
//! NFR p95 budgets in `DESIGN.md` §1.2.
//!
//! All labels are bounded to enumerated value sets (see the `label` module):
//! unbounded identifiers (`tenant_id`, `gts_id`, record `id`, `idempotency_key`,
//! `corrects_id`, `request_id`, `trace_id`) MUST NOT appear as metric dimensions
//! — they belong in logs and traces.

use std::sync::Arc;
use std::time::Instant;

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use opentelemetry::{InstrumentationScope, KeyValue, global};

/// `OpenTelemetry` instrumentation scope (meter name) for every plugin series.
const SCOPE_NAME: &str = "uc.clickhouse";

/// Seconds-valued duration histogram bucket boundaries for backend operations
/// and cluster lock acquisition. Brackets the §1.2 p95 budgets with finer
/// low-end resolution so client-side percentiles are comparable.
const DURATION_BOUNDARIES_SECS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Row-count histogram bucket boundaries for the per-write batch size.
/// Integer-ish boundaries spanning a single row up to a large bulk write,
/// so write amortization is observable (`uc_clickhouse_batch_rows`).
const BATCH_ROW_BOUNDARIES: &[f64] = &[1.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0];

/// Bounded metric label keys and values.
///
/// Centralizing the `&'static str` constants keeps every call site on the
/// enumerated value sets from `DESIGN.md` §4 and prevents accidental
/// high-cardinality labels from leaking in.
pub mod label {
    /// Label key for the insert / lock mode dimension.
    pub const MODE: &str = "mode";
    /// `mode` value: a single-row ingest.
    pub const MODE_SINGLE: &str = "single";
    /// `mode` value: a batch (multi-row) ingest.
    pub const MODE_BATCH: &str = "batch";
    /// `mode` value: exclusive lock acquired on the create / ingest path.
    pub const MODE_CREATE: &str = "create";
    /// `mode` value: exclusive lock acquired on the catalog-delete path.
    pub const MODE_DELETE: &str = "delete";

    /// Label key for the query-kind dimension.
    pub const QUERY_KIND: &str = "query_kind";
    /// `query_kind` value: a raw (keyset) record listing.
    pub const QUERY_KIND_RAW: &str = "raw";
    /// `query_kind` value: a server-side aggregated query.
    pub const QUERY_KIND_AGGREGATED: &str = "aggregated";

    /// Label key for the backend-error classification dimension.
    pub const ERROR_CATEGORY: &str = "error_category";
    /// `error_category` value: a retryable transient backend failure.
    pub const ERROR_CATEGORY_TRANSIENT: &str = "transient";
    /// `error_category` value: a non-retryable internal backend failure.
    pub const ERROR_CATEGORY_INTERNAL: &str = "internal";
}

/// Insert-mode dimension behind the `mode` label of
/// `uc_clickhouse_insert_duration_seconds`. Closed enum so a call site cannot
/// pass an arbitrary string into the bounded label set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertMode {
    /// A single-row ingest (`mode = "single"`).
    Single,
    /// A batch (multi-row) ingest (`mode = "batch"`).
    Batch,
}

impl InsertMode {
    /// The bounded `mode` label value for this mode.
    pub(crate) const fn as_label(self) -> &'static str {
        match self {
            Self::Single => label::MODE_SINGLE,
            Self::Batch => label::MODE_BATCH,
        }
    }
}

/// Query-kind dimension behind the `query_kind` label of
/// `uc_clickhouse_query_duration_seconds` and `uc_clickhouse_query_requests_total`.
/// Closed enum so the bounded label set is enforced by the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// A raw (keyset) record listing (`query_kind = "raw"`).
    Raw,
    /// A server-side aggregated query (`query_kind = "aggregated"`).
    Aggregated,
}

impl QueryKind {
    /// The bounded `query_kind` label value for this kind.
    pub(crate) const fn as_label(self) -> &'static str {
        match self {
            Self::Raw => label::QUERY_KIND_RAW,
            Self::Aggregated => label::QUERY_KIND_AGGREGATED,
        }
    }
}

/// Backend-error classification behind the `error_category` label of
/// `uc_clickhouse_backend_errors_total`, mirroring the SPI transient-vs-internal
/// split. Closed enum so an out-of-set class is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// A retryable transient backend failure (`error_category = "transient"`).
    Transient,
    /// A non-retryable internal backend failure (`error_category = "internal"`).
    Internal,
}

impl ErrorClass {
    /// The bounded `error_category` label value for this classification.
    pub(crate) const fn as_label(self) -> &'static str {
        match self {
            Self::Transient => label::ERROR_CATEGORY_TRANSIENT,
            Self::Internal => label::ERROR_CATEGORY_INTERNAL,
        }
    }
}

/// Lock-mode dimension behind the `mode` label of
/// `uc_clickhouse_lock_acquire_duration_seconds`,
/// `uc_clickhouse_lock_contention_total`, and
/// `uc_clickhouse_lock_manager_unavailable_total`.
///
/// Closed enum so an out-of-set mode is unrepresentable at a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Exclusive lock acquired on the create path — by `RecordStore` for
    /// `create_usage_record`/`create_usage_records` and by `CatalogStore` for
    /// `create_usage_type` (via `CatalogLockPort::acquire_exclusive_for_create`).
    Create,
    /// Exclusive lock acquired by `CatalogStore` on the
    /// `delete_usage_type` path.
    Delete,
}

impl LockMode {
    /// The bounded `mode` label value for this lock mode.
    pub(crate) const fn as_label(self) -> &'static str {
        match self {
            Self::Create => label::MODE_CREATE,
            Self::Delete => label::MODE_DELETE,
        }
    }
}

/// The full `OpenTelemetry` metric inventory for the `ClickHouse` plugin.
///
/// Built once via [`Metrics::new`] and shared through an `Arc<Metrics>`; the
/// `OTel` instrument handles are themselves cheap `Arc`-backed clones, so the
/// struct is intentionally not `Clone` (share the `Arc`, not the struct).
#[derive(Debug)]
pub struct Metrics {
    // --- Histograms (seconds, unless noted) ---
    /// `uc_clickhouse_insert_duration_seconds` — labelled by `mode`.
    insert_duration: Histogram<f64>,
    /// `uc_clickhouse_query_duration_seconds` — labelled by `query_kind`.
    query_duration: Histogram<f64>,
    /// `uc_clickhouse_deactivate_duration_seconds`.
    deactivate_duration: Histogram<f64>,
    /// `uc_clickhouse_pool_acquire_duration_seconds` — time to acquire an HTTP
    /// connection from the `ClickHouse` client's underlying pool.
    pool_acquire_duration: Histogram<f64>,
    /// `uc_clickhouse_batch_rows` — row count per batch write.
    batch_rows: Histogram<f64>,
    /// `uc_clickhouse_lock_acquire_duration_seconds` — cluster lock
    /// acquisition wait time, labelled by `mode`.
    lock_acquire_duration: Histogram<f64>,

    // --- Counters ---
    /// `uc_clickhouse_dedup_absorbed_total`.
    dedup_absorbed: Counter<u64>,
    /// `uc_clickhouse_idempotency_conflicts_total`.
    idempotency_conflict: Counter<u64>,
    /// `uc_clickhouse_compensations_total`.
    compensation: Counter<u64>,
    /// `uc_clickhouse_backend_errors_total` — labelled by `error_category`.
    backend_error: Counter<u64>,
    /// `uc_clickhouse_usage_type_referenced_total`.
    usage_type_referenced: Counter<u64>,
    /// `uc_clickhouse_migration_failures_total`.
    migration_failure: Counter<u64>,
    /// `uc_clickhouse_query_requests_total` — labelled by `query_kind`.
    query_requests: Counter<u64>,
    /// `uc_clickhouse_lock_contention_total` — incremented once per lock
    /// acquisition that had to wait for a conflicting lock holder, labelled
    /// by `mode`.
    lock_contention: Counter<u64>,
    /// `uc_clickhouse_lock_manager_unavailable_total` — incremented when
    /// the cluster lock cannot be granted/released **and also** when a lease-renew
    /// check (`ensure_still_held`) fails, making session loss observable via
    /// metrics. Labelled by `mode`.
    lock_manager_unavailable: Counter<u64>,

    // --- Synchronous gauges ---
    /// `uc_clickhouse_usage_type_catalog_size` — live catalog row count.
    usage_type_catalog_size: Gauge<u64>,
    /// `uc_clickhouse_ready` — 1 = healthy, 0 = degraded.
    ///
    /// Set to 0 at the start of `init()`, to 1 once the full init sequence has
    /// succeeded, and back to 0 when the gear's cancellation token fires, so a
    /// missing series (never started) is distinguishable from a published 0
    /// (starting up, failed to start, or shut down). MUST NOT be re-armed to 1
    /// by the drain-time catalog-size refresh worker after the cancellation
    /// token fires.
    ready: Gauge<u64>,
    // Intentionally omitted instruments (each has a reason):
    //
    // - No `uc_clickhouse_dedup_stale_total`: `ClickHouse` has no server-side
    //   dedup; the plugin performs its own SELECT-before-INSERT dedup. There is no
    //   "stale dedup hit whose record had aged out" scenario analogous to the
    //   `TimescaleDB` plugin's MVCC-window race.
    //
    // - No `uc_clickhouse_batch_retries_total`: `ClickHouse` has no deadlock
    //   victim / row-level locking; bounded in-process retries after a transient
    //   backend error are not implemented for the batch insert path.
    //
    // - No `uc_clickhouse_tls_handshake_failures_total`: the `ClickHouse` HTTP
    //   client surfaces TLS errors as generic `Network` errors; there is no
    //   separate TLS-handshake error variant to distinguish.
    //
    // - No observable pool gauges (`pool_connections_active`,
    //   `pool_connections_idle`): the `clickhouse` 0.15.x crate uses `reqwest`'s
    //   internal HTTP connection pool and exposes no pool-size counters.
    //
    // - No `uc_clickhouse_orphaned_reference_detected_total`: its only
    //   legitimate incrementer is the periodic orphan-reconciliation worker,
    //   which is deferred (see `docs/features/0006-*`). The instrument is added
    //   back together with that worker.
}

impl Metrics {
    /// Build the complete metric inventory against the global meter provider.
    ///
    /// Production entry point; resolves the meter from the process-global
    /// provider and delegates to [`Metrics::with_meter`].
    #[must_use]
    pub fn new() -> Self {
        let scope = InstrumentationScope::builder(SCOPE_NAME).build();
        let meter = global::meter_with_scope(scope);
        Self::with_meter(&meter)
    }

    /// Build the inventory against an explicit [`Meter`] instead of the global
    /// provider.
    ///
    /// [`Metrics::new`] resolves the meter from the process-global provider;
    /// this seam lets a test install a local meter provider backed by an
    /// in-memory reader and assert the recorded series without mutating global
    /// state (so the assertions stay parallel-safe).
    #[must_use]
    pub fn with_meter(meter: &Meter) -> Self {
        let insert_duration = meter
            .f64_histogram("uc_clickhouse_insert_duration_seconds")
            .with_description("Duration of usage-record inserts into `ClickHouse`, by mode")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let query_duration = meter
            .f64_histogram("uc_clickhouse_query_duration_seconds")
            .with_description("Duration of usage-record queries against `ClickHouse`, by kind")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let deactivate_duration = meter
            .f64_histogram("uc_clickhouse_deactivate_duration_seconds")
            .with_description("Duration of the event-deactivation cascade in `ClickHouse`")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let pool_acquire_duration = meter
            .f64_histogram("uc_clickhouse_pool_acquire_duration_seconds")
            .with_description(
                "Time to acquire an HTTP connection from the `ClickHouse` client pool",
            )
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let batch_rows = meter
            .f64_histogram("uc_clickhouse_batch_rows")
            .with_description("Row count per batch write to `ClickHouse`")
            .with_boundaries(BATCH_ROW_BOUNDARIES.to_vec())
            .build();
        let lock_acquire_duration = meter
            .f64_histogram("uc_clickhouse_lock_acquire_duration_seconds")
            .with_description("cluster exclusive lock acquisition wait time, by mode")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();

        let dedup_absorbed = meter
            .u64_counter("uc_clickhouse_dedup_absorbed_total")
            .with_description("Exact-equality retries silently absorbed on the dedup-key conflict")
            .build();
        let idempotency_conflict = meter
            .u64_counter("uc_clickhouse_idempotency_conflicts_total")
            .with_description("Canonical-field-mismatch idempotency conflicts")
            .build();
        let compensation = meter
            .u64_counter("uc_clickhouse_compensations_total")
            .with_description("Inserts carrying a `corrects_id` (compensating records)")
            .build();
        let backend_error = meter
            .u64_counter("uc_clickhouse_backend_errors_total")
            .with_description("`ClickHouse` errors, by SPI transient/internal classification")
            .build();
        let usage_type_referenced = meter
            .u64_counter("uc_clickhouse_usage_type_referenced_total")
            .with_description(
                "Delete rejections because live usage records still reference the type",
            )
            .build();
        let migration_failure = meter
            .u64_counter("uc_clickhouse_migration_failures_total")
            .with_description("Schema-migration failures at plugin startup")
            .build();
        let query_requests = meter
            .u64_counter("uc_clickhouse_query_requests_total")
            .with_description(
                "Query requests dispatched to `ClickHouse`, by kind (workload mix observable)",
            )
            .build();
        let lock_contention = meter
            .u64_counter("uc_clickhouse_lock_contention_total")
            .with_description(
                "cluster lock acquisitions that had to wait for a conflicting holder, by mode",
            )
            .build();
        let lock_manager_unavailable = meter
            .u64_counter("uc_clickhouse_lock_manager_unavailable_total")
            .with_description(
                "cluster lock grant/release failures and session-validity check failures, by mode",
            )
            .build();
        let usage_type_catalog_size = meter
            .u64_gauge("uc_clickhouse_usage_type_catalog_size")
            .with_description("Current live usage-type catalog row count in `ClickHouse`")
            .build();
        let ready = meter
            .u64_gauge("uc_clickhouse_ready")
            .with_description("Plugin-local backend readiness (1 = migration ok, 0 = degraded)")
            .build();

        Self {
            insert_duration,
            query_duration,
            deactivate_duration,
            pool_acquire_duration,
            batch_rows,
            lock_acquire_duration,
            dedup_absorbed,
            idempotency_conflict,
            compensation,
            backend_error,
            usage_type_referenced,
            migration_failure,
            query_requests,
            lock_contention,
            lock_manager_unavailable,
            usage_type_catalog_size,
            ready,
        }
    }

    // --- Histogram recording helpers ---

    /// Record an insert duration (seconds) for the given [`InsertMode`].
    pub(crate) fn record_insert(&self, mode: InsertMode, secs: f64) {
        self.insert_duration
            .record(secs, &[KeyValue::new(label::MODE, mode.as_label())]);
    }

    /// Record a query duration (seconds) for the given [`QueryKind`].
    pub(crate) fn record_query(&self, kind: QueryKind, secs: f64) {
        self.query_duration
            .record(secs, &[KeyValue::new(label::QUERY_KIND, kind.as_label())]);
    }

    /// Record a deactivation-cascade duration (seconds).
    pub(crate) fn record_deactivate(&self, secs: f64) {
        self.deactivate_duration.record(secs, &[]);
    }

    /// Record a `ClickHouse` HTTP connection-pool acquire duration (seconds).
    pub(crate) fn record_pool_acquire(&self, secs: f64) {
        self.pool_acquire_duration.record(secs, &[]);
    }

    /// Record the row count of a batch write.
    pub(crate) fn record_batch_rows(&self, n: f64) {
        self.batch_rows.record(n, &[]);
    }

    /// Record a cluster lock-acquisition duration (seconds) for the given [`LockMode`].
    pub(crate) fn record_lock_acquire(&self, mode: LockMode, secs: f64) {
        self.lock_acquire_duration
            .record(secs, &[KeyValue::new(label::MODE, mode.as_label())]);
    }

    // --- Counter helpers ---

    /// Increment the silently-absorbed dedup retry counter.
    pub(crate) fn inc_dedup_absorbed(&self) {
        self.dedup_absorbed.add(1, &[]);
    }

    /// Increment the idempotency-conflict counter.
    pub(crate) fn inc_idempotency_conflict(&self) {
        self.idempotency_conflict.add(1, &[]);
    }

    /// Increment the compensation (`corrects_id` insert) counter.
    pub(crate) fn inc_compensation(&self) {
        self.compensation.add(1, &[]);
    }

    /// Increment the backend-error counter for the given [`ErrorClass`].
    pub(crate) fn inc_backend_error(&self, class: ErrorClass) {
        self.backend_error
            .add(1, &[KeyValue::new(label::ERROR_CATEGORY, class.as_label())]);
    }

    /// Increment the usage-type-referenced (delete rejection) counter.
    pub(crate) fn inc_usage_type_referenced(&self) {
        self.usage_type_referenced.add(1, &[]);
    }

    /// Increment the migration-failure counter.
    pub(crate) fn inc_migration_failure(&self) {
        self.migration_failure.add(1, &[]);
    }

    /// Increment the query-requests counter for the given [`QueryKind`].
    pub(crate) fn inc_query_request(&self, kind: QueryKind) {
        self.query_requests
            .add(1, &[KeyValue::new(label::QUERY_KIND, kind.as_label())]);
    }

    /// Increment the lock-contention counter for the given [`LockMode`].
    ///
    /// Called by the cluster lock manager when a lock acquisition had to
    /// wait for a conflicting holder.
    pub(crate) fn inc_lock_contention(&self, mode: LockMode) {
        self.lock_contention
            .add(1, &[KeyValue::new(label::MODE, mode.as_label())]);
    }

    /// Increment the lock-manager-unavailable counter for the given [`LockMode`].
    ///
    /// Incremented both when the cluster lock cannot be granted/released and when a
    /// session-validity check (`ensure_still_held`) fails — making session loss
    /// observable as a counter increment under the same series.
    pub(crate) fn inc_lock_manager_unavailable(&self, mode: LockMode) {
        self.lock_manager_unavailable
            .add(1, &[KeyValue::new(label::MODE, mode.as_label())]);
    }

    // --- Synchronous gauge setters ---

    /// Set the live usage-type catalog row count.
    pub(crate) fn set_catalog_size(&self, n: u64) {
        self.usage_type_catalog_size.record(n, &[]);
    }

    /// Set the plugin-local readiness gauge (1 when ready, else 0).
    ///
    /// Called with `false` at the start of `init()`, with `true` after a
    /// successful `init()`, and with `false` again by the shutdown watcher.
    /// Must NOT be called with `true` after the cancellation token fires.
    pub(crate) fn set_ready(&self, ready: bool) {
        self.ready.record(u64::from(ready), &[]);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Which duration histogram an [`OpDurationGuard`] records on drop.
#[derive(Debug, Clone, Copy)]
pub enum TimedOp {
    /// `uc_clickhouse_query_duration_seconds`, labelled by [`QueryKind`].
    Query(QueryKind),
    /// `uc_clickhouse_deactivate_duration_seconds`.
    Deactivate,
    /// `uc_clickhouse_lock_acquire_duration_seconds`, labelled by [`LockMode`].
    LockAcquire(LockMode),
}

/// Records an operation-duration histogram on drop, so the duration is captured
/// on **every** return path — including error arms that `?` out before a
/// success-only `record_*` call would run. Construct it at the top of an
/// operation and let it fall out of scope on return.
///
/// Holds an `Arc<Metrics>` (the inventory is shared via `Arc`, never deep
/// cloned); the target series is fixed at construction.
#[derive(Debug)]
pub struct OpDurationGuard {
    metrics: Arc<Metrics>,
    op: TimedOp,
    start: Instant,
}

impl OpDurationGuard {
    /// Start timing `op` against `metrics`; records on drop.
    #[must_use]
    pub(crate) fn start(metrics: Arc<Metrics>, op: TimedOp) -> Self {
        Self {
            metrics,
            op,
            start: Instant::now(),
        }
    }
}

impl Drop for OpDurationGuard {
    fn drop(&mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        match self.op {
            TimedOp::Query(kind) => self.metrics.record_query(kind, secs),
            TimedOp::Deactivate => self.metrics.record_deactivate(secs),
            TimedOp::LockAcquire(mode) => self.metrics.record_lock_acquire(mode, secs),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
