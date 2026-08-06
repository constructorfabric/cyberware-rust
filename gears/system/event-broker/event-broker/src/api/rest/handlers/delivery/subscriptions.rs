//! JOIN, list, read, leave, seek (`DESIGN.md:591`,
//! `docs/schemas/gts.cf.core.events.subscription.v1~.schema.json`).

use axum::Extension;
use axum::extract::Path;
use axum::http::Uri;
use toolkit::api::canonical_prelude::*;
use toolkit_gts::GtsInstanceId;
use toolkit_odata::ast::Value as ODataValue;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    JoinSubscriptionRequest, ResolvedPositionDto, SeekSubscriptionRequest, SubscriptionDto,
};
use crate::api::rest::handlers::action_suffix::parse_action_suffixed_id;
use crate::api::rest::pagination::{eval_filter, paginate_by_key};
use crate::api::rest::state::HandlerState;
use crate::domain::delivery::{JoinRequest, SeekTarget};
use crate::domain::error::DomainError;
use crate::domain::model::Interest;

const DEFAULT_SESSION_TIMEOUT_SECS: u64 = 30;

/// Parses `session_timeout` (`docs/schemas/gts.cf.core.events.subscription.v1~.schema.json`'s
/// `"format": "duration"`, i.e. ISO 8601) via
/// `toolkit_utils::iso8601_duration::Iso8601Duration`. `None` (field absent)
/// defaults to `PT30S` - documented, correct behavior. A value that is
/// present but fails to parse, or parses to a zero duration, is rejected
/// with `400 Validation` instead of silently substituting the default (the
/// previous hand-rolled parser here only understood `PT<n>S`/`PT<n>M` and
/// silently defaulted on anything else - `PT1H`, `P1D`, combined
/// `PT1H30M`, or outright garbage - and had a distinct overflow bug where a
/// sufficiently large minutes value silently produced a *1-second* timeout
/// instead of the 30s default).
fn parse_session_timeout(raw: Option<&str>) -> Result<std::time::Duration, DomainError> {
    let Some(raw) = raw else {
        return Ok(std::time::Duration::from_secs(DEFAULT_SESSION_TIMEOUT_SECS));
    };
    let duration = raw
        .parse::<toolkit_utils::iso8601_duration::Iso8601Duration>()
        .map_err(|err| DomainError::Validation {
            code: "InvalidSessionTimeout",
            message: format!("'{raw}' is not a valid ISO 8601 duration: {err}"),
        })?
        .as_duration();
    if duration.is_zero() {
        return Err(DomainError::Validation {
            code: "InvalidSessionTimeout",
            message: format!("session_timeout must be a positive duration, got '{raw}'"),
        });
    }
    Ok(duration)
}

/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `DeliveryService::join` produces (consumer group/topic not found, bad
/// type pattern).
pub async fn join_subscription(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    uri: Uri,
    Json(req): Json<JoinSubscriptionRequest>,
) -> ApiResult<impl IntoResponse> {
    event_broker_sdk::validate::client_agent(&req.client_agent).map_err(DomainError::from)?;
    let session_timeout = parse_session_timeout(req.session_timeout.as_deref())?;
    let consumer_group =
        GtsInstanceId::try_new(&req.consumer_group).map_err(|err| DomainError::Validation {
            code: "InvalidBody",
            message: format!(
                "'{}' is not a valid GTS instance id: {err}",
                req.consumer_group
            ),
        })?;
    let request = JoinRequest {
        consumer_group,
        client_agent: req.client_agent,
        interests: req
            .interests
            .into_iter()
            .map(Interest::try_from)
            .collect::<Result<Vec<_>, _>>()?,
        session_timeout,
    };
    let subscription = state.delivery.join(&ctx, request).await?;
    let id = subscription.id.to_string();
    Ok(created_json(SubscriptionDto::from(subscription), &uri, &id))
}

/// # Errors
/// Returns `400` for an invalid `$filter` expression or pagination cursor.
pub async fn list_subscriptions(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    OData(query): OData,
) -> ApiResult<JsonPage<SubscriptionDto>> {
    let mut subscriptions = state.delivery.list_subscriptions(&ctx).await?;
    if let Some(filter) = query.filter() {
        subscriptions.retain(|s| {
            eval_filter(filter, &|field| match field {
                "id" => Some(ODataValue::String(s.id.to_string())),
                "consumer_group" => Some(ODataValue::String(s.consumer_group.to_string())),
                _ => None,
            })
        });
    }
    let page = paginate_by_key(subscriptions, &query, "id", |s| s.id.to_string())?;
    Ok(Json(page.map_items(SubscriptionDto::from)))
}

/// # Errors
/// Returns the mapped `CanonicalError` for `DomainError::NotFound` if the
/// subscription doesn't exist.
pub async fn get_subscription(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<JsonBody<SubscriptionDto>> {
    let subscription = state.delivery.get_subscription(&ctx, id).await?;
    Ok(Json(subscription.into()))
}

/// # Errors
/// Returns the mapped `CanonicalError` for `DomainError::NotFound` if the
/// subscription doesn't exist.
pub async fn leave_subscription(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.delivery.leave(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The `:seek` suffix can't be a normal axum route template - a bare `{id}`
/// is registered instead, split via `action_suffix::parse_action_suffixed_id`
/// (shared with `producers::reset_producer`).
///
/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `DeliveryService::seek` produces, or a `400` if the path segment isn't
/// `<uuid>:seek`, the body isn't valid JSON, or a `value` sentinel is
/// malformed.
pub async fn seek_subscription(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Path(raw_id): Path<String>,
    Json(req): Json<SeekSubscriptionRequest>,
) -> ApiResult<JsonBody<Vec<ResolvedPositionDto>>> {
    let subscription_id = parse_action_suffixed_id(&raw_id, "seek", "subscription")?;

    let mut targets = Vec::with_capacity(req.partition_positions.len());
    for pos in req.partition_positions {
        let topic = GtsInstanceId::try_new(&pos.topic).map_err(|err| DomainError::Validation {
            code: "InvalidBody",
            message: format!("'{}' is not a valid GTS instance id: {err}", pos.topic),
        })?;
        targets.push(SeekTarget {
            topic,
            partition: pos.partition,
            value: pos.value.into_domain()?,
        });
    }

    let resolved = state.delivery.seek(&ctx, subscription_id, targets).await?;
    Ok(Json(
        resolved
            .into_iter()
            .map(|p| ResolvedPositionDto {
                topic: p.topic.into_string(),
                partition: p.partition,
                value: p.offset,
            })
            .collect(),
    ))
}
