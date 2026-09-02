//! Hello gear definition.

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;

use toolkit::api::OpenApiRegistry;
use toolkit::context::GearCtx;
use toolkit::contracts::RestApiCapability;

use crate::api::rest::routes;

/// Minimal REST gear exposing `GET /hello/v1/ping`.
#[toolkit::gear(
    name = "hello",
    capabilities = [rest]
)]
#[derive(Default)]
pub struct Hello;

#[async_trait]
impl toolkit::Gear for Hello {
    async fn init(&self, _ctx: &GearCtx) -> Result<()> {
        Ok(())
    }
}

impl RestApiCapability for Hello {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> Result<Router> {
        tracing::info!("Registering hello REST routes");
        let router = routes::register_routes(router, openapi);
        Ok(router)
    }
}
