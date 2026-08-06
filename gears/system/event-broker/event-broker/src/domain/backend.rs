//! `BackendResolver` (eb-single-process-implementation D3): resolves the
//! event-storage backend for a topic. `EventRepo` no longer exists -
//! `IngestServiceImpl`/`DeliveryServiceImpl` call the resolved
//! `event_broker_sdk::EventBrokerBackend` directly for event
//! append/query/segments, with no repo-level indirection.
//!
//! Also home to the `domain::model::Event`/`Topic` <-> SDK `models::Event`
//! conversion functions every backend call site needs - this depends only
//! on `event_broker_sdk` (already a normal crate dependency, same as
//! `domain::authz`'s use of its GTS constants) and this crate's own
//! `domain::model`, so it stays within the "no infra dependencies in
//! domain" invariant despite talking about the backend boundary.

use std::sync::Arc;

use event_broker_sdk::EventBrokerBackend;
use event_broker_sdk::models::{Event as SdkEvent, ProducerMeta as SdkProducerMeta};
use toolkit_gts::GtsInstanceId;

use crate::domain::error::DomainError;
use crate::domain::ingest::PublishRequest;
use crate::domain::model::{Event, Meta, Topic};

/// Resolves the backend for a topic. Takes `&Topic` (not nothing) so that
/// when a second backend type eventually exists, call sites - which already
/// look up the topic via `SpecificationManager` before doing anything else -
/// don't need to change; only this trait's implementation does. For this
/// change there is exactly one registered backend, so resolution is
/// trivial - deliberately not implementing DESIGN.md's full GTS
/// backend-type + `ClusterCapabilities.register_shard()` backend-instance
/// registration scheme (design.md D3's "Alternatives considered").
pub trait BackendResolver: Send + Sync {
    fn resolve(&self, topic: &Topic) -> Arc<dyn EventBrokerBackend>;
}

/// The trivial resolver for this change: always the one backend it was
/// constructed with, regardless of `topic`.
pub struct SingleBackendResolver {
    backend: Arc<dyn EventBrokerBackend>,
}

impl SingleBackendResolver {
    #[must_use]
    pub fn new(backend: Arc<dyn EventBrokerBackend>) -> Self {
        Self { backend }
    }
}

impl BackendResolver for SingleBackendResolver {
    fn resolve(&self, _topic: &Topic) -> Arc<dyn EventBrokerBackend> {
        Arc::clone(&self.backend)
    }
}

/// The value an event type's partition-key pointer resolves to within a publish
/// request, as the string whose bytes are hashed.
///
/// Delegates to the SDK's own [`SdkEvent::partition_input`], which owns the
/// mapping from schema member names to fields: a pointer addresses `/tenant_id`
/// or `/data/order_id`, not a Rust identifier, and a second copy of that
/// mapping here is exactly the kind of thing that drifts. A producer computing
/// a local hint resolves the pointer through the same function.
///
/// # Errors
/// [`DomainError::Validation`] when the pointer resolves to nothing, to null,
/// or to an object or array. The registration check proves the member is
/// *declared*, not that every event carries it, so an event omitting an
/// optional member is refused here rather than silently routed somewhere else.
pub fn partition_input(request: &PublishRequest, pointer: &str) -> Result<String, DomainError> {
    let addressable = SdkEvent {
        id: request.id,
        type_id: request.r#type.as_ref().to_owned(),
        tenant_id: request.tenant_id,
        source: request.source.clone(),
        subject: request.subject.clone(),
        subject_type: request.subject_type.clone(),
        occurred_at: request.occurred_at,
        trace_parent: request.trace_parent.clone(),
        data: Some(request.data.clone()),
        partition: None,
        sequence: None,
        sequence_time: None,
        meta: None,
    };
    addressable
        .partition_input(pointer)
        .map_err(|err| DomainError::Validation {
            code: "PartitionKeyUnresolved",
            message: format!(
                "event type '{}' partitions on `{pointer}`, which this event does not resolve: \
                 {err}",
                request.r#type.as_ref()
            ),
        })
}

/// `domain::model::Event` (publish input) -> the SDK's `Event` shape a
/// backend's `persist` call expects. `partition` is resolved by the caller
/// (`IngestServiceImpl::publish_event`'s existing `partition_for(...)`
/// logic) before this conversion, not derived here.
///
/// The owning topic is not part of the SDK event: `persist` takes it as its
/// own argument, so the caller names the log it is appending to rather than
/// repeating it once per event in the batch.
#[must_use]
pub fn to_sdk_event(partition: i32, event: &Event) -> SdkEvent {
    SdkEvent {
        id: event.id,
        type_id: event.r#type.to_string(),
        tenant_id: event.tenant_id,
        source: event.source.clone(),
        subject: event.subject.clone(),
        subject_type: event.subject_type.clone(),
        occurred_at: event.occurred_at,
        trace_parent: event.trace_parent.clone(),
        data: Some(event.data.clone()),
        // Broker-stamped fields: unset on publish input, matching the SDK's
        // own "readOnly on the wire; absent on publish" framing for these.
        partition: u32::try_from(partition).ok(),
        sequence: None,
        sequence_time: None,
        meta: event.meta.as_ref().map(to_sdk_producer_meta),
    }
}

fn to_sdk_producer_meta(meta: &Meta) -> SdkProducerMeta {
    SdkProducerMeta {
        // `Meta::version` (domain, `i32`) is always a small protocol version
        // number (`1` today) - `unwrap_or(1)` rather than propagating a
        // conversion error keeps this a pure, infallible mapping; an
        // out-of-range version would already have failed earlier
        // (schema/type validation), not here.
        version: u8::try_from(meta.version).unwrap_or(1),
        producer_id: Some(meta.producer_id),
        previous: Some(meta.previous),
        sequence: Some(meta.sequence),
        partition_hint: None,
    }
}

/// The SDK's `Event` (backend read projection) -> `domain::model::Event`.
/// `meta` is always `None` on the way back - the read projection strips
/// publish-input-only fields, matching `domain::model::Event::meta`'s own
/// doc comment ("Publish-input only; stripped on the read projection").
///
/// `topic` comes from the caller, which already knows the log it read: the
/// SDK event carries no topic, and re-deriving one per event from its type
/// would ask the specification manager the same question the read itself
/// already answered.
///
/// # Errors
/// Returns `DomainError::Internal` if `type_id` isn't a well-formed GTS
/// instance id - would indicate backend-stored data corruption, since
/// nothing publishes an event without validating it first.
pub fn from_sdk_event(topic: &GtsInstanceId, event: SdkEvent) -> Result<Event, DomainError> {
    Ok(Event {
        id: event.id,
        r#type: parse_gts_type_id(&event.type_id)?,
        topic: topic.clone(),
        tenant_id: event.tenant_id,
        source: event.source,
        subject: event.subject,
        subject_type: event.subject_type,
        occurred_at: event.occurred_at,
        trace_parent: event.trace_parent,
        data: event.data.unwrap_or(serde_json::Value::Null),
        meta: None,
        partition: event.partition.and_then(|p| i32::try_from(p).ok()),
        sequence: event.sequence,
        sequence_time: event.sequence_time,
    })
}

/// The same, for the event's own type: a backend stores the identifier as a
/// string, and an event type is a GTS type rather than an instance of one.
fn parse_gts_type_id(raw: &str) -> Result<gts::GtsTypeId, DomainError> {
    gts::GtsTypeId::try_new(raw).map_err(|e| {
        DomainError::Internal(format!(
            "backend returned a malformed GTS type id '{raw}': {e}"
        ))
    })
}
