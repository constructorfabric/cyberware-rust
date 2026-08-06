use std::sync::Arc;
use std::time::Duration;

use event_broker_sdk::mock::stubs::test_ctx_for_tenant;
use event_broker_sdk::mock::{MockBroker, MockBrokerHandle, PartitionKeyFixture};
use event_broker_sdk::{Event, EventBrokerApi};
use toolkit_security::SecurityContext;
use uuid::Uuid;

pub const TENANT: &str = "00000000-0000-0000-0000-000000000001";

pub struct TopicFixture {
    pub broker: Arc<dyn EventBrokerApi>,
    pub control: MockBrokerHandle,
    pub ctx: SecurityContext,
}

pub struct PublishJson<'a> {
    pub broker: &'a Arc<dyn EventBrokerApi>,
    pub ctx: &'a SecurityContext,
    /// No topic: the broker resolves the destination stream from `event_type`.
    pub event_type: &'a str,
    pub subject: &'a str,
    /// Value the fixture's partition-key pointer will resolve to. The fixture
    /// points at `/data/partition_key`, so this lands in the payload.
    pub partition_key: Option<&'a str>,
    pub partition: Option<u32>,
    pub data: serde_json::Value,
}

/// Where the fixture's event types take their partition key from. The default
/// points at the tenant, which would land every fixture event on one partition;
/// these fixtures need to place events on a chosen partition. `/source` is the
/// member to point at: no test asserts it, so routing leaves the subject and the
/// payload - which tests do assert - exactly as the caller wrote them.
const FIXTURE_PARTITION_POINTER: &str = "/source";

pub async fn topic_fixture(topic: &str, event_type: &str, partitions: u32) -> TopicFixture {
    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(topic, partitions).await;
    control
        .register_event_type(
            topic,
            event_type,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_partition_key(PartitionKeyFixture {
            event_type,
            pointer: FIXTURE_PARTITION_POINTER,
        })
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    TopicFixture {
        broker: Arc::new(mock),
        control,
        ctx: test_ctx_for_tenant(Uuid::parse_str(TENANT).expect("tenant uuid")),
    }
}

pub async fn publish_json(
    broker: &Arc<dyn EventBrokerApi>,
    ctx: &SecurityContext,
    event_type: &str,
    subject: &str,
    partition: Option<u32>,
    data: serde_json::Value,
) {
    publish_json_with_partition_key(PublishJson {
        broker,
        ctx,
        event_type,
        subject,
        partition_key: None,
        partition,
        data,
    })
    .await;
}

pub async fn publish_json_with_partition_key(request: PublishJson<'_>) {
    let PublishJson {
        broker,
        ctx,
        event_type,
        subject,
        partition_key,
        partition,
        data,
    } = request;
    let resolved_partition_key = partition_key
        .map(str::to_owned)
        .or_else(|| partition.map(partition_key_for_two_partition_fixture))
        .unwrap_or_else(|| ctx.subject_tenant_id().to_string());
    broker
        .publish(
            ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: event_type.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                // The fixture's types are partitioned by this member.
                source: resolved_partition_key,
                subject: subject.to_owned(),
                subject_type: "showcase".to_owned(),
                occurred_at: chrono::Utc::now(),
                trace_parent: None,
                data: Some(data),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");
}

fn partition_key_for_two_partition_fixture(target: u32) -> String {
    match target {
        0 => "fixture-partition-key-0-0",
        1 => "fixture-partition-key-1-0",
        _ => panic!("two-partition fixture cannot target partition {target}"),
    }
    .to_owned()
}

pub async fn wait_until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..100 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition was not observed before timeout");
}

/// A fixture with two topics, each with its own event type, for the multi-topic
/// delivery path. A delivered event names no topic, so attributing it correctly
/// depends entirely on its type resolving - and partition 0 exists on both.
pub async fn two_topic_fixture(
    first: (&str, &str),
    second: (&str, &str),
    partitions: u32,
) -> TopicFixture {
    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    for (topic, event_type) in [first, second] {
        control.register_topic(topic, partitions).await;
        control
            .register_event_type(
                topic,
                event_type,
                serde_json::json!({ "type": "object" }),
                &[],
            )
            .await;
        control
            .set_partition_key(PartitionKeyFixture {
                event_type,
                pointer: FIXTURE_PARTITION_POINTER,
            })
            .await;
    }
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    TopicFixture {
        broker: Arc::new(mock),
        control,
        ctx: test_ctx_for_tenant(Uuid::parse_str(TENANT).expect("tenant uuid")),
    }
}
