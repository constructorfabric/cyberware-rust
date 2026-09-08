//! Native leader election over one `Lease` per election (DESIGN.md §4).
//!
//! [`K8sLeaderElection`] implements [`LeaderElectionBackend`] over
//! `coordination.k8s.io/v1.Lease`. Each active election runs one background task
//! that owns the whole lifecycle: it establishes a `metadata.name`-scoped watch
//! *before* its first acquire (so a transition between the two cannot be missed,
//! §4.3), claims or follows, renews on the derived interval (§4.2), reconciles
//! status from the watch (§4.3), services an explicit [`resign`](LeaderWatch::resign)
//! (§4.4), and revokes cleanly on shutdown (§11).
//!
//! Two pure state machines the task drives live in the submodules and carry the L1
//! coverage: [`renew`] (the renewal-outcome decision) and [`watch`] (the
//! watcher-event → leadership-transition mapping). Real-server behaviour is
//! exercised in Phase 6.

mod renew;
mod watch;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::Api;
use kube::api::ObjectMeta;
use kube::runtime::watcher;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use cluster_sdk::ClusterError;
use cluster_sdk::leader::{
    ElectionConfig, LeaderElectionBackend, LeaderElectionFeatures, LeaderStatus, LeaderWatch,
    LeaderWatchEvent, LeaderWatchSender, ResignReceiver, ResignResponder,
};
use cluster_sdk::observability::ClusterMetrics;
use cluster_sdk::observability::{self, ResourceId, logs, spans, transition};
use tracing::Instrument as _;

use crate::client::ResolvedClient;
use crate::config::K8sLeaderElectionConfig;
use crate::guarded::{self, CallSite, Created, Replaced};
use crate::k8s_error;
use crate::lease::lease_duration_seconds;
use crate::naming::{
    self, ANNOTATION_NAME, LABEL_MANAGED_BY, LABEL_PRIMITIVE, MANAGED_BY_VALUE, Seg,
};
use crate::observed::Observed;

use self::renew::{RenewAction, RenewOutcome, decide_renew};
use self::watch::{WatchSignal, classify_event, holder_of, holder_transitions};

/// The in-flight event buffer for each [`LeaderWatch`] (§4.3).
const EVENT_BUFFER: usize = 16;

/// The `(holderIdentity, renewTime)` pair `Observed` tracks for expiry (§2.8). Both
/// are compared only for equality — the timestamp string is never parsed.
type Record = (Option<String>, Option<String>);

/// Shared, cheaply-cloned runtime for every election this backend runs: the client,
/// the resolved namespace/identity, and the derived timing/naming config.
struct LeaderRuntime {
    client: kube::Client,
    namespace: String,
    identity: String,
    lease_prefix: String,
    /// Per-election overrides pinning a coordination name to a literal, pre-existing
    /// Lease object name — the rolling-migration escape hatch (§14).
    election_lease_names: BTreeMap<String, String>,
    request_timeout: Duration,
    max_acquire_backoff: Duration,
    min_election_ttl: Duration,
    provider: &'static str,
    metrics: Arc<dyn ClusterMetrics>,
}

impl LeaderRuntime {
    /// The namespaced `Lease` API.
    fn api(&self) -> Api<Lease> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// The object name for `coordination_name` (§2.2), honouring an
    /// `election_lease_names` override that pins it to a literal Lease name (§14).
    fn lease_name(&self, coordination_name: &str) -> String {
        self.election_lease_names
            .get(coordination_name)
            .cloned()
            .unwrap_or_else(|| {
                naming::lease_name(&self.lease_prefix, Seg::Election, coordination_name)
            })
    }

    /// Reads the election Lease, bounded by `request_timeout` (§4.2).
    async fn read(&self, name: &str) -> Result<Option<Lease>, ClusterError> {
        let api = self.api();
        self.timed("get lease", guarded::read(&api, name)).await
    }

    /// Runs `fut` under the per-request timeout, mapping an elapsed budget to a
    /// [`ProviderErrorKind::Timeout`](cluster_sdk::ProviderErrorKind::Timeout) (§4.2).
    async fn timed<T, F>(&self, ctx: &'static str, fut: F) -> Result<T, ClusterError>
    where
        F: std::future::Future<Output = Result<T, ClusterError>>,
    {
        match tokio::time::timeout(self.request_timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(k8s_error::timeout(ctx)),
        }
    }

    /// Emits the shared provider-error signals (`cluster_provider_errors_total` +
    /// the `cluster.provider.error` ERROR log) for a leader op that yielded a
    /// `Provider` error (§8). A non-`Provider` outcome (a 409 re-read, a follow) is a
    /// no-op — `emit_provider_error` filters. There is no leader *op* counter in the
    /// ADR-004 catalog (only `cluster_leader_transitions_total`, emitted by the
    /// watch), so this is the leader's whole metric contribution beyond transitions.
    fn emit_error<T>(&self, op: &'static str, election: &str, outcome: &Result<T, ClusterError>) {
        if let Err(err) = outcome {
            observability::emit_provider_error(
                &*self.metrics,
                self.provider,
                op,
                ResourceId::Election(election),
                err,
            );
        }
    }

    /// A fresh claim `Lease` for `coordination_name`, holder set to us (no
    /// `resourceVersion` — this is the create path).
    fn new_claim(&self, coordination_name: &str, ttl: Duration) -> Result<Lease, ClusterError> {
        let mut lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.lease_name(coordination_name)),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([
                    (LABEL_MANAGED_BY.to_owned(), MANAGED_BY_VALUE.to_owned()),
                    (
                        LABEL_PRIMITIVE.to_owned(),
                        Seg::Election.primitive_label().to_owned(),
                    ),
                ])),
                annotations: Some(BTreeMap::from([(
                    ANNOTATION_NAME.to_owned(),
                    coordination_name.to_owned(),
                )])),
                ..ObjectMeta::default()
            },
            spec: Some(LeaseSpec::default()),
        };
        self.set_claim(&mut lease, ttl)?;
        Ok(lease)
    }

    /// Stamps `lease.spec` with our holder, a fresh `renewTime`, and the rounded-up
    /// `leaseDurationSeconds` (§2.9). Used by both the create and guarded-replace
    /// paths.
    fn set_claim(&self, lease: &mut Lease, ttl: Duration) -> Result<(), ClusterError> {
        let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
        spec.holder_identity = Some(self.identity.clone());
        spec.lease_duration_seconds = Some(lease_duration_seconds(ttl)?);
        spec.renew_time = Some(now_micro());
        if spec.acquire_time.is_none() {
            spec.acquire_time = Some(now_micro());
        }
        Ok(())
    }
}

/// The native Kubernetes leader-election backend (§4).
pub struct K8sLeaderElection {
    runtime: Arc<LeaderRuntime>,
    /// Cancelled on shutdown so every in-flight election task revokes (§11).
    shutdown: CancellationToken,
    /// Handles of the spawned election tasks, awaited by [`stop`](Self::stop).
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl K8sLeaderElection {
    /// Builds a backend from a resolved client and the leader-election config (§3.5).
    ///
    /// # Errors
    ///
    /// [`ClusterError::InvalidConfig`] when `lease_prefix` is not a legal RFC 1123
    /// label (§2.2) — validated once at build so a bad prefix fails fast rather than
    /// on the first `elect`.
    pub fn new(
        resolved: &ResolvedClient,
        config: &K8sLeaderElectionConfig,
        metrics: Arc<dyn ClusterMetrics>,
    ) -> Result<Self, ClusterError> {
        naming::validate_lease_prefix(&config.lease_prefix)?;
        let runtime = LeaderRuntime {
            client: resolved.client.clone(),
            namespace: resolved.namespace.clone(),
            identity: resolved.identity.clone(),
            lease_prefix: config.lease_prefix.clone(),
            election_lease_names: config.election_lease_names.clone(),
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            max_acquire_backoff: Duration::from_millis(config.max_acquire_backoff_ms),
            min_election_ttl: Duration::from_millis(config.min_election_ttl_ms),
            provider: crate::provider::PROVIDER_NAME,
            metrics,
        };
        Ok(Self {
            runtime: Arc::new(runtime),
            shutdown: CancellationToken::new(),
            tasks: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Cancels every election task and awaits them, so a leader has observed loss
    /// before this returns (§11). Idempotent.
    pub async fn stop(&self) {
        self.shutdown.cancel();
        let handles = {
            let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *tasks)
        };
        for handle in handles {
            let _joined = handle.await;
        }
    }

    /// Cancels every election task synchronously, without awaiting them — the
    /// teardown the handle's `Drop` uses when `stop()` was never called and cannot
    /// `.await` (§11). Each task revokes its own leader watch as it observes the
    /// cancel.
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }

    /// Tracks a spawned task, pruning finished handles.
    fn track(&self, handle: JoinHandle<()>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|h| !h.is_finished());
        tasks.push(handle);
    }

    /// Validates timing, spawns the election task, and returns the consumer watch.
    fn enrol(&self, name: &str, config: ElectionConfig) -> Result<LeaderWatch, ClusterError> {
        let _span = tracing::info_span!(
            spans::LEADER_ELECT, provider = %self.runtime.provider, election = %name
        )
        .entered();
        if self.shutdown.is_cancelled() {
            return Err(ClusterError::Shutdown);
        }
        // Reject an election TTL that would generate an abusive renewal rate (§2.10).
        crate::lease::check_election_ttl_floor(
            config.ttl(),
            self.runtime.min_election_ttl,
            u32::from(config.max_missed_renewals()),
        )?;
        naming::validate_lease_prefix(&self.runtime.lease_prefix)?;

        let (sender, resign_rx, mut consumer_watch) =
            LeaderWatch::channel(EVENT_BUFFER, LeaderStatus::Follower);
        consumer_watch.set_observability(self.runtime.provider, Arc::clone(&self.runtime.metrics));

        let task = ElectionTask {
            runtime: Arc::clone(&self.runtime),
            coordination_name: name.to_owned(),
            lease_name: self.runtime.lease_name(name),
            config,
            sender,
            status: LeaderStatus::Follower,
            last_emitted: None,
            resigning: false,
            held: None,
            incumbent: None,
            missed: 0,
            shutdown: self.shutdown.clone(),
        };
        self.track(tokio::spawn(task.run(resign_rx)));
        Ok(consumer_watch)
    }
}

#[async_trait]
impl LeaderElectionBackend for K8sLeaderElection {
    /// Unconditionally linearizable (§3.7): a Lease guarded replace is arbitrated by
    /// the API server's Raft quorum, so at most one holder wins regardless of config.
    fn features(&self) -> LeaderElectionFeatures {
        LeaderElectionFeatures::new(true)
    }

    fn provider_name(&self) -> &'static str {
        crate::provider::PROVIDER_NAME
    }

    async fn elect(&self, name: &str) -> Result<LeaderWatch, ClusterError> {
        self.enrol(name, ElectionConfig::default())
    }

    async fn elect_with_config(
        &self,
        name: &str,
        config: ElectionConfig,
    ) -> Result<LeaderWatch, ClusterError> {
        self.enrol(name, config)
    }
}

/// One in-flight election's background task (§4).
struct ElectionTask {
    runtime: Arc<LeaderRuntime>,
    /// The unmapped coordination name (spans/annotations), distinct from the mapped
    /// object [`lease_name`](Self::lease_name).
    coordination_name: String,
    lease_name: String,
    config: ElectionConfig,
    sender: LeaderWatchSender,
    /// The current internal leadership state, driving the tick logic (claim vs renew).
    status: LeaderStatus,
    /// The last status actually *emitted* to the consumer, for duplicate suppression.
    /// A follower re-confirming it still follows on each claim tick, or any unchanged
    /// re-tick, must not re-emit (cpt-cf-clst-nfr-watch-delivery's no-duplicates rule,
    /// K8S-WATCH-001). `None` until the first emission, so the first status of any kind
    /// always goes through even when it equals the initial internal `Follower`.
    last_emitted: Option<LeaderStatus>,
    /// Set while servicing an explicit resign, so the resulting `Leader -> Lost`
    /// edge records a `resigned` transition rather than a `lost` one (§8).
    resigning: bool,
    /// Our current claim while we hold it: the Lease object (carrying its
    /// `resourceVersion` for the next guarded write) and the monotonic deadline
    /// authority (§2.8).
    held: Option<Held>,
    /// The incumbent's observed record while we follow, for steal timing (§2.8).
    incumbent: Option<Observed<Record>>,
    /// Consecutive renewal failures against the budget (§4.2).
    missed: u8,
    shutdown: CancellationToken,
}

/// Our held claim: the Lease we last wrote and the monotonic deadline it is valid
/// until (§2.8).
struct Held {
    lease: Lease,
    deadline: Instant,
}

/// Whether the task loop continues or tears down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Stop,
}

impl ElectionTask {
    /// The task entry point: emit the initial status, establish the watch, then
    /// select over the renewal timer, the watch stream, resign requests, and
    /// shutdown until the consumer or the plugin tears down.
    async fn run(mut self, mut resign_rx: ResignReceiver) {
        // The design's ordering is watch-then-claim, but the watcher's own initial
        // list (Init/InitApply) delivers the current state, so we subscribe first and
        // let the first `Observed`/timer tick drive the claim.
        //
        // No initial `Status` event is emitted here: the first event the consumer
        // receives is the *outcome* of the first claim — `Leader` for a sole
        // candidate (`became_leader`), `Follower` on a contended one (`set_follower`)
        // — so a sole candidate's first observed status is `Leader`, not a spurious
        // transient `Follower` (SC-LEAD-001). The `LeaderWatch::status()` snapshot is
        // already seeded to `Follower`, which covers the pre-claim baseline. A
        // consumer that has already dropped the watch is caught by the `resign_rx`
        // branch below (biased before the timer), so no claim write is issued for it.
        let api = self.runtime.api();
        let wc = watcher::Config::default().fields(&format!("metadata.name={}", self.lease_name));
        let stream = watcher(api, wc);
        tokio::pin!(stream);

        // Kick off an immediate claim attempt on the first timer fire.
        let mut timer = Box::pin(tokio::time::sleep(Duration::ZERO));

        loop {
            tokio::select! {
                biased;

                () = self.shutdown.cancelled() => {
                    self.sender.revoke_for_shutdown(self.status == LeaderStatus::Leader);
                    return;
                }

                responder = resign_rx.recv() => {
                    match responder {
                        Some(responder) => {
                            self.handle_resign(responder).await;
                            return;
                        }
                        // Consumer dropped the watch without resigning; the claim
                        // lapses via TTL (§11). Nothing more to do.
                        None => return,
                    }
                }

                () = &mut timer => {
                    if self.on_tick().await == Step::Stop {
                        return;
                    }
                    timer = Box::pin(tokio::time::sleep(self.next_delay()));
                    // Keep the event stream's owed lag notice flowing when the
                    // election is quiet (§4.3, the drop-then-`Lagged` rule).
                    self.sender.flush_lagged();
                }

                event = stream.next() => {
                    // A transient stream error is retried and re-listed by the
                    // watcher internally (the consumer sees the resulting `Reset` via
                    // the Init event), and an end-of-stream just means we keep
                    // renewing off the timer — only a real event needs handling.
                    if let Some(Ok(event)) = event
                        && self.on_watch(event).await == Step::Stop
                    {
                        return;
                    }
                }
            }
        }
    }

    /// The renewal / (re)claim timer tick (§4.1, §4.2).
    async fn on_tick(&mut self) -> Step {
        if self.status == LeaderStatus::Leader {
            self.renew_tick().await
        } else {
            match self.try_claim().await {
                Ok(()) => Step::Continue,
                Err(err) => self.close(err),
            }
        }
    }

    /// Renews the held claim (§4.2): guarded replace of `renewTime`, then apply the
    /// pure [`decide_renew`] decision.
    async fn renew_tick(&mut self) -> Step {
        let ttl = self.config.ttl();
        let outcome = match self.renew_once(ttl).await {
            Ok(outcome) => outcome,
            // A non-retryable, non-conflict error (e.g. AuthFailure) is terminal.
            Err(err) => return self.close(err),
        };
        let (action, missed) =
            decide_renew(outcome, self.missed, self.config.max_missed_renewals());
        self.missed = missed;
        match action {
            RenewAction::Continue | RenewAction::Retry => Step::Continue,
            RenewAction::LoseAndReenroll => {
                self.held = None;
                if !self.set_status(LeaderStatus::Lost) {
                    return Step::Stop;
                }
                // Immediately attempt to re-acquire so a transient blip re-enrols
                // with no consumer code (§4.2).
                match self.try_claim().await {
                    Ok(()) => Step::Continue,
                    Err(err) => self.close(err),
                }
            }
        }
    }

    /// One renewal attempt, returning the pure [`RenewOutcome`] or a fatal error.
    async fn renew_once(&mut self, ttl: Duration) -> Result<RenewOutcome, ClusterError> {
        let Some(held) = self.held.as_ref() else {
            return Ok(RenewOutcome::DeadlinePassed);
        };
        // The deadline authority is primary (§2.8): if we cannot prove we were
        // still inside our lease, do not write.
        if held.deadline <= Instant::now() {
            return Ok(RenewOutcome::DeadlinePassed);
        }
        let mut lease = held.lease.clone();
        self.runtime.set_claim(&mut lease, ttl)?;
        let api = self.runtime.api();
        let span = tracing::info_span!(
            spans::LEADER_RENEW, provider = %self.runtime.provider, election = %self.coordination_name
        );
        let replaced = self
            .runtime
            .timed(
                "renew lease",
                guarded::replace(&api, &self.lease_name, &lease, CallSite::LeaderRenew),
            )
            .instrument(span)
            .await;
        // Emit on the raw result so a *retryable* provider error (later folded into
        // `RenewOutcome::Retryable`) is still counted as a provider error (§8).
        self.runtime
            .emit_error("renew", &self.coordination_name, &replaced);
        match replaced {
            Ok(Replaced::Applied(applied)) => {
                self.held = Some(Held {
                    lease: *applied,
                    deadline: Instant::now() + ttl,
                });
                Ok(RenewOutcome::Renewed)
            }
            // A 409: someone else wrote the Lease, so the claim is gone now (§4.2).
            Ok(Replaced::Conflict(_)) => Ok(RenewOutcome::Conflict),
            Err(err) if err.is_retryable() => Ok(RenewOutcome::Retryable),
            Err(err) => Err(err),
        }
    }

    /// One acquire/steal attempt (§4.1). Updates `status`/`held`/`incumbent`; a
    /// successful acquire emits `Status(Leader)`.
    async fn try_claim(&mut self) -> Result<(), ClusterError> {
        let ttl = self.config.ttl();
        let existing = self.runtime.read(&self.lease_name).await;
        self.runtime
            .emit_error("elect", &self.coordination_name, &existing);
        let existing = existing?;
        let Some(lease) = existing else {
            // Absent: create it as ours.
            return self.create_claim(ttl).await;
        };

        let holder = holder_of(&lease);
        let is_ours = holder.as_deref() == Some(self.runtime.identity.as_str());
        let free = holder.is_none();

        if is_ours || free {
            self.incumbent = None;
            self.guarded_claim(lease, ttl).await?;
            return Ok(());
        }

        // A foreign live holder: track it and steal only once our own observation
        // has aged past a full TTL (§2.8) — never on first sight. `Observed` runs
        // on `std::time::Instant`, independent of the tokio timer clock.
        let record: Record = claim_record(&lease);
        let now = std::time::Instant::now();
        match self.incumbent.as_mut() {
            Some(observed) => observed.observe(record, now),
            None => self.incumbent = Some(Observed::new(record, now)),
        }
        let expired = self
            .incumbent
            .as_ref()
            .is_some_and(|o| o.is_expired(now, ttl));
        if expired {
            self.incumbent = None;
            self.guarded_claim(lease, ttl).await?;
        } else {
            self.set_follower();
        }
        Ok(())
    }

    /// Creates the Lease as ours (§4.1). On `409 AlreadyExists` another candidate
    /// raced us to the create; we fall back to following this tick.
    async fn create_claim(&mut self, ttl: Duration) -> Result<(), ClusterError> {
        let lease = self.runtime.new_claim(&self.coordination_name, ttl)?;
        let api = self.runtime.api();
        let created = self
            .runtime
            .timed(
                "create lease",
                guarded::create(&api, &lease, CallSite::LeaderAcquire),
            )
            .await;
        self.runtime
            .emit_error("elect", &self.coordination_name, &created);
        match created? {
            Created::Created(applied) => self.became_leader(*applied, ttl),
            Created::Exists => self.set_follower(),
        }
        Ok(())
    }

    /// Guarded replace claiming a free/lapsed/own Lease (§4.1). `Applied` → leader;
    /// a 409 → we lost the race, follow this tick.
    async fn guarded_claim(&mut self, mut lease: Lease, ttl: Duration) -> Result<(), ClusterError> {
        // Bump `leaseTransitions` when leadership actually changes hands — the holder
        // was someone else (or nobody) rather than us (§2.3, the k8s convention). A
        // renewal of our own claim leaves it untouched. The initial create path
        // (`create_claim`) leaves it at the default 0.
        if holder_of(&lease).as_deref() != Some(self.runtime.identity.as_str()) {
            let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
            spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0) + 1);
            // Leadership actually changed hands: stamp a fresh `acquireTime` so it
            // moves together with `leaseTransitions` (the k8s convention), rather
            // than reporting the previous holder's acquisition time under our
            // identity. `set_claim` below only *seeds* `acquireTime` when absent, so
            // it preserves this; a renewal of our own claim never enters this branch
            // and keeps its `acquireTime` stable (§2.3).
            spec.acquire_time = Some(now_micro());
        }
        self.runtime.set_claim(&mut lease, ttl)?;
        let api = self.runtime.api();
        let replaced = self
            .runtime
            .timed(
                "claim lease",
                guarded::replace(&api, &self.lease_name, &lease, CallSite::LeaderAcquire),
            )
            .await;
        self.runtime
            .emit_error("elect", &self.coordination_name, &replaced);
        match replaced? {
            Replaced::Applied(applied) => self.became_leader(*applied, ttl),
            Replaced::Conflict(_) => self.set_follower(),
        }
        Ok(())
    }

    /// Records the won claim and emits `Status(Leader)`.
    fn became_leader(&mut self, lease: Lease, ttl: Duration) {
        self.held = Some(Held {
            lease,
            deadline: Instant::now() + ttl,
        });
        self.missed = 0;
        // A consumer-gone here surfaces on the next `set_status`; ignore the bool.
        let _still_watching = self.set_status(LeaderStatus::Leader);
    }

    /// Transitions to `Follower` (idempotently).
    fn set_follower(&mut self) {
        let _still_watching = self.set_status(LeaderStatus::Follower);
    }

    /// Reconciles a watch event into transitions / a (re)claim (§4.3).
    async fn on_watch(&mut self, event: watcher::Event<Lease>) -> Step {
        match classify_event(event) {
            WatchSignal::Observed(holder) => {
                // A watch event can still name us as holder after we dropped the
                // claim (an in-flight event, or the re-list replay). Reporting
                // `Leader` from that observation — without a proven, in-window
                // `held` claim — opens a false leadership window that the very next
                // renew tick loses (`held == None` → `DeadlinePassed` → `Lost`),
                // during which a consumer may run leader-only work. Trust the
                // watch's `Leader` edge only while we actually hold a live claim
                // (§4.3).
                let proven = self
                    .held
                    .as_ref()
                    .is_some_and(|held| held.deadline > Instant::now());
                for transition in
                    holder_transitions(self.status, holder.as_deref(), &self.runtime.identity)
                {
                    if transition == LeaderStatus::Leader && !proven {
                        continue;
                    }
                    if !self.set_status(transition) {
                        return Step::Stop;
                    }
                }
                // A free Lease, or the object still naming us while we hold no
                // proven claim: (re)establish `held` through the claim path now
                // rather than waiting for the timer. `try_claim`'s `is_ours`/free
                // branch does the guarded replace and `became_leader`, which is
                // what actually re-acquires leadership after the gate above.
                let observed_self = holder.as_deref() == Some(self.runtime.identity.as_str());
                if self.status != LeaderStatus::Leader
                    && (holder.is_none() || (observed_self && !proven))
                    && let Err(err) = self.try_claim().await
                {
                    return self.close(err);
                }
                Step::Continue
            }
            WatchSignal::Vacated => {
                // The object was deleted out from under us; re-enter the claim path.
                if self.status == LeaderStatus::Leader && !self.set_status(LeaderStatus::Lost) {
                    return Step::Stop;
                }
                self.held = None;
                match self.try_claim().await {
                    Ok(()) => Step::Continue,
                    Err(err) => self.close(err),
                }
            }
            WatchSignal::Relisted => {
                if self.sender.try_send(LeaderWatchEvent::Reset) {
                    Step::Continue
                } else {
                    Step::Stop
                }
            }
            WatchSignal::Quiet => Step::Continue,
        }
    }

    /// Services an explicit resign (§4.4): guarded replace clearing the holder,
    /// respond with the result, emit `Status(Lost)`, and stop. A 409 is `Ok(())` —
    /// the claim we were asked to release is already gone.
    async fn handle_resign(&mut self, responder: ResignResponder) {
        // Mark the resign so the `Leader -> Lost` edge below records `resigned`
        // rather than `lost` (§8); a resign while merely following records nothing.
        self.resigning = true;
        let span = tracing::info_span!(
            spans::LEADER_RESIGN, provider = %self.runtime.provider, election = %self.coordination_name
        );
        let result = self.release_claim().instrument(span).await;
        self.runtime
            .emit_error("resign", &self.coordination_name, &result);
        responder.respond(result);
        let _still_watching = self.set_status(LeaderStatus::Lost);
    }

    /// Guarded replace clearing our `holderIdentity` (§4.4). Absence / a lost race
    /// are both `Ok(())`.
    async fn release_claim(&mut self) -> Result<(), ClusterError> {
        let Some(held) = self.held.take() else {
            return Ok(()); // never held, or already lost
        };
        let mut lease = held.lease;
        if let Some(spec) = lease.spec.as_mut() {
            spec.holder_identity = None;
            spec.renew_time = Some(now_micro());
        }
        let api = self.runtime.api();
        let replaced = self
            .runtime
            .timed(
                "resign lease",
                guarded::replace(&api, &self.lease_name, &lease, CallSite::Resign),
            )
            .await?;
        match replaced {
            // Applied, or a 409 that classified as AlreadyReleased: the claim is gone.
            Replaced::Applied(_) | Replaced::Conflict(_) => Ok(()),
        }
    }

    /// The next timer delay: the renewal interval while leading, else a jittered
    /// backoff bounded by `max_acquire_backoff` (§4.1).
    fn next_delay(&self) -> Duration {
        if self.status == LeaderStatus::Leader {
            self.config.renewal_interval()
        } else {
            jittered_backoff(self.runtime.max_acquire_backoff)
        }
    }

    /// Updates the cached status and emits the matching `Status` event, returning
    /// `false` when the consumer has dropped the watch.
    fn set_status(&mut self, status: LeaderStatus) -> bool {
        let previous = self.last_emitted;
        self.status = status;
        // Suppress a repeat of the already-emitted status (a follower re-confirming on
        // each claim tick is the common case). The first emission of any status always
        // goes through, even when it equals the initial internal `Follower`.
        if previous == Some(status) {
            return true; // unchanged; the watch is still live as far as we can tell
        }
        self.last_emitted = Some(status);
        // Record the leadership-transition signals on the semantic edges (§8): every
        // loss path funnels through `set_status(Lost)`, so this one place covers them
        // all — `resigned` when an explicit resign drove it, else `lost`.
        match status {
            LeaderStatus::Leader if previous != Some(LeaderStatus::Leader) => {
                self.record_transition(transition::ACQUIRED);
            }
            LeaderStatus::Lost if previous == Some(LeaderStatus::Leader) => {
                let kind = if self.resigning {
                    transition::RESIGNED
                } else {
                    transition::LOST
                };
                self.record_transition(kind);
            }
            _ => {}
        }
        self.sender.try_send_status(status)
    }

    /// Emits the leadership-transition signals: the `cluster_leader_transitions_total`
    /// metric and the `cluster.leader.transition` INFO log, labelled by the bounded
    /// [`transition`] kind (§8), mirroring the CAS-based default's `record_transition`.
    fn record_transition(&self, transition: &'static str) {
        self.runtime.metrics.leader_transition(transition);
        tracing::event!(
            name: logs::LEADER_TRANSITION,
            tracing::Level::INFO,
            provider = %self.runtime.provider,
            election = %self.coordination_name,
            transition,
            "cluster leadership transition"
        );
    }

    /// Closes the watch terminally with `err` (§4.3) and stops the task.
    fn close(&mut self, err: ClusterError) -> Step {
        self.sender.try_close(err);
        Step::Stop
    }
}

/// A jittered backoff in `[0, max)`, full-jitter (§4.1).
fn jittered_backoff(max: Duration) -> Duration {
    use rand::RngExt as _;
    let max_nanos = u64::try_from(max.as_nanos()).unwrap_or(u64::MAX);
    if max_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rand::rng().random_range(0..max_nanos))
}

/// The current wall-clock as a `MicroTime`, written to `renewTime`/`acquireTime`
/// for `kubectl`/`client-go` readers (§2.8 — output, never input to expiry).
fn now_micro() -> MicroTime {
    MicroTime(k8s_openapi::jiff::Timestamp::now())
}

/// The `(holderIdentity, renewTime)` record for `Observed` equality (§2.8).
fn claim_record(lease: &Lease) -> Record {
    let holder = holder_of(lease);
    let renew = lease
        .spec
        .as_ref()
        .and_then(|spec| spec.renew_time.as_ref())
        .map(|t| t.0.to_string());
    (holder, renew)
}
