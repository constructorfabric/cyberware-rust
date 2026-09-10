//! Content revisions and compare-and-swap.
//!
//! Every test calls the worker directly, as `admission_worker_test.rs` does: no
//! `sleep`, no timer, no polling (SPEC §13).
//!
//! The four questions this file answers, and they are separate questions:
//!
//! 1. a second revision advances `resource_version` and moves the current pointer;
//! 2. a stale `expected_resource_version` is terminal `precondition_failed`, with
//!    no silent rebase;
//! 3. content equal to the **current** revision terminates `unchanged`, writing no
//!    revision and moving no version — while content equal to an *older* revision
//!    is an ordinary update (ADR-0005);
//! 4. the registration policy gates creations only, so closing a region cannot
//!    freeze the entities already in it (SPEC §8.1 step 3).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::SecureEntityExt;
use toolkit_db::{DBProvider, DbError, DbTx};
use toolkit_gts::gts_id;
use uuid::Uuid;

use types_registry::config::{PolicyEntry, TypesRegistryConfig};
use types_registry::domain::admission::AdmissionFailureReason;
use types_registry::domain::admission::acceptance::{AcceptanceContext, AcceptanceError, accept};
use types_registry::domain::admission::worker::{
    OperationOutcome, Tuning, WorkerError, run_operation,
};
use types_registry::domain::admission::{Candidate, OperationDispatch, SubmitRequest};
use types_registry::domain::enums as domain_enums;
use types_registry::domain::policy::RegistrationPolicy;
use types_registry::infra::storage::entity::{
    instance, instance_revision, type_schema, type_schema_revision,
};
use types_registry::infra::storage::repo::EntityRepo;

mod common;
use common::{allow_all, stores, test_db};

const NOW: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const LATER: OffsetDateTime = datetime!(2026-08-18 10:20:40 UTC);

const CF_TYPE: &str = gts_id!("cf.core.example.type.v1~");
const CF_INSTANCE: &str = gts_id!("cf.core.example.type.v1~cf.core.example.first.v1");
/// A vendor the closed default refuses, so a policy can be narrowed under it.
const ACME_TYPE: &str = gts_id!("acme.crm.customer.type.v1~");

struct NoDispatch;

#[async_trait::async_trait]
impl OperationDispatch for NoDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A schema whose `title` is the knob every content-equality test turns.
fn schema(gts_id: &str, title: &str) -> Value {
    json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": title,
        "type": "object",
        "properties": { "name": { "type": "string" } },
    })
}

fn permissive_policy() -> RegistrationPolicy {
    let mut map: BTreeMap<String, PolicyEntry> = BTreeMap::new();
    map.insert(
        gts_id!("acme.crm.customer.type.v1~").to_owned(),
        PolicyEntry {
            allowed_vendors: Some(vec!["acme".to_owned()]),
            tenant_ownable: None,
        },
    );
    RegistrationPolicy::compile(&map).expect("the fixture policy compiles")
}

/// Submit one candidate under an explicit policy and precondition.
async fn submit_with(
    db: &Arc<DBProvider<DbError>>,
    policy: &RegistrationPolicy,
    key: &str,
    gts_id: &str,
    content: Value,
    expected_resource_version: Option<i64>,
) -> Result<Uuid, AcceptanceError> {
    let provider: DBProvider<AcceptanceError> = DBProvider::new(db.db());
    let config = TypesRegistryConfig::default();
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(NoDispatch);
    accept(
        &stores(),
        &provider,
        &allow_all(),
        &AcceptanceContext {
            policy,
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

fn worker(db: &Arc<DBProvider<DbError>>) -> DBProvider<WorkerError> {
    DBProvider::new(db.db())
}

/// Submit under the default policy and run one full admission pass.
async fn admit(
    db: &Arc<DBProvider<DbError>>,
    key: &str,
    gts_id: &str,
    content: Value,
    expected_resource_version: Option<i64>,
) -> OperationOutcome {
    let policy = RegistrationPolicy::default();
    let op = submit_with(db, &policy, key, gts_id, content, expected_resource_version)
        .await
        .expect("accepted");
    run_operation(
        &stores(),
        &worker(db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        op,
        LATER,
    )
    .await
    .expect("the worker itself must not fail")
}

/// Every `type_schema_revision` row of one entity, in revision order.
async fn schema_revisions(
    db: &Arc<DBProvider<DbError>>,
    entity_id: i64,
) -> Vec<type_schema_revision::Model> {
    let provider = worker(db);
    let conn = provider.conn().expect("conn");
    type_schema_revision::Entity::find()
        .filter(type_schema_revision::Column::EntityId.eq(entity_id))
        .order_by_asc(type_schema_revision::Column::RevisionNo)
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("revisions")
}

async fn entity_id_of(db: &Arc<DBProvider<DbError>>, gts_id: &str) -> i64 {
    let provider = worker(db);
    let conn = provider.conn().expect("conn");
    EntityRepo::find_by_gts_id(&conn, &allow_all(), gts_id)
        .await
        .expect("read")
        .expect("the entity exists")
        .id
}

async fn resource_version_of(db: &Arc<DBProvider<DbError>>, gts_id: &str) -> i64 {
    let provider = worker(db);
    let conn = provider.conn().expect("conn");
    EntityRepo::find_by_gts_id(&conn, &allow_all(), gts_id)
        .await
        .expect("read")
        .expect("the entity exists")
        .resource_version
}

// ---------------------------------------------------------------------------
// A second revision
// ---------------------------------------------------------------------------

/// The whole of criterion 2: the precondition is met, so a new immutable revision
/// is inserted, the current pointer moves to it, and `resource_version` advances.
#[tokio::test]
async fn a_met_precondition_admits_a_second_revision_and_moves_the_pointer() {
    let db = test_db().await;
    admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "first"), None).await;

    let outcome = admit(&db, "k2", CF_TYPE, schema(CF_TYPE, "second"), Some(1)).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Succeeded);
    assert_eq!(item.revision_no, Some(2));
    assert_eq!(item.resource_version, Some(2));

    let entity_id = entity_id_of(&db, CF_TYPE).await;
    let revisions = schema_revisions(&db, entity_id).await;
    assert_eq!(
        revisions.len(),
        2,
        "the first revision is retained, not overwritten",
    );
    assert!(revisions[0].raw_schema.contains("first"));
    assert!(revisions[1].raw_schema.contains("second"));

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let current = type_schema::Entity::find()
        .filter(type_schema::Column::EntityId.eq(entity_id))
        .secure()
        .scope_with(&allow_all())
        .one(&conn)
        .await
        .expect("current row")
        .expect("a current row exists");
    assert_eq!(current.revision_no, 2, "the pointer moved");
    assert!(
        current.resolved_schema.contains("second"),
        "the artifacts were re-materialized from the new document: {}",
        current.resolved_schema,
    );
}

/// Contiguity: revision numbers are `1, 2, 3` per entity, allocated only by an
/// admitted revision.
#[tokio::test]
async fn revision_numbers_are_contiguous_per_entity() {
    let db = test_db().await;
    admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "one"), None).await;
    admit(&db, "k2", CF_TYPE, schema(CF_TYPE, "two"), Some(1)).await;
    admit(&db, "k3", CF_TYPE, schema(CF_TYPE, "three"), Some(2)).await;

    let entity_id = entity_id_of(&db, CF_TYPE).await;
    let numbers: Vec<i32> = schema_revisions(&db, entity_id)
        .await
        .iter()
        .map(|r| r.revision_no)
        .collect();
    assert_eq!(numbers, vec![1, 2, 3]);
    assert_eq!(resource_version_of(&db, CF_TYPE).await, 3);
}

/// A Registered Instance revises the same way, and its revision re-records the
/// schema revision that validated the new value.
#[tokio::test]
async fn an_instance_revises_and_re_records_its_schema_revision() {
    let db = test_db().await;
    admit(&db, "type", CF_TYPE, schema(CF_TYPE, "t"), None).await;
    admit(&db, "i1", CF_INSTANCE, json!({ "name": "first" }), None).await;

    let revised_type = admit(&db, "type-2", CF_TYPE, schema(CF_TYPE, "revised"), Some(1)).await;
    assert_eq!(revised_type.items[0].revision_no, Some(2));

    let outcome = admit(&db, "i2", CF_INSTANCE, json!({ "name": "second" }), Some(1)).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Succeeded);
    assert_eq!(item.revision_no, Some(2));
    assert_eq!(item.resource_version, Some(2));

    let entity_id = entity_id_of(&db, CF_INSTANCE).await;
    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let revisions = instance_revision::Entity::find()
        .filter(instance_revision::Column::EntityId.eq(entity_id))
        .order_by_asc(instance_revision::Column::RevisionNo)
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("revisions");
    assert_eq!(revisions.len(), 2);
    assert!(revisions[1].canonical_value.contains("second"));
    assert_eq!(
        revisions[1].type_schema_revision_no, 2,
        "the changed value records the revised conforming schema",
    );

    let current = instance::Entity::find()
        .filter(instance::Column::EntityId.eq(entity_id))
        .secure()
        .scope_with(&allow_all())
        .one(&conn)
        .await
        .expect("current row")
        .expect("a current row exists");
    assert_eq!(current.revision_no, 2);
}

// ---------------------------------------------------------------------------
// The precondition
// ---------------------------------------------------------------------------

/// A stale precondition is terminal and writes nothing — no silent rebase onto
/// whatever the current version happens to be.
#[tokio::test]
async fn a_stale_expected_resource_version_fails_terminally_and_writes_nothing() {
    let db = test_db().await;
    admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "first"), None).await;
    admit(&db, "k2", CF_TYPE, schema(CF_TYPE, "second"), Some(1)).await;

    // The caller still believes it is at version 1.
    let outcome = admit(&db, "k3", CF_TYPE, schema(CF_TYPE, "third"), Some(1)).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().expect("a recorded failure").reason,
        AdmissionFailureReason::PreconditionFailed,
    );
    assert_eq!(item.revision_no, None);

    let entity_id = entity_id_of(&db, CF_TYPE).await;
    assert_eq!(
        schema_revisions(&db, entity_id).await.len(),
        2,
        "the losing revision wrote nothing",
    );
    assert_eq!(resource_version_of(&db, CF_TYPE).await, 2);
}

/// A precondition naming an entity that does not exist is the same refusal, and
/// it must not fall back to creating one.
#[tokio::test]
async fn a_precondition_on_an_absent_entity_is_refused_rather_than_created() {
    let db = test_db().await;
    let outcome = admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "first"), Some(1)).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().expect("a recorded failure").reason,
        AdmissionFailureReason::PreconditionFailed,
    );

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    assert!(
        EntityRepo::find_by_gts_id(&conn, &allow_all(), CF_TYPE)
            .await
            .expect("read")
            .is_none(),
        "a refused revision must not create the entity it named",
    );
}

/// A tombstoned entity refuses a revision, and says so rather than reporting a
/// version.
///
/// `commit_revision` is the only write path that reaches a `DELETED` row. Without
/// the lifecycle check the caller's content would become the current state of a
/// withdrawn entity, with `resource_version` advancing behind it.
#[tokio::test]
async fn a_revision_is_refused_on_a_tombstoned_entity() {
    let db = test_db().await;
    admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "first"), None).await;

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let entity = EntityRepo::find_by_gts_id(&conn, &allow_all(), CF_TYPE)
        .await
        .expect("read")
        .expect("admitted");
    assert_eq!(
        EntityRepo::mark_deleted(
            &conn,
            &allow_all(),
            entity.id,
            entity.resource_version,
            LATER
        )
        .await
        .expect("tombstone the entity"),
        Some(entity.resource_version + 1),
        "the fixture must really produce a tombstone"
    );
    let after_delete = resource_version_of(&db, CF_TYPE).await;

    // The precondition names the version the deletion left behind, so nothing but
    // the lifecycle can refuse this.
    let outcome = admit(
        &db,
        "k2",
        CF_TYPE,
        schema(CF_TYPE, "second"),
        Some(after_delete),
    )
    .await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().expect("a recorded failure").reason,
        AdmissionFailureReason::EntityDeleted,
        "a withdrawn entity is not a stale version, and must not be reported as one",
    );
    assert_eq!(item.revision_no, None);

    let entity_id = entity_id_of(&db, CF_TYPE).await;
    assert_eq!(
        schema_revisions(&db, entity_id).await.len(),
        1,
        "the refused revision wrote nothing",
    );
    assert_eq!(resource_version_of(&db, CF_TYPE).await, after_delete);
}

// ---------------------------------------------------------------------------
// `unchanged`
// ---------------------------------------------------------------------------

/// Equal authored content creates no revision and does not advance
/// `resource_version` (ADR-0005).
#[tokio::test]
async fn content_equal_to_the_current_revision_is_unchanged() {
    let db = test_db().await;
    admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "first"), None).await;

    let outcome = admit(&db, "k2", CF_TYPE, schema(CF_TYPE, "first"), Some(1)).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Unchanged);
    assert_eq!(
        item.revision_no, None,
        "an unchanged candidate consumes no revision number",
    );
    assert_eq!(item.resource_version, Some(1), "the version did not move");

    let entity_id = entity_id_of(&db, CF_TYPE).await;
    assert_eq!(schema_revisions(&db, entity_id).await.len(), 1);
    assert_eq!(resource_version_of(&db, CF_TYPE).await, 1);
}

/// The same for a Registered Instance: the kinds share the rule, not the tables.
#[tokio::test]
async fn an_instance_value_equal_to_its_current_revision_is_unchanged() {
    let db = test_db().await;
    admit(&db, "type", CF_TYPE, schema(CF_TYPE, "t"), None).await;
    admit(&db, "i1", CF_INSTANCE, json!({ "name": "first" }), None).await;

    let outcome = admit(&db, "i2", CF_INSTANCE, json!({ "name": "first" }), Some(1)).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Unchanged);
    assert_eq!(item.revision_no, None);
    assert_eq!(item.resource_version, Some(1));
    assert_eq!(resource_version_of(&db, CF_INSTANCE).await, 1);
}

/// Equality is about authored content, even when the conforming schema changed
/// and would reject that old value if it were submitted as a new revision.
#[tokio::test]
async fn unchanged_instance_is_not_revalidated_against_a_new_conforming_schema() {
    let db = test_db().await;
    admit(&db, "type", CF_TYPE, schema(CF_TYPE, "t"), None).await;
    admit(&db, "value", CF_INSTANCE, json!({ "name": "first" }), None).await;
    let mut revised = schema(CF_TYPE, "numeric-name");
    revised["properties"]["name"]["type"] = json!("integer");
    let changed_type = admit(&db, "type-revised", CF_TYPE, revised, Some(1)).await;
    assert_eq!(
        changed_type.items[0].status,
        domain_enums::OperationItemStatus::Succeeded
    );

    let outcome = admit(
        &db,
        "same",
        CF_INSTANCE,
        json!({ "name": "first" }),
        Some(1),
    )
    .await;
    let item = &outcome.items[0];
    assert_eq!(
        item.status,
        domain_enums::OperationItemStatus::Unchanged,
        "{item:?}"
    );
    assert_eq!(item.resource_version, Some(1));
    assert_eq!(item.revision_no, None);
    let id = entity_id_of(&db, CF_INSTANCE).await;
    let revisions = instance_revision::Entity::find()
        .filter(instance_revision::Column::EntityId.eq(id))
        .secure()
        .scope_with(&allow_all())
        .all(&db.conn().expect("conn"))
        .await
        .expect("revisions");
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].type_schema_revision_no, 1);
}

/// ADR-0005: content equal to an **older, non-current** revision is a new update.
/// It allocates a new revision rather than moving the current pointer backwards.
#[tokio::test]
async fn content_equal_to_an_older_revision_creates_a_new_revision() {
    let db = test_db().await;
    admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "first"), None).await;
    admit(&db, "k2", CF_TYPE, schema(CF_TYPE, "second"), Some(1)).await;

    // Back to revision 1's content, which is no longer current.
    let outcome = admit(&db, "k3", CF_TYPE, schema(CF_TYPE, "first"), Some(2)).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Succeeded);
    assert_eq!(item.revision_no, Some(3), "forwards, never backwards");
    assert_eq!(item.resource_version, Some(3));

    let entity_id = entity_id_of(&db, CF_TYPE).await;
    let revisions = schema_revisions(&db, entity_id).await;
    assert_eq!(revisions.len(), 3);
    assert_eq!(
        revisions[0].content_hash, revisions[2].content_hash,
        "the fixture really is the same content under a new revision number",
    );
}

/// `unchanged` is unreachable for a creation: a second creation of an existing
/// identifier is `already_exists`, whatever its content.
#[tokio::test]
async fn a_creation_of_existing_content_is_already_exists_and_never_unchanged() {
    let db = test_db().await;
    admit(&db, "k1", CF_TYPE, schema(CF_TYPE, "first"), None).await;

    let outcome = admit(&db, "k2", CF_TYPE, schema(CF_TYPE, "first"), None).await;

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().expect("a recorded failure").reason,
        AdmissionFailureReason::AlreadyExists,
    );
}

// ---------------------------------------------------------------------------
// The policy gate applies to creations only
// ---------------------------------------------------------------------------

/// SPEC §8.1 step 3: closing a region stops new entities appearing in it and does
/// **not** freeze the entities already there. The pair is one test because it is
/// one rule seen from both sides.
#[tokio::test]
async fn a_revision_survives_a_region_the_policy_has_since_closed() {
    let db = test_db().await;
    let open = permissive_policy();
    let closed = RegistrationPolicy::default();

    let created = submit_with(
        &db,
        &open,
        "k1",
        ACME_TYPE,
        schema(ACME_TYPE, "first"),
        None,
    )
    .await
    .expect("the open policy admits the creation");
    run_operation(
        &stores(),
        &worker(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        created,
        LATER,
    )
    .await
    .expect("admission");

    // The region is closed from here on.
    let outcome = {
        let op = submit_with(
            &db,
            &closed,
            "k2",
            ACME_TYPE,
            schema(ACME_TYPE, "second"),
            Some(1),
        )
        .await
        .expect("a revision bypasses the policy gate");
        run_operation(
            &stores(),
            &worker(&db),
            &allow_all(),
            Tuning {
                limits: &common::limits(),
                worker: &common::worker_settings(),
                metrics: &common::metrics(),
            },
            op,
            LATER,
        )
        .await
        .expect("admission")
    };
    assert_eq!(
        outcome.items[0].status,
        domain_enums::OperationItemStatus::Succeeded,
    );
    assert_eq!(outcome.items[0].revision_no, Some(2));

    // A *creation* in the same closed region is still refused, synchronously.
    let refused = submit_with(
        &db,
        &closed,
        "k3",
        gts_id!("acme.crm.other.type.v1~"),
        schema(gts_id!("acme.crm.other.type.v1~"), "new"),
        None,
    )
    .await
    .expect_err("a creation in a closed region is refused");
    assert!(
        matches!(refused, AcceptanceError::PolicyRefused(_)),
        "expected a policy refusal, got {refused:?}",
    );
}
