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
use crate::test_support::{draft, tenant};

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

    // The Quota store is wired over the gear's late-bound outbox: until the
    // dispatcher binds it, a mutation rolls back as unavailable and nothing
    // is written, while reads answer.
    assert!(!gear.notification_outbox().is_bound());
    let err = plugin
        .create_quota(
            &security_ctx(),
            &toolkit_security::AccessScope::for_tenant(tenant().as_uuid()),
            draft(tenant(), "u1", Some(1)),
            &[],
        )
        .await
        .expect_err("outbox unbound");
    assert!(
        matches!(err, quota_enforcement_sdk::StorageError::Unavailable(_)),
        "{err:?}"
    );
    let page = plugin
        .read_quotas(
            &security_ctx(),
            &toolkit_security::AccessScope::allow_all(),
            quota_enforcement_sdk::QuotaFilter::default(),
            quota_enforcement_sdk::PageRequest::default(),
        )
        .await
        .expect("reads need no outbox");
    assert!(page.items.is_empty());
}

fn security_ctx() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5eed))
        .subject_tenant_id(tenant().as_uuid())
        .build()
        .expect("security context")
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
