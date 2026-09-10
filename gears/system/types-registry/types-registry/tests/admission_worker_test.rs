//! The admission worker on one dependency-free candidate (T8).
//!
//! Every test drives the worker by **calling it**. There is no `sleep`, no timer
//! and no polling anywhere in this file — which is the point of the worker being a
//! plain function of `(operation_id, runner)` rather than a task (SPEC §8.1, §13).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError, DbTx};
use toolkit_gts::gts_id;
use uuid::Uuid;

use types_registry::config::TypesRegistryConfig;
use types_registry::domain::admission::AdmissionFailureReason;
use types_registry::domain::admission::acceptance::{AcceptanceContext, AcceptanceError, accept};
use types_registry::domain::admission::unit::{commit_creation, evaluate};
use types_registry::domain::admission::worker::{Tuning, WorkerError, run_operation};
use types_registry::domain::admission::{Candidate, OperationDispatch, SubmitRequest};
use types_registry::domain::artifacts::resolution_fingerprint;
use types_registry::domain::policy::RegistrationPolicy;
// Both vocabularies appear here, and the distinction is the point of the port
// boundary: anything read through a repository or returned by the domain carries
// `domain_enums`, while a row read with a raw `SeaORM` query — which this file does
// where no repository method fits — carries the storage enums.
use types_registry::domain::enums as domain_enums;
use types_registry::infra::storage::entity::enums as storage_enums;
use types_registry::infra::storage::entity::{
    entity, operation, operation_item, type_schema, type_schema_revision, version_family,
};
use types_registry::infra::storage::repo::{EntityRepo, OperationRepo, TypeSchemaRepo};

mod common;
use common::{allow_all, stores, test_db};

const NOW: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const LATER: OffsetDateTime = datetime!(2026-08-18 10:20:40 UTC);
const CF_TYPE: &str = gts_id!("cf.core.example.type.v1~");

/// Enqueues nothing: T8 calls the worker directly, which is what SPEC §8.1's
/// plain-function shape is for. T21 replaces this with the real outbox.
struct NoDispatch;

#[async_trait::async_trait]
impl OperationDispatch for NoDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }
}

fn schema(gts_id: &str) -> Value {
    json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "name": { "type": "string" } },
    })
}

async fn submit(db: &Arc<DBProvider<DbError>>, key: &str, gts_id: &str, content: Value) -> Uuid {
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
                expected_resource_version: None,
                force: false,
            }],
        },
        NOW,
    )
    .await
    .expect("accepted")
    .operation_id
}

fn worker_provider(db: &Arc<DBProvider<DbError>>) -> DBProvider<WorkerError> {
    DBProvider::new(db.db())
}

// ---------------------------------------------------------------------------
// What one admission writes
// ---------------------------------------------------------------------------

/// The five affected tables, one row each. `dependency` is deliberately not among
/// them: this candidate references nothing, and edge extraction is T13.
#[tokio::test]
async fn admitting_a_schema_writes_one_row_in_each_affected_table() {
    let db = test_db().await;
    let operation_id = submit(&db, "k1", CF_TYPE, schema(CF_TYPE)).await;

    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("admission");

    assert!(!outcome.already_terminal);
    assert_eq!(outcome.items.len(), 1);
    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Succeeded);
    assert_eq!(item.gts_id, CF_TYPE);
    assert_eq!(item.revision_no, Some(1));
    assert_eq!(item.resource_version, Some(1));
    // The Registry Reference is `gts-rust`'s deterministic derivation, never a
    // locally invented UUID.
    assert_eq!(
        item.gts_uuid,
        Some(gts::GtsId::try_new(CF_TYPE).expect("identifier").to_uuid()),
    );

    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    let scope = allow_all();

    let families = version_family::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await
        .expect("families");
    assert_eq!(families.len(), 1);
    assert_eq!(families[0].family_key, "gts.cf.core.example.type");

    let entities = entity::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await
        .expect("entities");
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].gts_id, CF_TYPE);
    assert_eq!(entities[0].resource_version, 1);
    assert_eq!(entities[0].family_id, families[0].id);
    assert_eq!(entities[0].owning_gear.as_deref(), Some("types-registry"));

    let revisions = type_schema_revision::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await
        .expect("revisions");
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].revision_no, 1);
    assert!(!revisions[0].compat_forced);
    // Provenance is recorded even though this admission compared nothing: it
    // identifies the engine, and that cannot be reconstructed later (ADR-0003).
    assert_eq!(
        revisions[0].gts_spec_version,
        gts::GTS_SPECIFICATION_VERSION
    );
    assert_eq!(
        revisions[0].gts_impl_version,
        gts::GTS_IMPLEMENTATION_VERSION
    );

    let current = type_schema::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await
        .expect("current");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].revision_no, 1);
    // D3: the artifacts are materialized at admission, not on first read.
    assert!(current[0].resolved_schema.contains("\"name\""));
    assert!(!current[0].resolution_fingerprint.is_empty());
    assert_eq!(
        current[0].resolution_fingerprint,
        resolution_fingerprint(
            &current[0].resolved_schema,
            &current[0].effective_traits,
            &current[0].effective_traits_schema,
        ),
        "the stored digest must be the one its own artifacts produce",
    );

    let items = operation_item::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await
        .expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].status,
        storage_enums::OperationItemStatus::Succeeded
    );
    assert!(
        items[0].request_payload.is_none(),
        "the payload is dropped at terminality",
    );
    assert_eq!(items[0].result_revision_no, Some(1));
    assert_eq!(items[0].result_resource_version, Some(1));

    let ops = operation::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await
        .expect("operations");
    assert_eq!(ops[0].status, storage_enums::OperationStatus::Completed);
    assert!(ops[0].started_at.is_some());
    assert!(ops[0].completed_at.is_some());
}

/// The current-state row's digest is stable across two identical materializations,
/// which is what lets a recomputation that changes nothing change no read (D3).
#[tokio::test]
async fn the_resolution_fingerprint_is_stable_across_two_admissions_of_identical_content() {
    let first_db = test_db().await;
    let first = submit(&first_db, "k1", CF_TYPE, schema(CF_TYPE)).await;
    run_operation(
        &stores(),
        &worker_provider(&first_db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        first,
        LATER,
    )
    .await
    .expect("first admission");
    let first_provider = worker_provider(&first_db);
    let first_conn = first_provider.conn().expect("first conn");
    let first_fingerprint = type_schema::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&first_conn)
        .await
        .expect("first current")[0]
        .resolution_fingerprint
        .clone();

    // The same document in a different key order: canonicalization is what makes
    // the digest a property of the content rather than of the serialization.
    let reordered = json!({
        "properties": { "name": { "type": "string" } },
        "type": "object",
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": format!("gts://{CF_TYPE}"),
    });
    let second_db = test_db().await;
    let second = submit(&second_db, "k2", CF_TYPE, reordered).await;
    run_operation(
        &stores(),
        &worker_provider(&second_db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        second,
        LATER,
    )
    .await
    .expect("second admission");
    let second_provider = worker_provider(&second_db);
    let second_conn = second_provider.conn().expect("second conn");
    let second_fingerprint = type_schema::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&second_conn)
        .await
        .expect("second current")[0]
        .resolution_fingerprint
        .clone();

    assert_eq!(first_fingerprint, second_fingerprint);
}

// ---------------------------------------------------------------------------
// Failure is an outcome, not an error
// ---------------------------------------------------------------------------

/// An item's outcome is written once and stands. Two passes over one operation can
/// overlap — a retry under the same `Idempotency-Key` mid-flight, and at-least-once
/// delivery from T21 — and the loser must neither overwrite the outcome nor leave an
/// entity behind an item that says otherwise. Both halves: the item write is a CAS,
/// and losing it rolls the whole commit back.
#[tokio::test]
async fn a_pass_that_loses_the_item_cas_writes_nothing_at_all() {
    let db = test_db().await;
    let operation_id = submit(&db, "k1", CF_TYPE, schema(CF_TYPE)).await;
    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    let item = OperationRepo::find_items(&conn, &allow_all(), operation_id)
        .await
        .expect("items")[0]
        .clone();
    let payload = item.request_payload.clone().expect("payload");

    // This pass evaluates while the item is still pending, exactly as the loser of
    // an overlap does.
    let evaluated = evaluate(
        &stores(),
        &provider,
        &allow_all(),
        &item.gts_id,
        &payload,
        item.id,
        &common::limits(),
        None,
    )
    .await
    .expect("evaluation")
    .expect("the candidate is valid");
    let types_registry::domain::admission::unit::PreparedUnit::Evaluated(evaluated) = evaluated
    else {
        panic!("the probe was disabled");
    };

    // Meanwhile the other pass terminalizes the item.
    let recorded = provider
        .transaction(|tx| {
            Box::pin(async move {
                Ok(
                    types_registry::infra::storage::repo::OperationRepo::mark_item_failed(
                        tx,
                        &allow_all(),
                        item.id,
                        r#"{"reason":"already_exists"}"#.to_owned(),
                        LATER,
                    )
                    .await?,
                )
            })
        })
        .await
        .expect("the first write succeeds");
    assert!(recorded, "the winning pass records its outcome");

    // The loser's commit must roll back rather than commit an entity behind an item
    // that is already terminal.
    let stores = stores();
    let unit = evaluated.clone();
    let committed = provider
        .transaction(move |tx| {
            let unit = unit.clone();
            let stores = Arc::clone(&stores);
            Box::pin(async move {
                commit_creation(
                    stores.as_ref(),
                    tx,
                    &allow_all(),
                    &unit,
                    &common::limits(),
                    LATER,
                )
                .await
            })
        })
        .await;
    assert!(
        matches!(committed, Err(WorkerError::ItemAlreadyTerminal { .. })),
        "the losing pass must not commit",
    );

    for count in [
        entity::Entity::find()
            .secure()
            .scope_with(&allow_all())
            .all(&conn)
            .await
            .expect("entities")
            .len(),
        version_family::Entity::find()
            .secure()
            .scope_with(&allow_all())
            .all(&conn)
            .await
            .expect("families")
            .len(),
        type_schema_revision::Entity::find()
            .secure()
            .scope_with(&allow_all())
            .all(&conn)
            .await
            .expect("revisions")
            .len(),
    ] {
        assert_eq!(
            count, 0,
            "everything the rolled-back transaction wrote is gone"
        );
    }

    // And the winner's outcome is intact: the CAS refused the overwrite rather than
    // the second write silently landing.
    let after = OperationRepo::find_items(&conn, &allow_all(), operation_id)
        .await
        .expect("items");
    assert_eq!(after[0].status, domain_enums::OperationItemStatus::Failed);
    assert!(after[0].error_payload.is_some());
}

/// A redelivery reports the *same shape* of outcome the first pass returned, reason
/// and message in their own fields — not `reason: "recorded"` with the payload
/// stuffed into the message. T16 counts refusals by `reason`, and a metric that reads
/// `recorded` for every redelivered item counts nothing.
#[tokio::test]
async fn a_redelivered_failure_reports_the_reason_the_first_pass_recorded() {
    let db = test_db().await;
    let first = submit(&db, "k1", CF_TYPE, schema(CF_TYPE)).await;
    run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        first,
        LATER,
    )
    .await
    .expect("first admission");

    // A second operation for the same identifier fails with `already_exists`.
    let mut body = schema(CF_TYPE);
    body["title"] = json!("a second attempt");
    let second = submit(&db, "k2", CF_TYPE, body).await;
    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        second,
        LATER,
    )
    .await
    .expect("second admission");
    let first_pass = outcome.items[0].failure.clone().expect("a failure");

    // The redelivery: the operation is terminal, so this reports stored outcomes.
    let redelivered = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        second,
        LATER,
    )
    .await
    .expect("redelivery");
    assert!(redelivered.already_terminal);
    let replayed = redelivered.items[0].failure.clone().expect("a failure");

    assert_eq!(
        replayed, first_pass,
        "the two passes report one fact one way"
    );
    assert_eq!(replayed.reason, AdmissionFailureReason::AlreadyExists);
    assert!(
        !replayed.message.contains("reason"),
        "the payload must be parsed, not carried whole in the message: {}",
        replayed.message,
    );
}

/// A stored item that names a version is failed terminally and writes nothing.
///
/// Acceptance no longer produces such a row — it refuses a positive
/// `expected_resource_version` until T11 — so this manufactures one, which is also
/// the shape a row accepted by an earlier build has after an upgrade. What must not
/// happen: committing it as an ordinary creation at `resource_version = 1`, with the
/// policy gate never applied because the caller called it a revision.
#[tokio::test]
async fn an_item_naming_a_version_fails_terminally_and_writes_nothing() {
    let db = test_db().await;
    let operation_id = submit(&db, "k1", CF_TYPE, schema(CF_TYPE)).await;

    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    operation_item::Entity::update_many()
        .secure()
        .col_expr(
            operation_item::Column::ExpectedResourceVersion,
            Expr::value(7_i64),
        )
        .filter(Condition::all().add(operation_item::Column::OperationId.eq(operation_id)))
        .scope_with(&allow_all())
        .exec(&conn)
        .await
        .expect("name a version on the stored item");

    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().expect("a recorded failure").reason,
        AdmissionFailureReason::PreconditionFailed,
    );
    assert_eq!(item.resource_version, None);
    assert_eq!(item.revision_no, None);

    // Nothing was created: not the entity, and not the family that a creation
    // would have taken on its way in.
    let entities = entity::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("entities");
    assert!(entities.is_empty(), "no entity was registered");
    let families = version_family::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("families");
    assert!(families.is_empty(), "no family was created");

    let stored = OperationRepo::find_items(&conn, &allow_all(), operation_id)
        .await
        .expect("items");
    assert_eq!(
        stored[0].status,
        domain_enums::OperationItemStatus::Failed,
        "the outcome is recorded, so a redelivery does not retry it",
    );
    assert!(stored[0].error_payload.is_some(), "the reason is stored");
}

/// A creation against an existing identifier fails **terminally** — recorded on the
/// item, not returned as a worker error — and writes no second revision. The
/// recheck lives inside the commit transaction, so this is the same code path a
/// concurrent creation would hit.
#[tokio::test]
async fn a_creation_against_an_existing_identifier_fails_terminally_with_no_revision() {
    let db = test_db().await;
    let first = submit(&db, "k1", CF_TYPE, schema(CF_TYPE)).await;
    run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        first,
        LATER,
    )
    .await
    .expect("first admission");

    // A second *operation* for the same identifier: a different idempotency key and
    // a different body, so acceptance treats it as a fresh request.
    let mut second_body = schema(CF_TYPE);
    second_body["title"] = json!("a second attempt");
    let second = submit(&db, "k2", CF_TYPE, second_body).await;

    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        second,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");
    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    let failure = item.failure.as_ref().expect("a recorded failure");
    assert_eq!(failure.reason, AdmissionFailureReason::AlreadyExists);

    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    let revisions = type_schema_revision::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("revisions");
    assert_eq!(revisions.len(), 1, "no second revision was written");

    let items = OperationRepo::find_items(&conn, &allow_all(), second)
        .await
        .expect("items");
    assert_eq!(items[0].status, domain_enums::OperationItemStatus::Failed);
    assert!(items[0].error_payload.is_some(), "the reason is stored");
    assert!(items[0].request_payload.is_none());
}

/// A candidate whose reference cannot be resolved is a candidate-level failure, not
/// a worker error: retrying it would produce the same answer forever.
#[tokio::test]
async fn an_unresolvable_reference_is_an_item_failure_not_a_worker_error() {
    let db = test_db().await;
    let dangling = json!({
        "$id": format!("gts://{CF_TYPE}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "allOf": [{ "$ref": "gts://gts.cf.core.absent.type.v1~" }],
    });
    let operation_id = submit(&db, "k1", CF_TYPE, dangling).await;

    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");
    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().expect("failure").reason,
        AdmissionFailureReason::InvalidSchema,
    );

    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    assert!(
        EntityRepo::find_by_gts_id(&conn, &allow_all(), CF_TYPE)
            .await
            .expect("read")
            .is_none(),
        "a failed candidate writes no entity",
    );
}

// ---------------------------------------------------------------------------
// Nothing is retained between invocations
// ---------------------------------------------------------------------------

/// A second invocation sees a revision the first one committed. There is no
/// invalidation step, no rebuild hook and nothing to notify: the store is built
/// from the database inside each unit and dropped with it.
#[tokio::test]
async fn a_second_invocation_sees_the_first_ones_committed_revision() {
    let db = test_db().await;
    let base = gts_id!("cf.core.base.type.v1~");
    let first = submit(&db, "k1", base, schema(base)).await;
    run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        first,
        LATER,
    )
    .await
    .expect("first admission");

    // A candidate that can only resolve if the first admission is visible.
    let derived = gts_id!("cf.core.base.type.v1~cf.core.ns.premium.v1~");
    let derived_body = json!({
        "$id": format!("gts://{derived}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "allOf": [
            { "$ref": format!("gts://{base}") },
            { "type": "object", "properties": { "tier": { "type": "string" } } },
        ],
    });
    let second = submit(&db, "k2", derived, derived_body).await;

    // Inverted at T10: the base is reachable through `GtsId::chain_ids()` with the
    // edge table still empty, so the old comment blaming T13's missing rows was half
    // wrong. The `$ref` here points at the base, which the chain supplies;
    // `a_ref_outside_the_chain_is_admitted` covers the half T13 owned.
    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        second,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");
    let item = &outcome.items[0];
    assert_eq!(
        item.status,
        domain_enums::OperationItemStatus::Succeeded,
        "the chain seed must reach the committed base with no dependency row: {:?}",
        item.failure,
    );

    // What *is* provable now: a fresh invocation reads the committed state rather
    // than any carried-over copy.
    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    let base_row = EntityRepo::find_by_gts_id(&conn, &allow_all(), base)
        .await
        .expect("read")
        .expect("the base committed in the first invocation");
    let documents = TypeSchemaRepo::current_documents(&conn, &allow_all(), &[base_row.id])
        .await
        .expect("documents");
    assert_eq!(documents.len(), 1);
    assert!(documents[0].raw_schema.contains("\"name\""));
}

/// A redelivered message finds the operation terminal and does nothing: delivery is
/// at-least-once, so this is what makes a duplicate a no-op rather than a second
/// admission.
#[tokio::test]
async fn a_second_pass_over_a_completed_operation_is_a_no_op() {
    let db = test_db().await;
    let operation_id = submit(&db, "k1", CF_TYPE, schema(CF_TYPE)).await;
    let first = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("first pass");
    assert!(!first.already_terminal);

    let second = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("second pass");
    assert!(second.already_terminal);
    assert_eq!(second.items.len(), 1);
    assert_eq!(
        second.items[0].status,
        domain_enums::OperationItemStatus::Succeeded
    );

    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    let revisions = type_schema_revision::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("revisions");
    assert_eq!(
        revisions.len(),
        1,
        "a duplicate delivery writes nothing new"
    );
}

/// An operation UUID nothing wrote is a worker error, not a silent success — the
/// outbox must be able to tell a missing operation from an admitted one.
#[tokio::test]
async fn an_unknown_operation_is_an_error() {
    let db = test_db().await;
    let err = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        Uuid::new_v4(),
        LATER,
    )
    .await
    .expect_err("an unknown operation must not look like success");
    assert!(
        matches!(err, WorkerError::OperationNotFound { .. }),
        "got {err}"
    );
}

/// Evaluation happens with no transaction open, and the transaction holds only the
/// rechecks and the writes. Observable through its consequence: an item whose
/// *evaluation* fails leaves the operation completed and the item terminal, with no
/// partial write anywhere — a validation that had opened a transaction would have
/// had one to roll back.
#[tokio::test]
async fn a_failed_evaluation_leaves_no_partial_write() {
    let db = test_db().await;
    let bad = json!({
        "$id": format!("gts://{CF_TYPE}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "not-a-json-schema-type",
    });
    let operation_id = submit(&db, "k1", CF_TYPE, bad).await;
    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");
    assert_eq!(
        outcome.items[0].status,
        domain_enums::OperationItemStatus::Failed
    );

    let provider = worker_provider(&db);
    let conn = provider.conn().expect("conn");
    for count in [
        version_family::Entity::find()
            .secure()
            .scope_with(&allow_all())
            .all(&conn)
            .await
            .expect("families")
            .len(),
        entity::Entity::find()
            .secure()
            .scope_with(&allow_all())
            .all(&conn)
            .await
            .expect("entities")
            .len(),
        type_schema::Entity::find()
            .secure()
            .scope_with(&allow_all())
            .all(&conn)
            .await
            .expect("current")
            .len(),
    ] {
        assert_eq!(count, 0, "a failed evaluation writes nothing");
    }

    // The operation still reaches terminality: every item is terminal, which is
    // what `completed` means (`database.sql`).
    let ops = operation::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("operations");
    assert_eq!(ops[0].status, storage_enums::OperationStatus::Completed);
}

#[tokio::test]
async fn a_ref_outside_the_chain_is_admitted() {
    let db = test_db().await;

    // A committed type that is *not* an ancestor of the candidate.
    let unrelated = gts_id!("cf.core.other.type.v1~");
    let first = submit(&db, "k1", unrelated, schema(unrelated)).await;
    run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        first,
        LATER,
    )
    .await
    .expect("the unrelated type admits");

    let candidate = gts_id!("cf.core.base.type.v1~");
    let body = json!({
        "$id": format!("gts://{candidate}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "other": { "$ref": format!("gts://{unrelated}") } },
    });
    let second = submit(&db, "k2", candidate, body).await;

    let outcome = run_operation(
        &stores(),
        &worker_provider(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        second,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");
    let item = &outcome.items[0];
    assert_eq!(
        item.status,
        domain_enums::OperationItemStatus::Succeeded,
        "a cross-chain $ref to a committed schema must resolve: {:?}",
        item.failure,
    );
}
