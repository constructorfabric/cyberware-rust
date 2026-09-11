//! REST handlers for the hello gear.

use toolkit::api::canonical_prelude::*;

use super::dto::PingResponse;

/// Handler for `GET /hello/v1/ping`.
///
/// Anonymous (no auth). Reports the serving process id so a caller can confirm
/// the request reached this `OoP` pod (rather than being served in-process).
pub async fn handle_ping() -> ApiResult<Json<PingResponse>> {
    Ok(Json(PingResponse {
        message: "pong".to_owned(),
        served_by: format!("hello-oop (pid {})", std::process::id()),
    }))
}
