//! Unit tests for the error model.

use std::error::Error;

use event_broker_sdk::error::{reasons, resources};
use event_broker_sdk::{
    ConsumerError, ConsumerGroupId, EventBrokerError, OffsetManagerError, OutOfRange,
    PositionViolation, ProducerId, Sequence, StorageBackendError, SubscriptionId,
};
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

#[test]
fn display_interpolates_fields() {
    let e = EventBrokerError::EventTypeNotDeclared {
        type_id: "gts.cf.core.events.event.v1~example.foo.v1~".into(),
        detail: "d".into(),
        instance: String::new(),
    };
    assert!(
        e.to_string()
            .contains("gts.cf.core.events.event.v1~example.foo.v1~")
    );

    let e = EventBrokerError::SequenceViolation {
        expected_previous: 42,
        detail: "d".into(),
        instance: String::new(),
    };
    assert!(e.to_string().contains("42"));

    let e = EventBrokerError::RateLimitExceeded {
        retry_after_secs: 30,
        detail: "d".into(),
        instance: String::new(),
    };
    assert!(e.to_string().contains("30"));
}

#[test]
fn from_conversions() {
    let storage_err = StorageBackendError::Internal("test".into());
    let broker_err = EventBrokerError::from(storage_err);
    assert!(matches!(broker_err, EventBrokerError::StorageBackend(_)));

    let offset_err = OffsetManagerError::Internal("test".into());
    let broker_err = EventBrokerError::from(offset_err);
    assert!(matches!(broker_err, EventBrokerError::OffsetManager(_)));
}

#[test]
fn consumer_error_alias() {
    let e: ConsumerError = EventBrokerError::Internal("test".into());
    assert!(!e.to_string().is_empty());
}

#[derive(Debug, thiserror::Error)]
#[error("db failed")]
struct DbFailure;

#[test]
fn offset_manager_error_preserves_source_through_broker_error() {
    let offset_err = OffsetManagerError::persist_failed("write offset", "upsert failed", DbFailure);
    assert_eq!(
        offset_err.source().map(ToString::to_string),
        Some("db failed".to_owned())
    );

    let broker_err = EventBrokerError::from(offset_err);
    let source = broker_err
        .source()
        .expect("broker error should expose offset error");
    assert_eq!(source.to_string(), "persist failed: write offset");
    assert_eq!(
        source.source().map(ToString::to_string),
        Some("db failed".to_owned())
    );
}

fn fixed_uuid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn problem_json(err: impl Into<CanonicalError>) -> Value {
    let canonical = err.into();
    let problem = Problem::from(canonical)
        .with_instance("/v1/event-broker/test")
        .with_trace_id("trace-123");
    serde_json::to_value(problem).expect("problem serializes")
}

fn assert_problem(err: impl Into<CanonicalError>, expected: Value) {
    assert_eq!(problem_json(err), expected);
}

#[test]
fn top_level_event_broker_errors_have_full_canonical_representation() {
    let group_id = ConsumerGroupId(fixed_uuid(1));
    let subscription_id = SubscriptionId(fixed_uuid(2));
    let producer_id = ProducerId(fixed_uuid(3));

    let cases = [
        (
            EventBrokerError::InvalidProducerOptions {
                detail: "source is required".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "producer_options",
                        "description": "source is required",
                        "reason": reasons::INVALID_PRODUCER_OPTIONS
                    }],
                    "resource_type": resources::PRODUCER_OPTIONS
                }
            }),
        ),
        (
            EventBrokerError::InvalidConsumerOptions {
                detail: "group is required".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "consumer_options",
                        "description": "group is required",
                        "reason": reasons::INVALID_CONSUMER_OPTIONS
                    }],
                    "resource_type": resources::CONSUMER_OPTIONS
                }
            }),
        ),
        (
            EventBrokerError::EventTypeNotDeclared {
                type_id: "gts.cf.core.events.event.v1~example.orders.created.v1~".into(),
                detail: "type is not declared".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "event_type",
                        "description": "type is not declared",
                        "reason": reasons::EVENT_TYPE_NOT_DECLARED
                    }],
                    "resource_type": resources::EVENT,
                    "resource_name": "gts.cf.core.events.event.v1~example.orders.created.v1~"
                }
            }),
        ),
        (
            EventBrokerError::EventTypeUnknown {
                type_id: "gts.cf.core.events.event.v1~example.orders.created.v1~".into(),
                detail: "event type not found".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
                "title": "Not Found",
                "status": 404,
                "detail": "event type not found",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "resource_type": resources::EVENT,
                    "resource_name": "gts.cf.core.events.event.v1~example.orders.created.v1~"
                }
            }),
        ),
        (
            EventBrokerError::TypeNotInDeclaredTopic {
                type_id: "gts.cf.core.events.event.v1~example.orders.created.v1~".into(),
                expected_topic: "orders".into(),
                detail: "type belongs to another topic".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "type": reasons::TYPE_NOT_IN_DECLARED_TOPIC,
                        "subject": "gts.cf.core.events.event.v1~example.orders.created.v1~",
                        "description": "type belongs to another topic; expected topic orders"
                    }],
                    "resource_type": resources::EVENT,
                    "resource_name": "gts.cf.core.events.event.v1~example.orders.created.v1~"
                }
            }),
        ),
        (
            EventBrokerError::SchemaNotPrepared {
                type_id: "gts.cf.core.events.event.v1~example.orders.created.v1~".into(),
                detail: "schema cache missing".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "type": reasons::SCHEMA_NOT_PREPARED,
                        "subject": "gts.cf.core.events.event.v1~example.orders.created.v1~",
                        "description": "schema cache missing"
                    }],
                    "resource_type": resources::EVENT,
                    "resource_name": "gts.cf.core.events.event.v1~example.orders.created.v1~"
                }
            }),
        ),
        (
            EventBrokerError::InvalidEventField {
                field: "subject",
                detail: "must not be empty".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "subject",
                        "description": "must not be empty",
                        "reason": reasons::INVALID_EVENT_FIELD
                    }],
                    "resource_type": resources::EVENT
                }
            }),
        ),
        (
            EventBrokerError::InvalidTextField {
                field: "description",
                detail: "must contain only printable ASCII (0x20-0x7E)".into(),
                reason: reasons::ASCII_ONLY,
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "description",
                        "description": "must contain only printable ASCII (0x20-0x7E)",
                        "reason": reasons::ASCII_ONLY
                    }],
                    "resource_type": resources::REQUEST
                }
            }),
        ),
        (
            EventBrokerError::InvalidTextField {
                field: "client_agent",
                detail: "must be 1-256 bytes, got 257".into(),
                reason: reasons::FIELD_TOO_LONG,
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "client_agent",
                        "description": "must be 1-256 bytes, got 257",
                        "reason": reasons::FIELD_TOO_LONG
                    }],
                    "resource_type": resources::REQUEST
                }
            }),
        ),
        (
            EventBrokerError::EventDataInvalid {
                type_id: "gts.cf.core.events.event.v1~example.orders.created.v1~".into(),
                errors: vec!["/amount must be >= 0".into()],
                detail: "payload invalid".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "data",
                        "description": "payload invalid: /amount must be >= 0",
                        "reason": reasons::EVENT_DATA_INVALID
                    }],
                    "resource_type": resources::EVENT,
                    "resource_name": "gts.cf.core.events.event.v1~example.orders.created.v1~"
                }
            }),
        ),
        (
            EventBrokerError::TopicNotFound {
                topic: "orders".into(),
                detail: "topic not found".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
                "title": "Not Found",
                "status": 404,
                "detail": "topic not found",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "resource_type": resources::TOPIC,
                    "resource_name": "orders"
                }
            }),
        ),
        (
            EventBrokerError::ConsumerGroupNotFound {
                group_id,
                detail: "consumer group not found".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
                "title": "Not Found",
                "status": 404,
                "detail": "consumer group not found",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "resource_type": resources::CONSUMER_GROUP,
                    "resource_name": group_id.to_string()
                }
            }),
        ),
        (
            EventBrokerError::ConsumerGroupHasActiveMembers {
                detail: "group has active members".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "type": reasons::CONSUMER_GROUP_HAS_ACTIVE_MEMBERS,
                        "subject": resources::CONSUMER_GROUP,
                        "description": "group has active members"
                    }],
                    "resource_type": resources::CONSUMER_GROUP
                }
            }),
        ),
        (
            EventBrokerError::SubscriptionNotFound {
                id: subscription_id,
                detail: "subscription not found".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
                "title": "Not Found",
                "status": 404,
                "detail": "subscription not found",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "resource_type": resources::SUBSCRIPTION,
                    "resource_name": subscription_id.to_string()
                }
            }),
        ),
        (
            EventBrokerError::Unauthorized {
                detail: "tenant boundary violation".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~",
                "title": "Permission Denied",
                "status": 403,
                "detail": "You do not have permission to perform this operation",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "reason": reasons::UNAUTHORIZED,
                    "resource_type": resources::EVENT
                }
            }),
        ),
        (
            EventBrokerError::UnknownProducer {
                producer_id,
                detail: "producer not found".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
                "title": "Not Found",
                "status": 404,
                "detail": "producer not found",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "resource_type": resources::PRODUCER,
                    "resource_name": producer_id.0.to_string()
                }
            }),
        ),
        (
            EventBrokerError::SequenceViolation {
                expected_previous: 41,
                detail: "sequence mismatch".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "type": reasons::SEQUENCE_MISMATCH,
                        "subject": "producer_sequence",
                        "description": "sequence mismatch; expected previous 41"
                    }],
                    "resource_type": resources::PRODUCER
                }
            }),
        ),
        (
            EventBrokerError::RateLimitExceeded {
                retry_after_secs: 30,
                detail: "rate limit exceeded".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.resource_exhausted.v1~",
                "title": "Resource Exhausted",
                "status": 429,
                "detail": "rate limit exceeded",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "subject": reasons::RATE_LIMIT,
                        "description": "rate limit exceeded",
                        "retry_after_seconds": 30
                    }],
                    "resource_type": resources::EVENT
                }
            }),
        ),
        (
            EventBrokerError::GroupAtCapacity {
                active: 4,
                partitions: 4,
                detail: "group at capacity".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.resource_exhausted.v1~",
                "title": "Resource Exhausted",
                "status": 429,
                "detail": "group at capacity",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "subject": reasons::CONSUMER_GROUP_CAPACITY,
                        "description": "group at capacity; active=4; partitions=4"
                    }],
                    "resource_type": resources::CONSUMER_GROUP
                }
            }),
        ),
        (
            EventBrokerError::RateLimited {
                retry_after_secs: 15,
                detail: "publish rate limited".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.resource_exhausted.v1~",
                "title": "Resource Exhausted",
                "status": 429,
                "detail": "publish rate limited",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "subject": reasons::RATE_LIMIT,
                        "description": "publish rate limited",
                        "retry_after_seconds": 15
                    }],
                    "resource_type": resources::EVENT
                }
            }),
        ),
        (
            EventBrokerError::BatchTooLarge {
                count: 101,
                bytes: 2048,
                max_count: 100,
                max_bytes: 1024,
                detail: "batch too large".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [
                        {
                            "field": "batch.count",
                            "description": "batch too large; count=101; max_count=100",
                            "reason": reasons::BATCH_TOO_LARGE
                        },
                        {
                            "field": "batch.bytes",
                            "description": "batch too large; bytes=2048; max_bytes=1024",
                            "reason": reasons::BATCH_TOO_LARGE
                        }
                    ],
                    "resource_type": resources::EVENT
                }
            }),
        ),
        (
            EventBrokerError::SubscriptionRecoveryExhausted {
                attempts: 3,
                detail: "rejoin failed".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Service temporarily unavailable",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
        (
            // Two offenders, one below its partition's floor and one above
            // its ceiling: both are reported, each with the range it missed
            // and no part of the value that was submitted.
            EventBrokerError::InvalidInitialPosition {
                violations: vec![
                    PositionViolation::builder(0)
                        .floor(Sequence::assigned(99))
                        .ceiling(Sequence::assigned(5000))
                        .breached(OutOfRange::BelowFloor)
                        .build(),
                    PositionViolation::builder(2)
                        .floor(Sequence::NONE)
                        .ceiling(Sequence::assigned(12))
                        .breached(OutOfRange::AboveCeiling)
                        .build(),
                ],
                detail: "Request validation failed".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [
                        {
                            "field": "partition_positions[0].value",
                            "description": "the position is below the valid range [99, 5000]",
                            "reason": reasons::BELOW_RETENTION_FLOOR
                        },
                        {
                            "field": "partition_positions[2].value",
                            "description": "the position is above the valid range [0, 12]",
                            "reason": reasons::ABOVE_HIGH_WATER_MARK
                        }
                    ],
                    "resource_type": resources::REQUEST
                }
            }),
        ),
        (
            EventBrokerError::PositionsNotSet {
                unseeded: vec![("orders".into(), 0), ("orders".into(), 1)],
                detail: "positions not seeded".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [
                        {
                            "type": reasons::POSITIONS_NOT_SET,
                            "subject": "orders:0",
                            "description": "positions not seeded"
                        },
                        {
                            "type": reasons::POSITIONS_NOT_SET,
                            "subject": "orders:1",
                            "description": "positions not seeded"
                        }
                    ],
                    "resource_type": resources::STREAM
                }
            }),
        ),
        (
            EventBrokerError::PartitionNotAssigned {
                topic: "orders".into(),
                partition: 2,
                detail: "partition not assigned".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "type": reasons::PARTITION_NOT_ASSIGNED,
                        "subject": "orders:2",
                        "description": "partition not assigned"
                    }],
                    "resource_type": resources::PARTITION,
                    "resource_name": "orders:2"
                }
            }),
        ),
        (
            EventBrokerError::StreamingInProgress {
                detail: "stream already open".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "type": reasons::STREAMING_IN_PROGRESS,
                        "subject": resources::STREAM,
                        "description": "stream already open"
                    }],
                    "resource_type": resources::STREAM
                }
            }),
        ),
        (
            EventBrokerError::Transport("tcp reset by peer".into()),
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Service temporarily unavailable",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
        (
            EventBrokerError::Internal("secret stack frame".into()),
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.internal.v1~",
                "title": "Internal",
                "status": 500,
                "detail": "An internal error occurred. Please retry later.",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
    ];

    for (err, expected) in cases {
        assert_problem(err, expected);
    }
}

#[test]
fn nested_storage_backend_errors_have_full_canonical_representation() {
    let cases = [
        (
            StorageBackendError::Unavailable {
                reason: "database refused connection".into(),
                detail: "backend unavailable".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Service temporarily unavailable",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
        (
            StorageBackendError::InvalidConfig {
                detail: "missing DSN".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
                "title": "Invalid Argument",
                "status": 400,
                "detail": "Request validation failed",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "backend_config",
                        "description": "missing DSN",
                        "reason": reasons::INVALID_BACKEND_CONFIG
                    }],
                    "resource_type": resources::STORAGE
                }
            }),
        ),
        (
            StorageBackendError::OffsetOutOfRange {
                floor: Sequence::assigned(20),
                ceiling: Sequence::assigned(90),
                breached: OutOfRange::BelowFloor,
                detail: "valid positions are [20, 90]".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.out_of_range.v1~",
                "title": "Out of Range",
                "status": 400,
                "detail": "valid positions are [20, 90]",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "field_violations": [{
                        "field": "offset",
                        "description": "the position is below the valid range [20, 90]",
                        "reason": reasons::BELOW_RETENTION_FLOOR
                    }],
                    "resource_type": resources::OFFSET
                }
            }),
        ),
        (
            StorageBackendError::PartitionNotFound {
                detail: "partition missing".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
                "title": "Not Found",
                "status": 404,
                "detail": "partition missing",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "resource_type": resources::PARTITION,
                    "resource_name": resources::PARTITION
                }
            }),
        ),
        (
            StorageBackendError::PersistFailed {
                reason: "unique index detail".into(),
                detail: "persist failed".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Service temporarily unavailable",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
        (
            StorageBackendError::ReadFailed {
                reason: "driver timeout".into(),
                detail: "read failed".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Service temporarily unavailable",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
        (
            StorageBackendError::Internal("private storage invariant".into()),
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.internal.v1~",
                "title": "Internal",
                "status": 500,
                "detail": "An internal error occurred. Please retry later.",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
    ];

    for (err, expected) in cases {
        assert_problem(err, expected);
    }
}

#[test]
fn nested_offset_manager_errors_have_full_canonical_representation() {
    let cases = [
        (
            OffsetManagerError::InTxNotSupported {
                detail: "store cannot join transaction".into(),
                instance: "ignored".into(),
            },
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
                "title": "Failed Precondition",
                "status": 400,
                "detail": "Operation precondition not met",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {
                    "violations": [{
                        "type": reasons::IN_TX_OFFSETS_NOT_SUPPORTED,
                        "subject": resources::OFFSET,
                        "description": "store cannot join transaction"
                    }],
                    "resource_type": resources::OFFSET
                }
            }),
        ),
        (
            OffsetManagerError::persist_failed(
                "db write secret",
                "offset persist failed",
                DbFailure,
            ),
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Service temporarily unavailable",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
        (
            OffsetManagerError::load_failed("db read secret", "offset load failed", DbFailure),
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Service temporarily unavailable",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
        (
            OffsetManagerError::Internal("private offset invariant".into()),
            json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.internal.v1~",
                "title": "Internal",
                "status": 500,
                "detail": "An internal error occurred. Please retry later.",
                "instance": "/v1/event-broker/test",
                "trace_id": "trace-123",
                "context": {}
            }),
        ),
    ];

    for (err, expected) in cases {
        assert_problem(err, expected);
    }
}

#[test]
fn private_diagnostics_do_not_leak_to_production_problem() {
    let json = problem_json(EventBrokerError::Internal(
        "postgres://user:password@host/db stack frame".into(),
    ));
    let rendered = serde_json::to_string(&json).expect("json renders");

    assert!(!rendered.contains("password"));
    assert!(!rendered.contains("stack frame"));
    assert_eq!(
        json["detail"],
        "An internal error occurred. Please retry later."
    );
}

#[test]
fn failed_precondition_uses_canonical_status_without_local_override() {
    let json = problem_json(EventBrokerError::StreamingInProgress {
        detail: "already open".into(),
        instance: "ignored".into(),
    });

    assert_eq!(
        json["type"],
        "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~"
    );
    assert_eq!(json["status"], 400);
}

#[test]
fn canonical_to_event_broker_projection_preserves_modeled_and_unmodeled_cases() {
    let rate_limited = CanonicalError::from(EventBrokerError::RateLimitExceeded {
        retry_after_secs: 25,
        detail: "slow down".into(),
        instance: String::new(),
    });
    match EventBrokerError::from(rate_limited) {
        EventBrokerError::RateLimitExceeded {
            retry_after_secs: 25,
            ..
        } => {}
        other => panic!("expected rate-limit projection, got {other:?}"),
    }

    // A modeled not-found now round-trips to its typed variant (the inverse
    // mapping was completed in the rest-client change).
    let not_found = CanonicalError::from(EventBrokerError::TopicNotFound {
        topic: "orders".into(),
        detail: "topic missing".into(),
        instance: String::new(),
    });
    match EventBrokerError::from(not_found) {
        EventBrokerError::TopicNotFound { topic, .. } => assert_eq!(topic, "orders"),
        other => panic!("expected TopicNotFound projection, got {other:?}"),
    }

    // An unmodeled case (a text-field validation reason the inverse does not
    // name) still falls through to the catch-all, preserving the canonical error.
    let text_field = CanonicalError::from(EventBrokerError::InvalidTextField {
        field: "client_agent",
        detail: "must be printable ASCII".into(),
        reason: "ascii_only",
        instance: String::new(),
    });
    match EventBrokerError::from(text_field) {
        EventBrokerError::Other { canonical } => {
            assert!(matches!(canonical, CanonicalError::InvalidArgument { .. }));
        }
        other => panic!("expected catch-all projection, got {other:?}"),
    }
}
