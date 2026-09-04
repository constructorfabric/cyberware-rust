//! Quota Enforcement SDK
//!
//! Public, transport-agnostic contract of the `quota-enforcement` gear. The
//! foundation ships the plugin side first, so plugin authors implement against
//! one dependency:
//!
//! - [`QuotaEnforcementStoragePluginV1`] with the closed [`StorageError`] and
//!   the I1 to I13 invariants (see the [`storage_plugin`] module docs).
//! - The domain types the contract references ([`models`]).
//! - GTS plugin spec and resource identifiers ([`gts`]).
//!
//! Singleton coordination for the sweepers is not a contract of this SDK: the
//! gear consumes the platform `cluster` gear's leader election (ADR-0006).
//!
//! The consumer, manager, and operator client traits land with their features.
//!
//! Enable the `test-util` feature for a complete in-memory double of the
//! storage contract in [`testing`].
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod gts;
pub mod models;
pub mod storage_plugin;

#[cfg(feature = "test-util")]
pub mod testing;

pub use gts::{
    LEASE_RESOURCE, OPERATION_RESOURCE, POLICY_RESOURCE, QUOTA_RESOURCE,
    QuotaEnforcementStoragePluginSpecV1,
};
pub use models::{
    ApplicableQuotas, BatchDebitItem, BootstrapBundle, CapPatch, ConfigDefaults, ContractRef,
    CounterSnapshot, DeactivateOutcome, DebitPlan, Decision, DecisionResult, EnforcementMode,
    EventId, ExpiredLease, IdempotencyRecord, IdempotencyScope, IdempotencySubjectKey,
    IdempotencyWrite, LeaseHold, LeaseState, LeaseToken, MetricId, MutationResult,
    NotificationEvent, NotificationEventKind, OperationType, PageRequest, PageResult, PayloadHash,
    PeriodId, PeriodType, PeriodWindow, PolicyDraft, PolicyId, PolicyScope, PolicyUpdate,
    PolicyVersion, PolicyVersionMeta, PolicyVersionState, Quota, QuotaDebitPlan, QuotaDraft,
    QuotaFilter, QuotaId, QuotaPatch, QuotaSnapshot, QuotaSource, QuotaStatus, QuotaType,
    SubjectRef, TenantId, ThresholdCrossing, UnknownValue, ValidityWindow, ValidityWindowPatch,
};
pub use storage_plugin::{CONTRACT_MAJOR, QuotaEnforcementStoragePluginV1, StorageError};
