use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use toolkit_security::SecurityContext;
use tracing::{trace, warn};

use futures_util::StreamExt;

#[cfg(feature = "db")]
use super::commit::TxCommitHandleParts;
use crate::api::{
    AssignedPartition, EventBrokerApi, JoinRequest, Position, SubscriptionAssignment,
};
use crate::api::{
    BarrierMode, ControlCode, Filter, PartitionPosition, SeekPosition,
    SubscriptionInterest as BrokerSubscriptionInterest, TenantTraversalDepth, WireEvent, WireFrame,
};
use crate::consumer::builder::{
    event_type_ref_to_string, subscription_filter_ref_to_filter, topic_ref_to_string,
};
use crate::consumer::offset_manager::CommitOffset;
#[cfg(feature = "db")]
use crate::consumer::progress::processed_count_from_delivered_offset;
use crate::consumer::progress::processed_count_from_outcome;
use crate::consumer::types::{PartitionFrontier, SubscriptionInterest};
use crate::consumer::{
    BatchHandlerOutcome, ConnectionDropReason, ConsumerCommitMode, ConsumerGroupRef,
    ConsumerHandler, ConsumerRuntimeEvent, ConsumerRuntimeListener, EventBatch,
    PartitionBufferState, PartitionBufferStateSnapshot, PartitionProgress, RawEvent,
    SlowConsumerTrigger,
};
#[cfg(feature = "db")]
use crate::consumer::{CommitOffsetInTx, TxCommitHandle, TxConsumerHandler};
use crate::error::EventBrokerError;
use crate::ids::{ConsumerGroupId, SubscriptionId, TopicId};
use crate::sequence::Sequence;

/// Per-partition in-memory cursor for the contiguous processed frontier.
#[derive(Default)]
pub(crate) struct PartitionCursor {
    frontier: Option<PartitionFrontier>,
    committed: Sequence, // the last offset actually committed to CommitOffset
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TopicPartitionKey {
    topic: String,
    topic_id: TopicId,
    partition: u32,
}

impl TopicPartitionKey {
    pub(crate) fn new(topic: impl Into<String>, topic_id: TopicId, partition: u32) -> Self {
        Self {
            topic: topic.into(),
            topic_id,
            partition,
        }
    }
}

pub(crate) struct PartitionEventBuffer {
    capacity: usize,
    events: VecDeque<RawEvent>,
}

#[derive(Debug)]
pub(crate) struct EnqueuedPartitionBatch {
    pub(crate) buffered_count: usize,
}

impl PartitionEventBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            events: VecDeque::with_capacity(capacity.min(1024)),
        }
    }

    fn push(&mut self, event: RawEvent) -> Result<(), EventBrokerError> {
        if self.events.len() >= self.capacity {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: format!(
                    "partition buffer capacity {} exceeded for topic '{}' partition {}",
                    self.capacity, event.topic, event.partition
                ),
                instance: String::new(),
            });
        }
        self.events.push_back(event);
        Ok(())
    }

    fn len(&self) -> usize {
        self.events.len()
    }

    fn drain_batch(&mut self, max_events: usize) -> Vec<RawEvent> {
        let count = max_events.max(1).min(self.events.len());
        self.events.drain(..count).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlowConsumerReason {
    BufferHighWatermark,
    HandlerLatencyStrikes,
}

fn slow_consumer_trigger(reason: SlowConsumerReason) -> SlowConsumerTrigger {
    match reason {
        SlowConsumerReason::BufferHighWatermark => SlowConsumerTrigger::BufferHighWatermark,
        SlowConsumerReason::HandlerLatencyStrikes => SlowConsumerTrigger::HandlerLatencyStrikes,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlowConsumerSignal {
    pub reason: SlowConsumerReason,
    pub buffered_count: usize,
    pub consecutive_slow_handlers: u16,
    pub latest_observed_offset: Option<Sequence>,
    pub last_delivered_offset: Option<Sequence>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PartitionSlowState {
    buffered_count: usize,
    latest_observed_offset: Option<Sequence>,
    last_delivered_offset: Option<Sequence>,
    consecutive_slow_handlers: u16,
    slow_detected: bool,
}

type PartitionCursors = Arc<RwLock<HashMap<TopicPartitionKey, PartitionCursor>>>;
type PartitionBuffers = Arc<RwLock<HashMap<TopicPartitionKey, PartitionEventBuffer>>>;
type PartitionSlowStates = Arc<RwLock<HashMap<TopicPartitionKey, PartitionSlowState>>>;

#[derive(Clone, Copy)]
struct DispatchState<'a> {
    cursors: &'a PartitionCursors,
    buffers: &'a PartitionBuffers,
    slow_states: &'a PartitionSlowStates,
}

#[derive(Clone, Copy)]
struct ActiveSubscription<'a> {
    ctx: &'a SecurityContext,
    group_id: &'a ConsumerGroupId,
    sub_id: SubscriptionId,
    affected: &'a [AssignedPartition],
}

impl PartitionSlowState {
    pub(crate) fn observe_enqueue(
        &mut self,
        buffered_count: usize,
        latest_observed_offset: Sequence,
        high_watermark: usize,
    ) -> Option<SlowConsumerSignal> {
        self.buffered_count = buffered_count;
        self.latest_observed_offset = Some(latest_observed_offset);
        if buffered_count >= high_watermark {
            self.detect(SlowConsumerReason::BufferHighWatermark)
        } else {
            None
        }
    }

    pub(crate) fn observe_handler_completion(
        &mut self,
        elapsed: Duration,
        handler_latency: Duration,
        handler_strikes: u16,
        last_delivered_offset: Sequence,
    ) -> Option<SlowConsumerSignal> {
        self.last_delivered_offset = Some(last_delivered_offset);
        if elapsed >= handler_latency {
            self.consecutive_slow_handlers = self.consecutive_slow_handlers.saturating_add(1);
        } else {
            self.consecutive_slow_handlers = 0;
        }

        if self.consecutive_slow_handlers >= handler_strikes {
            self.detect(SlowConsumerReason::HandlerLatencyStrikes)
        } else {
            None
        }
    }

    fn detect(&mut self, reason: SlowConsumerReason) -> Option<SlowConsumerSignal> {
        if self.slow_detected {
            return None;
        }
        self.slow_detected = true;
        Some(SlowConsumerSignal {
            reason,
            buffered_count: self.buffered_count,
            consecutive_slow_handlers: self.consecutive_slow_handlers,
            latest_observed_offset: self.latest_observed_offset,
            last_delivered_offset: self.last_delivered_offset,
        })
    }
}

pub(crate) async fn enqueue_partition_batch(
    buffers: &PartitionBuffers,
    key: TopicPartitionKey,
    event: RawEvent,
    capacity: usize,
) -> Result<EnqueuedPartitionBatch, EventBrokerError> {
    let mut guard = buffers.write().await;
    let buffer = guard
        .entry(key)
        .or_insert_with(|| PartitionEventBuffer::new(capacity));
    buffer.push(event)?;
    let buffered_count = buffer.len();
    Ok(EnqueuedPartitionBatch { buffered_count })
}

async fn drain_partition_batch(
    buffers: &PartitionBuffers,
    key: &TopicPartitionKey,
    max_events: usize,
) -> Vec<RawEvent> {
    let mut guard = buffers.write().await;
    guard
        .get_mut(key)
        .map(|buffer| buffer.drain_batch(max_events))
        .unwrap_or_default()
}

async fn next_buffered_partition_batch(
    buffers: &PartitionBuffers,
    max_events: usize,
) -> Option<(TopicPartitionKey, Vec<RawEvent>)> {
    let mut guard = buffers.write().await;
    let key = guard
        .iter()
        .find_map(|(key, buffer)| (!buffer.events.is_empty()).then(|| key.clone()))?;
    let batch = guard
        .get_mut(&key)
        .map(|buffer| buffer.drain_batch(max_events))
        .unwrap_or_default();
    Some((key, batch))
}

pub(crate) async fn emit_runtime_event_to(
    listeners: &[Arc<dyn ConsumerRuntimeListener>],
    timeout: Duration,
    event: ConsumerRuntimeEvent,
) {
    // Listener failures are diagnostic only. They must never decide ack,
    // commit, reject, or stream-drop behavior.
    for listener in listeners {
        match tokio::time::timeout(timeout, listener.on_consumer_event(&event)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %err, "consumer runtime listener failed");
            }
            Err(_) => {
                warn!(
                    timeout_ms = timeout.as_millis(),
                    "consumer runtime listener timed out"
                );
            }
        }
    }
}

fn partition_progress(key: &TopicPartitionKey, offset: Sequence) -> PartitionProgress {
    PartitionProgress {
        topic_id: key.topic_id,
        topic: key.topic.clone(),
        partition: key.partition,
        offset,
    }
}

impl PartitionCursor {
    pub(crate) fn latest_offset(&self) -> Sequence {
        self.frontier
            .as_ref()
            .map(PartitionFrontier::committed)
            .unwrap_or(self.committed)
    }

    pub(crate) fn advance_through_delivered_prefix(&mut self, events: &[RawEvent]) -> Sequence {
        let Some(last) = events.last() else {
            return self.latest_offset();
        };
        let frontier = last.offset;
        self.frontier = Some(PartitionFrontier::new(frontier));
        frontier
    }
}

/// Runs one subscription slot: JOIN, poll, dispatch, retry, re-JOIN.
pub(crate) struct SlotDispatcher<H, OM>
where
    H: ConsumerHandler + 'static,
    OM: CommitOffset + 'static,
{
    pub slot_idx: u32,
    pub broker: Arc<dyn EventBrokerApi>,
    /// Shared across slots: every slot of one consumer resolves the same types.
    pub type_cache: Arc<super::type_cache::ConsumerTypeCache>,
    pub offset_manager: Arc<OM>,
    pub handler: Arc<H>,
    pub group_ref: ConsumerGroupRef,
    pub topics: Vec<String>,
    pub subscription_interests: Vec<SubscriptionInterest>,
    pub tenant_id: Option<uuid::Uuid>,
    pub tenant_depth: TenantTraversalDepth,
    pub barrier_mode: BarrierMode,
    pub event_type_patterns: Vec<String>,
    pub client_agent: String,
    pub session_timeout: Option<Duration>,
    pub filter: Option<Filter>,
    pub heartbeat_drop_threshold: usize,
    pub retry_base: Duration,
    pub retry_max: Duration,
    pub commit_mode: ConsumerCommitMode,
    pub partition_buffer_capacity: usize,
    pub buffer_high_watermark: usize,
    pub buffer_low_watermark: usize,
    pub batch_max_events: usize,
    pub handler_latency: Duration,
    pub handler_strikes: u16,
    pub listeners: Vec<Arc<dyn ConsumerRuntimeListener>>,
    pub listener_timeout: Duration,
    pub max_rejoin_attempts: u32,
    pub subscription_id: Arc<tokio::sync::Mutex<Option<SubscriptionId>>>,
}

#[cfg(feature = "db")]
/// Runs one transactional subscription slot. Unlike [`SlotDispatcher`], this path
/// never creates an auto-commit timer; offset durability is handler-driven through
/// `TxCommitHandle::commit_offset_in_tx`.
pub(crate) struct TxSlotDispatcher<H, OM>
where
    H: TxConsumerHandler<OM> + 'static,
    OM: CommitOffsetInTx + 'static,
{
    pub slot_idx: u32,
    pub broker: Arc<dyn EventBrokerApi>,
    /// Shared across slots: every slot of one consumer resolves the same types.
    pub type_cache: Arc<super::type_cache::ConsumerTypeCache>,
    pub offset_manager: Arc<OM>,
    pub handler: Arc<H>,
    pub group_ref: ConsumerGroupRef,
    pub topics: Vec<String>,
    pub subscription_interests: Vec<SubscriptionInterest>,
    pub tenant_id: Option<uuid::Uuid>,
    pub tenant_depth: TenantTraversalDepth,
    pub barrier_mode: BarrierMode,
    pub event_type_patterns: Vec<String>,
    pub client_agent: String,
    pub session_timeout: Option<Duration>,
    pub filter: Option<Filter>,
    pub heartbeat_drop_threshold: usize,
    pub retry_base: Duration,
    pub retry_max: Duration,
    pub partition_buffer_capacity: usize,
    pub buffer_high_watermark: usize,
    pub buffer_low_watermark: usize,
    pub batch_max_events: usize,
    pub handler_latency: Duration,
    pub handler_strikes: u16,
    pub listeners: Vec<Arc<dyn ConsumerRuntimeListener>>,
    pub listener_timeout: Duration,
    pub max_rejoin_attempts: u32,
    pub subscription_id: Arc<tokio::sync::Mutex<Option<SubscriptionId>>>,
}

fn build_join_interests(
    subscription_interests: &[SubscriptionInterest],
    topics: &[String],
    tenant_id: Option<uuid::Uuid>,
    tenant_depth: TenantTraversalDepth,
    barrier_mode: BarrierMode,
    event_type_patterns: &[String],
    filter: Option<Filter>,
) -> Result<Vec<BrokerSubscriptionInterest>, EventBrokerError> {
    let tenant_id = tenant_id.unwrap_or_else(uuid::Uuid::nil);
    if !subscription_interests.is_empty() {
        return subscription_interests
            .iter()
            .map(|interest| {
                let mut builder = BrokerSubscriptionInterest::builder()
                    .topic(topic_ref_to_string(&interest.topic))
                    .tenant_id(tenant_id)
                    .tenant_depth(tenant_depth)
                    .barrier_mode(barrier_mode)
                    .types(
                        interest
                            .event_types
                            .iter()
                            .map(event_type_ref_to_string)
                            .collect::<Vec<_>>(),
                    );
                if let Some(filter) = interest.filter.as_ref() {
                    builder = builder.filter(subscription_filter_ref_to_filter(filter));
                }
                builder.build()
            })
            .collect();
    }

    let event_type_patterns = if event_type_patterns.is_empty() {
        vec!["*".to_owned()]
    } else {
        event_type_patterns.to_vec()
    };
    topics
        .iter()
        .map(|topic| {
            let mut builder = BrokerSubscriptionInterest::builder()
                .topic(topic.clone())
                .tenant_id(tenant_id)
                .tenant_depth(tenant_depth)
                .barrier_mode(barrier_mode)
                .types(event_type_patterns.clone());
            if let Some(filter) = filter.clone() {
                builder = builder.filter(filter);
            }
            builder.build()
        })
        .collect()
}

#[cfg(feature = "db")]
impl<H, OM> TxSlotDispatcher<H, OM>
where
    H: TxConsumerHandler<OM> + 'static,
    OM: CommitOffsetInTx + 'static,
{
    pub async fn run(
        &self,
        ctx: Arc<SecurityContext>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), EventBrokerError> {
        trace!(
            slot_idx = self.slot_idx,
            "transactional slot dispatcher starting"
        );
        let group_id = self.ensure_group(&ctx).await?;
        let mut assignment = self.join_subscription(&ctx, &group_id).await?;
        {
            let mut guard = self.subscription_id.lock().await;
            *guard = Some(assignment.subscription_id);
        }

        self.resolve_and_seek(
            &ctx,
            &group_id,
            assignment.subscription_id,
            &assignment.assigned,
        )
        .await?;

        let cursors: PartitionCursors = Arc::new(RwLock::new(HashMap::new()));
        let buffers: PartitionBuffers = Arc::new(RwLock::new(HashMap::new()));
        let slow_states: PartitionSlowStates = Arc::new(RwLock::new(HashMap::new()));
        let mut consecutive_failures = 0u32;
        'outer: loop {
            if cancel.is_cancelled() {
                break;
            }

            let sub_id = assignment.subscription_id;
            let mut stream = match self.broker.stream(&ctx, sub_id).await {
                Ok(s) => s,
                Err(EventBrokerError::PositionsNotSet { unseeded, .. }) => {
                    consecutive_failures += 1;
                    if consecutive_failures > self.max_rejoin_attempts {
                        return Err(EventBrokerError::SubscriptionRecoveryExhausted {
                            attempts: consecutive_failures,
                            detail: "stream open: PositionsNotSet recovery exhausted".into(),
                            instance: String::new(),
                        });
                    }
                    let slots = self.slots_for_unseeded(&unseeded);
                    self.resolve_and_seek(&ctx, &group_id, sub_id, &slots)
                        .await?;
                    continue;
                }
                Err(e) => {
                    warn!(
                        slot_idx = self.slot_idx,
                        error = %e,
                        "transactional stream open failed; re-joining"
                    );
                    assignment = self
                        .rejoin(&ctx, &group_id, sub_id, &mut consecutive_failures)
                        .await?;
                    self.resolve_and_seek(
                        &ctx,
                        &group_id,
                        assignment.subscription_id,
                        &assignment.assigned,
                    )
                    .await?;
                    continue;
                }
            };

            let mut consecutive_heartbeats: usize = 0;
            consecutive_failures = 0;
            let mut drain_before_rejoin = false;

            while let Some(frame_result) = stream.next().await {
                if cancel.is_cancelled() {
                    break 'outer;
                }
                match frame_result {
                    Ok(WireFrame::Event(event)) => {
                        consecutive_heartbeats = 0;
                        if self
                            .dispatch_event(
                                event,
                                DispatchState {
                                    cursors: &cursors,
                                    buffers: &buffers,
                                    slow_states: &slow_states,
                                },
                                ActiveSubscription {
                                    ctx: &ctx,
                                    group_id: &group_id,
                                    sub_id,
                                    affected: &assignment.assigned,
                                },
                            )
                            .await
                        {
                            drain_before_rejoin = true;
                            break;
                        }
                    }
                    Ok(WireFrame::Heartbeat { .. }) => {
                        consecutive_heartbeats += 1;
                        if consecutive_heartbeats >= self.heartbeat_drop_threshold {
                            trace!(
                                slot_idx = self.slot_idx,
                                threshold = self.heartbeat_drop_threshold,
                                "transactional drop-on-Nth-heartbeat triggered"
                            );
                            break;
                        }
                    }
                    Ok(WireFrame::Control {
                        code,
                        positions,
                        reason,
                    }) => {
                        self.observe_positions(&positions).await;
                        if code == ControlCode::Terminal {
                            trace!(
                                slot_idx = self.slot_idx,
                                ?reason,
                                "transactional terminal control frame; re-JOIN"
                            );
                            break;
                        }
                    }
                    Ok(WireFrame::Topology {
                        topology_version: _,
                        assigned,
                    }) => {
                        self.observe_positions(&assigned).await;
                    }
                    Err(EventBrokerError::Transport(_)) => break,
                    Err(e) => {
                        warn!(
                            slot_idx = self.slot_idx,
                            error = %e,
                            "transactional stream frame error; ending stream"
                        );
                        break;
                    }
                }
            }

            if cancel.is_cancelled() {
                break;
            }
            if drain_before_rejoin {
                self.drain_subscription_buffers(
                    DispatchState {
                        cursors: &cursors,
                        buffers: &buffers,
                        slow_states: &slow_states,
                    },
                    ActiveSubscription {
                        ctx: &ctx,
                        group_id: &group_id,
                        sub_id,
                        affected: &assignment.assigned,
                    },
                )
                .await;
                let _ = self.broker.leave(&ctx, sub_id).await;
            }
            self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionTerminated {
                subscription_id: sub_id,
                reason: ConnectionDropReason::StreamTerminated,
            })
            .await;
            assignment = self
                .rejoin(&ctx, &group_id, sub_id, &mut consecutive_failures)
                .await?;
            self.resolve_and_seek(
                &ctx,
                &group_id,
                assignment.subscription_id,
                &assignment.assigned,
            )
            .await?;
        }

        let _ = self.broker.leave(&ctx, assignment.subscription_id).await;
        Ok(())
    }

    async fn dispatch_event(
        &self,
        wire: WireEvent,
        state: DispatchState<'_>,
        active: ActiveSubscription<'_>,
    ) -> bool {
        // A topic is what an event's type declares, so it is resolved rather than
        // read off the frame. A type this consumer has not seen costs one call
        // here and none afterwards.
        let topic = match self
            .type_cache
            .topic_of(&self.broker, active.ctx, &wire.type_id)
            .await
        {
            Ok(topic) => topic.as_ref().to_owned(),
            Err(err) => {
                warn!(
                    error = %err,
                    type_id = %wire.type_id,
                    "cannot attribute a delivered event to a topic; skipping it"
                );
                return false;
            }
        };
        let topic_id = TopicId::from_gts(&topic);
        let key = TopicPartitionKey::new(topic.clone(), topic_id, wire.partition);
        let raw = RawEvent {
            id: wire.id,
            type_id: wire.type_id.clone(),
            topic,
            tenant_id: wire.tenant_id,
            subject: wire.subject,
            subject_type: wire.subject_type,
            partition: wire.partition,
            sequence: wire.sequence,
            offset: wire.sequence,
            occurred_at: wire.occurred_at,
            sequence_time: wire.sequence_time,
            trace_parent: wire.trace_parent,
            data: wire.data,
        };
        let enqueued = match enqueue_partition_batch(
            state.buffers,
            key.clone(),
            raw,
            self.partition_buffer_capacity,
        )
        .await
        {
            Ok(events) => events,
            Err(e) => {
                warn!(error = %e, "transactional partition buffer rejected event");
                return false;
            }
        };
        if self
            .observe_enqueue_slow_signal(
                state,
                key.clone(),
                enqueued.buffered_count,
                wire.sequence,
                active,
            )
            .await
        {
            return true;
        }
        let batch_events = drain_partition_batch(state.buffers, &key, self.batch_max_events).await;
        self.process_batch(state, key, batch_events, active).await
    }

    async fn drain_subscription_buffers(
        &self,
        state: DispatchState<'_>,
        active: ActiveSubscription<'_>,
    ) {
        trace!(
            slot_idx = self.slot_idx,
            low_watermark = self.buffer_low_watermark,
            "draining transactional buffered events before re-JOIN"
        );
        loop {
            let Some((key, batch_events)) =
                next_buffered_partition_batch(state.buffers, self.batch_max_events).await
            else {
                break;
            };
            let _ = self.process_batch(state, key, batch_events, active).await;
        }
    }

    async fn process_batch(
        &self,
        state: DispatchState<'_>,
        key: TopicPartitionKey,
        batch_events: Vec<RawEvent>,
        active: ActiveSubscription<'_>,
    ) -> bool {
        if batch_events.is_empty() {
            return false;
        }
        let batch_partition = batch_events[0].partition;
        let batch_offset = batch_events
            .last()
            .map(|event| event.offset)
            .unwrap_or_else(|| batch_events[0].offset);
        let topic_id = key.topic_id;

        let mut attempts: u16 = 1;
        let mut backoff = self.retry_base;

        loop {
            self.emit_runtime_event(ConsumerRuntimeEvent::HandlerBatchStarted {
                topic_id,
                topic: key.topic.clone(),
                partition: batch_partition,
                len: batch_events.len(),
            })
            .await;
            let commit_handle = TxCommitHandle::new(TxCommitHandleParts {
                partition: batch_partition,
                offsets: batch_events.iter().map(|event| event.offset).collect(),
                offset_manager: self.offset_manager.clone(),
                group: *active.group_id,
                topic: topic_id,
            });
            let committed_offset = commit_handle.committed_offset.clone();
            let batch = EventBatch::new(&batch_events);
            let started_at = Instant::now();
            match self
                .handler
                .handle_batch(&batch, attempts, commit_handle)
                .await
            {
                Ok(crate::consumer::HandlerOutcome::Success) => {
                    let committed_offset_result = match committed_offset.lock() {
                        Ok(guard) => Ok(*guard),
                        Err(_) => Err("tx commit state mutex poisoned".to_owned()),
                    };
                    let committed_offset = match committed_offset_result {
                        Ok(committed_offset) => committed_offset,
                        Err(error) => {
                            self.emit_runtime_event(ConsumerRuntimeEvent::HandlerFailed {
                                topic_id,
                                topic: key.topic.clone(),
                                partition: batch_partition,
                                error,
                            })
                            .await;
                            return true;
                        }
                    };
                    let Some(committed_offset) = committed_offset else {
                        self.emit_runtime_event(ConsumerRuntimeEvent::HandlerFailed {
                            topic_id,
                            topic: key.topic.clone(),
                            partition: batch_partition,
                            error: "transactional handler returned success without committing an offset".to_owned(),
                        })
                        .await;
                        return true;
                    };
                    let processed_count = match processed_count_from_delivered_offset(
                        committed_offset,
                        &batch_events,
                    ) {
                        Ok(count) => count,
                        Err(err) => {
                            self.emit_runtime_event(ConsumerRuntimeEvent::HandlerFailed {
                                topic_id,
                                topic: key.topic.clone(),
                                partition: batch_partition,
                                error: err.to_string(),
                            })
                            .await;
                            return true;
                        }
                    };
                    let outcome = if processed_count == batch_events.len() {
                        BatchHandlerOutcome::Success
                    } else {
                        BatchHandlerOutcome::AdvanceThrough {
                            offset: committed_offset,
                        }
                    };
                    self.emit_runtime_event(ConsumerRuntimeEvent::HandlerBatchCompleted {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        outcome: outcome.clone(),
                    })
                    .await;
                    let drop_stream = self
                        .observe_handler_slow_signal(
                            state,
                            key.clone(),
                            started_at.elapsed(),
                            batch_offset,
                            active,
                        )
                        .await;
                    let frontier = {
                        let mut guard = state.cursors.write().await;
                        let c = guard.entry(key.clone()).or_default();
                        let frontier =
                            c.advance_through_delivered_prefix(&batch_events[..processed_count]);
                        c.committed = frontier;
                        frontier
                    };
                    self.emit_runtime_event(ConsumerRuntimeEvent::OffsetCommitted {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        offset: frontier,
                    })
                    .await;
                    self.emit_runtime_event(ConsumerRuntimeEvent::ProgressAdvanced {
                        subscription_id: active.sub_id,
                        progress: vec![partition_progress(&key, frontier)],
                    })
                    .await;
                    return drop_stream;
                }
                Ok(crate::consumer::HandlerOutcome::Retry { reason }) => {
                    self.emit_runtime_event(ConsumerRuntimeEvent::HandlerBatchCompleted {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        outcome: BatchHandlerOutcome::Retry {
                            reason: reason.clone(),
                        },
                    })
                    .await;
                    if self
                        .observe_handler_slow_signal(
                            state,
                            key.clone(),
                            started_at.elapsed(),
                            batch_offset,
                            active,
                        )
                        .await
                    {
                        return true;
                    }
                    trace!(
                        reason,
                        attempts, "transactional handler returned Retry; backing off"
                    );
                    self.emit_runtime_event(ConsumerRuntimeEvent::RetryScheduled {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        attempt: attempts,
                        delay: backoff,
                    })
                    .await;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.retry_max);
                    attempts = attempts.saturating_add(1);
                }
                Err(e) => {
                    self.emit_runtime_event(ConsumerRuntimeEvent::HandlerFailed {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        error: e.to_string(),
                    })
                    .await;
                    if self
                        .observe_handler_slow_signal(
                            state,
                            key.clone(),
                            started_at.elapsed(),
                            batch_offset,
                            active,
                        )
                        .await
                    {
                        return true;
                    }
                    warn!(error = %e, "transactional handler returned Err; treating as Retry");
                    self.emit_runtime_event(ConsumerRuntimeEvent::RetryScheduled {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        attempt: attempts,
                        delay: backoff,
                    })
                    .await;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.retry_max);
                    attempts = attempts.saturating_add(1);
                }
            }
        }
    }

    async fn observe_enqueue_slow_signal(
        &self,
        state: DispatchState<'_>,
        key: TopicPartitionKey,
        buffered_count: usize,
        latest_observed_offset: Sequence,
        active: ActiveSubscription<'_>,
    ) -> bool {
        let mut guard = state.slow_states.write().await;
        let signal = guard.entry(key.clone()).or_default().observe_enqueue(
            buffered_count,
            latest_observed_offset,
            self.buffer_high_watermark,
        );
        drop(guard);
        if let Some(signal) = signal {
            warn!(
                slot_idx = self.slot_idx,
                topic = %key.topic,
                partition = key.partition,
                reason = ?signal.reason,
                buffered_count = signal.buffered_count,
                latest_observed_offset = ?signal.latest_observed_offset,
                "transactional slow consumer predicate detected"
            );
            self.emit_slow_consumer_events(
                *active.group_id,
                active.sub_id,
                key,
                signal,
                active.affected,
            )
            .await;
            return true;
        }
        false
    }

    async fn observe_handler_slow_signal(
        &self,
        state: DispatchState<'_>,
        key: TopicPartitionKey,
        elapsed: Duration,
        last_delivered_offset: Sequence,
        active: ActiveSubscription<'_>,
    ) -> bool {
        let mut guard = state.slow_states.write().await;
        let signal = guard
            .entry(key.clone())
            .or_default()
            .observe_handler_completion(
                elapsed,
                self.handler_latency,
                self.handler_strikes,
                last_delivered_offset,
            );
        drop(guard);
        if let Some(signal) = signal {
            warn!(
                slot_idx = self.slot_idx,
                topic = %key.topic,
                partition = key.partition,
                reason = ?signal.reason,
                consecutive_slow_handlers = signal.consecutive_slow_handlers,
                last_delivered_offset = ?signal.last_delivered_offset,
                "transactional slow consumer predicate detected"
            );
            self.emit_slow_consumer_events(
                *active.group_id,
                active.sub_id,
                key,
                signal,
                active.affected,
            )
            .await;
            return true;
        }
        false
    }

    async fn emit_slow_consumer_events(
        &self,
        group_id: ConsumerGroupId,
        sub_id: SubscriptionId,
        key: TopicPartitionKey,
        signal: SlowConsumerSignal,
        affected: &[AssignedPartition],
    ) {
        let trigger = slow_consumer_trigger(signal.reason);
        let state = PartitionBufferStateSnapshot {
            group_id,
            subscription_id: sub_id,
            topic_id: key.topic_id,
            topic: key.topic.clone(),
            partition: key.partition,
            state: PartitionBufferState::SlowDetected,
            trigger: Some(trigger),
            buffered_count: signal.buffered_count,
            capacity: self.partition_buffer_capacity,
            latest_observed_offset: signal.latest_observed_offset,
            last_delivered_offset: signal.last_delivered_offset,
            consecutive_slow_handlers: signal.consecutive_slow_handlers,
        };
        self.emit_runtime_event(ConsumerRuntimeEvent::PartitionBufferStateChanged { state })
            .await;
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionConnectionDropped {
            subscription_id: sub_id,
            reason: ConnectionDropReason::SlowConsumer {
                topic_id: key.topic_id,
                topic: key.topic,
                partition: key.partition,
                trigger,
            },
            affected: affected.to_vec(),
        })
        .await;
    }

    async fn emit_runtime_event(&self, event: ConsumerRuntimeEvent) {
        emit_runtime_event_to(&self.listeners, self.listener_timeout, event).await;
    }

    async fn resolve_and_seek(
        &self,
        ctx: &SecurityContext,
        group_id: &ConsumerGroupId,
        subscription_id: SubscriptionId,
        slots: &[AssignedPartition],
    ) -> Result<(), EventBrokerError> {
        if slots.is_empty() {
            return Ok(());
        }
        let mut positions: Vec<SeekPosition> = Vec::with_capacity(slots.len());
        for slot in slots {
            let topic = slot.topic.clone();
            let topic_id = TopicId::from_gts(&topic);
            let value = self
                .offset_manager
                .load_position(group_id, &topic_id, slot.partition)
                .await?;
            self.emit_runtime_event(ConsumerRuntimeEvent::OffsetLoaded {
                topic_id,
                topic: topic.clone(),
                partition: slot.partition,
                position: value.clone(),
            })
            .await;
            positions.push(SeekPosition {
                topic,
                partition: slot.partition,
                value,
            });
        }
        self.broker
            .seek(ctx, subscription_id, &positions)
            .await
            .map(|_| ())
    }

    async fn observe_positions(&self, positions: &[PartitionPosition]) {
        for p in positions {
            trace!(
                topic = p.topic.as_ref(),
                partition = p.partition,
                offset = %p.offset,
                last_examined = %p.last_examined,
                "transactional position observed without out-of-tx commit"
            );
        }
    }

    fn slots_for_unseeded(&self, unseeded: &[(String, u32)]) -> Vec<AssignedPartition> {
        unseeded
            .iter()
            .map(|(topic, partition)| AssignedPartition {
                topic: topic.clone(),
                partition: *partition,
            })
            .collect()
    }

    async fn ensure_group(
        &self,
        ctx: &SecurityContext,
    ) -> Result<ConsumerGroupId, EventBrokerError> {
        match &self.group_ref {
            ConsumerGroupRef::Id(id) => Ok(*id),
            ConsumerGroupRef::Gts(gts) => Err(EventBrokerError::InvalidConsumerOptions {
                detail: format!(
                    "consumer group GTS reference '{gts}' must be resolved before startup"
                ),
                instance: String::new(),
            }),
            ConsumerGroupRef::AutoAnonymous { alias } => {
                let group = self
                    .broker
                    .create_consumer_group(
                        ctx,
                        crate::models::CreateConsumerGroupRequest {
                            client_agent: alias.clone(),
                            description: None,
                        },
                    )
                    .await?;
                Ok(group.id)
            }
        }
    }

    async fn join_subscription(
        &self,
        ctx: &SecurityContext,
        group_id: &ConsumerGroupId,
    ) -> Result<SubscriptionAssignment, EventBrokerError> {
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionJoining {
            group_id: *group_id,
        })
        .await;
        let interests = build_join_interests(
            &self.subscription_interests,
            &self.topics,
            self.tenant_id,
            self.tenant_depth,
            self.barrier_mode,
            &self.event_type_patterns,
            self.filter.clone(),
        )?;

        let assignment = self
            .broker
            .join(
                ctx,
                JoinRequest {
                    group: *group_id,
                    client_agent: self.client_agent.clone(),
                    interests,
                    session_timeout: self.session_timeout,
                },
            )
            .await?;
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionStarted {
            group_id: *group_id,
            subscription_id: assignment.subscription_id,
            assigned: assignment.assigned.clone(),
        })
        .await;
        self.emit_runtime_event(ConsumerRuntimeEvent::AssignmentChanged {
            subscription_id: assignment.subscription_id,
            assigned: assignment.assigned.clone(),
        })
        .await;
        Ok(assignment)
    }

    async fn rejoin(
        &self,
        ctx: &SecurityContext,
        group_id: &ConsumerGroupId,
        previous_subscription_id: SubscriptionId,
        consecutive_failures: &mut u32,
    ) -> Result<SubscriptionAssignment, EventBrokerError> {
        *consecutive_failures += 1;
        if *consecutive_failures > self.max_rejoin_attempts {
            return Err(EventBrokerError::SubscriptionRecoveryExhausted {
                attempts: *consecutive_failures,
                detail: "max transactional re-JOIN attempts exceeded".into(),
                instance: String::new(),
            });
        }
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionRejoining {
            group_id: *group_id,
            previous_subscription_id,
        })
        .await;
        tokio::time::sleep(Duration::from_millis(250) * (*consecutive_failures)).await;
        let assignment = self.join_subscription(ctx, group_id).await?;
        {
            let mut guard = self.subscription_id.lock().await;
            *guard = Some(assignment.subscription_id);
        }
        Ok(assignment)
    }
}

impl<H, OM> SlotDispatcher<H, OM>
where
    H: ConsumerHandler + 'static,
    OM: CommitOffset + 'static,
{
    pub async fn run(
        &self,
        ctx: Arc<SecurityContext>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), EventBrokerError> {
        trace!(slot_idx = self.slot_idx, "slot dispatcher starting");
        let group_id = self.ensure_group(&ctx).await?;
        let mut assignment = self.join_subscription(&ctx, &group_id).await?;
        {
            let mut guard = self.subscription_id.lock().await;
            *guard = Some(assignment.subscription_id);
        }

        // Pre-stream SEEK: resolve a starting position per assigned partition
        // via OffsetStore::load_position(...) and seed them on the broker before
        // opening the stream. Without this, the broker returns `409
        // PositionsNotSet` on stream-open (defensive backstop).
        self.resolve_and_seek(
            &ctx,
            &group_id,
            assignment.subscription_id,
            &assignment.assigned,
        )
        .await?;

        let cursors: PartitionCursors = Arc::new(RwLock::new(HashMap::new()));
        let buffers: PartitionBuffers = Arc::new(RwLock::new(HashMap::new()));
        let slow_states: PartitionSlowStates = Arc::new(RwLock::new(HashMap::new()));

        // Auto-commit timer task.
        let _commit_task = match self.commit_mode {
            ConsumerCommitMode::Auto { interval } => {
                let seek_cursors = cursors.clone();
                let seek_broker = self.broker.clone();
                let seek_om = self.offset_manager.clone();
                let seek_ctx = ctx.clone();
                let seek_group = group_id;
                let seek_sub = assignment.subscription_id;
                let seek_cancel = cancel.clone();
                let seek_listeners = self.listeners.clone();
                let seek_listener_timeout = self.listener_timeout;
                Some(tokio::spawn(async move {
                    let mut interval = tokio::time::interval(interval);
                    loop {
                        tokio::select! {
                            _ = seek_cancel.cancelled() => break,
                            _ = interval.tick() => {
                                let pending: Vec<_> = {
                                    let guard = seek_cursors.read().await;
                                    guard
                                        .iter()
                                        .filter_map(|(key, cursor)| {
                                            let latest_offset = cursor.latest_offset();
                                            (latest_offset > cursor.committed)
                                                .then(|| (key.clone(), latest_offset))
                                        })
                                        .collect()
                                };

                                for (key, latest_offset) in pending {
                                    if seek_om.commit(
                                        &seek_group,
                                        &key.topic_id,
                                        key.partition,
                                        latest_offset,
                                    ).await.is_ok() {
                                        {
                                            let mut guard = seek_cursors.write().await;
                                            if let Some(cursor) = guard.get_mut(&key)
                                                && cursor.latest_offset() >= latest_offset
                                            {
                                                cursor.committed = latest_offset;
                                            }
                                        }
                                        emit_runtime_event_to(
                                            &seek_listeners,
                                            seek_listener_timeout,
                                            ConsumerRuntimeEvent::OffsetCommitted {
                                                topic_id: key.topic_id,
                                                topic: key.topic.clone(),
                                                partition: key.partition,
                                                offset: latest_offset,
                                            },
                                        ).await;
                                        emit_runtime_event_to(
                                            &seek_listeners,
                                            seek_listener_timeout,
                                            ConsumerRuntimeEvent::ProgressAdvanced {
                                                subscription_id: seek_sub,
                                                progress: vec![partition_progress(
                                                    &key,
                                                    latest_offset,
                                                )],
                                            },
                                        ).await;
                                    }
                                    let _ = seek_broker.seek(
                                        &seek_ctx,
                                        seek_sub,
                                        &[SeekPosition {
                                            topic: key.topic.clone(),
                                            partition: key.partition,
                                            value: Position::Exact(latest_offset),
                                        }],
                                    ).await;
                                }
                            }
                        }
                    }
                }))
            }
            ConsumerCommitMode::Manual => None,
        };

        let mut consecutive_failures = 0u32;
        'outer: loop {
            if cancel.is_cancelled() {
                break;
            }

            let sub_id = assignment.subscription_id;
            let mut stream = match self.broker.stream(&ctx, sub_id).await {
                Ok(s) => s,
                Err(EventBrokerError::SubscriptionRecoveryExhausted { .. }) => {
                    return Err(EventBrokerError::SubscriptionRecoveryExhausted {
                        attempts: consecutive_failures,
                        detail: "stream open: recovery exhausted".into(),
                        instance: String::new(),
                    });
                }
                Err(EventBrokerError::PositionsNotSet { unseeded, .. }) => {
                    // Defensive recovery: re-resolve via position() and SEEK the
                    // unseeded partitions, then retry. Shares the
                    // SubscriptionRecoveryExhausted budget.
                    consecutive_failures += 1;
                    if consecutive_failures > self.max_rejoin_attempts {
                        return Err(EventBrokerError::SubscriptionRecoveryExhausted {
                            attempts: consecutive_failures,
                            detail: "stream open: PositionsNotSet recovery exhausted".into(),
                            instance: String::new(),
                        });
                    }
                    let slots = self.slots_for_unseeded(&unseeded);
                    self.resolve_and_seek(&ctx, &group_id, sub_id, &slots)
                        .await?;
                    continue;
                }
                Err(e) => {
                    warn!(
                        slot_idx = self.slot_idx,
                        error = %e,
                        "stream open failed; re-joining"
                    );
                    assignment = self
                        .rejoin(&ctx, &group_id, sub_id, &mut consecutive_failures)
                        .await?;
                    self.resolve_and_seek(
                        &ctx,
                        &group_id,
                        assignment.subscription_id,
                        &assignment.assigned,
                    )
                    .await?;
                    continue;
                }
            };

            let mut consecutive_heartbeats: usize = 0;
            consecutive_failures = 0;
            let mut drain_before_rejoin = false;

            while let Some(frame_result) = stream.next().await {
                if cancel.is_cancelled() {
                    break 'outer;
                }
                match frame_result {
                    Ok(WireFrame::Event(event)) => {
                        consecutive_heartbeats = 0;
                        if self
                            .dispatch_event(
                                &cancel,
                                event,
                                DispatchState {
                                    cursors: &cursors,
                                    buffers: &buffers,
                                    slow_states: &slow_states,
                                },
                                ActiveSubscription {
                                    ctx: &ctx,
                                    group_id: &group_id,
                                    sub_id,
                                    affected: &assignment.assigned,
                                },
                            )
                            .await
                        {
                            drain_before_rejoin = true;
                            break;
                        }
                    }
                    Ok(WireFrame::Heartbeat { .. }) => {
                        consecutive_heartbeats += 1;
                        if consecutive_heartbeats >= self.heartbeat_drop_threshold {
                            trace!(
                                slot_idx = self.slot_idx,
                                threshold = self.heartbeat_drop_threshold,
                                "drop-on-Nth-heartbeat triggered; voluntary disconnect + re-JOIN"
                            );
                            // Drop the stream by breaking out of the inner loop.
                            // Outer loop will rejoin.
                            break;
                        }
                    }
                    Ok(WireFrame::Control {
                        code,
                        positions,
                        reason,
                    }) => {
                        // Cursor carrier: feed positions into the offset store so a
                        // reconnect re-SEEKs from last_examined, skipping
                        // server-side-filtered events (R57).
                        self.commit_positions(&ctx, &group_id, sub_id, &positions)
                            .await;
                        if code == ControlCode::Terminal {
                            // Gain / lose-all: the subscription is terminating. Final
                            // positions are committed above; re-JOIN (outer loop).
                            trace!(
                                slot_idx = self.slot_idx,
                                ?reason,
                                "terminal control frame; re-JOIN"
                            );
                            break;
                        }
                        // Progress: sparse mid-stream update; keep streaming.
                    }
                    Ok(WireFrame::Topology {
                        topology_version: _,
                        assigned,
                    }) => {
                        // Non-terminal: a partition loss or a `topology_version`-only
                        // change. Commit the snapshot's positions and update the
                        // assignment (lost partitions drop off); keep streaming.
                        // Gains never arrive as a topology frame - they terminate via
                        // a Terminal control frame and a re-JOIN.
                        self.commit_positions(&ctx, &group_id, sub_id, &assigned)
                            .await;
                    }
                    Err(EventBrokerError::Transport(_)) => {
                        break; // outer loop rejoins
                    }
                    Err(e) => {
                        warn!(
                            slot_idx = self.slot_idx,
                            error = %e,
                            "stream frame error; ending stream"
                        );
                        break;
                    }
                }
            }

            // Stream ended (subscription gone, connection closed, or drop-on-Nth-heartbeat).
            // Re-JOIN unless we're cancelled.
            if cancel.is_cancelled() {
                break;
            }
            if drain_before_rejoin {
                self.drain_subscription_buffers(
                    &cancel,
                    DispatchState {
                        cursors: &cursors,
                        buffers: &buffers,
                        slow_states: &slow_states,
                    },
                    ActiveSubscription {
                        ctx: &ctx,
                        group_id: &group_id,
                        sub_id,
                        affected: &assignment.assigned,
                    },
                )
                .await;
                let _ = self.broker.leave(&ctx, sub_id).await;
            }
            self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionTerminated {
                subscription_id: sub_id,
                reason: ConnectionDropReason::StreamTerminated,
            })
            .await;
            assignment = self
                .rejoin(&ctx, &group_id, sub_id, &mut consecutive_failures)
                .await?;
            // Fresh subscription_id → no SEEK history on the broker → must
            // re-seed positions before opening the next stream.
            self.resolve_and_seek(
                &ctx,
                &group_id,
                assignment.subscription_id,
                &assignment.assigned,
            )
            .await?;
        }

        // Graceful shutdown: leave subscription.
        let _ = self.broker.leave(&ctx, assignment.subscription_id).await;
        Ok(())
    }

    async fn dispatch_event(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        wire: WireEvent,
        state: DispatchState<'_>,
        active: ActiveSubscription<'_>,
    ) -> bool {
        // A topic is what an event's type declares, so it is resolved rather than
        // read off the frame. A type this consumer has not seen costs one call
        // here and none afterwards.
        let topic = match self
            .type_cache
            .topic_of(&self.broker, active.ctx, &wire.type_id)
            .await
        {
            Ok(topic) => topic.as_ref().to_owned(),
            Err(err) => {
                warn!(
                    error = %err,
                    type_id = %wire.type_id,
                    "cannot attribute a delivered event to a topic; skipping it"
                );
                return false;
            }
        };
        let topic_id = TopicId::from_gts(&topic);
        let key = TopicPartitionKey::new(topic.clone(), topic_id, wire.partition);

        let raw = RawEvent {
            id: wire.id,
            type_id: wire.type_id.clone(),
            topic,
            tenant_id: wire.tenant_id,
            subject: wire.subject,
            subject_type: wire.subject_type,
            partition: wire.partition,
            sequence: wire.sequence,
            offset: wire.sequence,
            occurred_at: wire.occurred_at,
            sequence_time: wire.sequence_time,
            trace_parent: wire.trace_parent,
            data: wire.data,
        };
        let enqueued = match enqueue_partition_batch(
            state.buffers,
            key.clone(),
            raw,
            self.partition_buffer_capacity,
        )
        .await
        {
            Ok(events) => events,
            Err(e) => {
                warn!(error = %e, "partition buffer rejected event");
                return false;
            }
        };
        if self
            .observe_enqueue_slow_signal(
                state,
                key.clone(),
                enqueued.buffered_count,
                wire.sequence,
                active,
            )
            .await
        {
            return true;
        }
        let batch_events = drain_partition_batch(state.buffers, &key, self.batch_max_events).await;
        self.process_batch(cancel, state, key, batch_events, active)
            .await
    }

    async fn drain_subscription_buffers(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        state: DispatchState<'_>,
        active: ActiveSubscription<'_>,
    ) {
        trace!(
            slot_idx = self.slot_idx,
            low_watermark = self.buffer_low_watermark,
            "draining buffered events before re-JOIN"
        );
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let Some((key, batch_events)) =
                next_buffered_partition_batch(state.buffers, self.batch_max_events).await
            else {
                break;
            };
            let _ = self
                .process_batch(cancel, state, key, batch_events, active)
                .await;
        }
    }

    async fn process_batch(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        state: DispatchState<'_>,
        key: TopicPartitionKey,
        batch_events: Vec<RawEvent>,
        active: ActiveSubscription<'_>,
    ) -> bool {
        if batch_events.is_empty() {
            return false;
        }
        let batch_partition = batch_events[0].partition;
        let batch_offset = batch_events
            .last()
            .map(|event| event.offset)
            .unwrap_or_else(|| batch_events[0].offset);
        let topic_id = key.topic_id;

        let mut attempts: u16 = 1;
        let mut backoff = self.retry_base;

        loop {
            if cancel.is_cancelled() {
                return false;
            }
            self.emit_runtime_event(ConsumerRuntimeEvent::HandlerBatchStarted {
                topic_id,
                topic: key.topic.clone(),
                partition: batch_partition,
                len: batch_events.len(),
            })
            .await;
            let batch = EventBatch::new(&batch_events);
            let started_at = Instant::now();
            match self.handler.handle_batch(&batch, attempts).await {
                Ok(
                    outcome @ (BatchHandlerOutcome::Success
                    | BatchHandlerOutcome::AdvanceThrough { .. }),
                ) => {
                    let processed_count =
                        match processed_count_from_outcome(&outcome, &batch_events) {
                            Ok(Some(count)) => count,
                            Ok(None) => unreachable!("retry outcomes are handled separately"),
                            Err(err) => {
                                self.emit_runtime_event(ConsumerRuntimeEvent::HandlerFailed {
                                    topic_id,
                                    topic: key.topic.clone(),
                                    partition: batch_partition,
                                    error: err.to_string(),
                                })
                                .await;
                                return true;
                            }
                        };
                    self.emit_runtime_event(ConsumerRuntimeEvent::HandlerBatchCompleted {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        outcome: outcome.clone(),
                    })
                    .await;
                    let drop_stream = self
                        .observe_handler_slow_signal(
                            state,
                            key.clone(),
                            started_at.elapsed(),
                            batch_offset,
                            active,
                        )
                        .await;
                    // Advance in-memory cursor.
                    let (frontier, already_committed) = {
                        let mut guard = state.cursors.write().await;
                        let c = guard.entry(key.clone()).or_default();
                        (
                            c.advance_through_delivered_prefix(&batch_events[..processed_count]),
                            false,
                        )
                    };
                    self.emit_runtime_event(ConsumerRuntimeEvent::ProgressAdvanced {
                        subscription_id: active.sub_id,
                        progress: vec![partition_progress(&key, frontier)],
                    })
                    .await;
                    if already_committed {
                        self.emit_runtime_event(ConsumerRuntimeEvent::OffsetCommitted {
                            topic_id,
                            topic: key.topic.clone(),
                            partition: batch_partition,
                            offset: frontier,
                        })
                        .await;
                    }
                    return drop_stream;
                }
                Ok(BatchHandlerOutcome::Retry { reason }) => {
                    self.emit_runtime_event(ConsumerRuntimeEvent::HandlerBatchCompleted {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        outcome: BatchHandlerOutcome::Retry {
                            reason: reason.clone(),
                        },
                    })
                    .await;
                    if self
                        .observe_handler_slow_signal(
                            state,
                            key.clone(),
                            started_at.elapsed(),
                            batch_offset,
                            active,
                        )
                        .await
                    {
                        return true;
                    }
                    if cancel.is_cancelled() {
                        return false;
                    }
                    trace!(reason, attempts, "handler returned Retry; backing off");
                    self.emit_runtime_event(ConsumerRuntimeEvent::RetryScheduled {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        attempt: attempts,
                        delay: backoff,
                    })
                    .await;
                    tokio::select! {
                        _ = cancel.cancelled() => return false,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(self.retry_max);
                    attempts = attempts.saturating_add(1);
                }
                Err(e) => {
                    self.emit_runtime_event(ConsumerRuntimeEvent::HandlerFailed {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        error: e.to_string(),
                    })
                    .await;
                    if self
                        .observe_handler_slow_signal(
                            state,
                            key.clone(),
                            started_at.elapsed(),
                            batch_offset,
                            active,
                        )
                        .await
                    {
                        return true;
                    }
                    if cancel.is_cancelled() {
                        return false;
                    }
                    warn!(error = %e, "handler returned Err; treating as Retry");
                    self.emit_runtime_event(ConsumerRuntimeEvent::RetryScheduled {
                        topic_id,
                        topic: key.topic.clone(),
                        partition: batch_partition,
                        attempt: attempts,
                        delay: backoff,
                    })
                    .await;
                    tokio::select! {
                        _ = cancel.cancelled() => return false,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(self.retry_max);
                    attempts = attempts.saturating_add(1);
                }
            }
        }
    }

    async fn observe_enqueue_slow_signal(
        &self,
        state: DispatchState<'_>,
        key: TopicPartitionKey,
        buffered_count: usize,
        latest_observed_offset: Sequence,
        active: ActiveSubscription<'_>,
    ) -> bool {
        let mut guard = state.slow_states.write().await;
        let signal = guard.entry(key.clone()).or_default().observe_enqueue(
            buffered_count,
            latest_observed_offset,
            self.buffer_high_watermark,
        );
        drop(guard);
        if let Some(signal) = signal {
            warn!(
                slot_idx = self.slot_idx,
                topic = %key.topic,
                partition = key.partition,
                reason = ?signal.reason,
                buffered_count = signal.buffered_count,
                latest_observed_offset = ?signal.latest_observed_offset,
                "slow consumer predicate detected"
            );
            self.emit_slow_consumer_events(
                *active.group_id,
                active.sub_id,
                key,
                signal,
                active.affected,
            )
            .await;
            return true;
        }
        false
    }

    async fn observe_handler_slow_signal(
        &self,
        state: DispatchState<'_>,
        key: TopicPartitionKey,
        elapsed: Duration,
        last_delivered_offset: Sequence,
        active: ActiveSubscription<'_>,
    ) -> bool {
        let mut guard = state.slow_states.write().await;
        let signal = guard
            .entry(key.clone())
            .or_default()
            .observe_handler_completion(
                elapsed,
                self.handler_latency,
                self.handler_strikes,
                last_delivered_offset,
            );
        drop(guard);
        if let Some(signal) = signal {
            warn!(
                slot_idx = self.slot_idx,
                topic = %key.topic,
                partition = key.partition,
                reason = ?signal.reason,
                consecutive_slow_handlers = signal.consecutive_slow_handlers,
                last_delivered_offset = ?signal.last_delivered_offset,
                "slow consumer predicate detected"
            );
            self.emit_slow_consumer_events(
                *active.group_id,
                active.sub_id,
                key,
                signal,
                active.affected,
            )
            .await;
            return true;
        }
        false
    }

    async fn emit_slow_consumer_events(
        &self,
        group_id: ConsumerGroupId,
        sub_id: SubscriptionId,
        key: TopicPartitionKey,
        signal: SlowConsumerSignal,
        affected: &[AssignedPartition],
    ) {
        let trigger = slow_consumer_trigger(signal.reason);
        let state = PartitionBufferStateSnapshot {
            group_id,
            subscription_id: sub_id,
            topic_id: key.topic_id,
            topic: key.topic.clone(),
            partition: key.partition,
            state: PartitionBufferState::SlowDetected,
            trigger: Some(trigger),
            buffered_count: signal.buffered_count,
            capacity: self.partition_buffer_capacity,
            latest_observed_offset: signal.latest_observed_offset,
            last_delivered_offset: signal.last_delivered_offset,
            consecutive_slow_handlers: signal.consecutive_slow_handlers,
        };
        self.emit_runtime_event(ConsumerRuntimeEvent::PartitionBufferStateChanged { state })
            .await;
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionConnectionDropped {
            subscription_id: sub_id,
            reason: ConnectionDropReason::SlowConsumer {
                topic_id: key.topic_id,
                topic: key.topic,
                partition: key.partition,
                trigger,
            },
            affected: affected.to_vec(),
        })
        .await;
    }

    async fn emit_runtime_event(&self, event: ConsumerRuntimeEvent) {
        emit_runtime_event_to(&self.listeners, self.listener_timeout, event).await;
    }

    /// Resolve a starting position for each slot via `CommitOffset::position`
    /// and SEEK the broker. Used after JOIN (initial seed), after re-JOIN, on
    /// Topology-frame for newly-assigned partitions, and on `409
    /// PositionsNotSet` recovery.
    async fn resolve_and_seek(
        &self,
        ctx: &SecurityContext,
        group_id: &ConsumerGroupId,
        subscription_id: SubscriptionId,
        slots: &[AssignedPartition],
    ) -> Result<(), EventBrokerError> {
        if slots.is_empty() {
            return Ok(());
        }
        let mut positions: Vec<SeekPosition> = Vec::with_capacity(slots.len());
        for slot in slots {
            let topic = slot.topic.clone();
            let topic_id = TopicId::from_gts(&topic);
            let value = self
                .offset_manager
                .load_position(group_id, &topic_id, slot.partition)
                .await?;
            self.emit_runtime_event(ConsumerRuntimeEvent::OffsetLoaded {
                topic_id,
                topic: topic.clone(),
                partition: slot.partition,
                position: value.clone(),
            })
            .await;
            positions.push(SeekPosition {
                topic,
                partition: slot.partition,
                value,
            });
        }
        self.broker
            .seek(ctx, subscription_id, &positions)
            .await
            .map(|_| ())
    }

    /// Feed control / topology-frame positions into the consumer's own offset
    /// store. Persists `last_examined` (the scan frontier) so a later reconnect
    /// re-SEEKs past server-side-filtered events (R57). Best-effort: a failure to
    /// persist is logged, not fatal.
    async fn commit_positions(
        &self,
        _ctx: &SecurityContext,
        group_id: &ConsumerGroupId,
        subscription_id: SubscriptionId,
        positions: &[PartitionPosition],
    ) {
        for p in positions {
            let topic = p.topic.as_ref().to_owned();
            let topic_id = TopicId::from_gts(&topic);
            if let Err(e) = self
                .offset_manager
                .commit(group_id, &topic_id, p.partition, p.last_examined)
                .await
            {
                warn!(error = %e, "failed to commit control-frame position to offset store");
            } else {
                self.emit_runtime_event(ConsumerRuntimeEvent::OffsetCommitted {
                    topic_id,
                    topic: topic.clone(),
                    partition: p.partition,
                    offset: p.last_examined,
                })
                .await;
                self.emit_runtime_event(ConsumerRuntimeEvent::ProgressAdvanced {
                    subscription_id,
                    progress: vec![PartitionProgress {
                        topic_id,
                        topic,
                        partition: p.partition,
                        offset: p.last_examined,
                    }],
                })
                .await;
            }
        }
    }

    /// Translate `(topic, partition)` pairs reported by `409 PositionsNotSet`
    /// into the public assignment shape used by `EventBrokerApi::join`.
    fn slots_for_unseeded(&self, unseeded: &[(String, u32)]) -> Vec<AssignedPartition> {
        unseeded
            .iter()
            .map(|(topic, partition)| AssignedPartition {
                topic: topic.clone(),
                partition: *partition,
            })
            .collect()
    }

    async fn ensure_group(
        &self,
        ctx: &SecurityContext,
    ) -> Result<ConsumerGroupId, EventBrokerError> {
        match &self.group_ref {
            ConsumerGroupRef::Id(id) => Ok(*id),
            ConsumerGroupRef::Gts(gts) => Err(EventBrokerError::InvalidConsumerOptions {
                detail: format!(
                    "consumer group GTS reference '{gts}' must be resolved before startup"
                ),
                instance: String::new(),
            }),
            ConsumerGroupRef::AutoAnonymous { alias } => {
                let group = self
                    .broker
                    .create_consumer_group(
                        ctx,
                        crate::models::CreateConsumerGroupRequest {
                            client_agent: alias.clone(),
                            description: None,
                        },
                    )
                    .await?;
                Ok(group.id)
            }
        }
    }

    async fn join_subscription(
        &self,
        ctx: &SecurityContext,
        group_id: &ConsumerGroupId,
    ) -> Result<SubscriptionAssignment, EventBrokerError> {
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionJoining {
            group_id: *group_id,
        })
        .await;
        let interests = build_join_interests(
            &self.subscription_interests,
            &self.topics,
            self.tenant_id,
            self.tenant_depth,
            self.barrier_mode,
            &self.event_type_patterns,
            self.filter.clone(),
        )?;

        let assignment = self
            .broker
            .join(
                ctx,
                JoinRequest {
                    group: *group_id,
                    client_agent: self.client_agent.clone(),
                    interests,
                    session_timeout: self.session_timeout,
                },
            )
            .await?;
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionStarted {
            group_id: *group_id,
            subscription_id: assignment.subscription_id,
            assigned: assignment.assigned.clone(),
        })
        .await;
        self.emit_runtime_event(ConsumerRuntimeEvent::AssignmentChanged {
            subscription_id: assignment.subscription_id,
            assigned: assignment.assigned.clone(),
        })
        .await;
        Ok(assignment)
    }

    async fn rejoin(
        &self,
        ctx: &SecurityContext,
        group_id: &ConsumerGroupId,
        previous_subscription_id: SubscriptionId,
        consecutive_failures: &mut u32,
    ) -> Result<SubscriptionAssignment, EventBrokerError> {
        *consecutive_failures += 1;
        if *consecutive_failures > self.max_rejoin_attempts {
            return Err(EventBrokerError::SubscriptionRecoveryExhausted {
                attempts: *consecutive_failures,
                detail: "max re-JOIN attempts exceeded".into(),
                instance: String::new(),
            });
        }
        self.emit_runtime_event(ConsumerRuntimeEvent::SubscriptionRejoining {
            group_id: *group_id,
            previous_subscription_id,
        })
        .await;
        tokio::time::sleep(Duration::from_millis(250) * (*consecutive_failures)).await;
        let assignment = self.join_subscription(ctx, group_id).await?;
        {
            let mut guard = self.subscription_id.lock().await;
            *guard = Some(assignment.subscription_id);
        }
        Ok(assignment)
    }
}
