use chrono::Utc;

use crate::api::{IngestOutcome, ProducerMode};
use crate::error::EventBrokerError;
use crate::ids::ProducerId;
use crate::models::Event;
use crate::sequence::Sequence;

use super::core::Core;
use super::partitioning::partition_for;

/// Infer the producer mode from the `meta` chain fields on the event.
fn detect_mode(event: &Event) -> ProducerMode {
    match &event.meta {
        Some(m) if m.producer_id.is_some() && m.previous.is_some() && m.sequence.is_some() => {
            ProducerMode::Chained
        }
        Some(m) if m.producer_id.is_some() && m.sequence.is_some() => ProducerMode::Monotonic,
        _ => ProducerMode::Stateless,
    }
}

/// One governing trait of a derived event-type schema document.
fn event_type_trait<'a>(
    schema: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Value> {
    schema.get("x-gts-traits")?.get(name)
}

/// The registered event type an event is an instance of, plus the stream it
/// publishes to.
///
/// A published event carries no topic: the derived event-type schema owns that
/// binding in its `topic` trait, so ingest resolves the stream from the event's
/// type.
fn resolve_event_type(
    core: &Core,
    type_id: &str,
) -> Result<(String, serde_json::Value), EventBrokerError> {
    let schema = core
        .topics
        .values()
        .find_map(|state| state.event_types.get(type_id))
        .map(|reg| reg.schema.clone())
        .ok_or_else(|| EventBrokerError::EventTypeUnknown {
            type_id: type_id.to_owned(),
            detail: format!("event type '{type_id}' not registered in mock"),
            instance: String::new(),
        })?;
    let topic = event_type_trait(&schema, "topic")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            EventBrokerError::Internal(format!("event type '{type_id}' declares no topic trait"))
        })?
        .to_owned();
    Ok((topic, schema))
}

/// The partition input for `event`, resolved from its event type's partition-key
/// pointer. The base defaults the trait, so every registered type declares one.
fn resolve_partition_input(
    schema: &serde_json::Value,
    event: &Event,
) -> Result<String, EventBrokerError> {
    let pointer = event_type_trait(schema, "partition_key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            EventBrokerError::Internal(format!(
                "event type '{}' declares no partition_key trait",
                event.type_id
            ))
        })?;
    event.partition_input(pointer)
}

/// Core ingest pipeline. Called under the `Core` mutex (held by caller).
///
/// Steps: event-type lookup (which resolves the topic) → topic lookup → schema
/// validation → partition derivation (ADR-0002: the event type's partition-key pointer,
/// `tenant_id`) → mode detection → chain/monotonic dedup → offset assignment →
/// append to log.
///
/// Returns `(IngestOutcome, stamped_event)` on success where the stamped event
/// has `partition`/`sequence`/`offset` populated.
pub(super) fn ingest_one(
    core: &mut Core,
    event: &Event,
) -> Result<(IngestOutcome, Event), EventBrokerError> {
    // -- 0. GTS format validation ----------------------------------------------
    if let Err(e) = gts_id::GtsId::try_new(&event.type_id) {
        return Err(EventBrokerError::InvalidEventField {
            field: "type",
            detail: format!("event type must be a GTS identifier: {e}"),
            instance: String::new(),
        });
    }
    if event.partition.is_some() {
        return Err(EventBrokerError::InvalidEventField {
            field: "partition",
            detail: "partition is broker-stamped and read-only on publish".to_owned(),
            instance: "/v1/events".to_owned(),
        });
    }

    // -- 1. Event-type lookup (resolves the topic) -----------------------------
    let (topic, type_schema) = resolve_event_type(core, &event.type_id)?;

    // -- 2. Topic lookup -------------------------------------------------------
    if !core.topics.contains_key(&topic) {
        return Err(EventBrokerError::TopicNotFound {
            topic: topic.clone(),
            detail: format!("topic '{topic}' not registered in mock"),
            instance: String::new(),
        });
    }

    // -- 3. Governing traits + payload contract (M4) ---------------------------
    let allowed_subject_types: Vec<&str> = event_type_trait(&type_schema, "allowed_subject_types")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect()
        })
        .unwrap_or_default();
    if !allowed_subject_types.is_empty()
        && !allowed_subject_types
            .iter()
            .any(|allowed| *allowed == event.subject_type)
    {
        return Err(EventBrokerError::InvalidEventField {
            field: "subject_type",
            detail: format!(
                "subject_type '{}' is not allowed for event type '{}'",
                event.subject_type, event.type_id
            ),
            instance: "/v1/events".to_owned(),
        });
    }

    if let Some(data) = &event.data {
        // The event type narrows the base event's `data` member; that narrowing,
        // not the whole event schema, is what a publish payload must satisfy.
        let payload_schema = crate::gts::data_contract(&type_schema);
        match jsonschema::validator_for(&payload_schema) {
            Ok(validator) => {
                let errs: Vec<String> =
                    validator.iter_errors(data).map(|e| e.to_string()).collect();
                if !errs.is_empty() {
                    let detail = errs.join("; ");
                    return Err(EventBrokerError::EventDataInvalid {
                        type_id: event.type_id.clone(),
                        errors: vec![detail.clone()],
                        detail,
                        instance: String::new(),
                    });
                }
            }
            Err(e) => {
                return Err(EventBrokerError::EventDataInvalid {
                    type_id: event.type_id.clone(),
                    errors: vec![e.to_string()],
                    detail: format!("schema compile error: {e}"),
                    instance: String::new(),
                });
            }
        }
    }

    let partitions = core.topics[&topic].partitions;

    // -- 4. Partition derivation (ADR-0002: the event type's partition-key pointer)
    let partition = partition_for(&resolve_partition_input(&type_schema, event)?, partitions);

    // -- 5. Producer mode detection --------------------------------------------
    let mode = detect_mode(event);

    // -- 6. Chain / monotonic dedup --------------------------------------------
    if mode != ProducerMode::Stateless {
        let meta = event.meta.as_ref().expect("meta present for non-stateless");
        let producer_id = ProducerId(meta.producer_id.expect("producer_id present"));
        // B1: a chained/monotonic publish must carry a registered Producer-Id
        // (issued by POST /v1/producers). An unknown/expired id is rejected.
        let producer_reg = core.producers.get(&producer_id).ok_or_else(|| {
            EventBrokerError::UnknownProducer {
                producer_id,
                detail: format!(
                    "unknown producer_id {producer_id:?}; register via POST /v1/producers before publishing"
                ),
                instance: "/v1/events".to_owned(),
            }
        })?;
        if producer_reg.mode != mode {
            return Err(EventBrokerError::InvalidEventField {
                field: "meta",
                detail: format!(
                    "producer_id {producer_id:?} is registered as {:?}, but event metadata is {:?}",
                    producer_reg.mode, mode
                ),
                instance: "/v1/events".to_owned(),
            });
        }
        let seq = meta.sequence.expect("sequence present");
        let key = (producer_id, topic.clone(), partition);
        let last = core.producer_state.get(&key).copied().unwrap_or(-1);

        match mode {
            ProducerMode::Chained => {
                let prev = meta.previous.expect("previous present for chained");
                if seq <= last {
                    // Duplicate - do NOT advance state (M2).
                    return Ok((IngestOutcome::Duplicate, event.clone()));
                }
                if prev != last {
                    return Err(EventBrokerError::SequenceViolation {
                        expected_previous: last,
                        detail: format!(
                            "expected previous={last}, got previous={prev} for ({producer_id:?}, {topic}, {partition})"
                        ),
                        instance: String::new(),
                    });
                }
                core.producer_state.insert(key, seq);
            }
            ProducerMode::Monotonic => {
                if seq <= last {
                    return Ok((IngestOutcome::Duplicate, event.clone()));
                }
                core.producer_state.insert(key, seq);
            }
            ProducerMode::Stateless => unreachable!(),
        }
    }

    // -- 7. Offset assignment + append (serialised under Mutex → prevents M1) --
    let now = Utc::now();
    let topic_state = core.topics.get_mut(&topic).expect("checked above");
    let offset = topic_state.next_offset_for(partition);
    let mut stamped = event.clone();
    stamped.partition = Some(partition);
    stamped.sequence = Some(Sequence::assigned(offset));
    stamped.sequence_time = Some(now);
    // Strip writeOnly publish-input fields from the stored read-projection.
    stamped.meta = None;
    topic_state.append(partition, stamped.clone());

    Ok((IngestOutcome::Accepted, stamped))
}

/// Batch ingest: all events must resolve to the same `(topic, partition)`.
/// Called under the `Core` mutex.
pub(super) fn ingest_batch(
    core: &mut Core,
    events: &[Event],
) -> Result<Vec<(IngestOutcome, Event)>, EventBrokerError> {
    if events.is_empty() {
        return Ok(vec![]);
    }

    // Validate batch homogeneity (same topic + same partition key → same partition).
    let first = &events[0];
    let (first_topic, first_schema) = resolve_event_type(core, &first.type_id)?;
    let partitions = core
        .topics
        .get(&first_topic)
        .map(|state| state.partitions)
        .unwrap_or(1);
    let expected_partition =
        partition_for(&resolve_partition_input(&first_schema, first)?, partitions);

    for event in events.iter().skip(1) {
        let (topic, schema) = resolve_event_type(core, &event.type_id)?;
        if topic != first_topic {
            return Err(EventBrokerError::InvalidEventField {
                field: "topic",
                detail: format!(
                    "batch.mixed_partition: all events must share the same topic; got '{first_topic}' and '{topic}'"
                ),
                instance: String::new(),
            });
        }
        let this_partition = partition_for(&resolve_partition_input(&schema, event)?, partitions);
        if this_partition != expected_partition {
            return Err(EventBrokerError::InvalidEventField {
                field: "type",
                detail: format!(
                    "batch.mixed_partition: events resolve to different partitions ({expected_partition} vs {this_partition})"
                ),
                instance: String::new(),
            });
        }
    }

    let mut staged = Core {
        topics: core.topics.clone(),
        producers: core.producers.clone(),
        producer_state: core.producer_state.clone(),
        ..Core::default()
    };
    let results = events
        .iter()
        .map(|event| ingest_one(&mut staged, event))
        .collect::<Result<Vec<_>, _>>()?;
    core.topics = staged.topics;
    core.producer_state = staged.producer_state;
    Ok(results)
}
