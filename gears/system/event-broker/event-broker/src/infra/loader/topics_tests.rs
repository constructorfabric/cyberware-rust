//! Lazy creation and safe retirement.

use crate::domain::model::Sequence;
use toolkit_gts::GtsInstanceId;

use crate::domain::streaming::source::PartitionKey;

use super::topics::{TopicManager, TopicPolicy};

fn key(partition: i32) -> PartitionKey {
    PartitionKey::new(
        GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
            .expect("static topic id is valid"),
        partition,
    )
}

fn manager() -> TopicManager {
    TopicManager::new(TopicPolicy::default())
}

#[test]
fn nothing_exists_until_something_asks() {
    let topics = manager();

    assert_eq!(topics.count(), 0);
    assert!(topics.get(&key(0)).is_none());
}

#[test]
fn attaching_twice_yields_the_same_partition() {
    let topics = manager();

    let first = topics.attach(&key(0));
    let second = topics.attach(&key(0));

    assert_eq!(topics.count(), 1);
    assert!(
        std::ptr::eq(std::ptr::from_ref(&*first), std::ptr::from_ref(&*second)),
        "two caches for one partition would accumulate different spans while \
         readers on each believed they held the partition's state"
    );
}

#[test]
fn distinct_partitions_get_distinct_caches() {
    let topics = manager();

    let _first = topics.attach(&key(0));
    let _second = topics.attach(&key(1));

    assert_eq!(topics.count(), 2);
}

#[test]
fn an_idle_unheld_partition_is_retired() {
    let topics = manager();
    drop(topics.attach(&key(0)));

    let retired = topics.retire_idle(100, 10);

    assert_eq!(retired, 1);
    assert_eq!(topics.count(), 0);
}

#[test]
fn a_partition_someone_still_holds_is_never_retired() {
    let topics = manager();
    let held = topics.attach(&key(0));

    let retired = topics.retire_idle(100, 10);

    assert_eq!(
        retired, 0,
        "a later attach would build a second cache for it"
    );
    assert_eq!(topics.count(), 1);
    drop(held);
}

#[test]
fn a_partition_with_a_reader_is_never_retired() {
    let topics = manager();
    let partition = topics.attach(&key(0));
    let _reader = partition.cache().track_reader(Sequence::assigned(0));
    drop(partition);

    assert_eq!(topics.retire_idle(100, 10), 0);
    assert_eq!(topics.count(), 1);
}

#[test]
fn a_recently_active_partition_is_not_retired() {
    let topics = manager();
    let partition = topics.attach(&key(0));
    partition.touch(95);
    drop(partition);

    assert_eq!(topics.retire_idle(100, 10), 0, "active five rounds ago");
    assert_eq!(
        topics.retire_idle(110, 10),
        1,
        "and idle fifteen rounds later"
    );
}

#[test]
fn a_claim_is_exclusive_until_released() {
    let topics = manager();
    let partition = topics.attach(&key(0));

    assert!(partition.claim());
    assert!(
        !partition.claim(),
        "two workers fetching the same partition is the uncoalesced behaviour \
         the design exists to avoid, reintroduced by the scheduler"
    );

    partition.release();
    assert!(partition.claim());
}
