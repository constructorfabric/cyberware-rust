use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use toolkit_security::SecurityContext;

use crate::api::{EventBrokerApi, IngestOutcome, ProducerMode};
use crate::error::EventBrokerError;
use crate::ids::ProducerId;
use crate::models::{ProducerMeta, ResetScope};
use crate::sdk::EventBrokerSdk;
use crate::typed_event::TypedEvent;

use super::event_factory::prepare_event;
use super::schema_cache::ProducerSchemaCache;
use super::types::{DirectDeduplication, ProducerIdentity, ValidationTiming};

pub struct Missing;
pub struct Has;

type ProducerBuilderState<Broker, Ctx, Identity, Dedup, Topics, Patterns> =
    PhantomData<(Broker, Ctx, Identity, Dedup, Topics, Patterns)>;
type ProducerSequenceKey = (ProducerId, String, u32);
pub(super) type ProducerSequenceCursor = HashMap<ProducerSequenceKey, i64>;
type SharedProducerSequenceCursor = Arc<std::sync::RwLock<ProducerSequenceCursor>>;

pub struct ProducerBuilder<
    Broker = Missing,
    Ctx = Missing,
    Identity = Missing,
    Dedup = Missing,
    Topics = Missing,
    Patterns = Missing,
> {
    broker: Option<Arc<dyn EventBrokerApi>>,
    ctx: Option<SecurityContext>,
    identity: Option<ProducerIdentity>,
    deduplication: Option<DirectDeduplication>,
    topics: Vec<String>,
    event_type_patterns: Vec<String>,
    validation_timing: ValidationTiming,
    broker_partitions: u32,
    _state: ProducerBuilderState<Broker, Ctx, Identity, Dedup, Topics, Patterns>,
}

impl ProducerBuilder {
    pub(crate) fn new() -> Self {
        Self {
            broker: None,
            ctx: None,
            identity: None,
            deduplication: None,
            topics: Vec::new(),
            event_type_patterns: Vec::new(),
            validation_timing: ValidationTiming::Eager,
            broker_partitions: super::schema_cache::DEFAULT_BROKER_PARTITIONS,
            _state: PhantomData,
        }
    }
}

impl<B, C, I, D, T, P> ProducerBuilder<B, C, I, D, T, P> {
    #[must_use]
    pub fn broker(self, broker: Arc<dyn EventBrokerApi>) -> ProducerBuilder<Has, C, I, D, T, P> {
        ProducerBuilder {
            broker: Some(broker),
            ctx: self.ctx,
            identity: self.identity,
            deduplication: self.deduplication,
            topics: self.topics,
            event_type_patterns: self.event_type_patterns,
            validation_timing: self.validation_timing,
            broker_partitions: self.broker_partitions,
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn security_context(self, ctx: SecurityContext) -> ProducerBuilder<B, Has, I, D, T, P> {
        ProducerBuilder {
            broker: self.broker,
            ctx: Some(ctx),
            identity: self.identity,
            deduplication: self.deduplication,
            topics: self.topics,
            event_type_patterns: self.event_type_patterns,
            validation_timing: self.validation_timing,
            broker_partitions: self.broker_partitions,
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn identity(self, identity: ProducerIdentity) -> ProducerBuilder<B, C, Has, D, T, P> {
        ProducerBuilder {
            broker: self.broker,
            ctx: self.ctx,
            identity: Some(identity),
            deduplication: self.deduplication,
            topics: self.topics,
            event_type_patterns: self.event_type_patterns,
            validation_timing: self.validation_timing,
            broker_partitions: self.broker_partitions,
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn deduplication(
        self,
        deduplication: DirectDeduplication,
    ) -> ProducerBuilder<B, C, I, Has, T, P> {
        ProducerBuilder {
            broker: self.broker,
            ctx: self.ctx,
            identity: self.identity,
            deduplication: Some(deduplication),
            topics: self.topics,
            event_type_patterns: self.event_type_patterns,
            validation_timing: self.validation_timing,
            broker_partitions: self.broker_partitions,
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn topics<S, It>(self, topics: It) -> ProducerBuilder<B, C, I, D, Has, P>
    where
        S: Into<String>,
        It: IntoIterator<Item = S>,
    {
        ProducerBuilder {
            broker: self.broker,
            ctx: self.ctx,
            identity: self.identity,
            deduplication: self.deduplication,
            topics: topics.into_iter().map(Into::into).collect(),
            event_type_patterns: self.event_type_patterns,
            validation_timing: self.validation_timing,
            broker_partitions: self.broker_partitions,
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn event_type_patterns<S, It>(self, patterns: It) -> ProducerBuilder<B, C, I, D, T, Has>
    where
        S: Into<String>,
        It: IntoIterator<Item = S>,
    {
        ProducerBuilder {
            broker: self.broker,
            ctx: self.ctx,
            identity: self.identity,
            deduplication: self.deduplication,
            topics: self.topics,
            event_type_patterns: patterns.into_iter().map(Into::into).collect(),
            validation_timing: self.validation_timing,
            broker_partitions: self.broker_partitions,
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn lazy_validation(mut self) -> Self {
        self.validation_timing = ValidationTiming::Lazy;
        self
    }

    /// Partition count the producer assumes the broker gives every topic it
    /// publishes to. It feeds the partition a producer-state chain is keyed by and
    /// the partition hint an outbox envelope carries. A topic does not report a
    /// count, so a producer whose broker is configured away from the default must
    /// declare it here.
    #[must_use]
    pub fn broker_partitions(mut self, partitions: u32) -> Self {
        self.broker_partitions = partitions;
        self
    }
}

impl ProducerBuilder<Has, Has, Has, Has, Has, Has> {
    pub async fn prepare_all(self) -> Result<Producer, EventBrokerError> {
        self.build_inner(true).await
    }

    pub async fn build(self) -> Result<Producer, EventBrokerError> {
        let prepare_all = matches!(self.validation_timing, ValidationTiming::Eager);
        self.build_inner(prepare_all).await
    }

    async fn build_inner(self, prepare_all: bool) -> Result<Producer, EventBrokerError> {
        let broker = self.broker.expect("typestate requires broker");
        let ctx = self.ctx.expect("typestate requires security context");
        let identity = self.identity.expect("typestate requires identity");
        let deduplication = self
            .deduplication
            .expect("typestate requires deduplication");
        validate_non_empty("topics", &self.topics)?;
        validate_non_empty("event_type_patterns", &self.event_type_patterns)?;
        identity.validate()?;
        deduplication.validate()?;

        let cache = Arc::new(ProducerSchemaCache::new(self.broker_partitions));
        if prepare_all {
            cache
                .prepare_all(&broker, &ctx, &self.topics, &self.event_type_patterns)
                .await?;
        }

        let (producer_id, mode) = match deduplication {
            DirectDeduplication::Stateless => (None, ProducerMode::Stateless),
            DirectDeduplication::RegisterOnStart { mode } => {
                let client_agent = identity.client_agent_ref().map_or_else(
                    || EventBrokerSdk::default_client_agent().to_owned(),
                    str::to_owned,
                );
                let producer_id = broker.register_producer(&ctx, mode, &client_agent).await?;
                (Some(producer_id), mode)
            }
            DirectDeduplication::Reuse { mode, producer_id } => (Some(producer_id), mode),
        };

        let producer = Producer {
            broker,
            ctx,
            identity,
            mode,
            producer_id,
            topics: self.topics,
            event_type_patterns: self.event_type_patterns,
            validation_timing: self.validation_timing,
            cache,
            sequence_state: Arc::new(std::sync::RwLock::new(HashMap::new())),
        };
        producer.prime_sequence_state().await?;
        Ok(producer)
    }
}

pub struct Producer {
    broker: Arc<dyn EventBrokerApi>,
    ctx: SecurityContext,
    identity: ProducerIdentity,
    mode: ProducerMode,
    producer_id: Option<ProducerId>,
    topics: Vec<String>,
    event_type_patterns: Vec<String>,
    validation_timing: ValidationTiming,
    cache: Arc<ProducerSchemaCache>,
    sequence_state: SharedProducerSequenceCursor,
}

impl Producer {
    #[must_use]
    pub fn builder() -> ProducerBuilder {
        ProducerBuilder::new()
    }

    pub async fn prepare<E: TypedEvent>(&self) -> Result<(), EventBrokerError> {
        self.cache
            .prepare_one(
                &self.broker,
                &self.ctx,
                &self.topics,
                &self.event_type_patterns,
                E::TYPE_ID,
            )
            .await
    }

    pub async fn prepare_all(&self) -> Result<(), EventBrokerError> {
        self.cache
            .prepare_all(
                &self.broker,
                &self.ctx,
                &self.topics,
                &self.event_type_patterns,
            )
            .await
    }

    pub fn producer_id(&self) -> Option<ProducerId> {
        self.producer_id
    }

    pub async fn publish<E: TypedEvent>(
        &self,
        event: E,
    ) -> Result<IngestOutcome, EventBrokerError> {
        self.publish_inner(event, false).await
    }

    pub async fn publish_persisted<E: TypedEvent>(
        &self,
        event: E,
    ) -> Result<IngestOutcome, EventBrokerError> {
        self.publish_inner(event, true).await
    }

    pub async fn publish_batch<E: TypedEvent>(
        &self,
        events: Vec<E>,
    ) -> Result<Vec<IngestOutcome>, EventBrokerError> {
        let mut prepared = Vec::with_capacity(events.len());
        let mut batch_sequence_state = self
            .sequence_state
            .read()
            .expect("producer sequence state lock poisoned")
            .clone();
        for event in events {
            let event = self
                .prepare_wire_event_with_meta(event, |topic, partition| {
                    self.producer_meta_from_cursor(&batch_sequence_state, topic, partition)
                })
                .await?;
            advance_cursor(
                &mut batch_sequence_state,
                &event.topic,
                event.broker_partition,
                event.event.meta.as_ref(),
            );
            prepared.push(event);
        }
        let wire_events = prepared
            .iter()
            .map(|prepared| prepared.event.clone())
            .collect::<Vec<_>>();
        // A batch is all-or-nothing: the broker reports one outcome for the
        // whole batch. Apply it to every prepared event's local chain state.
        let outcome = self.broker.publish_batch(&self.ctx, &wire_events).await?;
        for prepared in &prepared {
            self.advance_after_acceptance(prepared, outcome).await;
        }
        Ok(vec![outcome; prepared.len()])
    }

    pub async fn reset_chain(&self, scope: ResetScope<'_>) -> Result<(), EventBrokerError> {
        let producer_id = self.producer_id.ok_or_else(|| {
            EventBrokerError::Internal("reset_chain called on a stateless producer".to_owned())
        })?;
        self.broker
            .reset_producer_chain(&self.ctx, producer_id, scope)
            .await
    }

    async fn publish_inner<E: TypedEvent>(
        &self,
        event: E,
        persisted: bool,
    ) -> Result<IngestOutcome, EventBrokerError> {
        let prepared = self.prepare_wire_event(event).await?;
        let outcome = if persisted {
            self.broker.publish_sync(&self.ctx, &prepared.event).await?
        } else {
            self.broker.publish(&self.ctx, &prepared.event).await?
        };
        self.advance_after_acceptance(&prepared, outcome).await;
        Ok(outcome)
    }

    async fn prepare_wire_event<E: TypedEvent>(
        &self,
        event: E,
    ) -> Result<super::event_factory::PreparedEvent, EventBrokerError> {
        self.prepare_wire_event_with_meta(event, |topic, partition| {
            self.producer_meta(topic, partition)
        })
        .await
    }

    async fn prepare_wire_event_with_meta<E: TypedEvent>(
        &self,
        event: E,
        meta_for_partition: impl FnOnce(&str, u32) -> Option<ProducerMeta>,
    ) -> Result<super::event_factory::PreparedEvent, EventBrokerError> {
        self.cache
            .ensure_declared(&self.event_type_patterns, E::TYPE_ID)
            .await?;
        if matches!(self.validation_timing, ValidationTiming::Lazy)
            && !self.cache.is_prepared(E::TYPE_ID).await
        {
            self.prepare::<E>().await?;
        }
        prepare_event(
            &self.cache,
            &self.identity,
            &self.ctx,
            event,
            meta_for_partition,
        )
        .await
    }

    fn producer_meta(&self, topic: &str, partition: u32) -> Option<ProducerMeta> {
        let sequence_state = self
            .sequence_state
            .read()
            .expect("producer sequence state lock poisoned");
        self.producer_meta_from_cursor(&sequence_state, topic, partition)
    }

    fn producer_meta_from_cursor(
        &self,
        cursor: &ProducerSequenceCursor,
        topic: &str,
        partition: u32,
    ) -> Option<ProducerMeta> {
        let producer_id = self.producer_id?;
        let last = cursor
            .get(&(producer_id, topic.to_owned(), partition))
            .copied()
            .unwrap_or(-1);
        producer_meta_for_last(self.mode, producer_id, last, partition)
    }

    async fn advance_after_acceptance(
        &self,
        prepared: &super::event_factory::PreparedEvent,
        outcome: IngestOutcome,
    ) {
        if !matches!(outcome, IngestOutcome::Accepted | IngestOutcome::Persisted) {
            return;
        }
        let Some(meta) = &prepared.event.meta else {
            return;
        };
        let (Some(producer_id), Some(sequence)) = (meta.producer_id, meta.sequence) else {
            return;
        };
        self.sequence_state
            .write()
            .expect("producer sequence state lock poisoned")
            .insert(
                (
                    ProducerId(producer_id),
                    prepared.topic.clone(),
                    prepared.broker_partition,
                ),
                sequence,
            );
    }

    async fn prime_sequence_state(&self) -> Result<(), EventBrokerError> {
        let Some(producer_id) = self.producer_id else {
            return Ok(());
        };
        let cursors = self
            .broker
            .get_producer_cursors(&self.ctx, producer_id)
            .await?;
        self.sequence_state
            .write()
            .expect("producer sequence state lock poisoned")
            .extend(cursors.topics.into_iter().flat_map(|topic| {
                let topic_id = topic.topic;
                topic.partitions.into_iter().map(move |p| {
                    (
                        (producer_id, topic_id.clone(), p.partition),
                        p.last_sequence,
                    )
                })
            }));
        Ok(())
    }
}

pub(super) fn advance_cursor(
    cursor: &mut ProducerSequenceCursor,
    topic: &str,
    partition: u32,
    meta: Option<&ProducerMeta>,
) {
    let Some(meta) = meta else {
        return;
    };
    let (Some(producer_id), Some(sequence)) = (meta.producer_id, meta.sequence) else {
        return;
    };
    cursor.insert(
        (ProducerId(producer_id), topic.to_owned(), partition),
        sequence,
    );
}

pub(super) fn producer_meta_for_last(
    mode: ProducerMode,
    producer_id: ProducerId,
    last: i64,
    partition: u32,
) -> Option<ProducerMeta> {
    match mode {
        ProducerMode::Stateless => None,
        ProducerMode::Monotonic => Some(ProducerMeta {
            version: 1,
            producer_id: Some(producer_id.0),
            previous: None,
            sequence: Some(last + 1),
            partition_hint: Some(partition),
        }),
        ProducerMode::Chained => Some(ProducerMeta {
            version: 1,
            producer_id: Some(producer_id.0),
            previous: Some(last),
            sequence: Some(last + 1),
            partition_hint: Some(partition),
        }),
    }
}

fn validate_non_empty(field: &'static str, values: &[String]) -> Result<(), EventBrokerError> {
    if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
        Err(EventBrokerError::InvalidProducerOptions {
            detail: format!("{field} must contain at least one non-empty entry"),
            instance: String::new(),
        })
    } else {
        Ok(())
    }
}
