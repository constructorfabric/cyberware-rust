//! The deadline-armed TTL sweeper (DESIGN.md §6.2).
//!
//! Read-path expiry is authoritative — an entry past its `expiresAt` reads as absent
//! whether or not it still exists (§6.2) — so the sweeper is only reclamation: it
//! deletes expired objects so they do not accumulate. It is a min-heap keyed by
//! deadline, fed by the shared watcher (every write carries an `expiresAt`) and by
//! the startup scan; when the earliest deadline passes it issues a **guarded**
//! delete, and a `404`/`409` (already gone, or revived) is dropped, never retried.
//!
//! The heap's ordering and its dedupe — a key re-armed with a new deadline
//! supersedes the old entry rather than deleting twice — are pure and carry the L1
//! coverage ([`SweepHeap`]); the delete I/O is Phase 6.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::jiff::Timestamp;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{CacheRuntime, SweepCmd, is_expired};

/// The idle sleep when nothing is armed — a bounded wake so a lost command cannot
/// wedge the loop; real work is driven by commands and armed deadlines.
const IDLE_TICK: Duration = Duration::from_hours(1);

/// One armed deadline: delete `key` at `at`, unless superseded first.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Armed {
    at: Timestamp,
    key: String,
}

// Order by deadline only (the heap is wrapped in `Reverse` for min-first); the key
// breaks ties so equal-deadline entries have a total order.
impl Ord for Armed {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.key.cmp(&other.key))
    }
}
impl PartialOrd for Armed {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A min-heap of expiry deadlines with per-key dedupe (§6.2).
///
/// Re-arming a key records its *latest* deadline; earlier heap entries for that key
/// are recognised as stale on pop (their deadline no longer matches the latest) and
/// skipped, so a key written N times is deleted once, at its final deadline.
#[derive(Default)]
pub struct SweepHeap {
    heap: BinaryHeap<Reverse<Armed>>,
    /// The current (latest) deadline armed for each key.
    latest: HashMap<String, Timestamp>,
}

impl SweepHeap {
    /// A fresh, empty heap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms (or re-arms) `key` to be swept at `at`. A later call for the same key
    /// supersedes the earlier deadline.
    pub fn arm(&mut self, key: &str, at: Timestamp) {
        self.latest.insert(key.to_owned(), at);
        self.heap.push(Reverse(Armed {
            at,
            key: key.to_owned(),
        }));
        self.compact_if_bloated();
    }

    /// Cancels any armed deadline for `key` (it was deleted or overwritten to
    /// indefinite). The stale heap entry is dropped on pop.
    pub fn disarm(&mut self, key: &str) {
        self.latest.remove(key);
    }

    /// Rebuilds the heap from the live `latest` set once it has grown well past the
    /// live key count.
    ///
    /// Each [`arm`](Self::arm) pushes a `Reverse(Armed)` without removing the
    /// superseded entry, and [`drop_stale_front`](Self::drop_stale_front) only
    /// reclaims stale entries at the *front* — so a key re-armed while an
    /// earlier-deadline key stays live leaves stale entries buried in the middle. Left
    /// unchecked the heap grows toward O(writes-in-TTL-window); this compaction bounds
    /// it to O(distinct live keys) amortized without changing any observable ordering.
    fn compact_if_bloated(&mut self) {
        /// Don't churn tiny heaps; only compact once there is real bloat to reclaim.
        const MIN_HEAP: usize = 16;
        if self.heap.len() > MIN_HEAP && self.heap.len() > self.latest.len() * 2 {
            self.heap = self
                .latest
                .iter()
                .map(|(key, at)| {
                    Reverse(Armed {
                        at: *at,
                        key: key.clone(),
                    })
                })
                .collect();
        }
    }

    /// The earliest live deadline, for arming a timer. Skips stale heap entries at
    /// the front without consuming live ones.
    pub fn peek_deadline(&mut self) -> Option<Timestamp> {
        self.drop_stale_front();
        self.heap.peek().map(|Reverse(armed)| armed.at)
    }

    /// Pops the next key whose deadline is at or before `now`, or `None` when none
    /// is due. Stale (superseded) entries are discarded, never returned.
    pub fn pop_due(&mut self, now: Timestamp) -> Option<String> {
        loop {
            self.drop_stale_front();
            let Reverse(front) = self.heap.peek()?;
            if front.at > now {
                return None;
            }
            let Reverse(armed) = self.heap.pop()?;
            // Live iff its deadline is still the latest armed for the key.
            if self.latest.get(&armed.key) == Some(&armed.at) {
                self.latest.remove(&armed.key);
                return Some(armed.key);
            }
            // Otherwise superseded/disarmed — discard and continue.
        }
    }

    /// Discards stale entries at the front of the heap so `peek` reflects a live
    /// deadline.
    fn drop_stale_front(&mut self) {
        while let Some(Reverse(front)) = self.heap.peek() {
            if self.latest.get(&front.key) == Some(&front.at) {
                break;
            }
            self.heap.pop();
        }
    }

    /// The number of live armed keys. Retained for the heap's own unit tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.latest.len()
    }

    /// Whether nothing is armed. Retained for the heap's own unit tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.latest.is_empty()
    }
}

/// The std [`Duration`] until `deadline`, clamped to zero if it is already past.
fn wait_until(deadline: Timestamp) -> Duration {
    let signed = deadline.duration_since(Timestamp::now());
    Duration::from_secs(signed.as_secs().max(0).unsigned_abs())
        + Duration::from_nanos(u64::from(signed.subsec_nanos().max(0).unsigned_abs()))
}

/// The sweeper background task (§6.2): maintain the deadline heap from the watcher's
/// arm/disarm commands and reclaim each entry when its deadline passes.
pub(super) async fn run_sweeper(
    runtime: Arc<CacheRuntime>,
    mut commands: mpsc::UnboundedReceiver<SweepCmd>,
    shutdown: CancellationToken,
) {
    let mut heap = SweepHeap::new();
    loop {
        let wait = heap.peek_deadline().map_or(IDLE_TICK, wait_until);
        tokio::select! {
            () = shutdown.cancelled() => return,
            command = commands.recv() => match command {
                Some(SweepCmd::Arm { key, at }) => heap.arm(&key, at),
                Some(SweepCmd::Disarm { key }) => heap.disarm(&key),
                // The backend was dropped; nothing more will be armed.
                None => return,
            },
            () = tokio::time::sleep(wait) => {
                let now = Timestamp::now();
                while let Some(key) = heap.pop_due(now) {
                    sweep_key(&runtime, &key).await;
                }
            }
        }
    }
}

/// Reclaims one expired key with a guarded delete (§6.2), re-reading first so the
/// delete carries `resourceVersion` + `uid` and cannot land on a revived object. A
/// key re-armed to the future (no longer expired) or already gone is left alone.
async fn sweep_key(runtime: &CacheRuntime, key: &str) {
    let Ok(Some(entry)) = runtime.read_raw(key).await else {
        return; // gone, or a transient read error — the next arm re-drives it
    };
    if !is_expired(entry.spec.expires_at.as_deref(), Timestamp::now()) {
        return; // overwritten to a later deadline between arm and fire
    }
    let api = runtime.api();
    let name = runtime.object_name(key);
    // A 404/409 (already gone, or revived) comes back as Ok(false); a real fault is
    // dropped — the sweeper is reclamation, not correctness (§6.2). Bounded by
    // `request_timeout` so a stalled delete can't hold `K8sCache::stop` open forever.
    let _swept = runtime
        .timed(
            "sweep cache entry",
            crate::guarded::delete(&api, &name, &entry.metadata),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::SweepHeap;
    use k8s_openapi::jiff::Timestamp;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    #[test]
    fn pops_in_deadline_order() {
        let mut heap = SweepHeap::new();
        heap.arm("late", ts(300));
        heap.arm("early", ts(100));
        heap.arm("mid", ts(200));
        assert_eq!(heap.peek_deadline(), Some(ts(100)));
        assert_eq!(heap.pop_due(ts(1_000)), Some("early".to_owned()));
        assert_eq!(heap.pop_due(ts(1_000)), Some("mid".to_owned()));
        assert_eq!(heap.pop_due(ts(1_000)), Some("late".to_owned()));
        assert_eq!(heap.pop_due(ts(1_000)), None);
    }

    #[test]
    fn nothing_is_due_before_its_deadline() {
        let mut heap = SweepHeap::new();
        heap.arm("k", ts(500));
        assert_eq!(heap.pop_due(ts(499)), None, "not yet due");
        assert_eq!(
            heap.pop_due(ts(500)),
            Some("k".to_owned()),
            "due at the deadline"
        );
    }

    #[test]
    fn rearming_supersedes_the_earlier_deadline() {
        let mut heap = SweepHeap::new();
        heap.arm("k", ts(100));
        heap.arm("k", ts(400)); // pushed the deadline out
        // The stale 100 entry must not fire the delete.
        assert_eq!(
            heap.pop_due(ts(200)),
            None,
            "the superseded deadline is dead"
        );
        assert_eq!(heap.peek_deadline(), Some(ts(400)));
        assert_eq!(heap.pop_due(ts(400)), Some("k".to_owned()));
        // And the key is swept exactly once.
        assert_eq!(heap.pop_due(ts(1_000)), None);
        assert!(heap.is_empty());
    }

    #[test]
    fn disarm_cancels_a_pending_sweep() {
        let mut heap = SweepHeap::new();
        heap.arm("k", ts(100));
        heap.disarm("k"); // e.g. the key was deleted or made indefinite
        assert_eq!(heap.pop_due(ts(1_000)), None);
        assert!(heap.is_empty());
    }

    #[test]
    fn len_tracks_distinct_live_keys() {
        let mut heap = SweepHeap::new();
        heap.arm("a", ts(100));
        heap.arm("b", ts(200));
        heap.arm("a", ts(300)); // re-arm same key, not a new one
        assert_eq!(heap.len(), 2);
    }

    #[test]
    fn repeated_rearming_compacts_the_heap_and_preserves_ordering() {
        let mut heap = SweepHeap::new();
        // A live earlier-deadline key that keeps the re-armed key's stale entries
        // buried (so `drop_stale_front` cannot reclaim them), then re-arm one key many
        // times to later, ever-growing deadlines.
        heap.arm("early", ts(1));
        for i in 0..500 {
            heap.arm("hot", ts(1000 + i));
        }
        // Two live keys, so the backing heap must have compacted rather than retained
        // ~500 buried stale entries.
        assert_eq!(heap.len(), 2, "only two distinct live keys");
        assert!(
            heap.heap.len() <= 8,
            "the heap compacts toward the live-key count, got {}",
            heap.heap.len()
        );
        // Ordering is unchanged: the earlier-deadline key still pops first, then the
        // hot key at its *latest* armed deadline, and nothing stale resurfaces.
        assert_eq!(heap.pop_due(ts(10_000)), Some("early".to_owned()));
        assert_eq!(heap.pop_due(ts(10_000)), Some("hot".to_owned()));
        assert_eq!(heap.pop_due(ts(10_000)), None);
    }
}
