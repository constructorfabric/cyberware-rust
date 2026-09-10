//! One admission unit: evaluate a candidate against its transient store, then
//! commit it (SPEC §8.1, worker steps 3 and 4).
//!
//! Evaluation — building the store, resolving references, composing effective
//! traits, meta-compiling the schema — runs with **no transaction open**, so a slow
//! validation never holds a row lock and a failed one never opened one. The
//! transaction that follows holds only the commit-time rechecks and the writes.
//!
//! P0 scope is one acyclic, reference-free candidate per unit; later phases add
//! steps to `evaluate` or `commit` without moving that boundary.
//!
//! [`commit_creation`] requires the identifier **absent**, [`commit_revision`]
//! requires it present at a named `resource_version`. They share evaluation and
//! nothing else — one function branching on an `Option<i64>` would make each half's
//! writes reachable under the other's precondition.

use std::sync::Arc;

use gts::{GTS_IMPLEMENTATION_VERSION, GTS_SPECIFICATION_VERSION, GtsId};
use serde_json::Value;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbTx};
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::bounds::{check_closure, materialize_bounded};
use super::errors::{ItemFailure, WorkerError};
use super::fingerprint::canonical_text;
use super::refresh::refresh_dependents;
use super::revision::{
    CommittedUnit, CurrentContent, RevisionCommit, read_current_content, revision_entity,
    stale_precondition, terminalize_unchanged,
};
use super::unchanged::{self, UnchangedCandidate};
use super::vector::{self, RevisionVector, VectorDrift};
use crate::config::Limits;
use crate::domain::admission::AdmissionFailureReason;
use crate::domain::artifacts::{MaterializedArtifacts, content_hash};
use crate::domain::dependency::{DependencyEdge, extract_edges};
use crate::domain::enums::{DependencyKind, EntityKind, OwnershipScope};
use crate::domain::family::{FamilyKey, admits_new_member, family_key};
use crate::domain::gts_store::{UnitDocument, UnitStore, load_unit_store};
use crate::domain::ports::metrics::AdmissionMetrics;
use crate::domain::ports::{
    NewCurrentInstance, NewCurrentTypeSchema, NewEntity, NewInstanceRevision, NewRevision,
    OperationItemRow, Stores, snapshot_read,
};

/// The owning gear recorded on a P0 admission.
///
/// ponytail: ceiling C3 — caller-declared attribution that MUST NOT authorize
/// (`database.sql`). Honest while P0 has one writer, the registry seeding itself.
/// Upgrade: the inventory record's own `owning_gear`.
pub const P0_OWNING_GEAR: &str = "types-registry";

/// The kind-specific half of an evaluation. The kind *is* the variant, so
/// `EvaluatedUnit` needs no `entity_kind` field and no payload can disagree with
/// one.
#[domain_model]
#[derive(Clone, Debug)]
pub enum EvaluatedOutcome {
    /// D3's artifacts, materialized at admission so the read path recomputes
    /// nothing.
    TypeSchema { artifacts: MaterializedArtifacts },
    /// The Type Schema revision this value was validated against. Recorded rather
    /// than re-derived: the schema's current revision may move afterwards, and this
    /// is the record of which rules the value passed.
    Instance {
        type_schema_entity_id: i64,
        type_schema_revision_no: i32,
    },
}

impl EvaluatedOutcome {
    /// Derived, never passed: the identifier's `~` chose the variant. Supplying the
    /// kind alongside it is how an entity row and its revision table come to disagree.
    #[must_use]
    pub const fn entity_kind(&self) -> EntityKind {
        match self {
            Self::TypeSchema { .. } => EntityKind::TypeSchema,
            Self::Instance { .. } => EntityKind::Instance,
        }
    }
}

/// What evaluation produced, and what the commit needs. Owned, because it crosses
/// into a transaction closure that borrows nothing shorter-lived than `'static`.
#[domain_model]
#[derive(Clone, Debug)]
pub struct EvaluatedUnit {
    pub gts_id: String,
    pub gts_uuid: Uuid,
    pub family_key: FamilyKey,
    pub canonical_body: String,
    pub content_hash: Vec<u8>,
    pub outcome: EvaluatedOutcome,
    pub operation_item_id: i64,
    /// The candidate's outgoing edges, by target **identifier** (T13).
    pub edges: Vec<DependencyEdge>,
    /// The database state on which this evaluation's verdict rests.
    pub vector: RevisionVector,
}

/// Claim the write order as the first statement of every commit transaction.
///
/// The database owns the wait timeout; cancelling a client-side timeout would not
/// cancel the statement. See SPEC §4.
async fn claim_entity_write_order(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    now: OffsetDateTime,
) -> Result<(), WorkerError> {
    Ok(stores.claim_entity_write_order(tx, scope, now).await?)
}

/// The snapshot either proves equality or supplies everything evaluation needs.
#[domain_model]
enum EvaluationSnapshot {
    Unchanged(UnchangedCandidate),
    Loaded {
        store: Box<UnitStore>,
        schema_pair: Option<(i64, i32)>,
        vector: RevisionVector,
        edges: Vec<DependencyEdge>,
    },
}

/// A revision may prove equality without evaluating any effective content.
#[domain_model]
#[derive(Clone, Debug)]
pub enum PreparedUnit {
    Evaluated(Arc<EvaluatedUnit>),
    Unchanged(Arc<UnchangedCandidate>),
}

/// Probe once when requested, then evaluate a miss from the same snapshot.
/// Validation and artifact materialization run after the snapshot closes.
///
/// Builds the unit's transient store from the database (D2), asks `gts-rust` to
/// validate the candidate, and materializes D3's artifacts. The store is dropped
/// when this returns: nothing is retained anywhere, and the next invocation reads
/// the database again.
///
/// Resolution budgets apply before commit; `activation_write_set` also bounds reverse impact.
///
/// # Errors
/// [`WorkerError`] for an infrastructure failure, which the outbox handler must
/// retry. A content failure is an [`ItemFailure`] in the `Ok(Err(..))` position: an
/// *outcome*, not a fault, and retrying it would answer the same forever.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate(
    stores: &Arc<dyn Stores>,
    db: &DBProvider<WorkerError>,
    scope: &AccessScope,
    gts_id: &str,
    canonical_body: &str,
    operation_item_id: i64,
    limits: &Limits,
    probe_item: Option<&OperationItemRow>,
) -> Result<Result<PreparedUnit, ItemFailure>, WorkerError> {
    let limits = *limits;
    let id = match GtsId::try_new(gts_id) {
        Ok(id) => id,
        // Acceptance already refused a non-canonical identifier, so reaching here
        // means the stored row disagrees with the rules that admitted it.
        Err(e) => {
            return Ok(Err(ItemFailure::new(
                AdmissionFailureReason::InvalidIdentifier,
                format!("stored identifier '{gts_id}' does not parse: {e}"),
            )));
        }
    };
    // The conforming type's `(entity_id, revision_no)` is read in the same snapshot as
    // the store: the recorded revision must be the one that validated the value.
    let conforming_type = (!id.is_type()).then(|| id.get_type_id()).flatten();
    let candidate_id = id.id().to_owned();
    let snapshot = {
        let stores = Arc::clone(stores);
        let scope = scope.clone();
        let conforming_type = conforming_type.clone();
        let probe_item = probe_item.cloned();
        let canonical_body = canonical_body.to_owned();
        let id = id.clone();
        db.transaction_with_config(snapshot_read(&db.db()), move |tx| {
            Box::pin(async move {
                if let Some(item) = probe_item
                    && let Some(candidate) =
                        unchanged::probe(stores.as_ref(), tx, &scope, &item, &canonical_body)
                            .await?
                {
                    return Ok(Ok(EvaluationSnapshot::Unchanged(candidate)));
                }
                let content: Value = match serde_json::from_str(&canonical_body) {
                    Ok(content) => content,
                    Err(e) => {
                        return Ok(Err(ItemFailure::new(
                            AdmissionFailureReason::InvalidDocument,
                            format!("stored request payload is not valid JSON: {e}"),
                        )));
                    }
                };

                // Extract only after a probe miss, before loading the dependency store.
                let edges = match extract_edges(&id, &content) {
                    Ok(edges) => edges,
                    Err(e) => {
                        return Ok(Err(ItemFailure::new(
                            AdmissionFailureReason::InvalidSchema,
                            e.to_string(),
                        )));
                    }
                };

                let candidates = vec![UnitDocument {
                    gts_id: id.id().to_owned(),
                    content,
                }];
                let store = load_unit_store(stores.as_ref(), tx, &scope, candidates)
                    .await
                    .map_err(WorkerError::StoreBuild)?;
                let pair = match conforming_type {
                    Some(type_id) => {
                        let entity = stores.find_by_gts_id(tx, &scope, &type_id).await?;
                        match entity {
                            Some(row) => stores
                                .current_schema_projections(tx, &scope, &[row.id])
                                .await?
                                .into_iter()
                                .find(|current| current.entity_id == row.id)
                                .map(|current| (row.id, current.cas.revision_no)),
                            None => None,
                        }
                    }
                    None => None,
                };
                // Derive the vector from the same snapshot as the validated documents (D4).
                let vector = vector::derive_from(
                    stores.as_ref(),
                    tx,
                    &scope,
                    &candidate_id,
                    store.roots(),
                    store.closure_entities(),
                    limits.activation_write_set,
                )
                .await?;
                let vector = match vector {
                    Ok(vector) => vector,
                    Err(failure) => return Ok(Err(failure)),
                };
                Ok(Ok(EvaluationSnapshot::Loaded {
                    store: Box::new(store),
                    schema_pair: pair,
                    vector,
                    edges,
                }))
            })
        })
        .await?
    };
    let (store, schema_pair, vector, edges) = match snapshot {
        Ok(EvaluationSnapshot::Loaded {
            store,
            schema_pair,
            vector,
            edges,
        }) => (store, schema_pair, vector, edges),
        Ok(EvaluationSnapshot::Unchanged(candidate)) => {
            return Ok(Ok(PreparedUnit::Unchanged(Arc::new(candidate))));
        }
        Err(failure) => return Ok(Err(failure)),
    };

    let canonical_body = canonical_body.to_owned();
    tokio::task::spawn_blocking(move || {
        evaluate_loaded(
            *store,
            &id,
            conforming_type,
            schema_pair,
            canonical_body,
            operation_item_id,
            edges,
            vector,
            &limits,
        )
    })
    .await
    .map_err(WorkerError::EvaluationTask)?
    .map(|result| result.map(|unit| PreparedUnit::Evaluated(Arc::new(unit))))
}

/// Run the CPU-heavy `gts-rust` validation and artifact materialization away from
/// the async executor. All database reads have completed before this function is
/// scheduled, so the blocking task owns a closed, in-memory unit store.
#[allow(clippy::too_many_arguments)]
fn evaluate_loaded(
    mut store: UnitStore,
    id: &GtsId,
    conforming_type: Option<String>,
    schema_pair: Option<(i64, i32)>,
    canonical_body: String,
    operation_item_id: i64,
    edges: Vec<DependencyEdge>,
    vector: RevisionVector,
    limits: &Limits,
) -> Result<Result<EvaluatedUnit, ItemFailure>, WorkerError> {
    if let Err(failure) = check_closure(store.store_mut(), id.id(), limits.resolution_closure) {
        return Ok(Err(failure));
    }
    let outcome = if id.is_type() {
        let resolved = match store.store_mut().validate_schema(id.id()) {
            Ok(resolved) => resolved,
            Err(e) => {
                return Ok(Err(ItemFailure::new(
                    AdmissionFailureReason::InvalidSchema,
                    e.to_string(),
                )));
            }
        };
        let artifacts = match materialize_bounded(&resolved, limits) {
            Ok(artifacts) => artifacts,
            Err(failure) => return Ok(Err(failure)),
        };
        EvaluatedOutcome::TypeSchema { artifacts }
    } else {
        // `Some` for every parsed Instance identifier: `get_type_id()` is `None` only
        // for a single segment, which `try_new` above already refused.
        let Some(type_id) = conforming_type else {
            return Ok(Err(ItemFailure::new(
                AdmissionFailureReason::InvalidIdentifier,
                format!("instance '{}' has no conforming type", id.id()),
            )));
        };
        // Checked before validation, so the failure names the cause:
        // `validate_instance` would report a missing schema as a content fault.
        let Some((type_schema_entity_id, type_schema_revision_no)) = schema_pair else {
            return Err(WorkerError::ConformingTypeAbsent {
                gts_id: id.id().to_owned(),
                type_id,
            });
        };
        // A type admitted under an older, larger budget must not bypass the
        // current resolution budget when it is used to validate an Instance.
        let resolved = match store.store_mut().validate_schema(&type_id) {
            Ok(resolved) => resolved,
            Err(error) => {
                return Ok(Err(ItemFailure::new(
                    AdmissionFailureReason::InvalidSchema,
                    error.to_string(),
                )));
            }
        };
        if let Err(failure) = materialize_bounded(&resolved, limits) {
            return Ok(Err(failure));
        }
        if let Err(e) = store.store_mut().validate_instance(id.id()) {
            return Ok(Err(ItemFailure::new(
                AdmissionFailureReason::InvalidValue,
                e.to_string(),
            )));
        }
        EvaluatedOutcome::Instance {
            type_schema_entity_id,
            type_schema_revision_no,
        }
    };

    let content_hash = content_hash(&canonical_body);
    Ok(Ok(EvaluatedUnit {
        gts_id: id.id().to_owned(),
        // Derived by `gts-rust`, never locally: the Registry Reference is a
        // deterministic UUIDv5 over the identifier and its namespace, and
        // reproducing that derivation here would be a second implementation of a
        // GTS rule (`constraint-gts-implementation`).
        gts_uuid: id.to_uuid(),
        family_key: family_key(id),
        canonical_body,
        content_hash,
        outcome,
        operation_item_id,
        edges,
        vector,
    }))
}

/// Resolve and replace an admitted entity's outgoing edges.
async fn replace_edges(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    entity_id: i64,
    edges: &[DependencyEdge],
) -> Result<(), WorkerError> {
    // An empty set must still delete the previous revision's edges.
    let targets: Vec<String> = edges.iter().map(|e| e.target.clone()).collect();
    let rows = stores.find_by_gts_ids(tx, scope, &targets).await?;
    let resolved: std::collections::HashMap<&str, i64> =
        rows.iter().map(|r| (r.gts_id.as_str(), r.id)).collect();

    let pairs: Vec<(DependencyKind, i64)> = edges
        .iter()
        .map(|edge| {
            resolved
                .get(edge.target.as_str())
                .copied()
                .map(|to| (edge.kind, to))
                .ok_or_else(|| WorkerError::DependencyTargetAbsent {
                    gts_id: edge.target.clone(),
                })
        })
        .collect::<Result<_, _>>()?;
    stores
        .replace_outgoing(tx, scope, entity_id, &pairs)
        .await?;
    Ok(())
}

/// Commit one evaluated unit: family, entity, revision, current-state projection,
/// and the item outcome.
///
/// The precondition recheck is inside the transaction because that is the only
/// place it means anything: a creation requires the identifier **absent**, and
/// between evaluation and here another admission may have created it.
///
/// # Errors
/// [`WorkerError`] for an infrastructure failure; a lost precondition race is an
/// [`ItemFailure`] in the `Ok(Err(..))` position.
pub async fn commit_creation(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    unit: &EvaluatedUnit,
    limits: &Limits,
    now: OffsetDateTime,
) -> Result<Result<CommittedUnit, ItemFailure>, WorkerError> {
    claim_entity_write_order(stores, tx, scope, now).await?;
    if stores
        .find_by_gts_id(tx, scope, &unit.gts_id)
        .await?
        .is_some()
    {
        return Ok(Err(ItemFailure::new(
            AdmissionFailureReason::AlreadyExists,
            format!(
                "'{}' already exists; a creation requires the identifier to be absent",
                unit.gts_id
            ),
        )));
    }

    // Create the family with its first member; the write-order claim serializes its rules.
    let (family, created) = stores
        .create_or_get(
            tx,
            scope,
            &unit.family_key,
            OwnershipScope::Global,
            None,
            now,
        )
        .await?;

    // Step 4.3: the revision vector, re-derived and compared inside this transaction, which the
    // `entity_write_order` claim above made exclusive.
    if let Err(failure) = vector::guard(
        stores,
        tx,
        scope,
        &unit.gts_id,
        &unit.vector,
        limits.activation_write_set,
    )
    .await?
    {
        return Ok(Err(failure));
    }

    // The three family rules — kind, minor shape, minor contiguity — in one call,
    // asked of a **new member** only: a revision adds nobody to the family and is
    // not gated. See `domain::family::rules`.
    //
    // Re-parsed rather than carried on `EvaluatedUnit`, which would hold two
    // spellings of one fact; the parse already succeeded in `evaluate`, so the
    // failure arm exists only because the type says it can.
    let id = match GtsId::try_new(&unit.gts_id) {
        Ok(id) => id,
        Err(e) => {
            return Ok(Err(ItemFailure::new(
                AdmissionFailureReason::InvalidIdentifier,
                format!("stored identifier '{}' does not parse: {e}", unit.gts_id),
            )));
        }
    };
    if let Some(refusal) = admits_new_member(
        stores,
        tx,
        scope,
        &id,
        &family,
        unit.outcome.entity_kind(),
        created,
    )
    .await?
    {
        return Ok(Err(ItemFailure::new(refusal.reason(), refusal.to_string())));
    }

    let inserted = stores
        .insert_entity(
            tx,
            scope,
            NewEntity {
                gts_uuid: unit.gts_uuid,
                gts_id: unit.gts_id.clone(),
                entity_kind: unit.outcome.entity_kind(),
                family_id: family.id,
                // A **projection** of the family row, never a second reading of the
                // request: the entity's owner columns are a copy kept for SecureORM
                // scoping and join-free visibility checks. Family ownership is
                // write-once, so this is the only writer of either column.
                ownership_scope: family.ownership_scope,
                owner_tenant_id: family.owner_tenant_id,
                owning_gear: Some(P0_OWNING_GEAR.to_owned()),
                now,
            },
        )
        .await?;
    // The same question as the check above, asked at the moment the unique key
    // answers it. `None` rather than a raised violation, so the loser's transaction
    // stays usable (`repo::conflict_do_nothing`).
    let Some(entity) = inserted else {
        return Ok(Err(ItemFailure::new(
            AdmissionFailureReason::AlreadyExists,
            format!(
                "'{}' was created concurrently; a creation requires the identifier to be absent",
                unit.gts_id
            ),
        )));
    };

    let revision_no = 1;
    match &unit.outcome {
        EvaluatedOutcome::TypeSchema { artifacts } => {
            stores
                .insert_schema_revision(
                    tx,
                    scope,
                    NewRevision {
                        entity_id: entity.id,
                        revision_no,
                        raw_schema: unit.canonical_body.clone(),
                        content_hash: unit.content_hash.clone(),
                        // Recorded for *every* revision, including one with no
                        // compatibility comparison at all: it identifies the engine,
                        // and that cannot be reconstructed later (ADR-0003).
                        gts_spec_version: GTS_SPECIFICATION_VERSION.to_owned(),
                        gts_impl_version: GTS_IMPLEMENTATION_VERSION.to_owned(),
                        compat_forced: false,
                        operation_item_id: unit.operation_item_id,
                        now,
                    },
                )
                .await?;

            stores
                .insert_current_schema(
                    tx,
                    scope,
                    NewCurrentTypeSchema {
                        entity_id: entity.id,
                        revision_no,
                        resolved_schema: artifacts.resolved_schema.clone(),
                        effective_traits: artifacts.effective_traits.clone(),
                        effective_traits_schema: artifacts.effective_traits_schema.clone(),
                        resolution_fingerprint: artifacts.resolution_fingerprint.clone(),
                        now,
                    },
                )
                .await?;
        }
        EvaluatedOutcome::Instance {
            type_schema_entity_id,
            type_schema_revision_no,
        } => {
            stores
                .insert_instance_revision(
                    tx,
                    scope,
                    NewInstanceRevision {
                        entity_id: entity.id,
                        revision_no,
                        canonical_value: unit.canonical_body.clone(),
                        content_hash: unit.content_hash.clone(),
                        // From evaluation's snapshot, not a fresh lookup: re-reading
                        // could pin a revision that landed after validation.
                        type_schema_entity_id: *type_schema_entity_id,
                        type_schema_revision_no: *type_schema_revision_no,
                        gts_spec_version: GTS_SPECIFICATION_VERSION.to_owned(),
                        gts_impl_version: GTS_IMPLEMENTATION_VERSION.to_owned(),
                        operation_item_id: unit.operation_item_id,
                        now,
                    },
                )
                .await?;

            stores
                .insert_current_instance(
                    tx,
                    scope,
                    NewCurrentInstance {
                        entity_id: entity.id,
                        revision_no,
                        now,
                    },
                )
                .await?;
        }
    }

    replace_edges(stores, tx, scope, entity.id, &unit.edges).await?;

    // The write is a CAS on the item's status, and its `false` must roll this
    // transaction back rather than be discarded: an overlapping pass already
    // recorded an outcome, and committing would leave an entity and a revision
    // behind an item that says otherwise. Everything written above goes with the
    // rollback — which is why the check belongs at the end of the transaction.
    if !stores
        .mark_item_succeeded(
            tx,
            scope,
            unit.operation_item_id,
            revision_no,
            entity.resource_version,
            now,
        )
        .await?
    {
        return Err(WorkerError::ItemAlreadyTerminal {
            item_id: unit.operation_item_id,
        });
    }

    Ok(Ok(CommittedUnit {
        gts_uuid: unit.gts_uuid,
        revision_no,
        resource_version: entity.resource_version,
    }))
}

/// Record an unchanged candidate without creating a revision.
async fn commit_unchanged(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    unit: &EvaluatedUnit,
    entity_id: i64,
    expected_resource_version: i64,
    now: OffsetDateTime,
) -> Result<Result<RevisionCommit, ItemFailure>, WorkerError> {
    // This re-read detects a vanished entity, not missing kind-specific state.
    let still = stores
        .find_by_gts_id(tx, scope, &unit.gts_id)
        .await?
        .ok_or_else(|| WorkerError::EntityVanished {
            gts_id: unit.gts_id.clone(),
            entity_id,
        })?;
    if still.resource_version != expected_resource_version {
        return Ok(Err(stale_precondition(
            &unit.gts_id,
            expected_resource_version,
            still.resource_version,
        )));
    }
    terminalize_unchanged(stores, tx, scope, &still, unit.operation_item_id, now).await
}

/// Commit an evaluated unit as a revision with compare-and-swap protection.
/// The final CAS closes the `READ COMMITTED` window after validation.
///
/// # Errors
/// [`WorkerError`] for infrastructure failures; candidate refusals are returned as
/// [`ItemFailure`] without committing.
#[allow(clippy::too_many_arguments)]
pub async fn commit_revision(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    unit: &EvaluatedUnit,
    expected_resource_version: i64,
    limits: &Limits,
    now: OffsetDateTime,
    metrics: &Arc<dyn AdmissionMetrics>,
) -> Result<Result<RevisionCommit, ItemFailure>, WorkerError> {
    claim_entity_write_order(stores, tx, scope, now).await?;
    let entity =
        match revision_entity(stores, tx, scope, &unit.gts_id, expected_resource_version).await? {
            Ok(entity) => entity,
            Err(failure) => return Ok(Err(failure)),
        };

    // Keep the artifact CAS token with the content that supplied it.
    let current = read_current_content(
        stores,
        tx,
        scope,
        &unit.gts_id,
        unit.outcome.entity_kind(),
        entity.id,
    )
    .await?;

    // The hash is a prefilter and the bytes are the decision (ADR-0012): a digest
    // collision would otherwise silently swallow a real edit. Equality against an
    // *older* revision is deliberately not asked — that is an ordinary update which
    // allocates a new number rather than moving the pointer backwards (ADR-0005).
    if current.matches_authored(&unit.content_hash, &unit.canonical_body) {
        return commit_unchanged(
            stores,
            tx,
            scope,
            unit,
            entity.id,
            expected_resource_version,
            now,
        )
        .await;
    }

    if expected_resource_version == i64::MAX {
        return Err(WorkerError::ResourceVersionExhausted {
            gts_id: unit.gts_id.clone(),
        });
    }

    // Step 4.3: re-derive and compare the complete revision vector.
    if let Err(failure) = vector::guard(
        stores,
        tx,
        scope,
        &unit.gts_id,
        &unit.vector,
        limits.activation_write_set,
    )
    .await?
    {
        return Ok(Err(failure));
    }

    // One statement carrying the precondition, so there is no window between
    // checking the version and moving it. `None` is the lost race — the version
    // moved, or the entity was deleted, both of which the statement's `WHERE`
    // covers and neither of which it can tell apart.
    let Some(resource_version) = stores
        .compare_and_swap_version(tx, scope, entity.id, expected_resource_version, now)
        .await?
    else {
        return Ok(Err(ItemFailure::new(
            AdmissionFailureReason::PreconditionFailed,
            format!(
                "'{}' moved past resource_version {expected_resource_version}, or was deleted, \
                 while this revision was being admitted",
                unit.gts_id
            ),
        )));
    };
    let revision_no = current.revision_no().checked_add(1).ok_or_else(|| {
        WorkerError::RevisionNumberExhausted {
            gts_id: unit.gts_id.clone(),
        }
    })?;

    match &unit.outcome {
        EvaluatedOutcome::TypeSchema { artifacts } => {
            let CurrentContent::TypeSchema { cas, .. } = &current else {
                // A mismatched variant means the stored kind-specific rows disagree.
                return Err(WorkerError::CurrentStateMissing {
                    gts_id: unit.gts_id.clone(),
                    entity_id: entity.id,
                });
            };
            stores
                .insert_schema_revision(
                    tx,
                    scope,
                    NewRevision {
                        entity_id: entity.id,
                        revision_no,
                        raw_schema: unit.canonical_body.clone(),
                        content_hash: unit.content_hash.clone(),
                        gts_spec_version: GTS_SPECIFICATION_VERSION.to_owned(),
                        gts_impl_version: GTS_IMPLEMENTATION_VERSION.to_owned(),
                        compat_forced: false,
                        operation_item_id: unit.operation_item_id,
                        now,
                    },
                )
                .await?;
            if !stores
                .update_current_schema(
                    tx,
                    scope,
                    NewCurrentTypeSchema {
                        entity_id: entity.id,
                        revision_no,
                        resolved_schema: artifacts.resolved_schema.clone(),
                        effective_traits: artifacts.effective_traits.clone(),
                        effective_traits_schema: artifacts.effective_traits_schema.clone(),
                        resolution_fingerprint: artifacts.resolution_fingerprint.clone(),
                        now,
                    },
                    cas.clone(),
                )
                .await?
            {
                // A CAS miss is retryable drift, not corrupt state.
                return Err(WorkerError::RevalidationRequired(
                    VectorDrift::CurrentProjectionMoved {
                        gts_id: unit.gts_id.clone(),
                    },
                ));
            }
        }
        EvaluatedOutcome::Instance {
            type_schema_entity_id,
            type_schema_revision_no,
        } => {
            stores
                .insert_instance_revision(
                    tx,
                    scope,
                    NewInstanceRevision {
                        entity_id: entity.id,
                        revision_no,
                        canonical_value: unit.canonical_body.clone(),
                        content_hash: unit.content_hash.clone(),
                        // Re-recorded per revision, not inherited: this value was
                        // validated against whatever the schema's current revision
                        // was at *this* evaluation.
                        type_schema_entity_id: *type_schema_entity_id,
                        type_schema_revision_no: *type_schema_revision_no,
                        gts_spec_version: GTS_SPECIFICATION_VERSION.to_owned(),
                        gts_impl_version: GTS_IMPLEMENTATION_VERSION.to_owned(),
                        operation_item_id: unit.operation_item_id,
                        now,
                    },
                )
                .await?;
            if !stores
                .update_current_instance(
                    tx,
                    scope,
                    NewCurrentInstance {
                        entity_id: entity.id,
                        revision_no,
                        now,
                    },
                )
                .await?
            {
                return Err(WorkerError::CurrentStateMissing {
                    gts_id: unit.gts_id.clone(),
                    entity_id: entity.id,
                });
            }
        }
    }

    // Every authored revision replaces its outgoing edges.
    replace_edges(stores, tx, scope, entity.id, &unit.edges).await?;

    refresh_reverse_impact(stores, tx, scope, unit, entity.id, limits, now, metrics).await?;

    // Last, and its `false` rolls everything above back — see `commit_creation`.
    if !stores
        .mark_item_succeeded(
            tx,
            scope,
            unit.operation_item_id,
            revision_no,
            resource_version,
            now,
        )
        .await?
    {
        return Err(WorkerError::ItemAlreadyTerminal {
            item_id: unit.operation_item_id,
        });
    }

    Ok(Ok(RevisionCommit::Admitted(CommittedUnit {
        gts_uuid: unit.gts_uuid,
        revision_no,
        resource_version,
    })))
}

/// Step 4.6: re-materialize artifacts of all dependents.
#[allow(clippy::too_many_arguments)]
async fn refresh_reverse_impact(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    unit: &EvaluatedUnit,
    entity_id: i64,
    limits: &Limits,
    now: OffsetDateTime,
    metrics: &Arc<dyn AdmissionMetrics>,
) -> Result<(), WorkerError> {
    if !matches!(unit.outcome, EvaluatedOutcome::TypeSchema { .. }) {
        return Ok(());
    }
    match refresh_dependents(stores, tx, scope, &[entity_id], limits, now).await? {
        Ok(outcome) => {
            // Record only write sets that actually commit.
            metrics.observe_activation_write_set(outcome.refreshed.len());
            tracing::debug!(
                gts_id = %unit.gts_id,
                refreshed = outcome.refreshed.len(),
                examined = outcome.examined,
                "types_registry refreshed the dependents of a revision"
            );
            Ok(())
        }
        Err(failure) => Err(WorkerError::RefusedAfterWrite(failure)),
    }
}

/// Canonicalize a document the way acceptance did, for a caller that has a `Value`
/// rather than the stored text. Exposed so the seeding path and tests share one
/// canonical form with the acceptance path.
#[must_use]
pub fn canonical_body(content: &Value) -> String {
    canonical_text(content)
}
