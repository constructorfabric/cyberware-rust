//! The catalogue-membership check Quota and Policy writes run before
//! persistence (`features/projection-contracts.md`, "Catalogue-Membership
//! Check for Quota and Policy Writes"). The evaluation hot path never runs it.

use gts::GtsTypeId;
use quota_enforcement_sdk::{MetricId, SUBJECT_BASE, SubjectScope};
use serde_json::Value;

use super::model::ProjectionContractCatalog;
use crate::domain::error::DomainError;
use crate::domain::ports::contracts::RegisteredType;
use crate::domain::ports::metrics::{QeMetrics, ValidationReason, ValidationSurface};
use crate::domain::tokens;

const LOG_TARGET: &str = "qe.catalog";

/// Check a projection reference carried by a write against the registry
/// snapshot the write path took (`snapshot`, `None` when the type is not
/// registered) and the published catalogue. `metric` is the Quota's metric on
/// Quota writes.
///
/// # Errors
///
/// - [`DomainError::ProjectionNotRegistered`] for an unregistered reference.
/// - [`DomainError::InvalidArgument`] with `PROJECTION_INVALID` for an
///   abstract, non-subject, unknown-scope, or non-derived reference, and with
///   `METRIC_NOT_ADMITTED` when the projection does not admit the metric.
/// - [`DomainError::ProjectionNotResolvable`] for a registered projection
///   outside the configured catalogue.
// @cpt-algo:cpt-cf-quota-enforcement-algo-catalog-membership:p1
// @cpt-dod:cpt-cf-quota-enforcement-dod-catalog-membership:p1
pub fn check_projection_reference(
    catalog: &ProjectionContractCatalog,
    metrics: &dyn QeMetrics,
    surface: ValidationSurface,
    snapshot: Option<&RegisteredType>,
    projection: &GtsTypeId,
    metric: Option<&MetricId>,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-lookup
    let Some(registered) = snapshot else {
        record(metrics, surface, ValidationReason::Unregistered, projection);
        return Err(DomainError::ProjectionNotRegistered {
            projection: projection.to_string(),
        });
    };
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-lookup

    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-invalid-if
    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-invalid
    if registered.is_abstract {
        return Err(invalid(
            metrics,
            surface,
            ValidationReason::Abstract,
            projection,
        ));
    }
    if !registered.derives_from(SUBJECT_BASE) {
        return Err(invalid(
            metrics,
            surface,
            ValidationReason::NotDerived,
            projection,
        ));
    }
    let scope_ok = registered
        .effective_traits
        .get("scope")
        .and_then(Value::as_str)
        .is_some_and(|raw| SubjectScope::parse(raw).is_ok());
    if !scope_ok {
        return Err(invalid(
            metrics,
            surface,
            ValidationReason::ScopeInvalid,
            projection,
        ));
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-invalid
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-invalid-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-nonconf-if
    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-nonconf
    if catalog.subject_projection(projection).is_none() {
        record(
            metrics,
            surface,
            ValidationReason::ProjectionNotResolvable,
            projection,
        );
        return Err(DomainError::ProjectionNotResolvable {
            projection: projection.to_string(),
        });
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-nonconf
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-nonconf-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-metric-if
    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-metric
    if let Some(metric) = metric
        && !catalog.admits(projection, metric)
    {
        metrics.record_admitted_metric_violation(surface);
        tracing::warn!(
            target: LOG_TARGET,
            %surface,
            projection = %projection,
            metric = %metric,
            "the projection does not admit the metric"
        );
        return Err(DomainError::InvalidArgument {
            field: "metric",
            reason: tokens::METRIC_NOT_ADMITTED,
        });
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-metric
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-metric-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-pass
    Ok(())
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-membership:p1:inst-mem-pass
}

fn record(
    metrics: &dyn QeMetrics,
    surface: ValidationSurface,
    reason: ValidationReason,
    projection: &GtsTypeId,
) {
    metrics.record_contract_validation_failure(surface, reason);
    tracing::warn!(
        target: LOG_TARGET,
        %surface,
        %reason,
        projection = %projection,
        "projection reference rejected"
    );
}

fn invalid(
    metrics: &dyn QeMetrics,
    surface: ValidationSurface,
    reason: ValidationReason,
    projection: &GtsTypeId,
) -> DomainError {
    record(metrics, surface, reason, projection);
    DomainError::InvalidArgument {
        field: "subject.projection_type",
        reason: tokens::PROJECTION_INVALID,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "membership_tests.rs"]
mod membership_tests;
