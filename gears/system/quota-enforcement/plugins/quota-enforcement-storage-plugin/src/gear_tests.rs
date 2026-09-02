#![allow(clippy::expect_used)]

use std::sync::Arc;

use quota_enforcement_sdk::BootstrapBundle;
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit::config::ConfigProvider;
use toolkit::{ClientHub, Gear, GearCtx};
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use uuid::Uuid;

use super::StoragePluginGear;
use crate::infra::storage::Migrator;

struct StaticConfigProvider {
    root: serde_json::Value,
}

impl ConfigProvider for StaticConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        self.root.get(gear)
    }
}

async fn make_ctx(vendor: &str, with_db: bool) -> GearCtx {
    let cfg = json!({ "quota-enforcement-storage-plugin": { "config": { "vendor": vendor } } });
    let ctx = GearCtx::new(
        StoragePluginGear::MODULE_NAME,
        Uuid::from_u128(1),
        Arc::new(StaticConfigProvider { root: cfg }),
        Arc::new(ClientHub::new()),
        CancellationToken::new(),
    );
    if !with_db {
        return ctx;
    }
    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..ConnectOpts::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .expect("in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("migrations");
    ctx.with_db(DBProvider::new(db))
}

#[tokio::test]
async fn init_binds_the_plugin_to_the_database_and_bootstrap_works_through_it() {
    let gear = StoragePluginGear::default();
    assert!(gear.plugin().is_none());
    gear.init(&make_ctx("acme", true).await)
        .await
        .expect("init succeeds");
    let plugin = gear.plugin().expect("plugin bound");
    let report = plugin
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect("bootstrap through the bound plugin");
    assert_eq!(report.inserted, 3);
}

#[tokio::test]
async fn init_fails_without_a_database_binding() {
    let gear = StoragePluginGear::default();
    let err = gear
        .init(&make_ctx("acme", false).await)
        .await
        .expect_err("db capability requires a binding");
    assert!(err.to_string().contains("Database"), "{err}");
    assert!(gear.plugin().is_none());
}

#[tokio::test]
async fn init_fails_on_a_blank_vendor_and_on_a_second_call() {
    let gear = StoragePluginGear::default();
    let err = gear
        .init(&make_ctx("  ", true).await)
        .await
        .expect_err("blank vendor rejected");
    assert!(err.to_string().contains("vendor"), "{err}");

    let ctx = make_ctx("acme", true).await;
    gear.init(&ctx).await.expect("first init");
    let err = gear.init(&ctx).await.expect_err("second init");
    assert!(err.to_string().contains("already initialized"), "{err}");
}
