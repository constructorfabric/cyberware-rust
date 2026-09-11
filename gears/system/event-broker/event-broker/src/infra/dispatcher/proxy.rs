//! Ingest-routing proxy handler. Topic-pattern matching, hetero/sharded
//! modes, and specificity tie-break (`DESIGN.md:1216-1391`) are out of scope
//! (see `eb-dispatcher-routing` design.md D1; #4438 owns any real
//! per-instance routing algorithm). `handle` resolves an ingest instance via
//! `DirectoryService` and forwards to it.

use std::sync::Arc;

use axum::Extension;
use axum::extract::Request;
use axum::response::Response;
use toolkit::api::canonical_prelude::CanonicalError;

use super::forward::{DispatcherState, Role, forward};

#[derive(Debug, Default)]
pub struct IngestProxy;

impl IngestProxy {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Axum handler for every Ingest- and Shared-classified route
/// (`DESIGN.md:1354-1392`), registered by `register_dispatcher_routes`.
/// `state` is attached once via `.layer(Extension(...))` in `module.rs`.
///
/// # Errors
/// Returns `CanonicalError::ServiceUnavailable` when no ingest instance is
/// registered, or when the resolved instance is unreachable (design.md D10).
pub async fn handle(
    Extension(state): Extension<Arc<DispatcherState>>,
    req: Request,
) -> Result<Response, CanonicalError> {
    forward(Role::Ingest, &state, req).await
}
