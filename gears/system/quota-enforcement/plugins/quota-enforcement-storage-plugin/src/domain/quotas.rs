//! The Quota primitives of the storage contract, forwarded to the
//! [`QuotaStore`] port with the contract's signatures. The eventual
//! `impl QuotaEnforcementStoragePluginV1 for StoragePlugin` delegates here;
//! until every primitive of the trait exists the client stays unpublished and
//! these methods are reached by tests only.
//!
//! The security context contributes the actor of the operation log; the access
//! scope is what the PDP authorized, re-applied by `SecureORM` on every row.

use std::collections::HashSet;

use quota_enforcement_sdk::{
    ActiveQuotaCounts, DeactivateOutcome, NotificationEvent, PageRequest, PageResult,
    ProjectionBinding, Quota, QuotaDraft, QuotaFilter, QuotaId, QuotaPatch, StorageError,
};
use toolkit_security::{AccessScope, SecurityContext};

use super::bootstrap::{FromStore, StoragePlugin};
use super::ports::{Actor, QuotaStore};

impl StoragePlugin {
    /// The Quota store this plugin forwards to.
    #[must_use]
    pub fn quota_store(&self) -> &dyn QuotaStore {
        self.quotas.as_ref()
    }

    /// Persist a new Quota and enqueue `events` in the same transaction.
    ///
    /// # Errors
    ///
    /// As the contract documents: `SubjectOutOfScope`, `Unavailable`, and
    /// `Internal` for a draft that does not fit its columns.
    pub async fn create_quota(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        draft: QuotaDraft,
        events: &[NotificationEvent],
    ) -> Result<QuotaId, StorageError> {
        self.quotas
            .create_quota(&actor_of(ctx), scope, draft, events)
            .await
            .map_err(StorageError::from_store)
    }

    /// Apply `patch` under the row lock and return the committed row.
    ///
    /// # Errors
    ///
    /// `QuotaNotFound`, `QuotaDeactivated`, `CapBelowConsumed` (I6),
    /// `ThresholdsRequireBoundedCap` (I14), `Unavailable`.
    pub async fn update_quota(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        quota_id: QuotaId,
        patch: QuotaPatch,
        events: &[NotificationEvent],
    ) -> Result<Quota, StorageError> {
        self.quotas
            .update_quota(&actor_of(ctx), scope, quota_id, patch, events)
            .await
            .map_err(StorageError::from_store)
    }

    /// Deactivate a Quota. The lease cascade slot is filled by the
    /// lease-operations feature; until then no lease is resolved.
    ///
    /// # Errors
    ///
    /// `QuotaNotFound`, `QuotaDeactivated`, `Unavailable`.
    pub async fn deactivate_quota(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        quota_id: QuotaId,
        events: &[NotificationEvent],
    ) -> Result<DeactivateOutcome, StorageError> {
        self.quotas
            .deactivate_quota(&actor_of(ctx), scope, quota_id, events)
            .await
            .map_err(StorageError::from_store)
    }

    /// One page of Quotas inside `scope`, ordered by id ascending.
    ///
    /// # Errors
    ///
    /// `InvalidCursor` for a malformed cursor; `Unavailable`; `Internal` for
    /// an over-long id filter.
    pub async fn read_quotas(
        &self,
        _ctx: &SecurityContext,
        scope: &AccessScope,
        filter: QuotaFilter,
        page: PageRequest,
    ) -> Result<PageResult<Quota>, StorageError> {
        self.quotas
            .read_quotas(scope, filter, page)
            .await
            .map_err(StorageError::from_store)
    }

    /// Distinct `(metric, projection_type)` pairs of active Quotas.
    ///
    /// # Errors
    ///
    /// `Unavailable`; `Internal` for a row that does not read back.
    pub async fn read_active_projection_bindings(
        &self,
    ) -> Result<HashSet<ProjectionBinding>, StorageError> {
        self.quotas
            .read_active_projection_bindings()
            .await
            .map_err(StorageError::from_store)
    }

    /// Active-Quota counts behind the lifecycle gauges.
    ///
    /// # Errors
    ///
    /// `Unavailable`; `Internal` for a row that does not read back.
    pub async fn read_active_quota_counts(&self) -> Result<ActiveQuotaCounts, StorageError> {
        self.quotas
            .read_active_quota_counts()
            .await
            .map_err(StorageError::from_store)
    }
}

/// The operation-log actor of a security context.
fn actor_of(ctx: &SecurityContext) -> Actor {
    Actor {
        subject_id: ctx.subject_id(),
        subject_type: ctx.subject_type().map(str::to_owned),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "quotas_tests.rs"]
mod quotas_tests;
