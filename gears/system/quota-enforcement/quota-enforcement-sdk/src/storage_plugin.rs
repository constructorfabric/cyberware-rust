//! Storage plugin contract: [`QuotaEnforcementStoragePluginV1`].
//!
//! Persistence is mediated by this single trait with a closed
//! [`StorageError`] enum (DESIGN section 3.3, "Storage Plugin Trait"). The
//! trait surface is the contractual boundary of QE core. Locking discipline,
//! indexing, isolation level, and table layout are plugin-internal.
//!
//! # Invariants every implementation upholds
//!
//! - **I1 Atomicity.** Every mutating call mutates counters, persists the
//!   idempotency record, enqueues outbox events, and writes the operation-log
//!   entry in one backend transaction.
//! - **I2 Idempotency.** Replay returns the original outcome verbatim. A
//!   different payload under the same scope returns
//!   [`StorageError::IdempotencyPayloadMismatch`].
//! - **I3 Read-only.** `read_*`, `list_*`, and `lookup_idempotency` write no
//!   persistent state. Lazy period-row creation in `read_quota_snapshot` is
//!   the single exception.
//! - **I4 Lease lazy expiry.** Every read and write path treats a lease with
//!   `expiry_at <= now()` as released, whether or not its row still exists.
//! - **I5 Period attribution.** Lease commit, release, and auto-release
//!   mutate the acquisition period's counter, not the current period's.
//! - **I6 Cap versus consumed.** `update_quota` with a lower cap returns
//!   [`StorageError::CapBelowConsumed`] when any active period exceeds it,
//!   checked in the transaction under a row lock.
//! - **I7 Active-lease cap.** `acquire_lease` returns
//!   [`StorageError::LeaseInflightLimitExceeded`] when the per-`(tenant,
//!   metric)` counter would exceed the configured cap.
//! - **I8 Contention timeout.** Mutating primitives respect the per-metric
//!   contention timeout and return [`StorageError::LeaseContentionTimeout`].
//! - **I9 Isolation.** Concurrent row mutations serialize under the ADR-0002
//!   acquisition order. No dirty reads inside a transaction.
//! - **I10 Strong consistency within tenant scope.** A committed mutation is
//!   visible to later reads in the same tenant scope.
//! - **I11 Outbox same-tx.** Events passed to a mutating call are enqueued in
//!   the same transaction as the mutation.
//! - **I12 Schema version coupling.** `bootstrap()` rejects an installed
//!   schema whose major differs from [`CONTRACT_MAJOR`] with
//!   [`StorageError::SchemaVersionMismatch`].
//! - **I13 Threshold-marker reset.** A newly materialized period row has a
//!   `NULL` highest-crossed-threshold marker.
//!
//! Every tenant-scoped call receives the caller's [`AccessScope`] unmodified
//! and binds it through `SecureConn`. No scoped operation runs without it.

use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};

use crate::models::{
    ApplicableQuotas, BatchDebitItem, BootstrapBundle, DeactivateOutcome, DebitPlan, ExpiredLease,
    IdempotencyRecord, IdempotencyScope, IdempotencyWrite, LeaseToken, MutationResult,
    NotificationEvent, PageRequest, PageResult, PolicyDraft, PolicyId, PolicyScope, PolicyUpdate,
    PolicyVersion, PolicyVersionMeta, Quota, QuotaDraft, QuotaFilter, QuotaId, QuotaPatch,
    QuotaSnapshot,
};

/// Major version of this contract. Coupled to the gear's major version. A
/// storage plugin that implements another major is not supported (I12).
pub const CONTRACT_MAJOR: u32 = 1;

/// Closed error set of [`QuotaEnforcementStoragePluginV1`].
///
/// Variants are grouped by concern. The gear lifts every variant 1:1 into its
/// domain error, with two exceptions: `QuotaNotFound` becomes a generic
/// not-found and `SubjectOutOfScope` becomes a PDP denial.
/// `SchemaVersionMismatch` never surfaces at runtime; `bootstrap()` fails fast.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    // --- lease state ---
    /// Commit or release against a lease that is not active.
    #[error("lease {token} is not active")]
    LeaseNotActive {
        /// The lease token.
        token: LeaseToken,
    },
    /// The per-`(tenant, metric)` active-lease cap would be exceeded (I7).
    #[error("active-lease cap reached for the (tenant, metric) pair")]
    LeaseInflightLimitExceeded,
    /// The acquisition contention timeout elapsed (I8).
    #[error("acquisition contention timeout elapsed")]
    LeaseContentionTimeout,
    /// Commit amount exceeds the reserved amount.
    #[error("commit amount {actual} exceeds reserved amount {reserved}")]
    OverCommitNotAuthorized {
        /// Reserved at acquisition.
        reserved: u64,
        /// Requested at commit.
        actual: u64,
    },

    // --- idempotency and versioning ---
    /// Same idempotency scope, different payload (I2).
    #[error("idempotency key replayed with a different payload")]
    IdempotencyPayloadMismatch,
    /// Optimistic-concurrency version mismatch.
    #[error("version conflict: expected {expected}, found {actual}")]
    VersionConflict {
        /// Version the caller expected.
        expected: u32,
        /// Version found in storage.
        actual: u32,
    },
    /// The named policy version does not exist.
    #[error("unknown version {version} of policy {policy_id}")]
    UnknownPolicyVersion {
        /// The policy.
        policy_id: PolicyId,
        /// The missing version.
        version: u32,
    },
    /// Rollback target is in the terminal `rolled_back` state.
    #[error("version {version} of policy {policy_id} was rolled back")]
    VersionRolledBack {
        /// The policy.
        policy_id: PolicyId,
        /// The rolled-back version.
        version: u32,
    },

    // --- quota lifecycle ---
    /// A cap reduction below the current consumed amount (I6).
    #[error("cap {new_cap} is below the consumed amount {consumed}")]
    CapBelowConsumed {
        /// Requested cap.
        new_cap: u64,
        /// Current consumed or in-flight amount.
        consumed: u64,
    },
    /// No Quota with this identifier in the caller's scope.
    #[error("quota {id} not found")]
    QuotaNotFound {
        /// The identifier.
        id: QuotaId,
    },
    /// The Quota is deactivated.
    #[error("quota {id} is deactivated")]
    QuotaDeactivated {
        /// The identifier.
        id: QuotaId,
    },
    /// The target period is closed.
    #[error("the target period is closed")]
    PeriodClosed,

    // --- metric and contract registry ---
    /// The metric is not registered.
    #[error("metric {metric} is not registered")]
    MetricNotRegistered {
        /// The metric identifier.
        metric: String,
    },
    /// The metric is registered but not quota-gated.
    #[error("metric {metric} is not quota-gated")]
    MetricNotQuotaGated {
        /// The metric identifier.
        metric: String,
    },
    /// The projection type is not registered.
    #[error("projection {projection} is not registered")]
    ProjectionNotRegistered {
        /// The projection type identifier.
        projection: String,
    },

    // --- post-PDP defense in depth ---
    /// A subject outside the authorized scope reached storage.
    #[error("subject is outside the authorized scope")]
    SubjectOutOfScope,

    // --- operational ---
    /// Transport or backend reachability failure.
    #[error("storage backend unavailable: {0}")]
    Unavailable(String),
    /// Installed schema major differs from the contract major (I12).
    #[error("installed schema major {installed} does not match contract major {expected}")]
    SchemaVersionMismatch {
        /// Major found in the schema metadata.
        installed: u32,
        /// Major the gear was compiled against.
        expected: u32,
    },
    /// Last-resort opaque failure.
    #[error("storage plugin internal error: {0}")]
    Internal(String),
}

/// Pluggable persistence for Quotas, counters, leases, policies, idempotency
/// records, and the operation log.
///
/// Registered by a plugin gear as a scoped `ClientHub` client under its GTS
/// instance id once every primitive exists. No partial implementation is ever
/// wired (foundation `DoD`, "Reference Storage Plugin on toolkit-db").
// @cpt-dod:cpt-cf-quota-enforcement-dod-sdk-contracts:p1
#[async_trait]
pub trait QuotaEnforcementStoragePluginV1: Send + Sync + 'static {
    // --- lifecycle ---

    /// Idempotent bootstrap: verify the schema major (I12) and seed the default
    /// configuration rows and, when present, the global policy.
    ///
    /// # Errors
    ///
    /// - [`StorageError::SchemaVersionMismatch`] when the installed schema major
    ///   differs from `bundle.contract_major`. The gear fails fast.
    /// - [`StorageError::Unavailable`] when the backend cannot answer.
    async fn bootstrap(&self, bundle: &BootstrapBundle) -> Result<(), StorageError>;

    // --- quota CRUD ---

    /// Persist a new Quota and enqueue `events` in the same transaction.
    async fn create_quota(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        draft: QuotaDraft,
        events: &[NotificationEvent],
    ) -> Result<QuotaId, StorageError>;

    /// Apply `patch` under a row lock. Enforces I6.
    async fn update_quota(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        quota_id: QuotaId,
        patch: QuotaPatch,
        events: &[NotificationEvent],
    ) -> Result<Quota, StorageError>;

    /// Deactivate a Quota and resolve its active leases atomically.
    async fn deactivate_quota(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        quota_id: QuotaId,
        events: &[NotificationEvent],
    ) -> Result<DeactivateOutcome, StorageError>;

    /// Read Quotas within the caller's scope.
    async fn read_quotas(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        filter: QuotaFilter,
        page: PageRequest,
    ) -> Result<PageResult<Quota>, StorageError>;

    // --- counter mutation ---

    /// Apply a debit plan atomically across every named Quota.
    async fn apply_debit_plan(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        applicable: &ApplicableQuotas,
        plan: &DebitPlan,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError>;

    /// Apply an atomic batch of debit plans under one envelope key.
    async fn apply_batch_debit(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        envelope: &IdempotencyWrite,
        items: &[BatchDebitItem],
        events: &[NotificationEvent],
    ) -> Result<Vec<MutationResult>, StorageError>;

    /// Credit one named Quota.
    async fn apply_credit(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        quota_id: QuotaId,
        amount: u64,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError>;

    /// Reverse the debit registered under `original`.
    async fn apply_rollback(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        original: &IdempotencyScope,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError>;

    // --- leases ---

    /// Acquire holds on every Quota of `plan` atomically (I5, I7, I8).
    async fn acquire_lease(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        applicable: &ApplicableQuotas,
        plan: &DebitPlan,
        ttl: Duration,
        idempotency: &IdempotencyWrite,
    ) -> Result<LeaseToken, StorageError>;

    /// Convert an active lease into a debit. `actual_amount` defaults to the
    /// reserved amount.
    async fn commit_lease(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        token: LeaseToken,
        actual_amount: Option<u64>,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError>;

    /// Return the held amount of an active lease.
    async fn release_lease(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        token: LeaseToken,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError>;

    // --- snapshot reads ---

    /// Per-Quota state for one applicable set. May materialize the current
    /// period row (the I3 exception).
    async fn read_quota_snapshot(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        applicable: &ApplicableQuotas,
    ) -> Result<Vec<QuotaSnapshot>, StorageError>;

    /// Paginated per-Quota state for many applicable sets.
    async fn bulk_read_quota_snapshot(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        pairs: &[ApplicableQuotas],
        page: PageRequest,
    ) -> Result<PageResult<QuotaSnapshot>, StorageError>;

    // --- policies (platform-wide, operator-scoped) ---

    /// Create version 1 of a new policy.
    async fn create_policy(
        &self,
        ctx: &SecurityContext,
        draft: PolicyDraft,
        events: &[NotificationEvent],
    ) -> Result<PolicyVersion, StorageError>;

    /// Create the next immutable version. Enforces `if_match_version`.
    async fn update_policy(
        &self,
        ctx: &SecurityContext,
        policy_id: PolicyId,
        update: PolicyUpdate,
        events: &[NotificationEvent],
    ) -> Result<PolicyVersion, StorageError>;

    /// Make `target_version` active again.
    async fn rollback_policy(
        &self,
        ctx: &SecurityContext,
        policy_id: PolicyId,
        target_version: u32,
        comment: Option<String>,
        events: &[NotificationEvent],
    ) -> Result<PolicyVersion, StorageError>;

    /// Soft-delete a narrow-scope policy. Idempotent on retry.
    async fn delete_policy(
        &self,
        ctx: &SecurityContext,
        policy_id: PolicyId,
        comment: Option<String>,
        events: &[NotificationEvent],
    ) -> Result<(), StorageError>;

    /// The latest active version at `scope`, if any.
    async fn read_policy(&self, scope: &PolicyScope)
    -> Result<Option<PolicyVersion>, StorageError>;

    /// One specific version, if it exists.
    async fn read_policy_version(
        &self,
        policy_id: &PolicyId,
        version: u32,
    ) -> Result<Option<PolicyVersion>, StorageError>;

    /// Ordered version list of a policy.
    async fn list_policy_versions(
        &self,
        policy_id: &PolicyId,
        page: PageRequest,
    ) -> Result<PageResult<PolicyVersionMeta>, StorageError>;

    // --- idempotency ---

    /// Read-only lookup of a stored record (I3).
    async fn lookup_idempotency(
        &self,
        scope: &IdempotencyScope,
    ) -> Result<Option<IdempotencyRecord>, StorageError>;

    // --- sweeper and reclamation ---

    /// Physically reclaim up to `batch_size` leases expired before `before`.
    async fn reclaim_expired_leases(
        &self,
        batch_size: u32,
        before: OffsetDateTime,
    ) -> Result<Vec<ExpiredLease>, StorageError>;

    /// Delete up to `batch_size` idempotency records expired before `before`.
    /// Returns the number of deleted records.
    async fn reclaim_expired_idempotency(
        &self,
        batch_size: u32,
        before: OffsetDateTime,
    ) -> Result<u64, StorageError>;

    /// Delete up to `batch_size` operation-log entries older than `before`.
    /// Returns the number of deleted entries.
    async fn reclaim_operation_log(
        &self,
        batch_size: u32,
        before: OffsetDateTime,
    ) -> Result<u64, StorageError>;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "storage_plugin_tests.rs"]
mod storage_plugin_tests;
