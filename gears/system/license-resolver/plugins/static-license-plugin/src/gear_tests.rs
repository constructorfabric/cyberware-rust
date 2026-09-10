use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit::config::ConfigProvider;
use toolkit::{ClientHub, Gear, GearCtx};
use uuid::Uuid;

use super::StaticLicensePlugin;

struct StaticConfigProvider {
    root: serde_json::Value,
}

impl ConfigProvider for StaticConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        self.root.get(gear)
    }
}

fn make_ctx(hub: Arc<ClientHub>) -> GearCtx {
    GearCtx::new(
        StaticLicensePlugin::MODULE_NAME,
        Uuid::from_u128(1),
        Arc::new(StaticConfigProvider { root: json!({}) }),
        hub,
        CancellationToken::new(),
    )
}

#[tokio::test]
async fn init_fails_closed_without_types_registry() {
    let hub = Arc::new(ClientHub::new());
    let gear = StaticLicensePlugin::default();

    let err = gear
        .init(&make_ctx(hub))
        .await
        .expect_err("init must fail without a types-registry client");
    assert!(
        format!("{err:#}").contains("not found"),
        "expected the missing-client cause in the chain, got: {err:#}"
    );
}
