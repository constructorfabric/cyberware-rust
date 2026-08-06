use crate::sequence::Sequence;
use std::time::Duration;
use toolkit_gts::{GTS_ID_PREFIX, gts_id};

use uuid::Uuid;

use super::*;
use crate::error::EventBrokerError;
use crate::ids::{ConsumerGroupId, EventTypeId, TopicId};

struct NoSecurityContextHandler;

#[async_trait::async_trait]
impl SingleEventHandler for NoSecurityContextHandler {
    async fn handle(
        &self,
        _event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, EventBrokerError> {
        Ok(HandlerOutcome::Success)
    }
}

struct NoOffsetDecisionRuntimeListener;

#[async_trait::async_trait]
impl ConsumerRuntimeListener for NoOffsetDecisionRuntimeListener {
    async fn on_consumer_event(
        &self,
        _event: &ConsumerRuntimeEvent,
    ) -> Result<(), EventBrokerError> {
        Ok(())
    }
}

#[test]
fn uuid_identity_newtypes_preserve_uuid_values() {
    let topic_uuid = Uuid::new_v4();
    let event_type_uuid = Uuid::new_v4();
    let group_uuid = Uuid::new_v4();

    let topic_id = TopicId::new(topic_uuid);
    let event_type_id = EventTypeId::new(event_type_uuid);
    let group_id = ConsumerGroupId::new(group_uuid);

    assert_eq!(topic_id.as_uuid(), topic_uuid);
    assert_eq!(event_type_id.as_uuid(), event_type_uuid);
    assert_eq!(group_id.as_uuid(), group_uuid);
    assert_eq!(topic_id.to_string(), topic_uuid.to_string());
    assert_eq!(event_type_id.to_string(), event_type_uuid.to_string());
    assert_eq!(group_id.to_string(), group_uuid.to_string());
}

#[test]
fn handler_signature_has_no_security_context_parameter() {
    fn assert_handler<T: SingleEventHandler>() {}

    assert_handler::<NoSecurityContextHandler>();
}

#[test]
fn runtime_listener_signature_has_no_offset_decision_return() {
    fn assert_listener<T: ConsumerRuntimeListener>() {}

    assert_listener::<NoOffsetDecisionRuntimeListener>();
}

#[test]
fn dead_letter_record_exposes_context_fields_for_diagnosis_and_replay() {
    let group_id = ConsumerGroupId::new(Uuid::new_v4());
    let topic = gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1");
    let topic_id = TopicId::from_gts(topic);
    let payload = serde_json::json!({ "order_id": "order-1" });
    let raw = RawEvent {
        id: Uuid::new_v4(),
        type_id: gts_id!("cf.core.events.event.v1~example.orders.order_created.x.v1~").to_owned(),
        topic: topic.to_owned(),
        tenant_id: Uuid::new_v4(),
        subject: "order-1".to_owned(),
        subject_type: "order".to_owned(),
        partition: 3,
        sequence: Sequence::assigned(42),
        offset: Sequence::assigned(42),
        occurred_at: chrono::Utc::now(),
        sequence_time: chrono::Utc::now(),
        trace_parent: Some("00-test".to_owned()),
        data: payload.clone(),
    };

    let record = crate::dlq::DeadLetterRecord::builder(&raw, "schema mismatch")
        .group_id(group_id)
        .topic_id(topic_id)
        .attempts(2)
        .build();

    assert_eq!(record.group_id, Some(group_id));
    assert_eq!(record.topic_id, Some(topic_id));
    assert_eq!(record.topic, topic);
    assert_eq!(record.partition, 3);
    assert_eq!(record.offset, Sequence::assigned(42));
    assert_eq!(record.payload, payload);
    assert_eq!(record.reason, "schema mismatch");
    assert_eq!(record.attempts, Some(2));
}

#[test]
fn refs_convert_from_resolved_ids() {
    let topic_id = TopicId::new(Uuid::new_v4());
    let event_type_id = EventTypeId::new(Uuid::new_v4());
    let group_id = ConsumerGroupId::new(Uuid::new_v4());

    assert_eq!(TopicRef::from(topic_id), TopicRef::Id(topic_id));
    assert_eq!(
        EventTypeRef::from(event_type_id),
        EventTypeRef::Id(event_type_id)
    );
    assert_eq!(
        ConsumerGroupRef::from(group_id),
        ConsumerGroupRef::Id(group_id)
    );
}

#[test]
fn subscription_interest_builder_keeps_types_and_filter_per_topic() {
    let interest = SubscriptionInterest::builder()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .types([
            EventTypeRef::gts(gts_id!(
                "cf.core.events.event.v1~example.orders.order_created.x.v1~"
            )),
            EventTypeRef::gts_pattern(format!(
                "{GTS_ID_PREFIX}cf.core.events.event.v1~example.orders.*"
            )),
        ])
        .filter(SubscriptionFilterRef::cel("tenant_id == $tenant_id"))
        .build()
        .expect("interest should be valid");

    assert_eq!(
        interest.topic,
        TopicRef::gts(gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1"))
    );
    assert_eq!(interest.event_types.len(), 2);
    assert_eq!(
        interest.filter,
        Some(SubscriptionFilterRef::cel("tenant_id == $tenant_id"))
    );
}

#[test]
fn subscription_interest_builder_rejects_missing_required_fields() {
    let missing_topic = SubscriptionInterest::builder()
        .types([EventTypeRef::gts_pattern(format!(
            "{GTS_ID_PREFIX}cf.core.events.event.v1~example.orders.*"
        ))])
        .build()
        .expect_err("topic is required");
    assert!(matches!(
        missing_topic,
        EventBrokerError::InvalidConsumerOptions { .. }
    ));

    let missing_types = SubscriptionInterest::builder()
        .topic(TopicRef::gts(gts_id!(
            "cf.core.events.topic.v1~example.orders.x.x.v1"
        )))
        .build()
        .expect_err("event types are required");
    assert!(matches!(
        missing_types,
        EventBrokerError::InvalidConsumerOptions { .. }
    ));
}

#[test]
fn cel_filter_uses_broker_filter_engine_ref() {
    let filter = SubscriptionFilterRef::cel("event.data.amount > 100");

    assert_eq!(filter.expression, "event.data.amount > 100");
    assert_eq!(
        filter.engine,
        FilterEngineRef::gts(gts_id!(
            "cf.core.events.filter.v1~cf.core.expression.cel.v1"
        ))
    );
}

#[test]
fn consumer_profiles_are_distinct_operating_modes() {
    let default = ConsumerProfile::default_profile();
    let low_latency = ConsumerProfile::low_latency();
    let high_throughput = ConsumerProfile::high_throughput();
    let replay = ConsumerProfile::replay();
    let relaxed = ConsumerProfile::relaxed();

    assert_eq!(default.batching.max_events, 1);
    assert_eq!(low_latency.batching.max_wait, Duration::from_millis(0));
    assert!(low_latency.slow_detection.handler_latency < default.slow_detection.handler_latency);

    assert!(high_throughput.batching.max_events > default.batching.max_events);
    assert!(high_throughput.buffering.partition_capacity > default.buffering.partition_capacity);

    assert!(replay.batching.max_events > high_throughput.batching.max_events);
    assert!(replay.slow_detection.handler_latency > high_throughput.slow_detection.handler_latency);

    assert!(relaxed.listener.channel_capacity < default.listener.channel_capacity);
    assert!(relaxed.retry.base_delay > default.retry.base_delay);
}

#[test]
fn consumer_profile_default_matches_named_default_profile() {
    assert_eq!(
        ConsumerProfile::default(),
        ConsumerProfile::default_profile()
    );
}

#[test]
fn consumer_settings_resolve_explicit_override_over_profile() {
    let profile = ConsumerProfile::high_throughput();
    let override_batching = ConsumerBatching {
        max_events: 7,
        max_wait: Duration::from_millis(25),
    };

    let settings = ConsumerSettings::resolve(
        profile.clone(),
        ConsumerSettingsOverrides {
            batching: Some(override_batching),
            ..ConsumerSettingsOverrides::default()
        },
    );

    assert_eq!(settings.batching, override_batching);
    assert_eq!(settings.buffering, profile.buffering);
    assert_eq!(settings.slow_detection, profile.slow_detection);
    assert_eq!(settings.retry, profile.retry);
    assert_eq!(settings.listener, profile.listener);
}

#[test]
fn consumer_settings_validation_rejects_invalid_values() {
    let mut settings = ConsumerSettings::from_profile(ConsumerProfile::default_profile());
    settings.buffering.low_watermark = settings.buffering.high_watermark + 1;
    assert!(matches!(
        settings.validate(),
        Err(EventBrokerError::InvalidConsumerOptions { .. })
    ));

    let mut settings = ConsumerSettings::from_profile(ConsumerProfile::default_profile());
    settings.batching.max_events = 0;
    assert!(matches!(
        settings.validate(),
        Err(EventBrokerError::InvalidConsumerOptions { .. })
    ));

    let mut settings = ConsumerSettings::from_profile(ConsumerProfile::default_profile());
    settings.listener.channel_capacity = 0;
    assert!(matches!(
        settings.validate(),
        Err(EventBrokerError::InvalidConsumerOptions { .. })
    ));

    let mut settings = ConsumerSettings::from_profile(ConsumerProfile::default_profile());
    settings.listener.timeout = Duration::ZERO;
    assert!(matches!(
        settings.validate(),
        Err(EventBrokerError::InvalidConsumerOptions { .. })
    ));
}

#[test]
fn consumer_commit_mode_defaults_to_auto_commit_interval() {
    assert_eq!(
        ConsumerCommitMode::default(),
        ConsumerCommitMode::Auto {
            interval: Duration::from_secs(20),
        }
    );
    assert_eq!(
        ConsumerCommitMode::auto(Duration::from_secs(5)),
        ConsumerCommitMode::Auto {
            interval: Duration::from_secs(5),
        }
    );
    assert_eq!(ConsumerCommitMode::manual(), ConsumerCommitMode::Manual);
}

#[test]
fn consumer_builder_uses_single_commit_mode_setting() {
    let auto = ConsumerBuilder::new_unbound()
        .commit_mode(ConsumerCommitMode::auto(Duration::from_secs(3)));
    assert_eq!(
        auto.commit_mode,
        ConsumerCommitMode::Auto {
            interval: Duration::from_secs(3),
        }
    );

    let manual = ConsumerBuilder::new_unbound().commit_mode(ConsumerCommitMode::manual());
    assert_eq!(manual.commit_mode, ConsumerCommitMode::Manual);
}
