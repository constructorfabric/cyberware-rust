//! In-process consumer group state and partition-assignment coordinator.
//! (`eb-group-rebalance-coordinator` design.md D1–D7)
//!
//! A consumer group is owned by exactly one delivery instance.  All open
//! streams for that group live in the same OS process and are reachable via
//! in-process `mpsc` channels — no cluster watch or distributed lock is
//! needed for steady-state operation.

use crate::domain::model::Sequence;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::Assignment;
use crate::domain::streaming::assignment::Generation;

/// Topic identity and partition count, passed to `join`.
pub struct TopicInterest {
    pub id: GtsInstanceId,
    pub partitions: i32,
}

pub struct ConsumerGroupCoordinator {
    pub(crate) state: Mutex<HashMap<GtsInstanceId, GroupState>>,
}

pub(crate) struct GroupState {
    pub(crate) topology_version: i64,
    pub(crate) members: HashMap<Uuid, MemberEntry>,
    /// `true` while the grace-period timer for a dead member is outstanding.
    pub(crate) timer_running: bool,
}

pub(crate) struct MemberEntry {
    /// `(topic_id, partition_count)` for each topic this member is interested in.
    pub(crate) interests: Vec<(GtsInstanceId, i32)>,
    pub(crate) assigned: Vec<Assignment>,
    pub(crate) status: MemberStatus,
    pub(crate) session_timeout: Duration,
    /// This member's assignment, published rather than pushed.
    ///
    /// A `watch` and not a channel: it keeps only the latest value, so a
    /// topology change cannot be dropped by delivery-path backpressure - the
    /// `try_send` this replaced discarded a topology or terminal frame whenever
    /// the consumer's 16-slot buffer was full.
    ///
    /// Present from join, with no open stream: a `watch::Sender` is valid with
    /// zero receivers, which is what makes `receiver_count()` mean "no stream
    /// open". The coordinator publishes; the session classifies.
    pub(crate) generations: watch::Sender<Generation>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum MemberStatus {
    Active,
    /// Stream connection closed; partitions held pending `session_timeout`.
    Unassigned,
}

impl ConsumerGroupCoordinator {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Records the new member, computes a range-based partition split across
    /// all active members, increments `topology_version`, and pushes
    /// `Frame::Topology` to existing members that lose partitions.
    ///
    /// Returns `(new_member_assigned, topology_version, sibling_updates)` where
    /// `sibling_updates` is the list of `(sub_id, new_assigned)` pairs for all
    /// existing members whose assignments changed, so the caller can persist
    /// those changes back to the subscription store.
    pub fn join(
        &self,
        group_id: &GtsInstanceId,
        sub_id: Uuid,
        interests: &[TopicInterest],
        session_timeout: Duration,
    ) -> (Vec<Assignment>, i64, Vec<(Uuid, Vec<Assignment>)>) {
        let mut state = self.state.lock().unwrap();
        let group = state.entry(group_id.clone()).or_insert_with(|| GroupState {
            topology_version: 0,
            members: HashMap::new(),
            timer_running: false,
        });

        // Evict Unassigned members — a new JOIN fills the gap; the pending
        // timer will find no Unassigned entries and exit without action.
        group
            .members
            .retain(|_, m| m.status == MemberStatus::Active);

        group.members.insert(
            sub_id,
            MemberEntry {
                interests: interests
                    .iter()
                    .map(|t| (t.id.clone(), t.partitions))
                    .collect(),
                assigned: Vec::new(),
                status: MemberStatus::Active,
                session_timeout,
                // Seeded empty at the version the join is about to produce;
                // `publish_generations` below sends the real assignment. A
                // sender with no receivers is exactly the "no stream open"
                // state.
                generations: watch::Sender::new(Generation::new(
                    group.topology_version,
                    Vec::new(),
                )),
            },
        );

        group.topology_version += 1;
        let version = group.topology_version;
        let new_assignments = compute_assignments(&group.members);

        // Existing members can only lose partitions on JOIN — all get Topology.
        publish_generations(&*group, &new_assignments, version);

        let sibling_updates: Vec<(Uuid, Vec<Assignment>)> = new_assignments
            .iter()
            .filter(|(id, _)| **id != sub_id)
            .map(|(id, assigned)| (*id, assigned.clone()))
            .collect();

        for (&id, assigned) in &new_assignments {
            if let Some(m) = group.members.get_mut(&id) {
                m.assigned = assigned.clone();
            }
        }

        let my_assigned = new_assignments.get(&sub_id).cloned().unwrap_or_default();
        (my_assigned, version, sibling_updates)
    }

    /// Removes the member, redistributes their partitions, and pushes
    /// `Frame::Control { code: Terminal }` to surviving members that gain
    /// partitions.
    pub fn leave(&self, group_id: &GtsInstanceId, sub_id: Uuid) {
        let mut state = self.state.lock().unwrap();

        let is_empty = {
            let Some(group) = state.get_mut(group_id) else {
                return;
            };
            group.members.remove(&sub_id);

            if !group.members.is_empty() {
                group.topology_version += 1;
                let version = group.topology_version;
                let new_assignments = compute_assignments(&group.members);
                // sub_id is already removed — skip_id is a no-op sentinel here.
                publish_generations(&*group, &new_assignments, version);
                for (&id, assigned) in &new_assignments {
                    if let Some(m) = group.members.get_mut(&id) {
                        m.assigned = assigned.clone();
                    }
                }
            }

            group.members.is_empty()
        };

        if is_empty {
            state.remove(group_id);
        }
    }

    /// Subscribes to this member's assignment, and takes the handle whose drop
    /// reports the stream closed.
    ///
    /// Returns `None` when the member is unknown - a caller racing a `leave`.
    pub fn subscribe(
        this: &Arc<Self>,
        group_id: &GtsInstanceId,
        sub_id: Uuid,
    ) -> Option<(watch::Receiver<Generation>, MembershipHandle)> {
        let receiver = {
            let state = this.state.lock().unwrap();
            let group = state.get(group_id)?;
            let member = group.members.get(&sub_id)?;
            member.generations.subscribe()
        };
        Some((
            receiver,
            MembershipHandle {
                coordinator: Arc::clone(this),
                group_id: group_id.clone(),
                sub_id,
            },
        ))
    }

    /// Marks a member unassigned and arms the grace period.
    ///
    /// Lifted verbatim out of the task that used to await `tx.closed()`. The
    /// signal changed - it is now a `Drop` rather than a channel close - and
    /// the state machine did not.
    pub(crate) fn on_stream_closed(this: &Arc<Self>, group_id: &GtsInstanceId, sub_id: Uuid) {
        let should_start_timer = {
            let mut state = this.state.lock().unwrap();
            let Some(group) = state.get_mut(group_id) else {
                return;
            };
            let Some(member) = group.members.get_mut(&sub_id) else {
                return;
            };
            let timeout = member.session_timeout;
            member.status = MemberStatus::Unassigned;
            if group.timer_running {
                None
            } else {
                group.timer_running = true;
                Some(timeout)
            }
        };
        let Some(timeout) = should_start_timer else {
            return;
        };

        // `Drop` may run off a runtime - a test that builds a session and drops
        // it synchronously does exactly that - and `tokio::spawn` panics there.
        // Without a runtime the grace period simply does not run, which leaves
        // the member `Unassigned` for the next join or leave to resolve rather
        // than taking the process down.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                %group_id,
                %sub_id,
                "stream closed outside a runtime; grace timer not armed"
            );
            return;
        };
        let group_id = group_id.clone();
        let coordinator = Arc::clone(this);
        handle.spawn(async move {
            tokio::time::sleep(timeout).await;
            coordinator.timer_fired(&group_id);
        });
    }

    /// Called by the one-shot timer.  If Unassigned partitions remain, evicts
    /// the dead members and rebalances survivors.  If the consumer reconnected
    /// and re-JOINed in the meantime, exits without action.
    pub(crate) fn timer_fired(&self, group_id: &GtsInstanceId) {
        let mut state = self.state.lock().unwrap();

        let is_empty = {
            let Some(group) = state.get_mut(group_id) else {
                return;
            };
            group.timer_running = false;

            let has_unassigned = group
                .members
                .values()
                .any(|m| m.status == MemberStatus::Unassigned);
            if !has_unassigned {
                return;
            }

            group
                .members
                .retain(|_, m| m.status == MemberStatus::Active);

            if !group.members.is_empty() {
                group.topology_version += 1;
                let version = group.topology_version;
                let new_assignments = compute_assignments(&group.members);
                // All surviving members may gain — use nil sentinel so none are skipped.
                publish_generations(&*group, &new_assignments, version);
                for (&id, assigned) in &new_assignments {
                    if let Some(m) = group.members.get_mut(&id) {
                        m.assigned = assigned.clone();
                    }
                }
            }

            group.members.is_empty()
        };

        if is_empty {
            state.remove(group_id);
        }
    }
}

/// Computes range-based partition assignments for all active members across
/// every topic any of them is interested in.
fn compute_assignments(members: &HashMap<Uuid, MemberEntry>) -> HashMap<Uuid, Vec<Assignment>> {
    let mut result: HashMap<Uuid, Vec<Assignment>> =
        members.keys().map(|&id| (id, Vec::new())).collect();

    let mut all_topics: HashMap<GtsInstanceId, i32> = HashMap::new();
    for m in members.values() {
        for (topic_id, n_parts) in &m.interests {
            all_topics.entry(topic_id.clone()).or_insert(*n_parts);
        }
    }

    for (topic_id, n_partitions) in &all_topics {
        let mut interested: Vec<Uuid> = members
            .iter()
            .filter(|(_, m)| m.interests.iter().any(|(t, _)| t == topic_id))
            .map(|(&id, _)| id)
            .collect();
        interested.sort_unstable(); // deterministic ordering by UUID

        let splits = range_split(*n_partitions as u32, &interested);
        for (i, &member_id) in interested.iter().enumerate() {
            let entries = result.entry(member_id).or_default();
            for &p in &splits[i] {
                entries.push(Assignment {
                    topic: topic_id.clone(),
                    partition: p as i32,
                    offset: Sequence::assigned(0),
                    last_examined: Sequence::assigned(0),
                });
            }
        }
    }

    result
}

/// Pushes `Frame::Topology` (loss only) or `Frame::Control { Terminal }`
/// (gain or lose-all) to members whose assignments changed.
/// `skip_id` is excluded — used to skip the newly-joined member who has no
/// open stream yet.
/// One session's membership in its group, for as long as its stream lives.
///
/// Dropping it reports the stream closed, which marks the member unassigned and
/// arms the grace period. That replaces a task per stream awaiting a channel
/// close: the signal is the same moment, and the mechanism is the one this
/// change already uses twice - `StreamLease` releases exclusion on drop, and the
/// module owns the loader's lifetime the same way.
pub struct MembershipHandle {
    coordinator: Arc<ConsumerGroupCoordinator>,
    group_id: GtsInstanceId,
    sub_id: Uuid,
}

impl MembershipHandle {
    #[must_use]
    pub fn subscription_id(&self) -> Uuid {
        self.sub_id
    }
}

impl Drop for MembershipHandle {
    fn drop(&mut self) {
        ConsumerGroupCoordinator::on_stream_closed(&self.coordinator, &self.group_id, self.sub_id);
    }
}

impl std::fmt::Debug for MembershipHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MembershipHandle")
            .field("group_id", &self.group_id)
            .field("sub_id", &self.sub_id)
            .finish()
    }
}

/// Publishes each changed member's new assignment on its own `watch`.
///
/// No classification and no frames. Whether a change is a gain, a loss, or only
/// a version bump is `AssignmentDelta::classify`'s answer, and it is the
/// session's to ask - it is the only party that knows where its readers
/// actually are. The coordinator computes assignments; it does not decide
/// frames.
fn publish_generations(
    group: &GroupState,
    new_assignments: &HashMap<Uuid, Vec<Assignment>>,
    version: i64,
) {
    // No member is skipped, including the one whose join or leave triggered
    // this. The frame-push version skipped the caller, because it received its
    // topology baseline in the response body and a pushed frame would have
    // duplicated it. A `watch` is state rather than an event: every member's
    // watch has to hold that member's *current* assignment, or a session opening
    // later reads a stale baseline and classifies its first real change against
    // it. Skipping the joiner left its watch on the empty seed, so the next
    // sibling change looked like a gain and terminated a stream that had only
    // lost partitions.
    for (&member_id, new_assigned) in new_assignments {
        let Some(member) = group.members.get(&member_id) else {
            continue;
        };

        let old_set: HashSet<(GtsInstanceId, i32)> = member
            .assigned
            .iter()
            .map(|a| (a.topic.clone(), a.partition))
            .collect();
        let new_set: HashSet<(GtsInstanceId, i32)> = new_assigned
            .iter()
            .map(|a| (a.topic.clone(), a.partition))
            .collect();
        if new_set == old_set {
            continue;
        }

        // `send_replace`, not `send`. `send` *fails and discards the value* when
        // there are no receivers, and at join time there is none yet - the
        // stream opens afterwards. That silently left a member's watch holding
        // its empty seed, so its session's first real change classified against
        // nothing and read as a gain, terminating a stream that had only lost
        // partitions. A watch used as state has to be written whether or not
        // anyone is currently listening.
        member
            .generations
            .send_replace(Generation::new(version, new_assigned.clone()));
    }
}

/// Partition split: member `i` of `k` receives partitions `[i*n/k .. (i+1)*n/k)`.
/// Produces contiguous ranges; minimises partition movement on incremental JOIN/LEAVE.
pub fn range_split(n_partitions: u32, members: &[Uuid]) -> Vec<Vec<u32>> {
    let k = members.len() as u32;
    if k == 0 {
        return Vec::new();
    }
    (0..k)
        .map(|i| {
            let start = i * n_partitions / k;
            let end = (i + 1) * n_partitions / k;
            (start..end).collect()
        })
        .collect()
}

#[cfg(test)]
mod tests;
