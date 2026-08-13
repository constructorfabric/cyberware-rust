//! Shared helpers used by the record and catalog stores.

use time::OffsetDateTime;

use usage_collector_sdk::{UsageCollectorPluginError, UsageRecord};

use super::entity::{EpochMicros, UsageRecordRow, UsageRecordStatusCode, ValidatedMetadata};

/// Build a deactivation-marker row with `status = inactive` and a bumped `version`.
#[must_use]
pub fn make_inactive_marker(
    source: &UsageRecordRow,
    base_version: u64,
    offset: u64,
) -> UsageRecordRow {
    let mut marker = source.clone();
    marker.status = UsageRecordStatusCode::Inactive;
    marker.version = base_version.saturating_add(offset);
    marker
}

/// Extract the cursor-key string for a field name from a row.
///
/// Returns `None` for unknown fields or `NULL` optional columns.
#[must_use]
pub fn record_row_key(row: &UsageRecordRow, field: &str) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    match field {
        "id" => Some(row.id.to_string()),
        "corrects_id" => row.corrects_id.map(|id| id.to_string()),
        "created_at" => {
            let dt = OffsetDateTime::try_from(EpochMicros(row.created_at)).ok()?;
            dt.format(&Rfc3339).ok()
        }
        "tenant_id" => Some(row.tenant_id.to_string()),
        "resource_id" => Some(row.resource_id.clone()),
        "resource_type" => Some(row.resource_type.clone()),
        "subject_id" => row.subject_id.clone(),
        "subject_type" => row.subject_type.clone(),
        "status" => Some(<&'static str>::from(row.status).to_owned()),
        _ => None,
    }
}

/// Compare canonical fields of a stored row against an incoming record for
/// dedup absorption vs [`UsageCollectorPluginError::IdempotencyConflict`].
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when stored metadata cannot
/// be decoded.
pub fn canonical_equal(
    row: &UsageRecordRow,
    incoming: &UsageRecord,
) -> Result<bool, UsageCollectorPluginError> {
    let stored_metadata = ValidatedMetadata::try_from(row.metadata.clone())?.0;
    Ok(row.id == incoming.id
        && row.value == incoming.value
        && row.resource_id == incoming.resource_ref.resource_id()
        && row.resource_type == incoming.resource_ref.resource_type()
        && row.subject_id.as_deref()
            == incoming
                .subject_ref
                .as_ref()
                .map(usage_collector_sdk::SubjectRef::subject_id)
        && row.subject_type.as_deref()
            == incoming.subject_ref.as_ref().and_then(|s| s.subject_type())
        && row.corrects_id == incoming.corrects_id
        && stored_metadata == incoming.metadata)
}

/// Mint a `ReplacingMergeTree` merge-resolution version (epoch microseconds as `u64`).
#[must_use]
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "micros since epoch is always positive and fits in u64 for practical timestamps"
)]
pub fn current_merge_version() -> u64 {
    let micros = usage_collector_sdk::created_at_micros(OffsetDateTime::now_utc());
    micros as u64
}

/// Compute a `version` strictly higher than `existing_version` by at least `offset + 1`.
#[must_use]
pub fn version_higher_than(existing_version: u64, offset: u64) -> u64 {
    let now = current_merge_version();
    now.max(existing_version.saturating_add(offset).saturating_add(1))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "mapper_tests.rs"]
mod mapper_tests;
