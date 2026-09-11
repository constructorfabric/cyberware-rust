// Updated: 2026-09-10 — implements the ADR-0006 `tenant_id/value_id` SPI.
//! SDK adapter for the static value store.
//!
//! Requests are already authorized and resolved by the host gear, so this
//! adapter keys values purely by `(tenant_id, value_id)` — see
//! `credstore_sdk::plugin_api`.
use async_trait::async_trait;
use credstore_sdk::{CredStoreError, CredStorePluginClientV1, SecretValue, TenantId, ValueId};
use toolkit_security::SecurityContext;

use super::service::Service;

/// The static plugin is a pure per-tenant, per-version value store: it
/// ignores the security context (the gear has already authorized the
/// request) and keys purely on `(tenant_id, value_id)`.
#[async_trait]
impl CredStorePluginClientV1 for Service {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
    ) -> Result<Option<SecretValue>, CredStoreError> {
        Ok(self.get_value(tenant_id, value_id))
    }

    async fn put(
        &self,
        _ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
        value: SecretValue,
    ) -> Result<(), CredStoreError> {
        self.put_value(tenant_id, value_id, value)
    }

    async fn delete(
        &self,
        _ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
    ) -> Result<(), CredStoreError> {
        self.delete_value(tenant_id, value_id);
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "client_tests.rs"]
mod client_tests;
