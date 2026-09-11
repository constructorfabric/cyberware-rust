//! Real `SpecificationManager`, backed by a local `SQLite` cache
//! (`event_broker_spec_cache`) that is loaded from `types-registry` and holds
//! what the most recent read found.
//!
//! Each entity is read as the GTS kind it actually is. A topic is an
//! **instance** of the topic base type, so it comes back from
//! `list_instances`. An event type is a **derived type schema** of the abstract
//! event base type, so it comes back from `list_type_schemas`; that listing
//! includes the base itself, because a base identifier behaves as the implicit
//! envelope `...~*`, and the base is excluded by identity.
//!
//! What lands in the cache is the projection rather than the registered
//! document, joined for a topic with the settings this deployment resolved for
//! it. Nothing on a request path parses a schema: the payload contract ingest
//! validates against is already part of the projection, and re-deriving it per
//! call would walk the inheritance chain to recompute a value that cannot
//! change between reads.

use std::sync::Arc;

use async_trait::async_trait;
use event_broker_sdk::models::EventType;
use gts::GtsTypeId;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use serde_json::Value as JsonValue;
use toolkit_db::DBProvider;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_gts::{GtsInstanceId, GtsSchema};
use toolkit_security::AccessScope;
use types_registry_sdk::{InstanceQuery, TypeSchemaQuery, TypesRegistryClient};

use crate::config::EventBrokerConfig;
use crate::domain::error::DomainError;
use crate::domain::model::Topic;
use crate::domain::resolution::{Declaration, resolve};
use crate::domain::specification::SpecificationManager;
use crate::domain::{event_type as event_type_traits, projection};
use crate::infra::storage::entity::spec_cache::{
    self, ActiveModel as SpecCacheAM, Column as SpecCacheColumn, Entity as SpecCacheEntity,
    SpecKind,
};

/// Serves what the last load put in the cache.
///
/// Holds no `types-registry` client: every read is a local lookup, and the load
/// that fills the cache is a free function with its own inputs. A client here
/// would be a second, unused route to the registry.
pub struct TypesRegistrySpecificationManager {
    db: Arc<DBProvider<toolkit_db::DbError>>,
}

impl TypesRegistrySpecificationManager {
    #[must_use]
    pub fn new(db: Arc<DBProvider<toolkit_db::DbError>>) -> Self {
        Self { db }
    }

    async fn find_row(
        &self,
        kind: SpecKind,
        id: &str,
    ) -> Result<Option<spec_cache::Model>, DomainError> {
        let conn = self.db.conn()?;
        let row = SpecCacheEntity::find()
            .filter(SpecCacheColumn::GtsId.eq(id))
            .filter(SpecCacheColumn::Kind.eq(kind.as_str()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await?;
        Ok(row)
    }

    async fn list_rows(&self, kind: SpecKind) -> Result<Vec<spec_cache::Model>, DomainError> {
        let conn = self.db.conn()?;
        let rows = SpecCacheEntity::find()
            .filter(SpecCacheColumn::Kind.eq(kind.as_str()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        Ok(rows)
    }
}

/// Reads every topic and event type `types-registry` holds, projects each one,
/// resolves a topic's settings against configuration, and upserts the result
/// into the local cache by `gts_id` - minting a fresh surrogate id only for a
/// never-before-seen identifier, so an id already assigned survives the load.
///
/// An entity that fails the broker's own validation on the way in is excluded
/// with a warning rather than failing the load: one gear's malformed event type
/// is not a reason to leave every other topic on the instance unserved. A
/// publish naming an excluded type is answered as unregistered.
///
/// A free function rather than a method: it needs the client, the database and
/// the configuration, and none of the trait's own methods, so a caller does not
/// have to keep the concrete manager type around to reach it.
///
/// # Errors
/// Returns `DomainError::StorageUnavailable` if the local cache database is
/// unreachable. A per-entity failure is logged and skipped.
pub async fn bulk_load(
    client: &Arc<dyn TypesRegistryClient>,
    db: &Arc<DBProvider<toolkit_db::DbError>>,
    config: &EventBrokerConfig,
) -> Result<(), DomainError> {
    let conn = db.conn()?;

    let topic_base = event_broker_sdk::gts::TopicV1::TYPE_ID;
    let instances = match client
        .list_instances(InstanceQuery::new().with_pattern(format!("{topic_base}*")))
        .await
    {
        Ok(instances) => instances,
        Err(err) => {
            tracing::warn!(%err, type_id = topic_base, "list_instances failed");
            Vec::new()
        }
    };
    for instance in &instances {
        let projected = match projection::topic(instance) {
            Ok(projected) => projected,
            Err(err) => {
                tracing::warn!(%err, id = %instance.id, "topic excluded: it does not project");
                continue;
            }
        };
        let declared = Declaration {
            retention: projected
                .retention
                .as_ref()
                .map(|retention| std::time::Duration::from(retention.clone())),
        };
        let settings = match resolve(config, &projected.id, &declared) {
            Ok(settings) => settings,
            Err(err) => {
                tracing::warn!(%err, id = %instance.id, "topic excluded: its settings do not resolve");
                continue;
            }
        };
        let topic = Topic {
            id: projected.id.clone(),
            description: projected.description,
            retention: projected.retention,
            settings,
        };
        upsert_spec_row(&conn, SpecKind::Topic, topic.id.as_ref(), &topic).await?;
    }

    let event_base = event_broker_sdk::gts::EventV1::TYPE_ID;
    let schemas = match client
        .list_type_schemas(TypeSchemaQuery::new().with_pattern(format!("{event_base}*")))
        .await
    {
        Ok(schemas) => schemas,
        Err(err) => {
            tracing::warn!(%err, type_id = event_base, "list_type_schemas failed");
            Vec::new()
        }
    };
    for schema in &schemas {
        // The pattern is an envelope, so the base comes back inside it. It is
        // abstract: no producer can publish it, and nothing may resolve it as a
        // type of its own.
        if schema.type_id.as_ref() == event_base {
            continue;
        }
        // Validated here because this is the only moment the broker holds the
        // resolved schema. A pointer naming no declared member would fail
        // identically on every publish of the type, so admitting it would turn
        // one gear's mistake into an open-ended runtime failure.
        if let Err(err) = event_type_traits::validate_partition_key(schema) {
            tracing::warn!(%err, id = %schema.type_id, "event type excluded: its partition key names no declared member");
            continue;
        }
        let projected = match projection::event_type(schema) {
            Ok(projected) => projected,
            Err(err) => {
                tracing::warn!(%err, id = %schema.type_id, "event type excluded: it does not project");
                continue;
            }
        };
        upsert_spec_row(
            &conn,
            SpecKind::EventType,
            projected.id.as_ref(),
            &EventTypeRecord::from(&projected),
        )
        .await?;
    }
    Ok(())
}

/// Updates the row's `payload` in place if `(kind, gts_id)` already exists
/// (preserving `id`), else inserts a fresh row. Tries the update first via
/// `update_many().filter(...)` (generic over any primary-key type, unlike
/// `secure_update_with_scope`, which hardcodes `id: uuid::Uuid` and can't
/// address `spec_cache`'s `i64` key) and falls back to insert only if it
/// affected zero rows - avoiding a separate existence-check round trip.
async fn upsert_spec_row(
    conn: &impl DBRunner,
    kind: SpecKind,
    gts_id: &str,
    spec: &impl serde::Serialize,
) -> Result<(), DomainError> {
    let payload = serde_json::to_string(spec)
        .map_err(|e| DomainError::Internal(format!("failed to serialize spec: {e}")))?;

    let update_result = SpecCacheEntity::update_many()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(SpecCacheColumn::GtsId.eq(gts_id))
                .add(SpecCacheColumn::Kind.eq(kind.as_str())),
        )
        .col_expr(SpecCacheColumn::Payload, Expr::value(payload.clone()))
        .exec(conn)
        .await?;

    if update_result.rows_affected == 0 {
        let am = SpecCacheAM {
            id: sea_orm::ActiveValue::NotSet,
            gts_id: Set(gts_id.to_owned()),
            kind: Set(kind.as_str().to_owned()),
            payload: Set(payload),
        };
        secure_insert::<SpecCacheEntity>(am, &AccessScope::allow_all(), conn).await?;
    }
    Ok(())
}

/// Cache-persistence shape for a projected event type.
///
/// The spec cache stores specs as JSON. The SDK's public
/// [`EventType`](event_broker_sdk::models::EventType) carries no serde -
/// persistence and transport stay off the public SDK surface - so this private
/// mirror owns the serialized form and converts at the cache boundary. Its
/// field names are the persisted contract.
#[derive(serde::Serialize, serde::Deserialize)]
struct EventTypeRecord {
    id: GtsTypeId,
    topic: GtsInstanceId,
    description: Option<String>,
    allowed_subject_types: Vec<String>,
    partition_key: String,
    data_schema: JsonValue,
}

impl From<&EventType> for EventTypeRecord {
    fn from(e: &EventType) -> Self {
        Self {
            id: e.id.clone(),
            topic: e.topic.clone(),
            description: e.description.clone(),
            allowed_subject_types: e.allowed_subject_types.clone(),
            partition_key: e.partition_key.clone(),
            data_schema: e.data_schema.clone(),
        }
    }
}

impl From<EventTypeRecord> for EventType {
    fn from(r: EventTypeRecord) -> Self {
        Self {
            id: r.id,
            topic: r.topic,
            description: r.description,
            allowed_subject_types: r.allowed_subject_types,
            partition_key: r.partition_key,
            data_schema: r.data_schema,
        }
    }
}

fn deserialize_row<T: serde::de::DeserializeOwned>(row: &spec_cache::Model) -> Option<T> {
    match serde_json::from_str(&row.payload) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::warn!(
                %err, gts_id = %row.gts_id, kind = %row.kind,
                "failed to deserialize cached spec row"
            );
            None
        }
    }
}

#[async_trait]
impl SpecificationManager for TypesRegistrySpecificationManager {
    async fn get_topic(&self, id: &GtsInstanceId) -> Option<Topic> {
        let row = match self.find_row(SpecKind::Topic, id.as_ref()).await {
            Ok(row) => row?,
            Err(err) => {
                tracing::warn!(%err, topic_id = %id, "spec cache lookup failed while resolving topic");
                return None;
            }
        };
        deserialize_row(&row)
    }

    async fn get_event_type(&self, id: &GtsTypeId) -> Option<EventType> {
        let row = match self.find_row(SpecKind::EventType, id.as_ref()).await {
            Ok(row) => row?,
            Err(err) => {
                tracing::warn!(%err, event_type_id = %id, "spec cache lookup failed while resolving event type");
                return None;
            }
        };
        deserialize_row::<EventTypeRecord>(&row).map(EventType::from)
    }

    async fn validate_event_data(
        &self,
        event_type: &EventType,
        data: &JsonValue,
    ) -> Result<(), DomainError> {
        crate::domain::specification::validate_against_schema(event_type, data)
    }

    async fn list_topics(&self) -> Vec<Topic> {
        match self.list_rows(SpecKind::Topic).await {
            Ok(rows) => rows.iter().filter_map(deserialize_row).collect(),
            Err(err) => {
                tracing::warn!(%err, "spec cache list failed while listing topics");
                Vec::new()
            }
        }
    }

    async fn list_event_types(&self) -> Vec<EventType> {
        match self.list_rows(SpecKind::EventType).await {
            Ok(rows) => rows
                .iter()
                .filter_map(|row| deserialize_row::<EventTypeRecord>(row))
                .map(EventType::from)
                .collect(),
            Err(err) => {
                tracing::warn!(%err, "spec cache list failed while listing event types");
                Vec::new()
            }
        }
    }

    async fn resolve_topic_id(&self, id: &GtsInstanceId) -> Result<i64, DomainError> {
        self.find_row(SpecKind::Topic, id.as_ref())
            .await?
            .map(|row| row.id)
            .ok_or_else(|| DomainError::NotFound {
                code: "TopicNotFound",
                message: format!("topic '{id}' is not registered"),
                resource: id.to_string(),
            })
    }

    async fn resolve_event_type_id(&self, id: &GtsTypeId) -> Result<i64, DomainError> {
        self.find_row(SpecKind::EventType, id.as_ref())
            .await?
            .map(|row| row.id)
            .ok_or_else(|| DomainError::NotFound {
                code: "EventTypeNotFound",
                message: format!("event type '{}' is not registered", id.as_ref()),
                resource: id.as_ref().to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use toolkit_db::migration_runner::run_migrations_for_testing;
    use toolkit_db::{ConnectOpts, DBProvider, connect_db};
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use uuid::Uuid;

    use super::{Arc, DomainError, GtsInstanceId, GtsSchema, GtsTypeId, SpecificationManager};
    use crate::config::EventBrokerConfig;
    use crate::domain::resolution::Source;
    use crate::infra::storage::migrations::Migrator;
    use crate::test_support::type_registry::{derived_event_type, event_base_schema};
    use sea_orm_migration::MigratorTrait;

    const TOPIC_ID: &str = "gts.cf.core.events.topic.v1~example.eb.t1.topic.v1";
    const EVENT_TYPE_ID: &str = "gts.cf.core.events.event.v1~example.eb.t1.foo.v1~";
    const SUBJECT_TYPE: &str = "gts.example.eb.t1.subject.v1~";
    const SQLITE_BACKEND: &str = "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~";

    /// A topic instance carrying exactly what its base type declares - the
    /// partition count is not among them, and a document that named one would
    /// be refused at registration.
    fn topic_document(id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "description": "events this test publishes",
            "retention": "PT168H",
        })
    }

    fn config(topics: &serde_json::Value) -> EventBrokerConfig {
        serde_json::from_value(json!({
            "mode": "standalone",
            "default_storage_backend": SQLITE_BACKEND,
            "topics": topics.clone(),
        }))
        .expect("test configuration deserializes")
    }

    async fn test_db() -> Arc<DBProvider<toolkit_db::DbError>> {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "cf-eb-spec-cache-test-{}.db",
            Uuid::now_v7().simple()
        ));
        let mut file = path.to_string_lossy().replace('\\', "/");
        if !file.starts_with('/') {
            file.insert(0, '/');
        }
        let dsn = format!("sqlite://{file}?mode=rwc");
        let opts = ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        };
        let db = connect_db(&dsn, opts).await.expect("connect sqlite");
        run_migrations_for_testing(&db, Migrator::migrations())
            .await
            .expect("migrations");
        Arc::new(DBProvider::new(db))
    }

    /// Loads the given documents through the real load path, so every test here
    /// is evidence about what a booting process does.
    async fn manager_with(
        instances: Vec<serde_json::Value>,
        schemas: Vec<types_registry_sdk::GtsTypeSchema>,
        cfg: &EventBrokerConfig,
    ) -> super::TypesRegistrySpecificationManager {
        let mock = MockTypesRegistryClient::new()
            .with_instances(instances.iter().map(|doc| {
                make_test_instance(
                    doc["id"].as_str().expect("a document names its id"),
                    doc.clone(),
                )
            }))
            .with_type_schemas(schemas);
        let client: Arc<dyn types_registry_sdk::TypesRegistryClient> = Arc::new(mock);
        let db = test_db().await;
        super::bulk_load(&client, &db, cfg)
            .await
            .expect("bulk_load");
        super::TypesRegistrySpecificationManager::new(db)
    }

    #[tokio::test]
    async fn a_topic_instance_resolves_with_the_settings_configuration_gives_it() {
        let cfg = config(&json!({ "gts.cf.core.events.topic.v1~": { "partitions": 4 } }));
        let manager = manager_with(vec![topic_document(TOPIC_ID)], Vec::new(), &cfg).await;

        let topic = manager
            .get_topic(&GtsInstanceId::try_new(TOPIC_ID).unwrap())
            .await
            .expect("a registered topic resolves");
        assert_eq!(topic.id.as_ref(), TOPIC_ID);
        assert_eq!(topic.description, "events this test publishes");
        assert_eq!(*topic.settings.partitions().value(), 4);
        assert_eq!(topic.settings.partitions().source(), Source::TypeEntry);
    }

    /// The declared tier of the ladder, end to end: the instance says a week,
    /// configuration says nothing about retention, so a week is enforced.
    #[tokio::test]
    async fn a_declared_retention_survives_a_deployment_that_states_none() {
        let cfg = config(&json!({}));
        let manager = manager_with(vec![topic_document(TOPIC_ID)], Vec::new(), &cfg).await;

        let topic = manager
            .get_topic(&GtsInstanceId::try_new(TOPIC_ID).unwrap())
            .await
            .expect("a registered topic resolves");
        assert_eq!(
            topic.settings.retention().value().duration,
            std::time::Duration::from_hours(168)
        );
        assert_eq!(
            topic.settings.retention().source(),
            Source::Specification,
            "the built-in must not displace what the topic declared"
        );
    }

    #[tokio::test]
    async fn an_event_type_resolves_from_its_derived_schema() {
        let cfg = config(&json!({}));
        let manager = manager_with(
            vec![topic_document(TOPIC_ID)],
            vec![derived_event_type(
                EVENT_TYPE_ID,
                TOPIC_ID,
                json!({}),
                &[SUBJECT_TYPE],
            )],
            &cfg,
        )
        .await;

        let event_type = manager
            .get_event_type(&GtsTypeId::try_new(EVENT_TYPE_ID).unwrap())
            .await
            .expect("a derived event-type schema resolves");
        assert_eq!(event_type.id.as_ref(), EVENT_TYPE_ID);
        assert_eq!(event_type.topic.as_ref(), TOPIC_ID);
        assert_eq!(event_type.allowed_subject_types, vec![SUBJECT_TYPE]);
        assert_eq!(
            event_type.partition_key, "/tenant_id",
            "the pointer this type declares for itself"
        );
    }

    /// A type that declares no pointer of its own still resolves one, from the
    /// default on the base's trait schema. This is the mechanism the whole
    /// partition contract rests on - without it, a type that says nothing about
    /// partitioning would resolve no key at all and every publish of it would
    /// fail - so it is asserted against a schema that genuinely omits the
    /// trait, not one built by a helper that fills it in.
    #[tokio::test]
    async fn a_type_declaring_no_pointer_resolves_the_bases_default() {
        let cfg = config(&json!({}));
        let silent = types_registry_sdk::GtsTypeSchema::try_new(
            GtsTypeId::new(EVENT_TYPE_ID),
            json!({
                "$id": format!("gts://{EVENT_TYPE_ID}"),
                "x-gts-traits": { "topic": TOPIC_ID, "allowed_subject_types": [SUBJECT_TYPE] },
                "type": "object",
                "allOf": [
                    { "$ref": format!("gts://{}", event_broker_sdk::gts::EventV1::TYPE_ID) },
                    { "type": "object", "properties": { "data": {} } },
                ],
            }),
            None,
            Some(event_base_schema()),
        )
        .expect("a derived schema that declares only its topic is still valid");

        let manager = manager_with(vec![topic_document(TOPIC_ID)], vec![silent], &cfg).await;

        let event_type = manager
            .get_event_type(&GtsTypeId::try_new(EVENT_TYPE_ID).unwrap())
            .await
            .expect("it resolves");
        assert_eq!(
            event_type.partition_key, "/tenant_id",
            "the default on the base's trait schema reaches a type that states none"
        );
    }

    /// The listing of a base type's derived schemas includes the base, because
    /// a base identifier behaves as the implicit envelope `...~*`. It is
    /// abstract, so nothing may resolve it as an event type of its own.
    #[tokio::test]
    async fn the_abstract_base_is_not_materialized_as_an_event_type() {
        let cfg = config(&json!({}));
        let manager = manager_with(
            vec![topic_document(TOPIC_ID)],
            vec![
                (*event_base_schema()).clone(),
                derived_event_type(EVENT_TYPE_ID, TOPIC_ID, json!({}), &[SUBJECT_TYPE]),
            ],
            &cfg,
        )
        .await;

        let listed: Vec<String> = manager
            .list_event_types()
            .await
            .into_iter()
            .map(|event_type| event_type.id.as_ref().to_owned())
            .collect();
        assert_eq!(listed, vec![EVENT_TYPE_ID.to_owned()]);
    }

    /// The delivery side of the role split: an instance that holds no topic
    /// configuration serves the settings ingest already resolved, out of the
    /// database they share. Two instances with drifted configuration cannot
    /// therefore serve one topic under two partition counts.
    #[tokio::test]
    async fn a_manager_serves_what_a_load_resolved_under_configuration_it_never_saw() {
        let db = test_db().await;
        let client: Arc<dyn types_registry_sdk::TypesRegistryClient> = Arc::new(
            MockTypesRegistryClient::new()
                .with_instances(vec![make_test_instance(TOPIC_ID, topic_document(TOPIC_ID))]),
        );
        let ingest_config = config(&json!({ "gts.cf.core.events.topic.v1~": { "partitions": 4 } }));
        super::bulk_load(&client, &db, &ingest_config)
            .await
            .expect("the ingest role loads");

        // Built with no configuration of its own at all - it only reads.
        let delivery = super::TypesRegistrySpecificationManager::new(Arc::clone(&db));
        let topic = delivery
            .get_topic(&GtsInstanceId::try_new(TOPIC_ID).unwrap())
            .await
            .expect("the stored record resolves");
        assert_eq!(*topic.settings.partitions().value(), 4);
        assert_eq!(topic.settings.partitions().source(), Source::TypeEntry);
    }

    #[tokio::test]
    async fn get_topic_returns_none_for_an_unregistered_id() {
        let cfg = config(&json!({}));
        let manager = manager_with(Vec::new(), Vec::new(), &cfg).await;

        let missing =
            GtsInstanceId::try_new("gts.cf.core.events.topic.v1~example.eb.missing.topic.v1")
                .unwrap();
        assert!(manager.get_topic(&missing).await.is_none());
    }

    /// A load must preserve an already-assigned surrogate id, not renumber it.
    #[tokio::test]
    async fn resolve_topic_id_is_stable_across_a_second_load() {
        let cfg = config(&json!({}));
        let client: Arc<dyn types_registry_sdk::TypesRegistryClient> = Arc::new(
            MockTypesRegistryClient::new()
                .with_instances(vec![make_test_instance(TOPIC_ID, topic_document(TOPIC_ID))]),
        );
        let db = test_db().await;

        super::bulk_load(&client, &db, &cfg).await.expect("first");
        let manager = super::TypesRegistrySpecificationManager::new(Arc::clone(&db));
        let topic_gts_id = GtsInstanceId::try_new(TOPIC_ID).unwrap();
        let first = manager
            .resolve_topic_id(&topic_gts_id)
            .await
            .expect("first");

        super::bulk_load(&client, &db, &cfg).await.expect("second");
        let second = manager
            .resolve_topic_id(&topic_gts_id)
            .await
            .expect("second");

        assert_eq!(first, second, "surrogate id must not be renumbered");
    }

    #[tokio::test]
    async fn resolve_topic_id_returns_not_found_for_an_unregistered_id() {
        let cfg = config(&json!({}));
        let manager = manager_with(Vec::new(), Vec::new(), &cfg).await;

        let missing =
            GtsInstanceId::try_new("gts.cf.core.events.topic.v1~example.eb.missing.topic.v1")
                .unwrap();
        let err = manager
            .resolve_topic_id(&missing)
            .await
            .expect_err("an unregistered topic must not resolve");
        assert!(matches!(err, DomainError::NotFound { .. }));
    }
}
