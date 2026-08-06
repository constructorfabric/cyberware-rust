//! `OperationBuilder` route registration (`DESIGN.md:586`). Real ingest/
//! delivery registration (DTOs, `OpenAPI` schemas, `OperationBuilder`
//! chains) lands with #4346 - `register_ingest_routes`/
//! `register_delivery_routes` remain shells for that. `register_dispatcher_routes`
//! is implemented here (`eb-dispatcher-routing`): every route is an opaque
//! forward to a resolved ingest/delivery instance, so its declared response
//! is a placeholder - the authoritative contract lives with #4346's own
//! registration of the same paths. `summary` text is copied verbatim from
//! `DESIGN.md:1022-1041`'s Request Routing table's Purpose column, matching
//! what #4346's own registration of the same path is expected to say - only
//! `description` notes that this particular registration forwards rather
//! than executes the operation. License requirements for these routes are
//! left to whatever #4346 decides for the underlying endpoints; dispatcher
//! forwarding does not invent its own.
//!
//! Two routes register a bare `{id}` axum template rather than DESIGN.md's
//! literal `{id}:reset`/`{id}:seek`: matchit (axum's router, pinned to
//! `=0.8.4` even in the latest published axum release) cannot mix a path
//! parameter with literal text in one segment (design.md D13 - a git-pinned
//! axum fix was investigated and rejected; pulling in *only* the matchit fix
//! would mean vendoring a hand-patched fork). The real wire path is
//! unaffected: `{id}` captures the whole segment value including the colon
//! suffix, and the forwarded request is the client's original, untouched
//! `Request`, whose URI still carries it - see the comments at each
//! registration below.

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder, ResponseSpec};

use crate::api::rest::handlers::delivery::consumer_groups as consumer_groups_h;
use crate::api::rest::handlers::delivery::dto as delivery_dto;
use crate::api::rest::handlers::delivery::streaming as delivery_h;
use crate::api::rest::handlers::delivery::subscriptions as subscriptions_h;
use crate::api::rest::handlers::ingest::dto as ingest_dto;
use crate::api::rest::handlers::ingest::event_types as event_types_h;
use crate::api::rest::handlers::ingest::events as ingest_h;
use crate::api::rest::handlers::ingest::producers as producers_h;
use crate::api::rest::handlers::ingest::topics as topics_h;
use crate::infra::dispatcher::{proxy, router};

const INGEST_API_TAG: &str = "events";
const PRODUCERS_TAG: &str = "producers";
const TOPICS_TAG: &str = "topics";
const EVENT_TYPES_TAG: &str = "event-types";
const CONSUMER_GROUPS_TAG: &str = "consumer-groups";
const SUBSCRIPTIONS_TAG: &str = "subscriptions";

/// Adds `$filter`/`$orderby`/`limit`/`cursor` - the four list endpoints'
/// shared query-param shape (`docs/openapi.yaml`). Not `OperationBuilderODataExt`
/// (`with_odata_filter::<T>()`): these handlers use `OData` + a manual
/// `pagination::eval_filter` closure rather than a typed `FilterField` enum
/// - no typed filter registry exists here, and adding one is out of scope
/// for this change.
fn list_query_params<H, R, S, A, L>(
    builder: OperationBuilder<H, R, S, A, L>,
) -> OperationBuilder<H, R, S, A, L>
where
    H: toolkit::api::operation_builder::HandlerSlot<S>,
    A: toolkit::api::operation_builder::AuthState,
    L: toolkit::api::operation_builder::LicenseState,
{
    builder
        .query_param("$filter", false, "OData v4 filter expression")
        .query_param("$orderby", false, "OData v4 orderby expression")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of items to return",
            "integer",
        )
        .query_param(
            "cursor",
            false,
            "Opaque pagination token from a previous response",
        )
}

/// Every one of #4346's 19 endpoints (`docs/openapi.yaml`) - registered here
/// when `DeploymentMode::ingest_active()`/`delivery_active()`
/// (`module.rs::register_rest`).
///
/// `events:batch` is a deliberate static-segment action suffix (unlike
/// `{id}:reset`/`{id}:seek`'s templated-segment workaround for a matchit
/// routing limitation - see `action_suffix.rs`), not a naming convention
/// violation - DE0801's kebab-case-only check doesn't yet have a carve-out
/// for it.
#[allow(unknown_lints)]
#[allow(de0801_api_endpoint_version)]
pub fn register_ingest_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/event-broker/v1/events")
        .operation_id("event_broker.publish_event")
        .summary("Publish a single event")
        .description(
            "Enqueue one event into the per-topic ingest outbox. Default response is 202 \
             Accepted once durably enqueued; opt into synchronous backend persistence via the \
             `Sync-Wait: true` header or `?wait=persisted` query parameter.",
        )
        .tag(INGEST_API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "wait",
            false,
            "Set to \"persisted\" for synchronous backend persistence",
        )
        .json_request::<ingest_dto::PublishEventRequest>(openapi, "Event to publish")
        .handler(ingest_h::publish_event)
        .json_response(
            StatusCode::ACCEPTED,
            "Accepted, durably enqueued in the outbox",
        )
        .json_response(StatusCode::CREATED, "Persisted to backend (sync mode)")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/events:batch")
        .operation_id("event_broker.publish_batch")
        .summary("Publish a batch of events (atomic per topic)")
        .description(
            "Atomic per-topic batch of up to 100 events. All-or-nothing per topic; mixing \
             topics in one batch is rejected.",
        )
        .tag(INGEST_API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ingest_dto::PublishBatchRequest>(openapi, "Batch of events to publish")
        .handler(ingest_h::publish_batch)
        .json_response(StatusCode::ACCEPTED, "Accepted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/producers")
        .operation_id("event_broker.register_producer")
        .summary("Register a producer and obtain a broker-minted producer_id")
        .description(
            "Mints a producer_id bound to the calling principal. Re-registration mints a \
             fresh producer_id; the broker does not reuse prior ids.",
        )
        .tag(PRODUCERS_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ingest_dto::RegisterProducerRequest>(openapi, "Producer registration")
        .handler(producers_h::register_producer)
        .json_response_with_schema::<ingest_dto::RegisterProducerResponse>(
            openapi,
            StatusCode::CREATED,
            "Producer registered; id minted",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/producers/{id}/cursors")
        .operation_id("event_broker.get_producer_cursors")
        .summary("Read per-(topic, partition) last_sequence for a registered producer")
        .description(
            "Returns the broker's known last_sequence per (topic, partition), grouped by \
             topic, for desync recovery. Principal-bound.",
        )
        .tag(PRODUCERS_TAG)
        .path_param("id", "Producer id (broker-minted UUID)")
        .authenticated()
        .no_license_required()
        .handler(producers_h::get_producer_cursors)
        .json_response_with_schema::<ingest_dto::ProducerCursorsResponse>(
            openapi,
            StatusCode::OK,
            "Producer cursors (topics may be empty)",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // Registered as bare `{id}` (matchit can't mix a path param with literal
    // text in one segment - same limitation `register_dispatcher_routes`
    // already documents for this exact path). `producers_h::reset_producer`
    // splits the `:reset` suffix out of the captured value itself.
    // [todo]: please use some tags for axum update
    router = OperationBuilder::post("/event-broker/v1/producers/{id}")
        .operation_id("event_broker.reset_producer")
        .summary("Reset the ingest-side chain state for a producer")
        .description(
            "Operator-driven chain reset. When the request body is absent or empty, all rows \
             for the producer are cleared; when `topic`/`partition` are present, only the \
             matching row is cleared. Principal-bound.",
        )
        .tag(PRODUCERS_TAG)
        .path_param(
            "id",
            "Producer id, with the literal \":reset\" suffix (matchit limitation)",
        )
        .authenticated()
        .no_license_required()
        .json_request::<ingest_dto::ResetProducerRequest>(openapi, "Optional reset scope")
        .request_optional()
        .handler(producers_h::reset_producer)
        .json_response(StatusCode::OK, "Reset applied; audit record emitted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = list_query_params(
        OperationBuilder::get("/event-broker/v1/topics")
            .operation_id("event_broker.list_topics")
            .summary("List topics visible to the caller")
            .description(
                "Returns a paginated, filterable list of topics. To retrieve a single topic \
                 by its GTS identifier use `$filter=id eq '<gts-id>'`.",
            )
            .tag(TOPICS_TAG)
            .authenticated()
            .no_license_required(),
    )
    .handler(topics_h::list_topics)
    .json_response_with_schema::<toolkit_odata::Page<ingest_dto::TopicDto>>(
        openapi,
        StatusCode::OK,
        "Page of topics",
    )
    .standard_errors(openapi)
    .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/topics/segments")
        .operation_id("event_broker.list_topic_segments")
        .summary("Get storage segments for a (topic, partition)")
        .description("Returns the backend's segment manifest for a specific (topic, partition).")
        .tag(TOPICS_TAG)
        .query_param("topic", true, "Topic GTS id")
        .query_param_typed("partition", true, "Partition number", "integer")
        .authenticated()
        .no_license_required()
        .handler(topics_h::list_topic_segments)
        .json_response_with_schema::<ingest_dto::TopicSegmentsResponse>(
            openapi,
            StatusCode::OK,
            "Segment manifest",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = list_query_params(
        OperationBuilder::get("/event-broker/v1/event-types")
            .operation_id("event_broker.list_event_types")
            .summary("List event types visible to the caller")
            .tag(EVENT_TYPES_TAG)
            .authenticated()
            .no_license_required(),
    )
    .handler(event_types_h::list_event_types)
    .json_response_with_schema::<toolkit_odata::Page<ingest_dto::EventTypeDto>>(
        openapi,
        StatusCode::OK,
        "Page of event types",
    )
    .standard_errors(openapi)
    .register(router, openapi);

    router
}

/// `events:stream`/`events:sse` are deliberate static-segment action
/// suffixes (see `register_ingest_routes`'s doc comment) - not a DE0801
/// naming violation.
#[allow(unknown_lints)]
#[allow(de0801_api_endpoint_version)]
pub fn register_delivery_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::get("/event-broker/v1/events:stream")
        .operation_id("event_broker.stream_events")
        .summary("Multipart event consumption (default transport)")
        .description(
            "The single consumption endpoint for multipart/mixed-framed delivery. Long-lived: \
             the response stays open and emits frames as events arrive.",
        )
        .tag(INGEST_API_TAG)
        .query_param("subscription_id", true, "Subscription id")
        .authenticated()
        .no_license_required()
        .handler(delivery_h::stream_events)
        .text_response(
            StatusCode::OK,
            "Long-lived multipart/mixed event stream (Transfer-Encoding chunked)",
            "multipart/mixed",
        )
        .problem_response(
            openapi,
            StatusCode::NOT_ACCEPTABLE,
            "Accept header excludes multipart/mixed",
        )
        .problem_response(openapi, StatusCode::GONE, "SubscriptionTerminated")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/events:sse")
        .operation_id("event_broker.sse_events")
        .summary("Server-Sent Events consumption (opt-in)")
        .description(
            "Browser-native text/event-stream consumption endpoint. Same frame kinds as \
             `/v1/events:stream`.",
        )
        .tag(INGEST_API_TAG)
        .query_param("subscription_id", true, "Subscription id")
        .authenticated()
        .no_license_required()
        .handler(delivery_h::sse_events)
        .sse_json::<delivery_dto::FrameDto>(openapi, "Long-lived text/event-stream of frames")
        .problem_response(openapi, StatusCode::GONE, "SubscriptionTerminated")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/consumer-groups")
        .operation_id("event_broker.create_consumer_group")
        .summary("Register an anonymous consumer group (broker-minted id)")
        .description(
            "Mints `gts.cf.core.events.consumer_group.v1~<uuid>` server-side. The minted \
             identifier is returned in the response body and the Location header.",
        )
        .tag(CONSUMER_GROUPS_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<delivery_dto::CreateConsumerGroupRequest>(
            openapi,
            "Optional client_agent/description",
        )
        .request_optional()
        .handler(consumer_groups_h::create_consumer_group)
        .json_response_with_schema::<delivery_dto::ConsumerGroupDto>(
            openapi,
            StatusCode::CREATED,
            "Created",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = list_query_params(
        OperationBuilder::get("/event-broker/v1/consumer-groups")
            .operation_id("event_broker.list_consumer_groups")
            .summary("List consumer groups visible to the caller")
            .tag(CONSUMER_GROUPS_TAG)
            .authenticated()
            .no_license_required(),
    )
    .handler(consumer_groups_h::list_consumer_groups)
    .json_response_with_schema::<toolkit_odata::Page<delivery_dto::ConsumerGroupDto>>(
        openapi,
        StatusCode::OK,
        "Page of consumer groups",
    )
    .standard_errors(openapi)
    .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/consumer-groups/{id}")
        .operation_id("event_broker.get_consumer_group")
        .summary("Read a registered consumer group")
        .tag(CONSUMER_GROUPS_TAG)
        .path_param("id", "Full GTS consumer-group identifier (URL-encoded)")
        .authenticated()
        .no_license_required()
        .handler(consumer_groups_h::get_consumer_group)
        .json_response_with_schema::<delivery_dto::ConsumerGroupDto>(
            openapi,
            StatusCode::OK,
            "Consumer group record",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/event-broker/v1/consumer-groups/{id}")
        .operation_id("event_broker.delete_consumer_group")
        .summary("Remove a consumer group from the registry")
        .description("Allowed only when there are no active members.")
        .tag(CONSUMER_GROUPS_TAG)
        .path_param("id", "Full GTS consumer-group identifier (URL-encoded)")
        .authenticated()
        .no_license_required()
        .handler(consumer_groups_h::delete_consumer_group)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/subscriptions")
        .operation_id("event_broker.join_subscription")
        .summary("JOIN - register a subscription against a consumer group")
        .description(
            "Creates a subscription with one or more typed-filter interests[] entries. \
             Subject to per-tenant rate cap.",
        )
        .tag(SUBSCRIPTIONS_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<delivery_dto::JoinSubscriptionRequest>(openapi, "JOIN request")
        .handler(subscriptions_h::join_subscription)
        .json_response_with_schema::<delivery_dto::SubscriptionDto>(
            openapi,
            StatusCode::CREATED,
            "Subscription created; assignment computed",
        )
        .standard_errors(openapi)
        .error_429(openapi)
        .register(router, openapi);

    router = list_query_params(
        OperationBuilder::get("/event-broker/v1/subscriptions")
            .operation_id("event_broker.list_subscriptions")
            .summary("List active subscriptions visible to the caller")
            .tag(SUBSCRIPTIONS_TAG)
            .authenticated()
            .no_license_required(),
    )
    .handler(subscriptions_h::list_subscriptions)
    .json_response_with_schema::<toolkit_odata::Page<delivery_dto::SubscriptionDto>>(
        openapi,
        StatusCode::OK,
        "Page of subscriptions (may be empty)",
    )
    .standard_errors(openapi)
    .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/subscriptions/{id}")
        .operation_id("event_broker.get_subscription")
        .summary("Read a single subscription by id")
        .tag(SUBSCRIPTIONS_TAG)
        .path_param("id", "Subscription id")
        .authenticated()
        .no_license_required()
        .handler(subscriptions_h::get_subscription)
        .json_response_with_schema::<delivery_dto::SubscriptionDto>(
            openapi,
            StatusCode::OK,
            "Subscription record",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/event-broker/v1/subscriptions/{id}")
        .operation_id("event_broker.leave_subscription")
        .summary("LEAVE - terminate a subscription")
        .tag(SUBSCRIPTIONS_TAG)
        .path_param("id", "Subscription id")
        .authenticated()
        .no_license_required()
        .handler(subscriptions_h::leave_subscription)
        .no_content_response(
            StatusCode::NO_CONTENT,
            "Subscription removed; rebalance triggered",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // Registered as bare `{id}` - same matchit limitation and mechanism as
    // `/v1/producers/{id}` above. Same literal path string as the GET/DELETE
    // registrations above, so axum merges all three into one route entry
    // rather than conflicting (`axum::routing::path_router::PathRouter::route`
    // merges method routers for identical path strings).
    router = OperationBuilder::post("/event-broker/v1/subscriptions/{id}")
        .operation_id("event_broker.seek_subscription")
        .summary("SEEK - set the group cursor for assigned (topic, partition) pairs")
        .description(
            "Pre-stream-only: the consumer SDK calls it once after JOIN (before opening \
             `:stream`) to declare the starting position for each assigned partition.",
        )
        .tag(SUBSCRIPTIONS_TAG)
        .path_param(
            "id",
            "Subscription id, with the literal \":seek\" suffix (matchit limitation)",
        )
        .authenticated()
        .no_license_required()
        .json_request::<delivery_dto::SeekSubscriptionRequest>(
            openapi,
            "Per-partition seek positions",
        )
        .handler(subscriptions_h::seek_subscription)
        .json_array_response_with_schema::<delivery_dto::ResolvedPositionDto>(
            openapi,
            StatusCode::OK,
            "Cursor seeded/updated; resolved integer offsets per partition",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

pub fn register_dispatcher_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = register_ingest_forwarding_routes(router, openapi);
    router = register_delivery_forwarding_routes(router, openapi);
    register_shared_forwarding_routes(router, openapi)
}

/// A placeholder success response for an opaque proxy route: the real shape
/// is whatever the resolved instance returns, documented by #4346's own
/// registration of the same path.
fn proxied_response(status: axum::http::StatusCode) -> ResponseSpec {
    ResponseSpec::new(
        status.as_u16(),
        "application/json",
        "Proxied verbatim from the resolved instance - see the underlying operation's own \
         documentation for the concrete response shape.",
        None,
    )
}

/// The forwarding note appended via `.description()`, distinct from
/// `summary`, so the declared "what it does" text stays identical to the
/// underlying operation's own registration.
fn forwarded_to(role: &str) -> String {
    format!("Forwarded to the resolved {role} instance.")
}

const INGEST_TAG: &str = "Dispatcher (Ingest)";
const DELIVERY_TAG: &str = "Dispatcher (Delivery)";
const SHARED_TAG: &str = "Dispatcher (Shared)";

/// `events:batch` is a deliberate static-segment action suffix (see
/// `register_ingest_routes`'s doc comment) - not a DE0801 naming violation.
#[allow(unknown_lints)]
#[allow(de0801_api_endpoint_version)]
fn register_ingest_forwarding_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/event-broker/v1/events")
        .operation_id("event_broker.dispatcher.publish_event")
        .summary("Publish single event")
        .description(forwarded_to("ingest"))
        .tag(INGEST_TAG)
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/events:batch")
        .operation_id("event_broker.dispatcher.publish_batch")
        .summary("Publish batch of events")
        .description(forwarded_to("ingest"))
        .tag(INGEST_TAG)
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/producers")
        .operation_id("event_broker.dispatcher.register_producer")
        .summary("Register a producer (mint producer_id)")
        .description(forwarded_to("ingest"))
        .tag(INGEST_TAG)
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/producers/{id}/cursors")
        .operation_id("event_broker.dispatcher.get_producer_cursors")
        .summary("Read per-(topic,partition) last_sequence for desync recovery")
        .description(forwarded_to("ingest"))
        .tag(INGEST_TAG)
        .path_param("id", "Producer id")
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // Registered as bare `{id}` (see module doc comment above - matchit
    // limitation). `{id}` captures "xyz:reset" whole; the real `:reset` wire
    // path survives because `proxy::handle` forwards the untouched `Request`.
    router = OperationBuilder::post("/event-broker/v1/producers/{id}")
        .operation_id("event_broker.dispatcher.reset_producer")
        .summary("Operator-driven chain reset (preserves producer_id)")
        .description(forwarded_to("ingest"))
        .tag(INGEST_TAG)
        .path_param(
            "id",
            "Producer id, with the literal \":reset\" suffix (matchit limitation)",
        )
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router
}

/// `events:stream`/`events:sse` are deliberate static-segment action
/// suffixes (see `register_ingest_routes`'s doc comment) - not a DE0801
/// naming violation.
#[allow(unknown_lints)]
#[allow(de0801_api_endpoint_version)]
fn register_delivery_forwarding_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    router = OperationBuilder::get("/event-broker/v1/events:stream")
        .operation_id("event_broker.dispatcher.stream_events")
        .summary("Multipart event stream (long-lived, one event per part)")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/events:sse")
        .operation_id("event_broker.dispatcher.sse_events")
        .summary("SSE event stream (opt-in, browser-native)")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/consumer-groups")
        .operation_id("event_broker.dispatcher.create_consumer_group")
        .summary("Create anonymous consumer group (broker-minted id)")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/consumer-groups")
        .operation_id("event_broker.dispatcher.list_consumer_groups")
        .summary("List consumer groups")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/consumer-groups/{id}")
        .operation_id("event_broker.dispatcher.get_consumer_group")
        .summary("Read a consumer group")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .path_param("id", "Consumer group id")
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/event-broker/v1/consumer-groups/{id}")
        .operation_id("event_broker.dispatcher.delete_consumer_group")
        .summary("Delete a consumer group (only if no active members)")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .path_param("id", "Consumer group id")
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::NO_CONTENT))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/event-broker/v1/subscriptions")
        .operation_id("event_broker.dispatcher.join_subscription")
        .summary("JOIN - create subscription against a consumer group")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/subscriptions")
        .operation_id("event_broker.dispatcher.list_subscriptions")
        .summary("List active subscriptions")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/subscriptions/{id}")
        .operation_id("event_broker.dispatcher.get_subscription")
        .summary("Read a single subscription")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .path_param("id", "Subscription id")
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/event-broker/v1/subscriptions/{id}")
        .operation_id("event_broker.dispatcher.leave_subscription")
        .summary("LEAVE - terminate a subscription")
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .path_param("id", "Subscription id")
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::NO_CONTENT))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // Registered as bare `{id}` - same matchit limitation and mechanism as
    // `/v1/producers/{id}` above.
    router = OperationBuilder::post("/event-broker/v1/subscriptions/{id}")
        .operation_id("event_broker.dispatcher.seek_subscription")
        .summary(
            "SEEK - set per-partition starting cursor; accepts integer offsets and sentinels \
             including \"at:<timestamp>\"",
        )
        .description(forwarded_to("delivery"))
        .tag(DELIVERY_TAG)
        .path_param(
            "id",
            "Subscription id, with the literal \":seek\" suffix (matchit limitation)",
        )
        .authenticated()
        .no_license_required()
        .handler(router::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router
}

fn register_shared_forwarding_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // Shared routes forward to the ingest service (design.md D2 - an
    // arbitrary but documented pick between two equally-valid targets).
    router = OperationBuilder::get("/event-broker/v1/topics")
        .operation_id("event_broker.dispatcher.list_topics")
        .summary("List topics")
        .description(forwarded_to("ingest"))
        .tag(SHARED_TAG)
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/topics/segments")
        .operation_id("event_broker.dispatcher.list_topic_segments")
        .summary("Get topic segment manifest for a (topic, partition)")
        .description(forwarded_to("ingest"))
        .tag(SHARED_TAG)
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/event-broker/v1/event-types")
        .operation_id("event_broker.dispatcher.list_event_types")
        .summary("List event types")
        .description(forwarded_to("ingest"))
        .tag(SHARED_TAG)
        .authenticated()
        .no_license_required()
        .handler(proxy::handle)
        .response(proxied_response(axum::http::StatusCode::OK))
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router
}

/// Test-only router for `event-broker`'s own ingest/delivery REST handlers
/// (not the dispatcher-forwarding routes above). Manual route registration
/// (no `OperationBuilder`/`OpenApiRegistry`) - mirrors `oagw`'s `test_router`
/// exactly, including wiring the same canonical-error middleware so tests
/// observe realistic `Problem` responses, and injecting a fixed
/// `SecurityContext` via `Extension` since there's no real auth layer in
/// tests.
///
/// Routes are added here incrementally as each handler group lands (task
/// groups 5-9); production registration (`OperationBuilder`, task group 10)
/// is separate and unaffected by this function's shape. Plain `#[cfg(test)]`,
/// unlike `oagw`: this crate has no `test-utils` feature, since no external
/// `tests/` integration binary needs this outside the crate's own unit
/// tests, matching `routes_tests.rs`'s existing convention.
#[cfg(test)]
pub fn test_router(
    state: crate::api::rest::state::HandlerState,
    ctx: toolkit_security::SecurityContext,
) -> Router {
    use crate::api::rest::handlers::delivery::consumer_groups as consumer_groups_h;
    use crate::api::rest::handlers::delivery::streaming as delivery_h;
    use crate::api::rest::handlers::delivery::subscriptions as subscriptions_h;
    use crate::api::rest::handlers::ingest::event_types as event_types_h;
    use crate::api::rest::handlers::ingest::events as ingest_h;
    use crate::api::rest::handlers::ingest::producers as producers_h;
    use crate::api::rest::handlers::ingest::topics as topics_h;
    use axum::routing::{get, post};

    Router::new()
        .route("/event-broker/v1/events", post(ingest_h::publish_event))
        .route(
            "/event-broker/v1/events:batch",
            post(ingest_h::publish_batch),
        )
        .route(
            "/event-broker/v1/producers",
            post(producers_h::register_producer),
        )
        .route(
            "/event-broker/v1/producers/{id}/cursors",
            get(producers_h::get_producer_cursors),
        )
        // Bare `{id}` - matchit can't mix a path param with literal text in
        // one segment; `producers_h::reset_producer` splits the `:reset`
        // suffix out of the captured value itself.
        .route(
            "/event-broker/v1/producers/{id}",
            post(producers_h::reset_producer),
        )
        .route("/event-broker/v1/topics", get(topics_h::list_topics))
        .route(
            "/event-broker/v1/topics/segments",
            get(topics_h::list_topic_segments),
        )
        .route(
            "/event-broker/v1/event-types",
            get(event_types_h::list_event_types),
        )
        .route(
            "/event-broker/v1/consumer-groups",
            post(consumer_groups_h::create_consumer_group)
                .get(consumer_groups_h::list_consumer_groups),
        )
        .route(
            "/event-broker/v1/consumer-groups/{id}",
            get(consumer_groups_h::get_consumer_group)
                .delete(consumer_groups_h::delete_consumer_group),
        )
        .route(
            "/event-broker/v1/subscriptions",
            post(subscriptions_h::join_subscription).get(subscriptions_h::list_subscriptions),
        )
        // `POST` here is the bare-`{id}` SEEK workaround (see
        // `producers_h::reset_producer`'s comment) - matchit conflict-detects
        // by path *shape*, not param name, so this MUST be one `.route()`
        // call with `GET`/`DELETE`/`POST` chained together, not a second
        // `.route()` call with a differently-named param (that panics at
        // router-build time as a duplicate route).
        .route(
            "/event-broker/v1/subscriptions/{id}",
            get(subscriptions_h::get_subscription)
                .delete(subscriptions_h::leave_subscription)
                .post(subscriptions_h::seek_subscription),
        )
        .route(
            "/event-broker/v1/events:stream",
            get(delivery_h::stream_events),
        )
        .route("/event-broker/v1/events:sse", get(delivery_h::sse_events))
        .layer(axum::middleware::from_fn(
            toolkit::api::canonical_error_middleware,
        ))
        .layer(axum::Extension(ctx))
        .layer(axum::Extension(state))
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;
