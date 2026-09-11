//! Delivery-routing handler. Cache-based lookup, new-group CAS placement,
//! and SD-gated failover cache invalidation (`DESIGN.md:1216-1391`) are out
//! of scope - see `eb-dispatcher-routing` design.md D1; #4438 owns any real
//! per-instance routing algorithm. `handle` resolves a delivery instance via
//! `DirectoryService` and forwards to it.

use std::sync::Arc;

use axum::Extension;
use axum::extract::Request;
use axum::response::Response;
use toolkit::api::canonical_prelude::CanonicalError;

use super::forward::{DispatcherState, Role, forward};

/// Correct only when exactly one delivery instance is registered
/// (design.md D11): random selection (`forward()`'s uniform pick, D8)
/// combined with no group-ownership stickiness (deferred to #4438) means a
/// consumer's JOIN and its subsequent long-poll could land on different
/// delivery instances if more than one is registered, breaking the
/// session. Not runtime-enforced - this is a documentation-only boundary
/// until #4438 lands real ownership-aware routing.
#[derive(Debug, Default)]
pub struct DeliveryRouter;

impl DeliveryRouter {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Axum handler for every Delivery-classified route (`DESIGN.md:1354-1392`),
/// registered by `register_dispatcher_routes`. `state` is attached once via
/// `.layer(Extension(...))` in `module.rs`.
///
/// # Errors
/// Returns `CanonicalError::ServiceUnavailable` when no delivery instance is
/// registered, or when the resolved instance is unreachable (design.md D10).
pub async fn handle(
    Extension(state): Extension<Arc<DispatcherState>>,
    req: Request,
) -> Result<Response, CanonicalError> {
    forward(Role::Delivery, &state, req).await
}
