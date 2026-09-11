//! `LeasedMessageHandler` draining the ingest outbox (design.md D5). Leases
//! one row -> decodes the `domain::model::Event` it carries (partition
//! already stamped by `IngestServiceImpl::publish_event`, before enqueue) ->
//! resolves the topic's backend via the same `domain::backend::
//! to_sdk_event` conversion the old synchronous path used -> calls
//! `EventBrokerBackend::persist()` as a plain, out-of-transaction call
//! (D5's component-boundary rule: "we always work with backend out-of-tx")
//! -> acks/retries/rejects based on the outcome, and - on success - publishes
//! a `domain::notify` wake-up notification (design.md D6) for delivery's
//! stream loop to pick up on its next iteration.
//! `LeasedMessageHandler::handle`'s own signature (`&self, msg:
//! &OutboxMessage) -> MessageResult`, with no DB handle at all) is what
//! makes that rule structurally true here, not just conventional.

use std::sync::Arc;

use async_trait::async_trait;
use cluster_sdk::ClusterCacheV1;
use cluster_sdk::cache::{PutRequest, Ttl};
use event_broker_sdk::StorageBackendError;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_security::SecurityContext;
use tracing::{error, warn};

use crate::domain::backend::{BackendResolver, to_sdk_event};
use crate::domain::model::Event;
use crate::domain::notify::notification_key;
use crate::domain::specification::SpecificationManager;

/// How long a notification entry lives in `ClusterCacheV1` before expiring.
/// Only matters for backend cleanup, not for whether a watcher observes it -
/// `watch_prefix` fires on the write itself, not on the entry's continued
/// existence - so this just needs to be "long enough that a laggy watcher
/// resubscribing has a moment to still see it," not tuned to any consumer
/// timing.
const NOTIFICATION_TTL: std::time::Duration = std::time::Duration::from_secs(10);

pub struct IngestOutboxHandler {
    spec_manager: Arc<dyn SpecificationManager>,
    backend_resolver: Arc<dyn BackendResolver>,
    cache: ClusterCacheV1,
}

impl IngestOutboxHandler {
    #[must_use]
    pub fn new(
        spec_manager: Arc<dyn SpecificationManager>,
        backend_resolver: Arc<dyn BackendResolver>,
        cache: ClusterCacheV1,
    ) -> Self {
        Self {
            spec_manager,
            backend_resolver,
            cache,
        }
    }

    /// Best-effort: a failure here (surrogate-id resolution or the cache
    /// write itself) is logged and swallowed, never turned into a `Retry`/
    /// `Reject` - the event is already durably persisted by this point, and
    /// the notification is only a wake-up hint (design.md D6), not part of
    /// this handler's durability contract. A missed notification costs
    /// delivery's stream loop one extra heartbeat-interval wait, not a
    /// correctness bug.
    async fn notify(&self, topic_id: &toolkit_gts::GtsInstanceId, partition: i32) {
        // Derived from the identifier, not looked up: the key has to be the
        // same on every instance that writes or watches it, and a value this
        // instance's database assigned is not that.
        let Some(key) = notification_key(topic_id, partition) else {
            warn!(topic = %topic_id, "topic identifier does not derive a notification key");
            return;
        };
        if let Err(err) = self
            .cache
            .put(PutRequest {
                key: &key,
                value: b"",
                ttl: Ttl::Of(NOTIFICATION_TTL),
            })
            .await
        {
            warn!(topic = %topic_id, partition, %err, "publishing delivery notification failed");
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for IngestOutboxHandler {
    #[tracing::instrument(name = "worker", skip_all, fields(worker = "ingest_outbox"))]
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event = match serde_json::from_slice::<Event>(&msg.payload) {
            Ok(event) => event,
            Err(e) => {
                error!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    payload_len = msg.payload.len(),
                    "ingest outbox payload deserialization failed: {e}"
                );
                return MessageResult::Reject(format!("deserialization failed: {e}"));
            }
        };

        let Some(partition) = event.partition else {
            error!(topic = %event.topic, "ingest outbox payload has no stamped partition");
            return MessageResult::Reject("missing partition".to_owned());
        };

        let Some(topic) = self.spec_manager.get_topic(&event.topic).await else {
            error!(topic = %event.topic, "ingest outbox payload references a topic that is no longer registered");
            return MessageResult::Reject(format!("topic '{}' is not registered", event.topic));
        };

        let backend = self.backend_resolver.resolve(&topic);
        let topic_str = topic.id.to_string();
        let sdk_event = to_sdk_event(partition, &event);

        match backend
            .persist(
                &SecurityContext::anonymous(),
                &topic_str,
                partition.cast_unsigned(),
                &[sdk_event],
            )
            .await
        {
            Ok(()) => {
                self.notify(&topic.id, partition).await;
                MessageResult::Ok
            }
            // Transient - the backend (or its connection) may recover; retry.
            Err(
                e @ (StorageBackendError::Unavailable { .. }
                | StorageBackendError::PersistFailed { .. }
                | StorageBackendError::Internal(_)),
            ) => {
                warn!(topic = %topic_str, partition, error = %e, "ingest outbox persist failed transiently - will retry");
                MessageResult::Retry
            }
            // Permanent - retrying with the same config/partition can't help.
            Err(
                e @ (StorageBackendError::InvalidConfig { .. }
                | StorageBackendError::PartitionNotFound { .. }),
            ) => {
                error!(topic = %topic_str, partition, error = %e, "ingest outbox persist failed permanently - dead-lettering");
                MessageResult::Reject(e.to_string())
            }
            // Variants `persist()` cannot produce: read-path errors, and a
            // retention failure, which only a maintenance pass raises.
            Err(
                e @ (StorageBackendError::OffsetOutOfRange { .. }
                | StorageBackendError::ReadFailed { .. }
                | StorageBackendError::RetentionFailed { .. }),
            ) => {
                error!(topic = %topic_str, partition, error = %e, "ingest outbox persist returned an error persist cannot raise - dead-lettering");
                MessageResult::Reject(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use event_broker_sdk::{EventBrokerBackend, Sequence};
    use toolkit_db::DBProvider;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use uuid::Uuid;

    use super::*;
    use crate::domain::backend::SingleBackendResolver;
    use crate::domain::model::Meta;
    use crate::domain::outbox::INGEST_PAYLOAD_TYPE;
    use crate::infra::specification::TypesRegistrySpecificationManager;
    use crate::infra::storage::migrations::Migrator;
    use sqlite_event_broker_plugin::{EventLogPath, SqliteEventBackend};

    const TOPIC_ID: &str = "gts.cf.core.events.topic.v1~example.eb.ingestoutbox.topic.v1";

    async fn test_db() -> Arc<DBProvider<toolkit_db::DbError>> {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "cf-eb-ingestoutbox-test-{}.db",
            Uuid::now_v7().simple()
        ));
        let mut file = path.to_string_lossy().replace('\\', "/");
        if !file.starts_with('/') {
            file.insert(0, '/');
        }
        let dsn = format!("sqlite://{file}?mode=rwc");
        let opts = toolkit_db::ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        };
        let db = toolkit_db::connect_db(&dsn, opts)
            .await
            .expect("connect sqlite");
        toolkit_db::migration_runner::run_migrations_for_testing(
            &db,
            <Migrator as sea_orm_migration::MigratorTrait>::migrations(),
        )
        .await
        .expect("migrations");
        Arc::new(DBProvider::new(db))
    }

    async fn test_handler() -> (IngestOutboxHandler, Arc<SqliteEventBackend>) {
        let db = test_db().await;
        let instance = make_test_instance(
            TOPIC_ID,
            serde_json::json!({
                "id": TOPIC_ID,
                "description": "a topic this test drains the outbox into",
            }),
        );
        let client: Arc<dyn types_registry_sdk::TypesRegistryClient> =
            Arc::new(MockTypesRegistryClient::new().with_instances(vec![instance]));
        crate::infra::specification::bulk_load(
            &client,
            &db,
            &crate::test_support::StaticTypesRegistry::empty_config(),
        )
        .await
        .expect("bulk_load");
        let spec_manager: Arc<dyn SpecificationManager> =
            Arc::new(TypesRegistrySpecificationManager::new(Arc::clone(&db)));
        let backend = Arc::new(
            SqliteEventBackend::open(&EventLogPath::InMemory)
                .await
                .expect("an in-memory event log must open"),
        );
        let resolver = Arc::new(SingleBackendResolver::new(
            Arc::clone(&backend) as Arc<dyn EventBrokerBackend>
        ));
        let (_hub, cluster) = crate::test_support::standalone_event_broker_cluster().await;
        (
            IngestOutboxHandler::new(spec_manager, resolver, cluster.cache),
            backend,
        )
    }

    fn test_event(sequence: i64) -> Event {
        Event {
            id: Uuid::new_v4(),
            r#type: crate::test_support::event_type_id(
                "gts.cf.core.events.event.v1~example.eb.ingestoutbox.event.v1~",
            ),
            topic: toolkit_gts::GtsInstanceId::try_new(TOPIC_ID).unwrap(),
            tenant_id: Uuid::new_v4(),
            source: "test".to_owned(),
            subject: "subject".to_owned(),
            subject_type: "type".to_owned(),
            occurred_at: chrono::Utc::now(),
            trace_parent: None,
            data: serde_json::json!({"k": "v"}),
            meta: Some(Meta {
                version: 1,
                producer_id: Uuid::new_v4(),
                previous: 0,
                sequence,
            }),
            partition: Some(0),
            sequence: None,
            sequence_time: None,
        }
    }

    fn test_message(event: &Event) -> OutboxMessage {
        OutboxMessage {
            partition_id: 0,
            seq: 1,
            payload: serde_json::to_vec(event).expect("serialize event"),
            payload_type: INGEST_PAYLOAD_TYPE.to_owned(),
            created_at: chrono::Utc::now(),
            attempts: 0,
        }
    }

    /// Task 6.5: a redelivery of the exact same outbox row - simulating a
    /// crash between `persist()` succeeding and the row being marked done
    /// - must not double-append, because `SqliteEventBackend::persist`'s
    /// own outbox-retry dedup (design.md D4, task 5.4) compares the
    /// event's producer chain sequence against the stored
    /// `last_chain_sequence` for the partition, not `event.id`.
    #[tokio::test]
    async fn redelivery_after_persist_is_a_safe_noop() {
        let (handler, backend) = test_handler().await;
        let event = test_event(1);
        let msg = test_message(&event);

        let first = handler.handle(&msg).await;
        assert!(
            matches!(first, MessageResult::Ok),
            "first delivery: {first:?}"
        );

        // Redeliver the identical message - as the outbox processor would
        // if the process crashed after persist() committed but before the
        // outbox row was marked done.
        let second = handler.handle(&msg).await;
        assert!(
            matches!(second, MessageResult::Ok),
            "redelivery: {second:?}"
        );

        let events = backend
            .read(
                &SecurityContext::anonymous(),
                TOPIC_ID,
                0,
                Sequence::assigned(0),
                100,
            )
            .await
            .expect("read must succeed");
        assert_eq!(
            events.len(),
            1,
            "redelivery must not durably append the event a second time"
        );
        assert_eq!(events[0].id, event.id);
    }

    #[tokio::test]
    async fn malformed_payload_is_rejected() {
        let (handler, _backend) = test_handler().await;
        let msg = OutboxMessage {
            partition_id: 0,
            seq: 1,
            payload: b"not json".to_vec(),
            payload_type: INGEST_PAYLOAD_TYPE.to_owned(),
            created_at: chrono::Utc::now(),
            attempts: 0,
        };
        let result = handler.handle(&msg).await;
        assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
    }

    #[tokio::test]
    async fn unregistered_topic_is_rejected() {
        let (handler, _backend) = test_handler().await;
        let mut event = test_event(1);
        event.topic = toolkit_gts::GtsInstanceId::try_new(
            "gts.cf.core.events.topic.v1~example.eb.ingestoutbox.unregistered.v1",
        )
        .unwrap();
        let msg = test_message(&event);
        let result = handler.handle(&msg).await;
        assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
    }
}
