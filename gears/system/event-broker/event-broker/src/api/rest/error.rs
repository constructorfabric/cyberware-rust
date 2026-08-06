//! `DomainError` → `CanonicalError`/`Problem` mapping (`domain/error.rs`'s
//! reserved path). Category choice matches `docs/DESIGN.md`'s Hard-Error
//! Catalog (`cpt-cf-evbk-interface-error-codes`). `SequenceViolation`/
//! `BatchTooLarge` keep their canonical `FailedPrecondition`/`InvalidArgument`
//! category (dispatch-relevant fields - `type`/`title` - stay put) but carry a
//! `TransportOverride` so the wire `status` matches `docs/openapi.yaml`'s
//! literal `412`/`413` instead of the category default of `400`
//! (`gears-rust#4465`/`canonical-error-transport-overrides`, which added the
//! escape hatch this needed).

use toolkit::api::canonical_prelude::*;
use toolkit_canonical_errors::Http;

use crate::domain::error::DomainError;

#[resource_error(gts_id!("cf.core.events.topic.v1~"))]
struct TopicResourceError;
#[resource_error(gts_id!("cf.core.events.event_type.v1~"))]
struct EventTypeResourceError;
#[resource_error(gts_id!("cf.core.events.subscription.v1~"))]
struct SubscriptionResourceError;
#[resource_error(gts_id!("cf.core.events.consumer_group.v1~"))]
struct ConsumerGroupResourceError;
#[resource_error(gts_id!("cf.core.events.producer.v1~"))]
struct ProducerResourceError;
/// Fallback for codes not yet tied to one specific entity type (validation
/// on the request itself, rate limiting) - not every `DomainError` is about
/// one addressable resource instance. `pub(crate)` (not module-private):
/// `infra::dispatcher::forward` reuses `invalid_argument()` directly for the
/// dispatcher's oversized-proxied-body `413`, which isn't a `DomainError` at
/// all (see that call site).
#[resource_error(gts_id!("cf.core.events.request.v1~"))]
pub(crate) struct EventBrokerResourceError;

fn not_found(code: &'static str, message: String, resource: String) -> CanonicalError {
    match code {
        "TopicNotFound" => TopicResourceError::not_found(message)
            .with_resource(resource)
            .create(),
        "EventTypeNotFound" => EventTypeResourceError::not_found(message)
            .with_resource(resource)
            .create(),
        "SubscriptionNotFound" => SubscriptionResourceError::not_found(message)
            .with_resource(resource)
            .create(),
        "ConsumerGroupNotFound" => ConsumerGroupResourceError::not_found(message)
            .with_resource(resource)
            .create(),
        "ProducerNotFound" => ProducerResourceError::not_found(message)
            .with_resource(resource)
            .create(),
        _ => EventBrokerResourceError::not_found(message)
            .with_resource(resource)
            .create(),
    }
}

fn forbidden(code: &'static str, message: String) -> CanonicalError {
    // `permission_denied()` takes no detail argument and its resource slot
    // is `ResourceAbsent` (no `with_resource` exists for it) - per
    // `DESIGN.md`'s Hard-Error Catalog, `ConsumerGroupNotOwned`'s `context`
    // is `reason: <detail>`, so the descriptive message rides in
    // `with_reason`, not a separate detail field. Same treatment for the
    // `eb-authz-enforcement` codes: the offending topic/event-type/tenant
    // rides in `message` (`with_reason`), not a resource slot.
    match code {
        "ProducerNotOwned" => ProducerResourceError::permission_denied()
            .with_reason(message)
            .create(),
        "TopicNotAuthorized" => TopicResourceError::permission_denied()
            .with_reason(message)
            .create(),
        "EventTypeNotAuthorized" => EventTypeResourceError::permission_denied()
            .with_reason(message)
            .create(),
        "TenantIdNotAuthorized" | "NotAuthorizedToProduce" => {
            EventBrokerResourceError::permission_denied()
                .with_reason(message)
                .create()
        }
        _ => EventBrokerResourceError::permission_denied()
            .with_reason(message)
            .create(),
    }
}

fn conflict(code: &'static str, message: String, resource: String) -> CanonicalError {
    match code {
        "PositionsNotSet" => {
            let subjects: Vec<String> = message
                .strip_prefix("unseeded assigned partitions: ")
                .unwrap_or(&message)
                .split(", ")
                .map(String::from)
                .collect();
            let description = format!(
                "call POST /v1/subscriptions/{}:seek before re-opening the stream",
                resource
            );
            let mut iter = subjects.into_iter();
            let first = iter.next().unwrap_or_default();
            let mut builder = SubscriptionResourceError::failed_precondition()
                .with_precondition_violation(first, description.clone(), "cursor_missing");
            for subject in iter {
                builder = builder.with_precondition_violation(
                    subject,
                    description.clone(),
                    "cursor_missing",
                );
            }
            builder.with_override(Http::status_code(409)).create()
        }
        "StreamingInProgress" => SubscriptionResourceError::failed_precondition()
            .with_precondition_violation(
                resource,
                "one stream per subscription; DELETE is the only permitted concurrent call",
                "streaming_in_progress",
            )
            .with_override(Http::status_code(409))
            .create(),
        "ConsumerGroupHasActiveMembers" => ConsumerGroupResourceError::failed_precondition()
            .with_precondition_violation(resource, message, "has_active_members")
            .with_override(Http::status_code(409))
            .create(),
        _ => EventBrokerResourceError::aborted(message)
            .with_resource(resource)
            .with_reason(code)
            .create(),
    }
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::Validation { code, message } => {
                // `invalid_argument()` takes no detail argument - `.with_format`
                // is what actually sets `Problem.detail` here. Also carries
                // `InvalidSubjectType`/`SubjectTypeNotAllowed`/
                // `InvalidSubjectTypePattern` (`eb-event-type-enforcement`).
                EventBrokerResourceError::invalid_argument()
                    .with_format(format!("{code}: {message}"))
                    .create()
            }

            DomainError::TextField {
                field,
                detail,
                reason,
            } => EventBrokerResourceError::invalid_argument()
                .with_field_violation(field, detail, reason)
                .create(),

            // Each violation renders itself, in the SDK, so the gear and the
            // reference mock reject the same request with the same body.
            DomainError::SeekOutOfRange { violations } => {
                let mut rendered = violations
                    .iter()
                    .map(event_broker_sdk::PositionViolation::as_field_violation);
                let Some(first) = rendered.next() else {
                    return EventBrokerResourceError::invalid_argument()
                        .with_format("seek rejected with no violation recorded")
                        .create();
                };
                let mut builder = EventBrokerResourceError::invalid_argument()
                    .with_field_violation(first.field, first.description, first.reason);
                for violation in rendered {
                    builder = builder.with_field_violation(
                        violation.field,
                        violation.description,
                        violation.reason,
                    );
                }
                builder.create()
            }

            DomainError::Forbidden {
                code,
                message,
                resource: _,
            } => forbidden(code, message),

            DomainError::NotFound {
                code,
                message,
                resource,
            } => not_found(code, message, resource),

            DomainError::Conflict {
                code,
                message,
                resource,
            } => conflict(code, message, resource),

            DomainError::SequenceViolation {
                topic,
                partition,
                last_sequence,
            } => {
                // `failed_precondition()` takes no detail argument and its
                // only context option is `.with_precondition_violation` (no
                // `.with_format`-equivalent for this context type) - the
                // descriptive text lives in the violation's `description`
                // only, matching `DESIGN.md`'s documented shape exactly
                // (`violations: [{type: "sequence_mismatch", subject:
                // "(producer)", description: "expected_previous=<n>"}]`).
                // `docs/openapi.yaml` documents this as a literal `412`; the
                // override only changes the wire `status`, not the category.
                EventBrokerResourceError::failed_precondition()
                    .with_precondition_violation(
                        "(producer)",
                        format!(
                            "topic={topic} partition={partition} expected_previous={last_sequence}"
                        ),
                        "sequence_mismatch",
                    )
                    .with_override(Http::status_code(412))
                    .create()
            }

            DomainError::BatchTooLarge { count, max } => {
                let message = format!("batch too large: {count} events (max {max})");
                // `docs/openapi.yaml` documents this as a literal `413`.
                EventBrokerResourceError::invalid_argument()
                    .with_format(message)
                    .with_override(Http::status_code(413))
                    .create()
            }

            DomainError::RateLimited {
                code,
                message,
                retry_after_secs,
            } => EventBrokerResourceError::resource_exhausted(message.clone())
                .with_quota_violation(code, message)
                .with_quota_violation_retry_after_seconds(u64::from(retry_after_secs))
                .create(),

            DomainError::StorageUnavailable(detail) => CanonicalError::service_unavailable()
                .with_detail(detail)
                .create(),

            DomainError::Internal(detail) => CanonicalError::internal(detail).create(),
        }
    }
}

/// Round-trip guard for the six `#[resource_error(gts_id!(...))]` literals
/// above against `event-broker-sdk::gts`'s constants - the proc-macro cannot
/// reference the const directly (`eb-gts-type-registration`'s design.md
/// "cannot be centralized"), so a drift between the two is only caught here,
/// matching `resource-group-sdk`'s own `gts_resource_type_round_trip`
/// precedent for the identical limitation.
#[cfg(test)]
mod resource_type_round_trip_tests {
    use event_broker_sdk::gts::{
        CONSUMER_GROUP_RESOURCE_TYPE, EVENT_TYPE_RESOURCE_TYPE, PRODUCER_RESOURCE_TYPE,
        REQUEST_RESOURCE_TYPE, SUBSCRIPTION_RESOURCE_TYPE, TopicV1,
    };
    use toolkit::api::canonical_prelude::CanonicalError;
    use toolkit_canonical_errors::Problem;
    use toolkit_gts::GtsSchema;

    use super::{
        ConsumerGroupResourceError, EventBrokerResourceError, EventTypeResourceError,
        ProducerResourceError, SubscriptionResourceError, TopicResourceError,
    };

    fn resource_type_of(err: CanonicalError) -> String {
        let json = serde_json::to_value(Problem::from(err)).expect("Problem serializes");
        json["context"]["resource_type"]
            .as_str()
            .expect("resource_type must be present")
            .to_owned()
    }

    #[test]
    fn topic_resource_error_matches_sdk_constant() {
        assert_eq!(
            resource_type_of(
                TopicResourceError::not_found("x")
                    .with_resource("x")
                    .create()
            ),
            TopicV1::TYPE_ID,
        );
    }

    #[test]
    fn event_type_resource_error_matches_sdk_constant() {
        assert_eq!(
            resource_type_of(
                EventTypeResourceError::not_found("x")
                    .with_resource("x")
                    .create()
            ),
            EVENT_TYPE_RESOURCE_TYPE,
        );
    }

    #[test]
    fn subscription_resource_error_matches_sdk_constant() {
        assert_eq!(
            resource_type_of(
                SubscriptionResourceError::not_found("x")
                    .with_resource("x")
                    .create()
            ),
            SUBSCRIPTION_RESOURCE_TYPE,
        );
    }

    #[test]
    fn consumer_group_resource_error_matches_sdk_constant() {
        assert_eq!(
            resource_type_of(
                ConsumerGroupResourceError::not_found("x")
                    .with_resource("x")
                    .create()
            ),
            CONSUMER_GROUP_RESOURCE_TYPE,
        );
    }

    #[test]
    fn producer_resource_error_matches_sdk_constant() {
        assert_eq!(
            resource_type_of(
                ProducerResourceError::not_found("x")
                    .with_resource("x")
                    .create()
            ),
            PRODUCER_RESOURCE_TYPE,
        );
    }

    #[test]
    fn event_broker_resource_error_matches_sdk_constant() {
        assert_eq!(
            resource_type_of(
                EventBrokerResourceError::not_found("x")
                    .with_resource("x")
                    .create()
            ),
            REQUEST_RESOURCE_TYPE,
        );
    }
}
