//! Draft validation (`features/quota-lifecycle.md`, "Quota Draft Validation")
//! and the request-shape gates of update and list.
//!
//! Everything here runs before the PDP call and before any storage call, on the
//! request alone or on the request plus the row the service read. The one rule
//! that depends on the row, thresholds versus the merged cap, is a fast
//! pre-check: two concurrent patches can each pass it against the same old
//! row, so storage enforces it again on the merged row inside the transaction
//! (invariant I14) and is authoritative.

use quota_enforcement_sdk::{
    CapPatch, EnforcementMode, MetricId, PageRequest, PeriodType, Quota, QuotaFilter, QuotaPatch,
    QuotaSource, QuotaType, SubjectRef, SubjectScope, TenantId, ValidityWindow,
    ValidityWindowPatch,
};
use serde_json::{Map, Value};
use toolkit_macros::domain_model;

use super::request::{
    CreateQuotaRequest, IMMUTABLE_FIELDS, ListQuotasRequest, Presence, UpdateQuotaRequest,
};
use crate::domain::catalog::parse_metric_under_base;
use crate::domain::error::DomainError;
use crate::domain::tokens;

/// The reserved capability name in `NotYetImplemented`.
pub const RATE_QUOTAS: &str = "rate quotas";

/// Longest `subject_id` accepted for a non-tenant scope.
pub const SUBJECT_ID_MAX_LEN: usize = 256;

/// Operator-configured bounds of the lifecycle surface.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaLimits {
    /// Largest canonical-JSON size of a `metadata` object, in bytes.
    pub metadata_max_bytes: usize,
    /// Largest page a list request may ask for.
    pub list_max_limit: u32,
    /// Largest number of explicit ids one list request may name.
    pub list_max_ids: usize,
}

/// A create request that passed the draft validation. Everything but the
/// constraint contract, which the catalogue supplies.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapedDraft {
    /// The explicit target tenant.
    pub tenant_id: TenantId,
    /// The explicit target subject.
    pub subject: SubjectRef,
    /// The metric, well-formed under the metric base.
    pub metric: MetricId,
    /// Accounting model; never `rate`.
    pub quota_type: QuotaType,
    /// Period specification; present exactly for consumption Quotas.
    pub period: Option<PeriodType>,
    /// Behaviour at the cap boundary.
    pub enforcement_mode: EnforcementMode,
    /// Cap; `None` means unbounded.
    pub cap: Option<u64>,
    /// Notification thresholds, each in `1..=100`, strictly ascending.
    pub notification_thresholds: Vec<u8>,
    /// Optional validity bounds, start not after end.
    pub validity_window: Option<ValidityWindow>,
    /// Informational fail-open hint.
    pub fail_open_hint: bool,
    /// Metadata to validate against the constraint contract.
    pub metadata: Map<String, Value>,
    /// Who imposes the Quota.
    pub source: QuotaSource,
}

fn invalid(field: &'static str, reason: &'static str) -> DomainError {
    DomainError::InvalidArgument { field, reason }
}

/// The draft validation algorithm on a create request.
///
/// # Errors
///
/// `NotYetImplemented` for `rate`; `InvalidArgument` with the closed token of
/// the first violated rule; `CapMustBeNonNegative`;
/// `ThresholdsRequireBoundedCap`.
// @cpt-algo:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1
// @cpt-dod:cpt-cf-quota-enforcement-dod-rate-rejection:p1
pub fn validate_create_shape(req: CreateQuotaRequest) -> Result<ShapedDraft, DomainError> {
    if req.subject.subject_id.trim().is_empty() {
        return Err(invalid("subject.subject_id", tokens::SUBJECT_ID_REQUIRED));
    }
    let metric = parse_metric_under_base(&req.metric)
        .ok_or_else(|| invalid("metric", tokens::METRIC_INVALID))?;

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-type
    // `quota_type` is a closed SDK enum whose values are the instances under
    // `gts.cf.qe.quota.type.v1~`; anything else failed deserialization.
    let quota_type = req.quota_type;
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-type

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-rate-if
    if quota_type == QuotaType::Rate {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-rate
        // The identifier and the data-model slot stay reserved; P3 activation
        // needs no migration of existing allocation and consumption Quotas.
        return Err(DomainError::NotYetImplemented {
            feature: RATE_QUOTAS,
        });
        // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-rate
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-rate-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-period-if
    let period = match (quota_type, req.period) {
        (QuotaType::Allocation, Presence::Absent) => None,
        (QuotaType::Allocation, Presence::Null | Presence::Value(_)) => {
            // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-period
            return Err(invalid("period", tokens::PERIOD_NOT_ALLOWED));
            // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-period
        }
        (QuotaType::Consumption, Presence::Value(period)) => Some(period),
        (QuotaType::Consumption, Presence::Absent | Presence::Null) => {
            return Err(invalid("period", tokens::PERIOD_REQUIRED));
        }
        (QuotaType::Rate, _) => unreachable!("rate was rejected above"),
    };
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-period-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-mode
    // `enforcement_mode` is a closed SDK enum under
    // `gts.cf.qe.enforcement.type.v1~`; P1 holds `hard` only, and a future mode
    // arrives as a new variant without API breakage.
    let enforcement_mode = req.enforcement_mode;
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-mode
    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-source
    // `source` is a closed SDK enum under `gts.cf.qe.source.type.v1~` holding
    // the seeded `licensing` and `operator`; a stored value never changes
    // silently because no patch carries the field.
    let source = req.source;
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-source

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-cap-if
    let cap = match req.cap {
        Some(cap) if cap < 0 => {
            // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-cap
            return Err(DomainError::CapMustBeNonNegative { cap });
            // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-cap
        }
        // `0` denies everything and `None` is unbounded: both explicitly valid.
        Some(cap) => Some(cap.unsigned_abs()),
        None => None,
    };
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-cap-if

    validate_thresholds(&req.notification_thresholds)?;
    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-thresh-if
    if !req.notification_thresholds.is_empty() && cap.is_none() {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-thresh
        return Err(DomainError::ThresholdsRequireBoundedCap);
        // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-thresh
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-thresh-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-optional
    validate_window(req.validity_window.as_ref())?;
    let fail_open_hint = req.fail_open_hint;
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-optional

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-multi
    // No uniqueness rule on `(subject, metric)`: several Quotas per pair are
    // resolved at evaluation time under the active Policy.
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-multi

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-return
    Ok(ShapedDraft {
        tenant_id: req.tenant_id,
        subject: req.subject,
        metric,
        quota_type,
        period,
        enforcement_mode,
        cap,
        notification_thresholds: req.notification_thresholds,
        validity_window: req.validity_window,
        fail_open_hint,
        metadata: req.metadata.unwrap_or_default(),
        source,
    })
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-draft-validation:p1:inst-qdv-return
}

/// Each threshold in `1..=100`, strictly ascending.
///
/// # Errors
///
/// `InvalidArgument` on `notification_thresholds`.
pub fn validate_thresholds(thresholds: &[u8]) -> Result<(), DomainError> {
    if thresholds.iter().any(|t| !(1..=100).contains(t)) {
        return Err(invalid(
            "notification_thresholds",
            tokens::THRESHOLD_OUT_OF_RANGE,
        ));
    }
    if thresholds.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid(
            "notification_thresholds",
            tokens::THRESHOLDS_NOT_ASCENDING,
        ));
    }
    Ok(())
}

/// A window's start is not after its end. Absent bounds are unbounded.
///
/// # Errors
///
/// `InvalidArgument` on `validity_window`.
pub fn validate_window(window: Option<&ValidityWindow>) -> Result<(), DomainError> {
    if let Some(window) = window
        && let (Some(start), Some(end)) = (window.start, window.end)
        && start > end
    {
        return Err(invalid("validity_window", tokens::VALIDITY_WINDOW_INVERTED));
    }
    Ok(())
}

/// The request-shape gate of an update: `rate` first, then the immutable
/// fields, then the patch itself. Returns the storage patch.
///
/// # Errors
///
/// `NotYetImplemented` when `quota_type` names `rate`; `InvalidArgument` with
/// `IMMUTABLE_FIELD` for any other present immutable field, `PATCH_EMPTY`, or a
/// threshold or window violation; `CapMustBeNonNegative`;
/// `ThresholdsRequireBoundedCap` for thresholds next to an unbinding cap.
// @cpt-flow:cpt-cf-quota-enforcement-flow-quota-update:p1
pub fn validate_update_shape(req: UpdateQuotaRequest) -> Result<QuotaPatch, DomainError> {
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-rate-if
    if let Presence::Value(value) = &req.quota_type
        && value.as_str() == Some(QuotaType::Rate.as_gts_id())
    {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-rate
        // Before the breaking-change gate, so a patch naming `rate` answers
        // `Unimplemented`, not `IMMUTABLE_FIELD`.
        return Err(DomainError::NotYetImplemented {
            feature: RATE_QUOTAS,
        });
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-rate
    }
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-rate-if
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-breaking-if
    let present = [
        req.metric.is_present(),
        req.quota_type.is_present(),
        req.period.is_present(),
        req.subject.is_present(),
    ];
    if let Some(index) = present.iter().position(|p| *p) {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-breaking
        // A breaking change is a deactivate followed by a create.
        return Err(invalid(IMMUTABLE_FIELDS[index], tokens::IMMUTABLE_FIELD));
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-breaking
    }
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-breaking-if

    let cap = match req.cap {
        Presence::Absent => None,
        Presence::Null => Some(CapPatch::Unbounded),
        Presence::Value(cap) if cap < 0 => {
            return Err(DomainError::CapMustBeNonNegative { cap });
        }
        Presence::Value(cap) => Some(CapPatch::Bounded(cap.unsigned_abs())),
    };
    if let Some(thresholds) = &req.notification_thresholds {
        validate_thresholds(thresholds)?;
    }
    let validity_window = match req.validity_window {
        Presence::Absent => None,
        Presence::Null => Some(ValidityWindowPatch::Clear),
        Presence::Value(window) => {
            validate_window(Some(&window))?;
            Some(ValidityWindowPatch::Set(window))
        }
    };
    let patch = QuotaPatch {
        cap,
        notification_thresholds: req.notification_thresholds,
        validity_window,
        metadata: req.metadata,
        // Filled by the service once the metadata passed its contract.
        constraint_contract: None,
        enforcement_mode: req.enforcement_mode,
        fail_open_hint: req.fail_open_hint,
    };
    if patch.is_empty() {
        return Err(invalid("patch", tokens::PATCH_EMPTY));
    }
    if patch.cap == Some(CapPatch::Unbounded)
        && patch
            .notification_thresholds
            .as_ref()
            .is_some_and(|t| !t.is_empty())
    {
        return Err(DomainError::ThresholdsRequireBoundedCap);
    }
    Ok(patch)
}

/// The patched-shape rule that needs the current row: thresholds on an
/// unbounded merged cap. A fast pre-check; storage decides on the locked row
/// (I14).
///
/// # Errors
///
/// `ThresholdsRequireBoundedCap`.
pub fn validate_patched_shape(current: &Quota, patch: &QuotaPatch) -> Result<(), DomainError> {
    let merged_cap = match patch.cap {
        Some(CapPatch::Bounded(cap)) => Some(cap),
        Some(CapPatch::Unbounded) => None,
        None => current.cap,
    };
    let thresholds_present = patch
        .notification_thresholds
        .as_deref()
        .unwrap_or(&current.notification_thresholds)
        .is_empty()
        .not();
    if merged_cap.is_none() && thresholds_present {
        return Err(DomainError::ThresholdsRequireBoundedCap);
    }
    Ok(())
}

trait Not {
    fn not(self) -> bool;
}

impl Not for bool {
    fn not(self) -> bool {
        !self
    }
}

/// The explicit `subject_id` against the declared scope of the resolved
/// projection: a tenant-scope subject is the target tenant itself; any other
/// scope takes a non-empty, bounded identifier.
///
/// # Errors
///
/// `InvalidArgument` with `SUBJECT_SCOPE_VIOLATION` on `subject.subject_id`.
pub fn validate_subject_scope(
    scope: &SubjectScope,
    tenant_id: TenantId,
    subject_id: &str,
) -> Result<(), DomainError> {
    let accepted = if scope.is_tenant() {
        subject_id == tenant_id.to_string()
    } else {
        !subject_id.trim().is_empty() && subject_id.chars().count() <= SUBJECT_ID_MAX_LEN
    };
    if accepted {
        Ok(())
    } else {
        Err(invalid(
            "subject.subject_id",
            tokens::SUBJECT_SCOPE_VIOLATION,
        ))
    }
}

/// The list gate: the subject halves come together, the ids and the page are
/// bounded, the metric parses. Returns the storage filter and page.
///
/// # Errors
///
/// `InvalidArgument` with the closed token of the violated bound.
pub fn validate_list(
    req: ListQuotasRequest,
    limits: &QuotaLimits,
) -> Result<(QuotaFilter, PageRequest), DomainError> {
    let subject = match (req.projection_type, req.subject_id) {
        (Some(projection_type), Some(subject_id)) => Some(SubjectRef {
            projection_type,
            subject_id,
        }),
        (None, None) => None,
        (Some(_), None) => return Err(invalid("subject_id", tokens::LIST_SUBJECT_INCOMPLETE)),
        (None, Some(_)) => {
            return Err(invalid("projection_type", tokens::LIST_SUBJECT_INCOMPLETE));
        }
    };
    if req.ids.len() > limits.list_max_ids {
        return Err(invalid("id", tokens::LIST_TOO_MANY_IDS));
    }
    let metric = req
        .metric
        .as_deref()
        .map(|text| {
            parse_metric_under_base(text).ok_or_else(|| invalid("metric", tokens::METRIC_INVALID))
        })
        .transpose()?;
    let limit = match req.limit {
        None => PageRequest::DEFAULT_LIMIT.min(limits.list_max_limit),
        Some(limit) if limit == 0 || limit > limits.list_max_limit => {
            return Err(invalid("limit", tokens::LIST_LIMIT_OUT_OF_RANGE));
        }
        Some(limit) => limit,
    };
    Ok((
        QuotaFilter {
            tenant_id: req.tenant_id,
            subject,
            metric,
            status: req.status,
            ids: req.ids,
        },
        PageRequest {
            limit,
            cursor: req.cursor,
        },
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "validation_tests.rs"]
mod validation_tests;
