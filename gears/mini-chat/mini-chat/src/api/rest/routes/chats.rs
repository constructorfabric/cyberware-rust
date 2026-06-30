use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, OperationBuilderODataExt, ThrottlingSpec};

use super::AiChatLicense;
use crate::api::rest::{dto, handlers};
use crate::infra::db::odata_mapper::ChatCursorField;

const API_TAG: &str = "Mini Chat Chats";

pub(super) fn register_chat_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    prefix: &str,
) -> Router {
    // POST {prefix}/v1/chats
    router = OperationBuilder::post(format!("{prefix}/v1/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(API_TAG)
        .authenticated()
        // Zone-based throttling: 10 rps + 100 concurrent in-flight per client IP.
        // Limits live in the gateway config zones referenced by name below; this
        // binding is what the gateway evaluates at request time. IP-keyed, so it
        // runs before auth (require_security_context = false).
        //
        // dry_run: the gateway observes limits and logs would-be rejections but
        // never returns 429, so functional tests and clients are not throttled.
        // Operators can enforce for real by flipping this to false.
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            identity_key_func: None,
            require_security_context: false,
            dry_run: true,
        })
        .require_license_features([&AiChatLicense])
        .json_request::<dto::CreateChatReq>(openapi, "Chat creation data")
        .handler(handlers::chats::create_chat)
        .json_response_with_schema::<dto::ChatDetailDto>(
            openapi,
            http::StatusCode::CREATED,
            "Created chat",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET {prefix}/v1/chats
    router = OperationBuilder::get(format!("{prefix}/v1/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(API_TAG)
        .authenticated()
        .require_license_features([&AiChatLicense])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::chats::list_chats)
        .json_response_with_schema::<toolkit_odata::Page<dto::ChatDetailDto>>(
            openapi,
            http::StatusCode::OK,
            "Paginated list of chats",
        )
        .with_odata_filter::<ChatCursorField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET {prefix}/v1/chats/{id}
    router = OperationBuilder::get(format!("{prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(API_TAG)
        .authenticated()
        .require_license_features([&AiChatLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::chats::get_chat)
        .json_response_with_schema::<dto::ChatDetailDto>(
            openapi,
            http::StatusCode::OK,
            "Chat found",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // PATCH {prefix}/v1/chats/{id}
    router = OperationBuilder::patch(format!("{prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(API_TAG)
        .authenticated()
        .require_license_features([&AiChatLicense])
        .path_param("id", "Chat UUID")
        .json_request::<dto::UpdateChatReq>(openapi, "Chat update data")
        .handler(handlers::chats::update_chat)
        .json_response_with_schema::<dto::ChatDetailDto>(
            openapi,
            http::StatusCode::OK,
            "Updated chat",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE {prefix}/v1/chats/{id}
    router = OperationBuilder::delete(format!("{prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(API_TAG)
        .authenticated()
        .require_license_features([&AiChatLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::chats::delete_chat)
        .json_response(http::StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}
