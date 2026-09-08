//! Route registration into the platform `api-gateway`.
//!
//! The foundation mounts no operation: routes land with their owning
//! features (DECOMPOSITION 2.1, "API"). The domain service is attached once
//! here, so every later route reads it through `Extension<Arc<Service>>`.

use std::sync::Arc;

use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;

use crate::domain::Service;

/// Path prefix of every QE operation (DESIGN section 3.3, "Versioning").
pub const PATH_PREFIX: &str = "/v1/quota-enforcement";

/// Register the QE routes and attach the service.
///
/// No contract-registration route exists here or anywhere in the gear:
/// owners publish their projections in `types-registry`
/// (`cpt-cf-quota-enforcement-constraint-types-registry-delegation`).
// @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-registry
pub fn register_routes(
    router: Router,
    _openapi: &dyn OpenApiRegistry,
    service: Arc<Service>,
) -> Router {
    router.layer(Extension(service))
}
// @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-registry
