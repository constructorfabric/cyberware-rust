//! Passes are forced, never slept for, so what these assert is what the worker
//! does rather than what a timer happened to do.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use toolkit_gts::GtsInstanceId;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};

use super::SpecificationRefreshWorker;
use crate::domain::specification::SpecificationManager;
use crate::infra::specification::TypesRegistrySpecificationManager;
use crate::test_support::StaticTypesRegistry;

const TOPIC_ID: &str = "gts.cf.core.events.topic.v1~example.eb.refresh.topic.v1";

fn topic_document() -> serde_json::Value {
    json!({ "id": TOPIC_ID, "description": "a topic registered after startup" })
}

async fn test_db() -> Arc<toolkit_db::DBProvider<toolkit_db::DbError>> {
    use sea_orm_migration::MigratorTrait;
    let mut path = std::env::temp_dir();
    path.push(format!(
        "cf-eb-refresh-test-{}.db",
        uuid::Uuid::now_v7().simple()
    ));
    let dsn = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect sqlite");
    toolkit_db::migration_runner::run_migrations_for_testing(
        &db,
        crate::infra::storage::migrations::Migrator::migrations(),
    )
    .await
    .expect("migrations");
    Arc::new(toolkit_db::DBProvider::new(db))
}

/// The point of the cadence: a topic that did not exist when the process
/// started is resolvable after a pass, with no restart in between.
#[tokio::test]
async fn a_topic_registered_after_the_first_pass_resolves_after_the_next_one() {
    let db = test_db().await;
    let empty: Arc<dyn types_registry_sdk::TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new());
    let config = StaticTypesRegistry::empty_config();
    let manager = TypesRegistrySpecificationManager::new(Arc::clone(&db));
    let topic_id = GtsInstanceId::try_new(TOPIC_ID).expect("a valid GTS instance id");

    SpecificationRefreshWorker::new(
        Arc::clone(&empty),
        Arc::clone(&db),
        config.clone(),
        Duration::from_mins(1),
    )
    .run_once()
    .await;
    assert!(
        manager.get_topic(&topic_id).await.is_none(),
        "nothing was registered when the first pass ran"
    );

    let registered: Arc<dyn types_registry_sdk::TypesRegistryClient> = Arc::new(
        MockTypesRegistryClient::new()
            .with_instances(vec![make_test_instance(TOPIC_ID, topic_document())]),
    );
    SpecificationRefreshWorker::new(registered, Arc::clone(&db), config, Duration::from_mins(1))
        .run_once()
        .await;

    let topic = manager
        .get_topic(&topic_id)
        .await
        .expect("the next pass admits it");
    assert_eq!(topic.description, "a topic registered after startup");
}

/// Two forced passes over the same registry leave the entity's identity alone -
/// the durable tables key a topic by that integer, so renumbering it would
/// silently re-point every cursor.
#[tokio::test]
async fn a_second_forced_pass_does_not_renumber_what_it_still_finds() {
    let db = test_db().await;
    let client: Arc<dyn types_registry_sdk::TypesRegistryClient> = Arc::new(
        MockTypesRegistryClient::new()
            .with_instances(vec![make_test_instance(TOPIC_ID, topic_document())]),
    );
    let worker = || {
        SpecificationRefreshWorker::new(
            Arc::clone(&client),
            Arc::clone(&db),
            StaticTypesRegistry::empty_config(),
            Duration::from_mins(1),
        )
    };
    let manager = TypesRegistrySpecificationManager::new(Arc::clone(&db));
    let topic_id = GtsInstanceId::try_new(TOPIC_ID).expect("a valid GTS instance id");

    worker().run_once().await;
    let first = manager
        .resolve_topic_id(&topic_id)
        .await
        .expect("resolves after the first pass");
    worker().run_once().await;
    let second = manager
        .resolve_topic_id(&topic_id)
        .await
        .expect("resolves after the second pass");

    assert_eq!(first, second);
}

/// A pass against an unreachable registry is not a failure that empties the
/// cache: what the last pass found still serves.
#[tokio::test]
async fn a_failing_pass_leaves_the_previous_load_in_place() {
    let db = test_db().await;
    let client: Arc<dyn types_registry_sdk::TypesRegistryClient> = Arc::new(
        MockTypesRegistryClient::new()
            .with_instances(vec![make_test_instance(TOPIC_ID, topic_document())]),
    );
    let manager = TypesRegistrySpecificationManager::new(Arc::clone(&db));
    let topic_id = GtsInstanceId::try_new(TOPIC_ID).expect("a valid GTS instance id");

    SpecificationRefreshWorker::new(
        client,
        Arc::clone(&db),
        StaticTypesRegistry::empty_config(),
        Duration::from_mins(1),
    )
    .run_once()
    .await;

    // A client that answers nothing at all, standing in for a registry that
    // has become unreachable between passes.
    let unreachable: Arc<dyn types_registry_sdk::TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new());
    SpecificationRefreshWorker::new(
        unreachable,
        Arc::clone(&db),
        StaticTypesRegistry::empty_config(),
        Duration::from_mins(1),
    )
    .run_once()
    .await;

    assert!(
        manager.get_topic(&topic_id).await.is_some(),
        "a pass that found nothing must not remove what the last one admitted"
    );
}
