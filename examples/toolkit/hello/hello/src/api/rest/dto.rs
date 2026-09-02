//! REST DTOs for the hello gear.

/// Response for `GET /hello/v1/ping`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PingResponse {
    /// Always `"pong"`.
    pub message: String,
    /// Identifies the serving process (proves edge -> `OoP` pod proxying).
    pub served_by: String,
}
