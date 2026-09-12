use std::sync::Arc;

use async_trait::async_trait;
use toolkit::client_hub::ClientHub;

use super::CacheResolverBuilder;
use crate::cache::backend::ClusterCacheBackend;
use crate::cache::facade::ClusterCacheV1;
use crate::cache::types::{
    CacheCapability, CacheConsistency, CacheEntry, CacheFeatures, PutRequest, Ttl,
};
use crate::cache::watch::CacheWatch;
use crate::dto::{
    CacheDescriptor, LeaderElectionDescriptor, LockDescriptor, ProfileDescriptor, ProfileHealth,
    WireCacheConsistency, WireCacheFeatures, WireLeaderElectionFeatures, WireLockFeatures,
};
use crate::error::ClusterError;
use crate::profile::ClusterProfile;
use crate::test_support::StubClusterClient;
use crate::test_support::with_nothing_derivable;

struct StubBackend {
    consistency: CacheConsistency,
    prefix_watch: bool,
}

#[async_trait]
impl ClusterCacheBackend for StubBackend {
    fn consistency(&self) -> CacheConsistency {
        self.consistency
    }
    fn features(&self) -> CacheFeatures {
        CacheFeatures::new(self.prefix_watch)
    }
    async fn get(&self, _key: &str) -> Result<Option<CacheEntry>, ClusterError> {
        Ok(None)
    }
    async fn put(&self, _req: PutRequest<'_>) -> Result<(), ClusterError> {
        Ok(())
    }
    async fn delete(&self, _key: &str) -> Result<bool, ClusterError> {
        Ok(false)
    }
    async fn contains(&self, _key: &str) -> Result<bool, ClusterError> {
        Ok(false)
    }
    async fn put_if_absent(
        &self,
        _req: PutRequest<'_>,
    ) -> Result<Option<CacheEntry>, ClusterError> {
        Ok(None)
    }
    async fn compare_and_swap(
        &self,
        _key: &str,
        _expected_version: u64,
        _new_value: &[u8],
        _ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        Ok(CacheEntry {
            value: Vec::new(),
            version: 1,
        })
    }
    async fn watch(&self, _key: &str) -> Result<CacheWatch, ClusterError> {
        let (_tx, watch) = CacheWatch::channel(1);
        Ok(watch)
    }
    async fn watch_prefix(&self, _prefix: &str) -> Result<CacheWatch, ClusterError> {
        // Honest declaration: track whatever `features()` advertises. A stub that
        // claims `prefix_watch` must actually return a watch, not `Unsupported`.
        if self.prefix_watch {
            let (_tx, watch) = CacheWatch::channel(1);
            Ok(watch)
        } else {
            Err(ClusterError::Unsupported {
                feature: "prefix_watch",
            })
        }
    }
}

#[derive(Clone, Copy)]
struct OrdersProfile;
impl ClusterProfile for OrdersProfile {
    const NAME: &'static str = "orders";
}

fn linearizable_backend() -> StubBackend {
    StubBackend {
        consistency: CacheConsistency::Linearizable,
        prefix_watch: true,
    }
}

#[tokio::test]
async fn resolve_without_profile_errors() {
    let hub = ClientHub::new();
    let result = CacheResolverBuilder::new(&hub).resolve().await;
    assert!(matches!(result, Err(ClusterError::ProfileNotSpecified)));
}

/// Registers a cluster client binding `backend` to the orders profile — since
/// `K4` the resolvers bind through `dyn ClusterClient`, not through a per-profile
/// hub registration (DESIGN.md).
fn client_with(hub: &ClientHub, backend: StubBackend) {
    StubClusterClient::for_profile(OrdersProfile::NAME)
        .with_cache(Arc::new(backend))
        .register(hub);
}

#[tokio::test]
async fn resolve_unbound_profile_errors() {
    let hub = ClientHub::new();
    // A client that binds a *different* profile: cluster is reachable and does
    // not bind this one, which DESIGN §4.7 classifies as a permanent config error
    // and returns from `resolve()` itself.
    StubClusterClient::for_profile("other")
        .with_cache(Arc::new(linearizable_backend()))
        .register(&hub);

    let result = ClusterCacheV1::resolver(&hub)
        .profile(OrdersProfile)
        .resolve()
        .await;
    assert!(matches!(
        result,
        Err(ClusterError::ProfileNotBound { profile: "orders" })
    ));
}

/// The nothing-wired case, at the facade rather than the backend: `resolve()`
/// succeeds and the first *call* reports it (§4.9.1). This is the misconfiguration
/// lazy binding would otherwise hide, so it is asserted here and not
/// only in the binding module's own tests.
#[tokio::test]
async fn resolve_with_nothing_wired_succeeds_and_the_first_call_reports_it() {
    let hub = ClientHub::new();

    // `with_nothing_derivable`, not a bare resolve: with `grpc-client` on, the resolve
    // path would otherwise self-construct a client from a `POD_NAMESPACE` a concurrent
    // test happens to have set, and this test would see a bound facade. See the
    // helper's docs.
    let Ok(cache) = with_nothing_derivable(
        ClusterCacheV1::resolver(&hub)
            .profile(OrdersProfile)
            .resolve(),
    )
    .await
    else {
        panic!("an empty hub must not fail resolution");
    };

    assert!(matches!(
        cache.get("k").await,
        Err(ClusterError::ProfileNotBound { profile: "orders" })
    ));
    // The weakest reading of every capability, so nothing a consumer branches on
    // is falsely satisfied.
    assert_eq!(cache.consistency(), CacheConsistency::EventuallyConsistent);
    assert!(!cache.features().prefix_watch);
}

#[tokio::test]
async fn resolve_happy_path_returns_facade() {
    let hub = ClientHub::new();
    client_with(&hub, linearizable_backend());

    let Ok(cache) = ClusterCacheV1::resolver(&hub)
        .profile(OrdersProfile)
        .require(CacheCapability::Linearizable)
        .resolve()
        .await
    else {
        panic!("resolution against a matching backend must succeed");
    };
    assert_eq!(cache.consistency(), CacheConsistency::Linearizable);
}

/// A consumer that requires `Watch` against a descriptor declaring no exact watch
/// (`watch: Some(false)`, e.g. redis `watch_mode: disabled`) is refused at
/// resolution — naming the operator-facing provider — rather than at the first
/// `watch()` call. The exact-watch twin of `resolve_rejects_capability_mismatch_at_startup`,
/// and like it the *descriptor* is authoritative, not the bound backend.
#[tokio::test]
async fn resolve_refuses_a_watch_requirement_against_a_watchless_descriptor() {
    let hub = ClientHub::new();
    let descriptor = ProfileDescriptor {
        name: OrdersProfile::NAME.to_owned(),
        cache: CacheDescriptor {
            consistency: WireCacheConsistency::Linearizable,
            features: WireCacheFeatures {
                prefix_watch: false,
                watch: Some(false),
            },
            provider: "redis".to_owned(),
        },
        lock: LockDescriptor {
            features: WireLockFeatures { linearizable: true },
            provider: "redis".to_owned(),
        },
        leader_election: LeaderElectionDescriptor {
            features: WireLeaderElectionFeatures { linearizable: true },
            provider: "redis".to_owned(),
        },
        health: ProfileHealth::Serving,
    };
    StubClusterClient::for_profile(OrdersProfile::NAME)
        // The backend can watch; the descriptor says the deployment cannot — and
        // the descriptor is what a required capability is checked against.
        .with_cache(Arc::new(linearizable_backend()))
        .with_descriptor(descriptor)
        .register(&hub);

    let Err(ClusterError::CapabilityNotMet {
        capability,
        provider,
        ..
    }) = ClusterCacheV1::resolver(&hub)
        .profile(OrdersProfile)
        .require(CacheCapability::Watch)
        .resolve()
        .await
    else {
        panic!("a Watch requirement must be refused against a watchless descriptor");
    };
    assert_eq!(capability, "Watch");
    // The operator-facing provider name, not `StubBackend`.
    assert_eq!(provider, "redis");
}

/// The success path of the same requirement: a descriptor that declares exact
/// watch resolves (`watch: Some(true)`; an absent bit decodes to supported too,
/// covered in `dto_tests`).
#[tokio::test]
async fn resolve_admits_a_watch_requirement_against_a_watchful_descriptor() {
    let hub = ClientHub::new();
    let descriptor = ProfileDescriptor {
        name: OrdersProfile::NAME.to_owned(),
        cache: CacheDescriptor {
            consistency: WireCacheConsistency::Linearizable,
            features: WireCacheFeatures {
                prefix_watch: false,
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
    };
    StubClusterClient::for_profile(OrdersProfile::NAME)
        .with_cache(Arc::new(linearizable_backend()))
        .with_descriptor(descriptor)
        .register(&hub);

    let Ok(cache) = ClusterCacheV1::resolver(&hub)
        .profile(OrdersProfile)
        .require(CacheCapability::Watch)
        .resolve()
        .await
    else {
        panic!("a Watch requirement must resolve against a watchful descriptor");
    };
    assert!(cache.features().watch);
}

#[tokio::test]
async fn resolve_rejects_capability_mismatch_at_startup() {
    let hub = ClientHub::new();
    client_with(
        &hub,
        StubBackend {
            consistency: CacheConsistency::EventuallyConsistent,
            prefix_watch: true,
        },
    );

    let result = ClusterCacheV1::resolver(&hub)
        .profile(OrdersProfile)
        .require(CacheCapability::Linearizable)
        .resolve()
        .await;
    assert!(matches!(
        result,
        Err(ClusterError::CapabilityNotMet {
            capability: "Linearizable",
            ..
        })
    ));
}

/// What validation reads is the **descriptor**, not the bound backend — which is
/// what lets a remote consumer validate at all, and what makes the diagnostic
/// byte-identical across deployment profiles (DESIGN.md).
///
/// The two are deliberately in conflict here: a backend that declares
/// `Linearizable` behind a descriptor that does not. Only reading the descriptor
/// can fail this, and only reading the descriptor can name the operator's
/// provider rather than the Rust type.
#[tokio::test]
async fn validation_reads_the_descriptor_rather_than_the_backend() {
    let hub = ClientHub::new();
    let descriptor = ProfileDescriptor {
        name: OrdersProfile::NAME.to_owned(),
        cache: CacheDescriptor {
            consistency: WireCacheConsistency::EventuallyConsistent,
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
    };
    StubClusterClient::for_profile(OrdersProfile::NAME)
        // The backend says linearizable; the server-side descriptor says it is not.
        .with_cache(Arc::new(linearizable_backend()))
        .with_descriptor(descriptor)
        .register(&hub);

    let Err(ClusterError::CapabilityNotMet {
        capability,
        provider,
        ..
    }) = ClusterCacheV1::resolver(&hub)
        .profile(OrdersProfile)
        .require(CacheCapability::Linearizable)
        .resolve()
        .await
    else {
        panic!("the descriptor is what a declared capability is checked against");
    };
    assert_eq!(capability, "Linearizable");
    // The operator-facing provider name, not `StubBackend`.
    assert_eq!(provider, "postgres");
}

/// Validation is inline whenever the descriptor is in hand, and deferred when it
/// is not — the two rows of §4.7.1's table, over the same unmet requirement.
#[tokio::test]
async fn an_unreachable_descriptor_defers_the_capability_check() {
    let hub = ClientHub::new();
    StubClusterClient::for_profile(OrdersProfile::NAME)
        .with_cache(Arc::new(StubBackend {
            consistency: CacheConsistency::EventuallyConsistent,
            prefix_watch: true,
        }))
        .without_descriptor()
        .register(&hub);

    let result = ClusterCacheV1::resolver(&hub)
        .profile(OrdersProfile)
        .require(CacheCapability::Linearizable)
        .resolve()
        .await;
    assert!(
        result.is_ok(),
        "with no descriptor there is nothing to validate against, so the check moves to readiness"
    );
}
