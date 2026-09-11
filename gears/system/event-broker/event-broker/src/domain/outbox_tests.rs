//! Mapping a topic partition onto an ingest outbox partition.
//!
//! Two properties matter and neither is visible from the function body alone:
//! the mapping must be total over every topic partition a topic can have (a
//! topic may have more partitions than the outbox does), and it must be stable
//! per topic partition, because the outbox sequences within a partition and
//! that sequence is the only thing preserving the order of one topic
//! partition's events on their way to the backend.

use super::outbox::{INGEST_OUTBOX_PARTITIONS, outbox_partition_for};

#[test]
fn every_outbox_partition_is_reachable() {
    let reached: std::collections::BTreeSet<u32> = (0..64).map(outbox_partition_for).collect();
    let expected: std::collections::BTreeSet<u32> =
        (0..u32::from(INGEST_OUTBOX_PARTITIONS)).collect();
    assert_eq!(
        reached, expected,
        "the topic partitions of a 64-partition topic must reach every outbox \
         partition; a mapping that reaches only some leaves the rest of the \
         sequencer/processor slots permanently idle"
    );
}

#[test]
fn a_topic_partition_beyond_the_outbox_count_stays_in_range() {
    // The count is 4, so a topic with 8 partitions names 4..=7 - the values
    // that used to address outbox partitions that do not exist.
    assert_eq!(outbox_partition_for(4), 0);
    assert_eq!(outbox_partition_for(5), 1);
    assert_eq!(outbox_partition_for(6), 2);
    assert_eq!(outbox_partition_for(7), 3);
    assert_eq!(outbox_partition_for(i32::MAX), 3);
}

#[test]
fn the_mapping_is_stable_per_topic_partition() {
    for partition in 0..64 {
        assert_eq!(
            outbox_partition_for(partition),
            outbox_partition_for(partition),
            "the same topic partition must always map to the same outbox \
             partition, or its events lose their relative order"
        );
    }
}
