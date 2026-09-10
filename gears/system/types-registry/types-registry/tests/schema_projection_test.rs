//! Verify that admission reads exclude artifact payloads in the executed SQL.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use parking_lot::Mutex;
use time::macros::datetime;
use toolkit_db::secure::AccessScope;
use toolkit_gts::gts_id;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

use common::{
    allow_all, seed_current_type_schema, seed_operation_item, seed_type_schema_revision, test_db,
};
use types_registry::domain::enums::{EntityKind, OwnershipScope};
use types_registry::domain::ports::{NewEntity, snapshot_read};
use types_registry::infra::storage::repo::{EntityRepo, TypeSchemaRepo, VersionFamilyRepo};

#[derive(Clone, Default)]
struct SqlStatements(Arc<Mutex<Vec<String>>>);

#[derive(Default)]
struct Statement(Option<String>);

impl Visit for Statement {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "db.statement" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber> Layer<S> for SqlStatements {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            let mut statement = Statement::default();
            event.record(&mut statement);
            if let Some(sql) = statement.0 {
                self.0.lock().push(sql);
            }
        }
    }
}

#[tokio::test]
async fn admission_reads_select_revision_identity_and_authored_content_without_artifacts() {
    // SQLite executes queries on its connection worker thread.
    // This test binary installs one subscriber so those events are captured too.
    let statements = SqlStatements::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(statements.clone()),
    )
    .unwrap();
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let scope = allow_all();
    let now = datetime!(2026-09-08 12:00 UTC);
    let id = gts_id!("cf.core.projection.subject.v1~");
    let (family, _) = VersionFamilyRepo::create_or_get(
        &conn,
        &scope,
        "gts.cf.core.projection.subject",
        OwnershipScope::Global,
        None,
        now,
    )
    .await
    .unwrap();
    let entity = EntityRepo::insert(
        &conn,
        &scope,
        NewEntity {
            gts_uuid: gts::GtsId::try_new(id).unwrap().to_uuid(),
            gts_id: id.to_owned(),
            entity_kind: EntityKind::TypeSchema,
            family_id: family.id,
            ownership_scope: OwnershipScope::Global,
            owner_tenant_id: None,
            owning_gear: Some("types-registry".to_owned()),
            now,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let old = r#"{"title":"old"}"#;
    let latest = r#"{"title":"current"}"#;
    for (revision, body) in [(1, old), (2, latest)] {
        let item = seed_operation_item(&conn, id, revision, now).await;
        seed_type_schema_revision(&conn, entity.id, revision, item, body, now).await;
    }
    seed_current_type_schema(&conn, entity.id, 2, latest, now).await;

    statements.0.lock().clear();
    let projections = TypeSchemaRepo::current_projections(&conn, &scope, &[entity.id])
        .await
        .unwrap();
    assert_eq!(projections.len(), 1);
    assert_eq!(projections[0].cas.revision_no, 2);
    assert_eq!(projections[0].cas.resolution_fingerprint, vec![0x11]);

    let documents = db
        .transaction_with_config(snapshot_read(&db.db()), move |tx| {
            Box::pin(async move {
                Ok(TypeSchemaRepo::current_documents(tx, &allow_all(), &[entity.id]).await?)
            })
        })
        .await
        .unwrap();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].raw_schema, latest);
    assert_eq!(documents[0].content_hash, vec![2]);
    assert_eq!(documents[0].projection.revision_no, 2);
    assert_eq!(documents[0].projection.resolution_fingerprint, vec![0x11]);

    assert!(
        TypeSchemaRepo::current_projections(&conn, &AccessScope::default(), &[entity.id])
            .await
            .unwrap()
            .is_empty(),
        "a custom SQL projection must preserve deny-all scoping"
    );

    let captured = statements.0.lock();
    let selects: Vec<&str> = captured
        .iter()
        .map(|sql| sql.trim())
        .filter(|sql| sql.starts_with("SELECT") && sql.contains("types_registry__type_schema"))
        .collect();
    assert_eq!(
        selects.len(),
        4,
        "capture every repository read: {captured:?}"
    );
    for sql in selects {
        let columns = sql.split(" FROM ").next().unwrap();
        for unused in [
            "resolved_schema",
            "effective_traits",
            "effective_traits_schema",
            "gts_spec_version",
            "gts_impl_version",
            "operation_item_id",
            "created_at",
            "updated_at",
        ] {
            assert!(!columns.contains(unused), "unneeded {unused} in {sql}");
        }
    }
}
