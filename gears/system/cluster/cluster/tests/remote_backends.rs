//! The three remote backend handles against a **real** cluster gear
//! (DESIGN.md).
//!
//! This file lives in the gear crate rather than beside the backends themselves,
//! and it has to: only this crate can serve the four services, because the
//! service impls are the gear's. What it buys is the assertion that matters most
//! for the whole deployable model — **the same trait, the same behaviour, both
//! sides of a socket**. Every test here drives an `Arc<dyn _Backend>` obtained
//! from a `RemoteClusterClient`, which is exactly what a consumer's `resolve()`
//! hands it, and several of them assert the remote answer against the *local*
//! backend's answer for the same operation.
//!
//! # Two halves
//!
//! The first half is the operations themselves: cache, lock and leader election
//! round-tripping over the wire, and the descriptor that makes the synchronous
//! accessors answer like the backend behind them.
//!
//! The second half — from "Elections when the connection carrying them goes
//! away" — is the property that only shows up once a connection can die under a
//! live election. A `LeaderWatch` carries two independent paths: the
//! *subscription*, which conveys whether events are flowing, and the *renewal
//! task*, which conveys whether the claim is still valid. ADR-003 requires them
//! kept apart — *"A `Closed(ConnectionLost)` on a `LeaderWatch` is a subscription
//! event. State validity is determined by the renewal-task path"* — and §6.6
//! prices it: *"losing it costs a re-subscribe, not a leadership change"*. So a
//! rolling restart, replica kill, LB drain or GOAWAY must cost a re-subscribe and
//! nothing more. Those tests drive that through a [`CuttableRelay`], and they run
//! on a multi-threaded runtime on purpose: on the single-threaded default the
//! contender's poll loop and the pump under test share one thread, and starving
//! the pump would fail them for a reason unrelated to what they assert. Each
//! timing assertion is sized so both the holding and the failing margin are
//! comfortable; the per-test comments give both.
//!
//! The standalone plugin backs the server, to stay hermetic (§7.6).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::err_expect,
    reason = "integration tests: a setup failure IS the test failure"
)]
#![allow(
    clippy::print_stdout,
    clippy::use_debug,
    reason = "the election tests below print the measured timings and the decoded \
              events they exist to show; the `Debug` form is that evidence"
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cluster_sdk::cache::{CacheWatchEvent, PutRequest, Ttl};
use cluster_sdk::dto;
use cluster_sdk::grpc::stubs;
use cluster_sdk::leader::{LeaderStatus, LeaderWatchEvent};
use cluster_sdk::{CacheConsistency, ClusterClient, ClusterError, RemoteClusterClient};
use tokio::net::{TcpListener, TcpStream};
// For `serve_fake` below — a bespoke `RefusesTheSubscription` service rather than
// the real gear, so it stands up its own server instead of using `served_gear`.
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;

mod common;
use common::served_gear::{PROFILE, ServedGear, served_gear};

/// How long a stream assertion waits before declaring the event lost.
///
/// A watch that never delivers hangs the test binary rather than failing it, so
/// every `recv` in this file is wrapped. Generous enough not to flake on a loaded
/// machine, short enough to fail inside the harness timeout.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A running cluster gear plus a lazy client pointed at it.
struct Fixture {
    gear: ServedGear,
}

impl Fixture {
    async fn start() -> Self {
        Self {
            gear: served_gear().start().await,
        }
    }

    /// The remote cache handle, as a consumer would hold it.
    fn cache(&self) -> Arc<dyn cluster_sdk::ClusterCacheBackend> {
        self.gear.client().cache_backend(PROFILE).expect("a handle")
    }

    fn lock(&self) -> Arc<dyn cluster_sdk::DistributedLockBackend> {
        self.gear.client().lock_backend(PROFILE).expect("a handle")
    }

    fn leader(&self) -> Arc<dyn cluster_sdk::LeaderElectionBackend> {
        self.gear
            .client()
            .leader_election_backend(PROFILE)
            .expect("a handle")
    }

    /// An election handle on its **own** connection, dialled straight at the gear.
    ///
    /// The connection-loss tests put a [`CuttableRelay`] in front of the gear and
    /// sever it; a contender has to survive that, so it cannot share the client
    /// whose connection is being cut.
    fn direct_leader(&self) -> Arc<dyn cluster_sdk::LeaderElectionBackend> {
        leader_at(self.gear.addr)
    }

    /// The server-side backend for the same profile, for the comparisons that
    /// make "the same trait, both sides of the socket" checkable rather than
    /// asserted.
    fn local_cache(&self) -> Arc<dyn cluster_sdk::ClusterCacheBackend> {
        Arc::clone(
            &self
                .gear
                .registry
                .resolve(PROFILE)
                .expect("published")
                .cache,
        )
    }

    async fn stop(self) {
        self.gear.stop().await;
    }
}

/// Awaits one watch event, failing rather than hanging.
async fn next_cache_event(watch: &mut cluster_sdk::CacheWatch) -> CacheWatchEvent {
    tokio::time::timeout(EVENT_TIMEOUT, watch.recv())
        .await
        .expect("a watch event must arrive inside the timeout")
        .expect("the watch must not close")
}

/// Awaits one election event, failing rather than hanging.
async fn next_leader_event(watch: &mut cluster_sdk::LeaderWatch) -> LeaderWatchEvent {
    tokio::time::timeout(EVENT_TIMEOUT, watch.changed())
        .await
        .expect("an election event must arrive inside the timeout")
}

// The descriptor, and what it makes the synchronous accessors answer

#[tokio::test]
async fn the_descriptor_makes_the_sync_accessors_answer_like_the_real_backend() {
    // §5.5's whole purpose: `consistency()`, `features()` and `provider_name()`
    // are synchronous on a trait plugins implement, and a remote handle answers
    // them out of one `DescribeProfiles` rather than not at all.
    let fixture = Fixture::start().await;
    let cache = fixture.cache();
    let local = fixture.local_cache();

    // Before the fetch the handle fails safe - the weaker reading in every case.
    assert_eq!(cache.consistency(), CacheConsistency::EventuallyConsistent);
    assert_eq!(cache.provider_name(), "unknown");

    let descriptor = fixture
        .gear
        .client()
        .descriptor(PROFILE)
        .await
        .expect("the profile is bound");
    assert_eq!(descriptor.name, PROFILE);

    // ...and afterwards it agrees with the backend on the other side.
    assert_eq!(
        cache.consistency(),
        local.consistency(),
        "the remote handle must declare what the real backend declares"
    );
    assert_eq!(cache.features().prefix_watch, local.features().prefix_watch);
    assert_eq!(
        cache.provider_name(),
        "standalone",
        "the *server-side* provider, not the remote handle's own type"
    );

    fixture.stop().await;
}

#[tokio::test]
async fn an_unbound_profile_is_profile_not_bound_from_the_descriptor() {
    let fixture = Fixture::start().await;

    let err = fixture
        .gear
        .client()
        .descriptor("nowhere")
        .await
        .expect_err("the server binds no such profile");
    assert!(
        matches!(err, ClusterError::ProfileNotBound { .. }),
        "expected ProfileNotBound, got: {err}"
    );

    fixture.stop().await;
}

#[tokio::test]
async fn a_call_against_an_unbound_profile_reports_profile_not_bound() {
    // The factory succeeded for a profile the server does not bind; the *call* is
    // where that is reported, and it comes back as the frozen model's existing
    // variant (invariant I3).
    let fixture = Fixture::start().await;
    let cache = fixture
        .gear
        .client()
        .cache_backend("nowhere")
        .expect("a handle");

    let err = cache.get("k").await.expect_err("no such profile");
    assert!(
        matches!(err, ClusterError::ProfileNotBound { .. }),
        "expected ProfileNotBound, got: {err}"
    );

    fixture.stop().await;
}

// The cache

#[tokio::test]
async fn the_cache_round_trips_every_unary_operation() {
    let fixture = Fixture::start().await;
    let cache = fixture.cache();

    assert!(cache.get("ledger").await.expect("get").is_none());
    assert!(!cache.contains("ledger").await.expect("contains"));

    cache
        .put(PutRequest {
            key: "ledger",
            value: b"41",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");

    let entry = cache
        .get("ledger")
        .await
        .expect("get")
        .expect("just written");
    assert_eq!(entry.value, b"41");
    assert!(cache.contains("ledger").await.expect("contains"));

    // The write is visible to the *server's own* backend: the wire moved it, not
    // a client-side cache.
    let local = fixture.local_cache();
    assert_eq!(
        local
            .get("ledger")
            .await
            .expect("get")
            .expect("present")
            .value,
        b"41"
    );

    let swapped = cache
        .compare_and_swap("ledger", entry.version, b"42", Ttl::Indefinite)
        .await
        .expect("cas on the current version");
    assert_eq!(swapped.value, b"42");

    // A stale version is a typed `CasConflict`, reconstructed through the trailer
    // rather than inferred from the gRPC code (§6.9).
    let conflict = cache
        .compare_and_swap("ledger", entry.version, b"43", Ttl::Indefinite)
        .await
        .expect_err("the version moved");
    assert!(
        matches!(conflict, ClusterError::CasConflict { ref key, .. } if key == "ledger"),
        "expected CasConflict, got: {conflict}"
    );

    // `put_if_absent` on a present key creates nothing.
    assert!(
        cache
            .put_if_absent(PutRequest {
                key: "ledger",
                value: b"99",
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put_if_absent")
            .is_none()
    );

    // A value-guarded delete against the wrong value is a no-op, not an error.
    assert!(
        !cache
            .compare_and_delete("ledger", b"wrong")
            .await
            .expect("cad")
    );
    assert!(
        cache
            .compare_and_delete("ledger", b"42")
            .await
            .expect("cad")
    );
    assert!(!cache.contains("ledger").await.expect("contains"));

    assert!(!cache.delete("ledger").await.expect("delete an absent key"));

    fixture.stop().await;
}

#[tokio::test]
async fn scan_prefix_reassembles_every_page() {
    // The wire is paginated and the trait is not (§6.4). The server's default page
    // size is 256, so 300 keys is the smallest count that proves the loop rather
    // than the first page.
    let fixture = Fixture::start().await;
    let cache = fixture.cache();

    for index in 0..300 {
        cache
            .put(PutRequest {
                key: &format!("orders/{index:04}"),
                value: b"x",
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put");
    }

    let mut keys = cache.scan_prefix("orders/").await.expect("scan");
    keys.sort();
    assert_eq!(keys.len(), 300, "every page must be reassembled");
    assert_eq!(keys.first().map(String::as_str), Some("orders/0000"));
    assert_eq!(keys.last().map(String::as_str), Some("orders/0299"));

    fixture.stop().await;
}

#[tokio::test]
async fn a_cache_watch_delivers_the_servers_events() {
    let fixture = Fixture::start().await;
    let cache = fixture.cache();

    let mut watch = cache.watch("ledger").await.expect("the watch opens");

    cache
        .put(PutRequest {
            key: "ledger",
            value: b"1",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");

    let event = next_cache_event(&mut watch).await;
    assert!(
        matches!(
            event,
            CacheWatchEvent::Event(cluster_sdk::CacheEvent::Changed { ref key }) if key == "ledger"
        ),
        "expected Changed(ledger), got: {event:?}"
    );

    cache.delete("ledger").await.expect("delete");
    let event = next_cache_event(&mut watch).await;
    assert!(
        matches!(
            event,
            CacheWatchEvent::Event(cluster_sdk::CacheEvent::Deleted { ref key }) if key == "ledger"
        ),
        "expected Deleted(ledger), got: {event:?}"
    );

    fixture.stop().await;
}

// The lock

#[tokio::test]
async fn the_lock_guard_renews_and_releases_over_the_wire() {
    // The guard's fields are private, so the token lives in the pump's closure
    // (§12.11). Renewing and releasing through the guard is what proves the pump
    // is actually holding it.
    let fixture = Fixture::start().await;
    let lock = fixture.lock();

    let guard = lock
        .try_lock("ledger", Duration::from_secs(30))
        .await
        .expect("the lock is free");
    assert_eq!(guard.name(), "ledger");

    // Held: a second acquisition is refused with the typed contention error.
    let contended = lock
        .try_lock("ledger", Duration::from_secs(30))
        .await
        .expect_err("the lock is held");
    assert!(
        matches!(contended, ClusterError::LockContended { ref name } if name == "ledger"),
        "expected LockContended, got: {contended}"
    );

    guard
        .renew(Duration::from_mins(1))
        .await
        .expect("the holder can renew");
    guard.release().await.expect("the holder can release");

    // Released: the next acquisition succeeds.
    let next = lock
        .try_lock("ledger", Duration::from_secs(30))
        .await
        .expect("the lock is free again");
    next.release().await.expect("release");

    fixture.stop().await;
}

#[tokio::test]
async fn a_lock_release_is_idempotent_by_absence() {
    // §6.10: a token matching nothing has already achieved what its caller
    // wanted, so the release is `Ok` — which is also what makes a token
    // unprobeable, since both answers are the same `Ok` (§5.8.1).
    let fixture = Fixture::start().await;
    let lock = fixture.lock();

    let token = lock
        .acquire(
            "ledger",
            "ignored-the-server-mints-it",
            Duration::from_secs(30),
        )
        .await
        .expect("acquired");
    lock.release(&token).await.expect("the first release");
    lock.release(&token)
        .await
        .expect("and the second, against nothing");

    // A renewal of the same gone lease is *not* `Ok`: the caller has to learn it
    // lost the lease, which is the one place idempotency stops at the wire.
    let err = lock
        .renew(&token, Duration::from_secs(30))
        .await
        .expect_err("the lease is gone");
    assert!(
        matches!(err, ClusterError::LockExpired { .. }),
        "expected LockExpired, got: {err}"
    );

    fixture.stop().await;
}

#[tokio::test]
async fn a_blocking_lock_times_out_with_the_wait_the_server_measured() {
    // The server does the waiting (§6.5), and `waited` is populated server-side
    // because the server is what did it (§6.9).
    let fixture = Fixture::start().await;
    let lock = fixture.lock();

    let held = lock
        .try_lock("ledger", Duration::from_secs(30))
        .await
        .expect("acquired");

    let err = lock
        .lock(
            "ledger",
            Duration::from_secs(30),
            Duration::from_millis(200),
        )
        .await
        .expect_err("the lock is held for longer than the timeout");
    assert!(
        matches!(err, ClusterError::LockTimeout { ref name, .. } if name == "ledger"),
        "expected LockTimeout, got: {err}"
    );

    held.release().await.expect("release");
    fixture.stop().await;
}

// Leader election

#[tokio::test]
async fn electing_reports_leadership_and_resigning_gives_it_back() {
    let fixture = Fixture::start().await;
    let leader = fixture.leader();

    let mut watch = leader
        .elect("primary")
        .await
        .expect("the election is joined");

    let event = next_leader_event(&mut watch).await;
    assert!(
        matches!(event, LeaderWatchEvent::Status(LeaderStatus::Leader)),
        "the sole candidate must be told it leads, got: {event:?}"
    );
    assert!(watch.is_leader(), "and the cached snapshot must agree");

    watch.resign().await.expect("the leader can step down");

    // The claim is back: a fresh election wins it.
    let mut next = leader.elect("primary").await.expect("re-elected");
    let event = next_leader_event(&mut next).await;
    assert!(
        matches!(event, LeaderWatchEvent::Status(LeaderStatus::Leader)),
        "the resigned claim must be available again, got: {event:?}"
    );

    fixture.stop().await;
}

#[tokio::test]
async fn a_second_candidate_follows_rather_than_failing() {
    // Losing an election is an ordinary outcome, not an error (§6.6). A follower
    // must read `initial_status` and never the token's shape — it receives the
    // zero token, because `LeaderJoined.token` is not optional on the wire.
    let fixture = Fixture::start().await;
    let leader = fixture.leader();

    let mut first = leader.elect("primary").await.expect("joined");
    let event = next_leader_event(&mut first).await;
    assert!(matches!(
        event,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));

    let mut second = leader.elect("primary").await.expect("joined");
    let event = next_leader_event(&mut second).await;
    assert!(
        matches!(event, LeaderWatchEvent::Status(LeaderStatus::Follower)),
        "the second candidate must follow, got: {event:?}"
    );
    assert!(!second.is_leader());

    fixture.stop().await;
}

#[tokio::test]
async fn the_lease_half_of_the_election_round_trips() {
    // `join`/`renew`/`resign` as the token-keyed operations they are — the half a
    // serving gear uses, and the half that makes a claim survive the replica it
    // was made through (invariant I7).
    let fixture = Fixture::start().await;
    let leader = fixture.leader();
    let config = cluster_sdk::ElectionConfig::default();

    let token = leader
        .join("primary", "ignored-the-server-mints-it", config)
        .await
        .expect("join")
        .expect("the sole candidate wins");

    leader
        .renew(&token, config.ttl())
        .await
        .expect("the holder can renew");

    // A second candidate loses, and says so with `None` rather than an error.
    assert!(
        leader
            .join("primary", "another", config)
            .await
            .expect("join")
            .is_none(),
        "a contended election is Ok(None), never an error"
    );

    leader.resign(&token).await.expect("resign");
    leader
        .resign(&token)
        .await
        .expect("and again, against nothing - absence is Ok");

    fixture.stop().await;
}

#[tokio::test]
async fn dropping_the_watch_releases_the_claim_best_effort() {
    // The Profile 3 mirror of `defaults::leader_tests::
    // dropping_watch_releases_claim_best_effort`, and an invariant I1 assertion:
    // one consumer source file, one observable behaviour. Profile 1 asserted this
    // and Profile 3 never did, which is exactly how the two came to disagree - the
    // remote pump wrote its resign arm as `Some(responder) = resigns.recv()`, and
    // a `select!` branch whose pattern fails is *disabled* rather than taken, so
    // the `None` a dropped watch produces was discarded and the claim was renewed
    // forever.
    //
    // The TTL is far longer than this test and the renewal cadence far shorter
    // (25 s / 249 missed renewals = a 100 ms tick), so a claim merely *lapsing*
    // cannot be what frees the election. Only a pump that stopped and resigned
    // can.
    let fixture = Fixture::start().await;
    let leader = fixture.leader();
    let config = cluster_sdk::ElectionConfig::new(Duration::from_secs(25), 249)
        .expect("a long TTL on a fast cadence");

    let mut watch = leader
        .elect_with_config("primary", config)
        .await
        .expect("the sole candidate leads");
    let event = next_leader_event(&mut watch).await;
    assert!(
        matches!(event, LeaderWatchEvent::Status(LeaderStatus::Leader)),
        "expected leadership before dropping it, got: {event:?}"
    );

    drop(watch);

    // Poll rather than guess: the pump wakes on the closed resign channel and
    // issues one resign RPC, and what is asserted is that this completes in far
    // less than the 25 s the claim would otherwise be held for.
    let freed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Some(token)) = leader.join("primary", "successor", config).await {
                return token;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    let token = freed.expect(
        "a dropped watch must free the election without waiting out the TTL - the pump \
         has to stop renewing and best-effort resign, as the in-process one does",
    );
    assert_eq!(token.name, "primary");

    fixture.stop().await;
}

// The follower pump's subscription leak, and the sweep that bounds it (`S2`)

/// A fast election: `renewal_interval = ttl / (max_missed_renewals + 1)`, so a
/// 300 ms TTL with the default budget of 2 puts the pump on a 100 ms cadence and
/// several intervals fit inside a test.
fn fast_election() -> cluster_sdk::ElectionConfig {
    cluster_sdk::ElectionConfig::new(Duration::from_millis(300), 2).expect("a valid config")
}

/// The pump's cadence for [`fast_election`].
const FAST_RENEWAL: Duration = Duration::from_millis(100);

#[tokio::test]
async fn a_follower_pump_mints_one_subscription_per_renewal_interval() {
    // The measurement behind the subscription sweep. A
    // follower re-`join`s on the renewal cadence because the server announces no
    // leadership (section 6.6), `join` opens a subscription unconditionally, and
    // the pump keeps its *original* `election_id` - so every re-claim attempt
    // leaves an unattached entry behind and nothing closes it.
    //
    // This is also the mutation check for the whole item: break `join`'s `open`
    // and this stops growing.
    let fixture = Fixture::start().await;
    let leader = fixture.leader();
    let config = fast_election();

    let _held = leader
        .elect_with_config("primary", config)
        .await
        .expect("the first candidate leads");
    let _follows = leader
        .elect_with_config("primary", config)
        .await
        .expect("the second candidate follows");

    // Both `elect`s attach one subscription each. Poll for the pair rather than
    // reading the count once: `elect` can return to the caller before its pump has
    // registered the server-side subscription, so an immediate read may race the
    // second attach (the growth half below polls for the same reason). Require *at
    // least* two: the follower's first renewal can land before the poll first
    // observes the pair, so an overshoot to three is legitimate here - the leak
    // itself is what the growth assertion below pins down.
    let settled = tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let len = fixture.gear.subscriptions.len();
            if len >= 2 {
                return len;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both subscriptions should attach within the event timeout");
    assert!(settled >= 2, "one subscription per `elect`: got {settled}");

    // Eight nominal intervals of observation against a floor of three: the pump's
    // cadence is a real timer, so on a saturated CI box (this runs under the whole
    // workspace's test load) it can be scheduler-starved and fire fewer times than
    // the wall clock would suggest. A wide window keeps the *property* - that a
    // steady-state follower keeps leaking - decidable without racing that latency.
    tokio::time::sleep(FAST_RENEWAL * 8).await;

    let grown = fixture.gear.subscriptions.len();
    assert!(
        grown >= settled + 3,
        "a steady-state follower must leak one unattached subscription per renewal \
         interval: started at {settled}, after eight intervals {grown}"
    );

    fixture.stop().await;
}

#[tokio::test]
async fn the_sweep_bounds_the_follower_pumps_unattached_subscriptions() {
    // The same drive as above with the sweep running, and the assertion inverted:
    // the population stops at the two attached subscriptions plus whatever the
    // grace window has yet to age off, instead of climbing forever (section
    // 5.4.1).
    //
    // The cadence is scaled down from the shipped 5 s so the test finishes; the
    // *ratio* is the shipped one, since the grace window is a multiple of the
    // interval. That is the property under test - the shape of the bound, not
    // the size of the constants.
    let fixture = Fixture::start().await;
    let sweeping = CancellationToken::new();
    let interval = FAST_RENEWAL * 2;
    let _sweep = cluster::api::grpc::spawn_subscription_sweep(
        Arc::clone(&fixture.gear.subscriptions),
        interval,
        cluster::api::grpc::SubscriptionMetrics::global(),
        sweeping.clone(),
    );

    let leader = fixture.leader();
    let config = fast_election();
    let held = leader
        .elect_with_config("primary", config)
        .await
        .expect("the first candidate leads");
    let follows = leader
        .elect_with_config("primary", config)
        .await
        .expect("the second candidate follows");

    // Long enough that the unswept version of this test would be well past
    // twenty leaked subscriptions.
    tokio::time::sleep(FAST_RENEWAL * 25).await;

    // Two attached, plus at most one grace window's worth of not-yet-aged
    // arrivals and the pass they are waiting on.
    let ceiling = 2 + (cluster::api::grpc::SWEEP_GRACE_MULTIPLIER as usize + 1) * 2;
    let bounded = fixture.gear.subscriptions.len();
    assert!(
        bounded <= ceiling,
        "the sweep must hold the table near its live population: {bounded} entries \
         after twenty-five renewal intervals, ceiling {ceiling}"
    );

    // And it bounded rather than broke: both participants still hold their feeds.
    assert!(
        held.is_leader(),
        "the leader kept its claim across the sweep - a subscription is not a lease"
    );
    assert!(!follows.is_leader());

    sweeping.cancel();
    fixture.stop().await;
}

// Elections when the connection carrying them goes away

/// The election these tests hold.
const ELECTION: &str = "primary";

/// The TTL every election here runs on.
///
/// With the default budget of 2 this puts the pump on a
/// `ttl / (max_missed_renewals + 1)` = **500 ms** cadence, so a claim that stops
/// being renewed becomes takeable 1.5 s later and a pump that *is* renewing has
/// to be starved for a full second before it could lose one. That second of slack
/// is what keeps these tests honest on a loaded machine; the older 900 ms/300 ms
/// pairing left only 600 ms and was too tight to run in CI forever.
const TTL: Duration = Duration::from_millis(1500);

/// The pump's renewal cadence for [`TTL`].
const CADENCE: Duration = Duration::from_millis(500);

fn election_config() -> cluster_sdk::ElectionConfig {
    cluster_sdk::ElectionConfig::new(TTL, 2).expect("a valid config")
}

fn leader_at(addr: SocketAddr) -> Arc<dyn cluster_sdk::LeaderElectionBackend> {
    RemoteClusterClient::connect_lazy(&format!("http://{addr}"), None)
        .expect("a valid endpoint")
        .leader_election_backend(PROFILE)
        .expect("a handle")
}

/// A TCP relay whose *live* connections a test can sever while the gear behind
/// it keeps running.
///
/// This is what lets the election tests below say anything at all: it models what
/// a rolling restart, an LB drain or a GOAWAY does to a long-lived `await_change`
/// stream — the connection carrying it dies, the server and its subscription
/// table live on, and the client's next unary call reconnects through a fresh
/// one. Killing the *server* instead would confound the two variables under
/// test, because it would take the lease store down with the subscription.
///
/// Kept in this file rather than in `common`: it is transport plumbing for these
/// tests specifically. A second consumer — a cache-watch or lock-renewal
/// equivalent — is the point at which it should move to `common` whole.
struct CuttableRelay {
    addr: SocketAddr,
    live: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl CuttableRelay {
    async fn in_front_of(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let addr = listener.local_addr().expect("has an address");
        let live: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let accepting = Arc::clone(&live);
        tokio::spawn(async move {
            while let Ok((mut inbound, _peer)) = listener.accept().await {
                let relay = tokio::spawn(async move {
                    let Ok(mut outbound) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let _copied = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                });
                accepting
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(relay.abort_handle());
            }
        });
        Self { addr, live }
    }

    /// Severs every connection currently open through the relay. New ones are
    /// still accepted, so this breaks streams without making the gear
    /// unreachable — which is exactly the distinction under test.
    fn cut(&self) {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for relay in live.drain(..) {
            relay.abort();
        }
    }

    fn leader(&self) -> Arc<dyn cluster_sdk::LeaderElectionBackend> {
        leader_at(self.addr)
    }
}

/// Polls `join` until it takes the election, or `window` elapses.
///
/// `Some(elapsed)` is the moment the incumbent's claim stopped being renewed
/// hard enough for a contender to take it. The claim is handed straight back, so
/// the probe never becomes the thing that broke the election.
async fn taken_within(
    contender: &Arc<dyn cluster_sdk::LeaderElectionBackend>,
    window: Duration,
) -> Option<Duration> {
    let config = election_config();
    let started = tokio::time::Instant::now();
    while started.elapsed() < window {
        if let Ok(Some(token)) = contender.join(ELECTION, "contender", config).await {
            let at = started.elapsed();
            let _best_effort = contender.resign(&token).await;
            return Some(at);
        }
        // 50 ms rather than a tight spin: the steal is still located to within
        // one poll, and the probe stays cheap enough that it cannot itself be
        // what delays the pump it is measuring.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Elects, and asserts the initial status really is leadership.
async fn lead(leader: &Arc<dyn cluster_sdk::LeaderElectionBackend>) -> cluster_sdk::LeaderWatch {
    let mut watch = leader
        .elect_with_config(ELECTION, election_config())
        .await
        .expect("the sole candidate leads");
    let first = tokio::time::timeout(EVENT_TIMEOUT, watch.changed())
        .await
        .expect("an initial status arrives");
    assert!(
        matches!(first, LeaderWatchEvent::Status(LeaderStatus::Leader)),
        "expected leadership before testing what keeps it, got: {first:?}"
    );
    watch
}

// `ELEC-1` — a subscription-level close must not touch the renewal task

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn losing_the_subscription_stops_the_renewal_task_profile3() {
    // ADR-003's "Watch task and renewal task: independent signal paths", as an
    // end-to-end property: cutting the connection that carries `await_change`
    // must cost a re-subscribe and nothing else (§6.6), because §5.8.2 makes
    // "rolling the pod leaves every leader claim exactly where it was" a gate.
    //
    // The control loop runs first and is not decoration: without it, the
    // post-cut `None` could pass for the wrong reason (a contender that never
    // works, a fixture that never elects). It proves the probe *can* observe a
    // steal and that the pump is genuinely renewing across two full TTLs before
    // anything is broken.
    //
    // Margins. Broken: the pump returns at the cut, so the claim lapses ~1.0-1.5 s
    // later and the probe sees it inside a 3 s window — detected with >2x room.
    // Fixed: the pump renews every 500 ms against a 1500 ms TTL, so it would have
    // to be starved for a full second to flake.
    let fixture = Fixture::start().await;
    let relay = CuttableRelay::in_front_of(fixture.gear.addr).await;
    let leader = relay.leader();
    let contender = fixture.direct_leader();

    let watch = lead(&leader).await;

    let control = taken_within(&contender, TTL * 2).await;
    assert_eq!(
        control, None,
        "control: with its subscription healthy the pump renews, so no contender can \
         take the election inside two TTLs"
    );

    relay.cut();

    let stolen = taken_within(&contender, TTL * 2).await;
    assert_eq!(
        stolen, None,
        "ADR-003/6.6/5.8.2: losing the subscription must not cost the claim - but a \
         contender took the election {stolen:?} after the subscription closed, because \
         the pump that renews it returned when the stream did"
    );

    drop(watch);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_restart_cycle_cannot_readopt_the_orphaned_claim() {
    // §5.8.3 states it as a gate: "a killed replica costs subscribers one
    // `RestartingWatch` cycle and no lease". So a consumer doing exactly what the
    // design tells it to do after a broken feed - give up the handle and
    // re-`elect` - must come back leader promptly, not queue behind a claim its
    // own previous pump walked away from without resigning.
    //
    // Margins, and why the bound is absolute rather than a fraction of the TTL.
    // Fixed, the wait is one resign round trip plus one poll (~20 ms) *whatever*
    // the TTL is, because the pump gives the claim back on its way out. Broken,
    // it cannot be less than `TTL - CADENCE` = 1000 ms, because that is the
    // shortest a claim whose last renewal already happened can take to lapse. A
    // 400 ms bound therefore sits ~20x above the fixed case and ~2.5x below the
    // broken one.
    let fixture = Fixture::start().await;
    let relay = CuttableRelay::in_front_of(fixture.gear.addr).await;
    let leader = relay.leader();

    let watch = lead(&leader).await;

    relay.cut();
    // Long enough that the pump's first re-attach (due one backoff step after the
    // close) has already resolved, so this measures the teardown resign and not a
    // race against an in-flight re-subscribe.
    tokio::time::sleep(CADENCE).await;
    drop(watch);

    // Polled rather than measured in one shot: the teardown resign is best-effort
    // and off the caller's path in *both* profiles, so a single immediate
    // re-elect would be racing it. What the design forbids is being stuck behind
    // the orphaned claim for its TTL, and that is what this measures.
    let started = tokio::time::Instant::now();
    let (waited, again) = loop {
        let candidate = leader
            .elect_with_config(ELECTION, election_config())
            .await
            .expect("the consumer re-elects");
        if matches!(candidate.status(), LeaderStatus::Leader) {
            break (started.elapsed(), candidate);
        }
        drop(candidate);
        assert!(
            started.elapsed() < TTL * 3,
            "the re-elect never became leader inside three TTLs"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    println!("[election] the re-elect became leader after {waited:?}");

    assert!(
        waited < Duration::from_millis(400),
        "5.8.2 says the restart costs one RestartingWatch cycle and no lease, but the \
         re-elect was a FOLLOWER behind its own un-resigned claim for {waited:?} (the \
         claim's own {TTL:?} TTL)"
    );

    drop(again);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_feed_shows_the_consumer_reset_and_never_closed() {
    // What the consumer observes across a re-subscribe, which invariant I1 makes
    // a decision rather than an implementation detail.
    //
    // `Reset` is §6.8's own definition - "the server's upstream subscription was
    // re-established" - and the same event ADR-003 has `RestartingWatch`
    // synthesise on every successful resubscribe. `Closed` is the one thing it
    // must *not* be: ADR-003 makes `Closed` terminal ("providers MUST ensure no
    // further items are yielded"), and a terminal event here is precisely what
    // cost the claim. Profile 1 forwards `Reset` on a `LeaderWatch` too
    // (`defaults/leader.rs`, `on_watch_event`), so this is one event vocabulary
    // on both sides of the socket.
    //
    // Not timing-sensitive: the re-attach is due one backoff step (100 ms) after
    // the cut and the wait allows a full TTL.
    let fixture = Fixture::start().await;
    let relay = CuttableRelay::in_front_of(fixture.gear.addr).await;
    let leader = relay.leader();

    let mut watch = lead(&leader).await;

    relay.cut();

    let next = tokio::time::timeout(TTL, watch.changed())
        .await
        .expect("a re-subscribe must be observable inside one TTL");
    println!("[election] the consumer observed: {next:?}");
    assert!(
        matches!(next, LeaderWatchEvent::Reset),
        "a re-established subscription is section 6.8's `Reset`, got: {next:?}"
    );
    assert!(
        watch.is_leader(),
        "and the claim is untouched across it - section 6.6, 'losing it costs a re-subscribe, \
         not a leadership change'"
    );

    drop(watch);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permanently_unreattachable_feed_still_keeps_the_claim() {
    // The give-up branch, which is the riskiest thing the fix adds. Once the
    // subscription has been **reaped** (§5.4.1) no re-`attach` can ever succeed:
    // `attach` answers `None` and the server returns `NotFound`, which is not
    // retryable. The pump must stop re-attaching and go on renewing anyway,
    // exactly as Profile 1 does after `None => cache_watch = None`. Stopping
    // instead would be `ELEC-1` again by a slower route, and retrying forever
    // would be a new bug of its own.
    //
    // The sweep is driven by hand with a zero grace window rather than waiting
    // out the shipped 15 s: what is under test is the *client's* response to an
    // unreattachable subscription, not the sweep's own timing, which
    // `remote_backends.rs` already covers.
    let fixture = Fixture::start().await;
    let relay = CuttableRelay::in_front_of(fixture.gear.addr).await;
    let leader = relay.leader();
    let contender = fixture.direct_leader();

    let watch = lead(&leader).await;

    relay.cut();

    // Polled, not slept: the server notices the departed reader through its
    // stream task's `tx.closed()` arm, and how fast that happens is scheduling,
    // not contract.
    let reaped = tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let report = fixture.gear.subscriptions.sweep(Duration::ZERO);
            if report.reaped_total() >= 1 {
                return report.reaped_total();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a broken stream must leave a reader-less entry for the sweep to reap");
    println!("[election] swept {reaped} subscription(s)");

    let stolen = taken_within(&contender, TTL * 2).await;
    assert_eq!(
        stolen, None,
        "a pump with no recoverable feed must keep renewing - the claim is a row in \
         the store and only these renewals sustain it (I7, I8); a contender took it \
         {stolen:?} in"
    );
    assert!(
        watch.is_leader(),
        "and the consumer still believes it, because nothing revoked it"
    );

    drop(watch);
    fixture.stop().await;
}

// A fake server that wins the join and then refuses the subscription

#[derive(Default)]
struct Tally {
    joins: AtomicU64,
    resigns: AtomicU64,
    renews: AtomicU64,
}

impl Tally {
    fn get(&self) -> (u64, u64, u64) {
        (
            self.joins.load(Ordering::SeqCst),
            self.resigns.load(Ordering::SeqCst),
            self.renews.load(Ordering::SeqCst),
        )
    }
}

/// Answers `join` with the configured outcome and `await_change` with the bare
/// `NotFound` the shipped server really returns when the subscription is not on
/// the replica serving the call (`api/grpc/leader.rs`).
///
/// A fake rather than the real gear because the real one has no way to fail
/// `await_change` while succeeding `join` — which is exactly the split a mesh
/// like Linkerd or Istio produces.
struct RefusesTheSubscription {
    tally: Arc<Tally>,
    /// `Leader` for the winner arm, `Follower` for the control arm.
    status: dto::WireLeaderStatus,
}

#[tonic::async_trait]
impl stubs::leader::leader_election_api_server::LeaderElectionApi for RefusesTheSubscription {
    async fn join(
        &self,
        _request: tonic::Request<stubs::leader::JoinRequest>,
    ) -> Result<tonic::Response<stubs::leader::LeaderJoined>, tonic::Status> {
        self.tally.joins.fetch_add(1, Ordering::SeqCst);
        let token = match self.status {
            dto::WireLeaderStatus::Leader => dto::LeaseToken {
                name: ELECTION.to_owned(),
                owner: "unauthenticated".to_owned(),
                fence: 7,
            },
            // The zero token a follower receives, because `LeaderJoined.token`
            // is not optional on the wire (§6.6).
            _ => dto::LeaseToken {
                name: String::new(),
                owner: String::new(),
                fence: 0,
            },
        };
        Ok(tonic::Response::new(stubs::leader::LeaderJoined::from(
            dto::LeaderJoined {
                token,
                election_id: "sub-1".to_owned(),
                initial_status: self.status,
            },
        )))
    }

    async fn renew(
        &self,
        _request: tonic::Request<stubs::leader::LeaseRef>,
    ) -> Result<tonic::Response<stubs::leader::RenewResponse>, tonic::Status> {
        self.tally.renews.fetch_add(1, Ordering::SeqCst);
        Ok(tonic::Response::new(stubs::leader::RenewResponse::from(
            dto::RenewResponse { generation: 1 },
        )))
    }

    async fn resign(
        &self,
        request: tonic::Request<stubs::leader::LeaseRef>,
    ) -> Result<tonic::Response<stubs::leader::ResignResponse>, tonic::Status> {
        let lease = dto::LeaseRef::from(request.into_inner());
        println!(
            "[refused-subscription] resign for token name={:?} owner={:?} fence={}",
            lease.token.name, lease.token.owner, lease.token.fence
        );
        self.tally.resigns.fetch_add(1, Ordering::SeqCst);
        Ok(tonic::Response::new(stubs::leader::ResignResponse::from(
            dto::ResignResponse { generation: 1 },
        )))
    }

    type AwaitChangeStream = tokio_stream::wrappers::ReceiverStream<
        Result<stubs::leader::WireLeaderWatchEvent, tonic::Status>,
    >;

    async fn await_change(
        &self,
        _request: tonic::Request<stubs::leader::AwaitChangeRequest>,
    ) -> Result<tonic::Response<Self::AwaitChangeStream>, tonic::Status> {
        Err(tonic::Status::not_found("unknown election_id"))
    }
}

async fn serve_fake(
    status: dto::WireLeaderStatus,
) -> (
    Arc<Tally>,
    Arc<dyn cluster_sdk::LeaderElectionBackend>,
    tokio::sync::oneshot::Sender<()>,
) {
    let tally = Arc::new(Tally::default());
    let service = RefusesTheSubscription {
        tally: Arc::clone(&tally),
        status,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let addr = listener.local_addr().expect("has an address");
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        Server::builder()
            .add_service(
                stubs::leader::leader_election_api_server::LeaderElectionApiServer::new(service),
            )
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _stopped = shutdown_rx.await;
            })
            .await
            .expect("the fake server runs");
    });
    (tally, leader_at(addr), shutdown)
}

#[tokio::test]
async fn a_won_claim_is_given_back_when_the_subscription_cannot_be_opened() {
    // `enrol` calls `join_once` - which takes the lease server-side -
    // then `subscribe`, then `?`-propagates a subscribe failure. No pump exists
    // yet, so nothing renews and nothing resigns, and the election name is held
    // for a full TTL by a call that already returned an error. Profile 1 has no
    // such window, which makes this a Profile-3-only failure mode (I1).
    let (tally, leader, shutdown) = serve_fake(dto::WireLeaderStatus::Leader).await;

    let outcome = leader.elect_with_config(ELECTION, election_config()).await;
    let error = outcome.err().expect("the subscription failure propagates");
    println!("[refused-subscription] elect() error:  {error:?}");
    println!(
        "[refused-subscription] is_retryable(): {}",
        error.is_retryable()
    );

    // The resign is best-effort and off the caller's path, so it is polled for
    // rather than slept on.
    let _settled = tokio::time::timeout(EVENT_TIMEOUT, async {
        while tally.resigns.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    let (joins, resigns, renews) = tally.get();
    println!("[refused-subscription] joins observed:   {joins}");
    println!("[refused-subscription] resigns observed: {resigns}");
    assert_eq!(joins, 1, "exactly one join");
    assert_eq!(
        resigns, 1,
        "a claim won and then abandoned must be given back"
    );
    assert_eq!(
        renews, 0,
        "no pump was ever started, so nothing may be renewing"
    );

    let _stopped = shutdown.send(());
}

#[tokio::test]
async fn a_follower_whose_subscription_fails_resigns_nothing() {
    // The control for the test above, and the hazard it guards against: a
    // follower receives the *zero* token (empty name and owner, `fence: 0`)
    // because `LeaderJoined.token` is not optional on the wire. Resigning
    // unconditionally would send that zero token to the server on every lost
    // election, so the winner check must read `initial_status` and never the
    // token's shape (§6.6).
    //
    // The negative case, and it matters: without it, "resign on any subscribe
    // failure" would look just as green as the correct behaviour.
    let (tally, leader, shutdown) = serve_fake(dto::WireLeaderStatus::Follower).await;

    let outcome = leader.elect_with_config(ELECTION, election_config()).await;
    assert!(
        outcome.is_err(),
        "the subscription failure still propagates"
    );

    // Long enough that a stray resign would have landed.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (joins, resigns, _renews) = tally.get();
    println!("[refused-subscription] follower resigns observed: {resigns}");
    assert_eq!(joins, 1, "exactly one join");
    assert_eq!(
        resigns, 0,
        "a follower holds no claim, so it must send no resign - the zero token must \
         never reach the server"
    );

    let _stopped = shutdown.send(());
}

// B6 / B8 — a configurable fake leader gear
//
// `RefusesTheSubscription` above wins the join and refuses the feed; these tests
// need the *opposite* controls — a renew that hangs, answers slowly, or answers
// fine, paired with a feed that is silent or floods `Reset`s. A fake rather than
// the real gear because the real one cannot make `renew` hang while `join`
// succeeds, which is exactly the half-open connection B6 is about.

/// How the fake answers `renew`.
#[derive(Clone, Copy)]
enum RenewMode {
    /// Accept the call and never answer — the half-open connection B6 bounds.
    Hang,
    /// Answer `Ok`, but only after `d` — a slow-but-healthy renew.
    Slow(Duration),
    /// Answer `Ok` promptly.
    Answer,
}

/// What the fake pushes down the `await_change` feed.
#[derive(Clone, Copy)]
enum FeedMode {
    /// Open the stream and send nothing (it stays open).
    Silent,
    /// Flood `burst` `Reset`s at once, then one every `interval` — enough to fill
    /// and keep filling a gate consumer's buffer (B8).
    Flapping { burst: usize, interval: Duration },
}

struct FakeLeader {
    tally: Arc<Tally>,
    renew: RenewMode,
    feed: FeedMode,
}

fn reset_frame() -> stubs::leader::WireLeaderWatchEvent {
    stubs::leader::WireLeaderWatchEvent::from(dto::WireLeaderWatchEvent::reset())
}

#[tonic::async_trait]
impl stubs::leader::leader_election_api_server::LeaderElectionApi for FakeLeader {
    async fn join(
        &self,
        _request: tonic::Request<stubs::leader::JoinRequest>,
    ) -> Result<tonic::Response<stubs::leader::LeaderJoined>, tonic::Status> {
        // Only the very first join wins leadership; a re-claim after the incumbent
        // loses its deadline must come back a follower, so `is_leader()` stays
        // false once the claim has lapsed.
        let first = self.tally.joins.fetch_add(1, Ordering::SeqCst) == 0;
        let (token, status) = if first {
            (
                dto::LeaseToken {
                    name: ELECTION.to_owned(),
                    owner: "unauthenticated".to_owned(),
                    fence: 7,
                },
                dto::WireLeaderStatus::Leader,
            )
        } else {
            (
                dto::LeaseToken {
                    name: String::new(),
                    owner: String::new(),
                    fence: 0,
                },
                dto::WireLeaderStatus::Follower,
            )
        };
        Ok(tonic::Response::new(stubs::leader::LeaderJoined::from(
            dto::LeaderJoined {
                token,
                election_id: "sub-1".to_owned(),
                initial_status: status,
            },
        )))
    }

    async fn renew(
        &self,
        _request: tonic::Request<stubs::leader::LeaseRef>,
    ) -> Result<tonic::Response<stubs::leader::RenewResponse>, tonic::Status> {
        self.tally.renews.fetch_add(1, Ordering::SeqCst);
        match self.renew {
            RenewMode::Hang => {
                std::future::pending::<()>().await;
                unreachable!("a hanging renew never answers");
            }
            RenewMode::Slow(d) => tokio::time::sleep(d).await,
            RenewMode::Answer => {}
        }
        Ok(tonic::Response::new(stubs::leader::RenewResponse::from(
            dto::RenewResponse { generation: 1 },
        )))
    }

    async fn resign(
        &self,
        _request: tonic::Request<stubs::leader::LeaseRef>,
    ) -> Result<tonic::Response<stubs::leader::ResignResponse>, tonic::Status> {
        self.tally.resigns.fetch_add(1, Ordering::SeqCst);
        Ok(tonic::Response::new(stubs::leader::ResignResponse::from(
            dto::ResignResponse { generation: 1 },
        )))
    }

    type AwaitChangeStream = tokio_stream::wrappers::ReceiverStream<
        Result<stubs::leader::WireLeaderWatchEvent, tonic::Status>,
    >;

    async fn await_change(
        &self,
        _request: tonic::Request<stubs::leader::AwaitChangeRequest>,
    ) -> Result<tonic::Response<Self::AwaitChangeStream>, tonic::Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        match self.feed {
            FeedMode::Silent => {
                // Hold the sender so the stream stays open and silent.
                tokio::spawn(async move {
                    let _hold = tx;
                    std::future::pending::<()>().await;
                });
            }
            FeedMode::Flapping { burst, interval } => {
                tokio::spawn(async move {
                    for _ in 0..burst {
                        if tx.send(Ok(reset_frame())).await.is_err() {
                            return;
                        }
                    }
                    loop {
                        tokio::time::sleep(interval).await;
                        if tx.send(Ok(reset_frame())).await.is_err() {
                            return;
                        }
                    }
                });
            }
        }
        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

async fn serve_fake_leader(
    renew: RenewMode,
    feed: FeedMode,
) -> (
    Arc<Tally>,
    Arc<dyn cluster_sdk::LeaderElectionBackend>,
    tokio::sync::oneshot::Sender<()>,
) {
    let tally = Arc::new(Tally::default());
    let service = FakeLeader {
        tally: Arc::clone(&tally),
        renew,
        feed,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let addr = listener.local_addr().expect("has an address");
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        Server::builder()
            .add_service(
                stubs::leader::leader_election_api_server::LeaderElectionApiServer::new(service),
            )
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _stopped = shutdown_rx.await;
            })
            .await
            .expect("the fake server runs");
    });
    (tally, leader_at(addr), shutdown)
}

/// Confirms the initial `Status(Leader)` on a fresh watch.
async fn assert_leads(watch: &mut cluster_sdk::LeaderWatch) {
    let first = tokio::time::timeout(EVENT_TIMEOUT, watch.changed())
        .await
        .expect("an initial status arrives");
    assert!(
        matches!(first, LeaderWatchEvent::Status(LeaderStatus::Leader)),
        "expected leadership before testing what keeps it, got: {first:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renew_that_never_answers_loses_the_claim_within_one_ttl() {
    // B6(a). The server accepts `renew` and never answers. Without a per-RPC
    // bound the pump's timer freezes and `is_leader()` reports `Leader` until the
    // kernel gives up; with the bound plus the deadline authority the claim must
    // lapse at its deadline (~1 TTL), and `resign()` must still return.
    //
    // Margins (TTL = 1500 ms, cadence 500 ms, budget 2). Fixed: the deadline
    // takes the claim at ~1 TTL. Broken-timeout: the pump hangs on the first
    // renew and never loses. Broken-deadline (count only): each hung renew costs
    // the whole bound, so loss slips to ~1.67 TTL (~2500 ms). The `TTL + CADENCE`
    // (2000 ms) bound sits below that and well above the fixed case.
    let (tally, leader, shutdown) = serve_fake_leader(RenewMode::Hang, FeedMode::Silent).await;
    let mut watch = leader
        .elect_with_config(ELECTION, election_config())
        .await
        .expect("elects");
    assert_leads(&mut watch).await;

    let started = tokio::time::Instant::now();
    loop {
        if !watch.is_leader() {
            break;
        }
        assert!(
            started.elapsed() < TTL * 2,
            "is_leader stayed true past two TTLs: a half-open renew held Leader (renews seen: {})",
            tally.get().2
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let lost = started.elapsed();
    println!("[deadline] is_leader went false after {lost:?}");
    assert!(
        lost < TTL + CADENCE,
        "the claim must lapse at its deadline (~{TTL:?}), not the count-based bound; \
         lost after {lost:?}"
    );

    let resigned = tokio::time::timeout(EVENT_TIMEOUT, watch.resign()).await;
    assert!(
        resigned.is_ok(),
        "resign must return even though the renew RPC is wedged"
    );
    let _stopped = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_but_healthy_renew_keeps_the_claim() {
    // B6(c). A renew that answers at ~50% of the interval is inside the per-RPC
    // bound (one interval), so it must NOT lose the claim. This is the guard
    // against a too-tight renew timeout, which would turn normal latency into
    // spurious leadership loss.
    let (_tally, leader, shutdown) =
        serve_fake_leader(RenewMode::Slow(CADENCE / 2), FeedMode::Silent).await;
    let mut watch = leader
        .elect_with_config(ELECTION, election_config())
        .await
        .expect("elects");
    assert_leads(&mut watch).await;

    // Hold across two TTLs; a healthy-but-slow renew keeps extending the claim.
    tokio::time::sleep(TTL * 2).await;
    assert!(
        watch.is_leader(),
        "a renew at 50% of the interval must not lose the claim (too-tight-timeout guard)"
    );
    drop(watch);
    let _stopped = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gate_consumer_cannot_wedge_the_remote_pump() {
    // B8. A gate-pattern consumer that reads `status()` and never drains
    // `changed()` (which `LeaderWatch` documents and permits) must not be able to
    // wedge the pump. Before the fix every consumer-facing send was an `.await`
    // on the 32-slot buffer, so once it filled the pump parked in `send` forever:
    // renewal stopped and the latched snapshot kept answering `Leader`.
    //
    // The feed floods a burst that fills the buffer immediately, then trickles, so
    // a wedged pump stalls its renewals from the very start. The property is that
    // renewals *keep happening* through a full buffer — that is what holds the
    // claim — and that `resign()` returns.
    let (tally, leader, shutdown) = serve_fake_leader(
        RenewMode::Answer,
        FeedMode::Flapping {
            burst: 40,
            interval: Duration::from_millis(30),
        },
    )
    .await;
    // The gate consumer: hold the watch, never call `changed()`.
    let watch = leader
        .elect_with_config(ELECTION, fast_election())
        .await
        .expect("elects");
    assert!(
        watch.is_leader(),
        "leadership via the snapshot a gate consumer reads"
    );

    tokio::time::sleep(FAST_RENEWAL * 4).await;
    let renews_early = tally.get().2;
    // Nine nominal intervals against a floor of three renews: same reasoning as
    // `a_follower_pump_mints...` - the renewal cadence is a real timer that a
    // saturated runtime can starve, so the window is wide enough that genuine
    // progress is still decidable when the pump fires slower than its cadence.
    tokio::time::sleep(FAST_RENEWAL * 9).await;
    let renews_late = tally.get().2;
    println!("[wedge] renews {renews_early} -> {renews_late} through a full buffer");
    assert!(
        renews_late >= renews_early + 3,
        "the pump must keep renewing through a full event buffer; a gate consumer wedged it \
         (renews {renews_early} -> {renews_late})"
    );
    assert!(
        watch.is_leader(),
        "and the incumbent still holds the claim, because renewal never stopped"
    );

    let resigned = tokio::time::timeout(EVENT_TIMEOUT, watch.resign()).await;
    assert!(
        resigned.is_ok(),
        "resign must return with a full event buffer"
    );
    let _stopped = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wedged_remote_consumer_is_told_it_lagged() {
    // B8, the announced-loss half: dropping an event is safe only because the
    // consumer is told. A consumer that fell behind the flood receives a `Lagged`
    // accounting for the drops before it sees the next event.
    let (_tally, leader, shutdown) = serve_fake_leader(
        RenewMode::Answer,
        FeedMode::Flapping {
            burst: 60,
            interval: Duration::from_millis(20),
        },
    )
    .await;
    let mut watch = leader
        .elect_with_config(ELECTION, fast_election())
        .await
        .expect("elects");

    // Let the burst overrun the buffer and the owed `Lagged` accumulate.
    tokio::time::sleep(FAST_RENEWAL * 3).await;

    // Drain: the trickle keeps offering, so once we have made room the owed
    // `Lagged` is flushed ahead of the next event.
    let mut saw_lagged = false;
    for _ in 0..120 {
        match tokio::time::timeout(EVENT_TIMEOUT, watch.changed()).await {
            Ok(LeaderWatchEvent::Lagged { dropped }) => {
                assert!(dropped > 0, "a Lagged must account for at least one drop");
                saw_lagged = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(
        saw_lagged,
        "a wedged consumer must be told it lagged, not silently lose events"
    );
    drop(watch);
    let _stopped = shutdown.send(());
}
