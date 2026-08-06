//! The partitions this instance is currently serving.
//!
//! Caches are created on demand, never at bootstrap: an instance does not know
//! which topics or partitions it will be asked for, and standing up a cache for
//! every partition of every topic would cost memory for nothing. The first
//! session or the first demand for a `(topic, partition)` brings its cache into
//! existence.
//!
//! Which makes retirement this module's other job. Without it a long-lived
//! instance accumulates a cache for every partition it has ever touched.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::domain::streaming::source::PartitionKey;
use crate::infra::partition_cache::cache::PartitionCache;
use crate::infra::partition_cache::reclaim::ReclaimPolicy;

use super::poll::{PollPolicy, TailPoll};

/// How one topic's partitions are sized and paced.
///
/// Per topic rather than global because payload size, and therefore bytes per
/// event, differs by topic - the same residency limit in bytes buys very
/// different numbers of events for a small envelope and a large document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopicPolicy {
    reclaim: ReclaimPolicy,
    fetch_max_events: usize,
    poll: PollPolicy,
}

impl TopicPolicy {
    #[must_use]
    pub fn builder(reclaim: ReclaimPolicy) -> TopicPolicyBuilder {
        TopicPolicyBuilder {
            reclaim,
            fetch_max_events: 256,
            poll: PollPolicy::default(),
        }
    }

    #[must_use]
    pub fn reclaim(self) -> ReclaimPolicy {
        self.reclaim
    }

    #[must_use]
    pub fn fetch_max_events(self) -> usize {
        self.fetch_max_events
    }

    #[must_use]
    pub fn poll(self) -> PollPolicy {
        self.poll
    }
}

impl Default for TopicPolicy {
    fn default() -> Self {
        Self::builder(ReclaimPolicy::default()).build()
    }
}

pub struct TopicPolicyBuilder {
    reclaim: ReclaimPolicy,
    fetch_max_events: usize,
    poll: PollPolicy,
}

impl TopicPolicyBuilder {
    #[must_use]
    pub fn fetch_max_events(mut self, events: usize) -> Self {
        self.fetch_max_events = events.max(1);
        self
    }

    #[must_use]
    pub fn poll(mut self, poll: PollPolicy) -> Self {
        self.poll = poll;
        self
    }

    #[must_use]
    pub fn build(self) -> TopicPolicy {
        TopicPolicy {
            reclaim: self.reclaim,
            fetch_max_events: self.fetch_max_events,
            poll: self.poll,
        }
    }
}

/// One partition's cache plus the scheduling state that belongs beside it.
pub struct Partition {
    key: PartitionKey,
    cache: Arc<PartitionCache>,
    /// Set while a fetch for this partition is outstanding.
    ///
    /// Without it every worker in the pool can pile onto the same hungry
    /// partition and issue the same fetch, which is the uncoalesced behaviour
    /// the whole design exists to avoid - reintroduced by the scheduler rather
    /// than by the readers.
    in_flight: AtomicBool,
    poll: Mutex<TailPoll>,
    /// The last round that did something for this partition. A tick count
    /// rather than a clock, so retirement is deterministic in tests.
    last_active_round: AtomicU64,
}

impl Partition {
    #[must_use]
    pub fn key(&self) -> &PartitionKey {
        &self.key
    }

    #[must_use]
    pub fn cache(&self) -> &Arc<PartitionCache> {
        &self.cache
    }

    /// Claims this partition for a fetch, or reports that one is already out.
    #[must_use]
    pub fn claim(&self) -> bool {
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn release(&self) {
        self.in_flight.store(false, Ordering::Release);
    }

    #[must_use]
    pub fn is_claimed(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }

    pub fn poll(&self) -> MutexGuard<'_, TailPoll> {
        self.poll.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn touch(&self, round: u64) {
        self.last_active_round.store(round, Ordering::Relaxed);
    }

    #[must_use]
    pub fn last_active_round(&self) -> u64 {
        self.last_active_round.load(Ordering::Relaxed)
    }
}

/// Every partition this instance currently holds a cache for.
pub struct TopicManager {
    partitions: RwLock<HashMap<PartitionKey, Arc<Partition>>>,
    policy: TopicPolicy,
}

impl TopicManager {
    #[must_use]
    pub fn new(policy: TopicPolicy) -> Self {
        Self {
            partitions: RwLock::new(HashMap::new()),
            policy,
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, HashMap<PartitionKey, Arc<Partition>>> {
        self.partitions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<PartitionKey, Arc<Partition>>> {
        self.partitions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[must_use]
    pub fn policy(&self) -> TopicPolicy {
        self.policy
    }

    /// This partition's cache, creating it if this is the first ask.
    ///
    /// Read-locked on the hit path, which is what makes it cheap enough to call
    /// from a session attaching or from a demand scan. Not on the delivery path:
    /// a session holds the returned handle and reads through it.
    #[must_use]
    pub fn attach(&self, key: &PartitionKey) -> Arc<Partition> {
        if let Some(existing) = self.read().get(key) {
            return Arc::clone(existing);
        }

        let mut partitions = self.write();
        // Checked again under the write lock: another caller may have created it
        // while this one was upgrading.
        Arc::clone(partitions.entry(key.clone()).or_insert_with(|| {
            Arc::new(Partition {
                key: key.clone(),
                cache: PartitionCache::with_reclaim_policy(self.policy.reclaim()),
                in_flight: AtomicBool::new(false),
                poll: Mutex::new(TailPoll::new(self.policy.poll())),
                last_active_round: AtomicU64::new(0),
            })
        }))
    }

    #[must_use]
    pub fn get(&self, key: &PartitionKey) -> Option<Arc<Partition>> {
        self.read().get(key).map(Arc::clone)
    }

    /// A snapshot of the live partitions.
    ///
    /// The scheduler walks a snapshot rather than the map itself, so a partition
    /// created mid-round cannot be visited twice or skipped; it joins the next
    /// round.
    #[must_use]
    pub fn live(&self) -> Vec<Arc<Partition>> {
        self.read().values().map(Arc::clone).collect()
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.read().len()
    }

    /// Drops partitions nothing has wanted for `idle_rounds`.
    ///
    /// Only when the map is the sole holder. A partition someone still holds
    /// must not be removed: a later `attach` would build a *second* cache for
    /// the same key, and the two would accumulate different spans while readers
    /// on each believed they had the partition's state.
    pub fn retire_idle(&self, round: u64, idle_rounds: u64) -> usize {
        let mut partitions = self.write();
        let before = partitions.len();
        partitions.retain(|_, partition| {
            let idle = round.saturating_sub(partition.last_active_round()) >= idle_rounds;
            let unheld = Arc::strong_count(partition) == 1;
            let unread = partition.cache().reader_count() == 0;
            !(idle && unheld && unread)
        });
        before.saturating_sub(partitions.len())
    }
}
