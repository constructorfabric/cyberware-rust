use crate::consumer::{
    BatchHandlerOutcome, ConsumerBuffering, ConsumerBuilder, ConsumerGroupRef, ConsumerHandle,
    ConsumerHandler, ConsumerListenerSettings, ConsumerProfile, EventBatch, EventTypeRef, Fallback,
    HandlerOutcome, InMemoryOffsetManager, RawEvent, SingleEventHandler, TopicRef,
};
use crate::error::{ConsumerError, EventBrokerError};
use std::time::Duration;
use toolkit_gts::{GTS_ID_PREFIX, gts_id};

struct NoopHandler;

#[async_trait::async_trait]
impl SingleEventHandler for NoopHandler {
    async fn handle(
        &self,
        _event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        Ok(HandlerOutcome::Success)
    }
}

struct NoopBatchHandler;

#[async_trait::async_trait]
impl ConsumerHandler for NoopBatchHandler {
    async fn handle_batch(
        &self,
        _batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        Ok(BatchHandlerOutcome::Success)
    }
}

#[tokio::test]
async fn consumer_ready_starts_without_context_argument() {
    let ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-start"))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(NoopHandler);

    let err = match ready.start().await {
        Ok(_) => panic!("unbound builder cannot open subscriptions"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("broker not wired"),
        "unexpected error: {err}"
    );
}

#[test]
fn consumer_builder_accepts_batch_handler_terminal_method() {
    let _ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-batch"))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(NoopBatchHandler);
}

#[test]
fn consumer_builder_accepts_default_and_routed_handlers() {
    let _ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-routed"))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .default_handler(NoopHandler)
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .event_type(EventTypeRef::gts(gts_id!(
            "cf.core.events.event.v1~example.orders.order_created.x.v1~"
        )))
        .handler(NoopHandler)
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .event_type(EventTypeRef::gts_pattern(format!(
            "{GTS_ID_PREFIX}cf.core.events.event.v1~example.orders.*"
        )))
        .batch_handler(NoopBatchHandler);
}

#[test]
fn consumer_builder_accepts_route_only_with_topic_catch_all() {
    let _ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-route-only"))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .handler(NoopHandler);
}

#[test]
fn consumer_builders_keep_independent_profiles_and_listener_settings() {
    let low_latency = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-independent-low"))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .profile(ConsumerProfile::low_latency())
        .listener_settings(ConsumerListenerSettings {
            timeout: Duration::from_millis(25),
            channel_capacity: 8,
        })
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(NoopHandler);

    let high_throughput = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-independent-high"))
        .topics([gts_id!("cf.core.events.topic.v1~example.payments.x.x.v1")])
        .profile(ConsumerProfile::high_throughput())
        .buffering(ConsumerBuffering {
            partition_capacity: 1024,
            high_watermark: 900,
            low_watermark: 512,
        })
        .listener_settings(ConsumerListenerSettings {
            timeout: Duration::from_millis(250),
            channel_capacity: 64,
        })
        .offset_manager(InMemoryOffsetManager::new(Fallback::Latest))
        .handler(NoopHandler);

    let low_settings = low_latency.builder.effective_settings().unwrap();
    let high_settings = high_throughput.builder.effective_settings().unwrap();

    assert_ne!(low_latency.builder.topics, high_throughput.builder.topics);
    assert_ne!(low_settings.batching, high_settings.batching);
    assert_ne!(low_settings.listener, high_settings.listener);
    assert_eq!(high_settings.buffering.partition_capacity, 1024);
}

#[tokio::test]
async fn routed_consumer_rejects_route_outside_subscription_topics() {
    let ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous(
            "builder-routed-invalid-topic",
        ))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .default_handler(NoopHandler)
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.payments.x.x.v1"
        )))
        .handler(NoopHandler);

    let err = match ready.start().await {
        Ok(_) => panic!("route validation must fail"),
        Err(err) => err,
    };

    assert!(
        matches!(err, EventBrokerError::InvalidConsumerOptions { .. }),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string().contains("not part of the configured"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn routed_consumer_rejects_duplicate_routes() {
    let ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-routed-duplicate"))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .default_handler(NoopHandler)
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .event_type(EventTypeRef::gts(gts_id!(
            "cf.core.events.event.v1~example.orders.order_created.x.v1~"
        )))
        .handler(NoopHandler)
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .event_type(EventTypeRef::gts(gts_id!(
            "cf.core.events.event.v1~example.orders.order_created.x.v1~"
        )))
        .handler(NoopHandler);

    let err = match ready.start().await {
        Ok(_) => panic!("duplicate route validation must fail"),
        Err(err) => err,
    };

    assert!(
        matches!(err, EventBrokerError::InvalidConsumerOptions { .. }),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string().contains("duplicate consumer route"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn routed_consumer_without_default_rejects_incomplete_routes() {
    let ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous(
            "builder-routed-missing-default",
        ))
        .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .event_type(EventTypeRef::gts(gts_id!(
            "cf.core.events.event.v1~example.orders.order_created.x.v1~"
        )))
        .handler(NoopHandler);

    let err = match ready.start().await {
        Ok(_) => panic!("route-only consumer without catch-all must fail"),
        Err(err) => err,
    };

    assert!(
        matches!(err, EventBrokerError::InvalidConsumerOptions { .. }),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string().contains("without a default handler"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn routed_consumer_rejects_missing_subscription_topics() {
    let ready = ConsumerBuilder::new_unbound()
        .group(ConsumerGroupRef::auto_anonymous("builder-routed-no-topics"))
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .route()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .handler(NoopHandler);

    let err = match ready.start().await {
        Ok(_) => panic!("routed consumer without topics must fail"),
        Err(err) => err,
    };

    assert!(
        matches!(err, EventBrokerError::InvalidConsumerOptions { .. }),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string()
            .contains("requires at least one configured topic"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn consumer_handle_exposes_subscription_inspection_and_stop() {
    let handle = ConsumerHandle::from_consumer(super::runtime::Consumer::new());

    assert!(handle.subscription_ids().is_empty());
    handle.stop().await.expect("empty handle stop");
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn multiple_consumer_handles_run_with_independent_lifecycle() {
    use crate::EventBrokerApi;
    use crate::mock::{MockBroker, MockBrokerHandle};

    const ORDERS_TOPIC: &str = gts_id!("cf.core.events.topic.v1~cf.core.orders.topic.v1");
    const PAYMENTS_TOPIC: &str = gts_id!("cf.core.events.topic.v1~cf.core.payments.topic.v1");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(ORDERS_TOPIC, 4).await;
    control.register_topic(PAYMENTS_TOPIC, 4).await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: std::sync::Arc<dyn EventBrokerApi> = std::sync::Arc::new(mock);
    let orders = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("orders-handle"))
        .topics([ORDERS_TOPIC])
        .profile(ConsumerProfile::low_latency())
        .parallelism(2)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(NoopHandler)
        .start()
        .await
        .expect("orders consumer starts");
    let payments = ConsumerBuilder::new(broker)
        .group(ConsumerGroupRef::auto_anonymous("payments-handle"))
        .topics([PAYMENTS_TOPIC])
        .profile(ConsumerProfile::high_throughput())
        .parallelism(1)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Latest))
        .handler(NoopHandler)
        .start()
        .await
        .expect("payments consumer starts");

    wait_for_subscription_count(&orders, 2).await;
    wait_for_subscription_count(&payments, 1).await;

    let order_subscriptions = orders.subscription_ids();
    let payment_subscriptions = payments.subscription_ids();
    assert_eq!(order_subscriptions.len(), 2);
    assert_eq!(payment_subscriptions.len(), 1);
    assert!(
        order_subscriptions
            .iter()
            .all(|id| !payment_subscriptions.contains(id))
    );

    orders.stop().await.expect("orders handle stops");
    payments.stop().await.expect("payments handle stops");
}

#[cfg(feature = "test-util")]
async fn wait_for_subscription_count(handle: &ConsumerHandle, expected: usize) {
    for _ in 0..100 {
        if handle.subscription_ids().len() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "consumer handle exposed {} subscription ids, expected {expected}",
        handle.subscription_ids().len()
    );
}

#[cfg(feature = "db")]
mod tx_typestate {
    #[cfg(feature = "test-util")]
    use std::sync::Arc;
    #[cfg(feature = "test-util")]
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::consumer::{
        CommitOffsetInTx, ConsumerOffsetManager, OffsetManagerError, OffsetStore, Position,
        TxCommitHandle, TxConsumerHandler, TxSingleEventHandler, WithTx,
    };
    use crate::ids::{ConsumerGroupId, TopicId};
    use crate::sequence::Sequence;

    #[derive(Default)]
    struct RecordingTxOffsetManager;

    #[async_trait]
    impl OffsetStore for RecordingTxOffsetManager {
        async fn load_position(
            &self,
            _group: &ConsumerGroupId,
            _topic: &TopicId,
            _partition: u32,
        ) -> Result<Position, OffsetManagerError> {
            Ok(Fallback::Earliest.into())
        }
    }

    #[async_trait]
    impl CommitOffsetInTx for RecordingTxOffsetManager {
        async fn commit_in_tx<TX>(
            &self,
            _txn: &TX,
            _group: &ConsumerGroupId,
            _topic: &TopicId,
            _partition: u32,
            _offset: Sequence,
        ) -> Result<(), OffsetManagerError>
        where
            TX: toolkit_db::secure::DBRunner + Sync,
        {
            Ok(())
        }
    }

    impl ConsumerOffsetManager for RecordingTxOffsetManager {
        type BuilderState = WithTx<Self>;

        fn into_builder_state(self) -> Self::BuilderState {
            WithTx(self)
        }
    }

    #[cfg(feature = "test-util")]
    #[derive(Clone, Default)]
    struct CountingTxOffsetManager {
        commits: Arc<AtomicUsize>,
    }

    #[cfg(feature = "test-util")]
    #[async_trait]
    impl OffsetStore for CountingTxOffsetManager {
        async fn load_position(
            &self,
            _group: &ConsumerGroupId,
            _topic: &TopicId,
            _partition: u32,
        ) -> Result<Position, OffsetManagerError> {
            Ok(Fallback::Earliest.into())
        }
    }

    #[cfg(feature = "test-util")]
    #[async_trait]
    impl CommitOffsetInTx for CountingTxOffsetManager {
        async fn commit_in_tx<TX>(
            &self,
            _txn: &TX,
            _group: &ConsumerGroupId,
            _topic: &TopicId,
            _partition: u32,
            _offset: Sequence,
        ) -> Result<(), OffsetManagerError>
        where
            TX: toolkit_db::secure::DBRunner + Sync,
        {
            self.commits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[cfg(feature = "test-util")]
    impl ConsumerOffsetManager for CountingTxOffsetManager {
        type BuilderState = WithTx<Self>;

        fn into_builder_state(self) -> Self::BuilderState {
            WithTx(self)
        }
    }

    #[cfg(feature = "test-util")]
    struct NoCommitTxHandler;

    #[cfg(feature = "test-util")]
    #[async_trait]
    impl TxSingleEventHandler<CountingTxOffsetManager> for NoCommitTxHandler {
        async fn handle(
            &self,
            _event: RawEvent,
            _attempts: u16,
            _commit: TxCommitHandle<CountingTxOffsetManager>,
        ) -> Result<HandlerOutcome, ConsumerError> {
            Ok(HandlerOutcome::Success)
        }
    }

    struct NoopTxHandler;

    #[async_trait]
    impl TxSingleEventHandler<RecordingTxOffsetManager> for NoopTxHandler {
        async fn handle(
            &self,
            _event: RawEvent,
            _attempts: u16,
            _commit: TxCommitHandle<RecordingTxOffsetManager>,
        ) -> Result<HandlerOutcome, ConsumerError> {
            Ok(HandlerOutcome::Success)
        }
    }

    struct NoopTxBatchHandler;

    #[async_trait]
    impl TxConsumerHandler<RecordingTxOffsetManager> for NoopTxBatchHandler {
        async fn handle_batch(
            &self,
            _batch: &EventBatch<'_>,
            _attempts: u16,
            _commit: TxCommitHandle<RecordingTxOffsetManager>,
        ) -> Result<HandlerOutcome, ConsumerError> {
            Ok(HandlerOutcome::Success)
        }
    }

    #[test]
    fn consumer_builder_accepts_transactional_typestate_quadrants() {
        let _single = ConsumerBuilder::new_unbound()
            .group(ConsumerGroupRef::auto_anonymous("builder-tx-single"))
            .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
            .offset_manager(RecordingTxOffsetManager)
            .handler(NoopTxHandler);

        let _batch = ConsumerBuilder::new_unbound()
            .group(ConsumerGroupRef::auto_anonymous("builder-tx-batch"))
            .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
            .offset_manager(RecordingTxOffsetManager)
            .batch_handler(NoopTxBatchHandler);

        let _routed = ConsumerBuilder::new_unbound()
            .group(ConsumerGroupRef::auto_anonymous("builder-tx-routed"))
            .topics([gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1")])
            .offset_manager(RecordingTxOffsetManager)
            .default_handler(NoopTxHandler)
            .route()
            .topic(TopicRef::gts(gts_id!(
                "cf.core.events.topic.v1~example.orders.x.x.v1"
            )))
            .event_type(EventTypeRef::gts(gts_id!(
                "cf.core.events.event.v1~example.orders.order_created.x.v1~"
            )))
            .batch_handler(NoopTxBatchHandler);
    }

    #[cfg(feature = "test-util")]
    #[tokio::test]
    async fn transactional_consumer_start_paths_return_lifecycle_handles() {
        use crate::EventBrokerApi;
        use crate::mock::{MockBroker, MockBrokerHandle};

        const TOPIC: &str = gts_id!("cf.core.events.topic.v1~cf.core.orders.topic.v1");

        let mock = MockBroker::new();
        let control = MockBrokerHandle::from_broker(&mock);
        control.register_topic(TOPIC, 3).await;
        control
            .set_heartbeat_interval(Duration::from_millis(10))
            .await;
        let broker: std::sync::Arc<dyn EventBrokerApi> = std::sync::Arc::new(mock);

        let single = ConsumerBuilder::new(broker.clone())
            .group(ConsumerGroupRef::auto_anonymous("tx-single-start"))
            .topics([TOPIC])
            .parallelism(1)
            .offset_manager(RecordingTxOffsetManager)
            .handler(NoopTxHandler)
            .start()
            .await
            .expect("transactional single consumer starts");
        let batch = ConsumerBuilder::new(broker.clone())
            .group(ConsumerGroupRef::auto_anonymous("tx-batch-start"))
            .topics([TOPIC])
            .parallelism(1)
            .offset_manager(RecordingTxOffsetManager)
            .batch_handler(NoopTxBatchHandler)
            .start()
            .await
            .expect("transactional batch consumer starts");
        let routed = ConsumerBuilder::new(broker.clone())
            .group(ConsumerGroupRef::auto_anonymous("tx-routed-start"))
            .topics([TOPIC])
            .parallelism(1)
            .offset_manager(RecordingTxOffsetManager)
            .default_handler(NoopTxHandler)
            .route()
            .topic(TopicRef::gts(TOPIC))
            .batch_handler(NoopTxBatchHandler)
            .start()
            .await
            .expect("transactional routed consumer starts");
        wait_for_subscription_count(&single, 1).await;
        wait_for_subscription_count(&batch, 1).await;
        wait_for_subscription_count(&routed, 1).await;

        single.stop().await.expect("single stops");
        batch.stop().await.expect("batch stops");
        routed.stop().await.expect("routed stops");
    }

    #[cfg(feature = "test-util")]
    #[tokio::test]
    async fn transactional_consumer_does_not_auto_commit_when_handler_omits_commit_in_tx() {
        use crate::EventBrokerApi;
        use crate::consumer::ConsumerCommitMode;
        use crate::mock::stubs::test_ctx_for_tenant;
        use crate::mock::{MockBroker, MockBrokerHandle};
        use crate::models::Event;
        use uuid::Uuid;

        const TOPIC: &str = gts_id!("cf.core.events.topic.v1~cf.core.orders.topic.v1");
        const EVENT_TYPE: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.event.v1~");

        let mock = MockBroker::new();
        let control = MockBrokerHandle::from_broker(&mock);
        control.register_topic(TOPIC, 1).await;
        control
            .register_event_type(
                TOPIC,
                EVENT_TYPE,
                serde_json::json!({ "type": "object" }),
                &[],
            )
            .await;
        control
            .set_heartbeat_interval(Duration::from_millis(10))
            .await;
        let broker: std::sync::Arc<dyn EventBrokerApi> = std::sync::Arc::new(mock);
        let ctx =
            test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        let manager = CountingTxOffsetManager::default();
        let commits = manager.commits.clone();

        let handle = ConsumerBuilder::new(broker.clone())
            .group(ConsumerGroupRef::auto_anonymous("tx-no-auto-commit"))
            .topics([TOPIC])
            .commit_mode(ConsumerCommitMode::auto(Duration::from_millis(5)))
            .offset_manager(manager)
            .handler(NoCommitTxHandler)
            .start()
            .await
            .expect("transactional consumer starts");

        wait_for_subscription_count(&handle, 1).await;
        broker
            .publish(
                &ctx,
                &Event {
                    id: Uuid::new_v4(),
                    type_id: EVENT_TYPE.to_owned(),
                    tenant_id: ctx.subject_tenant_id(),
                    source: "consumer.builder.test".to_owned(),
                    subject: "subject-1".to_owned(),
                    subject_type: "test".to_owned(),
                    occurred_at: chrono::Utc::now(),
                    trace_parent: None,
                    data: Some(serde_json::json!({ "ok": true })),
                    partition: None,
                    sequence: None,
                    sequence_time: None,
                    meta: None,
                },
            )
            .await
            .expect("event published");
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.stop().await.expect("consumer stops");
        assert_eq!(commits.load(Ordering::SeqCst), 0);
    }
}
