use std::time::Duration;
use toolkit_gts::gts_id;

use uuid::Uuid;

use crate::Position;
use crate::api::{
    BarrierMode, EventBrokerApi, JoinRequest, SeekPosition, SubscriptionInterest,
    TenantTraversalDepth,
};
use crate::mock::MockBroker;
use crate::mock::stubs::test_ctx_for_tenant;
use crate::models::CreateConsumerGroupRequest;

const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.stream.unit.v1");

#[tokio::test]
async fn dropping_unpolled_stream_clears_active_marker() {
    let broker = MockBroker::new();
    broker.handle().register_topic(TOPIC, 1).await;
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let group = broker
        .create_consumer_group(
            &ctx,
            CreateConsumerGroupRequest {
                client_agent: "test-agent/1.0".to_owned(),
                description: None,
            },
        )
        .await
        .unwrap()
        .id;
    let sub = broker
        .join(
            &ctx,
            JoinRequest {
                group,
                client_agent: "test-consumer/1.0".to_owned(),
                interests: vec![SubscriptionInterest {
                    topic: TOPIC.to_owned(),
                    tenant_id: Uuid::nil(),
                    tenant_depth: TenantTraversalDepth::CurrentTenant,
                    barrier_mode: BarrierMode::Respect,
                    types: vec!["*".to_owned()],
                    filter: None,
                }],
                session_timeout: Some(Duration::from_secs(30)),
            },
        )
        .await
        .unwrap();
    broker
        .seek(
            &ctx,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Earliest,
            }],
        )
        .await
        .unwrap();

    let stream = broker.stream(&ctx, sub.subscription_id).await.unwrap();
    drop(stream);

    let second = broker.stream(&ctx, sub.subscription_id).await;
    assert!(
        second.is_ok(),
        "dropping an unpolled stream must clear StreamingInProgress"
    );
}
