//! In-memory pull-based task queue.
//!
//! Ported from the reference implementation's `scheduler/src/queue.rs`. A sync
//! session is orchestrated entirely in memory: tasks live only for the
//! duration of one run and are never persisted (DESIGN §4 "tasks are NOT
//! persisted"). Re-running a sync rebuilds the work set from scratch, relying
//! on the HTTP cache and the durable change-detection state to skip unchanged
//! work.
//!
//! Workers pull work via [`TaskQueue::claim_next_task_in`]; there are no
//! leases, heartbeats, or crash recovery. Operations are O(1) or O(log n):
//! enqueue de-duplicates through a key index, and pending tasks are held in a
//! per-`(session, phase)` ordered index so claims never scan the work set.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Utc};
use strum::IntoEnumIterator as _;
use uuid::Uuid;

use super::task::{ExtractionTask, Lane, NewTask, TaskPhase, TaskStatus};

/// Idempotency key: a task is unique per `(session, phase, entity_type,
/// entity_id)`. The session already belongs to exactly one tenant, so tenancy
/// is carried by the key without a separate column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DedupKey {
    tenant_id: Uuid,
    session_id: Uuid,
    phase: TaskPhase,
    entity_type: String,
    entity_id: Option<String>,
    attempt: u32,
}

impl DedupKey {
    fn of(task: &NewTask) -> Self {
        Self {
            tenant_id: task.tenant_id,
            session_id: task.session_id,
            phase: task.phase,
            entity_type: task.entity_type.clone(),
            entity_id: task.entity_id.clone(),
            attempt: task.attempt,
        }
    }
}

/// Ordering key within one `(session, phase)` pending bucket.
///
/// Claim order is `priority DESC, created_at ASC`; `neg_priority` makes a
/// higher priority sort first under `BTreeMap`'s ascending order, and `id` is
/// a uniqueness tie-breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    neg_priority: i64,
    created_at: DateTime<Utc>,
    id: Uuid,
}

impl OrderKey {
    fn of(task: &ExtractionTask) -> Self {
        Self {
            neg_priority: -i64::from(task.priority.0),
            created_at: task.created_at,
            id: task.id,
        }
    }
}

type BucketKey = (Uuid, TaskPhase);

#[derive(Debug, Default)]
struct Inner {
    by_id: HashMap<Uuid, ExtractionTask>,
    dedup: HashMap<DedupKey, Uuid>,
    pending: HashMap<BucketKey, BTreeMap<OrderKey, Uuid>>,
    running: HashMap<BucketKey, u64>,
}

/// In-memory pull-based task queue. Cheaply cloneable: the indexed state is
/// shared behind an `Arc<Mutex<_>>`.
#[derive(Debug, Clone, Default)]
pub struct TaskQueue {
    inner: Arc<Mutex<Inner>>,
}

impl TaskQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Idempotently insert a new task: a second enqueue of the same
    /// `(session, phase, entity_type, entity_id)` is a no-op.
    pub fn enqueue_task(&self, task: &NewTask) {
        let mut inner = self.lock();
        let key = DedupKey::of(task);
        if inner.dedup.contains_key(&key) {
            return;
        }
        let row = ExtractionTask {
            id: Uuid::new_v4(),
            session_id: task.session_id,
            tenant_id: task.tenant_id,
            phase: task.phase,
            entity_type: task.entity_type.clone(),
            entity_id: task.entity_id.clone(),
            priority: task.priority,
            attempt: task.attempt,
            status: TaskStatus::Pending,
            created_at: Utc::now(),
        };
        inner
            .pending
            .entry((row.session_id, row.phase))
            .or_default()
            .insert(OrderKey::of(&row), row.id);
        inner.dedup.insert(key, row.id);
        inner.by_id.insert(row.id, row);
    }

    /// Claim the highest-priority pending task whose phase is one of `phases`,
    /// moving it to `Running`.
    ///
    /// Phases are tried in ascending weight order, so an earlier phase is
    /// always preferred; within a phase, `priority DESC, created_at ASC`.
    /// Returns `None` when no task in any of the given phases is claimable.
    #[must_use]
    pub fn claim_next_task_in(
        &self,
        session_id: Uuid,
        phases: &[TaskPhase],
    ) -> Option<ExtractionTask> {
        self.claim(session_id, phases, None)
    }

    /// Claim only from `lane`, so one family cannot starve another.
    #[must_use]
    pub fn claim_next_task_in_lane(
        &self,
        session_id: Uuid,
        phases: &[TaskPhase],
        lane: Lane,
    ) -> Option<ExtractionTask> {
        self.claim(session_id, phases, Some(lane))
    }

    fn claim(
        &self,
        session_id: Uuid,
        phases: &[TaskPhase],
        lane: Option<Lane>,
    ) -> Option<ExtractionTask> {
        let mut inner = self.lock();

        let (phase, key, id) = TaskPhase::iter()
            .filter(|p| phases.contains(p))
            .find_map(|p| {
                let bucket = inner.pending.get(&(session_id, p))?;
                bucket
                    .iter()
                    .find(|(_, id)| {
                        lane.is_none_or(|lane| {
                            inner
                                .by_id
                                .get(id)
                                .is_some_and(|task| Lane::of_entity_type(&task.entity_type) == lane)
                        })
                    })
                    .map(|(k, id)| (p, *k, *id))
            })?;

        let bucket_key: BucketKey = (session_id, phase);
        if let Some(bucket) = inner.pending.get_mut(&bucket_key) {
            bucket.remove(&key);
            if bucket.is_empty() {
                inner.pending.remove(&bucket_key);
            }
        }
        let running_count: &mut u64 = inner.running.entry(bucket_key).or_insert(0);
        *running_count += 1;

        let task = inner.by_id.get_mut(&id)?;
        task.status = TaskStatus::Running;
        Some(task.clone())
    }

    pub fn complete_task(&self, task_id: Uuid) {
        self.finish_task(task_id, TaskStatus::Done);
    }

    /// The failure reason is logged at the call site.
    pub fn fail_task(&self, task_id: Uuid) {
        self.finish_task(task_id, TaskStatus::Failed);
    }

    fn finish_task(&self, task_id: Uuid, status: TaskStatus) {
        let mut inner = self.lock();
        let Some(task) = inner.by_id.get(&task_id).cloned() else {
            return;
        };
        let bucket_key: BucketKey = (task.session_id, task.phase);
        match task.status {
            TaskStatus::Running => {
                if let Some(count) = inner.running.get_mut(&bucket_key) {
                    *count = count.saturating_sub(1);
                }
            }
            TaskStatus::Pending => {
                if let Some(pending) = inner.pending.get_mut(&bucket_key) {
                    pending.remove(&OrderKey::of(&task));
                    if pending.is_empty() {
                        inner.pending.remove(&bucket_key);
                    }
                }
            }
            TaskStatus::Done | TaskStatus::Failed => {}
        }
        if let Some(stored) = inner.by_id.get_mut(&task_id) {
            stored.status = status;
        }
    }

    /// Pending tasks for `session_id` across all phases.
    #[must_use]
    pub fn pending_count(&self, session_id: Uuid) -> u64 {
        let inner = self.lock();
        TaskPhase::iter()
            .filter_map(|p| inner.pending.get(&(session_id, p)))
            .map(|bucket| u64::try_from(bucket.len()).unwrap_or(u64::MAX))
            .sum()
    }

    /// Tasks of `phase` still to be processed: pending plus running.
    #[must_use]
    pub fn remaining_count_for_phase(&self, session_id: Uuid, phase: TaskPhase) -> u64 {
        let inner = self.lock();
        let bucket_key: BucketKey = (session_id, phase);
        let pending = inner
            .pending
            .get(&bucket_key)
            .map_or(0, |bucket| u64::try_from(bucket.len()).unwrap_or(u64::MAX));
        let running = inner.running.get(&bucket_key).copied().unwrap_or(0);
        pending + running
    }

    /// Every task of `phase` ever enqueued for `session_id`, whatever its state.
    #[must_use]
    pub fn count_for_phase(&self, session_id: Uuid, phase: TaskPhase) -> u64 {
        let inner = self.lock();
        inner
            .by_id
            .values()
            .filter(|task| task.session_id == session_id && task.phase == phase)
            .count()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::domain::sync::task::TaskPriority;

    fn all_phases() -> Vec<TaskPhase> {
        TaskPhase::iter().collect()
    }

    fn discovery_task(session: Uuid) -> NewTask {
        NewTask {
            session_id: session,
            tenant_id: Uuid::nil(),
            phase: TaskPhase::Discovery,
            entity_type: "repository".to_owned(),
            entity_id: None,
            priority: TaskPriority::NORMAL,
            attempt: 0,
        }
    }

    fn refinement_task(session: Uuid, entity_id: &str, priority: TaskPriority) -> NewTask {
        NewTask {
            session_id: session,
            tenant_id: Uuid::nil(),
            phase: TaskPhase::Refinement,
            entity_type: "issue".to_owned(),
            entity_id: Some(entity_id.to_owned()),
            priority,
            attempt: 0,
        }
    }

    fn lane_task(session: Uuid, tenant: Uuid, entity_type: &str, entity_id: &str) -> NewTask {
        NewTask {
            session_id: session,
            tenant_id: tenant,
            phase: TaskPhase::Refinement,
            entity_type: entity_type.to_owned(),
            entity_id: Some(entity_id.to_owned()),
            priority: TaskPriority::NORMAL,
            attempt: 0,
        }
    }

    #[test]
    fn the_same_task_for_two_tenants_is_kept_apart() {
        let session = Uuid::new_v4();
        let queue = TaskQueue::new();
        let one = lane_task(session, Uuid::new_v4(), "issue", "11");
        let other = lane_task(session, Uuid::new_v4(), "issue", "11");

        queue.enqueue_task(&one);
        queue.enqueue_task(&other);

        assert_eq!(
            queue.count_for_phase(session, TaskPhase::Refinement),
            2,
            "two tenants asking for the same issue must not collapse into one task"
        );
    }

    #[test]
    fn the_same_task_for_one_tenant_is_enqueued_once() {
        let session = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let queue = TaskQueue::new();

        queue.enqueue_task(&lane_task(session, tenant, "issue", "11"));
        queue.enqueue_task(&lane_task(session, tenant, "issue", "11"));

        assert_eq!(queue.count_for_phase(session, TaskPhase::Refinement), 1);
    }

    #[test]
    fn a_lane_claim_only_takes_its_own_family() {
        let session = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let queue = TaskQueue::new();
        queue.enqueue_task(&lane_task(session, tenant, "pull_request", "13"));
        queue.enqueue_task(&lane_task(session, tenant, "issue", "11"));

        let claimed = queue
            .claim_next_task_in_lane(session, &[TaskPhase::Refinement], Lane::Issue)
            .expect("the issue lane has work");

        assert_eq!(claimed.entity_type, "issue");
    }

    #[test]
    fn an_empty_lane_claims_nothing() {
        let session = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let queue = TaskQueue::new();
        queue.enqueue_task(&lane_task(session, tenant, "issue", "11"));

        assert!(
            queue
                .claim_next_task_in_lane(session, &[TaskPhase::Refinement], Lane::PullRequest)
                .is_none()
        );
    }

    #[test]
    fn every_other_family_falls_into_the_generic_lane() {
        assert_eq!(Lane::of_entity_type("pull_request"), Lane::PullRequest);
        assert_eq!(Lane::of_entity_type("issue"), Lane::Issue);
        assert_eq!(Lane::of_entity_type("commit"), Lane::Generic);
        assert_eq!(Lane::of_entity_type("workflow_run"), Lane::Generic);
    }
    #[test]
    fn enqueue_is_idempotent() {
        let queue = TaskQueue::new();
        let session = Uuid::new_v4();
        queue.enqueue_task(&discovery_task(session));
        queue.enqueue_task(&discovery_task(session));
        assert_eq!(queue.pending_count(session), 1);
        assert_eq!(queue.count_for_phase(session, TaskPhase::Discovery), 1);
    }

    #[test]
    fn claim_moves_the_task_to_running() {
        let queue = TaskQueue::new();
        let session = Uuid::new_v4();
        queue.enqueue_task(&discovery_task(session));

        let task = queue
            .claim_next_task_in(session, &all_phases())
            .expect("claim");
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(queue.pending_count(session), 0);
        assert_eq!(
            queue.remaining_count_for_phase(session, TaskPhase::Discovery),
            1
        );

        queue.complete_task(task.id);
        assert_eq!(
            queue.remaining_count_for_phase(session, TaskPhase::Discovery),
            0
        );
    }

    #[test]
    fn claim_on_an_empty_queue_is_none() {
        let queue = TaskQueue::new();
        assert!(
            queue
                .claim_next_task_in(Uuid::new_v4(), &all_phases())
                .is_none()
        );
    }

    #[test]
    fn earlier_phases_are_claimed_first() {
        let queue = TaskQueue::new();
        let session = Uuid::new_v4();
        queue.enqueue_task(&refinement_task(session, "42", TaskPriority::NORMAL));
        queue.enqueue_task(&discovery_task(session));

        let first = queue
            .claim_next_task_in(session, &all_phases())
            .expect("first claim");
        assert_eq!(first.phase, TaskPhase::Discovery);
    }

    #[test]
    fn the_phase_filter_hides_other_phases() {
        let queue = TaskQueue::new();
        let session = Uuid::new_v4();
        queue.enqueue_task(&discovery_task(session));

        assert!(
            queue
                .claim_next_task_in(session, &[TaskPhase::Refinement])
                .is_none()
        );
        assert!(
            queue
                .claim_next_task_in(session, &[TaskPhase::Discovery])
                .is_some()
        );
    }

    #[test]
    fn higher_priority_is_claimed_first_within_a_phase() {
        let queue = TaskQueue::new();
        let session = Uuid::new_v4();
        queue.enqueue_task(&refinement_task(
            session,
            "closed",
            TaskPriority::CLOSED_ISSUE,
        ));
        queue.enqueue_task(&refinement_task(session, "open", TaskPriority::OPEN_ISSUE));

        let first = queue
            .claim_next_task_in(session, &[TaskPhase::Refinement])
            .expect("claim");
        assert_eq!(first.entity_id.as_deref(), Some("open"));
    }

    #[test]
    fn sessions_do_not_see_each_other() {
        let queue = TaskQueue::new();
        let mine = Uuid::new_v4();
        queue.enqueue_task(&discovery_task(mine));
        assert!(
            queue
                .claim_next_task_in(Uuid::new_v4(), &all_phases())
                .is_none()
        );
        assert_eq!(queue.count_for_phase(mine, TaskPhase::Discovery), 1);
    }
}
