use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use gts::GtsInstanceId;
use tokio::time::sleep;

use crate::api::{ControlCode, FrameStream, PartitionPosition, WireEvent, WireFrame};
use crate::error::EventBrokerError;
use crate::ids::SubscriptionId;
use crate::sequence::Sequence;

use super::core::MockBroker;

type AssignedPartitionRef = (String, u32);

struct StreamGuard {
    set: Arc<Mutex<HashSet<SubscriptionId>>>,
    id: SubscriptionId,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.set.lock() {
            s.remove(&self.id);
        }
    }
}

struct GuardedStream {
    inner: FrameStream,
    _guard: StreamGuard,
}

impl futures_core::Stream for GuardedStream {
    type Item = Result<WireFrame, EventBrokerError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

struct StreamBaseline {
    topology_version: i64,
    assigned: Vec<AssignedPartitionRef>,
    positions: Vec<PartitionPosition>,
}

/// Match an event `type_id` against a subscription interest's GTS type patterns.
/// `"*"` matches everything; a trailing-`*` pattern is a prefix match; otherwise
/// an exact match. (Sufficient for the mock; full GTS wildcard semantics live in
/// the real filter engine.)
fn type_matches(patterns: &[String], type_id: &str) -> bool {
    patterns.iter().any(|p| {
        if p == "*" {
            true
        } else if let Some(prefix) = p.strip_suffix('*') {
            type_id.starts_with(prefix)
        } else {
            p == type_id
        }
    })
}

/// Open a live `FrameStream` for the given subscription.
///
/// Emits:
/// 1. Initial `Topology` frame.
/// 2. Events loop: deliver new events, emit `Heartbeat` on idle.
/// 3. `Topology` frame whenever the group's `topology_version` advances.
/// 4. Fault: `SubscriptionGone` (410) or `NotFound` (404) if injected.
pub(super) fn open_stream(broker: MockBroker, sub_id: SubscriptionId) -> FrameStream {
    let guard = StreamGuard {
        set: broker.streaming.clone(),
        id: sub_id,
    };
    let stream = async_stream::try_stream! {
        // -- 0. Emit the open-time topology baseline (guaranteed first frame) --
        // Build the snapshot under the lock, then DROP the lock before yielding -
        // holding `core` across a yield would deadlock any broker call (leave,
        // rebalance) made while the stream is parked here.
        // Compute the snapshot under the lock and return an owned Result, so the
        // guard is dropped at the end of this block - BEFORE the `?` can yield an
        // Err. Holding `core` across the error yield would deadlock any later
        // broker call once the consumer stops polling the errored stream.
        let baseline: Result<StreamBaseline, EventBrokerError> = {
            let core = broker.core.lock().await;
            match core.subscriptions.get(&sub_id) {
                None => Err(EventBrokerError::Internal(format!("subscription {sub_id:?} not found"))),
                Some(sub) => {
                    let group = core.groups.get(&sub.group);
                    let topology_version = group.map(|g| g.topology_version).unwrap_or(0);
                    let assigned = sub.assigned.clone();
                    let positions: Vec<PartitionPosition> = assigned.iter().map(|(topic, partition)| {
                        let offset = group
                            .and_then(|g| g.cursor.get(&(topic.clone(), *partition)))
                            .map(|c| c.offset)
                            .unwrap_or(Sequence::NONE);
                        let last_examined = super::transport::hwm_cursor(&core, topic.as_str(), *partition);
                        PartitionPosition {
                            topic: GtsInstanceId::try_new(topic)
                                .expect("registration asserts the topic id"),
                            partition: *partition,
                            offset,
                            last_examined,
                        }
                    }).collect();
                    Ok(StreamBaseline {
                        topology_version,
                        assigned,
                        positions,
                    })
                }
            }
        };
        let baseline = baseline?;
        let baseline_tv = baseline.topology_version;
        let mut current = baseline.assigned;
        let baseline_positions = baseline.positions;
        yield WireFrame::Topology { topology_version: baseline_tv, assigned: baseline_positions };
        // Track the topology version this stream has observed. The mock keeps
        // `sub.topology_version` equal to the group's at all times, so we cannot
        // rely on it to detect a change - compare against the group's tv directly.
        let mut observed_tv = baseline_tv;

        // -- 1. Stream loop ----------------------------------------------------
        loop {
            // Check fault injection first.
            {
                let faults = broker.faults.lock().await;
                if faults.force_gone.contains(&sub_id) {
                    // 410 Gone - graceful shard shutdown.
                    Err(EventBrokerError::Internal(
                        "410: Subscription terminated; re-JOIN to recover".to_owned()
                    ))?;
                }
                if faults.force_not_found.contains(&sub_id) {
                    // 404 SubscriptionNotFound.
                    Err(EventBrokerError::Internal(
                        "404: Subscription not found or expired".to_owned()
                    ))?;
                }
            }

            // Detect a rebalance (topology change) on this subscription.
            {
                let mut core = broker.core.lock().await;
                // Drop the guard BEFORE the error yield - holding `core` across the
                // `?` would deadlock later broker calls once the consumer stops
                // polling the errored stream.
                if !core.subscriptions.contains_key(&sub_id) {
                    drop(core);
                    Err::<(), _>(EventBrokerError::Internal("subscription gone".to_owned()))?;
                    unreachable!();
                }
                let sub = core.subscriptions.get(&sub_id).expect("present (checked above)");
                let group = core.groups.get(&sub.group);
                let current_tv = group.map(|g| g.topology_version).unwrap_or(0);
                if current_tv > observed_tv {
                    let new_assigned = sub.assigned.clone();
                    let positions: Vec<PartitionPosition> = new_assigned.iter().map(|(topic, partition)| {
                        let offset = group
                            .and_then(|g| g.cursor.get(&(topic.clone(), *partition)))
                            .map(|c| c.offset)
                            .unwrap_or(Sequence::NONE);
                        let last_examined = super::transport::hwm_cursor(&core, topic.as_str(), *partition);
                        PartitionPosition {
                            topic: GtsInstanceId::try_new(topic)
                                .expect("registration asserts the topic id"),
                            partition: *partition,
                            offset,
                            last_examined,
                        }
                    }).collect();
                    let current_set: std::collections::HashSet<(String, u32)> =
                        current.iter().cloned().collect();
                    let gained = new_assigned.iter().any(|p| !current_set.contains(p));
                    let lose_all = new_assigned.is_empty();
                    if gained || lose_all {
                        // Mark terminated so any reuse of this id returns 410, and
                        // remove it from group membership so it no longer holds
                        // partitions - the consumer must re-JOIN to get a fresh
                        // subscription. (The terminated SubState is retained for the
                        // 410-on-reuse signal; it just stops participating in rebalance.)
                        let group_id = core.subscriptions.get(&sub_id).map(|s| s.group);
                        if let Some(sub_mut) = core.subscriptions.get_mut(&sub_id) {
                            sub_mut.terminated = true;
                        }
                        if let Some(gid) = group_id
                            && let Some(group) = core.groups.get_mut(&gid)
                        {
                            group.members.retain(|m| *m != sub_id);
                        }
                    }
                    observed_tv = current_tv;
                    drop(core);
                    if gained || lose_all {
                        // Gain / lose-all → terminate: a `terminal` control frame with the
                        // complete final positions, then a graceful close. The consumer
                        // commits the positions and re-JOINs.
                        yield WireFrame::Control {
                            code: ControlCode::Terminal,
                            positions,
                            reason: Some(if lose_all { "lose_all".to_owned() } else { "rebalanced".to_owned() }),
                        };
                        return;
                    }
                    // Loss / version-only change → non-terminal topology frame; keep streaming.
                    current = new_assigned;
                    yield WireFrame::Topology { topology_version: current_tv, assigned: positions };
                }
            }

            // Collect pending events under the lock, then yield outside it.
            let mut pending: Vec<WireFrame> = Vec::new();
            // Sparse offset-adviser positions: partitions whose scan frontier advanced
            // past what was delivered (events scanned but filtered out this round).
            let mut progress_positions: Vec<PartitionPosition> = Vec::new();
            {
                let mut core = broker.core.lock().await;
                let sub = match core.subscriptions.get(&sub_id) {
                    Some(s) => s,
                    None => break,
                };
                let assigned = sub.assigned.clone();

                for (topic, partition) in &assigned {
                    let sub = core.subscriptions.get(&sub_id).unwrap();
                    let seek_offset = sub.seek.get(&(topic.clone(), *partition)).copied();
                    let sent_offset = sub.sent.get(&(topic.clone(), *partition)).copied().unwrap_or(Sequence::NONE);
                    let scanned_off = sub.scanned.get(&(topic.clone(), *partition)).copied().unwrap_or(Sequence::NONE);
                    // Skip past the seek floor AND whatever we've already scanned (incl.
                    // filtered events) so the scan frontier only moves forward.
                    let start = seek_offset
                        .unwrap_or(Sequence::NONE)
                        .max(scanned_off)
                        .max(sent_offset)
                        .max(Sequence::NONE);
                    // Per-interest type-pattern filter for this topic (A5). `"*"` matches all.
                    let patterns: Vec<String> = sub
                        .interests
                        .iter()
                        .find(|i| &i.topic == topic)
                        .map(|i| i.types.clone())
                        .unwrap_or_else(|| vec!["*".to_owned()]);

                    let events: Vec<_> = core.topics
                        .get(topic.as_str())
                        .map(|t| t.read(*partition, start, 100).into_iter().map(|se| se.event.clone()).collect())
                        .unwrap_or_default();

                    let mut frontier = start;
                    let mut last_delivered = sent_offset;
                    let mut round_delivered = 0usize;
                    for stamped in events {
                        let off = stamped.sequence.unwrap_or(Sequence::NONE);
                        frontier = frontier.max(off); // scanned regardless of filter match
                        if !type_matches(&patterns, &stamped.type_id) {
                            continue; // scanned but filtered out - advances the frontier only
                        }
                        last_delivered = off;
                        round_delivered += 1;
                        pending.push(WireFrame::Event(WireEvent {
                            id: stamped.id,
                            type_id: stamped.type_id.clone(),
                            // The stream knows the topic from the partition it is
                            // reading; the stored event does not carry one.
                            tenant_id: stamped.tenant_id,
                            subject: stamped.subject.clone(),
                            subject_type: stamped.subject_type.clone(),
                            partition: stamped.partition.unwrap_or(*partition),
                            sequence: stamped.sequence.unwrap_or(Sequence::NONE),
                            occurred_at: stamped.occurred_at,
                            sequence_time: stamped.sequence_time.unwrap_or_else(chrono::Utc::now),
                            trace_parent: stamped.trace_parent.clone(),
                            data: stamped.data.clone().unwrap_or(serde_json::Value::Null),
                        }));
                    }

                    // Persist the scan frontier + delivered position under lock.
                    if let Some(sub_mut) = core.subscriptions.get_mut(&sub_id) {
                        sub_mut.scanned.insert((topic.clone(), *partition), frontier);
                        if round_delivered > 0 {
                            sub_mut.sent.insert((topic.clone(), *partition), last_delivered);
                        }
                    }
                    // Mirror the frontier into the group cursor's `last_examined`.
                    let group_id = core.subscriptions.get(&sub_id).map(|s| s.group);
                    if let Some(gid) = group_id
                        && let Some(g) = core.groups.get_mut(&gid)
                    {
                        let entry = g.cursor.entry((topic.clone(), *partition)).or_default();
                        entry.last_examined = entry.last_examined.max(frontier);
                    }
                    // Drift: events were scanned-and-filtered (frontier moved) but none
                    // delivered this round → a sparse Progress position so the consumer
                    // learns the true frontier without re-scanning on reconnect (R57).
                    if round_delivered == 0 && frontier > last_delivered {
                        let delivered_pos = seek_offset.unwrap_or(Sequence::NONE).max(last_delivered);
                        progress_positions.push(PartitionPosition {
                            topic: GtsInstanceId::try_new(topic)
                                .expect("registration asserts the topic id"),
                            partition: *partition,
                            offset: delivered_pos,
                            last_examined: frontier,
                        });
                    }
                }
                // Lock released here (end of block).
            }
            let delivered_any = !pending.is_empty();

            // Yield collected frames outside the lock.
            for frame in pending {
                yield frame;
            }

            // Emit a conditional, sparse Progress control frame for filter-saturated
            // partitions (A5) - only when this round scanned events but delivered none.
            if !progress_positions.is_empty() {
                yield WireFrame::Control {
                    code: ControlCode::Progress,
                    positions: progress_positions,
                    reason: None,
                };
            }

            if !delivered_any {
                // Idle - wait for a new event or heartbeat timeout.
                let heartbeat_interval = {
                    let faults = broker.faults.lock().await;
                    faults.heartbeat_interval
                };

                tokio::select! {
                    _ = broker.notify.notified() => {
                        // New event published or topology changed - loop again.
                    }
                    _ = sleep(heartbeat_interval) => {
                        yield WireFrame::Heartbeat { at: chrono::Utc::now().to_rfc3339() };
                    }
                }
            }
        }
    };

    Box::pin(GuardedStream {
        inner: Box::pin(stream),
        _guard: guard,
    })
}
