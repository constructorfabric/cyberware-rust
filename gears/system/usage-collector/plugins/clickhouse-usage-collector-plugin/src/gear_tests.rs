use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx};

use super::ClickHouseUsageCollectorPlugin;

/// Minimal [`ConfigProvider`] serving one fixed gear-config JSON.
///
/// `config_expanded_or_default` reads the gear node's `config` sub-object, so
/// the value must be shaped `{ "config": { ... } }`.
struct StaticConfig(serde_json::Value);

impl ConfigProvider for StaticConfig {
    fn get_gear_config(&self, _gear_name: &str) -> Option<&serde_json::Value> {
        Some(&self.0)
    }
}

#[tokio::test]
async fn init_rejects_empty_database_url() {
    let provider = Arc::new(StaticConfig(json!({
        "config": {}
    })));

    let ctx = GearCtx::new(
        "clickhouse-usage-collector-plugin",
        Uuid::from_u128(1),
        provider,
        Arc::new(ClientHub::default()),
        CancellationToken::new(),
    );

    let err = ClickHouseUsageCollectorPlugin
        .init(&ctx)
        .await
        .expect_err("empty database_url must be rejected before any ClickHouse I/O");

    assert!(
        err.to_string().contains("database_url"),
        "expected database_url validation error, got: {err}"
    );
}

#[tokio::test]
async fn init_rejects_zero_lock_ttl() {
    let provider = Arc::new(StaticConfig(json!({
        "config": {
            "database_url": "https://user:pass@ch:8123/usage",
            "lock_ttl_secs": 0
        }
    })));

    let ctx = GearCtx::new(
        "clickhouse-usage-collector-plugin",
        Uuid::from_u128(2),
        provider,
        Arc::new(ClientHub::default()),
        CancellationToken::new(),
    );

    let err = ClickHouseUsageCollectorPlugin
        .init(&ctx)
        .await
        .expect_err("zero lock_ttl_secs must be rejected");

    assert!(
        err.to_string().contains("lock_ttl_secs"),
        "expected lock_ttl_secs validation error, got: {err}"
    );
}

#[tokio::test]
async fn init_rejects_plaintext_http_database_url_without_override() {
    let provider = Arc::new(StaticConfig(json!({
        "config": {
            "database_url": "http://user:pass@ch:8123/usage"
        }
    })));

    let ctx = GearCtx::new(
        "clickhouse-usage-collector-plugin",
        Uuid::from_u128(4),
        provider,
        Arc::new(ClientHub::default()),
        CancellationToken::new(),
    );

    let err = ClickHouseUsageCollectorPlugin
        .init(&ctx)
        .await
        .expect_err("plaintext http:// database_url must be rejected without an explicit override");

    assert!(
        err.to_string().contains("allow_insecure_http"),
        "expected an allow_insecure_http validation error, got: {err}"
    );
}
