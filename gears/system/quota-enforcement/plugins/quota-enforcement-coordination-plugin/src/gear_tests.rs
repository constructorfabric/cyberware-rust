#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use quota_enforcement_sdk::{CoordinationPluginV1, QuotaEnforcementCoordinationPluginSpecV1};
use sea_orm_migration::MigratorTrait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientScope;
use toolkit::config::ConfigProvider;
use toolkit::gts::PluginV1;
use toolkit::{ClientHub, Gear, GearCtx};
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

use super::{CoordinationPlugin, INSTANCE_SEGMENT};
use crate::infra::storage::Migrator;

struct StaticConfigProvider {
    root: Value,
}

impl ConfigProvider for StaticConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&Value> {
        self.root.get(gear)
    }
}

/// Registry fake that accepts `register` and records the payloads. Reads
/// delegate to the SDK mock.
#[derive(Default)]
struct RecordingRegistry {
    inner: MockTypesRegistryClient,
    registered: Mutex<Vec<Value>>,
}

impl RecordingRegistry {
    fn registered(&self) -> Vec<Value> {
        self.registered.lock().expect("registry log").clone()
    }
}

#[async_trait]
impl TypesRegistryClient for RecordingRegistry {
    async fn register(&self, entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        let results = entities
            .iter()
            .map(|e| RegisterResult::Ok {
                gts_id: e["id"].as_str().unwrap_or_default().to_owned(),
            })
            .collect();
        self.registered
            .lock()
            .expect("registry log")
            .extend(entities);
        Ok(results)
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.inner.register_type_schemas(type_schemas).await
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        self.inner.get_type_schema(type_id).await
    }

    async fn get_type_schema_by_uuid(
        &self,
        type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        self.inner.get_type_schema_by_uuid(type_uuid).await
    }

    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        self.inner.get_type_schemas(type_ids).await
    }

    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        self.inner.get_type_schemas_by_uuid(type_uuids).await
    }

    async fn list_type_schemas(
        &self,
        query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        self.inner.list_type_schemas(query).await
    }

    async fn register_instances(
        &self,
        instances: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.register(instances).await
    }

    async fn get_instance(&self, id: &str) -> Result<GtsInstance, CanonicalError> {
        self.inner.get_instance(id).await
    }

    async fn get_instance_by_uuid(&self, uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        self.inner.get_instance_by_uuid(uuid).await
    }

    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        self.inner.get_instances(ids).await
    }

    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        self.inner.get_instances_by_uuid(uuids).await
    }

    async fn list_instances(
        &self,
        query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        self.inner.list_instances(query).await
    }
}

async fn make_ctx(hub: Arc<ClientHub>, vendor: &str) -> GearCtx {
    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..ConnectOpts::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .expect("in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("migrations");
    let cfg = json!({
        "quota-enforcement-coordination-plugin": {
            "config": { "vendor": vendor, "priority": 10 }
        }
    });
    GearCtx::new(
        CoordinationPlugin::MODULE_NAME,
        Uuid::from_u128(1),
        Arc::new(StaticConfigProvider { root: cfg }),
        hub,
        CancellationToken::new(),
    )
    .with_db(DBProvider::new(db))
}

fn hub_with_registry() -> (Arc<ClientHub>, Arc<RecordingRegistry>) {
    let hub = Arc::new(ClientHub::new());
    let registry = Arc::new(RecordingRegistry::default());
    let client: Arc<dyn TypesRegistryClient> = registry.clone();
    hub.register::<dyn TypesRegistryClient>(client);
    (hub, registry)
}

#[tokio::test]
async fn init_publishes_the_instance_and_registers_the_scoped_client_under_it() {
    let (hub, registry) = hub_with_registry();
    let gear = CoordinationPlugin::default();
    gear.init(&make_ctx(hub.clone(), "acme").await)
        .await
        .expect("init succeeds");

    let (instance_id, expected_payload) =
        PluginV1::<QuotaEnforcementCoordinationPluginSpecV1>::build_registration(
            INSTANCE_SEGMENT,
            "acme",
            10,
        )
        .expect("registration payload");
    let registered = registry.registered();
    assert_eq!(registered.len(), 1, "exactly one instance is published");
    assert_eq!(registered[0]["id"], expected_payload["id"]);
    assert_eq!(registered[0]["vendor"], json!("acme"));
    assert_eq!(registered[0]["priority"], json!(10));

    let client = hub
        .get_scoped::<dyn CoordinationPluginV1>(&ClientScope::gts_id(&instance_id))
        .expect("scoped client is registered under the instance id");
    let lock = client
        .try_lock(
            quota_enforcement_sdk::LockScope::LeaseSweeper,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the registered client works against the bound database");
    client.release(lock).await.expect("release");
}

#[tokio::test]
async fn init_fails_on_a_blank_vendor_before_touching_the_registry() {
    let (hub, registry) = hub_with_registry();
    let gear = CoordinationPlugin::default();
    let err = gear
        .init(&make_ctx(hub, "   ").await)
        .await
        .expect_err("blank vendor rejected");
    assert!(err.to_string().contains("vendor"), "{err}");
    assert!(
        registry.registered().is_empty(),
        "nothing is published on a bad config"
    );
}

#[tokio::test]
async fn init_fails_closed_without_a_types_registry_client() {
    let hub = Arc::new(ClientHub::new());
    let gear = CoordinationPlugin::default();
    let err = gear
        .init(&make_ctx(hub.clone(), "acme").await)
        .await
        .expect_err("no registry, no plugin");
    assert!(format!("{err:#}").contains("not found"), "{err:#}");
    assert!(
        hub.try_get_scoped::<dyn CoordinationPluginV1>(&ClientScope::gts_id(
            &PluginV1::<QuotaEnforcementCoordinationPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                "acme",
                10
            )
            .expect("payload")
            .0
        ))
        .is_none(),
        "no client is registered when publication failed"
    );
}

#[tokio::test]
async fn a_second_init_fails_on_the_already_initialized_guard() {
    let (hub, registry) = hub_with_registry();
    let gear = CoordinationPlugin::default();
    let ctx = make_ctx(hub, "acme").await;
    gear.init(&ctx).await.expect("first init");
    let err = gear.init(&ctx).await.expect_err("second init");
    assert!(err.to_string().contains("already initialized"), "{err}");
    assert_eq!(
        registry.registered().len(),
        1,
        "the guard fires before a second publication"
    );
}
