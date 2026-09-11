use crate::api::SubscriptionInterest;
use crate::api::{EventBrokerApi, JoinRequest, SubscriptionAssignment};
use crate::ids::ConsumerGroupId;
use crate::mock::stubs::test_ctx_for_tenant;
use crate::mock::{MockBroker, MockBrokerHandle};
use crate::models::CreateConsumerGroupRequest;
use crate::models::Event;
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The problem body a caller would actually receive for `err`, with the
/// per-request fields fixed so a test can assert the whole thing.
///
/// The mock is the reference implementation of this REST API, so what a
/// scenario documents is a body rather than an error variant; matching on the
/// variant would pass while the body said something else entirely.
pub fn problem_json(err: impl Into<toolkit_canonical_errors::CanonicalError>) -> serde_json::Value {
    let problem = toolkit_canonical_errors::Problem::from(err.into())
        .with_instance("/v1/subscriptions/test:seek")
        .with_trace_id("trace-123");
    serde_json::to_value(problem).expect("a problem body serializes")
}

// -- GTS identifier constants used across all mock tests ----------------------

pub const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.audit.v1");
pub const TOPIC2: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.notify.v1");
pub const TOPIC3: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.analytics.v1");
pub const EVT: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.event.v1~");
pub const EVT2: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.event2.v1~");
pub const EVT3: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.event3.v1~");

// -- Helpers -------------------------------------------------------------------

pub async fn broker_with_topic(topic: &str, partitions: u32) -> (MockBroker, MockBrokerHandle) {
    let broker = MockBroker::new();
    let h = broker.handle();
    register_topic_with_event_type(&h, topic, partitions, EVT).await;
    (broker, h)
}

/// Register a topic together with the event type published to it.
///
/// Ingest resolves an event's stream from its type, so a topic that declares no
/// event type can receive nothing - the pair is the smallest publishable setup.
pub async fn register_topic_with_event_type(
    h: &MockBrokerHandle,
    topic: &str,
    partitions: u32,
    type_id: &str,
) {
    h.register_topic(topic, partitions).await;
    h.register_event_type(topic, type_id, serde_json::json!({ "type": "object" }), &[])
        .await;
}

pub fn ctx() -> SecurityContext {
    test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
}

pub fn ctx2() -> SecurityContext {
    test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap())
}

pub fn wire_event(type_id: &str, tenant_id: Uuid) -> Event {
    Event {
        id: Uuid::new_v4(),
        type_id: type_id.to_owned(),
        tenant_id,
        source: "test.mock".to_owned(),
        subject: "test-subject".to_owned(),
        subject_type: "test-type".to_owned(),
        occurred_at: chrono::Utc::now(),
        trace_parent: None,
        data: None,
        partition: None,
        sequence: None,
        sequence_time: None,
        meta: None,
    }
}

pub async fn make_group(ctx: &SecurityContext, broker: &MockBroker) -> ConsumerGroupId {
    broker
        .create_consumer_group(
            ctx,
            CreateConsumerGroupRequest {
                client_agent: "test-agent/1.0".to_owned(),
                description: None,
            },
        )
        .await
        .unwrap()
        .id
}

pub async fn join_group(
    ctx: &SecurityContext,
    broker: &MockBroker,
    group: &ConsumerGroupId,
    topic: &str,
) -> SubscriptionAssignment {
    broker
        .join(
            ctx,
            JoinRequest {
                group: *group,
                client_agent: "test-consumer/1.0".to_owned(),
                interests: vec![SubscriptionInterest {
                    topic: topic.to_owned(),
                    tenant_id: uuid::Uuid::nil(),
                    tenant_depth: crate::api::TenantTraversalDepth::CurrentTenant,
                    barrier_mode: crate::api::BarrierMode::Respect,
                    types: vec!["*".to_owned()],
                    filter: None,
                }],
                session_timeout: Some(std::time::Duration::from_secs(30)),
            },
        )
        .await
        .unwrap()
}

/// Seed every assigned partition of a subscription to `Earliest` - the standard
/// "well-behaved consumer SEEKs before streaming" step (satisfies PositionsNotSet).
pub async fn seek_all_earliest(
    ctx: &SecurityContext,
    broker: &MockBroker,
    sub: &SubscriptionAssignment,
) {
    use crate::Position;
    use crate::api::SeekPosition;
    let positions: Vec<SeekPosition> = sub
        .assigned
        .iter()
        .map(|a| SeekPosition {
            topic: a.topic.clone(),
            partition: a.partition,
            value: Position::Earliest,
        })
        .collect();
    if !positions.is_empty() {
        broker
            .seek(ctx, sub.subscription_id, &positions)
            .await
            .unwrap();
    }
}
