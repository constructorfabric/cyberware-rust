//! Admission failures shared by unit evaluation and worker orchestration.

use serde_json::json;
use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::AdmissionFailureReason;
use super::drift::VectorDrift;
use crate::domain::gts_store::StoreBuildError;

/// An infrastructure failure. Retryable by construction: nothing here is a
/// statement about the candidate.
///
/// `#[non_exhaustive]` because this enum is still growing: T13, T15, T17, T19 and
/// T20 each add a failure mode, and without the marker every one of them is a hard
/// break for a downstream `match` on a published crate.
#[domain_model]
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkerError {
    #[error("operation {operation_id} does not exist")]
    OperationNotFound { operation_id: Uuid },
    #[error("operation item {item_id} carries no request payload")]
    MissingPayload { item_id: i64 },
    /// Not a fault, and never reaches a caller: the worker catches it and reports
    /// the outcome the other pass recorded. It exists as an error because rolling
    /// the commit transaction back is the only way to *not* write an entity behind
    /// an item that is already terminal.
    #[error("operation item {item_id} was terminalized by another pass")]
    ItemAlreadyTerminal { item_id: i64 },
    #[error("building the transient store failed: {0}")]
    StoreBuild(#[source] StoreBuildError),
    #[error("the blocking evaluation task failed: {0}")]
    EvaluationTask(#[source] tokio::task::JoinError),
    /// An Instance's conforming Type Schema has no committed current revision.
    ///
    /// **Retryable, not terminal**: the value is not wrong, its type has not landed
    /// yet. A terminal failure would make the outcome depend on the order two
    /// unrelated submissions reached the worker; a redelivery re-reads and succeeds.
    /// Until T21 there is no outbox, so this condition surfaces inline as an
    /// opaque `500`; write contention likewise surfaces as a storage error.
    #[error("instance '{gts_id}' conforms to '{type_id}', which has no current revision")]
    ConformingTypeAbsent { gts_id: String, type_id: String },
    /// An entity row exists with no matching current-state row, or with one of the
    /// other kind. Structurally impossible — entity, revision and current row are
    /// written by one transaction (D3) — so this is a corrupt row rather than a
    /// race, and it is infrastructure rather than a statement about the candidate.
    #[error("entity '{gts_id}' (id {entity_id}) has no current-state row of its kind")]
    CurrentStateMissing { gts_id: String, entity_id: i64 },
    /// The **entity** row itself disappeared between two reads in one transaction.
    ///
    /// Distinct from [`Self::CurrentStateMissing`] because the two send an operator
    /// to different tables: this one says `entity` lost a row that nothing in the
    /// admission protocol deletes, and that one says the `type_schema` / `instance`
    /// projection is missing behind an entity that is still there.
    #[error("entity '{gts_id}' (id {entity_id}) vanished mid-transaction")]
    EntityVanished { gts_id: String, entity_id: i64 },
    /// A resolved edge target disappeared before commit.
    #[error("dependency target '{gts_id}' vanished before its edge was committed")]
    DependencyTargetAbsent { gts_id: String },
    /// The entity version is a monotonic persisted identity and cannot be
    /// advanced beyond the storage type's ceiling.
    #[error("entity '{gts_id}' cannot advance resource_version after i64::MAX")]
    ResourceVersionExhausted { gts_id: String },
    /// The revision counter is part of persisted identity and must never wrap or
    /// saturate onto the current revision number. The surrounding transaction
    /// rolls the already-executed resource-version CAS back on this error.
    #[error("entity '{gts_id}' cannot allocate a revision after i32::MAX")]
    RevisionNumberExhausted { gts_id: String },
    /// A candidate refusal discovered after the commit transaction began writing.
    #[error("the revision was refused after its writes began: {0}")]
    RefusedAfterWrite(ItemFailure),
    /// Commit-time revision-vector drift (D4, SPEC §8.1 step 4.3).
    #[error("the evaluation is stale and must be redone: {0}")]
    RevalidationRequired(VectorDrift),
    #[error("storage failure during admission: {0}")]
    Storage(#[from] ScopeError),
    #[error("database failure during admission: {0}")]
    Db(#[from] DbError),
}

/// A candidate-level failure: final, recorded, and never retried.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemFailure {
    /// A stable machine reason, preserving unknown codes read from storage.
    pub reason: AdmissionFailureReason,
    pub message: String,
}

impl std::fmt::Display for ItemFailure {
    /// Format as the operator-facing `reason: message` pair.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl ItemFailure {
    #[must_use]
    pub fn new(reason: AdmissionFailureReason, message: String) -> Self {
        Self { reason, message }
    }

    /// The stored `error_payload`: structured, so the reason survives the round
    /// trip as a field rather than as a substring.
    #[must_use]
    pub fn to_payload(&self) -> String {
        json!({ "reason": self.reason.as_str(), "message": self.message }).to_string()
    }

    /// The inverse of [`Self::to_payload`], for an outcome read back off the row.
    ///
    /// Without it a redelivery and a first pass report *different shapes of the same
    /// fact* — `{reason, message}` versus `reason: "recorded"` with the JSON stuffed
    /// into `message`. Invisible on the wire today, since REST reads `error_payload`
    /// from the row, but T16 counts refusals by `reason` and a metric reading
    /// `recorded` for every redelivered item counts nothing.
    ///
    /// A payload that does not parse is kept verbatim under a reason that says so,
    /// rather than being dropped or panicked on: a corrupt row should be visible.
    #[must_use]
    pub fn from_payload(payload: &str) -> Self {
        match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(value) => {
                let reason = value.get("reason").and_then(serde_json::Value::as_str);
                let message = value.get("message").and_then(serde_json::Value::as_str);
                match (reason, message) {
                    (Some(reason), Some(message)) => Self {
                        reason: AdmissionFailureReason::from_wire(reason),
                        message: message.to_owned(),
                    },
                    _ => Self::new(
                        AdmissionFailureReason::UnrecognizedPayload,
                        payload.to_owned(),
                    ),
                }
            }
            Err(_) => Self::new(
                AdmissionFailureReason::UnparsablePayload,
                payload.to_owned(),
            ),
        }
    }
}
