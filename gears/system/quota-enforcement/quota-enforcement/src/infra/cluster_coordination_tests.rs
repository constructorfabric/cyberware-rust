#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use cluster_sdk::{ClusterError, ClusterProfile, LeaderStatus, LeaderWatch};
use tokio_util::sync::CancellationToken;
use toolkit::ClientHub;

use super::{ClusterCoordinationBinding, ElectionTiming, QuotaEnforcementProfile, drive};
use crate::domain::error::DomainError;
use crate::domain::ports::coordination::{CoordinatorBinding, LeaderWork, SingletonScope};
use crate::test_support::{AdvisoryOnlyLeader, OtherProfile, wire_cluster, wire_cluster_with};

#[derive(Default)]
struct Body {
    starts: AtomicUsize,
    stops: AtomicUsize,
}

impl Body {
    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    fn stops(&self) -> usize {
        self.stops.load(Ordering::SeqCst)
    }
}

/// A body that counts its runs and returns when its token fires.
fn counting_work(body: Arc<Body>) -> LeaderWork {
    Arc::new(move |token: CancellationToken| {
        let body = body.clone();
        Box::pin(async move {
            body.starts.fetch_add(1, Ordering::SeqCst);
            token.cancelled().await;
            body.stops.fetch_add(1, Ordering::SeqCst);
        })
    })
}

/// A body that raises a flag while it runs.
fn flag_work(flag: Arc<AtomicBool>) -> LeaderWork {
    Arc::new(move |token: CancellationToken| {
        let flag = flag.clone();
        Box::pin(async move {
            flag.store(true, Ordering::SeqCst);
            token.cancelled().await;
            flag.store(false, Ordering::SeqCst);
        })
    })
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out: {what}");
}

fn fast_timing() -> ElectionTiming {
    ElectionTiming::new(Duration::from_millis(400), 1, Duration::from_secs(1)).expect("timing")
}

#[tokio::test]
async fn drive_starts_the_body_on_election_stops_it_on_loss_and_resigns_on_shutdown() {
    let (sender, mut resigns, watch) = LeaderWatch::channel(8, LeaderStatus::Follower);
    let body = Arc::new(Body::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(drive(
        SingletonScope::LeaseSweeper,
        watch,
        shutdown.clone(),
        counting_work(body.clone()),
        Duration::from_secs(1),
    ));

    sender
        .send_status(LeaderStatus::Leader)
        .await
        .expect("watch alive");
    wait_until("body started", || body.starts() == 1).await;
    assert_eq!(body.stops(), 0);

    sender
        .send_status(LeaderStatus::Lost)
        .await
        .expect("watch alive");
    wait_until("body stopped on loss", || body.stops() == 1).await;

    sender
        .send_status(LeaderStatus::Leader)
        .await
        .expect("watch alive");
    wait_until("body restarted on re-election", || body.starts() == 2).await;

    shutdown.cancel();
    let responder = resigns
        .recv()
        .await
        .expect("shutdown resigns the election while the watch is still owned");
    responder.respond(Ok(()));
    task.await
        .expect("drive joins")
        .expect("drive returns Ok after the resign");
    assert_eq!(body.stops(), 2, "the body stopped before the resign");
}

#[tokio::test]
async fn drive_reports_a_terminal_close_as_cluster_unavailable() {
    let (mut sender, _resigns, watch) = LeaderWatch::channel(8, LeaderStatus::Follower);
    let body = Arc::new(Body::default());
    let task = tokio::spawn(drive(
        SingletonScope::RetentionSweeper,
        watch,
        CancellationToken::new(),
        counting_work(body.clone()),
        Duration::from_secs(1),
    ));
    sender
        .send_status(LeaderStatus::Leader)
        .await
        .expect("watch alive");
    wait_until("body started", || body.starts() == 1).await;

    sender.try_close(ClusterError::Shutdown);
    let err = task
        .await
        .expect("drive joins")
        .expect_err("a closed election is an error for the caller");
    assert!(matches!(err, DomainError::ClusterUnavailable(_)), "{err:?}");
    assert_eq!(body.stops(), 1, "the body stopped before the error");
}

#[tokio::test]
async fn resolve_succeeds_against_a_linearizable_standalone_backend() {
    let hub = Arc::new(ClientHub::new());
    let fixture = wire_cluster(&hub);
    let binding = ClusterCoordinationBinding::new(hub.clone(), fast_timing());
    binding
        .resolve()
        .await
        .expect("the standalone backend declares a linearizable election");
    fixture.stop().await;
}

#[tokio::test]
async fn resolve_fails_when_the_quota_enforcement_profile_is_unbound() {
    let hub = Arc::new(ClientHub::new());
    let fixture = wire_cluster_with(&hub, OtherProfile, None);
    let binding = ClusterCoordinationBinding::new(hub.clone(), fast_timing());
    let err = binding
        .resolve()
        .await
        .err()
        .expect("no backend is bound for the quota-enforcement profile");
    match &err {
        DomainError::ClusterUnavailable(reason) => {
            assert!(reason.contains(QuotaEnforcementProfile::NAME), "{reason}");
        }
        other => panic!("expected ClusterUnavailable, got {other:?}"),
    }
    fixture.stop().await;
}

#[tokio::test]
async fn resolve_requires_a_linearizable_election() {
    let hub = Arc::new(ClientHub::new());
    let fixture = wire_cluster_with(
        &hub,
        QuotaEnforcementProfile,
        Some(Arc::new(AdvisoryOnlyLeader)),
    );
    let binding = ClusterCoordinationBinding::new(hub.clone(), fast_timing());
    let err = binding
        .resolve()
        .await
        .err()
        .expect("an advisory-only backend fails the requirement");
    match &err {
        DomainError::ClusterUnavailable(reason) => {
            assert!(reason.contains("Linearizable"), "{reason}");
        }
        other => panic!("expected ClusterUnavailable, got {other:?}"),
    }
    fixture.stop().await;
}

#[tokio::test]
async fn two_participants_hand_over_when_the_leader_shuts_down() {
    let hub = Arc::new(ClientHub::new());
    let fixture = wire_cluster(&hub);
    let coordinator = ClusterCoordinationBinding::new(hub.clone(), fast_timing())
        .resolve()
        .await
        .expect("resolves");

    let leading_a = Arc::new(AtomicBool::new(false));
    let leading_b = Arc::new(AtomicBool::new(false));
    let shutdown_a = CancellationToken::new();
    let shutdown_b = CancellationToken::new();
    let run = |shutdown: CancellationToken, flag: Arc<AtomicBool>| {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .run_while_leader(SingletonScope::LeaseSweeper, shutdown, flag_work(flag))
                .await
        })
    };
    let task_a = run(shutdown_a.clone(), leading_a.clone());
    let task_b = run(shutdown_b.clone(), leading_b.clone());

    let is_leading = |flag: &Arc<AtomicBool>| flag.load(Ordering::SeqCst);
    wait_until("one participant leads", || {
        is_leading(&leading_a) || is_leading(&leading_b)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        is_leading(&leading_a) ^ is_leading(&leading_b),
        "exactly one participant leads in steady state"
    );

    let (leader_shutdown, leader_task, follower_flag, follower_shutdown, follower_task) =
        if is_leading(&leading_a) {
            (shutdown_a, task_a, leading_b, shutdown_b, task_b)
        } else {
            (shutdown_b, task_b, leading_a, shutdown_a, task_a)
        };
    leader_shutdown.cancel();
    leader_task
        .await
        .expect("leader task joins")
        .expect("the leader resigns and returns Ok");
    wait_until("the follower takes over after the resign", || {
        is_leading(&follower_flag)
    })
    .await;

    follower_shutdown.cancel();
    follower_task
        .await
        .expect("follower task joins")
        .expect("the successor resigns and returns Ok");
    fixture.stop().await;
}
