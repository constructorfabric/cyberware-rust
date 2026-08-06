//! What one stored event counts against its partition's retention byte bound.

use event_broker_sdk::models::Event;

/// Bytes the fixed-width columns of one row occupy: the uuid primary key, the
/// tenant uuid, the partition and sequence integers, and the two timestamps.
///
/// An approximation of a row's constant overhead rather than a measurement of
/// it - `PRAGMA page_count` is the only honest measurement and it is not
/// per-row. It exists so a partition of tiny events is not accounted as
/// occupying almost nothing, which would let an unbounded number of them
/// accumulate under any byte bound.
const FIXED_ROW_BYTES: i64 = 64;

/// What this row costs the backend that stores it.
///
/// The bound exists to keep a database from growing until the process dies, so
/// the figure tracks what actually grows: the bytes this backend writes for the
/// row, not the bytes the event occupied on the wire. The two differ - the wire
/// form carries producer metadata this projection drops - and an operator
/// setting a byte bound is sizing storage.
///
/// Stored on the row rather than recomputed at removal time, so a removal
/// subtracts exactly what its insert added even if this rule later changes.
///
/// The topic is not charged here even though the row stores it: an event no
/// longer carries its topic, which is resolved from its type and reaches this
/// backend as a `persist` argument instead. Nor is the partition key, which is
/// now a trait on the event type rather than a per-event field. Both are part
/// of the row overhead this figure deliberately leaves unaccounted.
pub fn stored_bytes(event: &Event) -> i64 {
    let text = |value: &str| i64::try_from(value.len()).unwrap_or(i64::MAX);
    let optional_text = |value: &Option<String>| value.as_deref().map_or(0, text);

    FIXED_ROW_BYTES
        .saturating_add(text(&event.type_id))
        .saturating_add(text(&event.source))
        .saturating_add(text(&event.subject))
        .saturating_add(text(&event.subject_type))
        .saturating_add(optional_text(&event.trace_parent))
        .saturating_add(
            event
                .data
                .as_ref()
                .and_then(|data| serde_json::to_vec(data).ok())
                .map_or(0, |bytes| i64::try_from(bytes.len()).unwrap_or(i64::MAX)),
        )
}

#[cfg(test)]
#[path = "sizing_tests.rs"]
mod sizing_tests;
