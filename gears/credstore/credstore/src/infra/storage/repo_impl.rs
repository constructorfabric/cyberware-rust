//! `SeaORM`-backed implementation of [`SecretRepo`] (ADR-0006).

pub mod helpers;
mod reads;
mod writes;

#[cfg(test)]
mod repo_tests;

use async_trait::async_trait;
use credstore_sdk::{OwnerId, SecretRef, SharingMode, TenantId, ValueId};
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

pub use helpers::{CredstoreDbProvider, SecretRepoImpl};

use crate::domain::error::DomainError;
use crate::domain::secret::model::{GcEntry, GcReason, NewSecret, SecretRow};
use crate::domain::secret::repo::SecretRepo;

#[async_trait]
impl SecretRepo for SecretRepoImpl {
    async fn resolve_for_get(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        chain: &[Uuid],
    ) -> Result<Option<SecretRow>, DomainError> {
        reads::resolve_for_get(self, req_tenant, subject, key, chain).await
    }

    async fn find_own(
        &self,
        scope: &AccessScope,
        tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
    ) -> Result<Option<SecretRow>, DomainError> {
        reads::find_own(self, scope, tenant, subject, key).await
    }

    async fn find_for_write(
        &self,
        scope: &AccessScope,
        tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        sharing: SharingMode,
    ) -> Result<Option<SecretRow>, DomainError> {
        reads::find_for_write(self, scope, tenant, subject, key, sharing).await
    }

    async fn scope_includes_tenant(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<bool, DomainError> {
        Ok(reads::scope_includes_tenant(scope, tenant))
    }

    async fn gc_insert_pending(
        &self,
        value_id: ValueId,
        tenant_id: TenantId,
    ) -> Result<(), DomainError> {
        writes::gc_insert_pending(self, value_id, tenant_id).await
    }

    async fn gc_delete(&self, value_id: ValueId) -> Result<bool, DomainError> {
        writes::gc_delete(self, value_id).await
    }

    async fn gc_mark(&self, value_id: ValueId, reason: GcReason) -> Result<bool, DomainError> {
        writes::gc_mark(self, value_id, reason).await
    }

    async fn gc_list(&self, limit: u64) -> Result<Vec<GcEntry>, DomainError> {
        writes::gc_list(self, limit).await
    }

    async fn is_value_referenced(&self, value_id: ValueId) -> Result<bool, DomainError> {
        writes::is_value_referenced(self, value_id).await
    }

    async fn insert_active(&self, scope: &AccessScope, new: &NewSecret) -> Result<(), DomainError> {
        writes::insert_active(self, scope, new).await
    }

    async fn switch_value(
        &self,
        scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
        sharing: SharingMode,
        expires_at: Option<OffsetDateTime>,
        new_value_id: ValueId,
        value_fp: Vec<u8>,
        fp_key_id: i16,
    ) -> Result<Option<(SecretRow, Option<ValueId>)>, DomainError> {
        writes::switch_value(
            self,
            scope,
            id,
            expected_version,
            sharing,
            expires_at,
            new_value_id,
            value_fp,
            fp_key_id,
        )
        .await
    }

    async fn delete_by_id(
        &self,
        scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
    ) -> Result<Option<ValueId>, DomainError> {
        writes::delete_by_id(self, scope, id, expected_version).await
    }

    async fn list_expired(&self, limit: u64) -> Result<Vec<SecretRow>, DomainError> {
        reads::list_expired(self, limit).await
    }

    async fn delete_expired_row(&self, id: Uuid) -> Result<Option<ValueId>, DomainError> {
        writes::delete_expired_row(self, id).await
    }
}
