//! Output port for singleton coordination (DESIGN section 3.3, "Cluster
//! Coordination").
//!
//! The two sweepers run as cluster-wide singletons. The platform `cluster`
//! gear elects the replica that runs each of them; this port is the domain's
//! view of that election. Its only implementation is the `CoordinationAdapter`
//! in `infra::cluster_coordination` (ADR-0006). The port exists for the
//! domain-layer dependency rule; it is not a plugin extension point.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;

/// The singleton scopes of the gear. Closed: each variant maps to exactly one
/// election name, so no free-form name reaches the cluster facade.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SingletonScope {
    /// Physical reclamation of expired leases.
    LeaseSweeper,
    /// Idempotency-record and operation-log retention.
    RetentionSweeper,
}

impl SingletonScope {
    /// Every scope.
    pub const ALL: [Self; 2] = [Self::LeaseSweeper, Self::RetentionSweeper];

    /// The election name of the scope, under the gear's scope prefix.
    #[must_use]
    pub const fn election_name(self) -> &'static str {
        match self {
            Self::LeaseSweeper => "lease-sweeper",
            Self::RetentionSweeper => "retention-sweeper",
        }
    }
}

impl fmt::Display for SingletonScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.election_name())
    }
}

/// The future one run of a sweep body produces.
pub type LeaderWorkFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A sweep body. The coordinator calls it on every election with a child
/// [`CancellationToken`] and cancels that token on leadership loss. The body
/// observes the token at every await point and returns promptly once it fires.
pub type LeaderWork = Arc<dyn Fn(CancellationToken) -> LeaderWorkFuture + Send + Sync>;

/// Runs a sweep body while this replica leads a scope.
///
/// Semantics (advisory, per the cluster gear): the body starts when this
/// replica becomes leader, is cancelled when leadership is lost, and restarts
/// on re-election. The cluster gear renews the claim. Two replicas can run the
/// body at once for a bounded window after a partition, so every body is
/// idempotent.
#[async_trait]
pub trait SingletonCoordinator: Send + Sync {
    /// Joins the election of `scope` and runs `work` while this replica leads.
    ///
    /// Returns `Ok(())` after `shutdown` fires: the running body is cancelled
    /// and the election is resigned, so a successor is elected without a TTL
    /// wait.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::ClusterUnavailable`] when the election cannot be
    /// joined or closes terminally.
    async fn run_while_leader(
        &self,
        scope: SingletonScope,
        shutdown: CancellationToken,
        work: LeaderWork,
    ) -> Result<(), DomainError>;
}

/// Resolves the coordinator at gear start.
///
/// The resolve validates the operator's cluster binding for the
/// `quota-enforcement` profile: an unbound profile or a backend without a
/// linearizable election fails here, before the gear reports ready.
#[async_trait]
pub trait CoordinatorBinding: Send + Sync {
    /// Resolve the coordinator.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::ClusterUnavailable`] with the cluster gear's
    /// diagnostic when the binding is unusable.
    async fn resolve(&self) -> Result<Arc<dyn SingletonCoordinator>, DomainError>;
}
