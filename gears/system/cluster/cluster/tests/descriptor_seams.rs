//! `RemoteClusterClient`'s descriptor cache, seen from a consumer's side.
//!
//! Two properties: what a client does when the server's `generation` moves
//! backwards, and what live handles report while a refresh is in flight. Both are
//! consumer-visible, so both are driven through the public client against a real
//! socket. The server here is a **scriptable** `ClusterProfileApi` rather than
//! the gear's own: what these tests need to vary is precisely the two things the
//! real service computes rather than accepts — the `generation` it reports, and
//! how long it takes to answer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]
#![allow(
    clippy::use_debug,
    reason = "these tests print the observed transcript; the `Debug` form is \
              the transcript"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use cluster_sdk::dto::{
    CacheDescriptor, LeaderElectionDescriptor, LockDescriptor, ProfileDescriptor, ProfileHealth,
    WireCacheConsistency, WireCacheFeatures, WireLeaderElectionFeatures, WireLockFeatures,
};
use cluster_sdk::grpc::stubs::profile as stubs;
use cluster_sdk::{CacheConsistency, CacheFeatures, ClusterClient, RemoteClusterClient};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

const PROFILE: &str = "orders";

/// A `Linearizable` / `postgres` descriptor for `PROFILE`, so a downgrade is
/// visible in every one of the three sync accessors at once.
fn strong_descriptor() -> ProfileDescriptor {
    ProfileDescriptor {
        name: PROFILE.to_owned(),
        cache: CacheDescriptor {
            consistency: WireCacheConsistency::Linearizable,
            // `WireCacheFeatures` is `#[non_exhaustive]`, so an out-of-crate
            // caller builds it through the domain `From` rather than a struct
            // literal. `new(true)` → exact watch + prefix watch.
            features: WireCacheFeatures::from(CacheFeatures::new(true)),
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
    }
}

/// What the fake server answers with, mutable between calls.
#[derive(Debug, Default)]
struct Script {
    /// The `ProfileRegistry` generation to report.
    generation: AtomicU64,
    /// Whether the profile set is non-empty. `false` models a drained pod, which
    /// is what the gear's `stop` publishes before teardown (DESIGN section 4.8).
    binds_profile: AtomicBool,
    /// How long to take before answering, in milliseconds.
    delay_ms: AtomicU64,
    /// How many `DescribeProfiles` calls have been served.
    calls: AtomicU64,
}

impl Script {
    fn set(&self, generation: u64, binds_profile: bool) {
        self.generation.store(generation, Ordering::SeqCst);
        self.binds_profile.store(binds_profile, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone)]
struct ScriptedProfileService(Arc<Script>);

#[tonic::async_trait]
impl stubs::cluster_profile_api_server::ClusterProfileApi for ScriptedProfileService {
    async fn describe_profiles(
        &self,
        _request: Request<stubs::DescribeProfilesRequest>,
    ) -> Result<Response<stubs::DescribeProfilesResponse>, Status> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        let delay = self.0.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let profiles = if self.0.binds_profile.load(Ordering::SeqCst) {
            vec![strong_descriptor()]
        } else {
            Vec::new()
        };
        Ok(Response::new(stubs::DescribeProfilesResponse::from(
            cluster_sdk::dto::DescribeProfilesResponse {
                profiles,
                generation: self.0.generation.load(Ordering::SeqCst),
            },
        )))
    }
}

/// A running scripted profile server plus a lazy client pointed at it.
struct Fixture {
    client: RemoteClusterClient,
    script: Arc<Script>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    stopped: tokio::sync::oneshot::Receiver<()>,
}

impl Fixture {
    async fn start() -> Self {
        let script = Arc::new(Script::default());
        let service = ScriptedProfileService(Arc::clone(&script));

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let addr = listener.local_addr().expect("has an address");
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let (stopped_tx, stopped) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    stubs::cluster_profile_api_server::ClusterProfileApiServer::new(service),
                )
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _stopped = shutdown_rx.await;
                })
                .await
                .expect("the server runs");
            let _done = stopped_tx.send(());
        });

        let client = RemoteClusterClient::connect_lazy(&format!("http://{addr}"), None)
            .expect("a valid endpoint");

        Self {
            client,
            script,
            shutdown,
            stopped,
        }
    }

    /// Stops the server and waits for the socket to be released, so a later call
    /// is a connection failure rather than a race.
    async fn kill_server(self) -> (RemoteClusterClient, Arc<Script>) {
        let _sent = self.shutdown.send(());
        let _down = tokio::time::timeout(Duration::from_secs(5), self.stopped).await;
        (self.client, self.script)
    }
}

// The generation guard treats a per-process counter as a global epoch

#[tokio::test]
async fn a_client_wedges_permanently_when_the_server_generation_regresses() {
    // The rolling restart. A consumer's readiness poll reads a
    // *draining* pod, which published the empty set at its own generation 2
    // (`gear.rs` stop -> `profiles.clear()`, DESIGN section 4.8). The channel then
    // reconnects to a fresh pod whose `ProfileRegistry::new()` started at 0 and
    // whose first publish is generation 1. `generation` is per process, so the
    // healthy pod's answer is *older* by the cache's reckoning and is dropped.
    let fixture = Fixture::start().await;

    // The draining pod: generation 2, nothing bound.
    fixture.script.set(2, false);
    let _drained = fixture.client.refresh_descriptors().await;

    // The replacement pod: generation 1, and it binds `orders`.
    fixture.script.set(1, true);

    fixture
        .client
        .refresh_descriptors()
        .await
        .expect("the healthy pod answers");
    println!(
        "after 1 refresh against the healthy pod: {:?}",
        fixture.client.descriptor(PROFILE).await
    );
    let cache = fixture.client.cache_backend(PROFILE).expect("a handle");
    println!("consistency(): {:?}", cache.consistency());
    println!("provider_name(): {:?}", cache.provider_name());

    // Ten more intervals -- long enough that "wedged" means permanently.
    for _ in 0..10 {
        let _refreshed = fixture.client.refresh_descriptors().await;
    }
    println!(
        "after 11 refreshes: {:?}",
        fixture.client.descriptor(PROFILE).await
    );

    assert!(
        fixture.client.descriptor(PROFILE).await.is_ok(),
        "a healthy server binding `{PROFILE}` must eventually describe it"
    );
    assert_eq!(
        cache.consistency(),
        CacheConsistency::Linearizable,
        "and the live handle must answer with the healthy pod's descriptor"
    );
}

#[tokio::test]
async fn the_same_sequence_without_a_regression_recovers() {
    // The control: byte-for-byte the sequence above except that the replacement
    // pod's generation happens to exceed the drained one. This passes before the
    // fix and must keep passing after it -- it is what pins the failure above to
    // the generation guard rather than to the drain.
    let fixture = Fixture::start().await;

    fixture.script.set(2, false);
    let _drained = fixture.client.refresh_descriptors().await;

    fixture.script.set(3, true);
    fixture
        .client
        .refresh_descriptors()
        .await
        .expect("the healthy pod answers");

    assert!(
        fixture.client.descriptor(PROFILE).await.is_ok(),
        "a healthy server binding `{PROFILE}` must describe it"
    );
}

// A refresh downgrades every live handle's capability answers

#[tokio::test]
async fn a_failed_refresh_downgrades_live_handles() {
    // ADR-011 accepts the fail-safe answer *once*, at cold start, because "no
    // consumer respecting /readyz observes it". The readiness contributor drives
    // this refresh every 10 s on a pod already in rotation, so a single failed
    // poll rewrites what every live handle answers -- and keeps it rewritten for
    // as long as the cluster gear is unreachable.
    let fixture = Fixture::start().await;
    fixture.script.set(1, true);
    fixture
        .client
        .refresh_descriptors()
        .await
        .expect("the server answers");

    let cache = fixture.client.cache_backend(PROFILE).expect("a handle");
    let lock = fixture.client.lock_backend(PROFILE).expect("a handle");
    println!(
        "before: consistency={:?} provider={} lock_linearizable={}",
        cache.consistency(),
        cache.provider_name(),
        lock.features().linearizable
    );
    assert_eq!(cache.consistency(), CacheConsistency::Linearizable);

    let (client, _script) = fixture.kill_server().await;
    let err = client
        .refresh_descriptors()
        .await
        .expect_err("the server is gone");
    println!("refresh while unreachable: {err:?}");
    println!(
        "after:  consistency={:?} provider={} lock_linearizable={}",
        cache.consistency(),
        cache.provider_name(),
        lock.features().linearizable
    );

    assert_eq!(
        cache.consistency(),
        CacheConsistency::Linearizable,
        "a refresh that failed must not rewrite what a live handle answers"
    );
    assert_eq!(cache.provider_name(), "postgres");
    assert!(lock.features().linearizable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_refresh_window_downgrades_handles_even_when_it_succeeds() {
    // Not only the failure path: `invalidate()` then `fetch_all_descriptors()`
    // opens a window one whole RTT wide in which a consumer branching on
    // `cache.consistency()` reads a weaker guarantee than the profile provides.
    // The server is made deliberately slow so the window is observed rather than
    // raced for.
    let fixture = Fixture::start().await;
    fixture.script.set(1, true);
    fixture
        .client
        .refresh_descriptors()
        .await
        .expect("the server answers");

    let cache = fixture.client.cache_backend(PROFILE).expect("a handle");
    assert_eq!(cache.consistency(), CacheConsistency::Linearizable);

    fixture.script.delay_ms.store(300, Ordering::SeqCst);
    let client = fixture.client.clone();
    let refresh = tokio::spawn(async move { client.refresh_descriptors().await });

    // Sample the accessors across the whole in-flight window.
    let mut downgrades = 0_u32;
    let mut observed = None;
    for _ in 0..60 {
        if cache.consistency() != CacheConsistency::Linearizable {
            downgrades += 1;
            observed = Some((cache.consistency(), cache.provider_name()));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    refresh
        .await
        .expect("the task joins")
        .expect("the refresh succeeds");

    if let Some((consistency, provider)) = observed {
        println!("during a SUCCESSFUL refresh: consistency={consistency:?} provider={provider}");
    }
    println!("after:  consistency={:?}", cache.consistency());

    assert_eq!(
        downgrades, 0,
        "a successful refresh must never expose a weaker capability answer \
         ({downgrades} samples saw one)"
    );
    assert_eq!(cache.consistency(), CacheConsistency::Linearizable);
}
