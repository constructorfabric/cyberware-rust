use std::sync::Arc;

use license_resolver_sdk::LicenseResolverClient;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit::config::ConfigProvider;
use toolkit::{ClientHub, Gear, GearCtx};
use uuid::Uuid;

use super::LicenseResolver;

struct StaticConfigProvider {
    root: serde_json::Value,
}

impl ConfigProvider for StaticConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        self.root.get(gear)
    }
}

fn make_ctx(hub: Arc<ClientHub>) -> GearCtx {
    let cfg = json!({ "license-resolver": { "vendor": "acme" } });
    GearCtx::new(
        LicenseResolver::MODULE_NAME,
        Uuid::from_u128(1),
        Arc::new(StaticConfigProvider { root: cfg }),
        hub,
        CancellationToken::new(),
    )
}

#[tokio::test]
async fn init_registers_the_resolver_client() {
    let hub = Arc::new(ClientHub::new());
    let gear = LicenseResolver::default();

    gear.init(&make_ctx(hub.clone()))
        .await
        .expect("init must succeed");

    hub.get::<dyn LicenseResolverClient>()
        .expect("resolver client must be registered in the hub");
}

#[tokio::test]
async fn a_second_init_fails_on_the_already_initialized_guard() {
    let hub = Arc::new(ClientHub::new());
    let ctx = make_ctx(hub);
    let gear = LicenseResolver::default();

    gear.init(&ctx).await.expect("first init must succeed");
    let err = gear.init(&ctx).await.expect_err("second init must fail");
    assert!(
        err.to_string().contains("already initialized"),
        "got: {err}"
    );
}
