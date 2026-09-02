//! Ports the storage plugin's domain depends on. The SQL adapter lives in
//! `infra::storage::foundation_store`; the domain never imports it.

use async_trait::async_trait;
use quota_enforcement_sdk::ConfigDefaults;
use toolkit_macros::domain_model;

/// What a `seed_defaults` call did.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeedReport {
    /// Rows this call inserted (0 to 3).
    pub inserted: u8,
    /// Rows that already existed.
    pub present: u8,
}

impl SeedReport {
    /// Count one row outcome.
    pub const fn count(&mut self, inserted: bool) {
        if inserted {
            self.inserted += 1;
        } else {
            self.present += 1;
        }
    }
}

/// Failure of a foundation store operation.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// A configured default does not fit its column type.
    #[error("configuration default {field}={value} does not fit the column type")]
    DefaultOutOfRange {
        /// The `ConfigDefaults` field.
        field: &'static str,
        /// The offending value.
        value: u64,
    },
    /// The database rejected the call. Detail stays in the adapter's log.
    #[error("database call failed during {operation}")]
    Unavailable {
        /// The store operation that failed.
        operation: &'static str,
    },
}

/// Schema metadata and the platform-default configuration rows.
#[async_trait]
pub trait FoundationStore: Send + Sync {
    /// The installed contract major, if the schema was ever bootstrapped.
    async fn read_installed_major(&self) -> Result<Option<i32>, StoreError>;

    /// Record `major`. Returns `true` when this call wrote the row and `false`
    /// when a concurrent bootstrap wrote it first.
    async fn record_major(&self, major: i32) -> Result<bool, StoreError>;

    /// Insert the platform-default rows that are missing.
    async fn seed_defaults(&self, defaults: &ConfigDefaults) -> Result<SeedReport, StoreError>;
}
