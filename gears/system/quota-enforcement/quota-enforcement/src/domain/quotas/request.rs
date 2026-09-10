//! Transport-agnostic request models of the Quota lifecycle.
//!
//! REST and the in-process client both produce these. They keep what the
//! validation order of `features/quota-lifecycle.md` needs to see: whether a
//! field was present at all (an explicit `null` counts as present), and caps
//! as signed integers so a negative value reaches `CAP_MUST_BE_NON_NEGATIVE`
//! instead of a deserialization error. The SDK conversions are fallible:
//! nothing narrows a `u64` cap silently.

use gts::GtsTypeId;
use quota_enforcement_sdk::{
    CapPatch, EnforcementMode, PageRequest, PeriodType, Quota, QuotaFilter, QuotaId, QuotaPatch,
    QuotaSource, QuotaSpec, QuotaStatus, QuotaType, SubjectRef, TenantId, ValidityWindow,
    ValidityWindowPatch,
};
use serde_json::{Map, Value};
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::tokens;

/// Whether a request field was present, and with what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Presence<T> {
    /// The key was not in the request.
    #[default]
    Absent,
    /// The key was present with an explicit `null`.
    Null,
    /// The key was present with a value.
    Value(T),
}

impl<T> Presence<T> {
    /// True for `Null` and `Value`: the caller named the field.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    /// `Some(value)` for `Value`, `None` otherwise.
    #[must_use]
    pub fn value(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }
}

/// A `u64` cap narrowed to the supported range `0..=i64::MAX`.
fn cap_to_i64(cap: u64) -> Result<i64, DomainError> {
    i64::try_from(cap).map_err(|_| DomainError::InvalidArgument {
        field: "cap",
        reason: tokens::CAP_OUT_OF_RANGE,
    })
}

/// A create request before validation (`flow-quota-create`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateQuotaRequest {
    /// The explicit target tenant.
    pub tenant_id: TenantId,
    /// The explicit target subject.
    pub subject: SubjectRef,
    /// The metric as written; parsed by validation so a malformed id is a
    /// field violation, not a deserialization error.
    pub metric: String,
    /// Accounting model. Closed by the SDK enum; `rate` is rejected later.
    pub quota_type: QuotaType,
    /// Period specification, presence-aware: allocation Quotas reject any
    /// present value, `null` included.
    pub period: Presence<PeriodType>,
    /// Behaviour at the cap boundary. Closed by the SDK enum.
    pub enforcement_mode: EnforcementMode,
    /// Cap as the caller wrote it; `None` means unbounded.
    pub cap: Option<i64>,
    /// Notification thresholds as percentages of cap.
    pub notification_thresholds: Vec<u8>,
    /// Optional validity bounds.
    pub validity_window: Option<ValidityWindow>,
    /// Informational fail-open hint.
    pub fail_open_hint: bool,
    /// Metadata to validate against the owner's constraint contract; absent
    /// means an empty object.
    pub metadata: Option<Map<String, Value>>,
    /// Who imposes the Quota. Closed by the SDK enum.
    pub source: QuotaSource,
}

impl TryFrom<QuotaSpec> for CreateQuotaRequest {
    type Error = DomainError;

    fn try_from(spec: QuotaSpec) -> Result<Self, Self::Error> {
        Ok(Self {
            tenant_id: spec.tenant_id,
            subject: spec.subject,
            metric: spec.metric.as_str().to_owned(),
            quota_type: spec.quota_type,
            period: spec.period.map_or(Presence::Absent, Presence::Value),
            enforcement_mode: spec.enforcement_mode,
            cap: spec.cap.map(cap_to_i64).transpose()?,
            notification_thresholds: spec.notification_thresholds,
            validity_window: spec.validity_window,
            fail_open_hint: spec.fail_open_hint,
            metadata: Some(spec.metadata),
            source: spec.source,
        })
    }
}

/// An update request before validation (`flow-quota-update`). The four
/// immutable fields are carried presence-aware so the gate can name them; a
/// present value of any of them is a rejection.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateQuotaRequest {
    /// Immutable; any present value is rejected.
    pub metric: Presence<Value>,
    /// Immutable; the reserved `rate` id is `Unimplemented`, anything else
    /// present is `IMMUTABLE_FIELD`.
    pub quota_type: Presence<Value>,
    /// Immutable; any present value is rejected.
    pub period: Presence<Value>,
    /// Immutable; any present value is rejected.
    pub subject: Presence<Value>,
    /// New cap: `Null` unbinds it, a value bounds it.
    pub cap: Presence<i64>,
    /// New thresholds, replacing the list; an empty list clears it.
    pub notification_thresholds: Option<Vec<u8>>,
    /// New validity window: `Null` clears it, a value replaces it.
    pub validity_window: Presence<ValidityWindow>,
    /// New metadata object, replacing the previous one.
    pub metadata: Option<Map<String, Value>>,
    /// New enforcement mode.
    pub enforcement_mode: Option<EnforcementMode>,
    /// New fail-open hint.
    pub fail_open_hint: Option<bool>,
}

impl TryFrom<QuotaPatch> for UpdateQuotaRequest {
    type Error = DomainError;

    fn try_from(patch: QuotaPatch) -> Result<Self, Self::Error> {
        if patch.constraint_contract.is_some() {
            return Err(DomainError::InvalidArgument {
                field: "constraint_contract",
                reason: tokens::CONSTRAINT_CONTRACT_NOT_CALLER_SUPPLIED,
            });
        }
        let cap = match patch.cap {
            None => Presence::Absent,
            Some(CapPatch::Unbounded) => Presence::Null,
            Some(CapPatch::Bounded(cap)) => Presence::Value(cap_to_i64(cap)?),
        };
        let validity_window = match patch.validity_window {
            None => Presence::Absent,
            Some(ValidityWindowPatch::Clear) => Presence::Null,
            Some(ValidityWindowPatch::Set(window)) => Presence::Value(window),
        };
        Ok(Self {
            metric: Presence::Absent,
            quota_type: Presence::Absent,
            period: Presence::Absent,
            subject: Presence::Absent,
            cap,
            notification_thresholds: patch.notification_thresholds,
            validity_window,
            metadata: patch.metadata,
            enforcement_mode: patch.enforcement_mode,
            fail_open_hint: patch.fail_open_hint,
        })
    }
}

/// A list request before validation (`flow-quota-read`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListQuotasRequest {
    /// Owning tenant; the caller's own tenant when absent.
    pub tenant_id: Option<TenantId>,
    /// Subject projection type; must come with `subject_id`.
    pub projection_type: Option<GtsTypeId>,
    /// Subject identifier; must come with `projection_type`.
    pub subject_id: Option<String>,
    /// Metric as written.
    pub metric: Option<String>,
    /// Lifecycle state.
    pub status: Option<QuotaStatus>,
    /// Explicit identifiers. Empty means no restriction.
    pub ids: Vec<QuotaId>,
    /// Page size; the platform default when absent.
    pub limit: Option<u32>,
    /// Opaque continuation cursor.
    pub cursor: Option<String>,
}

impl From<(QuotaFilter, PageRequest)> for ListQuotasRequest {
    fn from((filter, page): (QuotaFilter, PageRequest)) -> Self {
        let (projection_type, subject_id) = match filter.subject {
            Some(subject) => (Some(subject.projection_type), Some(subject.subject_id)),
            None => (None, None),
        };
        Self {
            tenant_id: filter.tenant_id,
            projection_type,
            subject_id,
            metric: filter.metric.map(|m| m.as_str().to_owned()),
            status: filter.status,
            ids: filter.ids,
            limit: Some(page.limit),
            cursor: page.cursor,
        }
    }
}

/// The fields of a stored Quota an update may not touch, for the gate's
/// diagnostics.
pub const IMMUTABLE_FIELDS: [&str; 4] = ["metric", "quota_type", "period", "subject"];

/// Convenience for tests and callers that hold a stored row: the subject and
/// metric text of `quota`.
#[must_use]
pub fn identity_of(quota: &Quota) -> (&SubjectRef, &str) {
    (&quota.subject, quota.metric.as_str())
}
