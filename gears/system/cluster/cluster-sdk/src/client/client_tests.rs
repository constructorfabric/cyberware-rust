//! Tests for [`ClusterClient`](super::ClusterClient).
//!
//! Three properties, all of which the deployable-gear design leans on:
//!
//! - the trait is consumed as `Arc<dyn ClusterClient>` (§3.1, invariant I4);
//! - the three factory methods are callable from a **synchronous** context, so
//!   `resolve()`'s only `await` is the descriptor (§3.1). A plain `#[test]` that
//!   calls them with no runtime is the assertion;
//! - an unbound profile is [`ClusterError::ProfileNotBound`] on every member, and
//!   no new error variant exists for this seam (invariant I3).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::ClusterClient;
use crate::cache::{
    CacheConsistency, CacheEntry, CacheFeatures, CacheWatch, ClusterCacheBackend, PutRequest, Ttl,
};
use crate::dto::{
    CacheDescriptor, LeaderElectionDescriptor, LockDescriptor, ProfileDescriptor, ProfileHealth,
    WireCacheConsistency, WireCacheFeatures, WireLeaderElectionFeatures, WireLockFeatures,
};
use crate::error::ClusterError;
use crate::leader::{ElectionConfig, LeaderElectionBackend, LeaderElectionFeatures, LeaderWatch};
use crate::lock::{DistributedLockBackend, LockFeatures, LockGuard};

const BOUND_PROFILE: &str = "orders";

/// Every stub method that is not part of what these tests assert answers with
/// this, rather than panicking: the tests exercise the seam, not the primitives.
fn not_under_test() -> ClusterError {
    ClusterError::Unsupported {
        feature: "not-under-test",
    }
}

struct StubCacheBackend;

#[async_trait]
impl ClusterCacheBackend for StubCacheBackend {
    fn consistency(&self) -> CacheConsistency {
        CacheConsistency::Linearizable
    }

    fn features(&self) -> CacheFeatures {
        CacheFeatures::new(true)
    }

    fn provider_name(&self) -> &'static str {
        "stub-cache"
    }

    async fn get(&self, _key: &str) -> Result<Option<CacheEntry>, ClusterError> {
        Err(not_under_test())
    }

    async fn put(&self, _req: PutRequest<'_>) -> Result<(), ClusterError> {
        Err(not_under_test())
    }

    async fn delete(&self, _key: &str) -> Result<bool, ClusterError> {
        Err(not_under_test())
    }

    async fn contains(&self, _key: &str) -> Result<bool, ClusterError> {
        Err(not_under_test())
    }

    async fn put_if_absent(
        &self,
        _req: PutRequest<'_>,
    ) -> Result<Option<CacheEntry>, ClusterError> {
        Err(not_under_test())
    }

    async fn compare_and_swap(
        &self,
        _key: &str,
        _expected_version: u64,
        _new_value: &[u8],
        _ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        Err(not_under_test())
    }

    async fn watch(&self, _key: &str) -> Result<CacheWatch, ClusterError> {
        Err(not_under_test())
    }

    async fn watch_prefix(&self, _prefix: &str) -> Result<CacheWatch, ClusterError> {
        Err(not_under_test())
    }
}

struct StubLockBackend;

#[async_trait]
impl DistributedLockBackend for StubLockBackend {
    fn features(&self) -> LockFeatures {
        LockFeatures::new(true)
    }

    fn provider_name(&self) -> &'static str {
        "stub-lock"
    }

    async fn try_lock(&self, _name: &str, _ttl: Duration) -> Result<LockGuard, ClusterError> {
        Err(not_under_test())
    }

    async fn lock(
        &self,
        _name: &str,
        _ttl: Duration,
        _timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        Err(not_under_test())
    }
}

struct StubLeaderElectionBackend;

#[async_trait]
impl LeaderElectionBackend for StubLeaderElectionBackend {
    fn features(&self) -> LeaderElectionFeatures {
        LeaderElectionFeatures::new(true)
    }

    fn provider_name(&self) -> &'static str {
        "stub-leader"
    }

    async fn elect(&self, _name: &str) -> Result<LeaderWatch, ClusterError> {
        Err(not_under_test())
    }

    async fn elect_with_config(
        &self,
        _name: &str,
        _config: ElectionConfig,
    ) -> Result<LeaderWatch, ClusterError> {
        Err(not_under_test())
    }
}

/// A client bound to exactly one profile, which is all that is needed to
/// exercise both the bound and the unbound arm of every member.
struct StubClient {
    cache: Arc<dyn ClusterCacheBackend>,
    lock: Arc<dyn DistributedLockBackend>,
    leader: Arc<dyn LeaderElectionBackend>,
}

impl StubClient {
    fn new() -> Self {
        Self {
            cache: Arc::new(StubCacheBackend),
            lock: Arc::new(StubLockBackend),
            leader: Arc::new(StubLeaderElectionBackend),
        }
    }

    /// Mirrors what a real implementation does: the profile name is interned to
    /// a `&'static str` rather than the frozen error variant being widened
    /// (§12.1, invariant I3).
    fn unbound(profile: &str) -> ClusterError {
        assert_ne!(profile, BOUND_PROFILE);
        ClusterError::ProfileNotBound { profile: "absent" }
    }
}

#[async_trait]
impl ClusterClient for StubClient {
    fn cache_backend(&self, profile: &str) -> Result<Arc<dyn ClusterCacheBackend>, ClusterError> {
        if profile == BOUND_PROFILE {
            return Ok(Arc::clone(&self.cache));
        }
        Err(Self::unbound(profile))
    }

    fn lock_backend(&self, profile: &str) -> Result<Arc<dyn DistributedLockBackend>, ClusterError> {
        if profile == BOUND_PROFILE {
            return Ok(Arc::clone(&self.lock));
        }
        Err(Self::unbound(profile))
    }

    fn leader_election_backend(
        &self,
        profile: &str,
    ) -> Result<Arc<dyn LeaderElectionBackend>, ClusterError> {
        if profile == BOUND_PROFILE {
            return Ok(Arc::clone(&self.leader));
        }
        Err(Self::unbound(profile))
    }

    async fn descriptor(&self, profile: &str) -> Result<ProfileDescriptor, ClusterError> {
        if profile != BOUND_PROFILE {
            return Err(Self::unbound(profile));
        }
        Ok(ProfileDescriptor {
            name: profile.to_owned(),
            cache: CacheDescriptor {
                consistency: WireCacheConsistency::Linearizable,
                features: WireCacheFeatures {
                    prefix_watch: true,
                    watch: Some(true),
                },
                provider: "postgres".to_owned(),
            },
            lock: LockDescriptor {
                features: WireLockFeatures { linearizable: true },
                provider: "postgres".to_owned(),
            },
            leader_election: LeaderElectionDescriptor {
                features: WireLeaderElectionFeatures { linearizable: true },
                provider: "postgres".to_owned(),
            },
            health: ProfileHealth::Serving,
        })
    }
}

fn client() -> Arc<dyn ClusterClient> {
    Arc::new(StubClient::new())
}

/// A plain `#[test]` — no runtime, no `block_on`. That the three factory calls
/// compile and run here *is* the assertion that they are synchronous, which is
/// what keeps the descriptor `resolve()`'s only await (§3.1).
#[test]
fn factory_methods_answer_from_a_synchronous_context() {
    let client = client();

    let cache = client
        .cache_backend(BOUND_PROFILE)
        .expect("the bound profile has a cache binding");
    let lock = client
        .lock_backend(BOUND_PROFILE)
        .expect("the bound profile has a lock binding");
    let leader = client
        .leader_election_backend(BOUND_PROFILE)
        .expect("the bound profile has a leader-election binding");

    assert_eq!(cache.provider_name(), "stub-cache");
    assert_eq!(lock.provider_name(), "stub-lock");
    assert_eq!(leader.provider_name(), "stub-leader");
}

/// The factory returns the *real* backend rather than a wrapper (invariant I14):
/// the `Arc` handed out points at the same allocation the client holds.
#[test]
fn factory_returns_the_backend_arc_itself_not_a_wrapper() {
    let stub = StubClient::new();
    let held = Arc::clone(&stub.cache);
    let client: Arc<dyn ClusterClient> = Arc::new(stub);

    let handed_out = client
        .cache_backend(BOUND_PROFILE)
        .expect("the bound profile has a cache binding");

    assert!(Arc::ptr_eq(&held, &handed_out));
}

#[test]
fn unbound_profile_is_profile_not_bound_on_every_factory_method() {
    let client = client();

    assert!(matches!(
        client.cache_backend("absent"),
        Err(ClusterError::ProfileNotBound { .. })
    ));
    assert!(matches!(
        client.lock_backend("absent"),
        Err(ClusterError::ProfileNotBound { .. })
    ));
    assert!(matches!(
        client.leader_election_backend("absent"),
        Err(ClusterError::ProfileNotBound { .. })
    ));
}

#[tokio::test]
async fn descriptor_reports_the_server_side_provider_and_health() {
    let client = client();

    let descriptor = client
        .descriptor(BOUND_PROFILE)
        .await
        .expect("the bound profile has a descriptor");

    assert_eq!(descriptor.name, BOUND_PROFILE);
    // Never "remote": the operator has to see which real backend a failed
    // capability requirement names (§5.5).
    assert_eq!(descriptor.cache.provider, "postgres");
    assert_eq!(descriptor.lock.provider, "postgres");
    assert_eq!(descriptor.leader_election.provider, "postgres");
    assert_eq!(descriptor.health, ProfileHealth::Serving);
    assert_eq!(
        CacheConsistency::from(descriptor.cache.consistency),
        CacheConsistency::Linearizable
    );
}

#[tokio::test]
async fn descriptor_for_an_unbound_profile_is_profile_not_bound() {
    let client = client();

    assert!(matches!(
        client.descriptor("absent").await,
        Err(ClusterError::ProfileNotBound { .. })
    ));
}
