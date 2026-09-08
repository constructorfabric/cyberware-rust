//! Closed `UPPER_SNAKE` reason tokens of the `InvalidArgument` field violations
//! the projection-contracts feature raises (DESIGN section 3.3, "Error Model":
//! discriminators ride as `errors[].reason` tokens, never as private variants).

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
