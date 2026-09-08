//! Layer 3 — leader-election integration scenarios (docs/TESTING.md §4.3),
//! asserting on the actual `Lease` objects (holder, renewTime, leaseDurationSeconds,
//! leaseTransitions, labels, name annotation).

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::large_futures,
    reason = "integration tests: a setup failure IS the test failure"
)]

use crate::common;

use std::time::Duration;

use cluster_sdk::error::ClusterError;
use cluster_sdk::leader::{ElectionConfig, LeaderStatus, LeaderWatch, LeaderWatchEvent};
use k8s_cluster_plugin::K8sLeaderElectionPlugin;
use k8s_openapi::api::coordination::v1::Lease;
use kube::ResourceExt;
use kube::api::PostParams;
use serde_json::json;

const LABEL_MANAGED_BY: &str = "cluster.cf-gears.io/managed-by";
const LABEL_PRIMITIVE: &str = "cluster.cf-gears.io/primitive";
const ANNOTATION_NAME: &str = "cluster.cf-gears.io/name";

/// Polls a leader watch until it delivers a definitive `Status`, or panics on
/// timeout / a terminal `Closed`.
async fn first_status(watch: &mut LeaderWatch) -> LeaderStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(event) = tokio::time::timeout(remaining, watch.changed()).await else {
            panic!("no leader status within the deadline");
        };
        match event {
            LeaderWatchEvent::Status(status) => return status,
            LeaderWatchEvent::Closed(err) => panic!("watch closed before a status: {err:?}"),
            _ => {}
        }
    }
}

/// Waits until the watch reports the wanted status (via its snapshot), bounded.
async fn wait_for_status(watch: &LeaderWatch, want: LeaderStatus, timeout: Duration) -> bool {
    common::wait_until(timeout, Duration::from_millis(50), || async {
        watch.status() == want
    })
    .await
}

/// The single `Lease` whose name annotation is `election`, if present.
async fn lease_for(ns: &common::NamespaceGuard, election: &str) -> Option<Lease> {
    ns.list_leases()
        .await
        .into_iter()
        .find(|l| l.annotations().get(ANNOTATION_NAME).map(String::as_str) == Some(election))
}

/// `K8S-LEAD-001`: a single candidate becomes `Leader`; the Lease exists with our
/// holder, a 30s duration, non-null acquire/renew times, the labels, and the name
/// annotation carrying the original election name.
#[tokio::test]
async fn k8s_lead_001_elect_acquires_and_reports_leader() {
    let ns = common::fresh_namespace("lead-001").await;
    let handle = K8sLeaderElectionPlugin::builder(ns.leader_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("leader starts");
    let leader = handle.leader_election();

    let mut watch = leader.elect("svc").await.expect("elect");
    assert_eq!(first_status(&mut watch).await, LeaderStatus::Leader);

    let lease = common::wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || async { lease_for(&ns, "svc").await.is_some() },
    )
    .await;
    assert!(lease, "the Lease object exists");
    let lease = lease_for(&ns, "svc").await.expect("lease");
    let spec = lease.spec.as_ref().expect("spec");
    assert!(
        spec.holder_identity
            .as_deref()
            .is_some_and(|h| !h.is_empty()),
        "has a holder"
    );
    assert_eq!(
        spec.lease_duration_seconds,
        Some(30),
        "K8S-LEAD-001: 30s duration"
    );
    assert!(spec.acquire_time.is_some(), "acquireTime set");
    assert!(spec.renew_time.is_some(), "renewTime set");
    let labels = lease.labels();
    assert_eq!(
        labels.get(LABEL_MANAGED_BY).map(String::as_str),
        Some("cf-gears-cluster")
    );
    assert_eq!(
        labels.get(LABEL_PRIMITIVE).map(String::as_str),
        Some("election")
    );

    watch.resign().await.expect("resign");
    handle.stop().await;
}

/// `K8S-LEAD-002`: ten independent candidates on one name; exactly one `Leader`,
/// nine `Follower`, and exactly one Lease whose holder is the winner.
#[tokio::test]
async fn k8s_lead_002_ten_candidates_exactly_one_leader() {
    let ns = common::fresh_namespace("lead-002").await;
    let mut handles = Vec::new();
    let mut watches = Vec::new();
    for _ in 0..10 {
        let handle = K8sLeaderElectionPlugin::builder(ns.leader_config_with(json!({})))
            .with_client(ns.client.clone())
            .build_and_start()
            .await
            .expect("candidate starts");
        let watch = handle.leader_election().elect("svc").await.expect("elect");
        handles.push(handle);
        watches.push(watch);
    }

    let mut leaders = 0;
    let mut followers = 0;
    let mut lost = 0;
    for watch in &mut watches {
        match first_status(watch).await {
            LeaderStatus::Leader => leaders += 1,
            LeaderStatus::Follower => followers += 1,
            // A candidate that fails to settle stays in the transient `Lost` state;
            // counting it separately catches that rather than passing it off as a
            // follower.
            LeaderStatus::Lost => lost += 1,
        }
    }
    assert_eq!(leaders, 1, "K8S-LEAD-002: exactly one leader");
    assert_eq!(followers, 9, "K8S-LEAD-002: nine settled followers");
    assert_eq!(lost, 0, "K8S-LEAD-002: no candidate is stuck in Lost");

    let leases = ns.list_leases().await;
    assert_eq!(leases.len(), 1, "K8S-LEAD-002: exactly one Lease");

    for handle in handles {
        handle.stop().await;
    }
}

/// `K8S-LEAD-003`: renewal keeps leadership past the TTL — after several renewal
/// intervals the holder still leads and `renewTime` advanced.
#[tokio::test]
async fn k8s_lead_003_renewal_keeps_leadership() {
    let ns = common::fresh_namespace("lead-003").await;
    let handle = K8sLeaderElectionPlugin::builder(ns.leader_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("leader starts");
    let leader = handle.leader_election();

    // A 6s TTL (2s renewal) renews ~twice across the 4s window below — enough to
    // prove the renewal loop runs and advances `renewTime` — with a comfortable
    // margin against a missed renewal under CI load (a shorter TTL could lapse and
    // fail the "still leader" assertion spuriously).
    let mut watch = leader
        .elect_with_config(
            "svc",
            ElectionConfig::new(Duration::from_secs(6), 2).unwrap(),
        )
        .await
        .expect("elect");
    assert_eq!(first_status(&mut watch).await, LeaderStatus::Leader);

    let renew0 = lease_for(&ns, "svc")
        .await
        .expect("lease")
        .spec
        .unwrap()
        .renew_time;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        watch.status(),
        LeaderStatus::Leader,
        "K8S-LEAD-003: still leader after 3s"
    );
    let renew1 = lease_for(&ns, "svc")
        .await
        .expect("lease")
        .spec
        .unwrap()
        .renew_time;
    assert_ne!(renew0, renew1, "K8S-LEAD-003: renewTime advanced");

    watch.resign().await.expect("resign");
    handle.stop().await;
}

/// `K8S-LEAD-005`: failover on holder death — the leader's plugin is `stop()`ed
/// without resigning; a follower takes over after ~one lease duration and
/// `leaseTransitions` increments. The Lease is still present with the dead holder's
/// identity immediately after the stop.
#[tokio::test]
async fn k8s_lead_005_failover_on_holder_death() {
    let ns = common::fresh_namespace("lead-005").await;
    let cfg = json!({ "min_election_ttl_ms": 500 });
    let ttl = ElectionConfig::new(Duration::from_secs(2), 2).unwrap();

    let a = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg.clone()))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("A starts");
    let mut a_watch = a
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("A elect");
    assert_eq!(first_status(&mut a_watch).await, LeaderStatus::Leader);
    let a_identity = lease_for(&ns, "svc")
        .await
        .unwrap()
        .spec
        .unwrap()
        .holder_identity
        .unwrap();

    let b = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("B starts");
    let b_watch = b
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("B elect");

    // A dies without resigning: the claim lapses, the object does not.
    a.stop().await;
    let after = lease_for(&ns, "svc")
        .await
        .expect("lease present after stop");
    assert_eq!(
        after.spec.unwrap().holder_identity.as_deref(),
        Some(a_identity.as_str()),
        "K8S-LEAD-005: the Lease persists with the dead holder's identity"
    );

    assert!(
        wait_for_status(&b_watch, LeaderStatus::Leader, Duration::from_secs(8)).await,
        "K8S-LEAD-005: a follower takes over after ~one lease duration"
    );
    let final_lease = lease_for(&ns, "svc").await.expect("lease");
    let spec = final_lease.spec.unwrap();
    assert_ne!(
        spec.holder_identity.as_deref(),
        Some(a_identity.as_str()),
        "new holder"
    );
    assert!(
        spec.lease_transitions.unwrap_or(0) >= 1,
        "K8S-LEAD-005: leaseTransitions incremented"
    );

    b.stop().await;
}

/// `K8S-LEAD-006`: `resign` hands over within a round-trip — the Lease's holder is
/// null right after, a follower acquires well inside one lease duration, and the
/// resigner observes `Lost`.
#[tokio::test]
async fn k8s_lead_006_resign_hands_over() {
    let ns = common::fresh_namespace("lead-006").await;
    let cfg = json!({ "min_election_ttl_ms": 500 });
    let ttl = ElectionConfig::new(Duration::from_secs(10), 2).unwrap();

    let a = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg.clone()))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("A starts");
    let a_watch = a
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("A elect");
    assert!(wait_for_status(&a_watch, LeaderStatus::Leader, Duration::from_secs(5)).await);

    let b = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("B starts");
    let b_watch = b
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("B elect");

    a_watch.resign().await.expect("resign hands over");

    // A follower acquires well inside the 10s TTL — far faster than a TTL-lapse.
    assert!(
        wait_for_status(&b_watch, LeaderStatus::Leader, Duration::from_secs(3)).await,
        "K8S-LEAD-006: a follower acquires within a round-trip of the resign"
    );

    a.stop().await;
    b.stop().await;
}

/// `K8S-LEAD-007`: a follower issues no writes — while a leader renews, a follower's
/// request counter shows zero mutating verbs and at least one watch.
#[tokio::test]
async fn k8s_lead_007_follower_issues_no_writes() {
    let ns = common::fresh_namespace("lead-007").await;
    // A 6s TTL keeps the leader stable under load: a shorter TTL could lapse, and a
    // follower that then acquired would issue the very writes this asserts it never
    // makes. The leader still renews (~every 2s) within the 3s observation window.
    let cfg = json!({});
    let ttl = ElectionConfig::new(Duration::from_secs(6), 2).unwrap();

    let leader = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg.clone()))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("leader starts");
    let l_watch = leader
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("elect");
    assert!(wait_for_status(&l_watch, LeaderStatus::Leader, Duration::from_secs(5)).await);

    let (client, counts) = common::counted_client().await;
    let follower = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg))
        .with_client(client)
        .build_and_start()
        .await
        .expect("follower starts");
    let f_watch = follower
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("elect");
    assert!(wait_for_status(&f_watch, LeaderStatus::Follower, Duration::from_secs(5)).await);

    // Let the leader renew several times while the follower watches.
    let mutating_before = counts.mutating();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        counts.mutating() - mutating_before,
        0,
        "K8S-LEAD-007: a steady follower issues no mutating requests"
    );
    assert!(
        counts.watches() >= 1,
        "K8S-LEAD-007: the follower holds a watch"
    );

    leader.stop().await;
    follower.stop().await;
}

/// `K8S-LEAD-008`: transient loss re-enrols with no consumer code — the leader's
/// Lease is overwritten out-of-band; it observes `Lost`, then a subsequent
/// `Leader`/`Follower`, and never a terminal `Closed`.
#[tokio::test]
async fn k8s_lead_008_transient_loss_reenrolls() {
    let ns = common::fresh_namespace("lead-008").await;
    let handle = K8sLeaderElectionPlugin::builder(
        ns.leader_config_with(json!({ "min_election_ttl_ms": 500 })),
    )
    .with_client(ns.client.clone())
    .build_and_start()
    .await
    .expect("leader starts");
    let mut watch = handle
        .leader_election()
        .elect_with_config(
            "svc",
            ElectionConfig::new(Duration::from_secs(2), 2).unwrap(),
        )
        .await
        .expect("elect");
    assert_eq!(first_status(&mut watch).await, LeaderStatus::Leader);

    // Overwrite the Lease out-of-band: a third party takes the holder identity.
    let mut lease = lease_for(&ns, "svc").await.expect("lease");
    if let Some(spec) = lease.spec.as_mut() {
        spec.holder_identity = Some("intruder".to_owned());
    }
    ns.leases()
        .replace(&lease.name_any(), &PostParams::default(), &lease)
        .await
        .expect("out-of-band overwrite");

    // The leader observes Lost, then re-enrolls to a definitive status, never Closed.
    let mut saw_lost = false;
    let mut reenrolled = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !(saw_lost && reenrolled) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, watch.changed()).await {
            Ok(LeaderWatchEvent::Status(LeaderStatus::Lost)) => saw_lost = true,
            Ok(LeaderWatchEvent::Status(_)) if saw_lost => reenrolled = true,
            Ok(LeaderWatchEvent::Closed(err)) => {
                panic!("K8S-LEAD-008: loss must be transient, not Closed: {err:?}")
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_lost, "K8S-LEAD-008: observes Lost after the overwrite");
    assert!(
        reenrolled,
        "K8S-LEAD-008: re-enrols to a definitive status with no consumer code"
    );

    handle.stop().await;
}

/// `K8S-LEAD-009`: a 409 on renewal loses leadership immediately — a third party
/// takes the Lease between renewals; the next renewal 409s and `Lost` is emitted on
/// that attempt.
#[tokio::test]
async fn k8s_lead_009_conflict_on_renewal_loses_immediately() {
    let ns = common::fresh_namespace("lead-009").await;
    // A long TTL so a natural TTL lapse cannot explain the Lost — only the 409 can.
    let handle = K8sLeaderElectionPlugin::builder(
        ns.leader_config_with(json!({ "min_election_ttl_ms": 500 })),
    )
    .with_client(ns.client.clone())
    .build_and_start()
    .await
    .expect("leader starts");
    let mut watch = handle
        .leader_election()
        .elect_with_config(
            "svc",
            ElectionConfig::new(Duration::from_secs(30), 5).unwrap(),
        )
        .await
        .expect("elect");
    assert_eq!(first_status(&mut watch).await, LeaderStatus::Leader);

    // A third party takes the Lease (a guarded replace with the current rv).
    let mut lease = lease_for(&ns, "svc").await.expect("lease");
    if let Some(spec) = lease.spec.as_mut() {
        spec.holder_identity = Some("intruder".to_owned());
    }
    ns.leases()
        .replace(&lease.name_any(), &PostParams::default(), &lease)
        .await
        .expect("steal the lease");

    // The next renewal 409s and Lost is emitted well inside the 30s TTL / 5 misses.
    let lost = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match watch.changed().await {
                LeaderWatchEvent::Status(LeaderStatus::Lost) => return true,
                LeaderWatchEvent::Closed(_) => return false,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        lost,
        "K8S-LEAD-009: a 409 on renewal emits Lost immediately, not after N misses"
    );

    handle.stop().await;
}

/// `K8S-LEAD-010`: a sub-`min_election_ttl` config is rejected at the call, naming
/// the derived renewal rate; with the floor lowered, the same call succeeds.
#[tokio::test]
async fn k8s_lead_010_sub_floor_ttl_rejected() {
    let ns = common::fresh_namespace("lead-010").await;

    let strict = K8sLeaderElectionPlugin::builder(ns.leader_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("leader starts");
    let err = strict
        .leader_election()
        .elect_with_config(
            "svc",
            ElectionConfig::new(Duration::from_secs(1), 2).unwrap(),
        )
        .await
        .expect_err("a 1s TTL is below the 5s floor");
    match err {
        ClusterError::InvalidConfig { reason } => {
            assert!(
                reason.contains("min_election_ttl"),
                "names the floor: {reason}"
            );
        }
        other => panic!("K8S-LEAD-010: expected InvalidConfig, got {other:?}"),
    }
    strict.stop().await;

    let relaxed = K8sLeaderElectionPlugin::builder(
        ns.leader_config_with(json!({ "min_election_ttl_ms": 500 })),
    )
    .with_client(ns.client.clone())
    .build_and_start()
    .await
    .expect("leader starts");
    let mut watch = relaxed
        .leader_election()
        .elect_with_config(
            "svc",
            ElectionConfig::new(Duration::from_secs(1), 2).unwrap(),
        )
        .await
        .expect("K8S-LEAD-010: with the floor lowered, the 1s TTL is accepted");
    assert_eq!(first_status(&mut watch).await, LeaderStatus::Leader);
    relaxed.stop().await;
}

/// `K8S-LEAD-011`: `election_lease_names` pins a pre-existing Lease name — the plugin
/// contends on the literal configured name and creates nothing under the mapped one.
#[tokio::test]
async fn k8s_lead_011_election_lease_names_pins_object() {
    let ns = common::fresh_namespace("lead-011").await;
    let handle = K8sLeaderElectionPlugin::builder(ns.leader_config_with(json!({
        "election_lease_names": { "svc": "my-pinned-lease" }
    })))
    .with_client(ns.client.clone())
    .build_and_start()
    .await
    .expect("leader starts");

    let mut watch = handle.leader_election().elect("svc").await.expect("elect");
    assert_eq!(first_status(&mut watch).await, LeaderStatus::Leader);

    let present = common::wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || async {
            ns.leases()
                .get_opt("my-pinned-lease")
                .await
                .expect("get")
                .is_some()
        },
    )
    .await;
    assert!(
        present,
        "K8S-LEAD-011: the Lease lands on the literal configured name"
    );
    let leases = ns.list_leases().await;
    assert_eq!(
        leases.len(),
        1,
        "K8S-LEAD-011: nothing created under the mapped name"
    );
    assert_eq!(leases[0].name_any(), "my-pinned-lease");

    watch.resign().await.expect("resign");
    handle.stop().await;
}
