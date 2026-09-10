#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! `ToolKit` Database abstraction crate.
//!
//! This crate provides a unified interface for working with different databases
//! (`SQLite`, `PostgreSQL`, `MySQL`) through `SQLx`, with optional `SeaORM` integration.
//! It emphasizes typed connection options over DSN string manipulation and
//! implements strict security controls (e.g., `SQLite` PRAGMA whitelist).
//!
//! # Features
//! - `pg`, `mysql`, `sqlite`: enable `SQLx` backends
//! - `sea-orm`: add `SeaORM` integration for type-safe operations
//!
//! The transactional outbox pipeline ([`outbox`]) is always available — it is no
//! longer behind a preview feature flag.
//!
//! # New Architecture
//! The crate now supports:
//! - Typed `DbConnectOptions` using sqlx `ConnectOptions` (no DSN string building)
//! - Per-gear database factories with configuration merging
//! - `SQLite` PRAGMA whitelist for security
//! - Environment variable expansion in passwords and DSNs
//!
//! # Example (`DbManager` API)
//! ```rust,no_run
//! use toolkit_db::{DbManager, GlobalDatabaseConfig, DbConnConfig};
//! use figment::{Figment, providers::Serialized};
//! use std::path::PathBuf;
//! use std::sync::Arc;
//!
//! // Create configuration using Figment
//! let figment = Figment::new()
//!     .merge(Serialized::defaults(serde_json::json!({
//!         "db": {
//!             "servers": {
//!                 "main": {
//!                     "host": "localhost",
//!                     "port": 5432,
//!                     "user": "app",
//!                     "password": "${DB_PASSWORD}",
//!                     "dbname": "app_db"
//!                 }
//!             }
//!         },
//!         "test_gear": {
//!             "database": {
//!                 "server": "main",
//!                 "dbname": "gear_db"
//!             }
//!         }
//!     })));
//!
//! // Create DbManager
//! let home_dir = PathBuf::from("/app/data");
//! let db_manager = Arc::new(DbManager::from_figment(figment, home_dir).unwrap());
//!
//! // Use in runtime with DbOptions::Manager(db_manager)
//! // Gears can then use: ctx.db_required_async().await?
//! ```

#![cfg_attr(
    not(any(feature = "pg", feature = "mysql", feature = "sqlite")),
    allow(
        unused_imports,
        unused_variables,
        dead_code,
        unreachable_code,
        unused_lifetimes,
        clippy::unused_async,
    )
)]

// Re-export key types for public API
// The `Scopable` derive emits `::toolkit_db::…` paths, which do not resolve
// inside this crate. Aliasing it into the extern prelude for test builds lets
// the derive's own output be tested next to the code that relies on it, rather
// than only in downstream crates that happen to use it.
#[cfg(test)]
extern crate self as toolkit_db;

pub use advisory_locks::{DbLockError, DbLockGuard, LockConfig};

// Re-export sea_orm_migration for gears that implement DatabaseCapability
pub use sea_orm_migration;

// Core gears
pub mod advisory_locks;
pub mod config;
pub mod contention;
pub mod manager;
pub mod migration_runner;
pub mod odata;
pub mod options;
pub mod outbox;
pub mod secure;
/// Test-only helpers for DB-behavior audits (SQL query recorder). Gated behind
/// the `test-support` feature; never part of a production build.
#[cfg(feature = "test-support")]
pub mod test_support;

mod db_provider;

// Internal gears
mod pool_opts;
#[cfg(feature = "sqlite")]
mod sqlite;

// Re-export important types from new gears
pub use config::{DbConnConfig, GlobalDatabaseConfig, PoolCfg};
pub use manager::DbManager;
pub use options::redact_credentials_in_dsn;

// Re-export secure database types for convenience
pub use secure::{DEFAULT_TX_RETRY_ATTEMPTS, Db, DbConn, DbTx};

// Re-export service-friendly provider
pub use db_provider::DBProvider;

/// Connect and return a secure `Db` (no `DbHandle` exposure).
///
/// This is the public constructor intended for gear code and tests.
///
/// # Errors
///
/// Returns `DbError` if the connection fails or the DSN/options are invalid.
pub async fn connect_db(dsn: &str, opts: ConnectOpts) -> Result<Db> {
    let handle = DbHandle::connect(dsn, opts).await?;
    Ok(Db::new(handle))
}

/// Build a secure `Db` from config (no `DbHandle` exposure).
///
/// # Errors
///
/// Returns `DbError` if configuration is invalid or connection fails.
pub async fn build_db(cfg: DbConnConfig, global: Option<&GlobalDatabaseConfig>) -> Result<Db> {
    let handle = options::build_db_handle(cfg, global).await?;
    Ok(Db::new(handle))
}

/// **Test-only**: connect and attach a `SeaORM` metric callback before the
/// connection is wrapped into `Db`.
///
/// `Db`/`DBProvider` never expose the inner `SeaORM` connection, and
/// `SeaORM` captures the callback by value at connect time, so it can't be
/// attached later. Lets suites like `QueryRecorder` observe every statement.
///
/// Gated behind the `test-support` feature; never used by production code.
///
/// # Errors
///
/// Returns `DbError` under the same conditions as [`connect_db`].
#[cfg(feature = "test-support")]
pub async fn connect_db_with_metric_callback<F>(
    dsn: &str,
    opts: ConnectOpts,
    callback: F,
) -> Result<Db>
where
    F: Fn(&sea_orm::metric::Info<'_>) + Send + Sync + 'static,
{
    let mut handle = DbHandle::connect(dsn, opts).await?;
    handle.set_metric_callback_for_testing(callback);
    Ok(Db::new(handle))
}

use std::time::Duration;

// Internal imports
#[cfg(any(feature = "pg", feature = "mysql", feature = "sqlite"))]
use pool_opts::ApplyPoolOpts;
#[cfg(feature = "sqlite")]
use sqlite::{Pragmas, extract_sqlite_pragmas, is_memory_dsn, prepare_sqlite_path};

// Used for parsing SQLite DSN query parameters

#[cfg(feature = "mysql")]
use sqlx::mysql::MySqlPoolOptions;
#[cfg(feature = "pg")]
use sqlx::postgres::PgPoolOptions;
#[cfg(feature = "sqlite")]
use sqlx::sqlite::SqlitePoolOptions;
#[cfg(feature = "sqlite")]
use std::str::FromStr;

use sea_orm::DatabaseConnection;
#[cfg(feature = "mysql")]
use sea_orm::SqlxMySqlConnector;
#[cfg(feature = "pg")]
use sea_orm::SqlxPostgresConnector;
#[cfg(feature = "sqlite")]
use sea_orm::SqlxSqliteConnector;

use thiserror::Error;

/// Library-local result type.
pub type Result<T> = std::result::Result<T, DbError>;

/// Typed error for the DB handle and helpers.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("Unknown DSN: {0}")]
    UnknownDsn(String),

    #[error("Feature not enabled: {0}")]
    FeatureDisabled(&'static str),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Configuration conflict: {0}")]
    ConfigConflict(String),

    #[error("Invalid SQLite PRAGMA parameter '{key}': {message}")]
    InvalidSqlitePragma { key: String, message: String },

    #[error("Unknown SQLite PRAGMA parameter: {0}")]
    UnknownSqlitePragma(String),

    #[error("Invalid connection parameter: {0}")]
    InvalidParameter(String),

    #[error("SQLite pragma error: {0}")]
    SqlitePragma(String),

    #[error("Environment variable '{name}': {source}")]
    EnvVar {
        name: String,
        source: std::env::VarError,
    },

    #[error("URL parsing error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[cfg(any(feature = "pg", feature = "mysql", feature = "sqlite"))]
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    #[error(transparent)]
    Sea(#[from] sea_orm::DbErr),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    // make advisory_locks errors flow into DbError via `?`
    #[error(transparent)]
    Lock(#[from] advisory_locks::DbLockError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),

    /// Attempted to create a non-transactional connection inside an active transaction.
    ///
    /// This error occurs when `Db::conn()` is called from within a transaction closure.
    /// The transaction guard prevents this to avoid accidental data bypass where writes
    /// would persist outside the transaction scope.
    ///
    /// # Resolution
    ///
    /// Use the transaction runner (`tx`) provided to the closure instead of creating
    /// a new connection:
    ///
    /// ```ignore
    /// // Wrong - fails with ConnRequestedInsideTx
    /// db.transaction(|_tx| {
    ///     let conn = some_db.conn()?;  // Error!
    ///     ...
    /// });
    ///
    /// // Correct - use the transaction runner
    /// db.transaction(|tx| {
    ///     Entity::find().secure().scope_with(&scope).one(tx).await?;
    ///     ...
    /// });
    /// ```
    #[error("Cannot create non-transactional connection inside an active transaction")]
    ConnRequestedInsideTx,
}

impl From<toolkit_utils::var_expand::ExpandVarsError> for DbError {
    fn from(err: toolkit_utils::var_expand::ExpandVarsError) -> Self {
        match err {
            toolkit_utils::var_expand::ExpandVarsError::Var { name, source } => {
                Self::EnvVar { name, source }
            }
            toolkit_utils::var_expand::ExpandVarsError::Regex(msg) => Self::InvalidParameter(msg),
        }
    }
}

impl From<crate::secure::ScopeError> for DbError {
    fn from(value: crate::secure::ScopeError) -> Self {
        // Scope errors are not infra connection errors, but they still originate from the DB
        // access layer. We keep the wrapper thin and preserve the message for callers.
        DbError::Other(anyhow::Error::new(value))
    }
}

/// Supported engines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbEngine {
    Postgres,
    MySql,
    Sqlite,
}

/// Connection options.
/// Extended to cover common sqlx pool knobs; each driver applies the subset it supports.
///
/// Not `#[non_exhaustive]` because both first-party tests and downstream callers construct it
/// with struct-literal + `..Default::default()`. Adding a public field (e.g. `lock_keepalive`)
/// is therefore a source-breaking change for any caller using an exhaustive struct literal;
/// such additions are called out in the crate semver note and ride the coordinated major/minor bump.
#[derive(Clone, Debug)]
pub struct ConnectOpts {
    /// Maximum number of connections in the pool.
    pub max_conns: Option<u32>,
    /// Minimum number of connections in the pool.
    pub min_conns: Option<u32>,
    /// Timeout to acquire a connection from the pool.
    pub acquire_timeout: Option<Duration>,
    /// Idle timeout before a connection is closed.
    pub idle_timeout: Option<Duration>,
    /// Maximum lifetime for a connection.
    pub max_lifetime: Option<Duration>,
    /// Test connection health before acquire.
    pub test_before_acquire: bool,
    /// For `SQLite` file DSNs, create parent directories if missing.
    pub create_sqlite_dirs: bool,
    /// Keepalive ping interval for the dedicated advisory-lock session (PG/MySQL only).
    ///
    /// `None` uses [`advisory_locks::DEFAULT_LOCK_KEEPALIVE`].
    pub lock_keepalive: Option<Duration>,
}
impl Default for ConnectOpts {
    fn default() -> Self {
        Self {
            max_conns: Some(10),
            min_conns: None,
            acquire_timeout: Some(Duration::from_secs(30)),
            idle_timeout: None,
            max_lifetime: None,
            test_before_acquire: false,

            create_sqlite_dirs: true,
            lock_keepalive: None,
        }
    }
}

/// Main handle.
#[derive(Debug, Clone)]
pub(crate) struct DbHandle {
    engine: DbEngine,
    dsn: String,
    sea: DatabaseConnection,
    locks: advisory_locks::LockManager,
}

#[cfg(feature = "sqlite")]
const DEFAULT_SQLITE_BUSY_TIMEOUT: i32 = 5000;

impl DbHandle {
    /// Detect engine by DSN.
    ///
    /// Note: we only check scheme prefixes and don't mutate the tail (credentials etc.).
    ///
    /// # Errors
    /// Returns `DbError::UnknownDsn` if the DSN scheme is not recognized.
    pub(crate) fn detect(dsn: &str) -> Result<DbEngine> {
        // Trim only leading spaces/newlines to be forgiving with env files.
        let s = dsn.trim_start();

        // Explicit, case-sensitive checks for common schemes.
        // Add more variants as needed (e.g., postgres+unix://).
        if s.starts_with("postgres://") || s.starts_with("postgresql://") {
            Ok(DbEngine::Postgres)
        } else if s.starts_with("mysql://") {
            Ok(DbEngine::MySql)
        } else if s.starts_with("sqlite:") || s.starts_with("sqlite://") {
            Ok(DbEngine::Sqlite)
        } else {
            Err(DbError::UnknownDsn(dsn.to_owned()))
        }
    }

    /// Connect and build handle.
    ///
    /// # Errors
    /// Returns an error if the connection fails or the DSN is invalid.
    pub(crate) async fn connect(dsn: &str, opts: ConnectOpts) -> Result<Self> {
        let engine = Self::detect(dsn)?;
        #[cfg(any(feature = "pg", feature = "mysql"))]
        let lock_keepalive = opts
            .lock_keepalive
            .unwrap_or(advisory_locks::DEFAULT_LOCK_KEEPALIVE);
        match engine {
            #[cfg(feature = "pg")]
            DbEngine::Postgres => {
                let o = PgPoolOptions::new().apply(&opts);
                let pool = o.connect(dsn).await?;
                let database_scope = advisory_locks::database_scope_from_dsn(dsn);
                let locks = advisory_locks::LockManager::postgres_lazy_dsn(
                    dsn,
                    database_scope,
                    lock_keepalive,
                )?;
                let sea = SqlxPostgresConnector::from_sqlx_postgres_pool(pool);
                Ok(Self {
                    engine,
                    dsn: dsn.to_owned(),
                    sea,
                    locks,
                })
            }
            #[cfg(not(feature = "pg"))]
            DbEngine::Postgres => Err(DbError::FeatureDisabled("PostgreSQL feature not enabled")),
            #[cfg(feature = "mysql")]
            DbEngine::MySql => {
                let o = MySqlPoolOptions::new().apply(&opts);
                let pool = o.connect(dsn).await?;
                let database_scope = advisory_locks::database_scope_from_dsn(dsn);
                let locks = advisory_locks::LockManager::mysql_lazy_dsn(
                    dsn,
                    database_scope,
                    lock_keepalive,
                )?;
                let sea = SqlxMySqlConnector::from_sqlx_mysql_pool(pool);
                Ok(Self {
                    engine,
                    dsn: dsn.to_owned(),
                    sea,
                    locks,
                })
            }
            #[cfg(not(feature = "mysql"))]
            DbEngine::MySql => Err(DbError::FeatureDisabled("MySQL feature not enabled")),
            #[cfg(feature = "sqlite")]
            DbEngine::Sqlite => {
                let dsn = prepare_sqlite_path(dsn, opts.create_sqlite_dirs)?;

                // Extract SQLite PRAGMA parameters from DSN
                let (clean_dsn, pairs) = extract_sqlite_pragmas(&dsn);
                let pragmas = Pragmas::from_pairs(&pairs);

                // Build pool options with shared trait
                let o = SqlitePoolOptions::new().apply(&opts);

                // Apply SQLite pragmas using typed `sqlx` connect options (no raw SQL).
                let is_memory = is_memory_dsn(&clean_dsn);
                let mut conn_opts = sqlx::sqlite::SqliteConnectOptions::from_str(&clean_dsn)?;

                let journal_mode = if let Some(mode) = &pragmas.journal_mode {
                    match mode {
                        sqlite::pragmas::JournalMode::Delete => {
                            sqlx::sqlite::SqliteJournalMode::Delete
                        }
                        sqlite::pragmas::JournalMode::Wal => sqlx::sqlite::SqliteJournalMode::Wal,
                        sqlite::pragmas::JournalMode::Memory => {
                            sqlx::sqlite::SqliteJournalMode::Memory
                        }
                        sqlite::pragmas::JournalMode::Truncate => {
                            sqlx::sqlite::SqliteJournalMode::Truncate
                        }
                        sqlite::pragmas::JournalMode::Persist => {
                            sqlx::sqlite::SqliteJournalMode::Persist
                        }
                        sqlite::pragmas::JournalMode::Off => sqlx::sqlite::SqliteJournalMode::Off,
                    }
                } else if let Some(wal_toggle) = pragmas.wal_toggle {
                    if wal_toggle {
                        sqlx::sqlite::SqliteJournalMode::Wal
                    } else {
                        sqlx::sqlite::SqliteJournalMode::Delete
                    }
                } else if is_memory {
                    sqlx::sqlite::SqliteJournalMode::Delete
                } else {
                    sqlx::sqlite::SqliteJournalMode::Wal
                };
                conn_opts = conn_opts.journal_mode(journal_mode);

                let sync_mode = pragmas.synchronous.as_ref().map_or(
                    sqlx::sqlite::SqliteSynchronous::Normal,
                    |s| match s {
                        sqlite::pragmas::SyncMode::Off => sqlx::sqlite::SqliteSynchronous::Off,
                        sqlite::pragmas::SyncMode::Normal => {
                            sqlx::sqlite::SqliteSynchronous::Normal
                        }
                        sqlite::pragmas::SyncMode::Full => sqlx::sqlite::SqliteSynchronous::Full,
                        sqlite::pragmas::SyncMode::Extra => sqlx::sqlite::SqliteSynchronous::Extra,
                    },
                );
                conn_opts = conn_opts.synchronous(sync_mode);

                if !is_memory {
                    let busy_timeout_ms_i64 = pragmas
                        .busy_timeout_ms
                        .unwrap_or(DEFAULT_SQLITE_BUSY_TIMEOUT.into())
                        .max(0);
                    let busy_timeout_ms = u64::try_from(busy_timeout_ms_i64).unwrap_or(0);
                    conn_opts =
                        conn_opts.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms));
                }

                let pool = o.connect_with(conn_opts).await?;
                let database_scope = advisory_locks::database_scope_from_dsn(&clean_dsn);
                let locks = advisory_locks::LockManager::file(database_scope);
                let sea = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);

                Ok(Self {
                    engine,
                    dsn: clean_dsn,
                    sea,
                    locks,
                })
            }
            #[cfg(not(feature = "sqlite"))]
            DbEngine::Sqlite => Err(DbError::FeatureDisabled("SQLite feature not enabled")),
        }
    }

    /// Get the backend.
    #[must_use]
    pub fn engine(&self) -> DbEngine {
        self.engine
    }

    /// Get the DSN used for this connection.
    #[must_use]
    pub fn dsn(&self) -> &str {
        &self.dsn
    }

    // NOTE: We intentionally do not expose raw `SQLx` pools from `DbHandle`.
    // Use `SecureConn` for all application-level DB access.

    // --- SeaORM accessor ---

    /// Create a secure database wrapper for gear code.
    ///
    /// This returns a `Db` which provides controlled access to the database
    /// via `conn()` and `transaction()` methods.
    ///
    /// # Security
    ///
    /// **INTERNAL**: Get raw `SeaORM` connection for internal runtime operations.
    ///
    /// This is `pub(crate)` and should **only** be used by:
    /// - The migration runner (for executing gear migrations)
    /// - Internal infrastructure code within `toolkit-db`
    ///
    #[must_use]
    pub(crate) fn sea_internal(&self) -> DatabaseConnection {
        self.sea.clone()
    }

    /// **INTERNAL**: Get a reference to the raw `SeaORM` connection.
    ///
    /// This is `pub(crate)` and should **only** be used by:
    /// - The `Db` wrapper for creating runners
    /// - Internal infrastructure code within `toolkit-db`
    ///
    /// **NEVER expose this to gears.**
    #[must_use]
    pub(crate) fn sea_internal_ref(&self) -> &DatabaseConnection {
        &self.sea
    }

    // --- Advisory locks ---

    /// Acquire an advisory lock with the given key and gear namespace.
    ///
    /// # Errors
    /// Returns an error if the lock cannot be acquired.
    pub async fn lock(&self, gear: &str, key: &str) -> Result<DbLockGuard> {
        let guard = self.locks.lock(gear, key).await?;
        Ok(guard)
    }

    /// Try to acquire an advisory lock with configurable retry/backoff policy.
    ///
    /// # Errors
    /// Returns an error if an unrecoverable lock error occurs.
    pub async fn try_lock(
        &self,
        gear: &str,
        key: &str,
        config: LockConfig,
    ) -> Result<Option<DbLockGuard>> {
        let res = self.locks.try_lock(gear, key, config).await?;
        Ok(res)
    }

    // NOTE: We intentionally do not expose raw SQL transactions from `DbHandle`.
    // Use `SecureConn::transaction` for application-level atomic operations.

    /// **Test-only**: attach a `SeaORM` metric callback to the underlying
    /// connection. See [`connect_db_with_metric_callback`] for why this must
    /// run before the handle is wrapped into `Db`.
    #[cfg(feature = "test-support")]
    pub(crate) fn set_metric_callback_for_testing<F>(&mut self, callback: F)
    where
        F: Fn(&sea_orm::metric::Info<'_>) + Send + Sync + 'static,
    {
        self.sea.set_metric_callback(callback);
    }
}

// ===================== tests =====================

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    #[cfg(feature = "sqlite")]
    use tokio::time::Duration;

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_connection() -> Result<()> {
        let dsn = "sqlite::memory:";
        let opts = ConnectOpts::default();
        let db = DbHandle::connect(dsn, opts).await?;
        assert_eq!(db.engine(), DbEngine::Sqlite);
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_connection_with_pragma_parameters() -> Result<()> {
        // Test that SQLite connections work with PRAGMA parameters in DSN
        let dsn = "sqlite::memory:?wal=true&synchronous=NORMAL&busy_timeout=5000&journal_mode=WAL";
        let opts = ConnectOpts::default();
        let db = DbHandle::connect(dsn, opts).await?;
        assert_eq!(db.engine(), DbEngine::Sqlite);

        // Verify that the stored DSN has been cleaned (SQLite parameters removed)
        // Note: For memory databases, the DSN should still be sqlite::memory: after cleaning
        assert!(db.dsn == "sqlite::memory:" || db.dsn.starts_with("sqlite::memory:"));

        Ok(())
    }

    #[tokio::test]
    async fn test_backend_detection() {
        assert_eq!(
            DbHandle::detect("sqlite::memory:").unwrap(),
            DbEngine::Sqlite
        );
        assert_eq!(
            DbHandle::detect("postgres://localhost/test").unwrap(),
            DbEngine::Postgres
        );
        assert_eq!(
            DbHandle::detect("mysql://localhost/test").unwrap(),
            DbEngine::MySql
        );
        assert!(DbHandle::detect("unknown://test").is_err());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_advisory_lock_sqlite() -> Result<()> {
        let dsn = "sqlite:file:memdb1?mode=memory&cache=shared";
        let db = DbHandle::connect(dsn, ConnectOpts::default()).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let test_id = format!("test_basic_{now}");

        let guard1 = db.lock("test_gear", &format!("{test_id}_key1")).await?;
        let _guard2 = db.lock("test_gear", &format!("{test_id}_key2")).await?;
        let _guard3 = db
            .lock("different_gear", &format!("{test_id}_key1"))
            .await?;

        // Deterministic unlock to avoid races with async Drop cleanup
        guard1.release().await?;
        let _guard4 = db.lock("test_gear", &format!("{test_id}_key1")).await?;
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_advisory_lock_different_keys() -> Result<()> {
        let dsn = "sqlite:file:memdb_diff_keys?mode=memory&cache=shared";
        let db = DbHandle::connect(dsn, ConnectOpts::default()).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let test_id = format!("test_diff_{now}");

        let _guard1 = db.lock("test_gear", &format!("{test_id}_key1")).await?;
        let _guard2 = db.lock("test_gear", &format!("{test_id}_key2")).await?;
        let _guard3 = db.lock("other_gear", &format!("{test_id}_key1")).await?;
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_try_lock_with_config() -> Result<()> {
        let dsn = "sqlite:file:memdb2?mode=memory&cache=shared";
        let db = DbHandle::connect(dsn, ConnectOpts::default()).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let test_id = format!("test_config_{now}");

        let _guard1 = db.lock("test_gear", &format!("{test_id}_key")).await?;

        let config = LockConfig {
            max_wait: Some(Duration::from_millis(200)),
            max_retries: Some(3),
            ..Default::default()
        };

        let result = db
            .try_lock("test_gear", &format!("{test_id}_different_key"), config)
            .await?;
        assert!(
            result.is_some(),
            "expected lock acquisition for different key"
        );
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sea_internal_access() -> Result<()> {
        let dsn = "sqlite::memory:";
        let db = DbHandle::connect(dsn, ConnectOpts::default()).await?;

        // Internal method for migrations
        let _raw = db.sea_internal();
        Ok(())
    }
}
