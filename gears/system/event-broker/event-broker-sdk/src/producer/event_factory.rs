use chrono::Utc;
use uuid::Uuid;

use crate::error::EventBrokerError;
use crate::models::{Event, ProducerMeta};
use crate::typed_event::TypedEvent;

use super::partitioning::broker_partition;
use super::schema_cache::ProducerSchemaCache;
use super::types::ProducerIdentity;

pub(crate) struct PreparedEvent {
    pub(crate) event: Event,
    /// Resolved from the event type, which owns the topic binding.
    pub(crate) topic: String,
    pub(crate) broker_partition: u32,
}

pub(crate) async fn prepare_event<E: TypedEvent>(
    cache: &ProducerSchemaCache,
    identity: &ProducerIdentity,
    ctx: &toolkit_security::SecurityContext,
    event: E,
    meta_for_partition: impl FnOnce(&str, u32) -> Option<ProducerMeta>,
) -> Result<PreparedEvent, EventBrokerError> {
    let type_id = E::TYPE_ID;
    let subject = event.subject();
    let tenant_id = event.tenant_id().unwrap_or_else(|| ctx.subject_tenant_id());
    let data = serde_json::to_value(&event)
        .map_err(|err| EventBrokerError::Internal(format!("serialize event data: {err}")))?;

    cache.validate_prepared(type_id, &data).await?;
    // The event type owns the topic binding and the partition key, so both come
    // from the prepared type.
    let topic = cache.prepared_topic(type_id).await?;
    let pointer = cache.prepared_partition_key(type_id).await?;

    let mut prepared = Event {
        id: Uuid::now_v7(),
        type_id: type_id.to_owned(),
        tenant_id,
        source: identity.source_ref().to_owned(),
        subject: subject.into_owned(),
        subject_type: E::SUBJECT_TYPE.to_owned(),
        occurred_at: Utc::now(),
        trace_parent: event.trace_parent().map(|value| value.into_owned()),
        data: Some(data),
        partition: None,
        sequence: None,
        sequence_time: None,
        meta: None,
    };

    let partition_count = cache.partition_count(&topic).await?;
    let partition = broker_partition(&prepared.partition_input(&pointer)?, partition_count);
    prepared.meta = meta_for_partition(&topic, partition);

    Ok(PreparedEvent {
        broker_partition: partition,
        topic,
        event: prepared,
    })
}
