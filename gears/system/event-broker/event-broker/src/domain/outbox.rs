//! Ingest-side outbox constants (design.md D5). The payload itself needs no
//! dedicated envelope type: `IngestServiceImpl::publish_event` enqueues the
//! `domain::model::Event` it already has (partition already stamped) as
//! JSON directly - `infra::workers::ingest_outbox::IngestOutboxHandler`
//! decodes the same type back and reuses `domain::backend::to_sdk_event`
//! unchanged, so nothing here duplicates that conversion.

/// `toolkit_db::outbox` queue name for the ingest publish path.
pub const INGEST_QUEUE_NAME: &str = "evbk-ingest";

/// `payload_type` every ingest outbox row is stamped with.
pub const INGEST_PAYLOAD_TYPE: &str = "application/vnd.event-broker.ingest-event+json;version=1";

/// Partition count `Outbox::builder(..).queue(INGEST_QUEUE_NAME, ..)` is
/// registered with - `toolkit_db::outbox::Partitions::of` requires a power
/// of 2 in `1..=64`; `4` is a reasonable default for a single-process
/// deployment (eb-single-process-implementation D7), not yet
/// operator-configurable.
pub const INGEST_OUTBOX_PARTITIONS: u16 = 4;

/// Maps the topic partition an event was stamped with onto the ingest outbox
/// partition that carries it.
///
/// A topic may have any number of partitions while the outbox has exactly
/// [`INGEST_OUTBOX_PARTITIONS`], so the two ranges have to be reconciled
/// somewhere. Modulo is the reconciliation that keeps the ordering guarantee:
/// the outbox sequences within a partition, so every event of one
/// `(topic, partition)` has to keep landing on the same outbox partition or
/// their relative order is lost on the way to the backend. A constant, a
/// round-robin or a hash of the event id would each break something - a
/// constant idles every sequencer/processor slot but one, and the other two
/// scatter one topic partition's events across slots that sequence
/// independently.
#[must_use]
pub fn outbox_partition_for(topic_partition: i32) -> u32 {
    // `unsigned_abs` rather than a cast: partition numbers are non-negative by
    // construction, and a negative one would otherwise wrap to a value modulo
    // cannot bring back into range.
    topic_partition.unsigned_abs() % u32::from(INGEST_OUTBOX_PARTITIONS)
}
