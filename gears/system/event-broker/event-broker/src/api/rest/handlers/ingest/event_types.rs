//! `GET /v1/event-types` (`DESIGN.md:587`).

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_odata::ast::Value as ODataValue;

use super::dto::EventTypeDto;
use crate::api::rest::pagination::{eval_filter, paginate_by_key};
use crate::api::rest::state::HandlerState;

/// # Errors
/// Returns `400` for an invalid `$filter` expression or pagination cursor.
pub async fn list_event_types(
    Extension(state): Extension<HandlerState>,
    OData(query): OData,
) -> ApiResult<JsonPage<EventTypeDto>> {
    let mut event_types = state.ingest.list_event_types().await;
    if let Some(filter) = query.filter() {
        event_types.retain(|t| {
            eval_filter(filter, &|field| match field {
                "id" => Some(ODataValue::String(t.id.to_string())),
                "topic" => Some(ODataValue::String(t.topic.to_string())),
                _ => None,
            })
        });
    }
    let page = paginate_by_key(event_types, &query, "id", |t| t.id.to_string())?;
    Ok(Json(page.map_items(EventTypeDto::from)))
}
