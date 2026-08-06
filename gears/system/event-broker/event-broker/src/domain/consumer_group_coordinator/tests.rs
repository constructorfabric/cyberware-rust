use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::domain::streaming::assignment::AssignmentDelta;
use toolkit_gts::GtsInstanceId;

use super::{ConsumerGroupCoordinator, MemberStatus, TopicInterest, range_split};

fn topic(suffix: &str, partitions: i32) -> TopicInterest {
    TopicInterest {
        id: GtsInstanceId::try_new(&format!("gts.cf.core.events.topic.v1~x.eb.t1.{suffix}.v1"))
            .unwrap(),
        partitions,
    }
}

fn group_id(suffix: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(&format!(
        "gts.cf.core.events.consumer_group.v1~x.eb.cg.{suffix}.v1"
    ))
    .unwrap()
}

// --- range_split ---

#[test]
fn range_split_single_member_gets_all() {
    let members = vec![Uuid::new_v4()];
    let splits = range_split(4, &members);
    assert_eq!(splits, vec![vec![0, 1, 2, 3]]);
}

#[test]
fn range_split_two_members_two_plus_two() {
    let members = vec![Uuid::new_v4(), Uuid::new_v4()];
    let splits = range_split(4, &members);
    assert_eq!(splits[0].len() + splits[1].len(), 4);
    assert_eq!(splits[0], vec![0, 1]);
    assert_eq!(splits[1], vec![2, 3]);
}

#[test]
fn range_split_three_members_four_partitions() {
    let members = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let splits = range_split(4, &members);
    let total: usize = splits.iter().map(|s| s.len()).sum();
    assert_eq!(total, 4);
    // floor(0*4/3)=0..floor(1*4/3)=1 → [0]
    // floor(1*4/3)=1..floor(2*4/3)=2 → [1]
    // floor(2*4/3)=2..floor(3*4/3)=4 → [2,3]
    assert_eq!(splits[0], vec![0]);
    assert_eq!(splits[1], vec![1]);
    assert_eq!(splits[2], vec![2, 3]);
}

#[test]
fn range_split_five_partitions_two_members() {
    let members = vec![Uuid::new_v4(), Uuid::new_v4()];
    let splits = range_split(5, &members);
    // floor(0*5/2)=0..floor(1*5/2)=2 → [0,1]
    // floor(1*5/2)=2..floor(2*5/2)=5 → [2,3,4]
    assert_eq!(splits[0], vec![0, 1]);
    assert_eq!(splits[1], vec![2, 3, 4]);
}

#[test]
fn range_split_empty_members() {
    assert!(range_split(4, &[]).is_empty());
}

#[test]
fn range_split_zero_partitions() {
    let members = vec![Uuid::new_v4(), Uuid::new_v4()];
    let splits = range_split(0, &members);
    assert_eq!(splits.len(), 2);
    assert!(splits.iter().all(|s| s.is_empty()));
}

// --- ConsumerGroupCoordinator::join ---

#[test]
fn first_join_version_one_all_partitions() {
    let coordinator = ConsumerGroupCoordinator::new();
    let group = group_id("g1");
    let sub_id = Uuid::new_v4();

    let (assigned, version, _) = coordinator.join(
        &group,
        sub_id,
        &[topic("topic", 4)],
        Duration::from_secs(30),
    );

    assert_eq!(version, 1);
    let mut parts: Vec<i32> = assigned.iter().map(|a| a.partition).collect();
    parts.sort_unstable();
    assert_eq!(parts, vec![0, 1, 2, 3]);
}

#[test]
fn second_join_splits_and_increments_version() {
    let coordinator = ConsumerGroupCoordinator::new();
    let group = group_id("g2");

    let (_, v1, _) = coordinator.join(
        &group,
        Uuid::new_v4(),
        &[topic("topic", 4)],
        Duration::from_secs(30),
    );
    assert_eq!(v1, 1);

    let (assigned_b, v2, _) = coordinator.join(
        &group,
        Uuid::new_v4(),
        &[topic("topic", 4)],
        Duration::from_secs(30),
    );
    assert_eq!(v2, 2);
    assert_eq!(assigned_b.len(), 2); // 4 partitions / 2 members = 2 each
}

#[test]
fn third_join_four_partitions_all_members_covered() {
    let coordinator = ConsumerGroupCoordinator::new();
    let group = group_id("g3");

    coordinator.join(
        &group,
        Uuid::new_v4(),
        &[topic("topic", 4)],
        Duration::from_secs(30),
    );
    coordinator.join(
        &group,
        Uuid::new_v4(),
        &[topic("topic", 4)],
        Duration::from_secs(30),
    );
    let (assigned_c, v3, _) = coordinator.join(
        &group,
        Uuid::new_v4(),
        &[topic("topic", 4)],
        Duration::from_secs(30),
    );

    assert_eq!(v3, 3);
    assert!(!assigned_c.is_empty());
    assert!(assigned_c.len() <= 2);
}

// --- ConsumerGroupCoordinator::leave ---

#[tokio::test]
async fn leave_sends_terminal_to_survivor() {
    let coordinator = Arc::new(ConsumerGroupCoordinator::new());
    let group = group_id("g4");

    let sub_a = Uuid::new_v4();
    let sub_b = Uuid::new_v4();
    coordinator.join(&group, sub_a, &[topic("topic", 4)], Duration::from_secs(30));
    coordinator.join(&group, sub_b, &[topic("topic", 4)], Duration::from_secs(30));

    let coordinator = Arc::new(coordinator);
    let (mut generations, _membership) =
        ConsumerGroupCoordinator::subscribe(&coordinator, &group, sub_a)
            .expect("sub_a is a member");
    let before = generations.borrow_and_update().clone();

    coordinator.leave(&group, sub_b);

    assert!(
        generations
            .has_changed()
            .expect("sender outlives the receiver"),
        "the survivor's assignment must be published when a sibling leaves"
    );
    let after = generations.borrow_and_update().clone();

    // The coordinator states the assignment; classifying it is the session's
    // job, so this asserts the input to that rather than a frame kind. A
    // survivor taking over a departed member's partitions is a *gain*, which
    // `apply` turns into a terminal close because it holds no cursor for them.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::Gain,
        "before {before:?}, after {after:?}"
    );
}

#[test]
fn leave_last_member_removes_group() {
    let coordinator = ConsumerGroupCoordinator::new();
    let group = group_id("g5");
    let sub_a = Uuid::new_v4();
    let _ = coordinator.join(&group, sub_a, &[topic("topic", 2)], Duration::from_secs(30));
    coordinator.leave(&group, sub_a);

    assert!(!coordinator.state.lock().unwrap().contains_key(&group));
}

// --- Dead-sender eviction on JOIN ---

#[test]
fn unassigned_member_evicted_on_next_join() {
    let coordinator = ConsumerGroupCoordinator::new();
    let group = group_id("g6");

    let sub_a = Uuid::new_v4();
    coordinator.join(&group, sub_a, &[topic("topic", 4)], Duration::from_secs(30));

    coordinator
        .state
        .lock()
        .unwrap()
        .get_mut(&group)
        .unwrap()
        .members
        .get_mut(&sub_a)
        .unwrap()
        .status = MemberStatus::Unassigned;

    let sub_b = Uuid::new_v4();
    let (assigned_b, _, _) =
        coordinator.join(&group, sub_b, &[topic("topic", 4)], Duration::from_secs(30));
    assert_eq!(assigned_b.len(), 4);

    assert!(
        !coordinator
            .state
            .lock()
            .unwrap()
            .get(&group)
            .unwrap()
            .members
            .contains_key(&sub_a)
    );
}

// --- Watcher fires on stream drop, timer evicts and rebalances survivor ---

/// Drop the stream receiver (simulating TCP disconnect) — the per-member watcher
/// task detects the closure via `tx.closed().await`, marks the member `Unassigned`,
/// and arms a one-shot timer for `session_timeout`.  When the timer fires, the
/// dead member is evicted and the surviving member receives a `Terminal` frame
/// (gain rule: any gained partition triggers terminal + re-JOIN).
#[tokio::test]
async fn watcher_fires_on_disconnect_timer_evicts_and_notifies_survivor() {
    tokio::time::pause();

    let coordinator = Arc::new(ConsumerGroupCoordinator::new());
    let group = group_id("g8");
    let session_timeout = Duration::from_millis(100);

    let sub_a = Uuid::new_v4();
    let sub_b = Uuid::new_v4();
    coordinator.join(&group, sub_a, &[topic("topic", 4)], session_timeout);
    coordinator.join(&group, sub_b, &[topic("topic", 4)], session_timeout);

    let (mut generations_b, _membership_b) =
        ConsumerGroupCoordinator::subscribe(&coordinator, &group, sub_b)
            .expect("sub_b is a member");
    generations_b.borrow_and_update();

    // Dropping the handle *is* the disconnect. No watcher task and no channel:
    // a session holds this for its lifetime, so the stream ending and the
    // member being marked unassigned are the same moment.
    let (_generations_a, membership_a) =
        ConsumerGroupCoordinator::subscribe(&coordinator, &group, sub_a)
            .expect("sub_a is a member");
    drop(membership_a);

    // One yield, for the grace timer to register with the time driver. The
    // status change itself already happened synchronously inside `Drop`, which
    // is the behaviour difference from awaiting a channel close.
    tokio::task::yield_now().await;

    // sub_a must be Unassigned, timer must be running.
    {
        let state = coordinator.state.lock().unwrap();
        let grp = state.get(&group).unwrap();
        assert_eq!(
            grp.members.get(&sub_a).unwrap().status,
            MemberStatus::Unassigned
        );
        assert!(grp.timer_running, "timer must be armed after disconnect");
    }

    // Advance mock time past session_timeout — wakes the timer task.
    tokio::time::advance(session_timeout + Duration::from_millis(1)).await;
    // One more yield to let the timer task call `timer_fired`.
    tokio::task::yield_now().await;

    // sub_b gains sub_a's partitions. The coordinator publishes that; the
    // session is what turns a gain into a terminal close.
    assert!(
        generations_b
            .has_changed()
            .expect("sender outlives the receiver"),
        "the surviving member's new assignment must be published once the timer evicts"
    );
    let after_b = generations_b.borrow_and_update().clone();
    assert_eq!(
        after_b.assigned.len(),
        4,
        "the survivor must hold every partition after eviction, got {after_b:?}"
    );

    // sub_a must be evicted; sub_b must still be active.
    let state = coordinator.state.lock().unwrap();
    let grp = state
        .get(&group)
        .expect("group must still exist with sub_b");
    assert!(
        !grp.members.contains_key(&sub_a),
        "dead member must be evicted"
    );
    assert!(grp.members.contains_key(&sub_b), "survivor must remain");
}

// --- Timer no-op when consumer reconnects before grace period ---

#[test]
fn timer_noop_when_no_unassigned_remain() {
    let coordinator = ConsumerGroupCoordinator::new();
    let group = group_id("g7");

    let sub_a = Uuid::new_v4();
    let _ = coordinator.join(
        &group,
        sub_a,
        &[topic("topic", 2)],
        Duration::from_millis(50),
    );

    coordinator
        .state
        .lock()
        .unwrap()
        .get_mut(&group)
        .unwrap()
        .members
        .get_mut(&sub_a)
        .unwrap()
        .status = MemberStatus::Unassigned;

    // New consumer re-JOINs (clears the Unassigned entry)
    let sub_b = Uuid::new_v4();
    let _ = coordinator.join(
        &group,
        sub_b,
        &[topic("topic", 2)],
        Duration::from_millis(50),
    );

    // Trigger timer_fired — should find no Unassigned, do nothing
    coordinator.timer_fired(&group);

    let state = coordinator.state.lock().unwrap();
    let grp = state.get(&group).expect("group should still exist");
    assert!(grp.members.contains_key(&sub_b), "sub_b should survive");
}

/// A membership change is never lost, whether or not a stream is open.
///
/// The frame-push version used `try_send` into a 16-slot channel, so a topology
/// or terminal frame was silently discarded whenever a consumer's buffer was
/// full - which `event-broker-consumption-frames` forbids. A `watch` cannot drop
/// a value; it can only coalesce to the newest, which is what a session needs
/// since only the latest assignment is actionable.
#[test]
fn an_assignment_published_with_no_stream_open_is_not_lost() {
    let coordinator = Arc::new(ConsumerGroupCoordinator::new());
    let group = group_id("gnostream");
    let sub_a = Uuid::new_v4();
    coordinator.join(&group, sub_a, &[topic("topic", 4)], Duration::from_secs(30));

    // Subscribing *after* the join, as a real stream does - the assignment was
    // published when nobody was listening. `watch::Sender::send` fails and
    // discards the value with zero receivers, which left the member's watch on
    // its empty seed; `send_replace` is what makes this hold.
    let (generations, _membership) =
        ConsumerGroupCoordinator::subscribe(&coordinator, &group, sub_a)
            .expect("sub_a is a member");

    let seen = generations.borrow().clone();
    assert_eq!(
        seen.assigned.len(),
        4,
        "a stream opening after the join must see the assignment the join produced, got {seen:?}"
    );
    assert_eq!(seen.topology_version, 1);
}

/// Several changes in a row collapse to the newest rather than dropping any.
#[test]
fn rapid_membership_changes_coalesce_to_the_latest() {
    let coordinator = Arc::new(ConsumerGroupCoordinator::new());
    let group = group_id("gcoalesce");
    let sub_a = Uuid::new_v4();
    coordinator.join(&group, sub_a, &[topic("topic", 8)], Duration::from_secs(30));

    let (mut generations, _membership) =
        ConsumerGroupCoordinator::subscribe(&coordinator, &group, sub_a)
            .expect("sub_a is a member");
    generations.borrow_and_update();

    // Three siblings join back to back, with nothing reading in between.
    let mut siblings = Vec::new();
    for _ in 0..3 {
        let sub = Uuid::new_v4();
        coordinator.join(&group, sub, &[topic("topic", 8)], Duration::from_secs(30));
        siblings.push(sub);
    }

    assert!(
        generations
            .has_changed()
            .expect("sender outlives the receiver"),
        "a change must be observable"
    );
    let latest = generations.borrow_and_update().clone();
    assert_eq!(
        latest.assigned.len(),
        2,
        "four members over eight partitions is two each - the newest split, not an \
         intermediate one, got {latest:?}"
    );

    // Deliberately no assertion on `topology_version`. A member whose partition
    // set is unchanged by a rebalance is skipped, so its watch keeps the version
    // at which its assignment last actually moved. With eight partitions the
    // range split gives member 0 the range [0, 2) at both three and four
    // members, so whether this member's version reaches the newest one depends
    // on where it lands in the member ordering. What coalescing has to
    // guarantee is that no change is dropped - that the value read is the
    // newest split rather than an intermediate one - and the partition count
    // above is what states that.
}
