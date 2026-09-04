//! `CoordinationAdapter`: the sweeper elections on the platform `cluster` gear
//! (DESIGN section 3.2, section 3.3 "Cluster Coordination", ADR-0006).
//!
//! The gear depends on `cluster-sdk` only. It declares no `deps = [cluster]`
//! edge: a deployed consumer links no cluster gear, and the edge would fail
//! the registry build (cluster DESIGN section 3.17.7). Start ordering comes
//! from the cluster gear's `system` tier, readiness gating from the
//! SDK-submitted consumer registration. The embedded binary links the
//! `cluster` gear, a provider plugin, and `grpc-hub`; the remote image enables
//! this crate's `grpc-client` feature and links none of them.
//!
//! The adapter drives the election watch itself, with the reactive pattern the
//! SDK's `run_while_leader` implements, and keeps ownership of the watch: the
//! SDK combinator consumes the watch, and a dropped watch performs no resign.
//! Keeping the watch is what lets graceful shutdown resign, so a successor is
//! elected without a TTL wait.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cluster_sdk::{
    ClusterError, ClusterProfile, ElectionConfig, LeaderElectionCapability, LeaderElectionV1,
    LeaderStatus, LeaderWatch, LeaderWatchEvent,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;

use crate::domain::error::DomainError;
use crate::domain::ports::coordination::{
    CoordinatorBinding, LeaderWork, SingletonCoordinator, SingletonScope,
};

const LOG_TARGET: &str = "qe.coordination";

/// Scope prefix of every quota-enforcement election name. The full name of a
/// scope is `qe/<election name>`.
pub const SCOPE_PREFIX: &str = "qe";

/// The typed cluster profile of the gear. The profile name appears here and
/// in the operator's cluster YAML, and nowhere else.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotaEnforcementProfile;

impl ClusterProfile for QuotaEnforcementProfile {
    const NAME: &'static str = "quota-enforcement";
}

cluster_sdk::register_cluster_profile!(QuotaEnforcementProfile);

/// Election timing plus the stop budget of a sweep body.
#[derive(Debug, Clone, Copy)]
pub struct ElectionTiming {
    config: ElectionConfig,
    stop_timeout: Duration,
}

impl ElectionTiming {
    /// Validated timing.
    ///
    /// # Errors
    ///
    /// Returns the cluster gear's [`ClusterError::InvalidConfig`] when the TTL
    /// or the missed-renewal budget cannot drive an election.
    pub fn new(
        ttl: Duration,
        max_missed_renewals: u8,
        stop_timeout: Duration,
    ) -> Result<Self, ClusterError> {
        Ok(Self {
            config: ElectionConfig::new(ttl, max_missed_renewals)?,
            stop_timeout,
        })
    }

    /// The election configuration handed to the cluster gear.
    #[must_use]
    pub const fn config(&self) -> ElectionConfig {
        self.config
    }

    /// Budget for a sweep body to stop after leadership loss or shutdown.
    #[must_use]
    pub const fn stop_timeout(&self) -> Duration {
        self.stop_timeout
    }
}

/// Resolves the leader-election facade for the `quota-enforcement` profile.
///
/// Runs in the gear's lifecycle `start`, after the cluster gear started. The
/// `Linearizable` requirement is validated against the operator's binding: an
/// eventually consistent backend can elect two leaders on every failover
/// (cluster ADR-009), so it fails here, before the gear reports ready.
pub struct ClusterCoordinationBinding {
    hub: Arc<ClientHub>,
    timing: ElectionTiming,
}

impl ClusterCoordinationBinding {
    /// Bind to the hub the cluster client is registered in.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, timing: ElectionTiming) -> Self {
        Self { hub, timing }
    }
}

#[async_trait]
impl CoordinatorBinding for ClusterCoordinationBinding {
    async fn resolve(&self) -> Result<Arc<dyn SingletonCoordinator>, DomainError> {
        let election = LeaderElectionV1::resolver(&self.hub)
            .profile(QuotaEnforcementProfile)
            .require(LeaderElectionCapability::Linearizable)
            .resolve()
            .await
            .map_err(|e| cluster_unavailable(&e))?;
        let election = election
            .scoped(SCOPE_PREFIX)
            .map_err(|e| cluster_unavailable(&e))?;
        tracing::info!(
            target: LOG_TARGET,
            profile = QuotaEnforcementProfile::NAME,
            linearizable = election.features().linearizable,
            "resolved the cluster leader election for the sweeper singletons"
        );
        Ok(Arc::new(ClusterCoordination {
            election,
            timing: self.timing,
        }))
    }
}

/// The adapter over the resolved leader-election facade.
// @cpt-dod:cpt-cf-quota-enforcement-dod-coordination-adapter:p1
pub struct ClusterCoordination {
    election: LeaderElectionV1,
    timing: ElectionTiming,
}

#[async_trait]
impl SingletonCoordinator for ClusterCoordination {
    async fn run_while_leader(
        &self,
        scope: SingletonScope,
        shutdown: CancellationToken,
        work: LeaderWork,
    ) -> Result<(), DomainError> {
        let watch = self
            .election
            .elect_with_config(scope.election_name(), self.timing.config())
            .await
            .map_err(|e| cluster_unavailable(&e))?;
        drive(scope, watch, shutdown, work, self.timing.stop_timeout()).await
    }
}

/// Drives one election watch until `shutdown` fires or the watch closes.
///
/// The work starts on `Leader` with a child token, is cancelled on `Lost` or
/// `Follower`, is aborted after `stop_timeout` when it does not return, and
/// restarts on re-election. `Lagged` and `Reset` change nothing: the next
/// status event reconciles. On shutdown the running work is stopped first and
/// the election is resigned afterwards.
pub(crate) async fn drive(
    scope: SingletonScope,
    mut watch: LeaderWatch,
    shutdown: CancellationToken,
    work: LeaderWork,
    stop_timeout: Duration,
) -> Result<(), DomainError> {
    let mut active: Option<ActiveWork> = None;
    loop {
        let event = tokio::select! {
            biased;
            () = shutdown.cancelled() => None,
            event = watch.changed() => Some(event),
        };
        let Some(event) = event else {
            return resign_on_shutdown(scope, watch, active.take(), stop_timeout).await;
        };
        if let Some(outcome) = apply_event(scope, event, &mut active, &work, stop_timeout).await {
            return outcome;
        }
    }
}

/// Shutdown: stop the running body first, then resign while the watch is still
/// owned, so a successor is elected without a TTL wait.
async fn resign_on_shutdown(
    scope: SingletonScope,
    watch: LeaderWatch,
    active: Option<ActiveWork>,
    stop_timeout: Duration,
) -> Result<(), DomainError> {
    stop_if_active(scope, active, stop_timeout).await;
    match watch.resign().await {
        Ok(()) => tracing::info!(
            target: LOG_TARGET,
            %scope,
            "resigned the election on shutdown; a successor is elected without a TTL wait"
        ),
        Err(err) => tracing::warn!(
            target: LOG_TARGET,
            %scope,
            error = %err,
            "resign was not confirmed; the claim lapses via TTL"
        ),
    }
    Ok(())
}

/// Applies one watch event. Returns `Some` when the loop must end.
async fn apply_event(
    scope: SingletonScope,
    event: LeaderWatchEvent,
    active: &mut Option<ActiveWork>,
    work: &LeaderWork,
    stop_timeout: Duration,
) -> Option<Result<(), DomainError>> {
    match event {
        LeaderWatchEvent::Status(status) => {
            apply_status(scope, status, active, work, stop_timeout).await;
            None
        }
        LeaderWatchEvent::Closed(err) => {
            stop_if_active(scope, active.take(), stop_timeout).await;
            tracing::warn!(target: LOG_TARGET, %scope, error = %err, "the election closed");
            Some(Err(cluster_unavailable(&err)))
        }
        // `Lagged` and `Reset` do not change leadership; the next `Status`
        // event reconciles. The enum is non-exhaustive on the SDK side.
        _ => None,
    }
}

/// A leadership transition: start the body on `Leader`, stop it otherwise.
async fn apply_status(
    scope: SingletonScope,
    status: LeaderStatus,
    active: &mut Option<ActiveWork>,
    work: &LeaderWork,
    stop_timeout: Duration,
) {
    match status {
        LeaderStatus::Leader => start_if_idle(scope, active, work),
        LeaderStatus::Lost | LeaderStatus::Follower => {
            if active.is_some() {
                tracing::info!(target: LOG_TARGET, %scope, "leadership lost; the sweep body stops");
            }
            stop_if_active(scope, active.take(), stop_timeout).await;
        }
    }
}

/// Starts the body unless one is still running.
fn start_if_idle(scope: SingletonScope, active: &mut Option<ActiveWork>, work: &LeaderWork) {
    if active.as_ref().is_none_or(ActiveWork::is_finished) {
        let child = CancellationToken::new();
        let handle = tokio::spawn(work(child.clone()));
        *active = Some(ActiveWork { child, handle });
        tracing::info!(target: LOG_TARGET, %scope, "elected; the sweep body runs");
    }
}

async fn stop_if_active(scope: SingletonScope, active: Option<ActiveWork>, stop_timeout: Duration) {
    if let Some(active) = active {
        active.stop(scope, stop_timeout).await;
    }
}

/// A running sweep body and the token that cancels it.
struct ActiveWork {
    child: CancellationToken,
    handle: JoinHandle<()>,
}

impl ActiveWork {
    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Cancels the body and waits for it; aborts it when it overruns the budget.
    async fn stop(mut self, scope: SingletonScope, stop_timeout: Duration) {
        self.child.cancel();
        if tokio::time::timeout(stop_timeout, &mut self.handle)
            .await
            .is_err()
        {
            tracing::warn!(
                target: LOG_TARGET,
                %scope,
                ?stop_timeout,
                "the sweep body did not stop within the budget; aborted"
            );
            self.handle.abort();
            let _aborted = (&mut self.handle).await;
        }
    }
}

impl Drop for ActiveWork {
    /// A dropped worker never outlives the loop: a bare `JoinHandle` drop
    /// detaches the task, so cancel and abort here.
    fn drop(&mut self) {
        self.child.cancel();
        self.handle.abort();
    }
}

/// The domain view of a cluster failure. The domain error is `Clone + Eq` and
/// crosses the layer boundary as a value, so the cause is kept as text.
fn cluster_unavailable(err: &ClusterError) -> DomainError {
    DomainError::ClusterUnavailable(err.to_string())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "cluster_coordination_tests.rs"]
mod cluster_coordination_tests;
