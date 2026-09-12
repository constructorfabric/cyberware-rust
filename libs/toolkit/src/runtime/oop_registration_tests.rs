//! Tests for `OoP` self-registration and dependency resolution.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use cf_system_sdks::directory::{
    DirectoryInvalidArgument, DirectoryPermissionDenied, ServiceEndpoint, ServiceInstanceInfo,
};

/// The permanent rejection a [`StubDirectory`] returns from `register`.
#[derive(Clone, Copy)]
enum Rejection {
    /// A mis-configured label / endpoint URI (gRPC `InvalidArgument`).
    InvalidArgument,
    /// An authorization denial or gRPC service-name conflict (gRPC
    /// `PermissionDenied` / `Unauthenticated`).
    PermissionDenied,
}

/// Stub directory: fails `register`/`resolve` a configurable number of times,
/// then succeeds; records register calls and heartbeats. While `rejecting` is
/// set, `register` always returns the permanent `reject_kind` rejection;
/// [`stop_rejecting`](StubDirectory::stop_rejecting) clears it to model the
/// conflict later resolving.
struct StubDirectory {
    fail_register: AtomicUsize,
    register_calls: AtomicUsize,
    heartbeats: AtomicUsize,
    fail_resolve: AtomicUsize,
    reject_kind: Option<Rejection>,
    rejecting: AtomicBool,
}

impl StubDirectory {
    fn new(fail_register: usize, fail_resolve: usize) -> Self {
        Self {
            fail_register: AtomicUsize::new(fail_register),
            register_calls: AtomicUsize::new(0),
            heartbeats: AtomicUsize::new(0),
            fail_resolve: AtomicUsize::new(fail_resolve),
            reject_kind: None,
            rejecting: AtomicBool::new(false),
        }
    }

    /// A directory that permanently rejects every registration with `kind` (as
    /// the gRPC front-end does for a mis-configured label / endpoint URI, or an
    /// authorization denial / service-name conflict).
    fn rejecting(kind: Rejection) -> Self {
        Self {
            reject_kind: Some(kind),
            rejecting: AtomicBool::new(true),
            ..Self::new(0, 0)
        }
    }

    /// Stop rejecting: subsequent registrations succeed, modelling the
    /// conflicting owner deregistering (or an authz fix landing).
    fn stop_rejecting(&self) {
        self.rejecting.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl DirectoryClient for StubDirectory {
    async fn resolve_grpc_service(&self, _service: &str) -> anyhow::Result<ServiceEndpoint> {
        Ok(ServiceEndpoint::new("http://grpc"))
    }

    async fn resolve_rest_service(&self, gear: &str) -> anyhow::Result<ServiceEndpoint> {
        if self.fail_resolve.load(Ordering::SeqCst) > 0 {
            self.fail_resolve.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("not yet available");
        }
        Ok(ServiceEndpoint::new(format!("http://{gear}:8080")))
    }

    async fn get_openapi_spec(&self, _gear: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn list_instances(&self, _gear: &str) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
        Ok(vec![])
    }

    async fn list_all_instances(&self) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
        Ok(vec![])
    }

    async fn register_instance(&self, _info: RegisterInstanceInfo) -> anyhow::Result<()> {
        self.register_calls.fetch_add(1, Ordering::SeqCst);
        if self.rejecting.load(Ordering::SeqCst) {
            match self.reject_kind {
                Some(Rejection::InvalidArgument) => {
                    return Err(DirectoryInvalidArgument::new("bad label").into());
                }
                Some(Rejection::PermissionDenied) => {
                    return Err(DirectoryPermissionDenied::new("peer not authorized").into());
                }
                None => {}
            }
        }
        if self.fail_register.load(Ordering::SeqCst) > 0 {
            self.fail_register.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("directory unavailable");
        }
        Ok(())
    }

    async fn deregister_instance(&self, _gear: &str, _instance: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn send_heartbeat(&self, _gear: &str, _instance: &str) -> anyhow::Result<()> {
        self.heartbeats.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn info() -> RegisterInstanceInfo {
    RegisterInstanceInfo::new("billing", "i-1")
        .with_version("1.0.0")
        .with_rest_endpoint(ServiceEndpoint::new("http://billing:8080"))
        .with_openapi_spec("{}")
}

#[test]
fn backoff_doubles_and_caps() {
    assert_eq!(
        next_backoff(Duration::from_millis(100)),
        Duration::from_millis(200)
    );
    assert_eq!(
        next_backoff(Duration::from_millis(200)),
        Duration::from_millis(400)
    );
    assert_eq!(next_backoff(Duration::from_secs(20)), MAX_BACKOFF);
    assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
}

#[tokio::test]
async fn registration_retries_until_success() {
    let directory: Arc<dyn DirectoryClient> = Arc::new(StubDirectory::new(2, 0));
    let cancel = CancellationToken::new();
    let ok = register_once_with_backoff(&directory, &info(), &cancel).await;
    assert!(ok);
}

#[tokio::test]
async fn registration_keeps_loop_alive_on_permanent_rejection() {
    // A DirectoryInvalidArgument is permanent: the backoff retry must give up
    // after a single attempt rather than retrying a request that can never
    // succeed, yet still return `true` so the presence loop keeps running.
    let stub = Arc::new(StubDirectory::rejecting(Rejection::InvalidArgument));
    let directory: Arc<dyn DirectoryClient> = stub.clone();
    let cancel = CancellationToken::new();
    let ok = register_once_with_backoff(&directory, &info(), &cancel).await;
    assert!(ok, "a permanent rejection must not stop the presence loop");
    assert_eq!(
        stub.register_calls.load(Ordering::SeqCst),
        1,
        "a permanent rejection must not be retried"
    );
}

#[tokio::test]
async fn registration_does_not_retry_permission_denied() {
    // A DirectoryPermissionDenied (authorization denial / service-name conflict)
    // is just as permanent as an invalid argument: retrying the identical
    // request can never turn the "no" into a "yes", so the backoff retry must
    // give up after a single attempt (regression: it previously fell into the
    // generic transient branch and retried forever at `warn!`), yet still keep
    // the presence loop alive.
    let stub = Arc::new(StubDirectory::rejecting(Rejection::PermissionDenied));
    let directory: Arc<dyn DirectoryClient> = stub.clone();
    let cancel = CancellationToken::new();
    let ok = register_once_with_backoff(&directory, &info(), &cancel).await;
    assert!(ok, "a permanent rejection must not stop the presence loop");
    assert_eq!(
        stub.register_calls.load(Ordering::SeqCst),
        1,
        "a permission-denied rejection must not be retried"
    );
}

#[tokio::test]
async fn registration_returns_false_when_cancelled() {
    let directory: Arc<dyn DirectoryClient> = Arc::new(StubDirectory::new(1000, 0));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ok = register_once_with_backoff(&directory, &info(), &cancel).await;
    assert!(!ok, "cancelled registration must return false");
}

#[tokio::test]
async fn presence_loop_registers_once_then_heartbeats_until_cancel() {
    let stub = Arc::new(StubDirectory::new(0, 0));
    let directory: Arc<dyn DirectoryClient> = stub.clone();
    let cancel = CancellationToken::new();

    // 1s is the minimum interval presence_loop clamps to; wait past one tick so
    // at least one heartbeat fires.
    let task = tokio::spawn(presence_loop(
        Arc::clone(&directory),
        info(),
        Duration::from_secs(1),
        cancel.clone(),
    ));

    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert_eq!(
        stub.register_calls.load(Ordering::SeqCst),
        1,
        "steady state registers exactly once (self-heal re-register is 30s, not fired here)"
    );
    assert!(
        stub.heartbeats.load(Ordering::SeqCst) >= 1,
        "the single presence task must send heartbeats"
    );

    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn presence_loop_survives_permanent_rejection() {
    // A permanent DirectoryInvalidArgument must NOT tear the presence loop down:
    // it keeps heartbeating and periodically retrying re-registration. Only
    // cancellation stops it.
    let stub = Arc::new(StubDirectory::rejecting(Rejection::InvalidArgument));
    let directory: Arc<dyn DirectoryClient> = stub.clone();
    let cancel = CancellationToken::new();

    let task = tokio::spawn(presence_loop(
        Arc::clone(&directory),
        info(),
        Duration::from_secs(1),
        cancel.clone(),
    ));

    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        !task.is_finished(),
        "a permanent rejection must not stop the presence loop"
    );
    assert!(
        stub.heartbeats.load(Ordering::SeqCst) >= 1,
        "the loop must keep heartbeating despite a permanent rejection"
    );

    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn presence_loop_recovers_when_rejection_clears() {
    // A gear denied on a gRPC service-name conflict keeps its presence loop
    // alive and re-registers every RE_REGISTER_INTERVAL. Once the conflict
    // clears (the owning gear deregisters) the next re-registration succeeds, so
    // the instance becomes discoverable without a restart. Driven on virtual
    // time so the 30s interval costs no wall clock.
    let stub = Arc::new(StubDirectory::rejecting(Rejection::PermissionDenied));
    let directory: Arc<dyn DirectoryClient> = stub.clone();
    let cancel = CancellationToken::new();

    let task = tokio::spawn(presence_loop(
        Arc::clone(&directory),
        info(),
        Duration::from_secs(1),
        cancel.clone(),
    ));

    // Let the initial (rejected) registration run and the loop park on its
    // timers. No time has advanced, so only the one attempt has happened.
    for _ in 0..10 {
        if stub.register_calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        stub.register_calls.load(Ordering::SeqCst),
        1,
        "the initial registration is rejected but the loop stays alive"
    );

    // The conflicting owner deregisters; the directory now accepts.
    stub.stop_rejecting();

    // Advance past the re-registration interval so the periodic re-register
    // fires — this time it succeeds.
    tokio::time::advance(RE_REGISTER_INTERVAL + Duration::from_secs(1)).await;
    for _ in 0..10 {
        if stub.register_calls.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        stub.register_calls.load(Ordering::SeqCst) >= 2,
        "the loop must re-register after the interval and recover once the conflict clears"
    );

    cancel.cancel();
    task.await.unwrap();
}

// Dependency resolution moved to the proxy-wiring phase (typed
// `#[toolkit::consumes]` clients feeding the shared `DependencyChecker`); the
// former `resolve_one_dep`/`resolve_deps`/`ResolvedRestEndpoints` stopgap and
// its tests were retired. Consumer-wiring resolution is covered by
// `host_runtime` proxy-wiring tests and the api-contracts example E2E tests.
