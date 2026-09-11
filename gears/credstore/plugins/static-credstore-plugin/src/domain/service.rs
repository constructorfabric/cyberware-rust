// Updated: 2026-09-10 by Constructor Tech — rebuilt as a pure `tenant_id/value_id`
// immutable value store (ADR-0006); out-of-band seeding withdrawn.
//! Thread-safe in-memory immutable-value store.
//!
//! Keyed by `(tenant_id, value_id)` (ADR-0006 "Backend key shape") — no
//! `reference`, no sharing-derived key class, no `owner_id`. Every entry, once
//! written, is immutable: `put` on an id already present is a contract
//! violation the gear itself never issues, and this plugin defends against it
//! by rejecting the call with [`CredStoreError::Conflict`]; `delete` of an id
//! this store does not hold is success (idempotent).
use std::collections::HashMap;
use std::sync::RwLock;

use credstore_sdk::{CredStoreError, SecretValue, TenantId, ValueId};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::StaticCredStorePluginConfig;

/// In-memory backend store: `(tenant_id, value_id) -> value`.
#[domain_model]
#[derive(Debug, Default)]
struct Store {
    values: HashMap<(Uuid, Uuid), SecretValue>,
}

/// Static credstore backend.
///
/// A pure per-tenant, per-version value store implementing the
/// `CredStorePluginClientV1` contract: `get`/`put`/`delete` keyed by
/// `(tenant_id, value_id)` only. No configuration seeds values any more
/// (ADR-0006 withdraws out-of-band seeding) — the store starts empty and is
/// populated only through the gear's write protocol.
#[domain_model]
#[derive(Debug, Default)]
pub struct Service {
    inner: RwLock<Store>,
}

impl Service {
    /// Create a service from plugin configuration.
    ///
    /// Configuration carries only GTS-registration input (vendor, priority);
    /// this constructor never fails but keeps the fallible signature other
    /// plugin backends need. The store always starts empty.
    ///
    /// # Errors
    ///
    /// Never returns an error today; kept `Result` so a future backend that
    /// does validate configuration does not need a signature change.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "signature matches the plugin construction contract other backends use, \
                  which may validate configuration and fail"
    )]
    pub fn from_config(_cfg: &StaticCredStorePluginConfig) -> anyhow::Result<Self> {
        Ok(Self {
            inner: RwLock::new(Store::default()),
        })
    }

    /// Read the value stored at `(tenant_id, value_id)`, or `None` if absent.
    #[must_use]
    pub fn get_value(&self, tenant_id: &TenantId, value_id: &ValueId) -> Option<SecretValue> {
        let store = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `SecretValue` is not `Clone` (it zeroizes on drop), so reconstruct
        // from the stored bytes.
        store
            .values
            .get(&(tenant_id.0, value_id.0))
            .map(|v| SecretValue::new(v.as_bytes().to_vec()))
    }

    /// Write a brand-new, immutable entry at `(tenant_id, value_id)`.
    ///
    /// # Errors
    ///
    /// Returns [`CredStoreError::Conflict`] if an entry already exists at
    /// this id — immutability guard; the gear never reissues a `value_id` it
    /// has already written.
    pub fn put_value(
        &self,
        tenant_id: &TenantId,
        value_id: &ValueId,
        value: SecretValue,
    ) -> Result<(), CredStoreError> {
        let mut store = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (tenant_id.0, value_id.0);
        if store.values.contains_key(&key) {
            return Err(CredStoreError::Conflict);
        }
        store.values.insert(key, value);
        Ok(())
    }

    /// Remove the entry at `(tenant_id, value_id)`. A miss is a no-op — the
    /// gear treats a missing backend value as success (idempotent delete).
    pub fn delete_value(&self, tenant_id: &TenantId, value_id: &ValueId) {
        let mut store = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.values.remove(&(tenant_id.0, value_id.0));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_tests.rs"]
mod service_tests;
