//! The CAS-based default leader-election backend over `Arc<dyn ClusterCacheBackend>`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rand::RngExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::defaults::lease::{Acquisition, CacheLeaseStore};
use crate::defaults::{ELECTION_KEY_PREFIX, ShutdownRevoke, guard, identity};
use cluster_sdk::cache::{CacheWatch, CacheWatchEvent, ClusterCacheBackend};
use cluster_sdk::error::ClusterError;
use cluster_sdk::leader::{
    ElectionConfig, LeaderElectionBackend, LeaderElectionFeatures, LeaderStatus, LeaderWatch,
    LeaderWatchEvent, LeaderWatchSender, ResignReceiver, ResignResponder,
};
use cluster_sdk::lease::{LeaseRecord, LeaseToken};
use cluster_sdk::observability::{self, ClusterMetrics, NoopMetrics, logs, spans, transition};

/// The in-flight event buffer for each [`LeaderWatch`].
const EVENT_BUFFER: usize = 16;

/// How many times a retryable cache-watch close is re-subscribed before the task
/// gives up on the feed and renews off the timer alone (B5).
///
/// Bounded on purpose: a watch that closes retryably forever must not spin
/// re-subscribing without limit, but neither may it release a live claim — so
/// once the budget is spent the task stops tracking via the watch and keeps
/// renewing, exactly as it does after an ordinary end-of-stream
/// (`None => cache_watch = None`). The budget is reset whenever the watch
/// delivers a real event, so an intermittently-flapping-but-working feed is not
/// capped by it. Only a **non-retryable** close ends the loop.
const MAX_WATCH_RESUBSCRIBES: u32 = 8;

/// A leader-election backend that derives single-leader behavior from cache
/// compare-and-swap operations (DESIGN §3.11, ADR-001).
///
/// # A leader claim is a store-owned lease (§5.8.1)
///
/// Candidacy takes the same [`LeaseRecord`] a lock does, under `election/{name}`:
/// insert it if the name is free, steal it at `fence + 1` if the incumbent's
/// `deadline` has passed. The claim is then held by renewing against the
/// [`LeaseToken`] on [`ElectionConfig::renewal_interval`], and a renewal that
/// matches no record surfaces as [`LeaderStatus::Lost`] followed by
/// auto-reenrollment. Because the claim is a record rather than a process's
/// promise, **a leader survives the replica it was elected through** — which is
/// the point of the model (invariant I7).
///
/// Renewal stays client-driven, so it keeps doubling as the consumer-liveness
/// proxy: a wedged holder stops renewing and loses the claim (invariant I8, §7.3).
///
/// A `watch` on the election key reconciles status reactively: it issues no
/// renewal write (only the renewal timer does), though it may opportunistically
/// re-`claim` a vacant key. A renewal's own change event therefore reconciles to a
/// no-op status check, so it cannot re-trigger a renewal.
///
/// A lease *lapsing*, unlike a physical TTL expiry, writes nothing and so raises
/// no watch event. A follower therefore times its next tick to the incumbent's
/// `deadline` rather than relying on the watch to announce the vacancy.
///
/// # Consistency safety (ADR-009)
///
/// The at-most-one-leader guarantee holds only over a **linearizable** cache.
/// Construct with [`new`](Self::new) (default-safe, rejects an
/// eventually-consistent cache) or, to intentionally accept the split-brain
/// risk, [`new_allow_weak_consistency`](Self::new_allow_weak_consistency).
/// [`features`](LeaderElectionBackend::features) derives `linearizable` from the
/// underlying cache's consistency.
pub struct CasBasedLeaderElectionBackend {
    leases: Arc<CacheLeaseStore>,
    /// Cancelled by [`ShutdownRevoke::revoke`] to signal every in-flight
    /// election task to surface a terminal shutdown (DESIGN §3.13).
    shutdown: CancellationToken,
    /// Handles of the spawned election tasks, so `revoke` can await their
    /// shutdown emit. Finished handles are pruned as new elections start.
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// The bounded `provider` label for emitted signals (default `"unknown"`
    /// until set via [`with_observability`](Self::with_observability)).
    provider: &'static str,
    /// The metrics sink (default [`NoopMetrics`]).
    metrics: Arc<dyn ClusterMetrics>,
}

impl CasBasedLeaderElectionBackend {
    const NAME: &'static str = "CasBasedLeaderElectionBackend";

    /// Creates a default-safe backend over `cache`.
    ///
    /// # Errors
    /// Returns [`ClusterError::InvalidConfig`] when `cache` declares
    /// [`CacheConsistency::EventuallyConsistent`](cluster_sdk::cache::CacheConsistency),
    /// because the at-most-one-leader guarantee requires linearizable CAS.
    pub fn new(cache: Arc<dyn ClusterCacheBackend>) -> Result<Self, ClusterError> {
        guard::reject_weak_consistency(cache.consistency(), Self::NAME)?;
        Ok(Self::with_cache(cache))
    }

    /// Creates a backend over `cache`, bypassing the consistency guard.
    ///
    /// Always succeeds and emits a `tracing::warn!` acknowledging the
    /// split-brain risk. Use only when the cache is intentionally
    /// eventually consistent and the consumer accepts that two leaders may be
    /// elected under partition (ADR-009).
    #[must_use]
    pub fn new_allow_weak_consistency(cache: Arc<dyn ClusterCacheBackend>) -> Self {
        guard::warn_weak_consistency(cache.consistency(), Self::NAME);
        Self::with_cache(cache)
    }

    fn with_cache(cache: Arc<dyn ClusterCacheBackend>) -> Self {
        // Tell the operator once, at construction, if this cache cannot serve
        // exact watch: the election then reconciles off the renewal timer alone
        // (see `enrol`). Not fatal — the timer path is correct on its own.
        guard::warn_without_watch(cache.features(), Self::NAME);
        Self {
            leases: Arc::new(CacheLeaseStore::new(cache)),
            shutdown: CancellationToken::new(),
            tasks: Arc::new(Mutex::new(Vec::new())),
            provider: "unknown",
            metrics: Arc::new(NoopMetrics),
        }
    }

    /// Sets how long a lease record outlives the claim it fenced (§5.8.1).
    ///
    /// See
    /// [`CasBasedDistributedLockBackend::with_fence_retention`](crate::defaults::CasBasedDistributedLockBackend::with_fence_retention)
    /// for why this is a builder method rather than a third constructor. An
    /// election claim and a lock are the same lease, so the same window governs
    /// both and the wiring sets them from one key.
    #[must_use]
    pub fn with_fence_retention(mut self, retention: Duration) -> Self {
        self.leases = Arc::new(CacheLeaseStore::with_retention(
            Arc::clone(self.leases.cache()),
            retention,
        ));
        self
    }

    /// **Test-only.** Rebuilds the lease store on a virtual clock that tracks
    /// [`tokio::time::pause`]/`advance`, so a test (including the gear's
    /// `TimeControl::Virtual` conformance run) can lapse a claim's TTL under
    /// virtual time (§5.8.1).
    ///
    /// Production must never call this: [`new`](Self::new) and
    /// [`new_allow_weak_consistency`](Self::new_allow_weak_consistency) build the
    /// pure wall clock, which cannot diverge across a wall step. Named and
    /// documented as test-support, in the same spirit as
    /// [`reserved_lease_cache`](cluster_sdk::reserved_lease_cache) — a `pub` seam
    /// the conformance harness reaches from outside the crate's unit-test cfg.
    #[must_use]
    pub fn with_virtual_clock(mut self) -> Self {
        use rand::RngExt as _;
        self.leases = Arc::new(CacheLeaseStore::with_virtual_clock(
            Arc::clone(self.leases.cache()),
            self.leases.retention(),
            rand::rng().random::<u64>(),
        ));
        self
    }

    /// Sets the `provider` label and metrics sink the backend emits through.
    ///
    /// Called by the wrapping plugin so emitted signals carry the deployment's
    /// provider name (ADR-004). Without it, signals use `provider = "unknown"`
    /// and a no-op sink.
    #[must_use]
    pub fn with_observability(
        mut self,
        provider: &'static str,
        metrics: Arc<dyn ClusterMetrics>,
    ) -> Self {
        self.provider = provider;
        self.metrics = metrics;
        self
    }

    /// Records a spawned election task's handle, pruning any that have already
    /// finished so the set stays bounded across many short-lived elections.
    fn track(&self, handle: JoinHandle<()>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|handle| !handle.is_finished());
        tasks.push(handle);
    }

    /// The cache key a named election claims. Prefixed so an election does not
    /// collide with a same-named lock when both defaults share one cache.
    fn election_key(name: &str) -> String {
        format!("{ELECTION_KEY_PREFIX}{name}")
    }

    /// Enrols in `name` and returns the consumer's [`LeaderWatch`], spawning the
    /// task that holds the claim.
    ///
    /// Named `enrol` rather than `join` so it does not shadow
    /// [`LeaderElectionBackend::join`], which is the lease-token half of the same
    /// operation and returns the token instead of a watch.
    async fn enrol(&self, name: &str, config: ElectionConfig) -> Result<LeaderWatch, ClusterError> {
        let span =
            tracing::info_span!(spans::LEADER_ELECT, provider = %self.provider, election = %name);
        let out = async {
            // Refuse to enrol a new election once graceful shutdown has begun:
            // `revoke()` drains and awaits the tracked tasks, and a task spawned
            // after that drain would escape the revocation-completion guarantee.
            if self.shutdown.is_cancelled() {
                return Err(ClusterError::Shutdown);
            }
            let key = Self::election_key(name);
            let owner = identity::fresh_id();
            // Subscribe before the first claim so a transition between the claim and
            // the watch establishment cannot be missed. A backend with no
            // exact-watch support (`features().watch == false`) answers
            // `Unsupported`; there is then no reactive feed, so reconcile off the
            // renewal timer alone — the same state the task reaches after an
            // ordinary end-of-stream (`None`). This keeps election working over
            // e.g. redis `watch_mode: disabled` instead of failing at first
            // `enrol` (plan D3).
            let cache_watch = match self.leases.cache().watch(&key).await {
                Ok(watch) => Some(watch),
                Err(ClusterError::Unsupported { .. }) => None,
                Err(err) => return Err(err),
            };
            let (token, initial, incumbent_lapse) = match self
                .leases
                .try_acquire(&key, name, &owner, config.ttl())
                .await?
            {
                // Arm the deadline authority: leadership is valid only to
                // `now + ttl`, whatever the renewal path does after (§7.3, I8) —
                // the same model Profile 3's remote pump uses, for I1 parity.
                Acquisition::Acquired(token) => (
                    Some(token.with_deadline(tokio::time::Instant::now() + config.ttl())),
                    LeaderStatus::Leader,
                    None,
                ),
                Acquisition::Contended { lapse_in } => (None, LeaderStatus::Follower, lapse_in),
            };
            let (sender, resign_rx, mut watch) =
                LeaderWatch::channel(EVENT_BUFFER, LeaderStatus::Follower);
            // Stamp the watch so an `auto_restart`ed consumer emits the watch-reset
            // signals (`cluster_watch_resets_total` / `cluster.watch.reset`).
            watch.set_observability(self.provider, Arc::clone(&self.metrics));
            let task = ElectionTask {
                leases: Arc::clone(&self.leases),
                name: name.to_owned(),
                key,
                owner,
                token,
                incumbent_lapse,
                config,
                sender,
                am_leader: matches!(initial, LeaderStatus::Leader),
                missed: 0,
                watch_resubscribes: 0,
                shutdown: self.shutdown.clone(),
                provider: self.provider,
                metrics: Arc::clone(&self.metrics),
            };
            self.track(tokio::spawn(task.run(initial, cache_watch, resign_rx)));
            Ok(watch)
        }
        .instrument(span)
        .await;
        if let Err(err) = &out {
            observability::emit_provider_error(
                &*self.metrics,
                self.provider,
                "elect",
                observability::ResourceId::Election(name),
                err,
            );
        }
        out
    }
}

#[async_trait]
impl ShutdownRevoke for CasBasedLeaderElectionBackend {
    /// Revokes leadership confidence on graceful shutdown
    /// (`cpt-cf-clst-fr-shutdown-revoke`): cancels the shared token — so every
    /// in-flight election task latches `Status(Lost)` then `Closed(Shutdown)` —
    /// and awaits those tasks, so a current leader has observed loss before this
    /// returns. No resign is issued: the claim is a record that outlives this
    /// process and lapses at its own deadline
    /// (`cpt-cf-clst-fr-shutdown-ttl-cleanup`, §5.8.2).
    async fn revoke(&self) {
        self.shutdown.cancel();
        let handles = {
            let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *tasks)
        };
        for handle in handles {
            let _joined = handle.await;
        }
    }
}

#[async_trait]
impl LeaderElectionBackend for CasBasedLeaderElectionBackend {
    fn features(&self) -> LeaderElectionFeatures {
        LeaderElectionFeatures::new(
            self.leases.cache().consistency() == cluster_sdk::cache::CacheConsistency::Linearizable,
        )
    }

    async fn elect(&self, name: &str) -> Result<LeaderWatch, ClusterError> {
        self.enrol(name, ElectionConfig::default()).await
    }

    async fn elect_with_config(
        &self,
        name: &str,
        config: ElectionConfig,
    ) -> Result<LeaderWatch, ClusterError> {
        self.enrol(name, config).await
    }

    async fn join(
        &self,
        name: &str,
        owner: &str,
        config: ElectionConfig,
    ) -> Result<Option<LeaseToken>, ClusterError> {
        let span =
            tracing::info_span!(spans::LEADER_ELECT, provider = %self.provider, election = %name);
        let out = async {
            let key = Self::election_key(name);
            match self
                .leases
                .try_acquire(&key, name, owner, config.ttl())
                .await?
            {
                Acquisition::Acquired(token) => Ok(Some(token)),
                // Losing an election is an ordinary outcome, not an error.
                Acquisition::Contended { .. } => Ok(None),
            }
        }
        .instrument(span)
        .await;
        if let Err(err) = &out {
            observability::emit_provider_error(
                &*self.metrics,
                self.provider,
                "elect",
                observability::ResourceId::Election(name),
                err,
            );
        }
        out
    }

    async fn renew(&self, token: &LeaseToken, ttl: Duration) -> Result<(), ClusterError> {
        let span = tracing::info_span!(
            spans::LEADER_RENEW,
            provider = %self.provider,
            election = %token.name
        );
        let key = Self::election_key(&token.name);
        let out = self.leases.renew(&key, token, ttl).instrument(span).await;
        if let Err(err) = &out {
            observability::emit_provider_error(
                &*self.metrics,
                self.provider,
                "renew",
                observability::ResourceId::Election(&token.name),
                err,
            );
        }
        out
    }

    async fn resign(&self, token: &LeaseToken) -> Result<(), ClusterError> {
        let span = tracing::info_span!(
            spans::LEADER_RESIGN,
            provider = %self.provider,
            election = %token.name
        );
        let key = Self::election_key(&token.name);
        let out = self.leases.release(&key, token).instrument(span).await;
        if let Err(err) = &out {
            observability::emit_provider_error(
                &*self.metrics,
                self.provider,
                "resign",
                observability::ResourceId::Election(&token.name),
                err,
            );
        }
        out
    }
}

/// The background task that owns the renewal loop and self-terminates on
/// channel closure (the consumer dropping its [`LeaderWatch`]).
struct ElectionTask {
    leases: Arc<CacheLeaseStore>,
    /// The election name (the span/log `election` attribute), distinct from the
    /// prefixed cache [`key`](Self::key).
    name: String,
    key: String,
    /// This candidate's identity, matched against the record's `owner`.
    owner: String,
    /// The authority over this candidate's claim — `Some` only while it believes
    /// it holds one. Replaced on every (re-)acquisition, since a steal carries a
    /// new `fence`.
    token: Option<LeaseToken>,
    /// How long until the *incumbent's* claim lapses, when a lost claim attempt
    /// observed it. Times a follower's next tick to the moment the claim becomes
    /// takeable, which nothing else would announce (see the type docs).
    incumbent_lapse: Option<Duration>,
    config: ElectionConfig,
    sender: LeaderWatchSender,
    am_leader: bool,
    missed: u8,
    /// Consecutive retryable cache-watch closes re-subscribed without an
    /// intervening real event (B5), capped at [`MAX_WATCH_RESUBSCRIBES`].
    watch_resubscribes: u32,
    /// Cancelled by [`ShutdownRevoke::revoke`] on graceful cluster shutdown.
    shutdown: CancellationToken,
    /// The bounded `provider` label for emitted signals.
    provider: &'static str,
    /// The metrics sink.
    metrics: Arc<dyn ClusterMetrics>,
}

impl ElectionTask {
    async fn run(
        mut self,
        initial: LeaderStatus,
        mut cache_watch: Option<CacheWatch>,
        mut resign_rx: ResignReceiver,
    ) {
        // Emit the resolved initial status. If the consumer is already gone,
        // best-effort release and stop.
        if !self.emit_initial(initial) {
            let _release = self.release_if_holder().await;
            return;
        }
        let interval = self.config.renewal_interval();
        // A recomputed absolute deadline rather than a fixed-period `interval`, so
        // a follower's reclaim tick can carry per-tick jitter (a leader's renewal
        // stays on the exact cadence — see `next_renewal_delay`).
        let mut next_tick = tokio::time::Instant::now() + self.next_renewal_delay(interval);
        // Cloned to a local so the `cancelled()` future does not borrow `self`,
        // which the other arms' bodies mutate.
        let shutdown = self.shutdown.clone();
        loop {
            let tick = tokio::time::sleep_until(next_tick);
            tokio::pin!(tick);
            tokio::select! {
                // Graceful cluster shutdown: revoke leadership confidence and end
                // the watch terminally, without resigning. Leaving the claim in the
                // store is the point of the lease model - the record survives this
                // process and lapses only at its own deadline (invariant I7), so a
                // restart is not a leadership event. A current leader observes
                // `Status(Lost)` first.
                () = shutdown.cancelled() => {
                    self.sender.revoke_for_shutdown(self.am_leader);
                    return;
                }
                () = &mut tick => {
                    // Pay any owed `Lagged` proactively (M13, invariant I1): a
                    // stable leader's renewal emits no event, so `offer` — which
                    // only pays the debt on the next event — would otherwise never
                    // announce a drop to a `changed()`-only consumer once the
                    // election falls quiet. That silent staleness is what ADR-003
                    // exists to eliminate for a leadership gate.
                    self.sender.flush_lagged();
                    if !self.renew_tick().await {
                        break;
                    }
                    next_tick = tokio::time::Instant::now() + self.next_renewal_delay(interval);
                }
                event = recv_optional(&mut cache_watch) => {
                    match event {
                        Some(ev) => match self.on_watch_event(ev).await {
                            WatchOutcome::Continue => {}
                            // A retryable close is a subscription event, not a
                            // leadership one (B5, ADR-003): stop tracking via the
                            // dead watch and re-`watch`, keeping the claim renewing
                            // off the timer. `None` back means the budget is spent
                            // or the re-subscribe failed, which is handled exactly
                            // like an ordinary end-of-stream below.
                            WatchOutcome::Resubscribe => {
                                cache_watch = self.resubscribe_watch().await;
                            }
                            WatchOutcome::Stop => break,
                        },
                        // The cache watch ended; keep tracking via the renewal
                        // timer alone.
                        None => cache_watch = None,
                    }
                }
                resign = resign_rx.recv() => {
                    match resign {
                        Some(responder) => {
                            self.handle_resign(responder).await;
                            return;
                        }
                        // Consumer dropped the watch without resigning.
                        None => break,
                    }
                }
            }
        }
        // Teardown (consumer gone / cache watch closed / fatal): best-effort
        // resign so a successor is elected promptly; the claim otherwise lapses at
        // its stored deadline.
        let _release = self.release_if_holder().await;
    }

    /// Releases the claim on an explicit consumer resign and reports the outcome
    /// to the resigner. A `resigned` transition is recorded only when this
    /// participant was the leader. Spans the release as `cluster.leader.resign`.
    async fn handle_resign(&mut self, responder: ResignResponder) {
        let was_leader = self.am_leader;
        let result = self
            .release_if_holder()
            .instrument(tracing::info_span!(
                spans::LEADER_RESIGN,
                provider = %self.provider,
                election = %self.name
            ))
            .await;
        if was_leader {
            self.record_transition(transition::RESIGNED);
        }
        responder.respond(result);
    }

    /// Emits the leadership-transition signals: the
    /// `cluster_leader_transitions_total` metric and the
    /// `cluster.leader.transition` INFO log, labelled by the bounded
    /// [`transition`](crate::observability::transition) kind.
    fn record_transition(&self, transition: &'static str) {
        self.metrics.leader_transition(transition);
        tracing::event!(
            name: logs::LEADER_TRANSITION,
            tracing::Level::INFO,
            provider = %self.provider,
            election = %self.name,
            transition,
            "cluster leadership transition"
        );
    }

    /// The wait before the next renewal/reclaim tick.
    ///
    /// A leader renews on the exact `interval`, kept comfortably inside the TTL.
    /// A follower adds up to half an interval of random jitter so that when many
    /// participants reclaim on the same cadence (cluster startup, or all
    /// followers after a leader drops) their `put_if_absent` attempts spread
    /// across the window instead of stampeding the election key on the same tick
    /// (cf. the k8s elector's equal-jitter backoff). The `put_if_absent` is
    /// atomic regardless, so this only relieves contention.
    fn next_renewal_delay(&self, interval: Duration) -> Duration {
        if self.am_leader {
            return interval;
        }
        // A follower also wakes when the incumbent's claim lapses. That is not an
        // optimisation: a lease lapsing writes nothing to the store, so no watch
        // event announces it, and without this the vacancy would go unnoticed until
        // the next ordinary tick. Jitter stays proportional to whichever bound won,
        // so a short remaining lease is not swamped by a full interval of jitter.
        let base = self
            .incumbent_lapse
            .map_or(interval, |lapse| interval.min(lapse));
        base + reclaim_jitter(base / 2)
    }

    /// Attempts to take the election's lease, recording the token when it lands and
    /// the incumbent's remaining lifetime when it does not.
    async fn take_lease(&mut self) -> Result<bool, ClusterError> {
        match self
            .leases
            .try_acquire(&self.key, &self.name, &self.owner, self.config.ttl())
            .await?
        {
            Acquisition::Acquired(token) => {
                self.token = Some(self.armed(token));
                self.incumbent_lapse = None;
                Ok(true)
            }
            Acquisition::Contended { lapse_in } => {
                self.incumbent_lapse = lapse_in;
                Ok(false)
            }
        }
    }

    /// `true` when `record` is this candidate's own live claim.
    fn holds(&self, record: &LeaseRecord) -> bool {
        self.token
            .as_ref()
            .is_some_and(|token| record.matches(token))
            && self.leases.is_live(record)
    }

    /// Renews the claim on the timer tick — the operation that *holds* leadership
    /// (§7.3). Only the timer renews — watch events never write — so a renewal's
    /// own change event cannot re-trigger one. Spanned as `cluster.leader.renew`.
    async fn renew_tick(&mut self) -> bool {
        let span = tracing::info_span!(spans::LEADER_RENEW, provider = %self.provider, election = %self.name);
        async {
            let Some(token) = self.token.clone().filter(|_| self.am_leader) else {
                // Not the leader: opportunistically (re)claim in case a vacancy
                // event was missed (e.g. after `Lagged`).
                return self.claim().await;
            };
            // The deadline is the authority (B6, invariant I8): if the last-renewed
            // lease has lapsed, leadership is gone regardless of the missed-renewal
            // budget. The same top-of-tick check the remote pump uses, for I1
            // parity — in-process this is only reached under renew latency, since a
            // local renew re-arms the deadline every interval.
            if token.is_expired(tokio::time::Instant::now()) {
                return self.lose_then_reclaim().await;
            }
            match self
                .leases
                .renew(&self.key, &token, self.config.ttl())
                .await
            {
                Ok(()) => {
                    self.missed = 0;
                    // This renewal moved the claim's TTL forward; re-arm the
                    // deadline authority (B6).
                    self.arm_deadline();
                    true
                }
                // The predicate matched nothing: lapsed, or stolen by a successor
                // that fenced this claim out. Both mean leadership is gone.
                Err(ClusterError::LockExpired { .. }) => self.lose_then_reclaim().await,
                Err(err) if err.is_retryable() => self.on_transient().await,
                Err(err) => self.close(err),
            }
        }
        .instrument(span)
        .await
    }

    /// Emits the resolved initial status to the consumer, recording an
    /// `acquired` transition when the initial claim won leadership. Returns
    /// `false` if the consumer is already gone (the caller releases and stops).
    fn emit_initial(&mut self, initial: LeaderStatus) -> bool {
        if !self.sender.try_send_status(initial) {
            return false;
        }
        if matches!(initial, LeaderStatus::Leader) {
            // The initial claim won leadership outright (e.g. sole candidate).
            self.record_transition(transition::ACQUIRED);
        }
        true
    }

    /// Reconciles stored state into a status transition (the reactive path for
    /// watch events). Issues no renewal, but may take a vacant or lapsed claim.
    async fn reconcile(&mut self) -> bool {
        let record = match self.leases.read(&self.key).await {
            Ok(record) => record,
            // Transient read failures are retried by the renewal timer.
            Err(err) if err.is_retryable() => return true,
            Err(err) => return self.close(err),
        };
        match record {
            Some(record) if self.holds(&record) => self.ensure_leader(),
            // Someone else's live claim.
            Some(record) if self.leases.is_live(&record) => {
                self.incumbent_lapse = self.leases.lapse_in(&record);
                if self.am_leader {
                    self.transition_lost_then(LeaderStatus::Follower)
                } else {
                    true
                }
            }
            // Vacant, lapsed, or unreadable — in every case there is no live claim
            // this candidate is bound by, so try to take it.
            _ if self.am_leader => {
                // Our own claim is gone (lapsed, or stolen) while we still believed
                // we held it. Surface the loss before reclaiming, mirroring the
                // renewal timer: letting `claim()`'s re-win flow through
                // `ensure_leader()` would hit its `am_leader` short-circuit and
                // silently swallow the lost-then-reacquired transition.
                self.lose_then_reclaim().await
            }
            _ => self.claim().await,
        }
    }

    /// Attempts to take a vacant or lapsed claim, resolving to leader or follower.
    async fn claim(&mut self) -> bool {
        match self.take_lease().await {
            Ok(true) => self.ensure_leader(),
            Ok(false) => {
                if self.am_leader {
                    self.transition_lost_then(LeaderStatus::Follower)
                } else {
                    true
                }
            }
            Err(err) if err.is_retryable() => true,
            Err(err) => self.close(err),
        }
    }

    /// Marks this participant leader, emitting `Status(Leader)` on a transition.
    fn ensure_leader(&mut self) -> bool {
        self.missed = 0;
        if self.am_leader {
            return true;
        }
        self.am_leader = true;
        self.record_transition(transition::ACQUIRED);
        self.sender.try_send_status(LeaderStatus::Leader)
    }

    /// Emits the transient `Status(Lost)` then the resolved `next` status.
    fn transition_lost_then(&mut self, next: LeaderStatus) -> bool {
        self.am_leader = matches!(next, LeaderStatus::Leader);
        self.missed = 0;
        self.record_transition(transition::LOST);
        if !self.sender.try_send_status(LeaderStatus::Lost) {
            return false;
        }
        self.sender.try_send_status(next)
    }

    /// Emits `Status(Lost)` then auto-reenrolls, resolving to leader or follower.
    async fn lose_then_reclaim(&mut self) -> bool {
        self.am_leader = false;
        self.missed = 0;
        // The claim this token was authority over is gone. Dropping it here keeps
        // `holds()` from matching a record a successor may yet write.
        self.token = None;
        self.record_transition(transition::LOST);
        if !self.sender.try_send_status(LeaderStatus::Lost) {
            return false;
        }
        match self.take_lease().await {
            Ok(true) => {
                self.am_leader = true;
                self.record_transition(transition::ACQUIRED);
                self.sender.try_send_status(LeaderStatus::Leader)
            }
            Ok(false) => self.sender.try_send_status(LeaderStatus::Follower),
            // A transient failure to reclaim resolves to follower for now; the
            // renewal timer retries the claim on the next tick.
            Err(err) if err.is_retryable() => self.sender.try_send_status(LeaderStatus::Follower),
            Err(err) => self.close(err),
        }
    }

    /// Records a missed renewal; once the budget is exceeded, treats the claim
    /// as lost and auto-reenrolls.
    async fn on_transient(&mut self) -> bool {
        if !self.am_leader {
            return true;
        }
        self.missed = self.missed.saturating_add(1);
        // The secondary bound; the deadline authority in `renew_tick` is the
        // primary one (B6). Once the missed-renewal budget is spent the claim is
        // treated as lost and the task auto-reenrolls.
        if self.missed > self.config.max_missed_renewals() {
            self.lose_then_reclaim().await
        } else {
            true
        }
    }

    /// Stamps `token` with a deadline of `now + ttl` — the claim-validity bound
    /// the renewal loop enforces (B6).
    fn armed(&self, token: LeaseToken) -> LeaseToken {
        token.with_deadline(tokio::time::Instant::now() + self.config.ttl())
    }

    /// Re-arms the held claim's deadline to `now + ttl`.
    fn arm_deadline(&mut self) {
        let deadline = tokio::time::Instant::now() + self.config.ttl();
        if let Some(token) = self.token.as_mut() {
            token.deadline = Some(deadline);
        }
    }

    async fn on_watch_event(&mut self, event: CacheWatchEvent) -> WatchOutcome {
        match event {
            CacheWatchEvent::Event(_) => {
                self.watch_resubscribes = 0;
                self.reconcile().await.into()
            }
            CacheWatchEvent::Lagged { dropped } => {
                self.watch_resubscribes = 0;
                if !self.sender.try_send(LeaderWatchEvent::Lagged { dropped }) {
                    return WatchOutcome::Stop;
                }
                self.reconcile().await.into()
            }
            CacheWatchEvent::Reset => {
                self.watch_resubscribes = 0;
                if !self.sender.try_send(LeaderWatchEvent::Reset) {
                    return WatchOutcome::Stop;
                }
                self.reconcile().await.into()
            }
            // A **retryable** close — the Postgres LISTEN reconnect budget
            // exhausting broadcasts exactly this on an ordinary failover — is a
            // subscription event, not a leadership one (B5, §6.6). Re-subscribe
            // and keep the claim; releasing it here would drop a live claim on a
            // failover, which is the Profile 1/Profile 3 divergence invariant I1
            // forbids. Retryability is reused from
            // [`ClusterError::is_retryable`], never re-classified (see H2).
            CacheWatchEvent::Closed(err) if err.is_retryable() => WatchOutcome::Resubscribe,
            CacheWatchEvent::Closed(err) => {
                // A non-retryable close is terminal. Through the reserved terminal
                // headroom, not the ordinary path: a `Closed` dropped for want of
                // room reaches the consumer as `changed()`'s synthesized
                // `Closed(Shutdown)` instead, which reports a provider failure as
                // an orderly one.
                self.sender.try_close(err);
                WatchOutcome::Stop
            }
            _ => WatchOutcome::Continue,
        }
    }

    /// Re-subscribes the cache watch after a retryable close, keeping the claim
    /// (B5).
    ///
    /// Returns the fresh watch, or `None` when the budget is spent or the
    /// re-subscribe itself fails — in which case the task keeps renewing off the
    /// timer alone, exactly as it does after an ordinary end-of-stream. It never
    /// releases the claim: a live claim is a record in the store that only the
    /// renewal timer sustains (invariants I7, I8).
    async fn resubscribe_watch(&mut self) -> Option<CacheWatch> {
        if self.watch_resubscribes >= MAX_WATCH_RESUBSCRIBES {
            tracing::warn!(
                election = %self.name,
                resubscribes = self.watch_resubscribes,
                "cluster: giving up on the election cache watch after too many retryable \
                 closes; the claim is still being renewed off the timer"
            );
            return None;
        }
        self.watch_resubscribes = self.watch_resubscribes.saturating_add(1);
        // A backend that answers `Unsupported` here (it no longer serves exact
        // watch) is caught by this same arm: it logs and returns `None`, which is
        // terminal — the task renews off the timer alone and never re-subscribes
        // again, so giving up is automatic and permanent (plan D3, step 7).
        match self.leases.cache().watch(&self.key).await {
            Ok(watch) => Some(watch),
            Err(err) => {
                tracing::warn!(
                    election = %self.name,
                    %err,
                    "cluster: re-subscribing the election cache watch failed; renewing off the \
                     timer alone"
                );
                None
            }
        }
    }

    /// Emits a terminal `Closed(err)` and signals the loop to stop. A genuine
    /// backend error also raises the shared provider-error signals.
    fn close(&mut self, err: ClusterError) -> bool {
        observability::emit_provider_error(
            &*self.metrics,
            self.provider,
            "leader",
            observability::ResourceId::Election(&self.name),
            &err,
        );
        self.sender.try_close(err);
        false
    }

    /// Gives up this candidate's claim, if it holds one.
    ///
    /// A conditional delete predicated on the token, so a successor that took the
    /// claim after this one lapsed is never resigned on its behalf — and, unlike
    /// the value-guarded delete this replaces, the predicate is over `owner` *and*
    /// `fence`, so it stays correct even when the successor is this same owner
    /// re-acquiring (§5.8.1).
    async fn release_if_holder(&self) -> Result<(), ClusterError> {
        match &self.token {
            Some(token) => self.leases.release(&self.key, token).await,
            None => Ok(()),
        }
    }
}

/// A uniform jitter in `0..max` drawn from the thread RNG (zero when `max` is
/// zero). Spreads follower reclaim attempts so simultaneous contenders
/// desynchronize. Uses the same `rand` source as the watch auto-restart backoff
/// jitter — `ThreadRng` is entropy-seeded, so concurrent participants diverge
/// without any explicit seeding.
fn reclaim_jitter(max: Duration) -> Duration {
    let max_nanos = u64::try_from(max.as_nanos()).unwrap_or(u64::MAX);
    if max_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rand::rng().random_range(0..max_nanos))
}

/// What one cache-watch event means for the election loop (B5).
///
/// Three outcomes rather than the `bool` this used to be, because the middle one
/// is the whole of the fix: a **retryable** subscription close is not a dead
/// claim, and collapsing it into "stop" released a live claim on an ordinary
/// failover — the divergence from Profile 3's remote pump that invariant I1
/// forbids.
enum WatchOutcome {
    /// Keep going; the watch is intact.
    Continue,
    /// The watch closed retryably. Re-subscribe and keep the claim renewing.
    Resubscribe,
    /// Terminal — a non-retryable close, or the consumer's channel is gone.
    Stop,
}

impl From<bool> for WatchOutcome {
    /// `reconcile` / `try_send` return `false` only when the consumer's channel is
    /// closed, which is terminal for the loop; `true` means carry on.
    fn from(alive: bool) -> Self {
        if alive { Self::Continue } else { Self::Stop }
    }
}

/// Awaits the next cache watch event, or pends forever once the watch has
/// ended (so the `select!` arm becomes inert rather than busy-looping).
async fn recv_optional(watch: &mut Option<CacheWatch>) -> Option<CacheWatchEvent> {
    match watch.as_mut() {
        Some(watch) => watch.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod leader_tests;
