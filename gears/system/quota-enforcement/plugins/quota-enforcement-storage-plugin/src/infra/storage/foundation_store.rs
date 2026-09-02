//! [`SqlFoundationStore`]: the `toolkit-db` adapter behind
//! [`FoundationStore`]. Every call goes through `SecureConn`; the tables are
//! operator configuration, read under `AccessScope::allow_all()`.

use async_trait::async_trait;
use quota_enforcement_sdk::ConfigDefaults;
use toolkit_db::secure::ScopeError;
use toolkit_db::{Db, DbConn, DbError};

use crate::domain::ports::{FoundationStore, SeedReport, StoreError};
use crate::infra::storage::repo::config_repo::{self, SeedError};
use crate::infra::storage::repo::schema_repo;

const LOG_TARGET: &str = "qe.storage";

/// Foundation tables on the plugin's database.
#[derive(Clone)]
pub struct SqlFoundationStore {
    db: Db,
}

impl SqlFoundationStore {
    /// Bind the store to the plugin's database.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    fn conn(&self, operation: &'static str) -> Result<DbConn<'_>, StoreError> {
        self.db.conn().map_err(|e| unavailable(operation, &e))
    }
}

#[async_trait]
impl FoundationStore for SqlFoundationStore {
    async fn read_installed_major(&self) -> Result<Option<i32>, StoreError> {
        let conn = self.conn("read schema major")?;
        schema_repo::read_installed_major(&conn)
            .await
            .map_err(|e| unavailable_scope("read schema major", &e))
    }

    async fn record_major(&self, major: i32) -> Result<bool, StoreError> {
        let conn = self.conn("record schema major")?;
        schema_repo::record_major(&conn, major)
            .await
            .map_err(|e| unavailable_scope("record schema major", &e))
    }

    async fn seed_defaults(&self, defaults: &ConfigDefaults) -> Result<SeedReport, StoreError> {
        let conn = self.conn("seed configuration defaults")?;
        config_repo::seed_defaults(&conn, defaults)
            .await
            .map_err(|e| match e {
                SeedError::OutOfRange(range) => StoreError::DefaultOutOfRange {
                    field: range.field,
                    value: range.value,
                },
                SeedError::Db(scope) => unavailable_scope("seed configuration defaults", &scope),
            })
    }
}

fn unavailable(operation: &'static str, err: &DbError) -> StoreError {
    tracing::warn!(target: LOG_TARGET, operation, error = %err, "storage backend call failed");
    StoreError::Unavailable { operation }
}

fn unavailable_scope(operation: &'static str, err: &ScopeError) -> StoreError {
    tracing::warn!(target: LOG_TARGET, operation, error = ?err, "storage backend call failed");
    StoreError::Unavailable { operation }
}
