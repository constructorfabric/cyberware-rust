use std::sync::Arc;

use toolkit_canonical_errors::CanonicalError;
use toolkit_canonical_errors::context::FieldViolation;
use toolkit_canonical_errors::resource_error;

use crate::ids::{ConsumerGroupId, ProducerId, SubscriptionId};
use crate::sequence::Sequence;

#[resource_error(gts_id!("cf.core.events.producer_options.v1~"))]
struct ProducerOptionsResourceError;
#[resource_error(gts_id!("cf.core.events.consumer_options.v1~"))]
struct ConsumerOptionsResourceError;
#[resource_error(gts_id!("cf.core.events.topic.v1~"))]
struct TopicResourceError;
#[resource_error(gts_id!("cf.core.events.consumer_group.v1~"))]
struct ConsumerGroupResourceError;
#[resource_error(gts_id!("cf.core.events.subscription.v1~"))]
struct SubscriptionResourceError;
#[resource_error(gts_id!("cf.core.events.producer.v1~"))]
struct ProducerResourceError;
#[resource_error(gts_id!("cf.core.events.partition.v1~"))]
struct PartitionResourceError;
#[resource_error(gts_id!("cf.core.events.event.v1~"))]
struct EventResourceError;
#[resource_error(gts_id!("cf.core.events.stream.v1~"))]
struct StreamResourceError;
#[resource_error(gts_id!("cf.core.events.storage.v1~"))]
struct StorageResourceError;
#[resource_error(gts_id!("cf.core.events.offset.v1~"))]
struct OffsetResourceError;
#[resource_error(gts_id!("cf.core.events.request.v1~"))]
struct RequestResourceError;

pub mod resources {
    use toolkit_gts::gts_id;

    pub const PRODUCER_OPTIONS: &str = gts_id!("cf.core.events.producer_options.v1~");
    pub const CONSUMER_OPTIONS: &str = gts_id!("cf.core.events.consumer_options.v1~");
    pub const TOPIC: &str = gts_id!("cf.core.events.topic.v1~");
    pub const CONSUMER_GROUP: &str = gts_id!("cf.core.events.consumer_group.v1~");
    pub const SUBSCRIPTION: &str = gts_id!("cf.core.events.subscription.v1~");
    pub const PRODUCER: &str = gts_id!("cf.core.events.producer.v1~");
    pub const PARTITION: &str = gts_id!("cf.core.events.partition.v1~");
    pub const EVENT: &str = gts_id!("cf.core.events.event.v1~");
    pub const STREAM: &str = gts_id!("cf.core.events.stream.v1~");
    pub const STORAGE: &str = gts_id!("cf.core.events.storage.v1~");
    pub const OFFSET: &str = gts_id!("cf.core.events.offset.v1~");
    pub const TRANSPORT: &str = gts_id!("cf.core.events.transport.v1~");
    pub const REQUEST: &str = gts_id!("cf.core.events.request.v1~");
}

pub mod reasons {
    pub const INVALID_PRODUCER_OPTIONS: &str = "invalid_producer_options";
    pub const INVALID_CONSUMER_OPTIONS: &str = "invalid_consumer_options";
    pub const EVENT_TYPE_NOT_DECLARED: &str = "event_type_not_declared";
    pub const INVALID_EVENT_FIELD: &str = "invalid_event_field";
    /// A text value carried a byte outside printable ASCII.
    pub const ASCII_ONLY: &str = "ascii_only";
    /// A text value fell outside its field's byte bounds.
    pub const FIELD_TOO_LONG: &str = "field_too_long";
    pub const EVENT_DATA_INVALID: &str = "event_data_invalid";
    pub const BATCH_TOO_LARGE: &str = "batch_too_large";
    pub const INVALID_BACKEND_CONFIG: &str = "invalid_backend_config";
    pub const TYPE_NOT_IN_DECLARED_TOPIC: &str = "type_not_in_declared_topic";
    pub const SCHEMA_NOT_PREPARED: &str = "schema_not_prepared";
    pub const CONSUMER_GROUP_HAS_ACTIVE_MEMBERS: &str = "consumer_group_has_active_members";
    pub const SEQUENCE_MISMATCH: &str = "sequence_mismatch";
    pub const POSITIONS_NOT_SET: &str = "positions_not_set";
    pub const PARTITION_NOT_ASSIGNED: &str = "partition_not_assigned";
    pub const STREAMING_IN_PROGRESS: &str = "streaming_in_progress";
    pub const IN_TX_OFFSETS_NOT_SUPPORTED: &str = "in_tx_offsets_not_supported";
    pub const RATE_LIMIT: &str = "event-broker.rate-limit";
    pub const CONSUMER_GROUP_CAPACITY: &str = "event-broker.consumer-group-capacity";
    pub const UNAUTHORIZED: &str = "event_broker_unauthorized";
    pub const STORAGE_UNAVAILABLE: &str = "storage_unavailable";
    pub const OFFSET_MANAGER_UNAVAILABLE: &str = "offset_manager_unavailable";
    /// A seek position sat below the lowest position the partition admits.
    pub const BELOW_RETENTION_FLOOR: &str = "below_retention_floor";
    /// A seek position sat above the partition's high-water mark.
    pub const ABOVE_HIGH_WATER_MARK: &str = "above_high_water_mark";
    pub const TRANSPORT_UNAVAILABLE: &str = "transport_unavailable";
}

/// Which end of a partition's admissible range a requested position fell
/// outside of.
///
/// A position is admissible when it sits in `[floor, ceiling]` - the position
/// below the oldest surviving event, up to the highest sequence assigned. The
/// two ends fail for different reasons and a caller corrects them differently,
/// so a rejection says which one it was rather than only that the range was
/// missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfRange {
    /// Below the floor: retention has removed everything down there, so no
    /// event remains for delivery to resume from.
    BelowFloor,
    /// Above the ceiling: nothing has been assigned that high yet.
    AboveCeiling,
}

impl OutOfRange {
    /// The machine reason a problem body carries for this end.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::BelowFloor => reasons::BELOW_RETENTION_FLOOR,
            Self::AboveCeiling => reasons::ABOVE_HIGH_WATER_MARK,
        }
    }

    /// How the end reads in a sentence describing the range that was missed.
    #[must_use]
    pub const fn phrase(self) -> &'static str {
        match self {
            Self::BelowFloor => "below",
            Self::AboveCeiling => "above",
        }
    }
}

/// One entry of a seek request whose position its partition does not admit.
///
/// Carries where the entry sat in the request, the range that partition admits,
/// and which end was crossed - and deliberately **not** the submitted value. A
/// rejection must not echo request content back through a problem body or the
/// logs that quote one; the range is broker state, and the index is enough for
/// a caller to find the entry it sent.
///
/// [`Self::as_field_violation`] is the single place this becomes a problem-body
/// entry, so every implementation of the API rejects the same input with the
/// same body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionViolation {
    index: usize,
    floor: Sequence,
    ceiling: Sequence,
    breached: OutOfRange,
}

impl PositionViolation {
    /// Starts a violation for the entry at `index` in the request's
    /// `partition_positions` array.
    #[must_use]
    pub fn builder(index: usize) -> PositionViolationBuilder {
        PositionViolationBuilder {
            index,
            floor: Sequence::NONE,
            ceiling: Sequence::NONE,
            breached: OutOfRange::BelowFloor,
        }
    }

    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    #[must_use]
    pub const fn breached(&self) -> OutOfRange {
        self.breached
    }

    /// The problem-body entry for this violation.
    #[must_use]
    pub fn as_field_violation(&self) -> FieldViolation {
        FieldViolation::new(
            format!("partition_positions[{}].value", self.index),
            format!(
                "the position is {} the valid range [{}, {}]",
                self.breached.phrase(),
                self.floor,
                self.ceiling
            ),
            self.breached.reason(),
        )
    }
}

/// Builds a [`PositionViolation`]. The two ends of the range are set by name,
/// because a positional pair of sequences transposes silently.
#[derive(Debug, Clone)]
pub struct PositionViolationBuilder {
    index: usize,
    floor: Sequence,
    ceiling: Sequence,
    breached: OutOfRange,
}

impl PositionViolationBuilder {
    /// The lowest position the partition admits.
    #[must_use]
    pub const fn floor(mut self, floor: Sequence) -> Self {
        self.floor = floor;
        self
    }

    /// The highest position the partition admits.
    #[must_use]
    pub const fn ceiling(mut self, ceiling: Sequence) -> Self {
        self.ceiling = ceiling;
        self
    }

    /// Which end the requested position fell outside of.
    #[must_use]
    pub const fn breached(mut self, breached: OutOfRange) -> Self {
        self.breached = breached;
        self
    }

    #[must_use]
    pub const fn build(self) -> PositionViolation {
        PositionViolation {
            index: self.index,
            floor: self.floor,
            ceiling: self.ceiling,
            breached: self.breached,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum EventBrokerError {
    #[error("invalid producer options: {detail}")]
    InvalidProducerOptions { detail: String, instance: String },

    #[error("invalid consumer options: {detail}")]
    InvalidConsumerOptions { detail: String, instance: String },

    #[error("event type not declared on producer: {type_id}")]
    EventTypeNotDeclared {
        type_id: String,
        detail: String,
        instance: String,
    },

    #[error("event type unknown to types-registry: {type_id}")]
    EventTypeUnknown {
        type_id: String,
        detail: String,
        instance: String,
    },

    #[error("resolved type's parent topic differs from declared topics: {type_id}")]
    TypeNotInDeclaredTopic {
        type_id: String,
        expected_topic: String,
        detail: String,
        instance: String,
    },

    #[error(
        "schema not prepared for {type_id}; call `producer.prepare::<E>()` before opening the txn"
    )]
    SchemaNotPrepared {
        type_id: String,
        detail: String,
        instance: String,
    },

    #[error("event field invalid: {field}: {detail}")]
    InvalidEventField {
        field: &'static str,
        detail: String,
        instance: String,
    },

    /// A text field broke its encoding or length rule. Carries no part of the
    /// submitted value - only the field, what the rule requires, and a
    /// machine-readable reason.
    #[error("text field invalid: {field}: {detail}")]
    InvalidTextField {
        field: &'static str,
        detail: String,
        reason: &'static str,
        instance: String,
    },

    #[error("event data invalid for {type_id}: {errors:?}")]
    EventDataInvalid {
        type_id: String,
        errors: Vec<String>,
        detail: String,
        instance: String,
    },

    #[error("topic not found: {topic}")]
    TopicNotFound {
        topic: String,
        detail: String,
        instance: String,
    },

    #[error("consumer group not found")]
    ConsumerGroupNotFound {
        group_id: ConsumerGroupId,
        detail: String,
        instance: String,
    },

    #[error("consumer group has active members")]
    ConsumerGroupHasActiveMembers { detail: String, instance: String },

    #[error("subscription not found: {id}")]
    SubscriptionNotFound {
        id: SubscriptionId,
        detail: String,
        instance: String,
    },

    #[error("not authorized: {detail}")]
    Unauthorized { detail: String, instance: String },

    #[error("unknown producer: {producer_id:?}")]
    UnknownProducer {
        producer_id: crate::ids::ProducerId,
        detail: String,
        instance: String,
    },

    #[error("sequence violation (broker expects previous={expected_previous})")]
    SequenceViolation {
        expected_previous: i64,
        detail: String,
        instance: String,
    },

    #[error("rate limit exceeded, retry after {retry_after_secs}s")]
    RateLimitExceeded {
        retry_after_secs: u32,
        detail: String,
        instance: String,
    },

    #[error("consumer group at capacity ({active} active members, {partitions} partitions)")]
    GroupAtCapacity {
        active: u32,
        partitions: u32,
        detail: String,
        instance: String,
    },

    #[error("publish rate limited (429), retry after {retry_after_secs}s")]
    RateLimited {
        retry_after_secs: u32,
        detail: String,
        instance: String,
    },

    #[error(
        "batch too large: {count} events / {bytes} bytes (limits: {max_count} events, {max_bytes} bytes)"
    )]
    BatchTooLarge {
        count: usize,
        bytes: usize,
        max_count: usize,
        max_bytes: usize,
        detail: String,
        instance: String,
    },

    #[error("subscription recovery exhausted ({attempts} consecutive re-JOIN failures)")]
    SubscriptionRecoveryExhausted {
        attempts: u32,
        detail: String,
        instance: String,
    },

    /// One or more seek entries named a position their partition does not
    /// admit. Every offender is reported, so a caller correcting several does
    /// not need one round trip each.
    #[error("{} seek position(s) outside the admissible range", violations.len())]
    InvalidInitialPosition {
        violations: Vec<PositionViolation>,
        detail: String,
        instance: String,
    },

    #[error("positions not set for {} partition(s): {}", unseeded.len(), display_unseeded(unseeded))]
    PositionsNotSet {
        unseeded: Vec<(String, u32)>,
        detail: String,
        instance: String,
    },

    #[error("partition {topic}:{partition} is not assigned to this subscription")]
    PartitionNotAssigned {
        topic: String,
        partition: u32,
        detail: String,
        instance: String,
    },

    #[error("a stream is already open for this subscription")]
    StreamingInProgress { detail: String, instance: String },

    #[error("storage backend error")]
    StorageBackend(#[from] StorageBackendError),

    #[error("offset manager error")]
    OffsetManager(#[from] OffsetManagerError),

    #[error("transport: {0}")]
    Transport(String),

    #[error("internal: {0}")]
    Internal(String),

    #[error("{canonical}")]
    Other { canonical: CanonicalError },
}

pub type ConsumerError = EventBrokerError;

impl EventBrokerError {
    /// Wire form of a text-field rejection from [`crate::validate`].
    ///
    /// Carries the field, the rule's requirement and its machine reason, and
    /// deliberately nothing from the submitted value.
    #[must_use]
    pub fn invalid_text_field(
        error: &crate::validate::TextFieldError,
        instance: impl Into<String>,
    ) -> Self {
        EventBrokerError::InvalidTextField {
            field: error.field(),
            detail: error.detail().to_owned(),
            reason: error.rule().as_reason(),
            instance: instance.into(),
        }
    }
}

impl From<EventBrokerError> for CanonicalError {
    fn from(err: EventBrokerError) -> Self {
        match err {
            EventBrokerError::InvalidProducerOptions { detail, .. } => {
                ProducerOptionsResourceError::invalid_argument()
                    .with_field_violation(
                        "producer_options",
                        detail,
                        reasons::INVALID_PRODUCER_OPTIONS,
                    )
                    .create()
            }
            EventBrokerError::InvalidConsumerOptions { detail, .. } => {
                ConsumerOptionsResourceError::invalid_argument()
                    .with_field_violation(
                        "consumer_options",
                        detail,
                        reasons::INVALID_CONSUMER_OPTIONS,
                    )
                    .create()
            }
            EventBrokerError::EventTypeNotDeclared {
                type_id, detail, ..
            } => EventResourceError::invalid_argument()
                .with_resource(type_id)
                .with_field_violation("event_type", detail, reasons::EVENT_TYPE_NOT_DECLARED)
                .create(),
            // The only arm carrying no reason: a 404 whose `resource` names the
            // unresolvable type already says everything a caller can act on, and
            // no other arm reports not-found on this resource.
            EventBrokerError::EventTypeUnknown {
                type_id, detail, ..
            } => EventResourceError::not_found(detail)
                .with_resource(type_id)
                .create(),
            EventBrokerError::TypeNotInDeclaredTopic {
                type_id,
                expected_topic,
                detail,
                ..
            } => EventResourceError::failed_precondition()
                .with_resource(type_id.clone())
                .with_precondition_violation(
                    type_id,
                    format!("{detail}; expected topic {expected_topic}"),
                    reasons::TYPE_NOT_IN_DECLARED_TOPIC,
                )
                .create(),
            EventBrokerError::SchemaNotPrepared {
                type_id, detail, ..
            } => EventResourceError::failed_precondition()
                .with_resource(type_id.clone())
                .with_precondition_violation(type_id, detail, reasons::SCHEMA_NOT_PREPARED)
                .create(),
            EventBrokerError::InvalidEventField { field, detail, .. } => {
                EventResourceError::invalid_argument()
                    .with_field_violation(field, detail, reasons::INVALID_EVENT_FIELD)
                    .create()
            }
            EventBrokerError::InvalidTextField {
                field,
                detail,
                reason,
                ..
            } => RequestResourceError::invalid_argument()
                .with_field_violation(field, detail, reason)
                .create(),
            EventBrokerError::EventDataInvalid {
                type_id,
                errors,
                detail,
                ..
            } => {
                let description = if errors.is_empty() {
                    detail
                } else {
                    format!("{detail}: {}", errors.join("; "))
                };
                EventResourceError::invalid_argument()
                    .with_resource(type_id)
                    .with_field_violation("data", description, reasons::EVENT_DATA_INVALID)
                    .create()
            }
            EventBrokerError::TopicNotFound { topic, detail, .. } => {
                TopicResourceError::not_found(detail)
                    .with_resource(topic)
                    .create()
            }
            EventBrokerError::ConsumerGroupNotFound {
                group_id, detail, ..
            } => ConsumerGroupResourceError::not_found(detail)
                .with_resource(group_id.to_string())
                .create(),
            EventBrokerError::ConsumerGroupHasActiveMembers { detail, .. } => {
                ConsumerGroupResourceError::failed_precondition()
                    .with_precondition_violation(
                        resources::CONSUMER_GROUP,
                        detail,
                        reasons::CONSUMER_GROUP_HAS_ACTIVE_MEMBERS,
                    )
                    .create()
            }
            EventBrokerError::SubscriptionNotFound { id, detail, .. } => {
                SubscriptionResourceError::not_found(detail)
                    .with_resource(id.to_string())
                    .create()
            }
            EventBrokerError::Unauthorized { detail, .. } => {
                let _ = detail;
                EventResourceError::permission_denied()
                    .with_reason(reasons::UNAUTHORIZED)
                    .create()
            }
            EventBrokerError::UnknownProducer {
                producer_id,
                detail,
                ..
            } => ProducerResourceError::not_found(detail)
                .with_resource(producer_id.0.to_string())
                .create(),
            EventBrokerError::SequenceViolation {
                expected_previous,
                detail,
                ..
            } => ProducerResourceError::failed_precondition()
                .with_precondition_violation(
                    "producer_sequence",
                    format!("{detail}; expected previous {expected_previous}"),
                    reasons::SEQUENCE_MISMATCH,
                )
                .create(),
            EventBrokerError::RateLimitExceeded {
                retry_after_secs,
                detail,
                ..
            }
            | EventBrokerError::RateLimited {
                retry_after_secs,
                detail,
                ..
            } => EventResourceError::resource_exhausted(detail.clone())
                .with_quota_violation(reasons::RATE_LIMIT, detail)
                .with_quota_violation_retry_after_seconds(u64::from(retry_after_secs))
                .create(),
            EventBrokerError::GroupAtCapacity {
                active,
                partitions,
                detail,
                ..
            } => ConsumerGroupResourceError::resource_exhausted(detail.clone())
                .with_quota_violation(
                    reasons::CONSUMER_GROUP_CAPACITY,
                    format!("{detail}; active={active}; partitions={partitions}"),
                )
                .create(),
            EventBrokerError::BatchTooLarge {
                count,
                bytes,
                max_count,
                max_bytes,
                detail,
                ..
            } => EventResourceError::invalid_argument()
                .with_field_violation(
                    "batch.count",
                    format!("{detail}; count={count}; max_count={max_count}"),
                    reasons::BATCH_TOO_LARGE,
                )
                .with_field_violation(
                    "batch.bytes",
                    format!("{detail}; bytes={bytes}; max_bytes={max_bytes}"),
                    reasons::BATCH_TOO_LARGE,
                )
                .create(),
            EventBrokerError::SubscriptionRecoveryExhausted { .. } => {
                CanonicalError::service_unavailable().create()
            }
            EventBrokerError::InvalidInitialPosition {
                violations, detail, ..
            } => {
                let mut iter = violations.iter().map(PositionViolation::as_field_violation);
                // Non-empty by construction - the error exists because an
                // entry was rejected - but the builder's typestate needs the
                // first violation separately, so the empty case is spelled out.
                let Some(first) = iter.next() else {
                    return RequestResourceError::invalid_argument()
                        .with_format(detail)
                        .create();
                };
                // The offending thing is a field of the request, not a
                // partition: the violation names an entry in
                // `partition_positions`, and the gear's own SEEK rejection
                // stamps the same resource. A rejection that differed here
                // between an implementation and its reference would be a
                // difference a caller can see.
                let mut with_violations = RequestResourceError::invalid_argument()
                    .with_field_violation(first.field, first.description, first.reason);
                for violation in iter {
                    with_violations = with_violations.with_field_violation(
                        violation.field,
                        violation.description,
                        violation.reason,
                    );
                }
                with_violations.create()
            }
            EventBrokerError::PositionsNotSet {
                unseeded, detail, ..
            } => {
                let mut iter = unseeded.into_iter();
                let Some((topic, partition)) = iter.next() else {
                    return StreamResourceError::failed_precondition()
                        .with_precondition_violation(
                            "stream.positions",
                            detail,
                            reasons::POSITIONS_NOT_SET,
                        )
                        .create();
                };
                let mut with_violations = StreamResourceError::failed_precondition()
                    .with_precondition_violation(
                        format!("{topic}:{partition}"),
                        detail.clone(),
                        reasons::POSITIONS_NOT_SET,
                    );
                for (topic, partition) in iter {
                    with_violations = with_violations.with_precondition_violation(
                        format!("{topic}:{partition}"),
                        detail.clone(),
                        reasons::POSITIONS_NOT_SET,
                    );
                }
                with_violations.create()
            }
            EventBrokerError::PartitionNotAssigned {
                topic,
                partition,
                detail,
                ..
            } => PartitionResourceError::failed_precondition()
                .with_resource(format!("{topic}:{partition}"))
                .with_precondition_violation(
                    format!("{topic}:{partition}"),
                    detail,
                    reasons::PARTITION_NOT_ASSIGNED,
                )
                .create(),
            EventBrokerError::StreamingInProgress { detail, .. } => {
                StreamResourceError::failed_precondition()
                    .with_precondition_violation(
                        resources::STREAM,
                        detail,
                        reasons::STREAMING_IN_PROGRESS,
                    )
                    .create()
            }
            EventBrokerError::StorageBackend(err) => CanonicalError::from(err),
            EventBrokerError::OffsetManager(err) => CanonicalError::from(err),
            EventBrokerError::Transport(_) => CanonicalError::service_unavailable().create(),
            EventBrokerError::Internal(detail) => CanonicalError::internal(detail).create(),
            EventBrokerError::Other { canonical } => canonical,
        }
    }
}

/// First field-violation reason on an `InvalidArgument`/`OutOfRange` context, if
/// the context carries field violations at all.
fn first_invalid_argument_reason(
    ctx: &toolkit_canonical_errors::context::InvalidArgument,
) -> Option<&str> {
    match ctx {
        toolkit_canonical_errors::context::InvalidArgumentV1::FieldViolations {
            field_violations,
        } => field_violations.first().map(|v| v.reason.as_str()),
        _ => None,
    }
}

/// Inverse of [`From<EventBrokerError> for CanonicalError`]. Recovers the typed
/// SDK error from a canonical error decoded off the wire, keyed on the category,
/// the `resource_type`, and the first violation's reason - the same identity the
/// forward mapping stamps. See the change design's "Error mapping" table.
///
/// Variants whose payload is not fully carried on the wire (for example the
/// `&'static str` field name in `InvalidEventField`/`InvalidTextField`, or the
/// counted `active`/`partitions` in `GroupAtCapacity`) are recovered to the
/// right variant with the wire-available fields; fields absent from the wire
/// take a neutral value. Anything the table does not name falls to
/// [`EventBrokerError::Other`], which preserves the canonical error verbatim.
impl From<CanonicalError> for EventBrokerError {
    fn from(canonical: CanonicalError) -> Self {
        let detail = canonical.detail().to_owned();
        let instance = String::new();
        match &canonical {
            CanonicalError::PermissionDenied { .. } => Self::Unauthorized { detail, instance },

            CanonicalError::NotFound {
                resource_type,
                resource_name,
                ..
            } => {
                let name = resource_name.clone().unwrap_or_default();
                match resource_type.as_deref() {
                    Some(resources::TOPIC) => Self::TopicNotFound {
                        topic: name,
                        detail,
                        instance,
                    },
                    Some(resources::CONSUMER_GROUP) => Self::ConsumerGroupNotFound {
                        group_id: ConsumerGroupId::from_gts(&name),
                        detail,
                        instance,
                    },
                    Some(resources::SUBSCRIPTION) => match uuid::Uuid::parse_str(&name) {
                        Ok(id) => Self::SubscriptionNotFound {
                            id: SubscriptionId(id),
                            detail,
                            instance,
                        },
                        Err(_) => Self::Other { canonical },
                    },
                    Some(resources::PRODUCER) => match uuid::Uuid::parse_str(&name) {
                        Ok(id) => Self::UnknownProducer {
                            producer_id: ProducerId(id),
                            detail,
                            instance,
                        },
                        Err(_) => Self::Other { canonical },
                    },
                    Some(resources::EVENT) => Self::EventTypeUnknown {
                        type_id: name,
                        detail,
                        instance,
                    },
                    Some(resources::PARTITION) => {
                        Self::StorageBackend(StorageBackendError::PartitionNotFound { detail, instance })
                    }
                    _ => Self::Other { canonical },
                }
            }

            CanonicalError::ResourceExhausted {
                ctx,
                resource_type,
                ..
            } => {
                let retry_after_secs = ctx
                    .violations
                    .first()
                    .and_then(|v| v.retry_after_seconds)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or_default();
                if resource_type.as_deref() == Some(resources::CONSUMER_GROUP) {
                    // active/partitions are not carried on the wire; the variant
                    // is recovered, the counts default.
                    Self::GroupAtCapacity {
                        active: 0,
                        partitions: 0,
                        detail,
                        instance,
                    }
                } else {
                    Self::RateLimitExceeded {
                        retry_after_secs,
                        detail,
                        instance,
                    }
                }
            }

            CanonicalError::FailedPrecondition {
                ctx,
                resource_type,
                ..
            } => {
                let reason = ctx.violations.first().map(|v| v.type_.as_str());
                match (resource_type.as_deref(), reason) {
                    (_, Some(reasons::SEQUENCE_MISMATCH)) => Self::SequenceViolation {
                        // The expected-previous value rides in the violation's
                        // description ("...; expected previous <n>"), not the
                        // top-level detail, so recover it from there.
                        expected_previous: ctx
                            .violations
                            .first()
                            .map(|v| expected_previous_from_detail(&v.description))
                            .unwrap_or_default(),
                        detail,
                        instance,
                    },
                    (_, Some(reasons::CONSUMER_GROUP_HAS_ACTIVE_MEMBERS)) => {
                        Self::ConsumerGroupHasActiveMembers { detail, instance }
                    }
                    (_, Some(reasons::STREAMING_IN_PROGRESS)) => {
                        Self::StreamingInProgress { detail, instance }
                    }
                    (_, Some(reasons::IN_TX_OFFSETS_NOT_SUPPORTED)) => {
                        Self::OffsetManager(OffsetManagerError::InTxNotSupported { detail, instance })
                    }
                    _ => Self::Other { canonical },
                }
            }

            CanonicalError::OutOfRange { .. } => Self::Other { canonical },

            CanonicalError::InvalidArgument { ctx, .. } => {
                match first_invalid_argument_reason(ctx) {
                    Some(reasons::INVALID_PRODUCER_OPTIONS) => {
                        Self::InvalidProducerOptions { detail, instance }
                    }
                    Some(reasons::INVALID_CONSUMER_OPTIONS) => {
                        Self::InvalidConsumerOptions { detail, instance }
                    }
                    _ => Self::Other { canonical },
                }
            }

            CanonicalError::Internal { .. } => Self::Internal(detail),
            CanonicalError::ServiceUnavailable { .. } => Self::Transport(detail),
            _ => Self::Other { canonical },
        }
    }
}

fn expected_previous_from_detail(detail: &str) -> i64 {
    detail
        .split(|ch: char| !ch.is_ascii_digit() && ch != '-')
        .rfind(|part| !part.is_empty())
        .and_then(|part| part.parse::<i64>().ok())
        .unwrap_or_default()
}

impl From<StorageBackendError> for CanonicalError {
    fn from(err: StorageBackendError) -> Self {
        match err {
            StorageBackendError::Unavailable { .. }
            // Unreachable in practice: a retention pass answers no request, so
            // its failure has no client to be mapped for. Mapped alongside
            // `Unavailable` rather than left to a catch-all so that adding a
            // variant stays a compile error here.
            | StorageBackendError::RetentionFailed { .. } => {
                CanonicalError::service_unavailable().create()
            }
            StorageBackendError::InvalidConfig { detail, .. } => {
                StorageResourceError::invalid_argument()
                    .with_field_violation("backend_config", detail, reasons::INVALID_BACKEND_CONFIG)
                    .create()
            }
            StorageBackendError::OffsetOutOfRange {
                floor,
                ceiling,
                breached,
                detail,
                ..
            } => OffsetResourceError::out_of_range(detail.clone())
                .with_field_violation(
                    "offset",
                    format!(
                        "the position is {} the valid range [{floor}, {ceiling}]",
                        breached.phrase()
                    ),
                    breached.reason(),
                )
                .create(),
            StorageBackendError::PartitionNotFound { detail, .. } => {
                PartitionResourceError::not_found(detail)
                    .with_resource(resources::PARTITION)
                    .create()
            }
            StorageBackendError::PersistFailed { .. } | StorageBackendError::ReadFailed { .. } => {
                CanonicalError::service_unavailable().create()
            }
            StorageBackendError::Internal(detail) => CanonicalError::internal(detail).create(),
        }
    }
}

impl From<OffsetManagerError> for CanonicalError {
    fn from(err: OffsetManagerError) -> Self {
        match err {
            OffsetManagerError::InTxNotSupported { detail, .. } => {
                OffsetResourceError::failed_precondition()
                    .with_precondition_violation(
                        resources::OFFSET,
                        detail,
                        reasons::IN_TX_OFFSETS_NOT_SUPPORTED,
                    )
                    .create()
            }
            OffsetManagerError::PersistFailed { .. } | OffsetManagerError::LoadFailed { .. } => {
                CanonicalError::service_unavailable().create()
            }
            OffsetManagerError::Internal(detail) => CanonicalError::internal(detail).create(),
        }
    }
}

/// Bounded `Display` formatter for `PositionsNotSet::unseeded` - first 5 then
/// "...and N more" to keep log output bounded.
fn display_unseeded(unseeded: &[(String, u32)]) -> String {
    const CAP: usize = 5;
    let shown = unseeded
        .iter()
        .take(CAP)
        .map(|(t, p)| format!("{t}:{p}"))
        .collect::<Vec<_>>()
        .join(", ");
    if unseeded.len() > CAP {
        format!("{shown}, ...and {} more", unseeded.len() - CAP)
    } else {
        shown
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum StorageBackendError {
    #[error("backend unavailable: {reason}")]
    Unavailable {
        reason: String,
        detail: String,
        instance: String,
    },

    #[error("invalid backend config")]
    InvalidConfig { detail: String, instance: String },

    /// A requested position is not one a cursor may hold in this partition.
    ///
    /// Carries the range the partition admits and which end was crossed, and
    /// not the submitted value - see [`PositionViolation`] for why.
    #[error("position outside the admissible range [{floor}, {ceiling}]")]
    OffsetOutOfRange {
        floor: Sequence,
        ceiling: Sequence,
        breached: OutOfRange,
        detail: String,
        instance: String,
    },

    #[error("partition not found")]
    PartitionNotFound { detail: String, instance: String },

    #[error("persist failed: {reason}")]
    PersistFailed {
        reason: String,
        detail: String,
        instance: String,
    },

    #[error("read failed: {reason}")]
    ReadFailed {
        reason: String,
        detail: String,
        instance: String,
    },

    /// A retention pass could not be applied. Distinct from `PersistFailed`
    /// because nothing a caller did produced it: retention runs on the broker's
    /// own tick, so this never answers a request and never reaches a consumer.
    /// A failed pass removes nothing and the next one has the same work to do.
    #[error("retention pass failed: {reason}")]
    RetentionFailed {
        reason: String,
        detail: String,
        instance: String,
    },

    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum OffsetManagerError {
    #[error("offset manager does not support in-tx persistence")]
    InTxNotSupported { detail: String, instance: String },

    #[error("persist failed: {reason}")]
    PersistFailed {
        reason: String,
        detail: String,
        instance: String,
        #[source]
        source: Option<Arc<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("load failed: {reason}")]
    LoadFailed {
        reason: String,
        detail: String,
        instance: String,
        #[source]
        source: Option<Arc<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("internal: {0}")]
    Internal(String),
}

impl OffsetManagerError {
    pub fn persist_failed(
        reason: impl Into<String>,
        detail: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::PersistFailed {
            reason: reason.into(),
            detail: detail.into(),
            instance: String::new(),
            source: Some(Arc::new(source)),
        }
    }

    pub fn load_failed(
        reason: impl Into<String>,
        detail: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::LoadFailed {
            reason: reason.into(),
            detail: detail.into(),
            instance: String::new(),
            source: Some(Arc::new(source)),
        }
    }
}

#[cfg(test)]
mod canonical_roundtrip_tests {
    use super::*;

    /// Each of these carries identity the wire preserves, so
    /// `EventBrokerError -> CanonicalError -> EventBrokerError` returns the same
    /// variant. Variants whose payload the wire cannot carry (validation field
    /// names, counted capacities) are covered by the `From<CanonicalError>` doc,
    /// not here.
    fn recoverable_samples() -> Vec<EventBrokerError> {
        vec![
            EventBrokerError::Unauthorized {
                detail: "denied".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::TopicNotFound {
                topic: "gts.cf.core.events.topic.v1~acme.billing.orders.stream.v1".to_owned(),
                detail: "no such topic".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::SubscriptionNotFound {
                id: SubscriptionId(uuid::Uuid::from_u128(7)),
                detail: "gone".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::UnknownProducer {
                producer_id: ProducerId(uuid::Uuid::from_u128(9)),
                detail: "gone".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::EventTypeUnknown {
                type_id: "gts.cf.core.events.event.v1~acme.orders.created.v1~".to_owned(),
                detail: "unknown".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::RateLimitExceeded {
                retry_after_secs: 5,
                detail: "slow down".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::GroupAtCapacity {
                active: 3,
                partitions: 4,
                detail: "full".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::SequenceViolation {
                expected_previous: 41,
                detail: "expected previous 41".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::ConsumerGroupHasActiveMembers {
                detail: "busy".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::StreamingInProgress {
                detail: "one stream".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::InvalidProducerOptions {
                detail: "bad".to_owned(),
                instance: String::new(),
            },
            EventBrokerError::InvalidConsumerOptions {
                detail: "bad".to_owned(),
                instance: String::new(),
            },
        ]
    }

    #[test]
    fn typed_errors_round_trip_through_canonical() {
        for original in recoverable_samples() {
            let canonical = CanonicalError::from(original.clone());
            let recovered = EventBrokerError::from(canonical);
            assert_eq!(
                std::mem::discriminant(&recovered),
                std::mem::discriminant(&original),
                "variant changed across the wire round-trip: {original:?} -> {recovered:?}"
            );
        }
    }

    #[test]
    fn sequence_violation_recovers_expected_previous() {
        let original = EventBrokerError::SequenceViolation {
            expected_previous: 41,
            detail: "expected previous 41".to_owned(),
            instance: String::new(),
        };
        match EventBrokerError::from(CanonicalError::from(original)) {
            EventBrokerError::SequenceViolation {
                expected_previous, ..
            } => assert_eq!(expected_previous, 41),
            other => panic!("expected SequenceViolation, got {other:?}"),
        }
    }
}
