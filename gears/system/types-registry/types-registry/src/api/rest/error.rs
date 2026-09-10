//! REST error mapping for the Types Registry gear.

use toolkit_canonical_errors::{CanonicalError, resource_error};
use types_registry_sdk::{field, precondition};

use crate::domain::admission::acceptance::{AcceptanceError, MAX_IDEMPOTENCY_KEY};
use crate::domain::admission::worker::WorkerError;
use crate::domain::error::DomainError;
use crate::domain::registry_service::ServiceError;

#[resource_error(gts_id!("cf.types_registry.registry.type.v1~"))]
pub struct TypeRegistryError;

impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::InvalidGtsId(msg) => TypeRegistryError::invalid_argument()
                .with_field_violation(field::GTS_ID_FIELD, msg, field::INVALID_GTS_ID)
                .create(),
            DomainError::NotFound { kind, target } => {
                TypeRegistryError::not_found(format!("No entity with {kind}: {target}"))
                    .with_resource(target)
                    .create()
            }
            DomainError::AlreadyExists(id) => TypeRegistryError::already_exists(format!(
                "Entity with GTS ID already exists: {id}"
            ))
            .with_resource(id)
            .create(),
            // Adapter-only (in-process) batch-register disposition — never
            // reaches a REST handler. Carries `parent_type_id` / `dependent_id`
            // losslessly so the SDK projects it back to
            // `TypesRegistryError::ParentNotRegistered`: dependent → resource,
            // parent → violation subject, message → violation description.
            DomainError::ParentTypeSchemaNotRegistered {
                parent_type_id,
                dependent_id,
            } => {
                let detail = format!(
                    "Cannot register {dependent_id}: required type-schema {parent_type_id} is not registered"
                );
                TypeRegistryError::failed_precondition()
                    .with_resource(dependent_id)
                    .with_precondition_violation(
                        parent_type_id,
                        detail,
                        precondition::PARENT_NOT_REGISTERED,
                    )
                    .create()
            }
            DomainError::InvalidQuery(msg) => TypeRegistryError::invalid_argument()
                .with_field_violation(field::QUERY_FIELD, msg, field::INVALID_QUERY)
                .create(),
            DomainError::ValidationFailed(msg) => TypeRegistryError::invalid_argument()
                .with_field_violation(field::ENTITY_FIELD, msg, field::VALIDATION_FAILED)
                .create(),
            DomainError::NotInReadyMode => CanonicalError::service_unavailable().create(),
            DomainError::ReadyCommitFailed(errors) => {
                // Unreachable from REST handlers — `switch_to_ready` runs in
                // gear `post_init` only. Kept for `From` exhaustiveness.
                // If it ever surfaces, we want an opaque internal response;
                // the validation detail is logged server-side and preserved
                // on the canonical error's diagnostic field.
                for ve in &errors {
                    tracing::error!(
                        gts_id = %ve.gts_id,
                        message = %ve.message,
                        "types_registry ready commit validation failure"
                    );
                }
                let summary = format!("ready commit failed with {} errors", errors.len());
                CanonicalError::internal(summary).create()
            }
            DomainError::Internal(e) => {
                tracing::error!(error = ?e, "Internal error in types_registry");
                CanonicalError::internal(e.to_string()).create()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The database-backed path (T7–T9)
// ---------------------------------------------------------------------------
//
// Every refusal below becomes an RFC-9457 problem through the canonical-error
// ladder — never a raw status tuple. One arm per `AcceptanceError` variant, so a
// new refusal reason cannot reach the wire as an opaque 500 by omission.
//
// # The wire detail is written here, not taken from `Display`
//
// Each wire `detail` is composed from the variant's own fields rather than from its
// `Display` — the pattern every other gear here follows, and what DE1302 enforces.
// `CanonicalError` is a terminal RFC-9457 document with no `source()` slot, so
// piping `Display` into it would fuse two messages with different audiences: the
// `#[error(...)]` text is for operators reading logs and names internals
// (`limits.batch_candidates`, `allow_compatibility_force`), while `detail` is a
// public API string. Fused, a reworded log line silently changes the wire contract.
// Infrastructure failures likewise log the cause and answer with a generic detail,
// because a rendered `DbErr` can carry a table name, a constraint name or SQL.
//
// **One arm renders, on purpose.** `PolicyRefused` calls `refusal.to_string()`:
// `PolicyRefusal` is not an error being converted but a value whose whole content
// is the region and the parameter an operator has to edit, and the
// `failed_precondition` constructor locks the wire `detail`, so that content
// survives only in the violation's description. It carries no internals to leak.
// The reasoning is repeated at that arm.

/// What the caller is told when the failure is ours.
const OPAQUE_INTERNAL: &str = "An internal error occurred. Please retry later.";

/// Log the cause where it happened, answer with [`OPAQUE_INTERNAL`].
///
/// A function rather than a `tracing::error!` inline in each arm: every macro
/// expansion adds branches, and three matches full of them trip
/// `clippy::cognitive_complexity` without making anything clearer. `at` names the
/// step, which is what an operator greps for.
fn opaque_internal(cause: &dyn std::fmt::Display, at: &'static str) -> CanonicalError {
    tracing::error!(error = %cause, at, "types_registry could not complete a request");
    CanonicalError::internal(OPAQUE_INTERNAL).create()
}

impl From<ServiceError> for CanonicalError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::Acceptance(inner) => inner.into(),
            ServiceError::Worker(inner) => inner.into(),
            // Storage and database failures are not the caller's fault and carry
            // nothing the caller can act on, so they stay opaque.
            ServiceError::Storage(inner) => opaque_internal(&inner, "storage read"),
            ServiceError::Db(inner) => opaque_internal(&inner, "database read"),
            // A stored document that will not parse is corruption, not input, so
            // the offending value goes to the operator log and not to the caller.
            ServiceError::CorruptDocument(detail) => {
                opaque_internal(&detail, "stored document parse")
            }
        }
    }
}

impl From<WorkerError> for CanonicalError {
    fn from(e: WorkerError) -> Self {
        match e {
            // A candidate refused on its merits never arrives here — that is an
            // `ItemFailure` recorded on the operation item. Everything else in
            // this type is an infrastructure failure, so all of it is opaque.
            WorkerError::OperationNotFound { operation_id } => {
                TypeRegistryError::not_found(format!("No operation with id: {operation_id}"))
                    .with_resource(operation_id.to_string())
                    .create()
            }
            WorkerError::MissingPayload { item_id } => opaque_internal(
                &format!("operation item {item_id} carries no request payload"),
                "admission",
            ),
            // Kept only for exhaustiveness: `run_operation` catches this one and
            // reports the outcome the winning pass recorded, so it does not reach a
            // handler. If it ever does, it is a worker bug and not a client's.
            WorkerError::ItemAlreadyTerminal { item_id } => opaque_internal(
                &format!("operation item {item_id} was terminalized by another pass"),
                "admission",
            ),
            WorkerError::StoreBuild(inner) => opaque_internal(&inner, "transient store build"),
            WorkerError::EvaluationTask(inner) => {
                opaque_internal(&inner, "blocking evaluation task")
            }
            // Deliberately not a `404` or `409`: a retryable condition dressed as a
            // client error invites the caller to "fix" a request that is correct.
            WorkerError::ConformingTypeAbsent { gts_id, type_id } => opaque_internal(
                &format!(
                    "instance '{gts_id}' conforms to '{type_id}', which has no current revision"
                ),
                "admission",
            ),
            // Corruption, not input: an entity row without the current-state row its
            // own admission transaction wrote. Nothing the caller can act on.
            WorkerError::CurrentStateMissing { gts_id, entity_id } => opaque_internal(
                &format!("entity '{gts_id}' (id {entity_id}) has no current-state row of its kind"),
                "admission",
            ),
            // Its own arm, not folded into the one above: the entity row is gone,
            // which points at a different table and a different cause.
            WorkerError::EntityVanished { gts_id, entity_id } => opaque_internal(
                &format!("entity '{gts_id}' (id {entity_id}) vanished mid-transaction"),
                "admission",
            ),
            // A retryable snapshot race, not a malformed candidate.
            WorkerError::DependencyTargetAbsent { gts_id } => opaque_internal(
                &format!("dependency target '{gts_id}' vanished before its edge was committed"),
                "admission",
            ),
            // Contention, not corruption: the request is correct and can be
            // repeated after the short hint advertised by the response.
            WorkerError::ResourceVersionExhausted { gts_id } => opaque_internal(
                &format!("entity '{gts_id}' cannot advance resource_version after i64::MAX"),
                "admission",
            ),
            WorkerError::RevisionNumberExhausted { gts_id } => opaque_internal(
                &format!("entity '{gts_id}' cannot allocate a revision after i32::MAX"),
                "admission",
            ),
            // Exhaustive only: `process_item` records this as a failed item.
            WorkerError::RefusedAfterWrite(failure) => {
                opaque_internal(&failure.to_string(), "admission")
            }
            // Exhaustive only: `process_item` retries or records revalidation exhaustion.
            WorkerError::RevalidationRequired(drift) => {
                opaque_internal(&drift.to_string(), "admission")
            }
            WorkerError::Storage(inner) => opaque_internal(&inner, "storage write"),
            WorkerError::Db(inner) => opaque_internal(&inner, "database write"),
        }
    }
}

/// The three field names this mapping keys violations by, beyond the two
/// `field::` constants the SDK already publishes.
mod violation_field {
    pub const IDEMPOTENCY_KEY: &str = "Idempotency-Key";
    pub const ITEMS: &str = "items";
    pub const KIND: &str = "kind";
    pub const DRY_RUN: &str = "dry_run";
    pub const FORCE: &str = "force";
    pub const EXPECTED_RESOURCE_VERSION: &str = "expected_resource_version";
}

/// One invalid-argument problem keyed by a field, with no resource attached.
fn invalid_field(field_name: &str, detail: String, code: &str) -> CanonicalError {
    TypeRegistryError::invalid_argument()
        .with_field_violation(field_name, detail, code)
        .create()
}

/// The `Idempotency-Key` header carries bytes that are not UTF-8.
///
/// Its own refusal rather than [`AcceptanceError::MissingIdempotencyKey`], because
/// the two are different mistakes and only the handler can tell them apart:
/// `validate` receives a `String`, so by the time it runs an unusable header and an
/// absent one look identical. Telling a caller that sent a key that a key is
/// *required* sends them looking for the wrong bug.
#[must_use]
pub fn idempotency_key_not_utf8() -> CanonicalError {
    invalid_field(
        violation_field::IDEMPOTENCY_KEY,
        "the Idempotency-Key header is not valid UTF-8".to_owned(),
        field::VALIDATION_FAILED,
    )
}

/// One invalid-argument problem naming the offending candidate as the resource.
fn invalid_candidate(gts_id: &str, field_name: &str, detail: String, code: &str) -> CanonicalError {
    TypeRegistryError::invalid_argument()
        .with_resource(gts_id.to_owned())
        .with_field_violation(field_name, detail, code)
        .create()
}

impl From<AcceptanceError> for CanonicalError {
    fn from(e: AcceptanceError) -> Self {
        use violation_field as vf;
        match &e {
            // --- the envelope -------------------------------------------------
            AcceptanceError::MissingIdempotencyKey => invalid_field(
                vf::IDEMPOTENCY_KEY,
                "an Idempotency-Key header is required".to_owned(),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::IdempotencyKeyTooLong { length } => invalid_field(
                vf::IDEMPOTENCY_KEY,
                format!(
                    "an Idempotency-Key may be at most {MAX_IDEMPOTENCY_KEY} characters; \
                     this one is {length}"
                ),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::EmptyBatch => invalid_field(
                vf::ITEMS,
                "a request must carry at least one entity".to_owned(),
                field::VALIDATION_FAILED,
            ),
            // The limit is a deployment setting, so the number is given rather
            // than the config key that holds it.
            AcceptanceError::BatchTooLarge { count, limit } => invalid_field(
                vf::ITEMS,
                format!("{count} entities exceeds the limit of {limit} per request"),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::UnsupportedOperationKind => invalid_field(
                vf::KIND,
                "only registration is accepted; deletion is not available yet".to_owned(),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::DryRunNotAccepted => invalid_field(
                vf::DRY_RUN,
                "dry_run is not available yet; omit it or set it to false".to_owned(),
                field::VALIDATION_FAILED,
            ),

            // --- the candidate identifier -------------------------------------
            AcceptanceError::InvalidIdentifier { gts_id, reason } => invalid_candidate(
                gts_id,
                field::GTS_ID_FIELD,
                format!("'{gts_id}' is not a canonical GTS identifier: {reason}"),
                field::INVALID_GTS_ID,
            ),
            AcceptanceError::DuplicateCandidate { gts_id } => invalid_candidate(
                gts_id,
                field::GTS_ID_FIELD,
                format!("'{gts_id}' appears twice in one request"),
                field::INVALID_GTS_ID,
            ),
            AcceptanceError::ExplicitUuidTail { gts_id } => invalid_candidate(
                gts_id,
                field::GTS_ID_FIELD,
                format!("'{gts_id}' carries an explicit UUID tail, which is not registrable"),
                field::INVALID_GTS_ID,
            ),
            AcceptanceError::InstanceVersionProfile { gts_id, reason } => invalid_candidate(
                gts_id,
                field::GTS_ID_FIELD,
                format!("registered Instance '{gts_id}' must name a stable version: {reason}"),
                field::INVALID_GTS_ID,
            ),

            // --- the candidate document ---------------------------------------
            AcceptanceError::MissingDialect { gts_id } => invalid_candidate(
                gts_id,
                field::ENTITY_FIELD,
                format!("'{gts_id}' declares no top-level $schema"),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::UnsupportedDialect { gts_id, found } => invalid_candidate(
                gts_id,
                field::ENTITY_FIELD,
                format!(
                    "'{gts_id}' declares dialect '{found}', which is not the Draft-07 \
                     spelling set"
                ),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::ConflictingDialect { gts_id, path } => invalid_candidate(
                gts_id,
                field::ENTITY_FIELD,
                format!("'{gts_id}' declares a differing $schema at '{path}'"),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::MissingContent { gts_id } => invalid_candidate(
                gts_id,
                field::ENTITY_FIELD,
                format!("'{gts_id}' carries no document, which a registration requires"),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::AuthoredDocumentTooLarge {
                gts_id,
                size,
                limit,
            } => invalid_candidate(
                gts_id,
                field::ENTITY_FIELD,
                format!("'{gts_id}' is {size} bytes, over the limit of {limit}"),
                field::VALIDATION_FAILED,
            ),

            // --- the candidate's flags ----------------------------------------
            // `allow_compatibility_force` is the operator's switch, so the caller
            // is told the outcome rather than the setting to go and change.
            AcceptanceError::ForceNotPermitted { gts_id } => invalid_candidate(
                gts_id,
                vf::FORCE,
                format!("force is not permitted on '{gts_id}' in this deployment"),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::ForceHasNothingToWaive { gts_id } => invalid_candidate(
                gts_id,
                vf::FORCE,
                format!("force on '{gts_id}' has no cross-minor compatibility check to waive"),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::ForceCompatibilityUnavailable { gts_id } => invalid_candidate(
                gts_id,
                vf::FORCE,
                format!(
                    "force on '{gts_id}' is not available until compatibility evaluation is enabled"
                ),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::MinorTypeSchemaRevision { gts_id } => invalid_candidate(
                gts_id,
                vf::EXPECTED_RESOURCE_VERSION,
                format!(
                    "minor-bearing Type Schema '{gts_id}' is immutable; register a new minor instead"
                ),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::ZeroPrecondition { gts_id } => invalid_candidate(
                gts_id,
                vf::EXPECTED_RESOURCE_VERSION,
                format!(
                    "expected_resource_version 0 on '{gts_id}' is refused: omit the field \
                     to require absence"
                ),
                field::VALIDATION_FAILED,
            ),
            AcceptanceError::NegativePrecondition { gts_id, version } => invalid_candidate(
                gts_id,
                vf::EXPECTED_RESOURCE_VERSION,
                format!("expected_resource_version {version} on '{gts_id}' is negative"),
                field::VALIDATION_FAILED,
            ),

            // --- policy -------------------------------------------------------
            // `failed_precondition`, not `permission_denied`. A closed region is a
            // statement about *what* may be registered, not about who is asking,
            // and the deployment's own configuration is the precondition that
            // failed. It is also the only shape that keeps the detail: the
            // `permission_denied` constructor locks the wire `detail`, so the
            // region and the parameter — the two things an operator has to edit —
            // would not survive the mapping.
            AcceptanceError::PolicyRefused(refusal) => TypeRegistryError::failed_precondition()
                .with_resource(refusal.0.gts_id.clone())
                .with_precondition_violation(
                    refusal
                        .0
                        .region
                        .clone()
                        .unwrap_or_else(|| "<default>".to_owned()),
                    refusal.to_string(),
                    format!("REGISTRATION_POLICY_{}", refusal.0.parameter.to_uppercase()),
                )
                .create(),

            // --- idempotency --------------------------------------------------
            // The operation id is given in both the detail and the resource: the
            // caller needs it to read what its key is already bound to.
            AcceptanceError::FingerprintConflict { operation_id } => {
                TypeRegistryError::already_exists(format!(
                    "this Idempotency-Key is already bound to operation {operation_id} \
                     with a different request"
                ))
                .with_resource(operation_id.to_string())
                .create()
            }

            // --- infrastructure ------------------------------------------------
            AcceptanceError::Dispatch(inner) => opaque_internal(inner, "operation dispatch"),
            AcceptanceError::Storage(inner) => opaque_internal(inner, "acceptance storage"),
            AcceptanceError::Db(inner) => opaque_internal(inner, "acceptance database"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admission::AdmissionFailureReason;
    use crate::domain::admission::acceptance::PolicyRefusalError;
    use crate::domain::admission::vector::{VectorDrift, VectorRole};
    use crate::domain::admission::worker::ItemFailure;
    use crate::domain::gts_store::StoreBuildError;
    use crate::domain::policy::PolicyRefusal;
    use toolkit_canonical_errors::Problem;
    use toolkit_db::DbError;
    use toolkit_db::secure::ScopeError;
    use toolkit_gts::{GTS_ID_PREFIX, gts_id};

    fn problem_from(err: DomainError) -> Problem {
        // Construct the wire `Problem` the same way the canonical error
        // middleware does — minus the post-response `instance` / `trace_id`
        // injection, which has no request context at the unit-test level.
        Problem::from(CanonicalError::from(err))
    }

    fn acceptance_problem(err: AcceptanceError) -> Problem {
        Problem::from(CanonicalError::from(err))
    }

    fn worker_problem(err: WorkerError) -> Problem {
        Problem::from(CanonicalError::from(err))
    }

    fn assert_field_violation(problem: &Problem, expected_field: &str, expected_reason: &str) {
        assert_eq!(problem.status, Some(400));
        let violation = problem
            .context
            .get("field_violations")
            .and_then(|value| value.get(0))
            .expect("an invalid argument must carry a field violation");
        assert_eq!(
            violation.get("field").and_then(serde_json::Value::as_str),
            Some(expected_field),
        );
        assert_eq!(
            violation.get("reason").and_then(serde_json::Value::as_str),
            Some(expected_reason),
        );
    }

    #[test]
    fn every_client_acceptance_refusal_has_the_expected_field_and_reason() {
        let id = gts_id!("cf.core.events.test.v1~").to_owned();
        let cases = vec![
            (
                AcceptanceError::MissingIdempotencyKey,
                violation_field::IDEMPOTENCY_KEY,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::IdempotencyKeyTooLong { length: 300 },
                violation_field::IDEMPOTENCY_KEY,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::EmptyBatch,
                violation_field::ITEMS,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::BatchTooLarge { count: 2, limit: 1 },
                violation_field::ITEMS,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::UnsupportedOperationKind,
                violation_field::KIND,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::DryRunNotAccepted,
                violation_field::DRY_RUN,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::InvalidIdentifier {
                    gts_id: id.clone(),
                    reason: "bad id".to_owned(),
                },
                field::GTS_ID_FIELD,
                field::INVALID_GTS_ID,
            ),
            (
                AcceptanceError::DuplicateCandidate { gts_id: id.clone() },
                field::GTS_ID_FIELD,
                field::INVALID_GTS_ID,
            ),
            (
                AcceptanceError::ExplicitUuidTail { gts_id: id.clone() },
                field::GTS_ID_FIELD,
                field::INVALID_GTS_ID,
            ),
            (
                AcceptanceError::InstanceVersionProfile {
                    gts_id: id.clone(),
                    reason: "unstable".to_owned(),
                },
                field::GTS_ID_FIELD,
                field::INVALID_GTS_ID,
            ),
            (
                AcceptanceError::MissingDialect { gts_id: id.clone() },
                field::ENTITY_FIELD,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::UnsupportedDialect {
                    gts_id: id.clone(),
                    found: "draft-next".to_owned(),
                },
                field::ENTITY_FIELD,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::ConflictingDialect {
                    gts_id: id.clone(),
                    path: "$.child".to_owned(),
                },
                field::ENTITY_FIELD,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::MissingContent { gts_id: id.clone() },
                field::ENTITY_FIELD,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::AuthoredDocumentTooLarge {
                    gts_id: id.clone(),
                    size: 2,
                    limit: 1,
                },
                field::ENTITY_FIELD,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::ForceNotPermitted { gts_id: id.clone() },
                violation_field::FORCE,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::ForceHasNothingToWaive { gts_id: id.clone() },
                violation_field::FORCE,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::ForceCompatibilityUnavailable { gts_id: id.clone() },
                violation_field::FORCE,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::MinorTypeSchemaRevision { gts_id: id.clone() },
                violation_field::EXPECTED_RESOURCE_VERSION,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::ZeroPrecondition { gts_id: id.clone() },
                violation_field::EXPECTED_RESOURCE_VERSION,
                field::VALIDATION_FAILED,
            ),
            (
                AcceptanceError::NegativePrecondition {
                    gts_id: id,
                    version: -1,
                },
                violation_field::EXPECTED_RESOURCE_VERSION,
                field::VALIDATION_FAILED,
            ),
        ];

        for (error, expected_field, expected_reason) in cases {
            assert_field_violation(&acceptance_problem(error), expected_field, expected_reason);
        }
    }

    #[test]
    fn policy_and_fingerprint_refusals_keep_their_distinct_problem_shapes() {
        let policy = acceptance_problem(AcceptanceError::PolicyRefused(PolicyRefusalError(
            PolicyRefusal {
                gts_id: "acme.crm.customer.type.v1~".to_owned(),
                parameter: "allowed_vendors",
                region: Some("acme.crm.*".to_owned()),
                detail: "vendor is closed".to_owned(),
            },
        )));
        assert_eq!(policy.status, Some(400));
        let violation = policy
            .context
            .get("violations")
            .and_then(|value| value.get(0))
            .expect("policy refusal must carry a precondition violation");
        assert_eq!(
            violation.get("type").and_then(serde_json::Value::as_str),
            Some("REGISTRATION_POLICY_ALLOWED_VENDORS"),
        );

        let operation_id = uuid::Uuid::nil();
        let conflict = acceptance_problem(AcceptanceError::FingerprintConflict { operation_id });
        assert_eq!(conflict.status, Some(409));
        assert!(conflict.detail.contains(&operation_id.to_string()));
    }

    #[test]
    fn acceptance_infrastructure_errors_are_opaque() {
        let cases = [
            acceptance_problem(AcceptanceError::Dispatch(anyhow::anyhow!(
                "dispatch-secret"
            ))),
            acceptance_problem(AcceptanceError::Storage(ScopeError::Invalid(
                "storage-secret",
            ))),
            acceptance_problem(AcceptanceError::Db(DbError::InvalidConfig(
                "database-secret".to_owned(),
            ))),
        ];
        for problem in cases {
            assert_eq!(problem.status, Some(500));
            assert_eq!(problem.detail, OPAQUE_INTERNAL);
            assert!(!problem.detail.contains("secret"));
        }
    }

    #[test]
    fn worker_variants_have_stable_status_and_opaque_internal_details() {
        let missing = worker_problem(WorkerError::OperationNotFound {
            operation_id: uuid::Uuid::nil(),
        });
        assert_eq!(missing.status, Some(404));

        let cases = [
            worker_problem(WorkerError::MissingPayload { item_id: 42 }),
            worker_problem(WorkerError::ItemAlreadyTerminal { item_id: 42 }),
            worker_problem(WorkerError::StoreBuild(StoreBuildError::Storage(
                ScopeError::Invalid("store-secret"),
            ))),
            worker_problem(WorkerError::ConformingTypeAbsent {
                gts_id: "instance-secret".to_owned(),
                type_id: "type-secret".to_owned(),
            }),
            worker_problem(WorkerError::CurrentStateMissing {
                gts_id: "state-secret".to_owned(),
                entity_id: 7,
            }),
            worker_problem(WorkerError::EntityVanished {
                gts_id: "entity-secret".to_owned(),
                entity_id: 7,
            }),
            worker_problem(WorkerError::RevisionNumberExhausted {
                gts_id: "revision-secret".to_owned(),
            }),
            worker_problem(WorkerError::DependencyTargetAbsent {
                gts_id: "dependency-secret".to_owned(),
            }),
            worker_problem(WorkerError::ResourceVersionExhausted {
                gts_id: "version-secret".to_owned(),
            }),
            // Cover the variants whose `Display` includes caller-visible content.
            worker_problem(WorkerError::RefusedAfterWrite(ItemFailure::new(
                AdmissionFailureReason::Unknown("reason-secret".to_owned()),
                "message-secret".to_owned(),
            ))),
            worker_problem(WorkerError::RevalidationRequired(VectorDrift::Moved {
                gts_id: "drift-secret".to_owned(),
                role: VectorRole::Dependent,
                recorded: 1,
                found: 2,
            })),
            worker_problem(WorkerError::Storage(ScopeError::Invalid("storage-secret"))),
            worker_problem(WorkerError::Db(DbError::InvalidConfig(
                "database-secret".to_owned(),
            ))),
        ];
        for problem in cases {
            assert_eq!(problem.status, Some(500));
            assert_eq!(problem.detail, OPAQUE_INTERNAL);
            assert!(!problem.detail.contains("secret"));
        }
    }

    #[tokio::test]
    async fn a_failed_blocking_task_is_also_opaque() {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let join_error = match task.await {
            Ok(()) => panic!("the task must be cancelled"),
            Err(error) => error,
        };
        let problem = worker_problem(WorkerError::EvaluationTask(join_error));
        assert_eq!(problem.status, Some(500));
        assert_eq!(problem.detail, OPAQUE_INTERNAL);
        assert!(!problem.detail.contains("cancelled"));
    }

    #[test]
    fn every_service_error_arm_delegates_or_stays_opaque() {
        assert_eq!(
            Problem::from(CanonicalError::from(ServiceError::Acceptance(
                AcceptanceError::MissingIdempotencyKey,
            )))
            .status,
            Some(400),
        );
        assert_eq!(
            Problem::from(CanonicalError::from(ServiceError::Worker(
                WorkerError::MissingPayload { item_id: 1 },
            )))
            .detail,
            OPAQUE_INTERNAL,
        );
        let opaque = [
            ServiceError::Storage(ScopeError::Invalid("storage-secret")),
            ServiceError::Db(DbError::InvalidConfig("database-secret".to_owned())),
            ServiceError::CorruptDocument("document-secret".to_owned()),
        ];
        for error in opaque {
            let problem = Problem::from(CanonicalError::from(error));
            assert_eq!(problem.status, Some(500));
            assert_eq!(problem.detail, OPAQUE_INTERNAL);
            assert!(!problem.detail.contains("secret"));
        }
    }

    #[test]
    fn test_domain_error_to_problem_not_found_by_id() {
        let problem = problem_from(DomainError::not_found_by_id(gts_id!(
            "cf.core.events.test.v1~"
        )));
        assert_eq!(problem.status, Some(404));
        // `instance` is filled by the canonical error middleware on the way
        // out — at the unit-test level no middleware is in scope.
        assert!(problem.instance.is_none());
        assert!(
            problem
                .detail
                .contains(&format!("GTS ID: {}", gts_id!("cf.core.events.test.v1~"))),
            "expected GTS-id-keyed detail, got {:?}",
            problem.detail,
        );
    }

    #[test]
    fn test_domain_error_to_problem_not_found_by_uuid() {
        let problem = problem_from(DomainError::not_found_by_uuid(uuid::Uuid::nil()));
        assert_eq!(problem.status, Some(404));
        assert!(
            problem
                .detail
                .contains("UUID: 00000000-0000-0000-0000-000000000000"),
            "expected UUID-keyed detail, got {:?}",
            problem.detail,
        );
    }

    #[test]
    fn test_domain_error_to_problem_already_exists() {
        let problem = problem_from(DomainError::already_exists(gts_id!(
            "cf.core.events.test.v1~"
        )));
        assert_eq!(problem.status, Some(409));
    }

    #[test]
    fn test_domain_error_to_problem_invalid_gts_id() {
        let problem = problem_from(DomainError::invalid_gts_id("bad format"));
        assert_eq!(problem.status, Some(400));
    }

    #[test]
    fn test_domain_error_to_problem_validation_failed() {
        let problem = problem_from(DomainError::validation_failed("schema invalid"));
        assert_eq!(problem.status, Some(400));
    }

    #[test]
    fn test_domain_error_to_problem_not_in_ready_mode() {
        let problem = problem_from(DomainError::NotInReadyMode);
        assert_eq!(problem.status, Some(503));
    }

    #[test]
    fn test_domain_error_to_problem_ready_commit_failed() {
        use crate::domain::error::ValidationError;
        let problem = problem_from(DomainError::ReadyCommitFailed(vec![
            ValidationError::new(format!("{GTS_ID_PREFIX}test1~"), "error1"),
            ValidationError::new(format!("{GTS_ID_PREFIX}test2~"), "error2"),
            ValidationError::new(format!("{GTS_ID_PREFIX}test3~"), "error3"),
        ]));
        // ReadyCommitFailed is only produced by post_init lifecycle and
        // never reaches a REST response; map opaquely to internal.
        assert_eq!(problem.status, Some(500));
    }

    #[test]
    fn test_domain_error_to_problem_internal() {
        let problem = problem_from(DomainError::Internal(anyhow::anyhow!("test error")));
        assert_eq!(problem.status, Some(500));
    }

    #[test]
    fn test_domain_error_to_problem_invalid_query() {
        let problem = problem_from(DomainError::invalid_query("bad pattern"));
        assert_eq!(problem.status, Some(400));
    }
}
