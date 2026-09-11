//! Live interest in traces, held in this process.
//!
//! Registering interest costs no statement, and neither does releasing it: the
//! registry is the only thing that decides whether a completion is delivered
//! in-process, and it is also what tells the notifier whether it has any
//! reason to run at all. An instance holding no subscriptions issues no
//! notification query, which is the property that makes the mail poll free
//! when nobody is waiting.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use tokio::sync::{oneshot, watch};

use super::trace::{TraceOutcome, TraceProgress};

/// What an ack needs to deliver a completion in-process: who this instance is,
/// and who is waiting.
#[derive(Debug)]
pub struct Mailbox {
    instance_id: String,
    subscriptions: Arc<Registry>,
}

impl Mailbox {
    #[must_use]
    pub fn new(instance_id: String, subscriptions: Arc<Registry>) -> Self {
        Self {
            instance_id,
            subscriptions,
        }
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[must_use]
    pub fn subscriptions(&self) -> &Arc<Registry> {
        &self.subscriptions
    }
}

/// Interest in one trace.
struct Interest {
    /// Which subscription this entry belongs to.
    ///
    /// The registry is keyed by the trace string and nothing requires a
    /// caller's traces to be unique, so two live subscriptions can share a
    /// key. Without an identity, the second `subscribe` would silently drop
    /// the first's sender and either one's `Drop` would evict the other's
    /// entry - so both callers would be told `None` and the completion would
    /// be claimed and discarded.
    generation: u64,
    /// Taken when the completion is delivered, so a second delivery finds
    /// nothing to send and cannot double-notify.
    outcome: Option<oneshot::Sender<TraceOutcome>>,
    /// The batch's current stall, or `None` while it is moving. A watch rather
    /// than a queue because a subscriber wants the situation now, not every
    /// retry that led to it, and a slow subscriber must not accumulate a
    /// backlog it will never read.
    progress: Arc<watch::Sender<Option<TraceProgress>>>,
}

/// Every trace this process is currently waiting on.
#[derive(Debug, Default)]
pub struct Registry {
    entries: Mutex<HashMap<String, Interest>>,
    /// Woken when the registry goes from empty to occupied.
    ///
    /// Without this the collector has no reason to look: it sleeps on its
    /// notifiers whenever nobody is waiting, and a new subscription is exactly
    /// the event that makes looking worthwhile.
    arrived: Arc<tokio::sync::Notify>,
    generations: std::sync::atomic::AtomicU64,
    /// Set once the outbox stops, so a subscription taken afterwards resolves
    /// rather than waiting for a pipeline that is no longer running.
    closed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Interest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interest")
            .field("generation", &self.generation)
            .field("waiting", &self.outcome.is_some())
            .field("watching_progress", &(self.progress.receiver_count() > 0))
            .finish()
    }
}

impl Registry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            arrived: Arc::new(tokio::sync::Notify::new()),
            generations: std::sync::atomic::AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Register interest in a trace, before the enqueuing transaction commits.
    ///
    /// A completion cannot precede that commit, so there is no race: the
    /// registration is in place before anything could deliver to it.
    pub fn subscribe(self: &Arc<Self>, trace: &str) -> TraceSubscription {
        let (tx, rx) = oneshot::channel();
        // The initial receiver is dropped straight away, so the channel starts
        // with none. `receiver_count() > 0` is then exactly "this caller asked
        // for progress", which is what gates the stall query.
        let (progress_tx, _) = watch::channel(None);
        let progress = Arc::new(progress_tx);
        let generation = self
            .generations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A subscription taken after the outbox stopped keeps no entry, so its
        // sender drops here and awaiting it yields `None` at once rather than
        // waiting on a pipeline that will never run.
        if !self.closed.load(std::sync::atomic::Ordering::Acquire)
            && let Ok(mut entries) = self.entries.lock()
        {
            entries.insert(
                trace.to_owned(),
                Interest {
                    generation,
                    outcome: Some(tx),
                    progress: Arc::clone(&progress),
                },
            );
        }
        // Tell the collector there is now a reason to look.
        self.arrived.notify_one();
        TraceSubscription {
            trace: trace.to_owned(),
            generation,
            registry: Arc::downgrade(self),
            rx: Some(rx),
            progress,
        }
    }

    /// The wakeup to hand the collector, fired when a subscription is taken.
    #[must_use]
    pub fn arrivals(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.arrived)
    }

    /// Whether anything in this process is waiting.
    ///
    /// The notifier's gate: false means there is provably no mail worth
    /// looking for, so no query runs.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.entries.lock().is_ok_and(|entries| entries.is_empty())
    }

    /// How many traces this process is waiting on.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }

    /// Hand a completion to whoever is waiting for it.
    ///
    /// Returns whether anyone was: `false` means the mail has been claimed and
    /// has nowhere to go, which is the case when the guard was dropped or the
    /// process restarted, and the mail is then correctly discarded rather than
    /// retained.
    pub fn deliver(&self, outcome: TraceOutcome) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let Some(interest) = entries.get_mut(&outcome.trace) else {
            return false;
        };
        let Some(tx) = interest.outcome.take() else {
            return false;
        };
        tx.send(outcome).is_ok()
    }

    /// Whether any live subscription is watching for stalls.
    ///
    /// The stall reporter's gate. A caller that only awaits completion never
    /// takes a progress receiver, so an instance full of such callers issues
    /// no stall query at all - the feature costs nothing until it is used.
    #[must_use]
    pub fn wants_progress(&self) -> bool {
        self.entries.lock().is_ok_and(|entries| {
            entries
                .values()
                .any(|interest| interest.progress.receiver_count() > 0)
        })
    }

    /// Report a stall to whoever is watching this trace.
    ///
    /// Returns whether anyone was. Sending the same stall twice is harmless -
    /// the receiver sees a change only when the value differs.
    pub fn publish_progress(&self, progress: TraceProgress) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return false;
        };
        let Some(interest) = entries.get(&progress.trace) else {
            return false;
        };
        if interest.progress.receiver_count() == 0 {
            return false;
        }
        interest.progress.send_if_modified(|current| {
            if current.as_ref() == Some(&progress) {
                false
            } else {
                *current = Some(progress);
                true
            }
        })
    }

    /// Withdraw the stall from every watched trace outside `still_stalled`,
    /// because those batches are moving again.
    pub fn withdraw_progress_except(&self, still_stalled: &HashSet<String>) {
        let Ok(entries) = self.entries.lock() else {
            return;
        };
        for (trace, interest) in entries.iter() {
            if still_stalled.contains(trace) || interest.progress.receiver_count() == 0 {
                continue;
            }
            interest.progress.send_if_modified(|current| {
                if current.is_none() {
                    false
                } else {
                    *current = None;
                    true
                }
            });
        }
    }

    /// Release one subscription's entry, and only its own.
    fn release(&self, trace: &str, generation: u64) {
        if let Ok(mut entries) = self.entries.lock()
            && entries
                .get(trace)
                .is_some_and(|interest| interest.generation == generation)
        {
            entries.remove(trace);
        }
    }

    /// Stop answering, and resolve everyone still waiting.
    ///
    /// Dropping every sender is what makes an awaiting subscription yield
    /// `None` - the documented signal that this process can no longer answer
    /// and the caller should ask [`Outbox::trace_status`](super::Outbox::trace_status)
    /// instead. Without this, a caller waiting when the outbox stops waits for
    /// ever, because it necessarily holds the `Arc` that owns this registry.
    pub fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }
}

/// A view of one batch's stalls while it is in flight.
///
/// A wrapper, not the channel itself: exposing `tokio::sync::watch::Receiver`
/// would make its version part of this crate's public contract, and would hand
/// callers a borrow guard that blocks the sender if it is held across an await.
/// [`TraceProgressWatch::current`] returns by value instead.
pub struct TraceProgressWatch(watch::Receiver<Option<TraceProgress>>);

impl TraceProgressWatch {
    /// Wait until the stall state changes.
    ///
    /// Returns `false` once nothing can change it again - the batch finished,
    /// or the subscription was dropped.
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }

    /// The batch's current stall, or `None` while it is moving.
    ///
    /// Marks the value seen, so a following [`TraceProgressWatch::changed`]
    /// waits for the next change rather than returning this one again.
    #[must_use]
    pub fn current(&mut self) -> Option<TraceProgress> {
        self.0.borrow_and_update().clone()
    }

    /// Whether a change has landed that [`TraceProgressWatch::current`] has
    /// not yet been shown.
    #[must_use]
    pub fn pending_change(&self) -> bool {
        self.0.has_changed().unwrap_or(false)
    }
}

impl std::fmt::Debug for TraceProgressWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceProgressWatch").finish()
    }
}

/// Interest in one trace's completion, and the means of awaiting it.
///
/// Awaiting yields `None` when this process can no longer answer - the outbox
/// was stopped, or this registry outlived the caller. The durable answer is
/// always available from
/// [`Outbox::trace_status`](super::Outbox::trace_status), which reads the trace
/// row rather than this process's memory.
///
/// Dropping the subscription releases it, with no statement issued. Mail that
/// arrives afterwards is marked delivered and discarded.
pub struct TraceSubscription {
    trace: String,
    generation: u64,
    registry: Weak<Registry>,
    /// Taken once it resolves. `oneshot::Receiver` panics if polled after it
    /// is ready, and this future is public, named and `Unpin` - so
    /// `(&mut sub).await` twice would panic rather than answer.
    rx: Option<oneshot::Receiver<TraceOutcome>>,
    progress: Arc<watch::Sender<Option<TraceProgress>>>,
}

impl TraceSubscription {
    /// The trace being awaited.
    #[must_use]
    pub fn trace(&self) -> &str {
        &self.trace
    }

    /// Watch the batch for stalls while waiting for it to finish.
    ///
    /// The receiver holds `None` while the batch is moving and
    /// `Some(`[`TraceProgress`]`)` while a handler is retrying one of its
    /// entities. Taking a receiver is what makes this instance look for
    /// stalls, so a caller that never calls this pays nothing.
    ///
    /// The receiver outlives the subscription; once the subscription is
    /// dropped the value stops changing, and `changed()` resolves with an
    /// error when the last sender goes.
    ///
    /// ```ignore
    /// let waiting = outbox.subscribe("import-1");
    /// let mut stalls = waiting.progress();
    /// tokio::spawn(async move {
    ///     while stalls.changed().await {
    ///         if let Some(p) = stalls.current() {
    ///             tracing::warn!(trace = %p.trace, attempts = p.attempts, "batch is stuck");
    ///         }
    ///     }
    /// });
    /// let outcome = waiting.await;
    /// ```
    #[must_use]
    pub fn progress(&self) -> TraceProgressWatch {
        TraceProgressWatch(self.progress.subscribe())
    }
}

impl Future for TraceSubscription {
    type Output = Option<TraceOutcome>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Already resolved: answer again rather than panicking.
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(rx).poll(cx) {
            Poll::Ready(Ok(outcome)) => {
                self.rx = None;
                Poll::Ready(Some(outcome))
            }
            // The sender went away without delivering: this process cannot
            // answer, and the caller should ask the trace row instead.
            Poll::Ready(Err(_)) => {
                self.rx = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for TraceSubscription {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.release(&self.trace, self.generation);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn outcome(trace: &str) -> TraceOutcome {
        TraceOutcome {
            trace: trace.to_owned(),
            entities: 2,
            failures: 0,
            attempts: 0,
            completed_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_subscription_receives_its_completion() {
        let registry = Registry::new();
        let sub = registry.subscribe("t1");
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_idle());

        assert!(registry.deliver(outcome("t1")));
        let received = sub.await.expect("delivered");
        assert_eq!(received.trace, "t1");
        assert_eq!(received.entities, 2);
        assert!(received.is_clean());
    }

    #[tokio::test]
    async fn a_completion_is_delivered_at_most_once() {
        let registry = Registry::new();
        let sub = registry.subscribe("t1");

        assert!(registry.deliver(outcome("t1")));
        assert!(
            !registry.deliver(outcome("t1")),
            "a second delivery must find nothing to send"
        );
        assert!(sub.await.is_some());
    }

    #[test]
    fn dropping_a_subscription_releases_it() {
        let registry = Registry::new();
        let sub = registry.subscribe("t1");
        assert_eq!(registry.len(), 1);
        drop(sub);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_idle());
    }

    #[test]
    fn mail_for_a_released_subscription_has_nowhere_to_go() {
        let registry = Registry::new();
        drop(registry.subscribe("t1"));
        assert!(
            !registry.deliver(outcome("t1")),
            "the caller stamps and discards it"
        );
    }

    #[test]
    fn mail_for_a_trace_nobody_asked_about_is_not_delivered() {
        let registry = Registry::new();
        assert!(!registry.deliver(outcome("never-subscribed")));
    }

    fn stall(trace: &str, attempts: i64) -> TraceProgress {
        TraceProgress {
            trace: trace.to_owned(),
            entities: 4,
            pending: 3,
            failures: 0,
            attempts,
            last_error: Some("upstream refused".to_owned()),
            stalled_since: chrono::Utc::now(),
        }
    }

    #[test]
    fn awaiting_a_completion_does_not_arm_the_stall_query() {
        let registry = Registry::new();
        let _sub = registry.subscribe("t1");
        assert!(
            !registry.wants_progress(),
            "a caller that only awaits completion pays for no stall query"
        );
    }

    #[test]
    fn taking_a_progress_receiver_arms_the_stall_query() {
        let registry = Registry::new();
        let sub = registry.subscribe("t1");
        let watching = sub.progress();
        assert!(registry.wants_progress());
        drop(watching);
        assert!(
            !registry.wants_progress(),
            "the last watcher going disarms it again"
        );
    }

    #[test]
    fn a_stall_reaches_a_watcher_and_is_withdrawn_when_the_batch_moves() {
        let registry = Registry::new();
        let sub = registry.subscribe("t1");
        let mut watching = sub.progress();
        assert!(watching.current().is_none(), "nothing is wrong yet");

        // The reporter re-reads the same row every tick, so the same stall is
        // republished verbatim; `stalled_since` comes from the row and does
        // not drift.
        let unchanged = stall("t1", 3);
        assert!(registry.publish_progress(unchanged.clone()));
        assert_eq!(watching.current().unwrap().attempts, 3);

        // Republishing it is not a change, so a watcher polling on
        // `changed()` is not woken for it.
        assert!(!registry.publish_progress(unchanged));
        assert!(!watching.pending_change());

        // A worse stall is.
        assert!(registry.publish_progress(stall("t1", 4)));
        assert_eq!(watching.current().unwrap().attempts, 4);

        registry.withdraw_progress_except(&HashSet::new());
        assert!(
            watching.current().is_none(),
            "a batch that moved again is no longer stalled"
        );
    }

    #[test]
    fn a_stall_still_listed_is_not_withdrawn() {
        let registry = Registry::new();
        let sub = registry.subscribe("t1");
        let mut watching = sub.progress();
        registry.publish_progress(stall("t1", 3));

        let still = HashSet::from(["t1".to_owned()]);
        registry.withdraw_progress_except(&still);
        assert!(watching.current().is_some());
    }

    #[test]
    fn a_stall_for_a_trace_nobody_watches_goes_nowhere() {
        let registry = Registry::new();
        let _sub = registry.subscribe("t1");
        assert!(
            !registry.publish_progress(stall("t1", 3)),
            "subscribed, but not watching progress"
        );
        assert!(!registry.publish_progress(stall("never-subscribed", 3)));
    }

    #[tokio::test]
    async fn two_subscriptions_on_one_trace_do_not_destroy_each_other() {
        // `trace_status` documents duplicate traces as legal, and the registry
        // is keyed by the trace string, so both must survive independently.
        let registry = Registry::new();
        let first = registry.subscribe("t1");
        let second = registry.subscribe("t1");

        // The newer entry is the live one, and dropping the OLDER must not
        // evict it.
        drop(first);
        assert_eq!(
            registry.len(),
            1,
            "the newer subscription is still registered"
        );
        assert!(
            registry.deliver(outcome("t1")),
            "and its completion still has somewhere to go"
        );
        assert!(second.await.is_some());
    }

    #[test]
    fn dropping_the_newer_subscription_leaves_no_stale_entry() {
        let registry = Registry::new();
        let first = registry.subscribe("t1");
        let second = registry.subscribe("t1");
        drop(second);
        assert_eq!(
            registry.len(),
            0,
            "the entry it owned is gone, and it did not resurrect the older one"
        );
        drop(first);
        assert_eq!(registry.len(), 0);
    }

    #[tokio::test]
    async fn closing_resolves_everyone_still_waiting() {
        // What `stop()` relies on: dropping the senders is the documented
        // signal that this process can no longer answer.
        let registry = Registry::new();
        let waiting = registry.subscribe("t1");
        registry.close();
        assert!(
            waiting.await.is_none(),
            "a caller waiting when the outbox stops is told so, rather than waiting for ever"
        );
    }

    #[tokio::test]
    async fn subscribing_after_close_resolves_at_once() {
        let registry = Registry::new();
        registry.close();
        assert!(
            registry.subscribe("t1").await.is_none(),
            "there is no pipeline left to answer, so do not pretend to wait for one"
        );
        assert!(registry.is_idle(), "and nothing is left registered");
    }

    #[tokio::test]
    async fn awaiting_yields_none_once_the_registry_is_gone() {
        let registry = Registry::new();
        let sub = registry.subscribe("t1");
        drop(registry);
        assert!(
            sub.await.is_none(),
            "this process can no longer answer; the trace row still can"
        );
    }
}
