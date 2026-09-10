//! Ports the storage plugin's domain depends on. The SQL adapters live in
//! `infra::storage`; the domain never imports them.

use std::collections::HashSet;

use async_trait::async_trait;
use quota_enforcement_sdk::{
    ActiveQuotaCounts, ConfigDefaults, DeactivateOutcome, NotificationEvent, PageRequest,
    PageResult, ProjectionBinding, Quota, QuotaDraft, QuotaFilter, QuotaId, QuotaPatch,
};
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

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

/// Failure of a store operation.
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

    // --- quota lifecycle, one to one with the contract ---
    /// No row with this id lies inside the caller's scope.
    #[error("quota {id} not found")]
    QuotaNotFound {
        /// The requested id.
        id: QuotaId,
    },
    /// The row is deactivated; nothing was written.
    #[error("quota {id} is deactivated")]
    QuotaDeactivated {
        /// The deactivated Quota.
        id: QuotaId,
    },
    /// Invariant I6: the merged cap is below what is already consumed.
    #[error("cap {new_cap} is below the consumed amount {consumed}")]
    CapBelowConsumed {
        /// The cap the patch would set.
        new_cap: u64,
        /// What the counters hold.
        consumed: u64,
    },
    /// Invariant I14: the merged row carries thresholds on an unbounded cap.
    #[error("notification thresholds require a bounded cap")]
    ThresholdsRequireBoundedCap,
    /// The draft's tenant lies outside the authorized scope.
    #[error("subject is outside the authorized scope")]
    SubjectOutOfScope,

    // --- caller input the contract maps to `Internal` ---
    /// The continuation cursor does not decode.
    #[error("malformed continuation cursor")]
    InvalidCursor,
    /// The patch is not one the gear can produce: a `metadata` change without
    /// the contract it was validated against.
    #[error("invalid patch: {detail}")]
    InvalidPatch {
        /// What is missing.
        detail: String,
    },
    /// A filter exceeds the plugin's bounds.
    #[error("invalid filter: {detail}")]
    InvalidFilter {
        /// What was out of bounds.
        detail: String,
    },
    /// A value does not fit its column type.
    #[error("{field}={value} does not fit the column type")]
    ValueOutOfRange {
        /// The field.
        field: &'static str,
        /// The offending value.
        value: String,
    },
    /// A stored row does not read back as the contract type, or a write
    /// the transaction relied on affected no row.
    #[error("storage state is inconsistent during {operation}: {detail}")]
    Corrupt {
        /// The store operation.
        operation: &'static str,
        /// What did not add up.
        detail: String,
    },
}

/// Who performed a mutation, for the operation log.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The caller's subject id.
    pub subject_id: Uuid,
    /// The caller's subject type, when the security context carries one.
    pub subject_type: Option<String>,
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

/// The Quota tables: the four lifecycle primitives and the two caller-less
/// platform-plane reads of the storage contract, with the contract's
/// semantics (I1, I6, I11, I14) and the plugin's own bounds. `events` are
/// enqueued in the same transaction as the rows; an event whose `quota_id` is
/// `None` receives the created id.
#[async_trait]
pub trait QuotaStore: Send + Sync {
    /// Insert a Quota, its counter row, one operation-log row, and `events`.
    async fn create_quota(
        &self,
        actor: &Actor,
        scope: &AccessScope,
        draft: QuotaDraft,
        events: &[NotificationEvent],
    ) -> Result<QuotaId, StoreError>;

    /// Apply `patch` under the row lock and return the committed row.
    async fn update_quota(
        &self,
        actor: &Actor,
        scope: &AccessScope,
        quota_id: QuotaId,
        patch: QuotaPatch,
        events: &[NotificationEvent],
    ) -> Result<Quota, StoreError>;

    /// Flip the row to `deactivated` under the row lock.
    async fn deactivate_quota(
        &self,
        actor: &Actor,
        scope: &AccessScope,
        quota_id: QuotaId,
        events: &[NotificationEvent],
    ) -> Result<DeactivateOutcome, StoreError>;

    /// One page ordered by id ascending; the cursor carries position only.
    async fn read_quotas(
        &self,
        scope: &AccessScope,
        filter: QuotaFilter,
        page: PageRequest,
    ) -> Result<PageResult<Quota>, StoreError>;

    /// Distinct `(metric, projection_type)` pairs of active Quotas.
    async fn read_active_projection_bindings(
        &self,
    ) -> Result<HashSet<ProjectionBinding>, StoreError>;

    /// Active-Quota counts behind the lifecycle gauges.
    async fn read_active_quota_counts(&self) -> Result<ActiveQuotaCounts, StoreError>;
}
