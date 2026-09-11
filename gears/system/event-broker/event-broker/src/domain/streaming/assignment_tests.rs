//! Pure: two generations in, one classification out. No clock, no channel.

use crate::domain::model::Sequence;
use toolkit_gts::GtsInstanceId;

use crate::domain::model::Assignment;

use super::assignment::{AssignmentDelta, Generation};

fn topic() -> GtsInstanceId {
    GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
        .expect("static topic id is valid")
}

/// `offset` and `last_examined` are set to values that differ from the defaults
/// on purpose, so a test that passes cannot be passing because they happened to
/// match.
fn held(partition: i32) -> Assignment {
    Assignment {
        topic: topic(),
        partition,
        offset: Sequence::assigned(500),
        last_examined: Sequence::assigned(700),
    }
}

fn generation(version: i64, partitions: &[i32]) -> Generation {
    Generation::new(version, partitions.iter().copied().map(held).collect())
}

#[test]
fn the_same_partitions_at_the_same_version_are_unchanged() {
    let before = generation(7, &[0, 1]);
    let after = generation(7, &[0, 1]);

    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::Unchanged
    );
}

#[test]
fn the_same_partitions_at_a_new_version_report_the_version_alone() {
    let before = generation(7, &[0, 1]);
    let after = generation(8, &[0, 1]);

    // A rebalance that did not move this member. The consumer still needs the
    // version, because it is what makes a later position report attributable to
    // a topology.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::VersionOnly {
            topology_version: 8
        }
    );
}

#[test]
fn a_differing_offset_is_not_a_topology_change() {
    let before = generation(7, &[0, 1]);
    let mut after = generation(7, &[0, 1]);
    if let Some(first) = after.assigned.first_mut() {
        first.offset = Sequence::assigned(9999);
        first.last_examined = Sequence::assigned(9999);
    }

    // The comparison is on `(topic, partition)`. Comparing whole structs would
    // report a rebalance every time a stale offset differed, and emit a frame
    // for a topology that had not moved.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::Unchanged
    );
}

#[test]
fn losing_some_partitions_retains_the_rest() {
    let before = generation(7, &[0, 1, 2, 3]);
    let after = generation(8, &[0, 1]);

    match AssignmentDelta::classify(&before, &after) {
        AssignmentDelta::Loss {
            topology_version,
            retained,
        } => {
            assert_eq!(topology_version, 8);
            assert_eq!(
                retained
                    .iter()
                    .map(|held| held.partition)
                    .collect::<Vec<_>>(),
                vec![0, 1]
            );
        }
        other => panic!("expected Loss, got {other:?}"),
    }
}

#[test]
fn losing_every_partition_is_distinct_from_losing_some() {
    let before = generation(7, &[0, 1]);
    let after = generation(8, &[]);

    // Not `Loss` with an empty retained set: there is nothing left to stream, so
    // the emission rule differs - terminal rather than a topology frame.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::LoseAll
    );
}

#[test]
fn gaining_a_partition_is_terminal() {
    let before = generation(7, &[0, 1]);
    let after = generation(8, &[0, 1, 2]);

    // A gained partition has no cursor in this session, and its correct starting
    // offset is whatever the group committed. Continuing would mean replaying
    // from zero or guessing.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::Gain
    );
}

#[test]
fn a_gain_dominates_a_simultaneous_loss() {
    let before = generation(7, &[0, 1]);
    let after = generation(8, &[1, 2]);

    // Partition 0 went and 2 arrived. The loss alone would continue the stream;
    // the gain cannot, so the gain decides.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::Gain
    );
}

#[test]
fn holding_nothing_before_and_after_is_only_a_version_move() {
    let before = generation(7, &[]);
    let after = generation(8, &[]);

    // Neither a loss nor a gain: nothing was held either side, so there is no
    // `LoseAll` to report even though the assignment is empty.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::VersionOnly {
            topology_version: 8
        }
    );
}

#[test]
fn only_a_gain_or_a_total_loss_ends_the_stream() {
    assert!(AssignmentDelta::Gain.is_terminal());
    assert!(AssignmentDelta::LoseAll.is_terminal());

    assert!(!AssignmentDelta::Unchanged.is_terminal());
    assert!(
        !AssignmentDelta::VersionOnly {
            topology_version: 1
        }
        .is_terminal()
    );
    assert!(
        !AssignmentDelta::Loss {
            topology_version: 1,
            retained: vec![held(0)],
        }
        .is_terminal()
    );
}

#[test]
fn partition_identity_is_scoped_to_its_topic() {
    let other_topic = GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.other.acme.v1")
        .expect("static topic id is valid");
    let before = generation(7, &[0]);
    let after = Generation::new(
        8,
        vec![Assignment {
            topic: other_topic,
            partition: 0,
            offset: Sequence::assigned(500),
            last_examined: Sequence::assigned(700),
        }],
    );

    // Same partition number, different topic. Treating the number alone as the
    // identity would call this `VersionOnly` and keep streaming a topic the
    // member no longer holds.
    assert_eq!(
        AssignmentDelta::classify(&before, &after),
        AssignmentDelta::Gain
    );
}
