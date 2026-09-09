//! Commit-time revision-vector guard and bounded revalidation loop.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]
#![recursion_limit = "256"]

mod common;

use std::sync::Arc;

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::ScopeError;
use toolkit_db::{DBProvider, DbError, DbTx};
use toolkit_gts::gts_id;
use uuid::Uuid;

use common::{
    CasMissStores, PausePoint, PausingStores, TestDir, allow_all, stores, test_db, test_db_file,
    worker_settings,
};
use types_registry::config::{TypesRegistryConfig, WorkerSettings};
use types_registry::domain::admission::AdmissionFailureReason;
use types_registry::domain::admission::acceptance::{AcceptanceContext, AcceptanceError, accept};
use types_registry::domain::admission::unit::{
    EvaluatedUnit, RevisionCommit, commit_creation, commit_revision, evaluate,
};
use types_registry::domain::admission::vector::{VectorDrift, VectorRole};
use types_registry::domain::admission::worker::{
    OperationOutcome, Tuning, WorkerError, run_operation,
};
use types_registry::domain::admission::{Candidate, OperationDispatch, SubmitRequest};
use types_registry::domain::enums as domain_enums;
use types_registry::domain::enums::OperationItemStatus;
use types_registry::domain::policy::RegistrationPolicy;
use types_registry::domain::ports::{
    CurrentSchemaCas, CurrentTypeSchemaRow, EntityRow, NewCurrentTypeSchema, Stores, commit_write,
};
use types_registry::domain::registry_service::{
    AdmissionMode, EntityKey, RegistryService, ServiceError,
};
use types_registry::infra::storage::repo::{
    CoordinationStateRepo, EntityRepo, OperationRepo, TypeSchemaRepo,
};

const NOW: OffsetDateTime = datetime!(2026-08-20 09:15:30 UTC);
const LATER: OffsetDateTime = datetime!(2026-08-20 10:20:40 UTC);

const BASE: &str = gts_id!("cf.core.reval.thing.v1~");
const DERIVED: &str = gts_id!("cf.core.reval.thing.v1~cf.core.reval.leaf.v1~");
const REFERRER: &str = gts_id!("cf.core.reval.referrer.v1~");

type Provider = Arc<DBProvider<DbError>>;

struct NoDispatch;

#[async_trait::async_trait]
impl OperationDispatch for NoDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }
}

fn worker(db: &Provider) -> DBProvider<WorkerError> {
    DBProvider::new(db.db())
}

fn base_schema(property: &str) -> Value {
    json!({
        "$id": format!("gts://{BASE}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { property: { "type": "string" } },
    })
}

fn derived_schema() -> Value {
    json!({
        "$id": format!("gts://{DERIVED}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "allOf": [
            { "$ref": format!("gts://{BASE}") },
            { "type": "object", "properties": { "tier": { "type": "string" } } },
        ],
    })
}

fn referencing_schema(marker: &str) -> Value {
    json!({
        "$id": format!("gts://{REFERRER}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "subject": { "$ref": format!("gts://{BASE}") },
            marker: { "type": "string" },
        },
    })
}

/// A referrer that reaches `BASE` through `DERIVED`.
fn chained_schema(marker: &str) -> Value {
    json!({
        "$id": format!("gts://{REFERRER}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "leaf": { "$ref": format!("gts://{DERIVED}") },
            marker: { "type": "string" },
        },
    })
}

async fn submit(
    db: &Provider,
    key: &str,
    gts_id: &str,
    content: Value,
    expected_resource_version: Option<i64>,
) -> Result<Uuid, AcceptanceError> {
    let provider: DBProvider<AcceptanceError> = DBProvider::new(db.db());
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(NoDispatch);
    accept(
        &stores(),
        &provider,
        &allow_all(),
        &AcceptanceContext {
            policy: &policy,
            config: &config,
            metrics: &common::metrics(),
        },
        &dispatch,
        &SubmitRequest {
            idempotency_key: key.to_owned(),
            kind: domain_enums::OperationKind::Registration,
            dry_run: false,
            candidates: vec![Candidate {
                gts_id: gts_id.to_owned(),
                content: Some(content),
                expected_resource_version,
                force: false,
            }],
        },
        NOW,
    )
    .await
    .map(|accepted| accepted.operation_id)
}

async fn admit(
    db: &Provider,
    key: &str,
    gts_id: &str,
    content: Value,
    expected_resource_version: Option<i64>,
) -> OperationOutcome {
    let operation_id = submit(db, key, gts_id, content, expected_resource_version)
        .await
        .expect("acceptance");
    run_operation(
        &stores(),
        &worker(db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &worker_settings(),
            metrics: &common::metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("the worker must not fail on infrastructure")
}

async fn evaluated(
    db: &Provider,
    key: &str,
    gts_id: &str,
    content: Value,
    expected_resource_version: Option<i64>,
) -> EvaluatedUnit {
    submitted(db, key, gts_id, content, expected_resource_version)
        .await
        .1
}

/// Return the accepted operation and its evaluated candidate.
async fn submitted(
    db: &Provider,
    key: &str,
    gts_id: &str,
    content: Value,
    expected_resource_version: Option<i64>,
) -> (Uuid, EvaluatedUnit) {
    let operation_id = submit(db, key, gts_id, content, expected_resource_version)
        .await
        .expect("acceptance");
    let provider = worker(db);
    let conn = provider.conn().expect("conn");
    let item = OperationRepo::find_items(&conn, &allow_all(), operation_id)
        .await
        .expect("items")[0]
        .clone();
    let payload = item.request_payload.clone().expect("payload");
    let unit = evaluate(
        &stores(),
        &provider,
        &allow_all(),
        &item.gts_id,
        &payload,
        item.id,
        &common::limits(),
    )
    .await
    .expect("evaluation must not fail on infrastructure")
    .expect("the candidate is valid");
    (operation_id, unit)
}

async fn commit_the_revision(
    db: &Provider,
    unit: &EvaluatedUnit,
    expected_resource_version: i64,
) -> Result<
    Result<RevisionCommit, types_registry::domain::admission::worker::ItemFailure>,
    WorkerError,
> {
    commit_the_revision_with(stores(), db, unit, expected_resource_version).await
}

/// Commit through explicit, optionally decorated ports.
async fn commit_the_revision_with(
    ports: Arc<dyn Stores>,
    db: &Provider,
    unit: &EvaluatedUnit,
    expected_resource_version: i64,
) -> Result<
    Result<RevisionCommit, types_registry::domain::admission::worker::ItemFailure>,
    WorkerError,
> {
    let provider = worker(db);
    let unit = unit.clone();
    provider
        .transaction_with_config(commit_write(&provider.db()), move |tx| {
            let unit = unit.clone();
            let ports = Arc::clone(&ports);
            Box::pin(async move {
                commit_revision(
                    ports.as_ref(),
                    tx,
                    &allow_all(),
                    &unit,
                    expected_resource_version,
                    &common::limits(),
                    LATER,
                    &common::metrics(),
                )
                .await
            })
        })
        .await
}

/// Whether one immutable authored revision exists.
async fn revision_exists(db: &Provider, entity_id: i64, revision_no: i32) -> bool {
    use sea_orm::EntityTrait as _;
    use toolkit_db::secure::SecureEntityExt as _;
    use types_registry::infra::storage::entity::type_schema_revision;

    let conn = db.conn().expect("conn");
    type_schema_revision::Entity::find_by_id((entity_id, revision_no))
        .secure()
        .scope_with(&allow_all())
        .one(&conn)
        .await
        .expect("read authored revision")
        .is_some()
}

async fn entity(db: &Provider, gts_id: &str) -> EntityRow {
    let conn = db.conn().expect("conn");
    EntityRepo::find_by_gts_id(&conn, &allow_all(), gts_id)
        .await
        .expect("read")
        .unwrap_or_else(|| panic!("{gts_id} must exist"))
}

async fn current(db: &Provider, gts_id: &str) -> CurrentTypeSchemaRow {
    let entity_id = entity(db, gts_id).await.id;
    let conn = db.conn().expect("conn");
    TypeSchemaRepo::find_current(&conn, &allow_all(), entity_id)
        .await
        .expect("read")
        .unwrap_or_else(|| panic!("{gts_id} must have a current row"))
}

async fn seed_base_and_dependents(db: &Provider) {
    admit(db, "k-base", BASE, base_schema("name"), None).await;
    admit(db, "k-derived", DERIVED, derived_schema(), None).await;
    admit(db, "k-referrer", REFERRER, referencing_schema("note"), None).await;
}

/// Seeds `BASE ← DERIVED ← REFERRER`.
async fn seed_the_chain(db: &Provider) {
    admit(db, "k-base", BASE, base_schema("name"), None).await;
    admit(db, "k-derived", DERIVED, derived_schema(), None).await;
    admit(db, "k-referrer", REFERRER, chained_schema("note"), None).await;
}

#[tokio::test]
async fn a_commit_whose_vector_did_not_move_stands() {
    let db = test_db().await;
    seed_base_and_dependents(&db).await;

    let unit = evaluated(&db, "k-ref-2", REFERRER, referencing_schema("tag"), Some(1)).await;
    let committed = commit_the_revision(&db, &unit, 1)
        .await
        .expect("no infrastructure failure")
        .expect("no candidate refusal");

    assert!(matches!(committed, RevisionCommit::Admitted(_)));
    assert_eq!(entity(&db, REFERRER).await.resource_version, 2);
}

#[tokio::test]
async fn a_dependency_that_moved_after_evaluation_rolls_the_commit_back() {
    let db = test_db().await;
    seed_base_and_dependents(&db).await;

    let unit = evaluated(&db, "k-ref-2", REFERRER, referencing_schema("tag"), Some(1)).await;
    let before = entity(&db, REFERRER).await;

    admit(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;

    let outcome = commit_the_revision(&db, &unit, 1).await;

    let Err(WorkerError::RevalidationRequired(drift)) = outcome else {
        panic!("the guard must refuse a stale evaluation, got {outcome:?}");
    };
    assert_eq!(
        drift,
        VectorDrift::Moved {
            gts_id: BASE.to_owned(),
            role: VectorRole::Dependency,
            recorded: 1,
            found: 2,
        }
    );

    let after = entity(&db, REFERRER).await;
    assert_eq!(
        after.resource_version, before.resource_version,
        "a rolled-back commit moves no version"
    );
    assert!(
        !current(&db, REFERRER).await.resolved_schema.contains("tag"),
        "a rolled-back commit writes no revision"
    );
}

#[tokio::test]
async fn a_phantom_dependent_created_after_the_scan_is_detected() {
    let db = test_db().await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;

    let unit = evaluated(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;
    assert!(
        unit.vector.entries().is_empty(),
        "the base has no dependencies and, yet, no dependents: {:?}",
        unit.vector
    );

    admit(&db, "k-derived", DERIVED, derived_schema(), None).await;

    let outcome = commit_the_revision(&db, &unit, 1).await;

    let Err(WorkerError::RevalidationRequired(drift)) = outcome else {
        panic!("a phantom dependent must roll the commit back, got {outcome:?}");
    };
    assert_eq!(
        drift,
        VectorDrift::Appeared {
            gts_id: DERIVED.to_owned(),
            role: VectorRole::Dependent,
        }
    );
    assert_eq!(
        entity(&db, BASE).await.resource_version,
        1,
        "a rolled-back commit moves no version"
    );
}

#[tokio::test]
async fn a_dependent_refreshed_after_the_scan_is_detected() {
    let db = test_db().await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;
    admit(&db, "k-derived", DERIVED, derived_schema(), None).await;

    let unit = evaluated(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;
    let recorded = unit
        .vector
        .entries()
        .iter()
        .find(|entry| entry.gts_id == DERIVED)
        .expect("the derived type is a dependent of the base")
        .clone();
    assert_eq!(recorded.role, VectorRole::Dependent);
    assert!(
        recorded.resolution_fingerprint.is_some(),
        "a live Type Schema dependent's effective content is consumed, so its \
         fingerprint is recorded"
    );

    let derived = current(&db, DERIVED).await;
    let derived_id = entity(&db, DERIVED).await.id;
    let before_version = entity(&db, DERIVED).await.resource_version;
    let ports = stores();
    worker(&db)
        .transaction(move |tx| {
            let ports = Arc::clone(&ports);
            Box::pin(async move {
                let moved = ports
                    .update_current_schema(
                        tx,
                        &allow_all(),
                        NewCurrentTypeSchema {
                            entity_id: derived_id,
                            revision_no: derived.revision_no,
                            resolved_schema: derived.resolved_schema.clone(),
                            effective_traits: derived.effective_traits.clone(),
                            effective_traits_schema: derived.effective_traits_schema.clone(),
                            resolution_fingerprint: vec![0xFF; 32],
                            now: LATER,
                        },
                        // The refresh names the state it computed its artifacts
                        // against; simulated here from the row as it stands.
                        CurrentSchemaCas {
                            revision_no: derived.revision_no,
                            resolution_fingerprint: derived.resolution_fingerprint.clone(),
                        },
                    )
                    .await?;
                assert!(moved, "the refresh must find the dependent's current row");
                Ok(())
            })
        })
        .await
        .expect("the simulated refresh commits");
    assert_eq!(
        entity(&db, DERIVED).await.resource_version,
        before_version,
        "a refresh moves no version, which is the whole reason the fingerprint is in \
         the vector"
    );

    let outcome = commit_the_revision(&db, &unit, 1).await;

    let Err(WorkerError::RevalidationRequired(drift)) = outcome else {
        panic!("a refreshed dependent must roll the commit back, got {outcome:?}");
    };
    assert_eq!(
        drift,
        VectorDrift::Refreshed {
            gts_id: DERIVED.to_owned(),
        }
    );
}

#[tokio::test]
async fn a_creation_whose_dependency_moved_after_evaluation_rolls_the_commit_back() {
    let db = test_db().await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;

    let unit = evaluated(&db, "k-ref", REFERRER, referencing_schema("note"), None).await;
    admit(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;

    let provider = worker(&db);
    let ports = stores();
    let candidate = unit.clone();
    let outcome = provider
        .transaction_with_config(commit_write(&provider.db()), move |tx| {
            let candidate = candidate.clone();
            let ports = Arc::clone(&ports);
            Box::pin(async move {
                commit_creation(
                    ports.as_ref(),
                    tx,
                    &allow_all(),
                    &candidate,
                    &common::limits(),
                    LATER,
                )
                .await
            })
        })
        .await;

    assert!(
        matches!(
            outcome,
            Err(WorkerError::RevalidationRequired(VectorDrift::Moved { .. }))
        ),
        "the guard must refuse a stale creation, got {outcome:?}"
    );
    let conn = db.conn().expect("conn");
    assert!(
        EntityRepo::find_by_gts_id(&conn, &allow_all(), REFERRER)
            .await
            .expect("read")
            .is_none(),
        "a rolled-back creation leaves no entity"
    );
}

#[tokio::test]
async fn an_unchanged_resubmission_is_not_refused_by_a_moved_dependency() {
    let db = test_db().await;
    seed_base_and_dependents(&db).await;

    let unit = evaluated(
        &db,
        "k-ref-same",
        REFERRER,
        referencing_schema("note"),
        Some(1),
    )
    .await;
    admit(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;

    let committed = commit_the_revision(&db, &unit, 1)
        .await
        .expect("no infrastructure failure")
        .expect("no candidate refusal");

    assert!(
        matches!(committed, RevisionCommit::Unchanged { .. }),
        "got {committed:?}"
    );
}

async fn admit_with_a_mutation_in_the_gap<F, Fut>(
    db: &Provider,
    settings: WorkerSettings,
    operation_id: Uuid,
    mutate: F,
) -> OperationOutcome
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    // Pause before the claim so the mutation can commit in the evaluation gap.
    let (paused, reached, resume) = PausingStores::new(PausePoint::BeforeEntityWriteOrderClaim);
    let ports: Arc<dyn Stores> = paused;
    let provider = worker(db);
    let pass = tokio::spawn(async move {
        run_operation(
            &ports,
            &provider,
            &allow_all(),
            Tuning {
                limits: &common::limits(),
                worker: &settings,
                metrics: &common::metrics(),
            },
            operation_id,
            LATER,
        )
        .await
    });

    reached.await.expect("the pass must reach the commit");
    mutate().await;
    resume.send(()).expect("the pass must still be waiting");

    pass.await
        .expect("the pass task must not panic")
        .expect("the worker must not fail on infrastructure")
}

#[tokio::test]
async fn a_dependency_mutated_between_evaluation_and_commit_costs_one_rollback_and_one_retry() {
    let dir = TestDir::new("types-registry-reval-retry");
    let db = test_db_file(&dir.path().join("registry.db")).await;
    seed_the_chain(&db).await;

    let operation_id = submit(&db, "k-ref-2", REFERRER, chained_schema("tag"), Some(1))
        .await
        .expect("acceptance");

    let mutating = Arc::clone(&db);
    let outcome = admit_with_a_mutation_in_the_gap(
        &db,
        worker_settings(),
        operation_id,
        move || async move {
            admit(&mutating, "k-base-2", BASE, base_schema("label"), Some(1)).await;
        },
    )
    .await;

    let item = &outcome.items[0];
    assert_eq!(
        item.status,
        OperationItemStatus::Succeeded,
        "the retry must succeed, got {item:?}"
    );
    assert_eq!(item.resource_version, Some(2));
    assert_eq!(
        item.revision_no,
        Some(2),
        "one revision, not two: the drifted attempt wrote nothing"
    );

    let after = current(&db, REFERRER).await;
    assert!(
        after.resolved_schema.contains("tag"),
        "the candidate's own change landed, got {}",
        after.resolved_schema
    );
    assert!(
        after.resolved_schema.contains("label"),
        "the retry re-resolved against the moved base; a committed stale evaluation \
         would still inline `name`, got {}",
        after.resolved_schema
    );
}

/// Refresh writes compare-and-swap the projection used to compute their artifacts.
#[tokio::test]
async fn a_refresh_write_is_a_compare_and_swap_on_the_fingerprint_it_read() {
    let db = test_db().await;
    seed_base_and_dependents(&db).await;

    let derived = current(&db, DERIVED).await;
    let derived_id = entity(&db, DERIVED).await.id;
    let stale = vec![0xAB; 32];
    assert_ne!(
        derived.resolution_fingerprint, stale,
        "the fixture must not accidentally carry the fingerprint this test calls stale"
    );

    let ports = stores();
    let write = |expected: CurrentSchemaCas, marker: &'static str| {
        let ports = Arc::clone(&ports);
        let derived = derived.clone();
        let provider = worker(&db);
        async move {
            provider
                .transaction(move |tx| {
                    let ports = Arc::clone(&ports);
                    let derived = derived.clone();
                    let expected = expected.clone();
                    Box::pin(async move {
                        Ok(ports
                            .update_current_schema(
                                tx,
                                &allow_all(),
                                NewCurrentTypeSchema {
                                    entity_id: derived_id,
                                    revision_no: derived.revision_no,
                                    resolved_schema: format!("{{\"marker\":\"{marker}\"}}"),
                                    effective_traits: derived.effective_traits.clone(),
                                    effective_traits_schema: derived
                                        .effective_traits_schema
                                        .clone(),
                                    resolution_fingerprint: vec![0xCD; 32],
                                    now: LATER,
                                },
                                expected,
                            )
                            .await?)
                    })
                })
                .await
                .expect("no infrastructure failure")
        }
    };

    assert!(
        !write(
            CurrentSchemaCas {
                revision_no: derived.revision_no,
                resolution_fingerprint: stale,
            },
            "lost"
        )
        .await,
        "a write whose expected fingerprint has moved must not land"
    );
    assert!(
        !write(
            CurrentSchemaCas {
                revision_no: derived.revision_no + 1,
                resolution_fingerprint: derived.resolution_fingerprint.clone(),
            },
            "lost"
        )
        .await,
        "nor one whose expected revision has moved, even with the fingerprint intact"
    );
    assert_eq!(
        current(&db, DERIVED).await.resolved_schema,
        derived.resolved_schema,
        "and the row is left exactly as it was"
    );

    assert!(
        write(
            CurrentSchemaCas {
                revision_no: derived.revision_no,
                resolution_fingerprint: derived.resolution_fingerprint.clone(),
            },
            "won"
        )
        .await,
        "the same write lands while the row still carries the state it read"
    );
    assert_eq!(
        current(&db, DERIVED).await.resolved_schema,
        "{\"marker\":\"won\"}"
    );
}

/// A dependent refresh CAS miss requests revalidation and rolls back the candidate.
#[tokio::test]
async fn a_dependent_refresh_losing_the_compare_and_swap_rolls_the_commit_back() {
    let db = test_db().await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;
    admit(&db, "k-derived", DERIVED, derived_schema(), None).await;

    let derived_before = current(&db, DERIVED).await;
    let (operation_id, unit) =
        submitted(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;

    let derived_id = entity(&db, DERIVED).await.id;
    let ports = CasMissStores::new(derived_id);
    let outcome = commit_the_revision_with(ports, &db, &unit, 1).await;

    let Err(WorkerError::RevalidationRequired(VectorDrift::CurrentProjectionMoved { ref gts_id })) =
        outcome
    else {
        panic!("a lost refresh compare-and-swap must ask for revalidation, got {outcome:?}");
    };
    assert_eq!(
        gts_id, DERIVED,
        "the dependent's write lost, not the candidate's"
    );

    assert_eq!(
        entity(&db, BASE).await.resource_version,
        1,
        "the candidate's version did not move"
    );
    assert_eq!(
        current(&db, BASE).await.revision_no,
        1,
        "and no candidate pointer landed"
    );
    let derived_after = current(&db, DERIVED).await;
    assert_eq!(
        derived_after.revision_no, derived_before.revision_no,
        "the refresh wrote no dependent pointer"
    );
    assert_eq!(
        derived_after.resolution_fingerprint, derived_before.resolution_fingerprint,
        "and no dependent artifacts"
    );

    // The rollback also removes the immutable candidate revision.
    let conn = db.conn().expect("conn");
    let base_id = entity(&db, BASE).await.id;
    assert!(
        !revision_exists(&db, base_id, 2).await,
        "no immutable revision 2 was created"
    );

    let item = &OperationRepo::find_items(&conn, &allow_all(), operation_id)
        .await
        .expect("items")[0];
    assert_eq!(
        item.status,
        OperationItemStatus::Pending,
        "the rolled-back outcome was never recorded"
    );
    assert_eq!(item.result_revision_no, None);
    assert_eq!(item.result_resource_version, None);
}

/// A candidate artifact CAS miss also requests revalidation and rolls back.
#[tokio::test]
async fn a_candidate_current_write_losing_the_compare_and_swap_rolls_the_commit_back() {
    let db = test_db().await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;

    let base_before = current(&db, BASE).await;
    let (operation_id, unit) =
        submitted(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;

    let base_id = entity(&db, BASE).await.id;
    let ports = CasMissStores::new(base_id);
    let outcome = commit_the_revision_with(ports, &db, &unit, 1).await;

    let Err(WorkerError::RevalidationRequired(VectorDrift::CurrentProjectionMoved { ref gts_id })) =
        outcome
    else {
        panic!("a lost candidate compare-and-swap must ask for revalidation, got {outcome:?}");
    };
    assert_eq!(
        gts_id, BASE,
        "the candidate's own write lost, not a dependent's"
    );

    assert_eq!(
        entity(&db, BASE).await.resource_version,
        1,
        "the version did not move"
    );
    let base_after = current(&db, BASE).await;
    assert_eq!(
        base_after.revision_no, base_before.revision_no,
        "no pointer move landed"
    );
    assert_eq!(
        base_after.resolved_schema, base_before.resolved_schema,
        "and no artifacts landed"
    );
    assert_eq!(
        base_after.resolution_fingerprint, base_before.resolution_fingerprint,
        "not even the fingerprint the new revision would have stamped"
    );

    // The rollback also removes the immutable candidate revision.
    let conn = db.conn().expect("conn");
    assert!(
        !revision_exists(&db, base_id, 2).await,
        "no immutable revision 2 was created"
    );

    let item = &OperationRepo::find_items(&conn, &allow_all(), operation_id)
        .await
        .expect("items")[0];
    assert_eq!(
        item.status,
        OperationItemStatus::Pending,
        "the rolled-back outcome was never recorded"
    );
    assert_eq!(item.result_revision_no, None);
    assert_eq!(item.result_resource_version, None);
}

/// Read the current entity-write claim count.
async fn entity_write_sequence(db: &Provider) -> i64 {
    let conn = db.conn().expect("conn");
    CoordinationStateRepo::entity_write_sequence(&conn, &allow_all())
        .await
        .expect("the migration seeds the state row")
}

/// A creation claims `entity_write_order` exactly once.
#[tokio::test]
async fn a_creation_claims_the_entity_write_order_row_exactly_once() {
    let db = test_db().await;
    let before = entity_write_sequence(&db).await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;
    assert_eq!(
        entity_write_sequence(&db).await - before,
        1,
        "a creation claims the row exactly once"
    );
}

/// A revision claims `entity_write_order` exactly once.
#[tokio::test]
async fn a_revision_claims_the_entity_write_order_row_exactly_once() {
    let db = test_db().await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;

    let before = entity_write_sequence(&db).await;
    admit(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;
    assert_eq!(
        entity_write_sequence(&db).await - before,
        1,
        "a revision claims the row exactly once"
    );
}

/// An `unchanged` commit claims `entity_write_order` exactly once.
#[tokio::test]
async fn an_unchanged_resubmission_claims_the_entity_write_order_row_exactly_once() {
    let db = test_db().await;
    admit(&db, "k-base", BASE, base_schema("name"), None).await;
    admit(&db, "k-base-2", BASE, base_schema("label"), Some(1)).await;

    let before = entity_write_sequence(&db).await;
    admit(&db, "k-base-3", BASE, base_schema("label"), Some(2)).await;
    assert_eq!(
        entity_write_sequence(&db).await - before,
        1,
        "the `unchanged` outcome still opens a commit transaction, exactly once"
    );
}

/// Read one coordination-state row.
async fn read_coordination_state(
    conn: &toolkit_db::secure::DbConn<'_>,
    scope: &toolkit_db::secure::AccessScope,
    name: &str,
) -> types_registry::infra::storage::entity::coordination_state::Model {
    use sea_orm::EntityTrait as _;
    use toolkit_db::secure::SecureEntityExt as _;
    types_registry::infra::storage::entity::coordination_state::Entity::find_by_id(name.to_owned())
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .expect("read the state row")
        .unwrap_or_else(|| panic!("{name} row present"))
}

/// A claim advances only `entity_write_order` and uses the caller's timestamp.
#[tokio::test]
async fn claiming_entity_write_order_advances_exactly_that_state() {
    use sea_orm::Set;
    use toolkit_db::secure::secure_insert;
    use types_registry::infra::storage::entity::coordination_state;

    const OTHER_STATE: &str = "future_routing";
    const OTHER_SEQ: i64 = 5;
    const OTHER_AT: OffsetDateTime = datetime!(2026-08-19 00:00:00 UTC);

    let db = test_db().await;
    let conn = db.conn().expect("conn");
    let scope = allow_all();

    secure_insert::<coordination_state::Entity>(
        coordination_state::ActiveModel {
            state_name: Set(OTHER_STATE.to_owned()),
            state_seq: Set(OTHER_SEQ),
            updated_at: Set(OTHER_AT),
        },
        &scope,
        &conn,
    )
    .await
    .expect("seed a second state row");

    CoordinationStateRepo::claim_entity_write_order(&conn, &scope, NOW)
        .await
        .expect("the seeded row accepts the claim");

    let claimed = read_coordination_state(&conn, &scope, "entity_write_order").await;
    assert_eq!(
        claimed.state_seq, 1,
        "one claim advances the sequence by exactly one"
    );
    assert_eq!(
        claimed.updated_at, NOW,
        "updated_at is the caller's deterministic now"
    );

    let untouched = read_coordination_state(&conn, &scope, OTHER_STATE).await;
    assert_eq!(
        untouched.state_seq, OTHER_SEQ,
        "the claim advances only the state it names"
    );
    assert_eq!(
        untouched.updated_at, OTHER_AT,
        "and never restamps another state"
    );
}

/// Claim and read both fail when the migration-seeded row is missing.
#[tokio::test]
async fn claiming_a_missing_entity_write_order_row_fails_closed() {
    use sea_orm::EntityTrait;
    use toolkit_db::secure::SecureDeleteExt;
    use types_registry::infra::storage::entity::coordination_state;

    let db = test_db().await;
    let conn = db.conn().expect("conn");
    let scope = allow_all();
    coordination_state::Entity::delete_many()
        .secure()
        .scope_with(&scope)
        .exec(&conn)
        .await
        .expect("remove the seeded row");

    let claim = CoordinationStateRepo::claim_entity_write_order(&conn, &scope, NOW).await;
    assert!(
        matches!(claim, Err(ScopeError::Invalid(_))),
        "an absent row must refuse the claim, not silently write nothing"
    );
    assert!(
        CoordinationStateRepo::entity_write_sequence(&conn, &scope)
            .await
            .is_err(),
        "the read fails closed the same way"
    );
}

#[tokio::test]
async fn exhausting_the_revalidation_budget_terminalizes_the_item_as_failed() {
    let dir = TestDir::new("types-registry-reval-exhausted");
    let db = test_db_file(&dir.path().join("registry.db")).await;
    seed_the_chain(&db).await;

    let operation_id = submit(&db, "k-ref-2", REFERRER, chained_schema("tag"), Some(1))
        .await
        .expect("acceptance");

    let mutating = Arc::clone(&db);
    let outcome = admit_with_a_mutation_in_the_gap(
        &db,
        WorkerSettings {
            max_revalidation_attempts: 1,
            ..WorkerSettings::default()
        },
        operation_id,
        move || async move {
            admit(&mutating, "k-base-2", BASE, base_schema("label"), Some(1)).await;
        },
    )
    .await;

    let item = &outcome.items[0];
    assert_eq!(
        item.status,
        OperationItemStatus::Failed,
        "one attempt and one drift is exhaustion, got {item:?}"
    );
    let failure = item.failure.as_ref().expect("a recorded failure");
    assert_eq!(
        failure.reason,
        AdmissionFailureReason::RevalidationExhausted
    );
    assert!(
        failure.message.contains(BASE),
        "the message names the last drift, got {}",
        failure.message
    );
    assert_eq!(
        entity(&db, REFERRER).await.resource_version,
        1,
        "an exhausted item wrote nothing"
    );
}

fn service(db: &Provider) -> RegistryService {
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(NoDispatch);
    RegistryService::new(
        db.db(),
        stores(),
        RegistrationPolicy::default(),
        TypesRegistryConfig::default(),
        dispatch,
        AdmissionMode::Inline,
        common::metrics(),
    )
}

#[tokio::test]
async fn a_commit_on_one_pod_is_visible_to_the_others_first_read() -> Result<(), ServiceError> {
    let dir = TestDir::new("types-registry-two-pods");
    let path = dir.path().join("registry.db");
    let pod_a = test_db_file(&path).await;
    let pod_b = test_db_file(&path).await;

    // B looks first, so its miss is a read it actually performed rather than an absence it never
    // asked about.
    let key = EntityKey::parse(BASE);
    assert!(
        service(&pod_b).entity(&key).await?.is_none(),
        "nothing is admitted yet"
    );

    admit(&pod_a, "k-base", BASE, base_schema("name"), None).await;

    let first_read = service(&pod_b)
        .entity(&key)
        .await?
        .expect("B's first read after A's commit must see it");
    assert_eq!(first_read.resource_version, 1);

    // And a revision on A is visible to B just the same: the read is a `SELECT`, not a snapshot, so
    // there is no second thing to invalidate.
    admit(&pod_a, "k-base-2", BASE, base_schema("label"), Some(1)).await;
    let second_read = service(&pod_b)
        .entity(&key)
        .await?
        .expect("the entity is still there");
    assert_eq!(second_read.resource_version, 2);
    assert_eq!(
        second_read.content,
        Some(base_schema("label")),
        "B reads A's newest authored document"
    );
    Ok(())
}
