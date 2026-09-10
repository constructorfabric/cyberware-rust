//! The admission worker: a plain function of `(operation_id, database)`.
//!
//! **Not a task.** SPEC §8.1 puts it this way because §13's testing rules forbid a
//! test that polls: an entry point that returns a result makes every concurrency
//! case reachable in a plain `#[tokio::test]`. There is no `sleep`, no timer and no
//! channel anywhere in this module. T21's outbox handler is a thin shell that calls
//! [`run_operation`] and maps its return to `Ok` / `Retry` / `Reject`.
//!
//! # The error boundary is the retry boundary
//!
//! [`WorkerError`] is for **infrastructure** failures — a dropped connection, a
//! deadlock, a store that could not be built from committed rows. Those are worth
//! retrying, and the outbox will.
//!
//! A candidate that is simply *wrong* — an unresolvable reference, a schema that
//! fails its meta-schema, an identifier that already exists — is an
//! [`ItemFailure`]: an **outcome** on the operation item, not a fault of the
//! worker. Retrying it would burn the outbox's attempt budget on a decision that is
//! already final. So the two travel in different positions: `Err(WorkerError)`
//! versus `Ok(_)` with a failed item.
//!
//! # Current admission scope (through T16)
//!
//! Each item is its own unit, processed in `item_no` order. References resolve
//! against committed dependencies plus this candidate; `gts-rust` validation
//! rejects circular `$ref`s. T19 adds the batch-wide candidate overlay,
//! topological ordering and cycle detection over combined `$ref`/derivation edges.
//! Creations and content revisions both land here — the item's stored precondition
//! chooses which commit runs.

use std::sync::Arc;
use std::time::Instant;

use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, ScopeError};
use toolkit_db::{DBProvider, DbError};
use toolkit_macros::domain_model;
use tracing::{Instrument, Span};
use uuid::Uuid;

pub use super::errors::{ItemFailure, WorkerError};
use super::revision::{CommittedUnit, RevisionCommit};
use super::unchanged;
use super::unit::{PreparedUnit, commit_creation, commit_revision, evaluate};
use super::vector::VectorDrift;
use crate::config::{Limits, WorkerSettings};
use crate::domain::admission::AdmissionFailureReason;
use crate::domain::admission::Precondition;
use crate::domain::enums::{OperationItemStatus, OperationStatus};
use crate::domain::ports::metrics::{AdmissionMetrics, RefusalStage, TerminalStatus};
use crate::domain::ports::{OperationItemRow, OperationRow, Stores, commit_write, snapshot_read};
use crate::observability;

/// The two configuration sections one admission pass obeys, carried together.
#[derive(Clone, Copy)]
pub struct Tuning<'a> {
    pub limits: &'a Limits,
    pub worker: &'a WorkerSettings,
    pub metrics: &'a Arc<dyn AdmissionMetrics>,
}

/// What one pass over an operation produced.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationOutcome {
    pub operation_id: Uuid,
    /// `true` when this pass found the operation already terminal and did nothing.
    /// A redelivered outbox message lands here.
    pub already_terminal: bool,
    pub items: Vec<ItemOutcome>,
}

/// One candidate's outcome.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemOutcome {
    pub gts_id: String,
    pub status: OperationItemStatus,
    /// The Registry Reference of the admitted entity, on success.
    pub gts_uuid: Option<Uuid>,
    pub resource_version: Option<i64>,
    pub revision_no: Option<i32>,
    pub failure: Option<ItemFailure>,
}

/// Perform one full admission pass over an operation.
///
/// Each invocation rebuilds its transient store and re-reads the database.
///
/// # Errors
/// [`WorkerError`] for an infrastructure failure. A candidate-level refusal is
/// recorded on its item and reported in [`OperationOutcome`], not returned here.
pub async fn run_operation(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    tuning: Tuning<'_>,
    operation_id: Uuid,
    now: OffsetDateTime,
) -> Result<OperationOutcome, WorkerError> {
    // Open before the first read; populate operation fields after loading it.
    let span = observability::operation_span(operation_id);
    let started = Instant::now();
    let outcome = run_operation_inner(stores, db, scope, tuning, operation_id, now)
        .instrument(span)
        .await;
    // Include failed passes in the duration histogram.
    tuning.metrics.observe_operation_duration(started.elapsed());
    outcome
}

/// [`run_operation`]'s body, running inside the operation span.
async fn run_operation_inner(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    tuning: Tuning<'_>,
    operation_id: Uuid,
    now: OffsetDateTime,
) -> Result<OperationOutcome, WorkerError> {
    // Step 1: the operation and its items under one snapshot. `mark_running` below
    // touches only the operation row, so reading the items before it rather than
    // after changes nothing — and it makes the pair consistent, which two
    // separately-snapshotted reads would not be.
    let (operation, items) = read_operation(stores, db, scope, operation_id).await?;
    observability::record_operation_facts(&Span::current(), operation.kind, operation.dry_run);

    // A redelivered message finds the operation terminal and reports the stored
    // outcomes. Delivery is at-least-once (T21), so this is the shape that makes
    // duplicate delivery a no-op rather than a second admission.
    if operation.status == OperationStatus::Completed {
        return Ok(already_terminal(operation_id, &items));
    }

    if !mark_running(stores, db, scope, operation_id, now).await? {
        tracing::warn!(
            %operation_id,
            "types_registry operation was already running; continuing with CAS-protected items"
        );
    }

    let mut outcomes = Vec::with_capacity(items.len());
    for item in items {
        // Instrument each item without splitting `process_item` to own the span.
        let span =
            observability::unit_span(operation_id, &item.gts_id, item.kind, item.dry_run, item.id);
        outcomes.push(
            process_item(stores, db, scope, tuning, operation_id, &item, now)
                .instrument(span)
                .await?,
        );
    }

    mark_completed(stores, db, scope, operation_id, now).await?;

    Ok(OperationOutcome {
        operation_id,
        already_terminal: false,
        items: outcomes,
    })
}

/// The `DbErr` inside a [`WorkerError`], for the transaction retry helper.
///
/// Only the two arms that actually wrap one. Everything else — a store that would
/// not build, an item another pass terminalized — is `None`, which short-circuits
/// the retry loop: those answers do not change on a second attempt.
///
/// The `sea_orm` type in the signature is `Db::transaction_with_retry`'s contract,
/// not a persistence choice this layer is making: the helper classifies contention
/// per backend and needs the driver error to do it.
#[allow(unknown_lints)]
#[allow(de0301_no_infra_in_domain)]
const fn retryable_db_err(e: &WorkerError) -> Option<&sea_orm::DbErr> {
    match e {
        WorkerError::Storage(ScopeError::Db(inner)) | WorkerError::Db(DbError::Sea(inner)) => {
            Some(inner)
        }
        _ => None,
    }
}

async fn prepare(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    item: &OperationItemRow,
    payload: &str,
    tuning: Tuning<'_>,
) -> Result<Result<PreparedUnit, ItemFailure>, WorkerError> {
    let prepared = evaluate(
        stores,
        db,
        scope,
        &item.gts_id,
        payload,
        item.id,
        tuning.limits,
        Some(item),
    )
    .await?;
    let hit = matches!(&prepared, Ok(PreparedUnit::Unchanged(_)));
    tuning.metrics.unchanged_probe(hit);
    if hit {
        tracing::debug!(operation_item_id = item.id, gts_id = %item.gts_id, "types_registry unchanged probe hit");
    }
    Ok(prepared)
}

/// Groups the per-item commit inputs so the transaction boundary stays readable
/// without crossing Clippy's argument-count threshold.
struct CommitRequest<'a> {
    prepared: &'a PreparedUnit,
    item: &'a OperationItemRow,
    now: OffsetDateTime,
    limits: Limits,
    metrics: &'a Arc<dyn AdmissionMetrics>,
}

/// Run the serialized commit transaction (SPEC step 4b).
///
/// Its first statement claims `entity_write_order`, replacing the former family locks.
async fn commit_prepared(
    db: &DBProvider<WorkerError>,
    stores: &Arc<dyn Stores>,
    scope: &AccessScope,
    request: CommitRequest<'_>,
) -> Result<Result<RevisionCommit, ItemFailure>, WorkerError> {
    let CommitRequest {
        prepared,
        item,
        now,
        limits,
        metrics,
    } = request;
    let precondition = item.precondition;
    // A short READ COMMITTED transaction containing only rechecks and
    // writes. The `Arc` keeps transaction retries from cloning the artifacts.
    //
    // Retried on lock contention: every statement in both commit paths re-reads
    // inside the transaction, so an attempt that rolled back leaves nothing to undo.
    // Without the retry, a deadlock on the entity compare-and-swap propagates out of
    // `process_item` before `mark_completed`, stranding the operation row in
    // `running` with its items `pending` and nothing to re-drive it.
    //
    // The item's stored precondition — never the candidate's shape and never a
    // caller-declared kind — chooses the commit. Acceptance skips the policy gate
    // for a revision (SPEC §8.1 step 3), so the claim "this is a revision" has to be
    // *enforced* here, by a commit that refuses an absent identifier.
    let tx_scope = scope.clone();
    let tx_stores = Arc::clone(stores);
    // The `'static` retry closure owns each attempt's handles.
    let tx_metrics = Arc::clone(metrics);
    // Copy limits into the `'static` retry closure.
    let tx_limits = limits;
    db.db()
        .transaction_with_retry(commit_write(&db.db()), retryable_db_err, |tx| {
            let prepared = prepared.clone();
            let tx_scope = tx_scope.clone();
            let tx_stores = Arc::clone(&tx_stores);
            let tx_metrics = Arc::clone(&tx_metrics);
            Box::pin(async move {
                let unit = match &prepared {
                    PreparedUnit::Unchanged(candidate) => {
                        return unchanged::commit(
                            tx_stores.as_ref(),
                            tx,
                            &tx_scope,
                            candidate,
                            now,
                        )
                        .await;
                    }
                    PreparedUnit::Evaluated(unit) => unit,
                };
                match precondition {
                    Precondition::MustNotExist => {
                        commit_creation(tx_stores.as_ref(), tx, &tx_scope, unit, &tx_limits, now)
                            .await
                            .map(|r| r.map(RevisionCommit::Admitted))
                    }
                    Precondition::Version(expected) => {
                        commit_revision(
                            tx_stores.as_ref(),
                            tx,
                            &tx_scope,
                            unit,
                            expected,
                            &tx_limits,
                            now,
                            &tx_metrics,
                        )
                        .await
                    }
                }
            })
        })
        .await
}

/// Evaluate and commit one non-terminal item.
/// Revision-vector drift triggers a fresh evaluation up to the configured attempt limit.
async fn process_item(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    tuning: Tuning<'_>,
    operation_id: Uuid,
    item: &OperationItemRow,
    now: OffsetDateTime,
) -> Result<ItemOutcome, WorkerError> {
    if item.status != OperationItemStatus::Pending && item.status != OperationItemStatus::Running {
        return Ok(stored_outcome(item));
    }

    let payload = item
        .request_payload
        .as_deref()
        .ok_or(WorkerError::MissingPayload { item_id: item.id })?;

    let attempts = tuning.worker.max_revalidation_attempts;
    let mut last_drift: Option<VectorDrift> = None;
    // Probe once in the initial evaluation snapshot. A miss stays on ordinary
    // evaluation even if a concurrent write makes the authored content identical.
    let mut initial = if attempts > 0 {
        Some(prepare(stores, db, scope, item, payload, tuning).await?)
    } else {
        None
    };
    // Log attempts using one-based numbering.
    for attempt in 1..=attempts {
        let prepared = match initial.take() {
            Some(prepared) => prepared,
            None => {
                evaluate(
                    stores,
                    db,
                    scope,
                    &item.gts_id,
                    payload,
                    item.id,
                    tuning.limits,
                    None,
                )
                .await?
            }
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(failure) => {
                return record_failure(
                    stores,
                    db,
                    scope,
                    operation_id,
                    item,
                    failure,
                    now,
                    tuning.metrics,
                )
                .await;
            }
        };

        let committed = match commit_prepared(
            db,
            stores,
            scope,
            CommitRequest {
                prepared: &prepared,
                item,
                now,
                limits: *tuning.limits,
                metrics: tuning.metrics,
            },
        )
        .await
        {
            Ok(committed) => committed,
            // Another pass terminalized the item; this pass rolled back.
            Err(WorkerError::ItemAlreadyTerminal { item_id }) => {
                return stored_item(stores, db, scope, operation_id, item_id).await;
            }
            // Terminalize a post-write refusal after its transaction rolls back.
            Err(WorkerError::RefusedAfterWrite(failure)) => {
                return record_failure(
                    stores,
                    db,
                    scope,
                    operation_id,
                    item,
                    failure,
                    now,
                    tuning.metrics,
                )
                .await;
            }
            // Guard or artifact CAS drift rolls the transaction back.
            Err(WorkerError::RevalidationRequired(drift)) => {
                tuning.metrics.revalidation_retried(&drift);
                tracing::info!(
                    %operation_id,
                    operation_item_id = item.id,
                    gts_id = %item.gts_id,
                    attempt,
                    max_attempts = attempts,
                    drift = %drift,
                    "types_registry revalidating a candidate whose evaluation went stale"
                );
                last_drift = Some(drift);
                continue;
            }
            Err(error) => return Err(error),
        };

        return match committed {
            Ok(commit) => Ok(committed_outcome(
                operation_id,
                item,
                commit,
                attempt,
                tuning.metrics,
            )),
            Err(failure) => {
                record_failure(
                    stores,
                    db,
                    scope,
                    operation_id,
                    item,
                    failure,
                    now,
                    tuning.metrics,
                )
                .await
            }
        };
    }

    // Every attempt drifted.
    let drift = last_drift.map_or_else(
        || "no attempt was made".to_owned(),
        |drift| drift.to_string(),
    );
    let failure = ItemFailure::new(
        AdmissionFailureReason::RevalidationExhausted,
        format!(
            "the state this candidate was validated against kept moving: {attempts} \
             revalidation attempts were exhausted, the last on {drift}"
        ),
    );
    record_failure(
        stores,
        db,
        scope,
        operation_id,
        item,
        failure,
        now,
        tuning.metrics,
    )
    .await
}

/// Report, log, and count a successful commit.
fn committed_outcome(
    operation_id: Uuid,
    item: &OperationItemRow,
    commit: RevisionCommit,
    attempt: u32,
    metrics: &Arc<dyn AdmissionMetrics>,
) -> ItemOutcome {
    match commit {
        RevisionCommit::Admitted(CommittedUnit {
            gts_uuid,
            revision_no,
            resource_version,
        }) => {
            tracing::info!(
                %operation_id,
                operation_item_id = item.id,
                gts_id = %item.gts_id,
                revision_no,
                resource_version,
                attempt,
                "types_registry candidate admitted"
            );
            metrics.candidate_terminalized(TerminalStatus::Succeeded);
            ItemOutcome {
                gts_id: item.gts_id.clone(),
                status: OperationItemStatus::Succeeded,
                gts_uuid: Some(gts_uuid),
                resource_version: Some(resource_version),
                revision_no: Some(revision_no),
                failure: None,
            }
        }
        // Terminal and successful, and deliberately not `Succeeded`: no revision
        // number was allocated, so reporting one would name a revision that does
        // not exist (ADR-0005).
        RevisionCommit::Unchanged {
            gts_uuid,
            resource_version,
        } => {
            tracing::info!(
                %operation_id,
                operation_item_id = item.id,
                gts_id = %item.gts_id,
                resource_version,
                attempt,
                "types_registry candidate content already current"
            );
            metrics.candidate_terminalized(TerminalStatus::Unchanged);
            ItemOutcome {
                gts_id: item.gts_id.clone(),
                status: OperationItemStatus::Unchanged,
                gts_uuid: Some(gts_uuid),
                resource_version: Some(resource_version),
                revision_no: None,
                failure: None,
            }
        }
    }
}

/// The `reason` label a refusal counts under.
#[must_use]
pub fn reason_label(reason: &AdmissionFailureReason) -> &'static str {
    reason.metric_label()
}

/// Record a candidate-level failure and return the outcome to report for it.
///
/// Its own statement rather than part of the commit transaction: the commit rolled
/// back, and the outcome must survive that.
///
/// The write is a CAS on the item's status. `false` means an overlapping pass
/// terminalized the item first — its outcome stands, so the stored row is re-read
/// and reported instead of the failure this pass computed. For a deterministic
/// refusal the two agree; where they do not, the store is right and this pass is
/// the duplicate.
#[allow(clippy::too_many_arguments)]
async fn record_failure(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    operation_id: Uuid,
    item: &OperationItemRow,
    failure: ItemFailure,
    now: OffsetDateTime,
    metrics: &Arc<dyn AdmissionMetrics>,
) -> Result<ItemOutcome, WorkerError> {
    let tx_stores = Arc::clone(stores);
    let tx_scope = scope.clone();
    let payload = failure.to_payload();
    let item_id = item.id;
    let recorded = db
        .transaction(move |tx| {
            Box::pin(async move {
                let recorded = tx_stores
                    .mark_item_failed(tx, &tx_scope, item_id, payload, now)
                    .await?;
                Ok(recorded)
            })
        })
        .await?;

    if recorded {
        // Count only the pass that won the item CAS.
        metrics.candidate_terminalized(TerminalStatus::Failed);
        metrics.refused(RefusalStage::Admission, reason_label(&failure.reason));
        tracing::warn!(
            %operation_id,
            operation_item_id = item.id,
            gts_id = %item.gts_id,
            reason = %failure.reason,
            "types_registry candidate refused"
        );
        return Ok(ItemOutcome {
            gts_id: item.gts_id.clone(),
            status: OperationItemStatus::Failed,
            gts_uuid: None,
            resource_version: None,
            revision_no: None,
            failure: Some(failure),
        });
    }
    stored_item(stores, db, scope, operation_id, item_id).await
}

/// The outcome a redelivered pass reports: every stored item, nothing written.
fn already_terminal(operation_id: Uuid, items: &[OperationItemRow]) -> OperationOutcome {
    tracing::debug!(
        %operation_id,
        "types_registry operation was already terminal; the redelivered pass reports \
         the stored outcomes"
    );
    OperationOutcome {
        operation_id,
        already_terminal: true,
        items: items.iter().map(stored_outcome).collect(),
    }
}

/// The outcome the store holds for one item, re-read outside any transaction this
/// pass opened. Reached only when an overlapping pass won a CAS.
async fn stored_item(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    operation_id: Uuid,
    item_id: i64,
) -> Result<ItemOutcome, WorkerError> {
    let (_, fresh) = read_operation(stores, db, scope, operation_id).await?;
    fresh
        .iter()
        .find(|row| row.id == item_id)
        .map(stored_outcome)
        .ok_or(WorkerError::OperationNotFound { operation_id })
}

/// Read the operation and its items under one snapshot.
///
/// # Errors
/// [`WorkerError::OperationNotFound`] when the id names no row — an unknown
/// operation is an infrastructure fault, not a candidate outcome.
async fn read_operation(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    operation_id: Uuid,
) -> Result<(OperationRow, Vec<OperationItemRow>), WorkerError> {
    let stores_tx = Arc::clone(stores);
    let scope_tx = scope.clone();
    let found = db
        .transaction_with_config(snapshot_read(&db.db()), move |tx| {
            Box::pin(async move {
                let Some(operation) = stores_tx.find_by_id(tx, &scope_tx, operation_id).await?
                else {
                    return Ok(None);
                };
                let items = stores_tx.find_items(tx, &scope_tx, operation_id).await?;
                Ok(Some((operation, items)))
            })
        })
        .await?;
    found.ok_or(WorkerError::OperationNotFound { operation_id })
}

/// Move the operation to `running`.
///
/// The CAS result is deliberately discarded, and that is now a choice rather than a
/// gap. `false` means the operation is already `running` — either another pass owns
/// it, or an earlier pass died mid-flight. P0 has no lease to tell those apart
/// (`worker.operation_timeout` is unread until T21), and treating `false` as
/// "someone else owns it" would strand every operation whose pass died: there is no
/// outbox to redeliver it, so the retry that arrives under the same
/// `Idempotency-Key` is the only driver there is. Proceeding is therefore the
/// recovering behaviour, and overlap is made **safe** instead of prevented: both
/// item writes are CAS on the item's status, and `commit_creation` rolls its
/// transaction back when it loses (`WorkerError::ItemAlreadyTerminal`). The cost is
/// duplicated evaluation work, never a wrong outcome.
///
/// TODO(T21): with the outbox and a lease built on `worker.operation_timeout`,
/// honour `false` for an operation whose lease is live and re-take one whose lease
/// has expired — which removes the duplicated work as well.
async fn mark_running(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    operation_id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, WorkerError> {
    let stores = Arc::clone(stores);
    let scope = scope.clone();
    db.transaction(move |tx| {
        Box::pin(async move {
            stores
                .mark_running(tx, &scope, operation_id, now)
                .await
                .map_err(WorkerError::from)
        })
    })
    .await
}

/// Move the operation to `completed`.
async fn mark_completed(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    operation_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), WorkerError> {
    let stores = Arc::clone(stores);
    let scope = scope.clone();
    db.transaction(move |tx| {
        Box::pin(async move {
            stores.mark_completed(tx, &scope, operation_id, now).await?;
            Ok(())
        })
    })
    .await
}

/// Read an already-terminal item's stored outcome back out.
fn stored_outcome(item: &OperationItemRow) -> ItemOutcome {
    ItemOutcome {
        gts_id: item.gts_id.clone(),
        status: item.status,
        // `gts_uuid` is not stored on the item — it derives from `gts_id`
        // (`database.sql`), and deriving it here would duplicate a GTS rule. The
        // caller that needs it has the identifier.
        gts_uuid: None,
        resource_version: item.result_resource_version,
        revision_no: item.result_revision_no,
        failure: item.error_payload.as_deref().map(ItemFailure::from_payload),
    }
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod worker_tests;
