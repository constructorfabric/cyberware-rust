use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use toolkit_db::outbox::{EnqueueMessage, LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::api::{IngestOutcome, ProducerMode};
use crate::error::EventBrokerError;
use crate::ids::ProducerId;
use crate::models::{Event, ProducerMeta};

use super::db::{DbProducer, UnknownProducerAction};

pub const PRODUCER_OUTBOX_ENVELOPE_VERSION: u16 = 1;
pub const PRODUCER_OUTBOX_PAYLOAD_TYPE: &str =
    "application/vnd.constructorfabric.event-broker.producer-outbox+json;version=1";

type ProducerOutboxCursorKey = (ProducerId, String, u32);
type ProducerOutboxCursorMap = HashMap<ProducerOutboxCursorKey, i64>;
type SharedProducerOutboxCursor = Arc<Mutex<ProducerOutboxCursorMap>>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProducerOutboxEnvelope {
    version: u16,
    event_id: uuid::Uuid,
    #[serde(rename = "type")]
    event_type_id: String,
    topic: String,
    tenant_id: uuid::Uuid,
    source: String,
    subject: String,
    subject_type: String,
    occurred_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    broker_partition: u32,
    producer_mode: ProducerMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    producer_id: Option<ProducerId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<i64>,
    diagnostic_metadata: ProducerOutboxDiagnostics,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProducerOutboxDiagnostics {
    sdk_client_agent: String,
}

impl ProducerOutboxEnvelope {
    pub(crate) fn from_event(
        event: Event,
        topic: String,
        broker_partition: u32,
        producer_mode: ProducerMode,
        producer_id: Option<ProducerId>,
        generation: Option<i64>,
        sdk_client_agent: String,
    ) -> Self {
        Self {
            version: PRODUCER_OUTBOX_ENVELOPE_VERSION,
            event_id: event.id,
            event_type_id: event.type_id,
            topic,
            tenant_id: event.tenant_id,
            source: event.source,
            subject: event.subject,
            subject_type: event.subject_type,
            occurred_at: event.occurred_at,
            trace_parent: event.trace_parent,
            data: event.data,
            broker_partition,
            producer_mode,
            producer_id,
            generation,
            diagnostic_metadata: ProducerOutboxDiagnostics { sdk_client_agent },
        }
    }

    fn to_event(&self, seq: i64, chained_previous: Option<i64>) -> Result<Event, EventBrokerError> {
        if self.version != PRODUCER_OUTBOX_ENVELOPE_VERSION {
            return Err(EventBrokerError::Internal(format!(
                "unsupported producer outbox envelope version {}",
                self.version
            )));
        }
        let meta = match self.producer_mode {
            ProducerMode::Stateless => None,
            ProducerMode::Monotonic => {
                let producer_id = self.producer_id.ok_or_else(|| {
                    EventBrokerError::Internal(
                        "producer outbox envelope is missing producer_id".to_owned(),
                    )
                })?;
                Some(ProducerMeta {
                    version: 1,
                    producer_id: Some(producer_id.0),
                    previous: None,
                    sequence: Some(seq),
                    partition_hint: Some(self.broker_partition),
                })
            }
            ProducerMode::Chained => {
                let producer_id = self.producer_id.ok_or_else(|| {
                    EventBrokerError::Internal(
                        "producer outbox envelope is missing producer_id".to_owned(),
                    )
                })?;
                let previous = chained_previous.ok_or_else(|| {
                    EventBrokerError::Internal(
                        "producer outbox chained publish is missing previous sequence".to_owned(),
                    )
                })?;
                Some(ProducerMeta {
                    version: 1,
                    producer_id: Some(producer_id.0),
                    previous: Some(previous),
                    sequence: Some(seq),
                    partition_hint: Some(self.broker_partition),
                })
            }
        };
        Ok(Event {
            id: self.event_id,
            type_id: self.event_type_id.clone(),
            tenant_id: self.tenant_id,
            source: self.source.clone(),
            subject: self.subject.clone(),
            subject_type: self.subject_type.clone(),
            occurred_at: self.occurred_at,
            trace_parent: self.trace_parent.clone(),
            data: self.data.clone(),
            partition: None,
            sequence: None,
            sequence_time: None,
            offset: None,
            offset_time: None,
            meta,
        })
    }
}

#[derive(Clone)]
pub struct ProducerOutbox {
    producer: DbProducer,
    outbox: Arc<toolkit_db::outbox::Outbox>,
    queue: String,
    partitions: u32,
}

impl ProducerOutbox {
    pub async fn enqueue<E: crate::typed_event::TypedEvent>(
        &self,
        runner: &(impl toolkit_db::secure::DBRunner + Sync + ?Sized),
        event: E,
    ) -> Result<toolkit_db::outbox::OutboxMessageId, EventBrokerError> {
        let (partition, envelope) = self
            .producer
            .outbox_envelope(event, self.partitions)
            .await?;
        let payload = serde_json::to_vec(&envelope).map_err(|err| {
            EventBrokerError::Internal(format!("serialize producer outbox envelope: {err}"))
        })?;
        self.outbox
            .enqueue(
                runner,
                &self.queue,
                partition,
                payload,
                PRODUCER_OUTBOX_PAYLOAD_TYPE,
            )
            .await
            .map_err(|err| EventBrokerError::Internal(format!("producer outbox enqueue: {err}")))
    }

    pub async fn enqueue_batch<E: crate::typed_event::TypedEvent>(
        &self,
        runner: &(impl toolkit_db::secure::DBRunner + Sync + ?Sized),
        events: impl IntoIterator<Item = E>,
    ) -> Result<Vec<toolkit_db::outbox::OutboxMessageId>, EventBrokerError> {
        let mut items = Vec::new();
        for event in events {
            let (partition, envelope) = self
                .producer
                .outbox_envelope(event, self.partitions)
                .await?;
            let payload = serde_json::to_vec(&envelope).map_err(|err| {
                EventBrokerError::Internal(format!("serialize producer outbox envelope: {err}"))
            })?;
            items.push(EnqueueMessage {
                partition,
                payload,
                payload_type: PRODUCER_OUTBOX_PAYLOAD_TYPE,
            });
        }
        self.outbox
            .enqueue_batch(runner, &self.queue, &items)
            .await
            .map_err(|err| {
                EventBrokerError::Internal(format!("producer outbox batch enqueue: {err}"))
            })
    }
}

#[derive(Clone)]
pub struct ProducerOutboxQueue {
    producer: DbProducer,
    queue: String,
    partitions: toolkit_db::outbox::Partitions,
}

impl ProducerOutboxQueue {
    pub(crate) fn new(
        producer: DbProducer,
        queue: String,
        partitions: toolkit_db::outbox::Partitions,
    ) -> Result<Self, EventBrokerError> {
        if queue.trim().is_empty() {
            return Err(EventBrokerError::InvalidProducerOptions {
                detail: "producer outbox queue name must not be empty".to_owned(),
                instance: String::new(),
            });
        }
        Ok(Self {
            producer,
            queue,
            partitions,
        })
    }

    pub fn register(
        &self,
        builder: toolkit_db::outbox::OutboxBuilder,
    ) -> toolkit_db::outbox::LeasedQueueBuilder {
        builder
            .queue(&self.queue, self.partitions)
            .leased(ProducerOutboxProcessor {
                producer: self.producer.clone(),
                cursor_state: Arc::new(Mutex::new(HashMap::new())),
            })
    }

    pub fn bind(&self, handle: &toolkit_db::outbox::OutboxHandle) -> ProducerOutbox {
        ProducerOutbox {
            producer: self.producer.clone(),
            outbox: Arc::clone(handle.outbox()),
            queue: self.queue.clone(),
            partitions: u32::from(self.partitions.count()),
        }
    }

    pub async fn start(
        &self,
        builder: toolkit_db::outbox::OutboxBuilder,
    ) -> Result<ProducerOutboxHandle, EventBrokerError> {
        let handle =
            self.register(builder).start().await.map_err(|err| {
                EventBrokerError::Internal(format!("start producer outbox: {err}"))
            })?;
        let outbox = self.bind(&handle);
        Ok(ProducerOutboxHandle { handle, outbox })
    }
}

pub struct ProducerOutboxHandle {
    handle: toolkit_db::outbox::OutboxHandle,
    outbox: ProducerOutbox,
}

impl ProducerOutboxHandle {
    pub fn outbox(&self) -> &ProducerOutbox {
        &self.outbox
    }

    pub async fn stop(self) {
        self.handle.stop().await;
    }
}

impl DbProducer {
    #[doc(hidden)]
    pub async fn process_outbox_payload_for_test(
        &self,
        payload: Vec<u8>,
        seq: i64,
    ) -> MessageResult {
        self.process_outbox_payload_with_cursor_for_test(payload, seq, None)
            .await
    }

    #[doc(hidden)]
    pub async fn process_outbox_payload_with_cursor_for_test(
        &self,
        payload: Vec<u8>,
        seq: i64,
        initial_chained_previous: Option<i64>,
    ) -> MessageResult {
        let cursor_state = Arc::new(Mutex::new(HashMap::new()));
        if let Some(previous) = initial_chained_previous
            && let Ok(envelope) = serde_json::from_slice::<ProducerOutboxEnvelope>(&payload)
            && let Ok(key) = cursor_key(&envelope)
        {
            cursor_state
                .lock()
                .expect("producer outbox cursor state lock poisoned")
                .insert(key, previous);
        }
        let processor = ProducerOutboxProcessor {
            producer: self.clone(),
            cursor_state,
        };
        let msg = OutboxMessage {
            partition_id: 0,
            seq,
            payload,
            payload_type: PRODUCER_OUTBOX_PAYLOAD_TYPE.to_owned(),
            created_at: chrono::Utc::now(),
            attempts: 0,
        };
        processor.handle(&msg).await
    }
}

struct ProducerOutboxProcessor {
    producer: DbProducer,
    cursor_state: SharedProducerOutboxCursor,
}

impl ProducerOutboxProcessor {
    async fn event_for_message(
        &self,
        envelope: &ProducerOutboxEnvelope,
        seq: i64,
    ) -> Result<Event, EventBrokerError> {
        match envelope.producer_mode {
            ProducerMode::Stateless | ProducerMode::Monotonic => envelope.to_event(seq, None),
            ProducerMode::Chained => {
                let previous = self.cursor_for(envelope).await?;
                envelope.to_event(seq, Some(previous))
            }
        }
    }

    async fn cursor_for(&self, envelope: &ProducerOutboxEnvelope) -> Result<i64, EventBrokerError> {
        let key = cursor_key(envelope)?;
        if let Some(previous) = self
            .cursor_state
            .lock()
            .expect("producer outbox cursor state lock poisoned")
            .get(&key)
            .copied()
        {
            return Ok(previous);
        }
        self.refresh_cursor(key).await
    }

    async fn refresh_cursor(&self, key: ProducerOutboxCursorKey) -> Result<i64, EventBrokerError> {
        let (producer_id, topic, partition) = key.clone();
        let cursors = self
            .producer
            .broker
            .get_producer_cursors(&self.producer.ctx, producer_id)
            .await?;
        let last_sequence = cursors
            .topics
            .into_iter()
            .find(|t| t.topic == topic)
            .and_then(|t| t.partitions.into_iter().find(|p| p.partition == partition))
            .map_or(-1, |p| p.last_sequence);
        self.cursor_state
            .lock()
            .expect("producer outbox cursor state lock poisoned")
            .insert(key, last_sequence);
        Ok(last_sequence)
    }

    async fn handle_sequence_violation(
        &self,
        envelope: &ProducerOutboxEnvelope,
        seq: i64,
    ) -> MessageResult {
        let key = match cursor_key(envelope) {
            Ok(key) => key,
            Err(err) => return MessageResult::Reject(err.to_string()),
        };
        let previous = match self.refresh_cursor(key).await {
            Ok(previous) => previous,
            Err(EventBrokerError::Transport(_))
            | Err(EventBrokerError::RateLimitExceeded { .. })
            | Err(EventBrokerError::RateLimited { .. }) => return MessageResult::Retry,
            Err(err) => return MessageResult::Reject(err.to_string()),
        };
        if seq <= previous {
            return MessageResult::Ok;
        }
        let event = match envelope.to_event(seq, Some(previous)) {
            Ok(event) => event,
            Err(err) => return MessageResult::Reject(err.to_string()),
        };
        self.publish_event(&envelope.topic, event).await
    }

    async fn publish_event(&self, topic: &str, event: Event) -> MessageResult {
        match self
            .producer
            .broker
            .publish(&self.producer.ctx, &event)
            .await
        {
            Ok(IngestOutcome::Accepted | IngestOutcome::Persisted) => {
                if let Some(meta) = event.meta
                    && let (Some(producer_id), Some(sequence), Some(partition)) =
                        (meta.producer_id, meta.sequence, meta.partition_hint)
                {
                    self.cursor_state
                        .lock()
                        .expect("producer outbox cursor state lock poisoned")
                        .insert(
                            (ProducerId(producer_id), topic.to_owned(), partition),
                            sequence,
                        );
                }
                MessageResult::Ok
            }
            Ok(IngestOutcome::Duplicate) => MessageResult::Ok,
            Err(EventBrokerError::SequenceViolation { .. }) => MessageResult::Reject(
                "producer outbox chained sequence divergence persisted after cursor refresh"
                    .to_owned(),
            ),
            Err(EventBrokerError::UnknownProducer { producer_id, .. }) => {
                self.handle_unknown_producer(producer_id).await
            }
            Err(EventBrokerError::Transport(_))
            | Err(EventBrokerError::RateLimitExceeded { .. })
            | Err(EventBrokerError::RateLimited { .. }) => MessageResult::Retry,
            Err(err) => MessageResult::Reject(err.to_string()),
        }
    }

    async fn handle_unknown_producer(&self, producer_id: ProducerId) -> MessageResult {
        match self.producer.handle_unknown_producer(producer_id).await {
            Ok(UnknownProducerAction::Rotated) => MessageResult::Reject(format!(
                "producer_id {producer_id:?} is unknown; registered replacement for future enqueues"
            )),
            Ok(UnknownProducerAction::AlreadyRotated) => MessageResult::Reject(format!(
                "producer_id {producer_id:?} is unknown; local registration already rotated"
            )),
            Ok(UnknownProducerAction::Fail) => {
                MessageResult::Reject(format!("producer_id {producer_id:?} is unknown"))
            }
            Err(EventBrokerError::Transport(_))
            | Err(EventBrokerError::RateLimitExceeded { .. })
            | Err(EventBrokerError::RateLimited { .. }) => MessageResult::Retry,
            Err(err) => MessageResult::Reject(err.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ProducerOutboxProcessor {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let envelope = match serde_json::from_slice::<ProducerOutboxEnvelope>(&msg.payload) {
            Ok(envelope) => envelope,
            Err(err) => return MessageResult::Reject(format!("decode producer envelope: {err}")),
        };
        let event = match self.event_for_message(&envelope, msg.seq).await {
            Ok(event) => event,
            Err(err) => return MessageResult::Reject(err.to_string()),
        };
        match self
            .producer
            .broker
            .publish(&self.producer.ctx, &event)
            .await
        {
            Ok(IngestOutcome::Accepted | IngestOutcome::Persisted) => {
                if let Some(meta) = event.meta
                    && let (Some(producer_id), Some(sequence), Some(partition)) =
                        (meta.producer_id, meta.sequence, meta.partition_hint)
                {
                    self.cursor_state
                        .lock()
                        .expect("producer outbox cursor state lock poisoned")
                        .insert(
                            (ProducerId(producer_id), envelope.topic.clone(), partition),
                            sequence,
                        );
                }
                MessageResult::Ok
            }
            Ok(IngestOutcome::Duplicate) => MessageResult::Ok,
            Err(EventBrokerError::SequenceViolation { .. })
                if envelope.producer_mode == ProducerMode::Chained =>
            {
                self.handle_sequence_violation(&envelope, msg.seq).await
            }
            Err(EventBrokerError::UnknownProducer { producer_id, .. }) => {
                self.handle_unknown_producer(producer_id).await
            }
            Err(EventBrokerError::Transport(_))
            | Err(EventBrokerError::RateLimitExceeded { .. })
            | Err(EventBrokerError::RateLimited { .. }) => MessageResult::Retry,
            Err(err) => MessageResult::Reject(err.to_string()),
        }
    }
}

fn cursor_key(
    envelope: &ProducerOutboxEnvelope,
) -> Result<ProducerOutboxCursorKey, EventBrokerError> {
    let producer_id = envelope.producer_id.ok_or_else(|| {
        EventBrokerError::Internal("producer outbox envelope is missing producer_id".to_owned())
    })?;
    Ok((
        producer_id,
        envelope.topic.clone(),
        envelope.broker_partition,
    ))
}
