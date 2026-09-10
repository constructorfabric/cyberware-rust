//! Quota Enforcement SDK
//!
//! Public, transport-agnostic contract of the `quota-enforcement` gear. The
//! foundation ships the plugin side first, so plugin authors implement against
//! one dependency:
//!
//! - [`QuotaEnforcementStoragePluginV1`] with the closed [`StorageError`] and
//!   the I1 to I14 invariants (see the [`storage_plugin`] module docs).
//! - The domain types the contract references ([`models`]), including the
//!   wire attribution types of a subject-based evaluation request
//!   ([`EvaluationAttribution`]) and the scope discriminator ([`SubjectScope`]).
//! - GTS plugin spec, resource identifiers, and the QE-owned projection
//!   contract bases ([`gts`], ADR-0007).
//!
//! Singleton coordination for the sweepers is not a contract of this SDK: the
//! gear consumes the platform `cluster` gear's leader election (ADR-0006).
//!
//! The manager client trait ([`QuotaManagerClientV1`]) carries the Quota
//! lifecycle; the consumer and operator traits land with their features.
//!
//! Enable the `test-util` feature for a complete in-memory double of the
//! storage contract in [`testing`].
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod client;
pub mod gts;
pub mod models;
pub mod storage_plugin;

#[cfg(feature = "test-util")]
pub mod testing;

pub use client::{QuotaEnforcementError, QuotaManagerClientV1};
pub use gts::{
    CONSTRAINT_BASE, LEASE_RESOURCE, METRIC_BASE_TYPE, OPERATION_RESOURCE, OwnedDefinition,
    POLICY_RESOURCE, QUOTA_RESOURCE, QuotaEnforcementStoragePluginSpecV1, REQUEST_BASE,
    RESOURCE_BASE, SCOPE_TENANT, SCOPE_TYPE, SCOPE_USER, SUBJECT_BASE, owned_definitions,
};
pub use models::{
    ActiveQuotaCounts, ApplicableQuotas, BatchDebitItem, BootstrapBundle, CapPatch, ConfigDefaults,
    ContractRef, CounterSnapshot, DeactivateOutcome, DebitPlan, Decision, DecisionResult,
    EnforcementMode, EvaluationAttribution, EventId, ExpiredLease, IdempotencyRecord,
    IdempotencyScope, IdempotencySubjectKey, IdempotencyWrite, LeaseHold, LeaseState, LeaseToken,
    MetricId, MetricKind, MutationResult, NotificationEvent, NotificationEventKind, OperationType,
    PageRequest, PageResult, PayloadHash, PeriodId, PeriodType, PeriodWindow, PolicyDraft,
    PolicyId, PolicyScope, PolicyUpdate, PolicyVersion, PolicyVersionMeta, PolicyVersionState,
    ProjectionBinding, Quota, QuotaDebitPlan, QuotaDraft, QuotaFilter, QuotaId, QuotaPatch,
    QuotaSnapshot, QuotaSource, QuotaSpec, QuotaStatus, QuotaType, QuotaView, ResourceProjection,
    ScopeError, SubjectClaim, SubjectRef, SubjectScope, TenantId, ThresholdCrossing, UnknownValue,
    ValidityWindow, ValidityWindowPatch,
};
pub use storage_plugin::{CONTRACT_MAJOR, QuotaEnforcementStoragePluginV1, StorageError};
