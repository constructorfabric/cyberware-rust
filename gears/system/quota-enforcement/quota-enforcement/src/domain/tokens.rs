//! Closed `UPPER_SNAKE` reason tokens of the `InvalidArgument` field violations
//! the projection-contracts and quota-lifecycle features raise (DESIGN section
//! 3.3, "Error Model": discriminators ride as `errors[].reason` tokens, never
//! as private variants).

/// `tenant_id` is nil.
pub const TENANT_ID_REQUIRED: &str = "TENANT_ID_REQUIRED";
/// `metric` is not a well-formed instance of the metric base.
pub const METRIC_INVALID: &str = "METRIC_INVALID";
/// The metric is not admitted by any configured projection at the kind's scope.
pub const METRIC_NOT_ADMITTED: &str = "METRIC_NOT_ADMITTED";
/// A subject id is empty.
pub const SUBJECT_ID_REQUIRED: &str = "SUBJECT_ID_REQUIRED";
/// A subject kind is not a well-formed instance of the scope type.
pub const SUBJECT_KIND_INVALID: &str = "SUBJECT_KIND_INVALID";
/// Two subjects carry the same kind.
pub const SUBJECT_KIND_DUPLICATE: &str = "SUBJECT_KIND_DUPLICATE";
/// A subject kind is a scope no configured projection admits the metric at.
pub const SUBJECT_KIND_NOT_ADMITTED: &str = "SUBJECT_KIND_NOT_ADMITTED";
/// The tenant scope appears in `subjects`; it is materialized from `tenant_id`.
pub const TENANT_SCOPE_REPEATED: &str = "TENANT_SCOPE_REPEATED";
/// The operation-level `metadata` object is absent.
pub const METADATA_REQUIRED: &str = "METADATA_REQUIRED";
/// The resource `type` is not a well-formed GTS type id.
pub const RESOURCE_TYPE_INVALID: &str = "RESOURCE_TYPE_INVALID";
/// The resource `type` is not a configured resource projection.
pub const RESOURCE_TYPE_UNKNOWN: &str = "RESOURCE_TYPE_UNKNOWN";
/// The resource `metadata` object is absent.
pub const RESOURCE_METADATA_REQUIRED: &str = "RESOURCE_METADATA_REQUIRED";
/// A metadata or resource document violated its contract.
pub const CONTRACT_VIOLATION: &str = "CONTRACT_VIOLATION";
/// A projection reference is abstract, not a subject projection, of unknown
/// scope, or not derived from the QE base.
pub const PROJECTION_INVALID: &str = "PROJECTION_INVALID";

// --- quota lifecycle ---

/// A patch names an immutable field (`metric`, `quota_type`, `period`,
/// `subject`); a breaking change is a deactivate followed by a create.
pub const IMMUTABLE_FIELD: &str = "IMMUTABLE_FIELD";
/// A patch changes nothing.
pub const PATCH_EMPTY: &str = "PATCH_EMPTY";
/// A consumption Quota carries no period.
pub const PERIOD_REQUIRED: &str = "PERIOD_REQUIRED";
/// An allocation Quota carries a period field, `null` included.
pub const PERIOD_NOT_ALLOWED: &str = "PERIOD_NOT_ALLOWED";
/// A notification threshold is outside `1..=100`.
pub const THRESHOLD_OUT_OF_RANGE: &str = "THRESHOLD_OUT_OF_RANGE";
/// Notification thresholds are not strictly ascending.
pub const THRESHOLDS_NOT_ASCENDING: &str = "THRESHOLDS_NOT_ASCENDING";
/// A validity window ends before it starts.
pub const VALIDITY_WINDOW_INVERTED: &str = "VALIDITY_WINDOW_INVERTED";
/// The `subject_id` violates the declared scope of the projection.
pub const SUBJECT_SCOPE_VIOLATION: &str = "SUBJECT_SCOPE_VIOLATION";
/// The canonical JSON of `metadata` exceeds the configured size limit.
pub const METADATA_TOO_LARGE: &str = "METADATA_TOO_LARGE";
/// A list filter names one half of the subject only.
pub const LIST_SUBJECT_INCOMPLETE: &str = "LIST_SUBJECT_INCOMPLETE";
/// A list filter names more ids than the configured maximum.
pub const LIST_TOO_MANY_IDS: &str = "LIST_TOO_MANY_IDS";
/// A list page limit is zero or above the configured maximum.
pub const LIST_LIMIT_OUT_OF_RANGE: &str = "LIST_LIMIT_OUT_OF_RANGE";
/// A Quota identifier is not a UUID.
pub const QUOTA_ID_INVALID: &str = "QUOTA_ID_INVALID";
/// A tenant identifier is not a UUID.
pub const TENANT_ID_INVALID: &str = "TENANT_ID_INVALID";
/// A status filter is neither `active` nor `deactivated`.
pub const STATUS_INVALID: &str = "STATUS_INVALID";
/// A cap above `Quota::MAX_CAP`; caps live in `0..=i64::MAX`.
pub const CAP_OUT_OF_RANGE: &str = "CAP_OUT_OF_RANGE";
/// A list continuation cursor that storage does not decode.
pub const CURSOR_INVALID: &str = "CURSOR_INVALID";
/// A client set `constraint_contract` on a patch; the gear alone fills it.
pub const CONSTRAINT_CONTRACT_NOT_CALLER_SUPPLIED: &str = "CONSTRAINT_CONTRACT_NOT_CALLER_SUPPLIED";
