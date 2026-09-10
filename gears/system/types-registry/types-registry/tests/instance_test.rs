//! Registered Instances end to end through the admission worker (T10).
//!
//! Every test calls the worker directly, as `admission_worker_test.rs` does: no
//! `sleep`, no timer, no polling (SPEC §13).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::SecureEntityExt;
use toolkit_db::{DBProvider, DbError, DbTx};
use toolkit_gts::gts_id;
use uuid::Uuid;

use types_registry::config::TypesRegistryConfig;
use types_registry::domain::admission::AdmissionFailureReason;
use types_registry::domain::admission::acceptance::{AcceptanceContext, AcceptanceError, accept};
use types_registry::domain::admission::worker::{Tuning, WorkerError, run_operation};
use types_registry::domain::admission::{Candidate, OperationDispatch, SubmitRequest};
use types_registry::domain::enums as domain_enums;
use types_registry::domain::policy::RegistrationPolicy;
use types_registry::infra::storage::entity::enums as storage_enums;
use types_registry::infra::storage::entity::{instance, instance_revision, version_family};
use types_registry::infra::storage::repo::EntityRepo;

mod common;
use common::{allow_all, stores, test_db};

const NOW: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const LATER: OffsetDateTime = datetime!(2026-08-18 10:20:40 UTC);

/// Conforming type with a dedicated family key to avoid cross-process lock contention.
const TYPE_ID: &str = gts_id!("cf.core.inst.thing.v1~");
/// An Instance of it: a full five-token last segment with no `~`.
const INSTANCE_ID: &str = gts_id!("cf.core.inst.thing.v1~cf.core.inst.first.v1");

struct NoDispatch;

#[async_trait::async_trait]
impl OperationDispatch for NoDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }
}

fn conforming_schema() -> Value {
    json!({
        "$id": format!("gts://{TYPE_ID}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"],
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

fn worker(db: &Arc<DBProvider<DbError>>) -> DBProvider<WorkerError> {
    DBProvider::new(db.db())
}

/// Admit the conforming type, then run one operation and return its outcome.
async fn admit_type_then(
    db: &Arc<DBProvider<DbError>>,
    key: &str,
    gts_id: &str,
    content: Value,
) -> Result<types_registry::domain::admission::worker::OperationOutcome, WorkerError> {
    let type_op = submit(db, "type-key", TYPE_ID, conforming_schema()).await;
    run_operation(
        &stores(),
        &worker(db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        type_op,
        LATER,
    )
    .await
    .expect("the conforming type admits");

    let op = submit(db, key, gts_id, content).await;
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
}

/// An Instance records the exact schema revision that validated it; its current row
/// is a pointer and nothing else.
#[tokio::test]
async fn an_instance_records_the_schema_revision_that_validated_it() {
    let db = test_db().await;
    let outcome = admit_type_then(&db, "k1", INSTANCE_ID, json!({ "name": "first" }))
        .await
        .expect("the worker itself must not fail");

    let item = &outcome.items[0];
    assert_eq!(
        item.status,
        domain_enums::OperationItemStatus::Succeeded,
        "{:?}",
        item.failure,
    );

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let scope = allow_all();

    let type_row = EntityRepo::find_by_gts_id(&conn, &scope, TYPE_ID)
        .await
        .expect("read")
        .expect("the type committed");
    let instance_row = EntityRepo::find_by_gts_id(&conn, &scope, INSTANCE_ID)
        .await
        .expect("read")
        .expect("the instance committed");

    // The entity kind is derived from the identifier, never passed in.
    assert_eq!(instance_row.entity_kind, domain_enums::EntityKind::Instance);
    assert_eq!(type_row.entity_kind, domain_enums::EntityKind::TypeSchema);

    let revisions = instance_revision::Entity::find()
        .filter(instance_revision::Column::EntityId.eq(instance_row.id))
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await
        .expect("revisions");
    assert_eq!(revisions.len(), 1);
    let revision = &revisions[0];
    assert_eq!(revision.revision_no, 1);
    assert_eq!(
        revision.type_schema_entity_id, type_row.id,
        "the recorded schema entity must be the conforming type",
    );
    assert_eq!(
        revision.type_schema_revision_no, 1,
        "the conforming type's current revision at validation time",
    );
    assert_eq!(
        revision.canonical_value, r#"{"name":"first"}"#,
        "the canonical authored value must be stored exactly",
    );

    // A pointer and nothing more: no artifact, no fingerprint.
    let current = instance::Entity::find()
        .filter(instance::Column::EntityId.eq(instance_row.id))
        .secure()
        .scope_with(&scope)
        .one(&conn)
        .await
        .expect("current")
        .expect("the current pointer");
    assert_eq!(current.revision_no, 1);
}

/// A value violating its schema is a content failure on the item, not a worker
/// fault: retrying would give the same answer forever.
#[tokio::test]
async fn a_value_violating_its_schema_is_refused_on_its_merits() {
    let db = test_db().await;
    // `name` is required by the conforming type, and this value omits it.
    let outcome = admit_type_then(&db, "k1", INSTANCE_ID, json!({ "nickname": "wrong" }))
        .await
        .expect("the worker itself must not fail");

    let item = &outcome.items[0];
    assert_eq!(
        item.status,
        domain_enums::OperationItemStatus::Failed,
        "a value that does not satisfy its type must not commit",
    );
    assert_eq!(
        item.failure.as_ref().map(|f| f.reason.clone()),
        Some(AdmissionFailureReason::InvalidValue),
        "distinct from `invalid_schema`: the schema is fine, the value is not",
    );

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    assert!(
        EntityRepo::find_by_gts_id(&conn, &allow_all(), INSTANCE_ID)
            .await
            .expect("read")
            .is_none(),
        "nothing is committed for a refused value",
    );
}

/// An Instance whose conforming type has no committed revision fails **retryably**.
/// Until T21 there is no outbox, so it is asserted as the `WorkerError` it is.
#[tokio::test]
async fn an_instance_without_its_type_fails_retryably() {
    let db = test_db().await;
    let op = submit(&db, "k1", INSTANCE_ID, json!({ "name": "orphan" })).await;

    let err = run_operation(
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
    .expect_err("an absent conforming type is retryable, so it surfaces as an error");

    match err {
        WorkerError::ConformingTypeAbsent { gts_id, type_id } => {
            assert_eq!(gts_id, INSTANCE_ID);
            assert_eq!(
                type_id, TYPE_ID,
                "the type named is the identifier's prefix"
            );
        }
        other => panic!("expected ConformingTypeAbsent, got {other:?}"),
    }
}

/// **A family holds one kind.** `family_key` drops the trailing `~`, so `…thing.v1~`
/// and `…thing.v1` share a key. Refused as `family_kind_conflict`, not
/// `already_exists`: the identifier is free.
#[tokio::test]
async fn an_instance_may_not_join_a_type_schema_family() {
    let db = test_db().await;

    // A Type Schema and an Instance that share a `family_key`: the schema is
    // `…thing.v1~` and the instance is the same identifier without the terminator.
    let schema_id = gts_id!("cf.core.inst.thing.v1~cf.core.inst.first.v1~");
    let instance_id = gts_id!("cf.core.inst.thing.v1~cf.core.inst.first.v1");

    let type_op = submit(&db, "type-key", TYPE_ID, conforming_schema()).await;
    run_operation(
        &stores(),
        &worker(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        type_op,
        LATER,
    )
    .await
    .expect("the base type admits");

    let derived = json!({
        "$id": format!("gts://{schema_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "allOf": [{ "$ref": format!("gts://{TYPE_ID}") }],
    });
    let schema_op = submit(&db, "k-schema", schema_id, derived).await;
    let schema_outcome = run_operation(
        &stores(),
        &worker(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        schema_op,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");
    assert_eq!(
        schema_outcome.items[0].status,
        domain_enums::OperationItemStatus::Succeeded,
        "the derived Type Schema founds the family: {:?}",
        schema_outcome.items[0].failure,
    );

    // Same family key, other kind.
    let instance_op = submit(&db, "k-instance", instance_id, json!({ "name": "clash" })).await;
    let outcome = run_operation(
        &stores(),
        &worker(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        instance_op,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");

    let item = &outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().map(|f| f.reason.clone()),
        Some(AdmissionFailureReason::FamilyKindConflict),
        "not `already_exists`: the identifier is free, the family is the conflict",
    );

    // Exactly one family row, and it still holds only the schema.
    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let families = version_family::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("families");
    let shared = families
        .iter()
        .filter(|f| f.family_key.ends_with("inst.first"))
        .count();
    assert_eq!(shared, 1, "one family key, not two: {families:?}");
}

/// The reverse order, asserted separately: a rule that holds only in the order it
/// was written is not the rule.
#[tokio::test]
async fn a_type_schema_may_not_join_an_instance_family() {
    let db = test_db().await;
    let instance_id = gts_id!("cf.core.inst.thing.v1~cf.core.inst.first.v1");
    let schema_id = gts_id!("cf.core.inst.thing.v1~cf.core.inst.first.v1~");

    let outcome = admit_type_then(&db, "k-instance", instance_id, json!({ "name": "first" }))
        .await
        .expect("the worker itself must not fail");
    assert_eq!(
        outcome.items[0].status,
        domain_enums::OperationItemStatus::Succeeded,
        "the Instance founds the family: {:?}",
        outcome.items[0].failure,
    );

    let derived = json!({
        "$id": format!("gts://{schema_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "allOf": [{ "$ref": format!("gts://{TYPE_ID}") }],
    });
    let schema_op = submit(&db, "k-schema", schema_id, derived).await;
    let schema_outcome = run_operation(
        &stores(),
        &worker(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        schema_op,
        LATER,
    )
    .await
    .expect("the worker itself must not fail");

    let item = &schema_outcome.items[0];
    assert_eq!(item.status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(
        item.failure.as_ref().map(|f| f.reason.clone()),
        Some(AdmissionFailureReason::FamilyKindConflict),
    );
}

#[tokio::test]
async fn an_instance_admits_against_a_type_from_an_earlier_operation() {
    let db = test_db().await;
    let outcome = admit_type_then(&db, "k1", INSTANCE_ID, json!({ "name": "later" }))
        .await
        .expect("the worker itself must not fail");
    assert_eq!(
        outcome.items[0].status,
        domain_enums::OperationItemStatus::Succeeded,
        "{:?}",
        outcome.items[0].failure,
    );

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let edges = types_registry::infra::storage::entity::dependency::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("edges");
    let instance_row = EntityRepo::find_by_gts_id(&conn, &allow_all(), INSTANCE_ID)
        .await
        .expect("read")
        .expect("committed");
    let type_row = EntityRepo::find_by_gts_id(&conn, &allow_all(), TYPE_ID)
        .await
        .expect("read")
        .expect("committed");
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0].from_entity_id, instance_row.id);
    assert_eq!(edges[0].to_entity_id, type_row.id);
    assert_eq!(
        edges[0].kind,
        types_registry::infra::storage::entity::enums::DependencyKind::InstanceOf,
    );

    // And the storage vocabulary agrees with the domain's on the kind.
    let row = EntityRepo::find_by_gts_id(&conn, &allow_all(), INSTANCE_ID)
        .await
        .expect("read")
        .expect("committed");
    let raw = types_registry::infra::storage::entity::entity::Entity::find()
        .filter(types_registry::infra::storage::entity::entity::Column::Id.eq(row.id))
        .secure()
        .scope_with(&allow_all())
        .one(&conn)
        .await
        .expect("raw")
        .expect("row");
    assert_eq!(raw.entity_kind, storage_enums::EntityKind::Instance);
}
