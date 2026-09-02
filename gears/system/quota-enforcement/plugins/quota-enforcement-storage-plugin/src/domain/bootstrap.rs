//! [`StoragePlugin`]: the foundation slice of the storage plugin.
//!
//! `bootstrap()` realizes the storage half of the gear bootstrap flow
//! (`features/foundation.md`, "Gear Bootstrap and Readiness"): it verifies
//! the installed schema major against the contract major (invariant I12) and
//! seeds the three platform-default configuration rows when missing. Both
//! steps are idempotent and safe under concurrent replicas.
//!
//! The `QuotaEnforcementStoragePluginV1` implementation is wired only once
//! every primitive the trait names exists. Until then this type is reached by
//! the plugin gear and by tests only.

use std::sync::Arc;

use quota_enforcement_sdk::{BootstrapBundle, ConfigDefaults, StorageError};
use toolkit_macros::domain_model;

use super::ports::{FoundationStore, SeedReport, StoreError};

const LOG_TARGET: &str = "qe.storage";

/// Storage plugin over its foundation store.
#[domain_model]
#[derive(Clone)]
pub struct StoragePlugin {
    store: Arc<dyn FoundationStore>,
}

impl StoragePlugin {
    /// Bind the plugin to a store.
    #[must_use]
    pub fn new(store: Arc<dyn FoundationStore>) -> Self {
        Self { store }
    }

    /// Verify the schema major and seed the default configuration rows.
    ///
    /// # Errors
    ///
    /// - [`StorageError::SchemaVersionMismatch`] when the installed major
    ///   differs from `bundle.contract_major`. Nothing is seeded.
    /// - [`StorageError::Unavailable`] when the store rejects a call.
    /// - [`StorageError::Internal`] when a configured default does not fit its
    ///   column type.
    // @cpt-flow:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1
    // @cpt-dod:cpt-cf-quota-enforcement-dod-storage-plugin:p1
    pub async fn bootstrap(&self, bundle: &BootstrapBundle) -> Result<SeedReport, StorageError> {
        let expected = i32::try_from(bundle.contract_major).map_err(|_| {
            StorageError::Internal(format!(
                "contract major {} does not fit the schema column",
                bundle.contract_major
            ))
        })?;

        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-schema
        let installed = self.ensure_schema_major(expected).await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-schema

        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-schema-if
        if installed != expected {
            // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-schema-abort
            tracing::error!(
                target: LOG_TARGET,
                installed,
                expected,
                "installed schema major does not match the contract major; refusing to serve"
            );
            return Err(StorageError::SchemaVersionMismatch {
                installed: u32::try_from(installed).unwrap_or(0),
                expected: bundle.contract_major,
            });
            // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-schema-abort
        }
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-schema-if

        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-seed-config
        let report = self
            .seed_configuration_defaults(&bundle.config_defaults)
            .await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-seed-config
        Ok(report)
    }

    /// The installed contract major, if any.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Unavailable`] when the store rejects the read.
    pub async fn installed_major(&self) -> Result<Option<i32>, StorageError> {
        self.store
            .read_installed_major()
            .await
            .map_err(StorageError::from_store)
    }

    /// Read the installed major. On a fresh schema, record `expected` first; a
    /// concurrent peer may win that write, so the value is re-read.
    async fn ensure_schema_major(&self, expected: i32) -> Result<i32, StorageError> {
        if let Some(major) = self
            .store
            .read_installed_major()
            .await
            .map_err(StorageError::from_store)?
        {
            return Ok(major);
        }
        let wrote = self
            .store
            .record_major(expected)
            .await
            .map_err(StorageError::from_store)?;
        if wrote {
            tracing::info!(target: LOG_TARGET, contract_major = expected, "recorded schema major");
        }
        Ok(self
            .store
            .read_installed_major()
            .await
            .map_err(StorageError::from_store)?
            .unwrap_or(expected))
    }

    async fn seed_configuration_defaults(
        &self,
        defaults: &ConfigDefaults,
    ) -> Result<SeedReport, StorageError> {
        let report = self
            .store
            .seed_defaults(defaults)
            .await
            .map_err(StorageError::from_store)?;
        tracing::info!(
            target: LOG_TARGET,
            inserted = report.inserted,
            present = report.present,
            "configuration defaults seeded"
        );
        Ok(report)
    }
}

/// Lift of the store port errors onto the contract error.
trait FromStore {
    fn from_store(err: StoreError) -> Self;
}

impl FromStore for StorageError {
    fn from_store(err: StoreError) -> Self {
        match err {
            StoreError::DefaultOutOfRange { .. } => Self::Internal(err.to_string()),
            StoreError::Unavailable { .. } => Self::Unavailable(err.to_string()),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "bootstrap_tests.rs"]
mod bootstrap_tests;
