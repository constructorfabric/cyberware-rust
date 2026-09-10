//! Persistence across closing and reopening the database connection pool.
//!
//! No new process and no `TypesRegistryGear::init`: the database is a real file, and
//! the `RegistryService`, `DBProvider`, pool and connections are dropped between
//! phases; phase two reopens the file and re-runs the test migration path. Full
//! process restart and startup seeding remain T30's e2e obligation.
//!
//! The comparison is on whole `Model` values rather than a hand-picked column list,
//! over all eight tables written by one Type Schema followed by one Instance
//! (including the operation audit trail), each query in primary-key order.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt};
use toolkit_db::{DBProvider, DbError};
use toolkit_gts::gts_id;

use types_registry::config::TypesRegistryConfig;
use types_registry::domain::admission::{
    Candidate, NullDispatch, OperationDispatch, SubmitRequest,
};
use types_registry::domain::enums::{OperationKind, OperationStatus};
use types_registry::domain::policy::RegistrationPolicy;
use types_registry::domain::registry_service::{
    AdmissionMode, EntityKey, RegistryService, ServiceError,
};
use types_registry::infra::storage::entity::enums as storage_enums;
use types_registry::infra::storage::entity::{
    entity, instance, instance_revision, operation, operation_item, type_schema,
    type_schema_revision, version_family,
};

mod common;
use common::{TestDir, allow_all, stores, test_db, test_db_file};

const BOOT: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const CF_TYPE: &str = gts_id!("cf.core.example.type.v1~");
const CF_INSTANCE: &str = gts_id!("cf.core.example.type.v1~cf.core.example.first.v1");

fn schema(gts_id: &str) -> Value {
    json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "size": { "type": "integer" },
        },
    })
}

/// The same database-backed dependencies and interim inline setting that `init()`
/// selects until T21. This constructs the service directly; it does not exercise
/// the gear's boot sequence.
fn service(db: &Arc<DBProvider<DbError>>) -> RegistryService {
    service_with(db, true)
}

/// The same service with the inline-admission switch exposed. `false` is how a
/// committed acceptance whose process stopped before admission is reproduced:
/// acceptance commits, while no worker runs.
fn service_with(db: &Arc<DBProvider<DbError>>, admit_inline: bool) -> RegistryService {
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(NullDispatch);
    RegistryService::new(
        db.db(),
        stores(),
        RegistrationPolicy::default(),
        TypesRegistryConfig::default(),
        dispatch,
        if admit_inline {
            AdmissionMode::Inline
        } else {
            AdmissionMode::Outbox
        },
        common::metrics(),
    )
}

/// All durable state written by the Type Schema and Instance submissions. Whole
/// rows make the no-write assertion sensitive to updates as well as inserts.
#[derive(Debug, PartialEq, Eq)]
struct DurableState {
    operations: Vec<operation::Model>,
    operation_items: Vec<operation_item::Model>,
    families: Vec<version_family::Model>,
    entities: Vec<entity::Model>,
    schema_revisions: Vec<type_schema_revision::Model>,
    schemas: Vec<type_schema::Model>,
    instance_revisions: Vec<instance_revision::Model>,
    instances: Vec<instance::Model>,
}

async fn read_durable_state(db: &Arc<DBProvider<DbError>>) -> DurableState {
    let provider: DBProvider<DbError> = DBProvider::new(db.db());
    let conn = provider.conn().expect("conn");
    let scope = allow_all();
    DurableState {
        operations: operation::Entity::find()
            .order_by_asc(operation::Column::Id)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("operations"),
        operation_items: operation_item::Entity::find()
            .order_by_asc(operation_item::Column::Id)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("operation items"),
        families: version_family::Entity::find()
            .order_by_asc(version_family::Column::Id)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("families"),
        entities: entity::Entity::find()
            .order_by_asc(entity::Column::Id)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("entities"),
        schema_revisions: type_schema_revision::Entity::find()
            .order_by_asc(type_schema_revision::Column::EntityId)
            .order_by_asc(type_schema_revision::Column::RevisionNo)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("schema revisions"),
        schemas: type_schema::Entity::find()
            .order_by_asc(type_schema::Column::EntityId)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("current schemas"),
        instance_revisions: instance_revision::Entity::find()
            .order_by_asc(instance_revision::Column::EntityId)
            .order_by_asc(instance_revision::Column::RevisionNo)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("instance revisions"),
        instances: instance::Entity::find()
            .order_by_asc(instance::Column::EntityId)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
            .expect("current instances"),
    }
}

fn submission(key: &str, gts_id: &str, content: Value) -> SubmitRequest {
    SubmitRequest {
        idempotency_key: key.to_owned(),
        kind: OperationKind::Registration,
        dry_run: false,
        candidates: vec![Candidate {
            gts_id: gts_id.to_owned(),
            content: Some(content),
            expected_resource_version: None,
            force: false,
        }],
    }
}

#[tokio::test]
async fn a_schema_and_instance_survive_database_reopen() {
    let dir = TestDir::new("tr-reopen");
    let path = dir.path().join("registry.db");

    // ---------------------------------------------------------------- boot one
    let authored_schema = schema(CF_TYPE);
    let authored_instance = json!({ "name": "first", "size": 7 });
    let (before, schema_operation_id, instance_operation_id) = {
        let db = test_db_file(&path).await;
        let svc = service(&db);
        let accepted_schema = svc
            .submit(
                &submission("reopen-schema", CF_TYPE, authored_schema.clone()),
                BOOT,
            )
            .await
            .expect("schema accepted");
        let accepted_instance = svc
            .submit(
                &submission("reopen-instance", CF_INSTANCE, authored_instance.clone()),
                BOOT,
            )
            .await
            .expect("instance accepted");

        for operation_id in [accepted_schema.operation_id, accepted_instance.operation_id] {
            let op = svc
                .operation(operation_id)
                .await
                .expect("read operation")
                .expect("the operation exists");
            assert_eq!(op.status, OperationStatus::Completed);
        }

        let before = read_durable_state(&db).await;
        assert_eq!(before.operations.len(), 2);
        assert_eq!(before.operation_items.len(), 2);
        assert_eq!(before.entities.len(), 2);
        assert_eq!(before.schema_revisions.len(), 1);
        assert_eq!(before.schemas.len(), 1);
        assert_eq!(before.instance_revisions.len(), 1);
        assert_eq!(before.instances.len(), 1);
        // The artifacts are materialized at admission (D3), not on first read —
        // otherwise the whole comparison below would be vacuous.
        let current = &before.schemas[0];
        assert!(!current.resolved_schema.is_empty());
        assert!(!current.effective_traits.is_empty());
        assert!(!current.effective_traits_schema.is_empty());
        assert!(!current.resolution_fingerprint.is_empty());

        (
            before,
            accepted_schema.operation_id,
            accepted_instance.operation_id,
        )
    }; // the service, the provider, its pool and every connection drop here

    // ---------------------------------------------------------------- boot two
    // A brand-new provider over the same file. `test_db_file` re-runs the
    // test migration path, which this asserts is safe over populated state.
    let db = test_db_file(&path).await;
    let after = read_durable_state(&db).await;

    assert_eq!(
        after, before,
        "reopening the database must not change any durable registration state"
    );

    let schema_uuid = before
        .entities
        .iter()
        .find(|row| row.gts_id == CF_TYPE)
        .expect("the schema entity row")
        .gts_uuid;

    // Readable through a fresh service, not just by direct table reads. The Type
    // Schema is checked through both key forms; the Instance exercises its own
    // current-state branch and two separate tables.
    let svc = service(&db);
    let schema_by_id = svc
        .entity(&EntityKey::parse(CF_TYPE))
        .await
        .expect("read by identifier")
        .expect("the schema survived");
    let schema_by_uuid = svc
        .entity(&EntityKey::parse(&schema_uuid.to_string()))
        .await
        .expect("read by Registry Reference")
        .expect("the schema survived");
    let instance_by_id = svc
        .entity(&EntityKey::parse(CF_INSTANCE))
        .await
        .expect("read Instance by identifier")
        .expect("the Instance survived");

    assert_eq!(schema_by_id.gts_id, CF_TYPE);
    assert_eq!(schema_by_id.gts_uuid, schema_uuid);
    assert_eq!(schema_by_id.resource_version, 1);
    assert_eq!(schema_by_uuid.gts_uuid, schema_by_id.gts_uuid);
    assert_eq!(schema_by_uuid.gts_id, schema_by_id.gts_id);

    assert_eq!(schema_by_id.content.as_ref(), Some(&authored_schema));
    let stored_raw: Value =
        serde_json::from_str(&after.schema_revisions[0].raw_schema).expect("raw_schema is JSON");
    assert_eq!(stored_raw, authored_schema);

    let persisted_schema = &after.schemas[0];
    assert_eq!(
        schema_by_id.resolved_schema,
        Some(serde_json::from_str(&persisted_schema.resolved_schema).expect("resolved schema")),
    );
    assert_eq!(
        schema_by_id.effective_traits,
        Some(serde_json::from_str(&persisted_schema.effective_traits).expect("effective traits")),
    );
    assert_eq!(
        schema_by_id.effective_traits_schema,
        Some(
            serde_json::from_str(&persisted_schema.effective_traits_schema)
                .expect("effective traits schema"),
        ),
    );

    assert_eq!(instance_by_id.gts_id, CF_INSTANCE);
    assert_eq!(instance_by_id.resource_version, 1);
    assert_eq!(instance_by_id.content.as_ref(), Some(&authored_instance));
    assert!(instance_by_id.resolved_schema.is_none());
    assert!(instance_by_id.effective_traits.is_none());
    assert!(instance_by_id.effective_traits_schema.is_none());

    for (operation_id, gts_id) in [
        (schema_operation_id, CF_TYPE),
        (instance_operation_id, CF_INSTANCE),
    ] {
        let op = svc
            .operation(operation_id)
            .await
            .expect("read operation")
            .expect("the operation survived");
        assert_eq!(op.status, OperationStatus::Completed);
        assert_eq!(op.items.len(), 1);
        assert_eq!(op.items[0].gts_id, gts_id);
    }

    // The idempotency record is durable too. Comparing all eight tables proves
    // that a terminal replay after reopen neither inserts nor updates anything.
    let replay = svc
        .submit(&submission("reopen-schema", CF_TYPE, authored_schema), BOOT)
        .await
        .expect("the stored operation is replayable");
    assert!(replay.replayed);
    assert!(replay.terminal());
    assert_eq!(replay.operation_id, schema_operation_id);
    assert_eq!(read_durable_state(&db).await, after);

    drop(svc);
    drop(db);
}

/// Before T21, a process that dies between acceptance and inline admission leaves
/// an operation that only a retry under the same `Idempotency-Key` can drive. The
/// first phase manufactures exactly that committed database state; the second
/// phase proves a fresh service resumes it because the gate is `terminal`, not
/// `replayed`.
#[tokio::test]
async fn a_nonterminal_replay_resumes_inline_admission_before_t21() {
    let dir = TestDir::new("tr-reopen-resume");
    let path = dir.path().join("registry.db");

    let authored = schema(CF_TYPE);
    let accepted_id = {
        let db = test_db_file(&path).await;
        let accepted = service_with(&db, false)
            .submit(&submission("resume-key", CF_TYPE, authored.clone()), BOOT)
            .await
            .expect("accepted");
        assert!(!accepted.replayed);
        assert!(!accepted.terminal());

        // The state the crash leaves behind: acceptance and its item committed,
        // while admission wrote no entity state.
        let pending = read_durable_state(&db).await;
        assert_eq!(pending.operations.len(), 1);
        assert_eq!(pending.operation_items.len(), 1);
        assert_eq!(
            pending.operations[0].status,
            storage_enums::OperationStatus::Pending,
        );
        assert_eq!(
            pending.operation_items[0].status,
            storage_enums::OperationItemStatus::Pending,
        );
        assert!(pending.entities.is_empty(), "admission never ran");
        accepted.operation_id
    };

    // ---------------------------------------------------------------- boot two
    let db = test_db_file(&path).await;
    let svc = service(&db);
    let replay = svc
        .submit(&submission("resume-key", CF_TYPE, authored.clone()), BOOT)
        .await
        .expect("the retry is accepted");

    assert!(
        replay.replayed,
        "the same key resolves to the same operation"
    );
    assert_eq!(replay.operation_id, accepted_id);

    let op = svc
        .operation(accepted_id)
        .await
        .expect("read operation")
        .expect("the operation exists");
    assert_eq!(
        op.status,
        OperationStatus::Completed,
        "the retry drove the admission that the first pass never reached",
    );
    assert_eq!(op.items[0].resource_version, Some(1));

    let entity = svc
        .entity(&EntityKey::parse(CF_TYPE))
        .await
        .expect("read")
        .expect("the retry registered the entity");
    assert_eq!(entity.resource_version, 1);

    drop(svc);
    drop(db);
}

#[tokio::test]
async fn an_entity_without_its_current_state_is_reported_as_corrupt() {
    let db = test_db().await;
    let svc = service(&db);
    svc.submit(
        &submission("corrupt-schema", CF_TYPE, schema(CF_TYPE)),
        BOOT,
    )
    .await
    .expect("schema accepted");
    svc.submit(
        &submission(
            "corrupt-instance",
            CF_INSTANCE,
            json!({ "name": "first", "size": 7 }),
        ),
        BOOT,
    )
    .await
    .expect("instance accepted");

    let state = read_durable_state(&db).await;
    let schema_id = state
        .entities
        .iter()
        .find(|row| row.gts_id == CF_TYPE)
        .expect("schema entity")
        .id;
    let instance_id = state
        .entities
        .iter()
        .find(|row| row.gts_id == CF_INSTANCE)
        .expect("instance entity")
        .id;

    {
        let provider: DBProvider<DbError> = DBProvider::new(db.db());
        let conn = provider.conn().expect("conn");
        let scope = allow_all();
        type_schema::Entity::delete_many()
            .filter(type_schema::Column::EntityId.eq(schema_id))
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await
            .expect("remove current schema state");
        instance::Entity::delete_many()
            .filter(instance::Column::EntityId.eq(instance_id))
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await
            .expect("remove current instance state");
    }

    for (gts_id, expected) in [
        (CF_TYPE, "no current Type Schema state"),
        (CF_INSTANCE, "no current Instance state"),
    ] {
        match svc.entity(&EntityKey::parse(gts_id)).await {
            Err(ServiceError::CorruptDocument(detail)) => {
                assert!(detail.contains(expected), "unexpected detail: {detail}");
            }
            other => panic!("expected corrupt current state, got {other:?}"),
        }
    }
}
