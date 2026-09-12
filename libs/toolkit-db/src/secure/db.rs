//! Secure database handle and runner types.
//!
//! This gear provides the primary entry point for secure database access:
//!
//! - [`Db`]: The database handle. NOT Clone, NOT storable by services.
//! - [`DbConn`]: Non-transactional runner (borrows from `Db`).
//! - [`DbTx`]: Transactional runner (lives inside transaction closure).
//!
//! # Security Model
//!
//! The transaction bypass vulnerability is prevented by multiple layers:
//!
//! 1. `Db` does NOT implement `Clone`
//! 2. `Db::transaction(self, f)` consumes `self`, making it inaccessible inside the closure
//! 3. Services receive `&impl DBRunner`, not `Db` or any factory
//! 4. **Task-local guard**: `Db::conn()` fails if called inside a transaction
//!
//! The task-local guard provides defense-in-depth: even if code obtains a `Db`
//! reference via another path (e.g., captured `Arc<AppServices>`), calling
//! `conn()` will fail at runtime with `DbError::ConnRequestedInsideTx`.
//!
//! # Example
//!
//! ```ignore
//! // In handler/entrypoint
//! let db: Db = ctx.db()?;
//!
//! // Non-transactional path
//! let conn = db.conn()?;  // Note: returns Result now
//! let user = service.get_user(&conn, &scope, id).await?;
//!
//! // Transactional path
//! let (db, result) = db.transaction(|tx| {
//!     Box::pin(async move {
//!         // Only `tx` is available here - `db` is consumed
//!         // Calling some_db.conn() here would fail with ConnRequestedInsideTx
//!         service.create_user(tx, &scope, data).await?;
//!         Ok(user_id)
//!     })
//! }).await;
//! let user_id = result?;
//! ```

use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use sea_orm::{DatabaseConnection, DatabaseTransaction, TransactionTrait};

use super::tx_config::{TxConfig, TxIsolationLevel};
use super::tx_error::TxError;
use crate::{DbError, DbHandle};

/// Default attempt budget for [`Db::transaction_with_retry`].
///
/// Three attempts is the canonical "small bounded retry" used across
/// CF/Gears that wrap writes in retry-aware transactions
/// (closure-table mutations, hierarchy invariants, write paths under
/// concurrent load). It balances:
///
/// - resilience against transient lock-contention failures detected by
///   [`crate::contention::is_retryable_contention`] (`PostgreSQL`
///   serialization failures / deadlocks, `MySQL`/`InnoDB` deadlocks,
///   `SQLite` `BUSY` / `BUSY_SNAPSHOT`);
/// - bounded latency on the hot path — the success path pays no backoff at
///   all, and even a fully-exhausted retry sequence only ever waits through
///   `RETRY_BACKOFF_MAX`-capped, millisecond-scale delays (see
///   `retry_backoff_delay`), not a real exponential-backoff schedule;
/// - predictable failure semantics — after exhausting attempts the
///   original error is returned, so callers can surface e.g.
///   `503 Service Unavailable`.
pub const DEFAULT_TX_RETRY_ATTEMPTS: u32 = 3;

/// Base delay (milliseconds) for the retry backoff computed by
/// [`retry_backoff_delay`].
///
/// Deliberately tiny: the only job of this delay is to de-synchronize two
/// transactions that just collided, not to add real latency to a path that
/// is already a bounded, small-N retry loop. See [`retry_backoff_delay`]
/// for the full rationale.
const RETRY_BACKOFF_BASE_MS: u64 = 2;

/// Growth factor applied on top of [`RETRY_BACKOFF_BASE_MS`] by
/// [`retry_backoff_delay`]'s [`ExponentialBackoff`](tokio_retry::strategy::ExponentialBackoff).
///
/// With base `2` and factor `5`, pre-jitter delays are `10ms, 20ms, 40ms,
/// …` — i.e. attempt 2 waits up to ~10ms, attempt 3 up to ~20ms, doubling
/// per further attempt (workspace default `DEFAULT_TX_RETRY_ATTEMPTS = 3`
/// only ever reaches the first of these).
const RETRY_BACKOFF_FACTOR: u64 = 5;

/// Upper bound on any single retry backoff delay, in case a caller raises
/// `max_attempts` well above [`DEFAULT_TX_RETRY_ATTEMPTS`].
const RETRY_BACKOFF_MAX: Duration = Duration::from_millis(100);

/// Delay to wait before retrying transaction attempt `next_attempt`
/// (1-based, counting the attempt about to run — so `next_attempt == 2` is
/// the delay before the *second* try, right after the first collided).
///
/// # Why back off at all
///
/// Before this, [`Db::transaction_with_retry_max`] looped straight back to
/// `BEGIN` on a retryable contention error, with no delay whatsoever. Two
/// transactions that collide once (e.g. both inserting a first row under
/// the same predicate lock) restart at essentially the same instant; with
/// nothing to de-synchronize them, they are liable to collide again on the
/// very next attempt and burn through the whole (small) attempt budget
/// within a few milliseconds without ever getting staggered. This is the
/// diagnosed root cause of a rare (reported ~1-in-14 runs) flake in a
/// Postgres-backed concurrency test of `cf-gears-resource-group`, where two
/// concurrent `add_membership` calls under `SERIALIZABLE` retry each other
/// into exhaustion instead of the invariant-preserving "one wins, one gets a
/// clean `TenantIncompatibility`" outcome. The failure
/// mode itself did not reproduce in extensive local testing (see the
/// commit message for numbers) -- this fix closes the one genuine gap the
/// diagnosis identified (this is the only retry loop in the workspace with
/// no backoff at all; see `git log --oneline -- libs/toolkit-db/src/advisory_locks.rs`
/// for the sibling lock-retry path, which always had one), on a mechanism
/// that is well established for this class of database contention retry.
///
/// # Why jitter is mandatory
///
/// Without jitter, two competing transactions computing the *same*
/// deterministic delay from the *same* backoff schedule would simply sleep
/// in lockstep and re-collide synchronized, defeating the point of backing
/// off at all. [`tokio_retry::strategy::jitter`] is already a dependency of
/// this crate -- see `advisory_locks::LockManager::try_lock` for its other,
/// pre-existing use in this same module tree -- so reusing it here adds no
/// new dependency; it applies `rand`-backed full jitter (multiplying the
/// delay by a fresh uniform `[0, 1)` sample), which is exactly the
/// de-synchronization primitive contention retries need.
///
/// # Why this shape, this small
///
/// [`ExponentialBackoff`](tokio_retry::strategy::ExponentialBackoff) from
/// `tokio-retry` (already used the same way for advisory-lock retries)
/// gives growth-with-attempt for free. The base/factor are tuned so the
/// *pre-jitter* delay is single-to-double-digit milliseconds even at the
/// first retry point, capped at [`RETRY_BACKOFF_MAX`] — enough to scatter
/// competing retries across a real window, not enough to meaningfully slow
/// down the already-rare contention path. This is unconditional on backend:
/// the same tiny delay is correct whether the caller is retrying a
/// `PostgreSQL` serialization failure, a `MySQL`/`InnoDB` deadlock, or a
/// `SQLite` `database is locked` -- all three benefit equally from not
/// re-colliding in lockstep, and none of them need more than milliseconds
/// of staggering to avoid it.
fn retry_backoff_delay(next_attempt: u32) -> Duration {
    use tokio_retry::strategy::{ExponentialBackoff, jitter};

    debug_assert!(
        next_attempt >= 2,
        "attempt 1 (the first try) is never delayed"
    );
    // 0-based index into the backoff sequence: the delay before attempt 2
    // is the sequence's first element, attempt 3's delay is the second, …
    let index = next_attempt.saturating_sub(2) as usize;

    let base = ExponentialBackoff::from_millis(RETRY_BACKOFF_BASE_MS)
        .factor(RETRY_BACKOFF_FACTOR)
        .max_delay(RETRY_BACKOFF_MAX)
        .nth(index)
        .unwrap_or(RETRY_BACKOFF_MAX);

    jitter(base)
}

/// Where inside a transaction attempt an error originated.
///
/// Exists only so [`Db::transaction_with_retry_max`]'s give-up diagnostics
/// can answer "did this fail inside the transactional work, or at commit?"
/// -- the two are indistinguishable from the returned error type `E` alone,
/// since both surface as a plain `Err(e)` to the caller.
#[derive(Clone, Copy, Debug)]
enum TxPhase {
    /// `BEGIN` (or `BEGIN ... ISOLATION LEVEL ...`) itself failed.
    Begin,
    /// The transaction body closure returned `Err`.
    Body,
    /// The body succeeded but `COMMIT` failed.
    Commit,
}

impl std::fmt::Display for TxPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Begin => "begin",
            Self::Body => "body",
            Self::Commit => "commit",
        })
    }
}

// Task-local guard to detect transaction bypass attempts.
//
// When set to `true`, any call to `Db::conn()` will fail with
// `DbError::ConnRequestedInsideTx`. This prevents code from creating
// non-transactional runners while inside a transaction closure.
tokio::task_local! {
    static IN_TX: Cell<bool>;
    static TX_ID: Cell<u64>;
    static TX_ISOLATION: Cell<Option<TxIsolationLevel>>;
}

/// Source of the ids `with_tx_guard` hands each transaction. Process-wide and
/// monotonic, not persisted or reset -- it only has to be unique for the
/// process's lifetime, to tell two transactions apart within one recorded
/// trace.
static NEXT_TX_ID: AtomicU64 = AtomicU64::new(1);

/// Check if we're currently inside a transaction context.
///
/// Returns `true` if a transaction is active in the current task.
fn is_in_transaction() -> bool {
    IN_TX.try_with(Cell::get).unwrap_or(false)
}

/// Whether the caller is inside a transaction, for query recorders that tag
/// captured SQL as in-tx or out-of-tx.
///
/// Reads the same task-local the guard enforces, so the answer is the one
/// `Db::conn()` acts on rather than a guess from the SQL text.
///
/// Gated behind the `test-support` feature; never used by production code.
#[cfg(feature = "test-support")]
#[must_use]
pub fn in_transaction_for_testing() -> bool {
    is_in_transaction()
}

/// The identity of the transaction the caller is currently inside, or `None`
/// outside any transaction.
///
/// [`in_transaction_for_testing`] alone cannot tell "these two queries ran in
/// the same transaction" from "each ran in a transaction, but not the same
/// one" -- a regression that split one transactional operation into two
/// sequential transactions would still answer `true` for every query either
/// way. This id is fresh every time [`with_tx_guard`] wraps a new attempt, so
/// query recorders can tell those two cases apart.
///
/// Gated behind the `test-support` feature; never used by production code.
#[cfg(feature = "test-support")]
#[must_use]
pub fn transaction_id_for_testing() -> Option<u64> {
    TX_ID.try_with(Cell::get).ok()
}

/// The isolation level the caller *asked for* when opening the transaction the
/// caller is currently inside.
///
/// Two nested `Option`s, and both carry meaning:
///
/// - the outer `None` means "not inside a transaction at all";
/// - `Some(None)` means "inside a transaction opened at the backend default",
///   which is what `TxConfig::default()` asks for;
/// - `Some(Some(level))` means the transaction was opened at `level`.
///
/// This records the *request*, not what the backend did with it. That is the
/// point: `SQLite` is serializable whatever it is told, so a test that asked
/// the engine would pass on `SQLite` no matter which level the service picked,
/// and the thing worth pinning is the service's own choice. An invariant that
/// several code paths must all open `SERIALIZABLE` -- so `PostgreSQL`'s SSI can
/// see them as conflicting -- is otherwise guarded by review alone: downgrading
/// one of them changes no statement, no count, and no result on `SQLite`.
///
/// Gated behind the `test-support` feature; never used by production code.
#[cfg(feature = "test-support")]
#[must_use]
pub fn transaction_isolation_for_testing() -> Option<Option<TxIsolationLevel>> {
    TX_ISOLATION.try_with(Cell::get).ok()
}

/// Name the class of a `SqlErr` without echoing its payload.
///
/// `SqlErr`'s variants carry the driver's message, which on `PostgreSQL` and
/// `MySQL` includes the conflicting column values; the discriminant alone is
/// what a log line can safely say.
fn sql_err_kind(err: Option<&sea_orm::SqlErr>) -> &'static str {
    match err {
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_)) => "unique_constraint_violation",
        Some(sea_orm::SqlErr::ForeignKeyConstraintViolation(_)) => {
            "foreign_key_constraint_violation"
        }
        Some(_) => "other_sql_error",
        None => "not_a_sql_error",
    }
}

/// Execute a closure with the transaction guard set.
///
/// Sets `IN_TX = true` for the duration of the closure, so that any call to
/// `Db::conn()` within it fails rather than silently opening a second
/// connection outside the transaction.
///
/// It also assigns the transaction a process-unique id and records the
/// isolation level it was asked to open at, both scoped to the same closure.
/// Those two are read only by the query recorder under `test-support`, though
/// the counter and the task-locals are compiled into every transaction.
async fn with_tx_guard<F, T>(isolation: Option<TxIsolationLevel>, f: F) -> T
where
    F: Future<Output = T>,
{
    let id = NEXT_TX_ID.fetch_add(1, Ordering::Relaxed);
    IN_TX
        .scope(
            Cell::new(true),
            TX_ID.scope(Cell::new(id), TX_ISOLATION.scope(Cell::new(isolation), f)),
        )
        .await
}

/// Database handle for secure operations.
///
/// # Security
///
/// This type is `Clone` to support ergonomic sharing in runtimes and service containers.
/// Transaction-bypass is still prevented by the task-local guard: any attempt to call
/// `conn()` inside a transaction closure fails with `DbError::ConnRequestedInsideTx`.
///
/// Services and repositories must NOT store this type. They should receive
/// `&impl DBRunner` as a parameter to all methods that need database access.
///
/// # Usage
///
/// ```ignore
/// // At the entrypoint (handler/command)
/// let db: Db = ctx.db()?;
///
/// // Pass runner to service methods
/// let conn = db.conn()?;
/// let result = service.do_something(&conn, &scope).await?;
///
/// // Or use a transaction
/// let (db, result) = db.transaction(|tx| {
///     Box::pin(async move {
///         service.do_something(tx, &scope).await
///     })
/// }).await;
/// ```
#[derive(Clone)]
pub struct Db {
    handle: Arc<DbHandle>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("engine", &self.handle.engine())
            .finish_non_exhaustive()
    }
}

impl Db {
    /// **INTERNAL**: Create a new `Db` from an owned `DbHandle`.
    ///
    /// This is typically called by the runtime/context layer, not by service code.
    #[must_use]
    pub(crate) fn new(handle: DbHandle) -> Self {
        Self {
            handle: Arc::new(handle),
        }
    }

    /// **INTERNAL**: Get a privileged `SeaORM` connection clone.
    ///
    /// This must not be exposed to gear code. It exists for infrastructure
    /// (migrations) inside `toolkit-db`.
    pub(crate) fn sea_internal(&self) -> DatabaseConnection {
        self.handle.sea_internal()
    }

    /// Get a reference to the underlying `DbHandle`.
    ///
    /// # Security
    ///
    /// This is `pub(crate)` to allow internal infrastructure access (migrations, etc.)
    /// but prevents service code from extracting the handle.
    /// Create a non-transactional database runner.
    ///
    /// The returned `DbConn` borrows from `self`, ensuring that while a `DbConn`
    /// exists, the `Db` cannot be used for other purposes (like starting a transaction).
    ///
    /// # Errors
    ///
    /// Returns `DbError::ConnRequestedInsideTx` if called from within a transaction
    /// closure. This prevents the transaction bypass vulnerability where code could
    /// create a non-transactional runner that persists writes outside the transaction.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let db: Db = ctx.db()?;
    /// let conn = db.conn()?;
    ///
    /// // Use conn for queries
    /// let users = Entity::find()
    ///     .secure()
    ///     .scope_with(&scope)
    ///     .all(&conn)
    ///     .await?;
    /// ```
    ///
    /// The `Result` itself is `#[must_use]`; this method does not add an extra must-use
    /// marker to avoid clippy `double_must_use`.
    pub fn conn(&self) -> Result<DbConn<'_>, DbError> {
        if is_in_transaction() {
            return Err(DbError::ConnRequestedInsideTx);
        }
        Ok(DbConn {
            conn: self.handle.sea_internal_ref(),
        })
    }

    /// Database backend in use (`Postgres` / `MySql` / `Sqlite`).
    ///
    /// Required by [`crate::contention::is_retryable_contention`] to scope
    /// retryable-error detection to the correct engine.
    #[must_use]
    pub fn backend(&self) -> sea_orm::DbBackend {
        self.handle.sea_internal_ref().get_database_backend()
    }

    // --- Advisory locks (forwarded, no `DbHandle` exposure) ---

    /// Acquire an advisory lock with the given key and gear namespace.
    ///
    /// # Errors
    /// Returns an error if the lock cannot be acquired.
    pub async fn lock(&self, gear: &str, key: &str) -> crate::Result<crate::DbLockGuard> {
        self.handle.lock(gear, key).await
    }

    /// Remove a lock marker left by a process that died holding `key`, so the
    /// key can be acquired again. Only meaningful for the file backend behind
    /// `SQLite`; a no-op elsewhere. Returns whether a marker was removed.
    ///
    /// # Errors
    /// Returns an error if a marker exists but cannot be removed.
    pub async fn break_stale_lock(&self, gear: &str, key: &str) -> crate::Result<bool> {
        self.handle.break_stale_lock(gear, key).await
    }

    /// Try to acquire an advisory lock with configurable retry/backoff policy.
    ///
    /// # Errors
    /// Returns an error if an unrecoverable lock error occurs.
    pub async fn try_lock(
        &self,
        gear: &str,
        key: &str,
        config: crate::LockConfig,
    ) -> crate::Result<Option<crate::DbLockGuard>> {
        self.handle.try_lock(gear, key, config).await
    }

    /// Execute a closure inside a database transaction (borrowed form).
    ///
    /// This variant keeps the call site ergonomic for service containers that store a
    /// reusable DB entrypoint (e.g. `DBProvider`) without exposing `DbHandle`.
    ///
    /// # Security
    ///
    /// The task-local guard is still enforced: any call to `Db::conn()` within the closure
    /// will fail with `DbError::ConnRequestedInsideTx`.
    ///
    /// # Errors
    ///
    /// Returns `DbError` if:
    /// - starting the transaction fails
    /// - the closure returns an error
    /// - commit fails (rollback is attempted on closure error)
    pub async fn transaction_ref<F, T>(&self, f: F) -> Result<T, DbError>
    where
        F: for<'a> FnOnce(
                &'a DbTx<'a>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'a>>
            + Send,
        T: Send + 'static,
    {
        let txn = self.handle.sea_internal_ref().begin().await?;
        let tx = DbTx { tx: &txn };

        // Run the closure with the transaction guard set
        let res = with_tx_guard(None, f(&tx)).await;

        match res {
            Ok(v) => {
                txn.commit().await?;
                Ok(v)
            }
            Err(e) => {
                _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    /// Execute a closure inside a database transaction, mapping infrastructure errors into `E`.
    ///
    /// This is the preferred building block for service-facing entrypoints (like `DBProvider`)
    /// that must return **domain** errors while still acquiring connections internally.
    ///
    /// - The transaction closure returns `Result<T, E>` (domain error).
    /// - Begin/commit failures are `DbError` and are mapped via `E: From<DbError>`.
    ///
    /// # Security
    ///
    /// The task-local guard is enforced for the duration of the closure.
    ///
    /// # Errors
    ///
    /// Returns `E` if:
    /// - starting the transaction fails (mapped from `DbError`)
    /// - the closure returns an error
    /// - commit fails (mapped from `DbError`)
    pub async fn transaction_ref_mapped<F, T, E>(&self, f: F) -> Result<T, E>
    where
        E: From<DbError> + Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>
            + Send,
        T: Send + 'static,
    {
        let txn = self
            .handle
            .sea_internal_ref()
            .begin()
            .await
            .map_err(DbError::from)
            .map_err(E::from)?;
        let tx = DbTx { tx: &txn };

        // Run the closure with the transaction guard set
        let res = with_tx_guard(None, f(&tx)).await;

        match res {
            Ok(v) => {
                txn.commit().await.map_err(DbError::from).map_err(E::from)?;
                Ok(v)
            }
            Err(e) => {
                _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    /// Execute a closure inside a database transaction with custom configuration
    /// (isolation level, access mode), mapping infrastructure errors into `E`.
    ///
    /// This is the preferred building block for service-facing entrypoints (like `DBProvider`)
    /// that must return **domain** errors and need non-default transaction settings
    /// (e.g., `SERIALIZABLE` isolation).
    ///
    /// # Security
    ///
    /// The task-local guard is enforced for the duration of the closure.
    ///
    /// # Errors
    ///
    /// Returns `E` if:
    /// - starting the transaction fails (mapped from `DbError`)
    /// - the closure returns an error
    /// - commit fails (mapped from `DbError`)
    pub async fn transaction_ref_mapped_with_config<F, T, E>(
        &self,
        tx_config: TxConfig,
        f: F,
    ) -> Result<T, E>
    where
        E: From<DbError> + Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>
            + Send,
        T: Send + 'static,
    {
        self.transaction_ref_mapped_with_config_tagged(tx_config, f)
            .await
            .map_err(|(e, _phase)| e)
    }

    /// Like [`Self::transaction_ref_mapped_with_config`], but also reports
    /// *where* in the transaction lifecycle the error came from.
    ///
    /// Exists solely so [`Self::transaction_with_retry_max`] can distinguish
    /// a body failure from a commit failure in its give-up diagnostics
    /// without duplicating the begin/run/commit sequence --
    /// `transaction_ref_mapped_with_config` above is now a thin wrapper
    /// around this that discards the phase tag, so there is exactly one
    /// place that knows how to run this sequence.
    async fn transaction_ref_mapped_with_config_tagged<F, T, E>(
        &self,
        tx_config: TxConfig,
        f: F,
    ) -> Result<T, (E, TxPhase)>
    where
        E: From<DbError> + Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>
            + Send,
        T: Send + 'static,
    {
        use sea_orm::{AccessMode, IsolationLevel};

        let isolation: Option<IsolationLevel> = tx_config.isolation.map(Into::into);
        let access_mode: Option<AccessMode> = tx_config.access_mode.map(Into::into);

        let txn = self
            .handle
            .sea_internal_ref()
            .begin_with_config(isolation, access_mode)
            .await
            .map_err(DbError::from)
            .map_err(E::from)
            .map_err(|e| (e, TxPhase::Begin))?;
        let tx = DbTx { tx: &txn };

        // Run the closure with the transaction guard set
        let res = with_tx_guard(tx_config.isolation, f(&tx)).await;

        match res {
            Ok(v) => match txn.commit().await {
                Ok(()) => Ok(v),
                Err(commit_err) => Err((E::from(DbError::from(commit_err)), TxPhase::Commit)),
            },
            Err(e) => {
                _ = txn.rollback().await;
                Err((e, TxPhase::Body))
            }
        }
    }

    /// Execute a closure inside a transaction with bounded retries on transient
    /// lock-contention failures.
    ///
    /// Retry detection is delegated to [`crate::contention::is_retryable_contention`],
    /// which is backend-aware (`PostgreSQL` serialization failure / deadlock,
    /// `MySQL`/`InnoDB` deadlock, `SQLite` `BUSY` / `BUSY_SNAPSHOT`). The caller
    /// supplies a small `extract_db_err` accessor that reaches into the domain
    /// error `E` and returns the underlying [`sea_orm::DbErr`], if any — that is
    /// the only piece of glue the helper needs from the caller.
    ///
    /// Uses [`DEFAULT_TX_RETRY_ATTEMPTS`] as the attempt budget. Use
    /// [`Self::transaction_with_retry_max`] if you need to override the budget
    /// (typically only in tests).
    ///
    /// # Parameters
    ///
    /// - `tx_config`: Transaction configuration (isolation level + access
    ///   mode). The helper itself is isolation-agnostic — pick whichever
    ///   level the operation needs:
    ///   - [`TxConfig::default()`] — engine default. Right for retry on
    ///     `SQLite` (BUSY) or `MySQL`/`InnoDB` (deadlocks happen at any
    ///     isolation level), or for `PostgreSQL` work that doesn't need
    ///     stronger guarantees than `READ COMMITTED`.
    ///   - [`TxConfig::serializable()`] — full `SERIALIZABLE`. Right when
    ///     the body relies on predicate-level invariants (closure-table
    ///     mutations, hierarchy or uniqueness checks across rows that
    ///     concurrent writers could insert), so that conflicting reads
    ///     surface as `40001` and get retried by this helper.
    ///   - Custom `TxConfig` (e.g. `RepeatableRead` + `ReadOnly`) for
    ///     reporting paths that want repeatable snapshots without paying
    ///     the cost of `SERIALIZABLE`.
    /// - `extract_db_err`: Accessor returning `Some(&DbErr)` if the domain
    ///   error wraps a database error, `None` otherwise. Returning `None`
    ///   always short-circuits the retry loop (the failure is non-DB and
    ///   is propagated immediately).
    /// - `body`: The transactional work. Called with a fresh `&DbTx` per
    ///   attempt. Each retry runs in a brand-new transaction, so the
    ///   closure must be idempotent across attempts (any in-memory state
    ///   mutated by an earlier attempt must be reset by the closure
    ///   itself before re-running).
    ///
    /// # Behaviour
    ///
    /// 1. Begin a transaction with the given `tx_config` and invoke `body`.
    /// 2. On `Ok`, commit and return.
    /// 3. On `Err(e)`:
    ///    - if `extract_db_err(&e)` yields a `DbErr` that
    ///      [`crate::contention::is_retryable_contention`] flags as
    ///      retryable for the active backend, and attempts remain, log at
    ///      `WARN`, wait a small jittered backoff (see
    ///      `retry_backoff_delay`), and retry;
    ///    - otherwise, return the error.
    ///
    /// On exhausting all attempts the **last** error is returned. The
    /// backoff only ever runs between a failed attempt and the next retry —
    /// the success path (first-try or otherwise) never waits.
    ///
    /// # Errors
    ///
    /// Returns `E` if the transaction fails (after retries) or if any
    /// infrastructure error mapped from `DbError` occurs.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result: Result<MyType, MyError> = db
    ///     .transaction_with_retry(
    ///         TxConfig::serializable(),
    ///         MyError::db_err, // fn(&MyError) -> Option<&sea_orm::DbErr>
    ///         |tx| Box::pin(async move {
    ///             repo.do_atomic_work(tx).await?;
    ///             Ok(MyType::default())
    ///         }),
    ///     )
    ///     .await;
    /// ```
    pub async fn transaction_with_retry<T, E, X, F>(
        &self,
        tx_config: TxConfig,
        extract_db_err: X,
        body: F,
    ) -> Result<T, E>
    where
        E: From<DbError> + Send + 'static,
        T: Send + 'static,
        X: Fn(&E) -> Option<&sea_orm::DbErr> + Send,
        F: for<'a> FnMut(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>
            + Send,
    {
        self.transaction_with_retry_max(tx_config, DEFAULT_TX_RETRY_ATTEMPTS, extract_db_err, body)
            .await
    }

    /// Like [`Self::transaction_with_retry`] but with an explicit attempt
    /// budget. See that method for behaviour and parameter semantics.
    ///
    /// `max_attempts` includes the first try (so `1` disables retries). Values
    /// below `1` are clamped to `1`. Production code should call the default
    /// variant instead of hard-coding a number here; this method exists mainly
    /// for tests and for the rare case where a service has a justified reason
    /// to deviate from the workspace-wide default.
    ///
    /// # Give-up diagnostics
    ///
    /// Both ways this can give up and return `Err` -- exhausting the attempt
    /// budget on a recognized contention error, and receiving a database
    /// error that [`crate::contention::is_retryable_contention`] does *not*
    /// recognize as retryable -- look identical to a caller (a plain `Err`),
    /// but have different fixes (raise the budget / investigate contention
    /// vs. extend the contention classifier). To make them distinguishable
    /// without changing the returned value, both emit a structured
    /// [`tracing`] event (`ERROR` for budget exhaustion, `WARN` for an
    /// unrecognized error) carrying the attempt number, the budget, the
    /// backend, the `retryable` flag, which phase of the transaction failed
    /// (begin/body/commit), and the *class* of `SeaORM`'s best-effort
    /// `sql_err()` classification. Deliberately not the driver's message:
    /// on `PostgreSQL` and `MySQL` a constraint violation embeds the
    /// conflicting column values, and this is shared infrastructure whose
    /// logs every gear inherits. Only emitted when `extract_db_err` actually
    /// found a `DbErr` -- a plain non-database domain error was never a retry
    /// candidate, so logging it here on every occurrence would drown both
    /// outcomes above in noise.
    ///
    /// # Errors
    ///
    /// Returns `E` if the transaction fails (after retries) or if any
    /// infrastructure error mapped from `DbError` occurs.
    pub async fn transaction_with_retry_max<T, E, X, F>(
        &self,
        tx_config: TxConfig,
        max_attempts: u32,
        extract_db_err: X,
        mut body: F,
    ) -> Result<T, E>
    where
        E: From<DbError> + Send + 'static,
        T: Send + 'static,
        X: Fn(&E) -> Option<&sea_orm::DbErr> + Send,
        F: for<'a> FnMut(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>
            + Send,
    {
        let max = max_attempts.max(1);
        let backend = self.backend();
        let mut attempt: u32 = 1;

        loop {
            let result = self
                .transaction_ref_mapped_with_config_tagged(tx_config.clone(), |tx| body(tx))
                .await;

            match result {
                Ok(value) => return Ok(value),
                Err((e, phase)) => {
                    let db_err = extract_db_err(&e);
                    let retryable = db_err.is_some_and(|db_err| {
                        crate::contention::is_retryable_contention(backend, db_err)
                    });
                    if retryable && attempt < max {
                        let next_attempt = attempt + 1;
                        let delay = retry_backoff_delay(next_attempt);
                        tracing::warn!(
                            attempt,
                            max_attempts = max,
                            delay = ?delay,
                            "retrying transaction after retryable failure"
                        );
                        // Small jittered backoff so two transactions that
                        // just collided don't restart in lockstep and
                        // collide again -- see `retry_backoff_delay`.
                        tokio::time::sleep(delay).await;
                        attempt = next_attempt;
                        continue;
                    }

                    // Giving up. `e` is returned unchanged below -- this
                    // block only *observes* the decision already made
                    // above, it never changes it (no behavior/backoff
                    // change here, diagnostics only).
                    //
                    // Only a recognized database error is worth a give-up
                    // event: a plain domain error unrelated to the database
                    // (validation failure, not-found, ...) was never a retry
                    // candidate in the first place -- logging every such
                    // exit here would drown the two outcomes this exists to
                    // make visible in routine noise.
                    if let Some(db_err) = db_err {
                        if retryable {
                            // attempt >= max here: this *was* a recognized
                            // contention error, but the budget ran out
                            // before another attempt was possible -- the
                            // scenario `retry_backoff_delay`'s doc comment
                            // diagnosed (and this backoff was meant to
                            // prevent) for
                            // `membership_first_write_race_exactly_one_tenant_wins`.
                            tracing::error!(
                                attempt,
                                max_attempts = max,
                                backend = ?backend,
                                phase = %phase,
                                retryable,
                                // Same reasoning as the WARN arm below, and
                                // the same two fields: neither `DbErr`'s
                                // `Display` nor `SqlErr`'s payload may reach
                                // a log line from shared infrastructure. This
                                // arm runs at ERROR, so it is the more likely
                                // of the two to be shipped and retained.
                                sql_err = sql_err_kind(db_err.sql_err().as_ref()),
                                "transaction retry budget exhausted"
                            );
                        } else {
                            // `is_retryable_contention` looked at this exact
                            // error and said no. Distinct from budget
                            // exhaustion above: the fix here, if any, is in
                            // that classifier, not in the attempt budget or
                            // backoff.
                            tracing::warn!(
                                attempt,
                                max_attempts = max,
                                backend = ?backend,
                                phase = %phase,
                                retryable,
                                // Only the classification, never the driver's
                                // message. Both `DbErr`'s `Display` and
                                // `SqlErr`'s payload embed the offending
                                // values on PostgreSQL and MySQL -- a unique
                                // violation renders as
                                // `Key (email)=(user@example.com) already
                                // exists`. This is shared infrastructure, so
                                // that would put user data in the logs of
                                // every gear that opens a transaction. The
                                // error itself is returned to the caller,
                                // which is where the detail belongs.
                                sql_err = sql_err_kind(db_err.sql_err().as_ref()),
                                "transaction failed with a database error not recognized as retryable"
                            );
                        }
                    }

                    return Err(e);
                }
            }
        }
    }

    /// Execute a closure inside a database transaction.
    ///
    /// # Security
    ///
    /// This method **consumes** `self` and returns it after the transaction completes.
    /// This is critical for security: inside the closure, the original `Db` is not
    /// accessible, so code cannot call `db.conn()` to create a non-transactional runner.
    ///
    /// Additionally, a task-local guard is set during the transaction, so any call
    /// to `conn()` on *any* `Db` instance will fail with `DbError::ConnRequestedInsideTx`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let db: Db = ctx.db()?;
    ///
    /// let (db, result) = db.transaction(|tx| {
    ///     Box::pin(async move {
    ///         // Only `tx` is available here
    ///         service.create_user(tx, &scope, data).await?;
    ///         Ok(user_id)
    ///     })
    /// }).await;
    ///
    /// let user_id = result?;
    /// ```
    ///
    /// # Returns
    ///
    /// Returns `(Self, Result<T>)` where:
    /// - `Self` is always returned (even on error) so the connection can be reused
    /// - `Result<T>` contains the transaction result or error
    pub async fn transaction<F, T>(self, f: F) -> (Self, anyhow::Result<T>)
    where
        F: for<'a> FnOnce(
                &'a DbTx<'a>,
            )
                -> Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>
            + Send,
        T: Send + 'static,
    {
        let txn = match self.handle.sea_internal_ref().begin().await {
            Ok(t) => t,
            Err(e) => return (self, Err(e.into())),
        };
        let tx = DbTx { tx: &txn };

        // Run the closure with the transaction guard set
        let res = with_tx_guard(None, f(&tx)).await;

        match res {
            Ok(v) => match txn.commit().await {
                Ok(()) => (self, Ok(v)),
                Err(e) => (self, Err(e.into())),
            },
            Err(e) => {
                _ = txn.rollback().await;
                (self, Err(e))
            }
        }
    }

    /// Execute a transaction with typed domain errors.
    ///
    /// This variant separates infrastructure errors (connection issues, commit failures)
    /// from domain errors returned by the closure.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (db, result) = db.in_transaction(|tx| {
    ///     Box::pin(async move {
    ///         service.create_user(tx, &scope, data).await
    ///     })
    /// }).await;
    ///
    /// match result {
    ///     Ok(user) => println!("Created: {:?}", user),
    ///     Err(TxError::Domain(e)) => println!("Business error: {}", e),
    ///     Err(TxError::Infra(e)) => println!("DB error: {}", e),
    /// }
    /// ```
    pub async fn in_transaction<T, E, F>(self, f: F) -> (Self, Result<T, TxError<E>>)
    where
        T: Send + 'static,
        E: std::fmt::Debug + std::fmt::Display + Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>
            + Send,
    {
        use super::tx_error::InfraError;

        let txn = match self.handle.sea_internal_ref().begin().await {
            Ok(txn) => txn,
            Err(e) => return (self, Err(TxError::Infra(InfraError::new(e.to_string())))),
        };

        let tx = DbTx { tx: &txn };

        // Run the closure with the transaction guard set
        let res = with_tx_guard(None, f(&tx)).await;

        match res {
            Ok(v) => match txn.commit().await {
                Ok(()) => (self, Ok(v)),
                Err(e) => (self, Err(TxError::Infra(InfraError::new(e.to_string())))),
            },
            Err(e) => {
                _ = txn.rollback().await;
                (self, Err(TxError::Domain(e)))
            }
        }
    }

    /// Execute a transaction with custom configuration (isolation level, access mode).
    ///
    /// # Example
    ///
    /// ```ignore
    /// use toolkit_db::secure::{TxConfig, TxIsolationLevel};
    ///
    /// let config = TxConfig {
    ///     isolation: Some(TxIsolationLevel::Serializable),
    ///     access_mode: None,
    /// };
    ///
    /// let (db, result) = db.transaction_with_config(config, |tx| {
    ///     Box::pin(async move {
    ///         // Serializable isolation
    ///         service.reconcile(tx, &scope).await
    ///     })
    /// }).await;
    /// ```
    pub async fn transaction_with_config<T, F>(
        self,
        config: TxConfig,
        f: F,
    ) -> (Self, anyhow::Result<T>)
    where
        T: Send + 'static,
        F: for<'a> FnOnce(
                &'a DbTx<'a>,
            )
                -> Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>
            + Send,
    {
        use sea_orm::{AccessMode, IsolationLevel};

        let isolation: Option<IsolationLevel> = config.isolation.map(Into::into);
        let access_mode: Option<AccessMode> = config.access_mode.map(Into::into);

        let txn = match self
            .handle
            .sea_internal_ref()
            .begin_with_config(isolation, access_mode)
            .await
        {
            Ok(t) => t,
            Err(e) => return (self, Err(e.into())),
        };
        let tx = DbTx { tx: &txn };

        // Run the closure with the transaction guard set
        let res = with_tx_guard(config.isolation, f(&tx)).await;

        match res {
            Ok(v) => match txn.commit().await {
                Ok(()) => (self, Ok(v)),
                Err(e) => (self, Err(e.into())),
            },
            Err(e) => {
                _ = txn.rollback().await;
                (self, Err(e))
            }
        }
    }

    /// Return database engine identifier for logging/tracing.
    #[must_use]
    pub fn db_engine(&self) -> &'static str {
        use sea_orm::DbBackend;

        match self.handle.sea_internal_ref().get_database_backend() {
            DbBackend::Postgres => "postgres",
            DbBackend::MySql => "mysql",
            DbBackend::Sqlite => "sqlite",
            _ => "unknown",
        }
    }
}

/// Non-transactional database runner.
///
/// This type borrows from a [`Db`] and can be used to execute queries outside
/// of a transaction context.
///
/// # Security
///
/// - NOT `Clone`: Cannot be duplicated
/// - Borrows from `Db`: While `DbConn` exists, the `Db` cannot start a transaction
/// - Cannot be constructed by user code: Only `Db::conn()` creates it
///
/// # Example
///
/// ```ignore
/// let conn = db.conn()?;
///
/// let users = Entity::find()
///     .secure()
///     .scope_with(&scope)
///     .all(&conn)
///     .await?;
/// ```
pub struct DbConn<'a> {
    pub(crate) conn: &'a DatabaseConnection,
}

impl std::fmt::Debug for DbConn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbConn").finish_non_exhaustive()
    }
}

/// Transactional database runner.
///
/// This type is only available inside a transaction closure and represents
/// an active database transaction.
///
/// # Security
///
/// - NOT `Clone`: Cannot be duplicated
/// - Lifetime-bound: Cannot escape the transaction closure
/// - Cannot be constructed by user code: Only `Db::transaction()` creates it
///
/// # Example
///
/// ```ignore
/// let (db, result) = db.transaction(|tx| {
///     Box::pin(async move {
///         Entity::insert(model)
///             .secure()
///             .scope_with_model(&scope, &model)?
///             .exec(tx)
///             .await?;
///         Ok(())
///     })
/// }).await;
/// ```
pub struct DbTx<'a> {
    pub(crate) tx: &'a DatabaseTransaction,
}

impl std::fmt::Debug for DbTx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbTx").finish_non_exhaustive()
    }
}

// NOTE: tests for `Db` live under `libs/toolkit-db/tests/` so they can be gated per-backend
// without creating feature-specific unused-import warnings in this gear.
//
// `retry_backoff_delay` is the exception: it's pure `Duration` arithmetic
// with no backend dependency (no `pg`/`sqlite`/`mysql` feature needed, no
// I/O, no actual sleeping), so it's tested in-place instead -- same pattern
// `contention.rs` uses for its own backend-agnostic pure logic in this
// crate. These tests never call `tokio::time::sleep`, so they're
// synchronous, deterministic, and effectively instantaneous.
#[cfg(test)]
mod tests {
    use super::{
        RETRY_BACKOFF_BASE_MS, RETRY_BACKOFF_FACTOR, RETRY_BACKOFF_MAX, TxPhase,
        retry_backoff_delay, sql_err_kind,
    };
    use std::collections::HashSet;
    use std::time::Duration;

    /// The deterministic pre-jitter ceiling for a given `next_attempt`,
    /// computed independently of `retry_backoff_delay` from the same
    /// constants, so this test doesn't just re-execute the production
    /// formula and trivially agree with itself.
    fn pre_jitter_ceiling(next_attempt: u32) -> Duration {
        let power = next_attempt.saturating_sub(2);
        let millis = RETRY_BACKOFF_BASE_MS
            .saturating_pow(power + 1)
            .saturating_mul(RETRY_BACKOFF_FACTOR);
        Duration::from_millis(millis).min(RETRY_BACKOFF_MAX)
    }

    #[test]
    fn delay_is_bounded_by_the_pre_jitter_ceiling() {
        // Full jitter (`tokio_retry::strategy::jitter`) multiplies by a
        // uniform `[0, 1)` sample, so every observed delay must fall in
        // `[0, ceiling]` -- never above it, never negative (Duration can't
        // be negative, but this also guards against an accidental >1.0
        // multiplier creeping in).
        for next_attempt in 2..=6 {
            let ceiling = pre_jitter_ceiling(next_attempt);
            for _ in 0..200 {
                let delay = retry_backoff_delay(next_attempt);
                assert!(
                    delay <= ceiling,
                    "attempt {next_attempt}: delay {delay:?} exceeded ceiling {ceiling:?}"
                );
            }
        }
    }

    #[test]
    fn delay_grows_with_attempt_number() {
        // Exercises `retry_backoff_delay` itself, not `pre_jitter_ceiling` --
        // that helper is deterministic by construction and would just prove
        // this crate can multiply and cap, telling us nothing about the
        // production function. A single sample from each attempt isn't
        // enough either: full jitter draws uniformly from `[0, ceiling]`, so
        // one attempt-5 sample can easily land below one attempt-2 sample.
        // The max of many samples converges to the ceiling instead, so
        // comparing maxima is a randomized stand-in for comparing ceilings.
        // Attempt 2's pre-jitter ceiling is 10ms, attempt 5's is 80ms (see
        // `pre_jitter_ceiling`); with 200 samples each, the chance that
        // max(attempt 5) fails to exceed max(attempt 2) is bounded by
        // `(10/80)^200`, indistinguishable from zero.
        const SAMPLES: usize = 200;
        let max_2 = (0..SAMPLES)
            .map(|_| retry_backoff_delay(2))
            .max()
            .expect("SAMPLES > 0");
        let max_5 = (0..SAMPLES)
            .map(|_| retry_backoff_delay(5))
            .max()
            .expect("SAMPLES > 0");
        assert!(
            max_5 > max_2,
            "expected attempt 5's backoff to exceed attempt 2's over {SAMPLES} samples: \
             max_2={max_2:?}, max_5={max_5:?}"
        );
    }

    #[test]
    fn delay_stays_small() {
        // Order-of-magnitude guard: the whole point of this backoff is to
        // desynchronize retries without adding real latency, so even the
        // worst case (attempt exhaustion with a caller-raised max_attempts)
        // must stay well under a second.
        for next_attempt in 2..=10 {
            assert!(
                retry_backoff_delay(next_attempt) <= RETRY_BACKOFF_MAX,
                "attempt {next_attempt} exceeded RETRY_BACKOFF_MAX"
            );
        }
        assert!(RETRY_BACKOFF_MAX <= Duration::from_millis(500));
    }

    #[test]
    fn jitter_is_actually_applied() {
        // If jitter were accidentally dropped (e.g. a refactor that calls
        // the base strategy directly), every sample for a fixed
        // `next_attempt` would come back identical. Full jitter over a
        // continuous `[0, 1)` sample makes repeated exact ties
        // astronomically unlikely, so requiring >1 distinct value across
        // 100 samples is a robust, non-flaky way to prove randomization is
        // wired in without asserting on any specific duration.
        let samples: HashSet<Duration> = (0..100).map(|_| retry_backoff_delay(2)).collect();
        assert!(
            samples.len() > 1,
            "expected jittered delays to vary, got {} identical samples",
            100 - samples.len() + 1
        );
    }

    #[test]
    #[should_panic(expected = "attempt 1 (the first try) is never delayed")]
    fn debug_assert_catches_delaying_the_first_attempt() {
        // `next_attempt` is 1-based counting the attempt about to run;
        // attempt 1 is the first try and must never be delayed (that would
        // put latency on the success path). This only fires in debug
        // builds (`debug_assert!`), which is how `cargo test` runs.
        let _ = retry_backoff_delay(1);
    }

    // `TxPhase` and `sql_err_kind` exist only to label a give-up log line.
    // Nothing asserts on them, so nothing would notice a label going wrong --
    // and a mislabelled phase or an error class that reads
    // "not_a_sql_error" for a constraint violation is exactly the kind of
    // thing that misleads whoever is reading the log during an incident.

    #[test]
    fn tx_phase_labels_are_distinct_and_lowercase() {
        let labels: Vec<String> = [TxPhase::Begin, TxPhase::Body, TxPhase::Commit]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(labels, vec!["begin", "body", "commit"]);
    }

    #[test]
    fn sql_err_kind_names_the_class_and_never_the_values() {
        // The payload of each variant carries the offending values on
        // PostgreSQL and MySQL; the point of this function is that none of it
        // reaches the log line.
        let unique = sea_orm::SqlErr::UniqueConstraintViolation(
            "Key (email)=(user@example.com) already exists".to_owned(),
        );
        let fk = sea_orm::SqlErr::ForeignKeyConstraintViolation(
            "Key (gts_type_id)=(7) is still referenced".to_owned(),
        );

        assert_eq!(sql_err_kind(Some(&unique)), "unique_constraint_violation");
        assert_eq!(sql_err_kind(Some(&fk)), "foreign_key_constraint_violation");
        assert_eq!(sql_err_kind(None), "not_a_sql_error");

        for kind in [
            sql_err_kind(Some(&unique)),
            sql_err_kind(Some(&fk)),
            sql_err_kind(None),
        ] {
            assert!(
                !kind.contains('@') && !kind.contains('='),
                "the class name must not carry a value: {kind}"
            );
        }
    }
}
