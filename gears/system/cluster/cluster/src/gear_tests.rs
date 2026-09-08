//! End-to-end lifecycle test for the `cluster` gear: drive `init` → `start` →
//! `stop` through a mock `GearCtx` and assert backends register under the
//! configured profile and unbind on stop.
//!
//! Also `S3`'s capability set. Its exit criterion — that neither
//! `get_grpc_services` nor `healthcheck()` captures a backend — is asserted the
//! only way that can actually fail: both are collected **before** `start`, then
//! the *same* server and the *same* check are observed changing answer once
//! `start` publishes the registry under them.

use std::sync::Arc;
use std::time::Duration;

use cluster_sdk::grpc::stubs;
use cluster_sdk::{
    ClusterCacheBackend, ClusterCacheV1, ClusterClient, ClusterError, ClusterProfile,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::contracts::{GrpcServiceCapability, RestApiCapability, RunnableCapability};
use toolkit::{ConfigProvider, Gear, GearCtx, HealthcheckStatus};

use super::ClusterGear;

/// Returns the `cluster` gear's config entry for `cluster`, and nothing else.
struct MockConfig(serde_json::Value);

impl ConfigProvider for MockConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
        (gear_name == "cluster").then_some(&self.0)
    }
}

#[derive(Clone, Copy)]
struct DefaultProfile;
impl ClusterProfile for DefaultProfile {
    const NAME: &'static str = "default";
}

/// A `GearCtx` carrying one standalone-cache profile named `default`.
fn ctx_with_default_profile(hub: &Arc<ClientHub>) -> GearCtx {
    let provider = Arc::new(MockConfig(serde_json::json!({
        "config": { "profiles": { "default": { "cache": { "provider": "standalone" } } } }
    })));
    GearCtx::new(
        "cluster",
        uuid::Uuid::new_v4(),
        provider,
        Arc::clone(hub),
        CancellationToken::new(),
    )
}

#[tokio::test]
async fn gear_lifecycle_registers_then_unbinds() {
    let hub = Arc::new(ClientHub::default());
    // The provider returns the gear entry; `ctx.config()` reads its `config` field.
    let provider = Arc::new(MockConfig(serde_json::json!({
        "config": { "profiles": { "default": { "cache": { "provider": "standalone" } } } }
    })));
    let ctx = GearCtx::new(
        "cluster",
        uuid::Uuid::new_v4(),
        provider,
        Arc::clone(&hub),
        CancellationToken::new(),
    );

    let gear = ClusterGear::default();
    gear.init(&ctx)
        .await
        .expect("init parses config and captures the hub");

    // The registry exists after `init` and is empty: the gear's services and
    // healthcheck are collected in this window, so they capture it rather than a
    // backend, and anything that resolves through it here is refused
    // (DESIGN §4.2, §5.2).
    let profiles = Arc::clone(gear.profiles.get().expect("init creates the registry"));
    assert_eq!(profiles.generation(), 0, "init publishes nothing");
    assert!(matches!(
        profiles.resolve("default"),
        Err(ClusterError::ProfileNotBound { .. })
    ));

    gear.start(CancellationToken::new())
        .await
        .expect("start wires backends from config");

    // The configured cache (and the omit-default trio over it) resolves.
    assert!(
        ClusterCacheV1::resolver(&hub)
            .profile(DefaultProfile)
            .resolve()
            .await
            .is_ok(),
        "the standalone cache is registered for the `default` profile"
    );
    // ...and the same profile is now addressable by name through the registry,
    // which the wire services and `LocalClusterClient` dispatch through.
    assert_eq!(profiles.generation(), 1, "start publishes the bound set");
    let bound = profiles
        .resolve("default")
        .expect("the configured profile is addressable after start");
    assert_eq!(bound.descriptor().cache.provider, "standalone");

    gear.stop(CancellationToken::new())
        .await
        .expect("stop tears the wiring down");

    assert!(
        matches!(
            ClusterCacheV1::resolver(&hub)
                .profile(DefaultProfile)
                .resolve()
                .await,
            Err(ClusterError::ProfileNotBound { .. })
        ),
        "stop deregisters the profile's backends"
    );
    assert!(
        matches!(
            profiles.resolve("default"),
            Err(ClusterError::ProfileNotBound { .. })
        ),
        "and the registry no longer routes to the backends being torn down"
    );
    assert_eq!(
        profiles.generation(),
        2,
        "the clearing swap is a generation"
    );
}

#[tokio::test(start_paused = true)]
async fn start_runs_the_subscription_sweep_and_stop_ends_it() {
    // Item `S2` / section 5.4.1's production wiring, which the table's own tests
    // and the integration tests both step around - they call the sweep directly
    // or spawn their own. What is asserted here is that the *shipped gear*
    // spawns one on its own cadence and cancels it, because a sweep nothing
    // starts is a table that grows forever with every test still green.
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();

    gear.init(&ctx).await.expect("init");
    let subscriptions = gear.subscriptions().expect("init created the table");

    // Abandoned from the moment it is opened: `join` registers, `await_change`
    // never comes. This is the follower pump's entry.
    let _abandoned = subscriptions.open("event-broker", "ledger", "default");
    assert_eq!(subscriptions.len(), 1);

    gear.start(CancellationToken::new()).await.expect("start");

    // Nothing yet: the entry is inside its grace window, and a sweep that reaped
    // on the first pass would close the `join`-to-`await_change` window.
    tokio::time::advance(crate::api::grpc::SWEEP_INTERVAL).await;
    tokio::task::yield_now().await;
    assert_eq!(
        subscriptions.len(),
        1,
        "reaped inside its grace window, which no client could survive"
    );

    tokio::time::advance(crate::api::grpc::sweep_grace(
        crate::api::grpc::SWEEP_INTERVAL,
    ))
    .await;
    tokio::task::yield_now().await;
    assert!(
        subscriptions.is_empty(),
        "the gear's own sweep must reap an abandoned subscription past its window"
    );

    gear.stop(CancellationToken::new()).await.expect("stop");

    // And it is gone: a fresh abandoned entry outlives every window there is.
    let _after_stop = subscriptions.open("event-broker", "ledger", "default");
    tokio::time::advance(crate::api::grpc::SWEEP_INTERVAL * 20).await;
    tokio::task::yield_now().await;
    assert_eq!(
        subscriptions.len(),
        1,
        "the sweep must not outlive the gear that spawned it"
    );
}

#[tokio::test(start_paused = true)]
async fn the_sweep_runs_again_after_a_stop_then_start_cycle() {
    // Item `S2` / M7's regression. The sweep token was created once in `Default`
    // and cancelled in `stop`, so a second `start` handed the sweep an
    // already-cancelled token: its `select!` broke on the first tick and it
    // reaped nothing. A fresh child token per `start` is what brings the sweep
    // back to life after a stop→start cycle.
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();

    gear.init(&ctx).await.expect("init");
    let subscriptions = gear.subscriptions().expect("init created the table");

    // First lifecycle: start then stop. `stop` cancels and joins this start's
    // sweep — the whole point being that it does not poison the next one.
    gear.start(CancellationToken::new())
        .await
        .expect("first start");
    gear.stop(CancellationToken::new())
        .await
        .expect("first stop");

    // Second start: the sweep must come back to life.
    gear.start(CancellationToken::new())
        .await
        .expect("second start");

    // Abandoned from the moment it is opened, after the second start so the first
    // stop's `close_all` cannot claim it.
    let _abandoned = subscriptions.open("event-broker", "ledger", "default");
    assert_eq!(subscriptions.len(), 1);

    // Inside the grace window nothing is reaped, exactly as on a first start.
    tokio::time::advance(crate::api::grpc::SWEEP_INTERVAL).await;
    tokio::task::yield_now().await;
    assert_eq!(
        subscriptions.len(),
        1,
        "reaped inside its grace window, which no client could survive"
    );

    // Past the window the second start's sweep must reap it — with a cancelled
    // token it would break on its first tick and this would still read 1.
    tokio::time::advance(crate::api::grpc::sweep_grace(
        crate::api::grpc::SWEEP_INTERVAL,
    ))
    .await;
    tokio::task::yield_now().await;
    assert!(
        subscriptions.is_empty(),
        "the sweep must run after a stop-then-start cycle, not inherit a cancelled token"
    );

    gear.stop(CancellationToken::new())
        .await
        .expect("second stop");
}

#[tokio::test]
async fn gear_with_no_config_starts_empty() {
    // No `cluster` entry → default (empty) config → start binds nothing, no panic.
    let hub = Arc::new(ClientHub::default());
    let provider = Arc::new(MockConfig(serde_json::json!({})));
    let ctx = GearCtx::new(
        "other-gear",
        uuid::Uuid::new_v4(),
        provider,
        Arc::clone(&hub),
        CancellationToken::new(),
    );

    let gear = ClusterGear::default();
    gear.init(&ctx).await.expect("init");
    gear.start(CancellationToken::new())
        .await
        .expect("start with empty config");

    // No profile was bound — an empty config must not register anything.
    assert!(
        matches!(
            ClusterCacheV1::resolver(&hub)
                .profile(DefaultProfile)
                .resolve()
                .await,
            Err(ClusterError::ProfileNotBound { .. })
        ),
        "an empty config must bind no profile"
    );

    gear.stop(CancellationToken::new()).await.expect("stop");
}

// `S3` — the capability set

/// The four wire service names, in the order `get_grpc_services` returns them.
///
/// Spelled out rather than read back from the generated constants: these strings
/// are the wire contract routing keys on, and a test that derives them from the
/// same source it is checking would pass through a rename that breaks every
/// client. Four packages, not one `cluster.v1` — see `cluster-sdk/src/grpc.rs`.
const SERVICE_NAMES: [&str; 4] = [
    "cluster.cache.v1.ClusterCacheApi",
    "cluster.lock.v1.DistributedLockApi",
    "cluster.leader.v1.LeaderElectionApi",
    "cluster.profile.v1.ClusterProfileApi",
];

#[tokio::test]
async fn the_gear_exports_the_four_coordination_services_before_start() {
    // Collected in the gRPC registration phase, which runs *before* `start` -- so
    // succeeding here at all is the criterion: a `get_grpc_services` that needed a
    // backend could not return, because no backend exists yet (DESIGN section 4.2).
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();
    gear.init(&ctx).await.expect("init");

    let services = gear
        .get_grpc_services(&ctx)
        .await
        .expect("services are collected without any backend existing");

    let names: Vec<&str> = services.iter().map(|s| s.service_name).collect();
    assert_eq!(names, SERVICE_NAMES);
}

#[tokio::test]
async fn each_service_installer_is_reusable() {
    // `RegisterGrpcServiceFn::register` is an `Fn`, not an `FnOnce`: the framework
    // may install into more than one builder. Running each twice proves the clone
    // path rather than assuming a single call.
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();
    gear.init(&ctx).await.expect("init");
    let services = gear.get_grpc_services(&ctx).await.expect("services");

    for round in 0..2 {
        let mut routes = tonic::service::RoutesBuilder::default();
        for service in &services {
            (service.register)(&mut routes);
        }
        assert!(
            routes.routes().into_axum_router().has_routes(),
            "round {round}: every installer must add its service"
        );
    }
}

#[tokio::test]
async fn services_and_healthcheck_follow_the_registry_across_start() {
    // The exit criterion, end to end and over a real socket. One server, built
    // once from the pre-`start` registration, and one healthcheck object: both
    // must change answer when `start` publishes the registry beneath them. A
    // captured backend could not do that, and a captured *snapshot* of the
    // registry could not either.
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();
    gear.init(&ctx).await.expect("init");

    let services = gear.get_grpc_services(&ctx).await.expect("services");
    let check = gear
        .healthcheck(&ctx)
        .expect("the gear contributes a readiness check");

    let mut routes = tonic::service::RoutesBuilder::default();
    for service in &services {
        (service.register)(&mut routes);
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let endpoint = format!("http://{}", listener.local_addr().expect("addressed"));
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(
        Server::builder()
            .add_routes(routes.routes())
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _stopped = shutdown_rx.await;
            }),
    );

    let channel = Channel::from_shared(endpoint)
        .expect("a valid endpoint")
        .timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("connects");
    let mut cache =
        stubs::cache::cluster_cache_api_client::ClusterCacheApiClient::new(channel.clone());

    // before `start`: routed, reachable, and refusing
    let refused = cache
        .get(stubs::cache::GetRequest {
            profile: "default".to_owned(),
            key: "ledger".to_owned(),
        })
        .await
        .expect_err("nothing is bound before start");
    assert_eq!(
        refused.code(),
        tonic::Code::NotFound,
        "an unbound profile is the NotFound-mapped ProfileNotBound, not a transport failure"
    );

    let starting = check.check().await;
    assert_eq!(starting.status, HealthcheckStatus::Unhealthy);
    assert_eq!(
        starting.code.as_deref(),
        Some("starting"),
        "which the framework renders as `starting`/503"
    );

    // `start` publishes the registry under both of them
    gear.start(CancellationToken::new()).await.expect("start");

    cache
        .put(stubs::cache::PutRequest {
            profile: "default".to_owned(),
            key: "ledger".to_owned(),
            value: b"41".to_vec(),
            ttl_ms: None,
            client_request_id: None,
        })
        .await
        .expect("the same server now serves the published profile");
    let entry = cache
        .get(stubs::cache::GetRequest {
            profile: "default".to_owned(),
            key: "ledger".to_owned(),
        })
        .await
        .expect("cache.Get is served")
        .into_inner()
        .entry
        .expect("the key was just written");
    assert_eq!(entry.value, b"41");

    let ready = check.check().await;
    assert_eq!(
        ready.status,
        HealthcheckStatus::Healthy,
        "the same check object reports Healthy once the profile is bound and probing"
    );

    let _sent = shutdown.send(());
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn the_healthcheck_reports_starting_when_collected_before_init() {
    // Unreachable in the framework's phase order, so it must not be a
    // silent `None`: opting out of readiness would report the pod ready with
    // nothing bound. A fresh empty registry is permanently at generation 0, so the
    // fail-safe verdict is `Starting`.
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();

    let check = gear
        .healthcheck(&ctx)
        .expect("a check is contributed even before init");
    let result = check.check().await;
    assert_eq!(result.status, HealthcheckStatus::Unhealthy);
    assert_eq!(result.code.as_deref(), Some("starting"));
}

#[tokio::test]
async fn rest_registration_adds_no_routes() {
    // The coordination data plane is gRPC only; no primitive is ever exposed over
    // REST (DESIGN section 2.2). `S4` adds the admin routes here, and this asserts
    // the surface is empty until it does.
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();
    gear.init(&ctx).await.expect("init");

    let router = gear
        .register_rest(&ctx, axum::Router::new(), &NoOpenApi)
        .expect("registration succeeds");
    assert!(
        !router.has_routes(),
        "cluster exposes no REST route until S4's admin plane"
    );
}

// `R5` — Profile 1's half of the seam

#[tokio::test]
async fn init_claims_the_client_and_start_makes_it_answer() {
    // `R5`'s exit criterion, literally: `hub.get::<dyn ClusterClient>()` succeeds
    // in a Profile 1 process after the cluster gear's `start`, routing to the
    // profile `start` actually wired.
    //
    // Since `K3` the claim happens one phase earlier, in `init`, and the two halves
    // are asserted separately below because they answer different questions.
    // **`init` registering the client is what keeps a cluster-hosting process from
    // wiring a remote client to its own socket**: cluster-sdk's
    // `ConsumerRegistration` is replayed in the proxy-wiring phase, which sits
    // between `init` and `start`, so a client that only arrived with `start` would
    // arrive too late for the local-wins check (DESIGN section 4.9.3 step 1).
    let hub = Arc::new(ClientHub::default());
    let ctx = ctx_with_default_profile(&hub);
    let gear = ClusterGear::default();
    gear.init(&ctx).await.expect("init");

    // Registered, and registered *locally* - the property the wiring phase probes.
    assert!(
        hub.try_get_local::<dyn ClusterClient>().is_some(),
        "init must claim `dyn ClusterClient` locally, before the proxy-wiring phase runs"
    );
    // But it answers nothing yet, because the registry it dispatches through is
    // empty until `start` publishes. `ProfileNotBound` is the correct answer for
    // that window and needs no new variant (invariant I3).
    let before_start = hub
        .get::<dyn ClusterClient>()
        .expect("init registered a client");
    assert!(
        matches!(
            before_start.cache_backend("default"),
            Err(ClusterError::ProfileNotBound { .. })
        ),
        "a client registered before `start` must report ProfileNotBound, not a backend"
    );

    gear.start(CancellationToken::new()).await.expect("start");

    let client = hub
        .get::<dyn ClusterClient>()
        .expect("start registers the local client under the trait consumers resolve through");
    let descriptor = client
        .descriptor("default")
        .await
        .expect("the wired profile is described");
    assert_eq!(descriptor.cache.provider, "standalone");

    // It must be visible to the *local*-wins probe specifically, not merely
    // present: a registration that read as a remote proxy would let a consumer
    // build a second, remote client alongside it (DESIGN section 4.9.3).
    assert!(
        hub.try_get_local::<dyn ClusterClient>().is_some(),
        "the local client must win over any remote proxy"
    );

    // And it hands back the same backend instance the hub already holds under
    // `cluster:default` - one object per process, no wrapper (invariant I14).
    // The scope string is spelled out because it *is* the stable contract the
    // resolvers key on, and `profile_scope` is `pub(crate)` in the SDK.
    let via_client = client.cache_backend("default").expect("bound");
    let via_hub: Arc<dyn ClusterCacheBackend> = hub
        .get_scoped(&ClientScope::new("cluster:default"))
        .expect("the scoped registration is still there");
    assert!(
        Arc::ptr_eq(&via_client, &via_hub),
        "both paths must reach the one real backend"
    );

    gear.stop(CancellationToken::new()).await.expect("stop");

    // `stop` leaves the registration in place on purpose: the cleared registry is
    // what refuses the call, and it can name the profile while an absent client
    // could not (see `start`).
    let after_stop = hub
        .get::<dyn ClusterClient>()
        .expect("the client outlives the wiring");
    assert!(matches!(
        after_stop.cache_backend("default"),
        Err(ClusterError::ProfileNotBound { .. })
    ));
}

/// An `OpenApiRegistry` that records nothing — `register_rest` registers no route,
/// so it is never called.
struct NoOpenApi;

impl toolkit::contracts::OpenApiRegistry for NoOpenApi {
    fn register_operation(&self, _spec: &toolkit::api::OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        root_name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        root_name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// B4 — the shutdown drain of the election subscription table (item `S5`, §4.8).
///
/// `ElectionSubscriptions::broadcast_terminal` had no production caller: `stop`
/// cancelled the sweep and dropped the wiring without ever touching the table.
/// Two things followed, and both are asserted here — the documented terminal
/// events were never delivered, and no sender was ever dropped, so
/// `await_change` never ended and tonic's graceful shutdown could not complete.
mod shutdown_drain {
    use std::sync::Arc;
    use std::time::Duration;

    use cluster_sdk::leader::{LeaderElectionV1, LeaderStatus, LeaderWatchEvent};
    use tokio::net::TcpListener;
    use tokio_stream::StreamExt as _;
    use tokio_stream::wrappers::TcpListenerStream;
    use tokio_util::sync::CancellationToken;
    use tonic::transport::Channel;
    use toolkit::Gear as _;
    use toolkit::client_hub::ClientHub;
    use toolkit::contracts::{GrpcServiceCapability as _, RunnableCapability as _};

    use super::super::ClusterGear;
    use super::{DefaultProfile, ctx_with_default_profile};
    use cluster_sdk::grpc::stubs;

    /// The election every test here subscribes to, and the caller that owns the
    /// subscription. `attach` answers only the caller that opened it, so the two
    /// have to agree.
    const ELECTION: &str = "ledger";
    const CALLER: &str = "event-broker";

    /// A started gear over one standalone-cache `default` profile.
    async fn started(hub: &Arc<ClientHub>) -> ClusterGear {
        let ctx = ctx_with_default_profile(hub);
        let gear = ClusterGear::default();
        gear.init(&ctx).await.expect("init");
        gear.start(CancellationToken::new()).await.expect("start");
        gear
    }

    /// Drains `events` into the vocabulary it delivered, stopping at the channel
    /// close. Bounded, so a stream that never ends fails rather than hangs.
    async fn drain_vocabulary(
        events: &mut tokio::sync::mpsc::Receiver<LeaderWatchEvent>,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        {
            seen.push(name_of(&event));
        }
        seen
    }

    /// A comparable name for an event, so the two profiles' vocabularies can be
    /// asserted *equal* rather than each matched against a hand-written list.
    fn name_of(event: &LeaderWatchEvent) -> String {
        match event {
            LeaderWatchEvent::Status(status) => format!("Status({status:?})"),
            LeaderWatchEvent::Lagged { .. } => "Lagged".to_owned(),
            LeaderWatchEvent::Reset => "Reset".to_owned(),
            LeaderWatchEvent::Closed(err) => format!("Closed({err:?})"),
            other => format!("unknown({other:?})"),
        }
    }

    /// The two-step reaches a subscribed remote participant, in order, and the
    /// stream then ends.
    #[tokio::test]
    async fn stop_delivers_lost_then_closed_to_a_subscribed_participant() {
        let hub = Arc::new(ClientHub::default());
        let gear = started(&hub).await;
        let subscriptions = gear.subscriptions().expect("init created the table");

        let id = subscriptions.open(CALLER, ELECTION, "default");
        let mut events = subscriptions
            .attach(&id, CALLER)
            .expect("the caller that opened it may attach");

        gear.stop(CancellationToken::new()).await.expect("stop");

        assert_eq!(
            drain_vocabulary(&mut events).await,
            vec![
                "Status(Lost)".to_owned(),
                format!("Closed({:?})", cluster_sdk::ClusterError::Shutdown),
            ],
            "the section 4.8 two-step, in order, and nothing after it"
        );
        assert!(
            subscriptions.is_empty(),
            "and the table is empty, so the stream task has nothing left to hold"
        );
    }

    /// The `TERMINAL_HEADROOM` reservation, on the path that needs it: a
    /// subscriber that stopped reading must not be able to hold the drain open or
    /// cost itself the two terminal events.
    #[tokio::test]
    async fn stop_cannot_block_on_a_full_subscriber_channel() {
        let hub = Arc::new(ClientHub::default());
        let gear = started(&hub).await;
        let subscriptions = gear.subscriptions().expect("init created the table");

        let id = subscriptions.open(CALLER, ELECTION, "default");
        let mut events = subscriptions.attach(&id, CALLER).expect("attaches");

        // Well past the consumer-visible buffer, and nothing reads: the buffer
        // fills and the remainder are dropped, so the terminal two-step has to
        // travel through the reserved headroom rather than the ordinary slots.
        subscriptions.fill_buffer_for_test(&id, 128);

        let stopped =
            tokio::time::timeout(Duration::from_secs(5), gear.stop(CancellationToken::new())).await;
        assert!(
            matches!(stopped, Ok(Ok(()))),
            "a subscriber that stopped draining must not be able to stall `stop`; the two \
             terminal sends go through the reserved headroom, which never blocks"
        );

        // And the events are *there*, behind the backlog, rather than dropped: the
        // reservation is what distinguishes "queued behind 32 events" from "lost".
        let vocabulary = drain_vocabulary(&mut events).await;
        let terminal: Vec<&String> = vocabulary
            .iter()
            .filter(|name| name.starts_with("Status(") || name.starts_with("Closed("))
            .collect();
        assert_eq!(
            terminal,
            vec![
                &"Status(Lost)".to_owned(),
                &format!("Closed({:?})", cluster_sdk::ClusterError::Shutdown),
            ],
            "the two-step survives a full buffer, in order (saw {vocabulary:?})"
        );
    }

    /// Invariant I1: one drain, two deployment profiles, the same vocabulary.
    ///
    /// Profile 1 is an in-process consumer holding a `LeaderWatch` from the
    /// resolved facade — its two-step comes from
    /// `LeaderWatchSender::revoke_for_shutdown` inside the wiring's stop hook.
    /// Profile 3 is a subscription in the table — its two-step comes from
    /// `ClusterGear::stop`. Both are driven by the *same* `stop()` call here,
    /// which is what makes the comparison meaningful rather than two
    /// separately-arranged sequences that happen to agree.
    #[tokio::test]
    async fn both_deployment_profiles_deliver_the_same_drain_vocabulary() {
        let hub = Arc::new(ClientHub::default());
        let gear = started(&hub).await;
        let subscriptions = gear.subscriptions().expect("init created the table");

        // Profile 1: an in-process leader, elected through the facade over the
        // gear's own registry.
        let election = LeaderElectionV1::resolver(&hub)
            .profile(DefaultProfile)
            .resolve()
            .await
            .expect("the omit-default election resolves over the standalone cache");
        let mut in_process = election.elect(ELECTION).await.expect("enrols");
        assert!(
            matches!(
                in_process.changed().await,
                LeaderWatchEvent::Status(LeaderStatus::Leader)
            ),
            "the sole candidate leads, so its drain is the leader's drain"
        );

        // Profile 3: a subscription, as `join` + `await_change` leave behind.
        let id = subscriptions.open(CALLER, ELECTION, "default");
        let mut remote = subscriptions.attach(&id, CALLER).expect("attaches");

        gear.stop(CancellationToken::new()).await.expect("stop");

        let mut profile_1 = Vec::new();
        // `changed()` synthesizes a `Closed` once the sender is gone, so the
        // terminal event is the stop condition rather than a channel close.
        while let Ok(event) =
            tokio::time::timeout(Duration::from_secs(5), in_process.changed()).await
        {
            let terminal = matches!(event, LeaderWatchEvent::Closed(_));
            profile_1.push(name_of(&event));
            if terminal {
                break;
            }
        }
        let profile_3 = drain_vocabulary(&mut remote).await;

        assert_eq!(
            profile_1, profile_3,
            "one drain must read the same to both profiles (invariant I1)"
        );
    }

    /// The direct regression test for the stall: a graceful shutdown with one live
    /// `await_change` stream open completes promptly.
    ///
    /// Over a real socket, because the thing that hung was tonic waiting on an
    /// in-flight response — which only exists when there is a real connection to
    /// hold it. Before `stop` closed the table, `await_change` never ended
    /// server-side, `serve_with_incoming_shutdown` never returned, and in
    /// production the 30 s `stop_timeout` elapsed and `abort()` severed the stream.
    #[tokio::test]
    async fn a_live_await_change_stream_no_longer_stalls_graceful_shutdown() {
        let hub = Arc::new(ClientHub::default());
        let ctx = ctx_with_default_profile(&hub);
        let gear = ClusterGear::default();
        gear.init(&ctx).await.expect("init");
        let services = gear.get_grpc_services(&ctx).await.expect("services");
        gear.start(CancellationToken::new()).await.expect("start");

        let mut routes = tonic::service::RoutesBuilder::default();
        for service in &services {
            (service.register)(&mut routes);
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let endpoint = format!("http://{}", listener.local_addr().expect("addressed"));
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_routes(routes.routes())
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _stopped = shutdown_rx.await;
                }),
        );

        let channel = Channel::from_shared(endpoint)
            .expect("a valid endpoint")
            .connect()
            .await
            .expect("connects");
        let mut leader =
            stubs::leader::leader_election_api_client::LeaderElectionApiClient::new(channel);
        let joined = leader
            .join(stubs::leader::JoinRequest {
                profile: "default".to_owned(),
                name: ELECTION.to_owned(),
                ttl_ms: 30_000,
                max_missed_renewals: None,
                client_request_id: None,
            })
            .await
            .expect("join succeeds")
            .into_inner();
        let mut stream = leader
            .await_change(stubs::leader::AwaitChangeRequest {
                profile: "default".to_owned(),
                election_id: joined.election_id,
            })
            .await
            .expect("the subscription opens")
            .into_inner();
        // The subscription is genuinely attached before shutdown begins: without
        // this the stream may not have reached the server yet, and the test would
        // be asserting a drain with nothing to drain.
        let subscriptions = gear.subscriptions().expect("the table exists");
        let attached = tokio::time::timeout(Duration::from_secs(5), async {
            while subscriptions.len() != 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(attached.is_ok(), "the subscription must reach the table");

        // The gear drains, then the hub stops serving — the production order.
        gear.stop(CancellationToken::new()).await.expect("stop");
        let _sent = shutdown.send(());

        let drained = tokio::time::timeout(Duration::from_secs(5), server).await;
        assert!(
            drained.is_ok(),
            "graceful shutdown must complete with a live `await_change` open; it is still \
             waiting on the in-flight stream 5 s later"
        );

        // And the client's stream ended rather than being severed: the last frame
        // is the terminal close, not a transport error.
        let mut last = None;
        while let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_secs(5), stream.next()).await
        {
            last = Some(frame.expect("no frame is a transport error"));
        }
        assert_eq!(
            last.map(|event| event.kind),
            Some(i32::from(stubs::leader::LeaderWatchEventKind::Closed)),
            "the subscriber's last frame is the close it was promised"
        );
    }
}

/// Phase 5 (docs/IMPLEMENTATION.md section 5): the `k8s` feature registers the
/// native Kubernetes providers into `provider_registry` for all three primitives,
/// so a profile binding `provider: k8s` resolves each one. This is the first
/// **native** leader-election provider in the tree — every earlier provider left
/// leader election to the SDK default over a cache.
#[cfg(feature = "k8s")]
mod k8s_registration {
    use super::ClusterGear;

    #[test]
    fn provider_registry_resolves_k8s_for_every_primitive() {
        let registry = ClusterGear::provider_registry();

        let cache = registry
            .cache_provider("k8s")
            .expect("the k8s cache provider must be registered under the `k8s` feature");
        let leader = registry
            .leader_election_provider("k8s")
            .expect("the k8s leader-election provider (the first native one) must be registered");
        let lock = registry
            .lock_provider("k8s")
            .expect("the k8s lock provider must be registered under the `k8s` feature");

        assert_eq!(cache.provider(), "k8s");
        assert_eq!(leader.provider(), "k8s");
        assert_eq!(lock.provider(), "k8s");

        // The k8s cache is an *additional* entry — the pre-existing providers still
        // resolve alongside it (the registry keys per name, per primitive).
        assert!(registry.cache_provider("standalone").is_some());
        assert!(registry.cache_provider("postgres").is_some());
        assert!(registry.lock_provider("postgres").is_some());
    }
}

/// Without the `k8s` feature the registry links neither the k8s providers nor kube,
/// so a `provider: k8s` binding is an unknown provider the wiring rejects — while
/// the unconditional standalone/postgres providers still resolve.
#[cfg(not(feature = "k8s"))]
mod no_k8s_registration {
    use super::ClusterGear;

    #[test]
    fn provider_registry_has_no_k8s_providers_without_the_feature() {
        let registry = ClusterGear::provider_registry();
        assert!(registry.cache_provider("k8s").is_none());
        assert!(registry.leader_election_provider("k8s").is_none());
        assert!(registry.lock_provider("k8s").is_none());
        // The base providers are unconditional.
        assert!(registry.cache_provider("standalone").is_some());
        assert!(registry.cache_provider("postgres").is_some());
        assert!(registry.lock_provider("postgres").is_some());
    }
}
