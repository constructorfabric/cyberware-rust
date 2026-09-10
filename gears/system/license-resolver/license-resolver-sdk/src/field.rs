//! Wire `field` / `reason` vocabulary for the violations carried by
//! [`InvalidRequest`](crate::LicenseResolverError::InvalidRequest).
//!
//! A rejected check reports **every** contract violation it found, each as a
//! [`FieldViolation`](crate::FieldViolation) whose three slots mean:
//!
//! - `field` — where the offending element sits *in the request*: a role root
//!   ([`SUBJECT_FIELD`] / [`RESOURCE_FIELD`]) followed by a JSON pointer into
//!   that contract object, e.g. `resource/metadata/model_name`. A violation of
//!   the contract type itself is reported at [`SUBJECT_TYPE_FIELD`] /
//!   [`RESOURCE_TYPE_FIELD`].
//! - `reason` — one of the codes in this module. This is the dispatch
//!   discriminator, so it is fanned into the typed [`ValidationReason`]
//!   sub-enum: a consumer matches a variant instead of comparing wire strings.
//! - `description` — human-readable, and the slot that names the registered
//!   contract type the request was judged against.
//!
//! Every reason code is a **validation error, never a not-granted decision** —
//! see [`LicenseResolverError`](crate::LicenseResolverError).
//!
//! This vocabulary is the resolver's own. A backend plugin that rejects a
//! conforming request over a constraint its contract does not express carries
//! its own `field` / `reason` values, which land in
//! [`ValidationReason::Unknown`].

/// Root of a `field` path pointing into the request's Subject contract object.
pub const SUBJECT_FIELD: &str = "subject";

/// Root of a `field` path pointing into the request's Resource contract object.
pub const RESOURCE_FIELD: &str = "resource";

/// The `field` value for a violation of the Subject's contract type itself.
pub const SUBJECT_TYPE_FIELD: &str = "subject/type";

/// The `field` value for a violation of the Resource's contract type itself.
pub const RESOURCE_TYPE_FIELD: &str = "resource/type";

/// The declared contract type is not registered in the types registry.
pub const CONTRACT_NOT_REGISTERED: &str = "CONTRACT_NOT_REGISTERED";

/// The declared contract type is not a well-formed GTS type id.
pub const CONTRACT_TYPE_MALFORMED: &str = "CONTRACT_TYPE_MALFORMED";

/// The contract type does not derive from the licensing base type for its role.
pub const CONTRACT_NOT_DERIVED: &str = "CONTRACT_NOT_DERIVED";

/// The contract type is abstract, so it cannot be instantiated by a check.
pub const CONTRACT_ABSTRACT: &str = "CONTRACT_ABSTRACT";

/// The contract object does not conform to its registered contract schema.
pub const SCHEMA_MISMATCH: &str = "SCHEMA_MISMATCH";

/// The Subject contract type is not admitted by the Resource contract type.
pub const SUBJECT_NOT_ADMITTED: &str = "SUBJECT_NOT_ADMITTED";

/// Typed view of the `reason` codes declared above.
///
/// [`from_wire`](Self::from_wire) returns `Self` rather than `Option<Self>`
/// because every `reason` the resolver emits is one of the modeled values. The
/// [`Unknown`](Self::Unknown) catch-all fires for a code added by a newer
/// resolver, or for one a backend plugin raised from its own vocabulary — either
/// way a consumer's `match` keeps compiling and stays forward-compatible.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationReason {
    /// See [`CONTRACT_NOT_REGISTERED`].
    ContractNotRegistered,
    /// See [`CONTRACT_TYPE_MALFORMED`].
    ContractTypeMalformed,
    /// See [`CONTRACT_NOT_DERIVED`].
    ContractNotDerived,
    /// See [`CONTRACT_ABSTRACT`].
    ContractAbstract,
    /// See [`SCHEMA_MISMATCH`].
    SchemaMismatch,
    /// See [`SUBJECT_NOT_ADMITTED`].
    SubjectNotAdmitted,
    /// Unmodeled reason — a future resolver code, or a backend's own —
    /// preserves the raw wire string.
    Unknown(String),
}

impl ValidationReason {
    /// Project a wire `reason` string into the typed discriminator.
    ///
    /// Any unmodeled value is preserved in [`Unknown`](Self::Unknown).
    #[must_use]
    pub fn from_wire(reason: &str) -> Self {
        match reason {
            CONTRACT_NOT_REGISTERED => Self::ContractNotRegistered,
            CONTRACT_TYPE_MALFORMED => Self::ContractTypeMalformed,
            CONTRACT_NOT_DERIVED => Self::ContractNotDerived,
            CONTRACT_ABSTRACT => Self::ContractAbstract,
            SCHEMA_MISMATCH => Self::SchemaMismatch,
            SUBJECT_NOT_ADMITTED => Self::SubjectNotAdmitted,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Render the discriminator back to its wire `reason` string.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::ContractNotRegistered => CONTRACT_NOT_REGISTERED,
            Self::ContractTypeMalformed => CONTRACT_TYPE_MALFORMED,
            Self::ContractNotDerived => CONTRACT_NOT_DERIVED,
            Self::ContractAbstract => CONTRACT_ABSTRACT,
            Self::SchemaMismatch => SCHEMA_MISMATCH,
            Self::SubjectNotAdmitted => SUBJECT_NOT_ADMITTED,
            Self::Unknown(reason) => reason.as_str(),
        }
    }
}

impl core::fmt::Display for ValidationReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_wire())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "field_tests.rs"]
mod field_tests;
