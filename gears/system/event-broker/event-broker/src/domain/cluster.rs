//! `ClusterCapabilities` usage (`DESIGN.md:596,739-768`). Per
//! `docs/ADR/0007-service-decomposition.md` D2/D3, this wraps the platform
//! `cluster` gear's real `cluster-sdk` facades directly - no bespoke
//! coordination abstraction is introduced. There is no separate pub/sub
//! primitive; notification semantics are realized on top of
//! `ClusterCacheV1::put` + `ClusterCacheV1::watch`
//! (`infra::workers::ingest_outbox`'s `notify`, keyed by `domain::notify`).

use cluster_sdk::{
    ClusterCacheV1, ClusterError, ClusterProfile, DistributedLockV1, LeaderElectionV1,
};
use toolkit::client_hub::ClientHub;
use toolkit::domain_model;

/// Every Event Broker cache key / lock name / election name is scoped under
/// this prefix (`DESIGN.md:756-764`'s `evbk.*` cache-key table).
pub const CLUSTER_SCOPE_PREFIX: &str = "evbk";

/// The cluster-gear profile the event broker binds to. Operators configure
/// `modules.cluster.profiles.event-broker` (cache/lock/discovery provider
/// selection) to bind a backend to it - not decided in
/// `docs/ADR/0007-service-decomposition.md` (its "Not decided here" list),
/// resolved here as the first ticket to exercise real cluster-mode wiring.
#[derive(Debug, Clone, Copy)]
struct EventBrokerProfile;

impl ClusterProfile for EventBrokerProfile {
    const NAME: &'static str = "event-broker";
}

/// Resolved handle onto the platform `cluster` gear's three coordination
/// primitives, scoped for the Event Broker. Both standalone and cluster
/// modes resolve the same facade types (`DESIGN.md:2208`'s "variants of the
/// same module wiring") - standalone is simply backed by the
/// zero-dependency `standalone` cluster-gear provider instead of a
/// network-backed one.
///
/// Instance discovery is **not** one of these primitives - it's
/// `DirectoryService`'s job (ADR-0009), resolved separately via
/// `ClientHub::get::<dyn toolkit::directory::DirectoryClient>()` (see
/// `module::EventBrokerModule::register_self` and
/// `infra::dispatcher::forward`), not scoped under `CLUSTER_SCOPE_PREFIX`
/// since the directory is a flat, cross-gear namespace (`docs/DESIGN.md`'s
/// "Open — broker team" shard-discovery note).
#[domain_model]
pub struct EventBrokerCluster {
    pub cache: ClusterCacheV1,
    pub leader_election: LeaderElectionV1,
    pub lock: DistributedLockV1,
}

impl EventBrokerCluster {
    /// Resolves and `"evbk"`-scopes all three primitives from `ClientHub`,
    /// bound to the `event-broker` cluster-gear profile (whatever backend -
    /// `standalone` or `postgres` - the operator configured for it).
    ///
    /// # Errors
    /// Returns [`ClusterError::ProfileNotBound`] if no cluster-gear provider
    /// is registered for the `event-broker` profile (the `cluster` gear must
    /// run first and declare it), or another [`ClusterError`] if a primitive
    /// fails to resolve.
    /// `async` because the cluster gear's own primitive resolvers are: a
    /// profile's backend may not be bound yet when this is first asked, and
    /// resolving awaits it rather than failing.
    pub async fn resolve(hub: &ClientHub) -> Result<Self, ClusterError> {
        Ok(Self {
            cache: ClusterCacheV1::resolver(hub)
                .profile(EventBrokerProfile)
                .resolve()
                .await?
                .scoped(CLUSTER_SCOPE_PREFIX)?,
            leader_election: LeaderElectionV1::resolver(hub)
                .profile(EventBrokerProfile)
                .resolve()
                .await?
                .scoped(CLUSTER_SCOPE_PREFIX)?,
            lock: DistributedLockV1::resolver(hub)
                .profile(EventBrokerProfile)
                .resolve()
                .await?
                .scoped(CLUSTER_SCOPE_PREFIX)?,
        })
    }
}
