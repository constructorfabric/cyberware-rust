//! Stable admission failure codes used in stored outcomes and metrics.

use toolkit_macros::domain_model;

/// A candidate refusal, or a code preserved from another service version.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionFailureReason {
    ActivationWriteSetExceeded,
    AlreadyExists,
    DependentInvalid,
    EntityDeleted,
    FamilyKindConflict,
    FamilyShapeConflict,
    InvalidDocument,
    InvalidIdentifier,
    InvalidSchema,
    InvalidValue,
    MissingPredecessor,
    PreconditionFailed,
    ResolutionClosureExceeded,
    ResolvedDocumentTooLarge,
    RevalidationExhausted,
    UnparsablePayload,
    UnreadableVersion,
    UnrecognizedPayload,
    /// Preserve an unrecognized stored code without adding a metric label.
    Unknown(String),
}

impl AdmissionFailureReason {
    /// Restore a typed reason while retaining codes from other service versions.
    #[must_use]
    pub fn from_wire(code: &str) -> Self {
        match code {
            "activation_write_set_exceeded" => Self::ActivationWriteSetExceeded,
            "already_exists" => Self::AlreadyExists,
            "dependent_invalid" => Self::DependentInvalid,
            "entity_deleted" => Self::EntityDeleted,
            "family_kind_conflict" => Self::FamilyKindConflict,
            "family_shape_conflict" => Self::FamilyShapeConflict,
            "invalid_document" => Self::InvalidDocument,
            "invalid_identifier" => Self::InvalidIdentifier,
            "invalid_schema" => Self::InvalidSchema,
            "invalid_value" => Self::InvalidValue,
            "missing_predecessor" => Self::MissingPredecessor,
            "precondition_failed" => Self::PreconditionFailed,
            "resolution_closure_exceeded" => Self::ResolutionClosureExceeded,
            "resolved_document_too_large" => Self::ResolvedDocumentTooLarge,
            "revalidation_exhausted" => Self::RevalidationExhausted,
            "unparsable_payload" => Self::UnparsablePayload,
            "unreadable_version" => Self::UnreadableVersion,
            "unrecognized_payload" => Self::UnrecognizedPayload,
            unknown => Self::Unknown(unknown.to_owned()),
        }
    }

    /// The stable code persisted in error payloads and returned to clients.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unknown(code) => code,
            known => known.metric_label(),
        }
    }

    /// A bounded metric label: unknown codes share the single `other` series.
    #[must_use]
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::ActivationWriteSetExceeded => "activation_write_set_exceeded",
            Self::AlreadyExists => "already_exists",
            Self::DependentInvalid => "dependent_invalid",
            Self::EntityDeleted => "entity_deleted",
            Self::FamilyKindConflict => "family_kind_conflict",
            Self::FamilyShapeConflict => "family_shape_conflict",
            Self::InvalidDocument => "invalid_document",
            Self::InvalidIdentifier => "invalid_identifier",
            Self::InvalidSchema => "invalid_schema",
            Self::InvalidValue => "invalid_value",
            Self::MissingPredecessor => "missing_predecessor",
            Self::PreconditionFailed => "precondition_failed",
            Self::ResolutionClosureExceeded => "resolution_closure_exceeded",
            Self::ResolvedDocumentTooLarge => "resolved_document_too_large",
            Self::RevalidationExhausted => "revalidation_exhausted",
            Self::UnparsablePayload => "unparsable_payload",
            Self::UnreadableVersion => "unreadable_version",
            Self::UnrecognizedPayload => "unrecognized_payload",
            Self::Unknown(_) => "other",
        }
    }
}

impl std::fmt::Display for AdmissionFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
