use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gts::GtsInstanceId;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::Position;
use crate::api::{
    AssignedPartition, EventBrokerApi, IngestOutcome, JoinRequest, PartitionCursor,
    ProducerCursors, ProducerMode, SeekResult, SubscriptionAssignment, TopicCursors,
};
use crate::api::{FrameStream, SeekPosition};
use crate::error::EventBrokerError;
use crate::error::{OutOfRange, PositionViolation, StorageBackendError};
use crate::ids::{ConsumerGroupId, ProducerId, SubscriptionId};
use crate::models::Event;
use crate::models::{
    ConsumerGroup, ConsumerGroupKind, ConsumerGroupQuery, CreateConsumerGroupRequest, EventType,
    Page, PartitionRange, ResetScope, Subscription, Topic, TopicSegment,
};
use crate::sequence::Sequence;

use super::core::{Core, GroupReg, GroupState, MockBroker, SubState};
use super::ingest::{ingest_batch, ingest_one};
use super::rebalance::run_rebalance;
use super::stream::open_stream;

// --- Helper --------------------------------------------------------------------

fn principal(ctx: &SecurityContext) -> Uuid {
    ctx.subject_id()
}

fn tenant(ctx: &SecurityContext) -> Uuid {
    ctx.subject_tenant_id()
}

fn instant_to_utc(instant: Instant) -> DateTime<Utc> {
    let now_instant = Instant::now();
    let now_utc = Utc::now();
    if instant >= now_instant {
        now_utc
            + chrono::Duration::from_std(instant.duration_since(now_instant)).unwrap_or_default()
    } else {
        now_utc
            - chrono::Duration::from_std(now_instant.duration_since(instant)).unwrap_or_default()
    }
}

/// Batch publish limits (DESIGN: ingest batch caps).
const MAX_BATCH_EVENTS: usize = 100;
const MAX_BATCH_BYTES: usize = 1024 * 1024; // 1 MiB total payload

/// Reject a batch exceeding the event-count or total-payload-byte cap.
fn check_batch_size(events: &[Event]) -> Result<(), EventBrokerError> {
    let count = events.len();
    let bytes: usize = events
        .iter()
        .map(|e| {
            e.data
                .as_ref()
                .map(|d| serde_json::to_vec(d).map(|v| v.len()).unwrap_or(0))
                .unwrap_or(0)
        })
        .sum();
    if count > MAX_BATCH_EVENTS || bytes > MAX_BATCH_BYTES {
        return Err(EventBrokerError::BatchTooLarge {
            count,
            bytes,
            max_count: MAX_BATCH_EVENTS,
            max_bytes: MAX_BATCH_BYTES,
            detail: format!(
                "batch of {count} events / {bytes} bytes exceeds limits ({MAX_BATCH_EVENTS} events, {MAX_BATCH_BYTES} bytes)"
            ),
            instance: String::new(),
        });
    }
    Ok(())
}

/// Resolve `AtTimestamp` against the in-memory log by a linear scan over stored
/// events ordered by offset (offset == storage order == `occurred_at` order for
/// the mock's append-only log). Returns the offset of the first event whose
/// `occurred_at >= ts`. A timestamp at/before the retention floor resolves to
/// the floor offset; a timestamp beyond the high-water mark resolves to the HWM.
/// The cursor to read after so delivery begins at `position`.
///
/// Storage-side: this adapter owns the integer space, and the position it
/// returns is not claimed to exist - it is frequently the one retention removed.
fn cursor_delivering(position: Sequence) -> Sequence {
    Sequence::assigned(position.as_i64().saturating_sub(1))
}

/// The mock's high-water mark as a cursor: the last offset it assigned.
///
/// `next_offset` is the mock's own integer counter, so the step happens before
/// the value becomes a `Sequence` - this is a storage adapter, and the counter
/// is not itself a position.
///
/// **Unclamped on purpose.** A partition that has never been written yields
/// `-1`, which the mock uses as its "nothing assigned yet" sentinel and which
/// `last_examined` reports verbatim. A caller that needs the cursor floor
/// instead clamps at its own site, because the two readings differ: a scan
/// frontier can legitimately sit below the first position, a cursor cannot.
pub(super) fn hwm_cursor(core: &Core, topic: &str, partition: u32) -> Sequence {
    Sequence::assigned(
        core.topics
            .get(topic)
            .and_then(|t| t.next_offset.get(&partition).copied())
            .unwrap_or(0)
            .saturating_sub(1)
            .max(0),
    )
}

/// Resolves a requested position against what this mock holds.
///
/// The one place the mock answers "which position should a cursor take?", so
/// the four forms cannot drift apart.
pub(super) fn resolve_position(
    core: &Core,
    topic: &str,
    partition: u32,
    position: Position,
) -> Result<Sequence, StorageBackendError> {
    let ceiling = hwm_cursor(core, topic, partition);
    let oldest = core
        .topics
        .get(topic)
        .and_then(|t| t.log.get(&partition))
        .and_then(|log| log.first())
        .and_then(|stored| stored.event.sequence);
    // Nothing retained leaves the ceiling as the only position a cursor can
    // hold: no event remains for delivery to resume from.
    let floor = oldest.map_or(ceiling, cursor_delivering);

    match position {
        Position::Earliest => Ok(floor),
        Position::Latest => Ok(ceiling),
        Position::At(at) => Ok(core
            .topics
            .get(topic)
            .and_then(|t| t.log.get(&partition))
            .and_then(|log| log.iter().find(|stored| stored.event.occurred_at >= at))
            .and_then(|stored| stored.event.sequence)
            .map_or(ceiling, cursor_delivering)),
        Position::Exact(requested) => {
            if (floor..=ceiling).contains(&requested) {
                Ok(requested)
            } else {
                Err(StorageBackendError::OffsetOutOfRange {
                    floor,
                    ceiling,
                    breached: if requested < floor {
                        OutOfRange::BelowFloor
                    } else {
                        OutOfRange::AboveCeiling
                    },
                    detail: format!("valid positions are [{floor}, {ceiling}]"),
                    instance: String::new(),
                })
            }
        }
    }
}

impl MockBroker {
    /// Honour the `reject_persist` fault (M3 chain-gap surface).
    async fn check_reject_persist(&self) -> Result<(), EventBrokerError> {
        let faults = self.faults.lock().await;
        if let Some(reason) = &faults.reject_persist {
            return Err(EventBrokerError::Internal(reason.clone()));
        }
        Ok(())
    }

    /// Charge `n` units against the producer publish rate-limit allowance. When
    /// the allowance is insufficient, refuse with `RateLimited` and do not
    /// consume any allowance.
    async fn consume_publish_allowance(&self, n: u32) -> Result<(), EventBrokerError> {
        let mut faults = self.faults.lock().await;
        if let Some(remaining) = faults.publish_rate_limit {
            if remaining < n {
                return Err(EventBrokerError::RateLimited {
                    retry_after_secs: 30,
                    detail: format!(
                        "publish rate limit exhausted ({remaining} of {n} requested unit(s) available)"
                    ),
                    instance: String::new(),
                });
            }
            faults.publish_rate_limit = Some(remaining - n);
        }
        Ok(())
    }
}

#[async_trait]
impl EventBrokerApi for MockBroker {
    // -- Producer --------------------------------------------------------------
    async fn register_producer(
        &self,
        _ctx: &SecurityContext,
        mode: ProducerMode,
        client_agent: &str,
    ) -> Result<ProducerId, EventBrokerError> {
        crate::validate::client_agent(client_agent)
            .map_err(|err| EventBrokerError::invalid_text_field(&err, "/v1/producers"))?;
        // Stateless producers are not registered (DESIGN §3.2); only chained and
        // monotonic modes mint a producer id.
        if mode == ProducerMode::Stateless {
            return Err(EventBrokerError::InvalidProducerOptions {
                detail: "a stateless producer is not registered".to_owned(),
                instance: String::new(),
            });
        }
        let id = ProducerId(Uuid::new_v4());
        let mut core = self.core.lock().await;
        core.producers.insert(
            id,
            super::core::ProducerReg {
                mode,
                client_agent: client_agent.to_owned(),
            },
        );
        Ok(id)
    }

    async fn publish(
        &self,
        _ctx: &SecurityContext,
        event: &Event,
    ) -> Result<IngestOutcome, EventBrokerError> {
        self.check_reject_persist().await?;
        self.consume_publish_allowance(1).await?;
        let mut core = self.core.lock().await;
        let (outcome, _) = ingest_one(&mut core, event)?;
        if outcome == IngestOutcome::Accepted {
            self.notify.notify_waiters();
        }
        Ok(outcome)
    }

    async fn publish_sync(
        &self,
        _ctx: &SecurityContext,
        event: &Event,
    ) -> Result<IngestOutcome, EventBrokerError> {
        self.check_reject_persist().await?;
        self.consume_publish_allowance(1).await?;
        // The mock's `ingest_one` appends to the in-memory log synchronously, so
        // by the time it returns the event is durably "persisted". Report the
        // persist-confirmed outcome (Accepted → Persisted; Duplicate stays).
        let mut core = self.core.lock().await;
        let (outcome, _) = ingest_one(&mut core, event)?;
        let outcome = match outcome {
            IngestOutcome::Accepted => IngestOutcome::Persisted,
            other => other,
        };
        if outcome == IngestOutcome::Persisted {
            self.notify.notify_waiters();
        }
        Ok(outcome)
    }

    async fn publish_batch(
        &self,
        _ctx: &SecurityContext,
        events: &[Event],
    ) -> Result<IngestOutcome, EventBrokerError> {
        self.check_reject_persist().await?;
        check_batch_size(events)?;
        self.consume_publish_allowance(events.len() as u32).await?;
        let mut core = self.core.lock().await;
        let results = ingest_batch(&mut core, events)?;
        let any_accepted = results.iter().any(|(o, _)| *o == IngestOutcome::Accepted);
        if any_accepted {
            self.notify.notify_waiters();
        }
        // A batch is all-or-nothing, so it reports one outcome: `Duplicate` only
        // when every event was a duplicate, otherwise `Accepted`.
        Ok(
            if !results.is_empty() && results.iter().all(|(o, _)| *o == IngestOutcome::Duplicate) {
                IngestOutcome::Duplicate
            } else {
                IngestOutcome::Accepted
            },
        )
    }

    async fn get_producer_cursors(
        &self,
        _ctx: &SecurityContext,
        producer_id: ProducerId,
    ) -> Result<ProducerCursors, EventBrokerError> {
        let core = self.core.lock().await;
        let client_agent = core
            .producers
            .get(&producer_id)
            .map(|reg| reg.client_agent.clone())
            .unwrap_or_default();
        let mut by_topic: std::collections::BTreeMap<String, Vec<PartitionCursor>> =
            std::collections::BTreeMap::new();
        for ((pid, topic, partition), seq) in core.producer_state.iter() {
            if *pid == producer_id {
                by_topic
                    .entry(topic.clone())
                    .or_default()
                    .push(PartitionCursor {
                        partition: *partition,
                        last_sequence: *seq,
                    });
            }
        }
        let topics = by_topic
            .into_iter()
            .map(|(topic, mut partitions)| {
                partitions.sort_by_key(|p| p.partition);
                TopicCursors { topic, partitions }
            })
            .collect();
        Ok(ProducerCursors {
            producer_id,
            client_agent,
            topics,
        })
    }

    async fn reset_producer_chain(
        &self,
        _ctx: &SecurityContext,
        producer_id: ProducerId,
        scope: ResetScope<'_>,
    ) -> Result<(), EventBrokerError> {
        let mut core = self.core.lock().await;
        match scope {
            ResetScope::Partition { topic, partition } => {
                // Reset single (producer, topic, partition).
                core.producer_state
                    .remove(&(producer_id, topic.to_owned(), partition));
            }
            ResetScope::Topic(topic) => {
                // Reset all (producer, topic, *) - M7 branch 2.
                let keys: Vec<_> = core
                    .producer_state
                    .keys()
                    .filter(|(pid, tp, _)| *pid == producer_id && tp == topic)
                    .cloned()
                    .collect();
                for k in keys {
                    core.producer_state.remove(&k);
                }
            }
            ResetScope::AllTopics => {
                // Reset all (producer, *, *) - M7 branch 1 (reset-all).
                let keys: Vec<_> = core
                    .producer_state
                    .keys()
                    .filter(|(pid, _, _)| *pid == producer_id)
                    .cloned()
                    .collect();
                for k in keys {
                    core.producer_state.remove(&k);
                }
            }
        }
        Ok(())
    }

    // -- Consumer groups -------------------------------------------------------
    async fn create_consumer_group(
        &self,
        ctx: &SecurityContext,
        req: CreateConsumerGroupRequest,
    ) -> Result<ConsumerGroup, EventBrokerError> {
        // B3: `client_agent` follows the RFC 9110 User-Agent grammar.
        crate::validate::client_agent(&req.client_agent)
            .map_err(|err| EventBrokerError::invalid_text_field(&err, "/v1/consumer-groups"))?;
        if let Some(description) = req.description.as_deref() {
            crate::validate::description(description)
                .map_err(|err| EventBrokerError::invalid_text_field(&err, "/v1/consumer-groups"))?;
        }
        let group_id = ConsumerGroupId::new(Uuid::new_v4());
        let mut core = self.core.lock().await;
        core.groups_registry.insert(
            group_id,
            GroupReg {
                kind: ConsumerGroupKind::Anonymous,
                owner_tenant: tenant(ctx),
                owner_principal: principal(ctx),
            },
        );
        Ok(ConsumerGroup {
            id: group_id,
            tenant_id: tenant(ctx),
            owner_principal_id: principal(ctx),
            kind: ConsumerGroupKind::Anonymous,
            description: None,
            created_at: Utc::now(),
        })
    }

    async fn get_consumer_group(
        &self,
        _ctx: &SecurityContext,
        id: &ConsumerGroupId,
    ) -> Result<ConsumerGroup, EventBrokerError> {
        let core = self.core.lock().await;
        let reg = core.groups_registry.get(id).ok_or_else(|| {
            EventBrokerError::ConsumerGroupNotFound {
                group_id: *id,
                detail: format!("consumer group '{id}' not found"),
                instance: String::new(),
            }
        })?;
        Ok(ConsumerGroup {
            id: *id,
            tenant_id: reg.owner_tenant,
            owner_principal_id: reg.owner_principal.clone(),
            kind: reg.kind,
            description: None,
            created_at: Utc::now(),
        })
    }

    async fn list_consumer_groups(
        &self,
        _ctx: &SecurityContext,
        query: ConsumerGroupQuery,
    ) -> Result<Page<ConsumerGroup>, EventBrokerError> {
        // Mock: cursor, filter, and orderby are accepted but not implemented.
        // Returns the first page only.
        let page_limit = query.limit.unwrap_or(25) as usize;
        let core = self.core.lock().await;
        let items: Vec<ConsumerGroup> = core
            .groups_registry
            .iter()
            .take(page_limit)
            .map(|(id, reg)| ConsumerGroup {
                id: *id,
                tenant_id: reg.owner_tenant,
                owner_principal_id: reg.owner_principal,
                kind: reg.kind,
                description: None,
                created_at: Utc::now(),
            })
            .collect();
        let total = core.groups_registry.len();
        let next_cursor = if total > page_limit {
            Some("mock-next-cursor".to_owned())
        } else {
            None
        };
        Ok(Page {
            items,
            next_cursor,
            prev_cursor: None,
            limit: page_limit as u32,
        })
    }

    async fn delete_consumer_group(
        &self,
        _ctx: &SecurityContext,
        id: &ConsumerGroupId,
    ) -> Result<(), EventBrokerError> {
        let mut core = self.core.lock().await;
        if !core.groups_registry.contains_key(id) {
            return Err(EventBrokerError::ConsumerGroupNotFound {
                group_id: *id,
                detail: format!("consumer group '{id}' not found"),
                instance: String::new(),
            });
        }
        if core.groups.contains_key(id) {
            let has_members = core
                .groups
                .get(id)
                .map(|g| !g.members.is_empty())
                .unwrap_or(false);
            if has_members {
                return Err(EventBrokerError::ConsumerGroupHasActiveMembers {
                    detail: format!("consumer group '{id}' has active subscriptions"),
                    instance: String::new(),
                });
            }
        }
        core.groups_registry.remove(id);
        core.groups.remove(id);
        Ok(())
    }

    // -- Subscriptions ---------------------------------------------------------
    async fn join(
        &self,
        _ctx: &SecurityContext,
        req: JoinRequest,
    ) -> Result<SubscriptionAssignment, EventBrokerError> {
        // B3: `client_agent` follows the RFC 9110 User-Agent grammar.
        crate::validate::client_agent(&req.client_agent)
            .map_err(|err| EventBrokerError::invalid_text_field(&err, "/v1/subscriptions"))?;
        // B4: a subscription carries 1-64 interests.
        const MAX_INTERESTS: usize = 64;
        if req.interests.is_empty() || req.interests.len() > MAX_INTERESTS {
            return Err(EventBrokerError::InvalidEventField {
                field: "interests",
                detail: format!(
                    "interests must be 1..={MAX_INTERESTS} (got {})",
                    req.interests.len()
                ),
                instance: "/v1/subscriptions".to_owned(),
            });
        }
        let now = Instant::now();
        let sub_id = SubscriptionId(Uuid::new_v4());
        let timeout = req
            .session_timeout
            .unwrap_or(std::time::Duration::from_secs(30));

        let mut core = self.core.lock().await;

        // Validate group exists.
        if !core.groups_registry.contains_key(&req.group) {
            return Err(EventBrokerError::ConsumerGroupNotFound {
                group_id: req.group,
                detail: format!("consumer group '{}' not registered", req.group),
                instance: String::new(),
            });
        }

        // Build the set of topics from interests.
        let topics: std::collections::HashSet<String> =
            req.interests.iter().map(|i| i.topic.clone()).collect();

        // Capacity guard: a group can carry at most one active member per partition
        // for any given topic (v1 round-robin). If every topic this member is
        // interested in is already saturated (active members ≥ partitions), the
        // member could never be assigned a partition - refuse the JOIN.
        let active_members = core
            .groups
            .get(&req.group)
            .map(|g| g.members.len() as u32)
            .unwrap_or(0);
        let max_partitions = topics
            .iter()
            .filter_map(|t| core.topics.get(t).map(|state| state.partitions))
            .max();
        if let Some(partitions) = max_partitions
            && active_members >= partitions
        {
            return Err(EventBrokerError::GroupAtCapacity {
                active: active_members,
                partitions,
                detail: format!(
                    "consumer group '{}' already has {active_members} active member(s) for a topic with {partitions} partition(s); no partition available for a further member",
                    req.group
                ),
                instance: String::new(),
            });
        }

        // Build SubState.
        let sub = SubState {
            group: req.group,
            client_agent: req.client_agent,
            interests: req.interests,
            topics,
            assigned: Vec::new(),
            topology_version: 0,
            created_at: now,
            session_timeout: timeout,
            expires_at: now + timeout,
            seek: HashMap::new(),
            sent: HashMap::new(),
            scanned: HashMap::new(),
            terminated: false,
        };
        core.subscriptions.insert(sub_id, sub);

        // Ensure GroupState exists.
        core.groups.entry(req.group).or_insert_with(GroupState::new);
        let group = core.groups.get_mut(&req.group).unwrap();
        group.members.push(sub_id);

        // Run v1 rebalance.
        run_rebalance(&req.group, &mut core);
        self.notify.notify_waiters();

        // Build SubscriptionAssignment from the group's cursor state.
        let group = core.groups.get(&req.group).unwrap();
        let sub = core.subscriptions.get(&sub_id).unwrap();
        let topology_version = group.topology_version;
        let assigned: Vec<AssignedPartition> = sub
            .assigned
            .iter()
            .map(|(topic, partition)| AssignedPartition {
                topic: topic.clone(),
                partition: *partition,
            })
            .collect();

        Ok(SubscriptionAssignment {
            subscription_id: sub_id,
            topology_version,
            expires_at: instant_to_utc(sub.expires_at),
            assigned,
        })
    }

    async fn get_subscription(
        &self,
        _ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<Subscription, EventBrokerError> {
        let core = self.core.lock().await;
        let sub = core
            .subscriptions
            .get(&id)
            .ok_or_else(|| EventBrokerError::Internal(format!("subscription {id:?} not found")))?;
        let group = core.groups.get(&sub.group);
        let topology_version = group.map(|g| g.topology_version).unwrap_or(0);
        Ok(Subscription {
            id,
            consumer_group: sub.group,
            client_agent: sub.client_agent.clone(),
            interests: sub.interests.clone(),
            assigned: sub
                .assigned
                .iter()
                .map(|(topic, p)| crate::models::PartitionAssignment {
                    topic: GtsInstanceId::try_new(topic)
                        .expect("registration asserts the topic id"),
                    partition: *p,
                })
                .collect(),
            topology_version,
            expires_at: instant_to_utc(sub.expires_at),
        })
    }

    async fn list_subscriptions(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<Vec<Subscription>, EventBrokerError> {
        let core = self.core.lock().await;
        Ok(core
            .subscriptions
            .iter()
            .map(|(id, sub)| {
                let tv = core
                    .groups
                    .get(&sub.group)
                    .map(|g| g.topology_version)
                    .unwrap_or(0);
                Subscription {
                    id: *id,
                    consumer_group: sub.group,
                    client_agent: sub.client_agent.clone(),
                    interests: sub.interests.clone(),
                    assigned: sub
                        .assigned
                        .iter()
                        .map(|(topic, p)| crate::models::PartitionAssignment {
                            topic: GtsInstanceId::try_new(topic)
                                .expect("registration asserts the topic id"),
                            partition: *p,
                        })
                        .collect(),
                    topology_version: tv,
                    expires_at: instant_to_utc(sub.expires_at),
                }
            })
            .collect())
    }

    async fn leave(
        &self,
        _ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<(), EventBrokerError> {
        let mut core = self.core.lock().await;
        match core.subscriptions.remove(&id) {
            Some(sub) => {
                let group_id = sub.group;
                if let Some(group) = core.groups.get_mut(&group_id) {
                    group.members.retain(|m| *m != id);
                }
                run_rebalance(&group_id, &mut core);
                self.notify.notify_waiters();
                Ok(())
            }
            // B2: leaving an unknown/expired subscription is a 404, not a silent no-op.
            None => Err(EventBrokerError::SubscriptionNotFound {
                id,
                detail: "no such subscription (unknown or already removed)".to_owned(),
                instance: format!("/v1/subscriptions/{id:?}"),
            }),
        }
    }

    async fn stream(
        &self,
        _ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<FrameStream, EventBrokerError> {
        // A subscription terminated by a gain / lose-all rebalance is dead - any
        // reuse of its id returns 410 (the safety net for a consumer that missed
        // the terminal control frame).
        {
            let mut core = self.core.lock().await;
            let sub = core.subscriptions.get(&id).ok_or_else(|| {
                EventBrokerError::SubscriptionNotFound {
                    id,
                    detail: "no such subscription (unknown or expired)".to_owned(),
                    instance: format!("/v1/events:stream?subscription_id={id:?}"),
                }
            })?;
            if sub.terminated {
                return Err(EventBrokerError::Internal(
                    "410: Subscription terminated; re-JOIN to recover".to_owned(),
                ));
            }
            // A1: every assigned partition must have a committed cursor (set via SEEK,
            // or inherited group-scoped from a prior member) before the stream opens.
            let group_cursors = core.groups.get(&sub.group).map(|g| &g.cursor);
            let unseeded: Vec<(String, u32)> = sub
                .assigned
                .iter()
                .filter(|(t, p)| {
                    group_cursors
                        .map(|c| !c.contains_key(&(t.clone(), *p)))
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            if !unseeded.is_empty() {
                return Err(EventBrokerError::PositionsNotSet {
                    unseeded,
                    detail: "SEEK every assigned partition before opening the stream".to_owned(),
                    instance: format!("/v1/events:stream?subscription_id={id:?}"),
                });
            }
            if let Some(sub) = core.subscriptions.get_mut(&id) {
                sub.expires_at = Instant::now() + sub.session_timeout;
            }
        }
        // A2: one open stream per subscription. Mark it streaming; the stream's
        // Drop guard clears the marker when it ends/drops.
        {
            let mut streaming = self.streaming.lock().unwrap();
            if streaming.contains(&id) {
                return Err(EventBrokerError::StreamingInProgress {
                    detail: "a stream is already open for this subscription".to_owned(),
                    instance: format!("/v1/events:stream?subscription_id={id:?}"),
                });
            }
            streaming.insert(id);
        }
        Ok(open_stream(self.clone(), id))
    }

    async fn seek(
        &self,
        _ctx: &SecurityContext,
        id: SubscriptionId,
        positions: &[SeekPosition],
    ) -> Result<Vec<SeekResult>, EventBrokerError> {
        // A2: SEEK is a pre-stream operation - rejected while a stream is open.
        if self.streaming.lock().unwrap().contains(&id) {
            return Err(EventBrokerError::StreamingInProgress {
                detail: "SEEK is not allowed while a stream is open; it is a pre-stream operation"
                    .to_owned(),
                instance: format!("/v1/subscriptions/{id:?}:seek"),
            });
        }
        let mut core = self.core.lock().await;
        // A3: SEEK is only valid for partitions assigned to this subscription.
        let assigned: Vec<(String, u32)> = core
            .subscriptions
            .get(&id)
            .map(|s| s.assigned.clone())
            .unwrap_or_default();
        for pos in positions {
            if !assigned
                .iter()
                .any(|(t, p)| t == &pos.topic && *p == pos.partition)
            {
                return Err(EventBrokerError::PartitionNotAssigned {
                    topic: pos.topic.clone(),
                    partition: pos.partition,
                    detail: "seek targets a partition not in the subscription's assignment"
                        .to_owned(),
                    instance: format!("/v1/subscriptions/{id:?}:seek"),
                });
            }
        }
        // Resolve every entry before applying any, and report every offender:
        // the request applies in full or not at all, and a caller correcting
        // several positions does not need one round trip each.
        let mut results: Vec<SeekResult> = Vec::with_capacity(positions.len());
        let mut internal: HashMap<(String, u32), Sequence> = HashMap::new();
        let mut violations: Vec<PositionViolation> = Vec::new();
        for (index, pos) in positions.iter().enumerate() {
            // One resolver for all four forms, shared with the backend
            // surface, so the two cannot disagree.
            match resolve_position(&core, &pos.topic, pos.partition, pos.value.clone()) {
                Ok(offset) => {
                    results.push(SeekResult {
                        topic: pos.topic.clone(),
                        partition: pos.partition,
                        offset,
                    });
                    internal.insert((pos.topic.clone(), pos.partition), offset);
                }
                Err(StorageBackendError::OffsetOutOfRange {
                    floor,
                    ceiling,
                    breached,
                    ..
                }) => violations.push(
                    PositionViolation::builder(index)
                        .floor(floor)
                        .ceiling(ceiling)
                        .breached(breached)
                        .build(),
                ),
                Err(other) => {
                    return Err(EventBrokerError::Internal(other.to_string()));
                }
            }
        }
        if !violations.is_empty() {
            return Err(EventBrokerError::InvalidInitialPosition {
                violations,
                detail: "Request validation failed".to_owned(),
                instance: format!("/v1/subscriptions/{id:?}:seek"),
            });
        }
        // Advance group cursor using MAX rule (forward-only, equivalent to old ack behaviour).
        let group_id = core.subscriptions.get(&id).map(|s| s.group);
        if let Some(gid) = group_id
            && let Some(group) = core.groups.get_mut(&gid)
        {
            for ((topic, partition), offset) in &internal {
                let entry = group.cursor.entry((topic.clone(), *partition)).or_default();
                entry.offset = entry.offset.max(*offset);
            }
        }
        if let Some(sub) = core.subscriptions.get_mut(&id) {
            sub.expires_at = Instant::now() + sub.session_timeout;
            sub.seek.extend(internal);
        }
        self.notify.notify_waiters();
        Ok(results)
    }

    // -- Introspection ---------------------------------------------------------
    async fn list_topics(&self, _ctx: &SecurityContext) -> Result<Vec<Topic>, EventBrokerError> {
        let core = self.core.lock().await;
        Ok(core
            .topics
            .iter()
            .map(|(id, state)| state.topic(id))
            .collect())
    }

    async fn list_topic_segments(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        _range: PartitionRange,
    ) -> Result<TopicSegment, EventBrokerError> {
        let core = self.core.lock().await;
        let t = core
            .topics
            .get(topic)
            .ok_or_else(|| EventBrokerError::TopicNotFound {
                topic: topic.to_owned(),
                detail: String::new(),
                instance: String::new(),
            })?;
        // The wire returns one manifest per (topic, partition). An empty
        // partition has no span, so its sequences are NONE and its times absent.
        let events = t.log.get(&partition).filter(|events| !events.is_empty());
        let start = events
            .and_then(|e| e.first())
            .and_then(|e| e.event.sequence)
            .unwrap_or(Sequence::NONE);
        let end = events
            .and_then(|e| e.last())
            .and_then(|e| e.event.sequence)
            .unwrap_or(Sequence::NONE);
        Ok(TopicSegment {
            topic: topic.to_owned(),
            partition,
            start_sequence: start,
            end_sequence: end,
            start_time: events.and_then(|e| e.first()).and_then(|e| e.event.sequence_time),
            end_time: events.and_then(|e| e.last()).and_then(|e| e.event.sequence_time),
            segments: vec![],
        })
    }

    async fn list_event_types(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<Vec<EventType>, EventBrokerError> {
        let core = self.core.lock().await;
        Ok(core
            .topics
            .values()
            .flat_map(|t| t.event_types.iter())
            .map(|(type_id, reg)| reg.event_type(type_id))
            .collect())
    }

    async fn get_event_type(
        &self,
        _ctx: &SecurityContext,
        id: &str,
    ) -> Result<EventType, EventBrokerError> {
        let core = self.core.lock().await;
        let reg = core
            .topics
            .values()
            .find_map(|t| t.event_types.get(id))
            .ok_or_else(|| EventBrokerError::EventTypeUnknown {
                type_id: id.to_owned(),
                detail: format!("event type '{id}' not registered in mock"),
                instance: String::new(),
            })?;
        Ok(reg.event_type(id))
    }
}
