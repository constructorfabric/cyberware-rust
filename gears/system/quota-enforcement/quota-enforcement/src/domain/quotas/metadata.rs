//! Quota metadata validation (`features/quota-lifecycle.md`, "Quota Metadata
//! Validation"; ADR-0007).
//!
//! The operator's metadata object is validated once, at create or update,
//! against the constraint contract attached to the metric's request contract,
//! and never again during evaluation. The size bound applies to the compact
//! JSON encoding, whose length is the same whatever order the keys are written
//! in. The content stays opaque: it is validated for shape, stored and
//! forwarded verbatim, and never logged.

use quota_enforcement_sdk::ContractRef;
use serde_json::{Map, Value, json};

use crate::domain::catalog::ConstraintContract;
use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{QeMetrics, ValidationReason, ValidationSurface};
use crate::domain::tokens;

const LOG_TARGET: &str = "qe.quotas";

/// Validate `metadata` against `constraint` and return the contract reference
/// to snapshot with the row.
///
/// # Errors
///
/// `InvalidArgument` with `METADATA_TOO_LARGE` when the compact JSON encoding
/// exceeds `max_bytes`; `ConstraintContractMismatch` when the envelope
/// violates the contract; `Internal` when the object does not serialize.
///
/// The measured size is independent of key order, so the bound is well
/// defined. The encoding itself is not byte-canonical: key order follows
/// whichever `serde_json` map implementation feature unification selects, so
/// nothing may hash or compare these bytes.
// @cpt-algo:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1
// @cpt-dod:cpt-cf-quota-enforcement-dod-quota-metadata:p1
pub fn validate_metadata(
    metadata: &Map<String, Value>,
    constraint: &ConstraintContract,
    max_bytes: usize,
    metrics: &dyn QeMetrics,
) -> Result<ContractRef, DomainError> {
    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-size-if
    let encoded = serde_json::to_vec(metadata)
        .map_err(|e| DomainError::Internal(format!("metadata does not serialize: {e}")))?;
    if encoded.len() > max_bytes {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-size
        tracing::warn!(
            target: LOG_TARGET,
            size = encoded.len(),
            limit = max_bytes,
            "quota metadata exceeds the configured size limit"
        );
        return Err(DomainError::InvalidArgument {
            field: "metadata",
            reason: tokens::METADATA_TOO_LARGE,
        });
        // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-size
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-size-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-contract
    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-envelope
    // The whole `{type, metadata}` document is validated, never the inner
    // object alone: the inner path would skip the base's own `required` and
    // `additionalProperties` rules (ADR-0007).
    let envelope = json!({
        "type": constraint.reference.type_id.as_ref(),
        "metadata": metadata,
    });
    let outcome = constraint.contract.validate(&envelope);
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-envelope
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-contract

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-mismatch-if
    if let Err(violations) = outcome {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-mismatch
        metrics.record_contract_validation_failure(
            ValidationSurface::Arbitration,
            ValidationReason::SchemaViolation,
        );
        // The count, not the messages: the messages quote the values.
        tracing::warn!(
            target: LOG_TARGET,
            contract = %constraint.reference.type_id,
            violations = violations.len(),
            "quota metadata violates its constraint contract"
        );
        return Err(DomainError::ConstraintContractMismatch {
            contract: constraint.reference.type_id.as_ref().to_owned(),
        });
        // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-mismatch
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-mismatch-if

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-snapshot
    // The accepted contract id and version travel with the row; a stored value
    // is not revalidated during evaluation (ADR-0003, ADR-0007).
    let snapshot = constraint.reference.clone();
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-snapshot

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-opaque
    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-pii
    // Nothing here reads a key for its meaning or indexes one: the object is
    // Platform Operational Data the operator keeps free of regulated content,
    // stored and forwarded verbatim as the Engine's `arbitration` object.
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-pii
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-opaque

    // @cpt-begin:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-return
    Ok(snapshot)
    // @cpt-end:cpt-cf-quota-enforcement-algo-quota-metadata-validation:p1:inst-qmd-return
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metadata_tests.rs"]
mod metadata_tests;
