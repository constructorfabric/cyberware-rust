//! Unit tests for the cluster-backed [`LockManager`].

use std::sync::Arc;
use std::time::Duration;

use cluster::defaults::CasBasedDistributedLockBackend;
use cluster_sdk::profile::ClusterProfile;
use cluster_sdk::register_lock_backend;
use standalone_cluster_plugin::StandaloneCache;
use toolkit::client_hub::ClientHub;

use super::{LockManager, UsageCollectorProfile};
use crate::infra::metrics::{LockMode, Metrics};

fn hub_with_lock() -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    let cache = StandaloneCache::new();
    let backend = CasBasedDistributedLockBackend::new(cache).expect("linearizable cache");
    register_lock_backend(&hub, UsageCollectorProfile::NAME, Arc::new(backend))
        .expect("register lock backend");
    hub
}

fn manager(hub: Arc<ClientHub>) -> Arc<LockManager> {
    manager_with_ttl(hub, Duration::from_secs(30))
}

fn manager_with_ttl(hub: Arc<ClientHub>, ttl: Duration) -> Arc<LockManager> {
    Arc::new(LockManager::new(
        hub,
        ttl,
        Duration::from_secs(2),
        Arc::new(Metrics::new()),
    ))
}

#[test]
fn lock_leaf_is_cluster_valid_and_stable() {
    let a = LockManager::lock_leaf("gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~x");
    let b = LockManager::lock_leaf("gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~x");
    let c = LockManager::lock_leaf("gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~y");
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert!(a.starts_with("gts-"));
    assert!(
        a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    );
}

#[tokio::test]
async fn acquire_and_release_round_trip() {
    let mgr = manager(hub_with_lock());
    let guard = mgr
        .acquire(
            "gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~test",
            LockMode::Create,
        )
        .await
        .expect("acquire");
    guard.ensure_still_held().await.expect("renew");
    guard.release().await.expect("release");
}

#[tokio::test]
async fn create_and_delete_modes_share_mutex() {
    let mgr = manager(hub_with_lock());
    let gts = "gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~shared-mutex";
    let create_guard = mgr.acquire_for_create(gts).await.expect("create acquire");

    let mgr2 = Arc::clone(&mgr);
    let gts2 = gts.to_owned();
    let contested = tokio::spawn(async move { mgr2.acquire_for_delete(&gts2).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    create_guard.release().await.expect("release create");

    let delete_guard = tokio::time::timeout(Duration::from_secs(2), contested)
        .await
        .expect("join")
        .expect("task")
        .expect("delete acquire after create release");
    delete_guard.release().await.expect("release delete");
}

#[tokio::test]
async fn acquire_fails_closed_when_profile_unbound() {
    let hub = Arc::new(ClientHub::default());
    let mgr = manager(hub);
    let result = mgr.acquire_for_create("gts.cf.x~y~z").await;
    assert!(result.is_err(), "unbound profile must fail closed, got Ok");
    let err = result.err().unwrap();
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("transient")
            || msg.contains("unavailable")
            || msg.contains("ProfileNotBound")
            || matches!(
                err,
                usage_collector_sdk::UsageCollectorPluginError::Transient { .. }
            ),
        "expected Transient-ish error, got {err:?}"
    );
}

#[test]
fn map_acquire_error_classifies_transient_and_internal_arms() {
    use std::time::Duration;

    use cluster_sdk::error::{ClusterError, ProviderErrorKind};

    use super::{map_acquire_error, map_renew_error, map_resolve_error};

    let transient_acquire = [
        ClusterError::LockTimeout {
            name: "n".to_owned(),
            waited: Duration::from_secs(1),
        },
        ClusterError::LockContended {
            name: "n".to_owned(),
        },
        ClusterError::ProfileNotBound { profile: "p" },
        ClusterError::Shutdown,
        ClusterError::Provider {
            kind: ProviderErrorKind::Timeout,
            message: "boom".to_owned(),
        },
        ClusterError::Unsupported { feature: "x" },
    ];
    for err in transient_acquire {
        assert!(
            matches!(
                map_acquire_error(err),
                usage_collector_sdk::UsageCollectorPluginError::Transient { .. }
            ),
            "expected Transient"
        );
    }

    assert!(matches!(
        map_acquire_error(ClusterError::InvalidName {
            name: "bad".to_owned(),
            reason: "rule",
        }),
        usage_collector_sdk::UsageCollectorPluginError::Internal(_)
    ));

    let renew_cases = [
        ClusterError::LockExpired {
            name: "n".to_owned(),
        },
        ClusterError::Provider {
            kind: ProviderErrorKind::ConnectionLost,
            message: "gone".to_owned(),
        },
        ClusterError::Shutdown,
        ClusterError::Unsupported { feature: "y" },
    ];
    for err in renew_cases {
        assert!(matches!(
            map_renew_error(err),
            usage_collector_sdk::UsageCollectorPluginError::Transient { .. }
        ));
    }

    assert!(matches!(
        map_resolve_error(ClusterError::ProfileNotSpecified),
        usage_collector_sdk::UsageCollectorPluginError::Transient { .. }
    ));
    assert!(matches!(
        map_resolve_error(ClusterError::Unsupported { feature: "z" }),
        usage_collector_sdk::UsageCollectorPluginError::Internal(_)
    ));
}

/// A guard dropped without an explicit `release` must still free the lock name
/// promptly, rather than leaving it held until the TTL lapses.
///
/// The cluster `LockGuard`'s own drop is a no-op, so `ClusterLockGuard::drop`
/// spawns the release itself. Without that, a caller that returns early on an
/// error path would block every subsequent create/delete for the same `gts_id`
/// for a full TTL.
#[tokio::test]
async fn dropping_a_guard_without_release_still_frees_the_lock() {
    let gts = "gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~forgotten-release";
    // A long TTL so a re-acquire can only succeed via the drop-spawned release,
    // never by waiting the lease out.
    let mgr = manager_with_ttl(hub_with_lock(), Duration::from_mins(5));

    drop(mgr.acquire_for_create(gts).await.expect("first acquire"));

    let regained = mgr
        .acquire_for_delete(gts)
        .await
        .expect("the lock must be re-acquirable after the guard was dropped");
    regained.release().await.expect("release");
}

/// Once the lease lapses, `ensure_still_held` fails as `Transient` — the caller
/// must retry rather than proceed with a write it can no longer serialize.
#[tokio::test]
async fn ensure_still_held_fails_transient_after_the_lease_lapses() {
    use usage_collector_sdk::UsageCollectorPluginError;

    let mgr = manager_with_ttl(hub_with_lock(), Duration::from_millis(50));
    let guard = mgr
        .acquire_for_create("gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~lapsed-lease")
        .await
        .expect("acquire");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let err = guard
        .ensure_still_held()
        .await
        .expect_err("a lapsed lease must not report itself as still held");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "a lapsed lease is retryable, got {err:?}"
    );

    // Releasing afterwards is still `Ok`: the backend's release is idempotent
    // for a lease that already lapsed, so the caller's mandatory release on
    // every exit path does not turn a lapsed lease into a second error.
    guard
        .release()
        .await
        .expect("releasing an already-lapsed lease is a no-op, not an error");
}

#[tokio::test]
async fn lock_guard_port_shims_delegate() {
    use super::LockGuardPort;

    let mgr = manager(hub_with_lock());
    let guard = mgr
        .acquire_for_create("gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~port-shim")
        .await
        .expect("acquire");
    let boxed: Box<dyn LockGuardPort> = Box::new(guard);
    boxed.ensure_still_held().await.expect("renew via port");
    boxed.release().await.expect("release via port");
}
