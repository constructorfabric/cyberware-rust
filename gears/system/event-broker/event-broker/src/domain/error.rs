//! Domain errors for the Event Broker.
//!
//! Coarse-grained by HTTP-status family (`api/rest/error.rs` maps each
//! variant to one status code) rather than one variant per
//! `docs/openapi.yaml` machine code - the specific code (e.g.
//! `BadTypePattern`, `ConsumerGroupHasActiveMembers`) rides along as a
//! `&'static str` field instead, since there are dozens of them across the
//! Hard-Error Catalogs and they don't otherwise affect handling here.

use thiserror::Error;
use toolkit::domain_model;

#[domain_model]
#[derive(Debug, Error)]
pub enum DomainError {
    /// Client-supplied input failed validation - `api/rest/error.rs` lifts
    /// this to `400`. `code` is the `docs/openapi.yaml` machine code (e.g.
    /// `BadTypePattern`, `InvalidMode`, `InvalidPartition`).
    #[error("{code}: {message}")]
    Validation { code: &'static str, message: String },

    /// A text field broke its printable-ASCII or byte-length rule - lifted to
    /// `400` with a `context.field_violations[]` entry naming the field.
    ///
    /// Distinct from `Validation` because that variant has no field slot, and
    /// a caller needs to know which field to correct. Deliberately carries no
    /// part of the submitted value: `detail` states what the rule requires and
    /// `reason` says which rule it was, so a rejection cannot become a channel
    /// for echoing request content back through logs and clients.
    #[error("{field}: {detail}")]
    TextField {
        field: &'static str,
        detail: String,
        reason: &'static str,
    },

    /// One or more seek entries named a position their partition does not
    /// admit - lifted to `400` with a `context.field_violations[]` entry per
    /// offender, so a caller correcting several does not need one round trip
    /// each.
    ///
    /// Every offender is reported rather than the first, and none of them
    /// carries the submitted value: the violation names the entry by its index
    /// in the request and the range that entry missed, both of which the
    /// caller can act on without the rejection quoting anything back.
    #[error("{} seek position(s) outside the admissible range", violations.len())]
    SeekOutOfRange {
        violations: Vec<event_broker_sdk::PositionViolation>,
    },

    /// Caller is not allowed to perform this operation on this resource -
    /// lifted to `403`. Distinct from `NotFound` even where
    /// `docs/openapi.yaml` deliberately conflates them for information
    /// hiding (callers decide whether to conflate at the REST layer).
    /// `resource` is the raw identifier (not a full sentence) - `DESIGN.md`'s
    /// Hard-Error Catalog carries this separately as `context.resource_name`.
    #[error("{code}: {message}")]
    Forbidden {
        code: &'static str,
        message: String,
        resource: String,
    },

    /// Referenced resource does not exist - lifted to `404`. `resource` is
    /// the raw identifier, matching `DESIGN.md`'s `context.resource_name`.
    #[error("{code}: {message}")]
    NotFound {
        code: &'static str,
        message: String,
        resource: String,
    },

    /// Request conflicts with current state - lifted to `409`. `resource` is
    /// the raw identifier, matching `DESIGN.md`'s `context.resource_name`.
    #[error("{code}: {message}")]
    Conflict {
        code: &'static str,
        message: String,
        resource: String,
    },

    /// Producer chain metadata does not match the broker's stored state.
    /// `docs/openapi.yaml` documents this as `412` - `api/rest/error.rs`
    /// keeps the canonical `FailedPrecondition` category but overrides the
    /// wire status to `412` via `TransportOverride`, with `last_sequence`
    /// echoed in the `Problem` context so the producer can resync.
    #[error(
        "sequence violation on topic {topic} partition {partition}: last_sequence={last_sequence}"
    )]
    SequenceViolation {
        topic: String,
        partition: i32,
        last_sequence: i64,
    },

    /// A batch exceeded the documented item-count ceiling. `docs/openapi.yaml`
    /// documents this as `413` - `api/rest/error.rs` overrides the wire
    /// status accordingly while keeping the canonical `InvalidArgument`
    /// category.
    #[error("batch too large: {count} events (max {max})")]
    BatchTooLarge { count: usize, max: usize },

    /// Per-tenant/per-resource rate cap exceeded - lifted to `429` with a
    /// `Retry-After` header.
    #[error("{code}: {message}")]
    RateLimited {
        code: &'static str,
        message: String,
        retry_after_secs: u32,
    },

    #[error("storage backend unavailable: {0}")]
    StorageUnavailable(String),

    #[error("internal error: {0}")]
    Internal(String),
}

/// `EnforcerError::Denied` -> `Forbidden` with a generic `"AuthzDenied"` code;
/// call sites that need a specific `docs/openapi.yaml` code
/// (`TopicNotAuthorized`, `EventTypeNotAuthorized`, `NotAuthorizedToProduce`,
/// `TenantIdNotAuthorized`) overwrite `code` after this conversion
/// (`eb-authz-enforcement`'s design.md "code-per-call-site" - tenant scope is
/// its own `PolicyEnforcer` call via `domain::authz::tenant_authorized`, not
/// a separate SDK, so it flows through this same `From` impl). The two
/// non-denial variants are PDP/evaluation failures, not policy denials -
/// lifted to `Internal`, matching `oagw`'s own `EnforcerError`-to-domain-error
/// mapping.
// `toolkit_db::DbError`/`ScopeError` -> `DomainError` impls live in
// `infra/storage/error.rs`, not here - `domain/` has no infra dependencies
// (this file's own module doc comment, `domain/mod.rs`), and Rust's orphan
// rule permits a foreign-trait-for-local-type `impl` from anywhere in the
// crate, not only alongside the type's own definition.

/// `event_broker_sdk::StorageBackendError` -> `DomainError`
/// (eb-single-process-implementation D3; `event-broker-canonical-errors`'s
/// "EventBrokerBackend operations return backend errors with canonical
/// projection" requirement - previously unfulfilled, since no backend
/// implementation existed yet to produce this error type in the first
/// place). `event_broker_sdk` is an established `domain/`-allowed
/// dependency (`domain/authz.rs` already consumes its GTS constants), not
/// an infra type.
impl From<event_broker_sdk::StorageBackendError> for DomainError {
    fn from(err: event_broker_sdk::StorageBackendError) -> Self {
        use event_broker_sdk::StorageBackendError;
        match err {
            // Reached only where the caller could not attribute the failure
            // to one request entry - SEEK does, and builds
            // `SeekOutOfRange` itself. The range is broker state, so quoting
            // it here echoes nothing the caller submitted.
            StorageBackendError::OffsetOutOfRange {
                floor,
                ceiling,
                breached,
                ..
            } => DomainError::Validation {
                code: "OffsetOutOfRange",
                message: format!(
                    "the requested position is {} the valid range [{floor}, {ceiling}]",
                    breached.phrase()
                ),
            },
            StorageBackendError::PartitionNotFound { .. } => DomainError::NotFound {
                code: "PartitionNotFound",
                message: err.to_string(),
                resource: String::new(),
            },
            StorageBackendError::Unavailable { .. }
            | StorageBackendError::InvalidConfig { .. }
            | StorageBackendError::PersistFailed { .. }
            | StorageBackendError::ReadFailed { .. }
            // Never reaches a request: a retention pass runs on the broker's
            // own tick and answers no caller. Mapped rather than left to a
            // catch-all so a new variant stays a compile error here.
            | StorageBackendError::RetentionFailed { .. }
            | StorageBackendError::Internal(_) => DomainError::StorageUnavailable(err.to_string()),
        }
    }
}

impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    fn from(err: authz_resolver_sdk::EnforcerError) -> Self {
        use authz_resolver_sdk::EnforcerError;
        match err {
            EnforcerError::Denied { deny_reason } => DomainError::Forbidden {
                code: "AuthzDenied",
                message: deny_reason
                    .and_then(|r| r.details)
                    .unwrap_or_else(|| "access denied by policy".to_owned()),
                resource: String::new(),
            },
            EnforcerError::CompileFailed(e) => {
                DomainError::Internal(format!("authz constraint compilation failed: {e}"))
            }
            EnforcerError::EvaluationFailed(e) => {
                DomainError::Internal(format!("authz evaluation failed: {e}"))
            }
        }
    }
}

impl From<event_broker_sdk::validate::TextFieldError> for DomainError {
    fn from(err: event_broker_sdk::validate::TextFieldError) -> Self {
        DomainError::TextField {
            field: err.field(),
            detail: err.detail().to_owned(),
            reason: err.rule().as_reason(),
        }
    }
}
