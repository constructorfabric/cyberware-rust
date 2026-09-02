//! Bootstrap semantics against an in-memory `SQLite` database.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use quota_enforcement_sdk::{BootstrapBundle, CONTRACT_MAJOR, ConfigDefaults, StorageError};
use sea_orm::{ActiveValue, EntityTrait};
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_db::{ConnectOpts, Db, connect_db};
use toolkit_security::AccessScope;

use super::StoragePlugin;
use crate::domain::ports::{FoundationStore, SeedReport, StoreError};
use crate::infra::storage::entity::{
    DEFAULT_KEY, contention_timeout_config, idempotency_retention_config, lease_capacity_config,
    schema_meta,
};
use crate::infra::storage::repo::config_repo;
use crate::infra::storage::{Migrator, SqlFoundationStore};

async fn test_db() -> Db {
    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..ConnectOpts::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("apply storage migrations");
    db
}

async fn count<E: EntityTrait + toolkit_db::secure::ScopableEntity>(db: &Db) -> usize {
    let conn = db.conn().expect("connection");
    E::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read rows")
        .len()
}

fn sql_plugin(db: &Db) -> StoragePlugin {
    StoragePlugin::new(Arc::new(SqlFoundationStore::new(db.clone())))
}

/// Store double for the domain-only paths: no database involved.
struct FakeStore {
    major: Option<i32>,
    fail: Option<StoreError>,
}

#[async_trait]
impl FoundationStore for FakeStore {
    async fn read_installed_major(&self) -> Result<Option<i32>, StoreError> {
        match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(self.major),
        }
    }

    async fn record_major(&self, _major: i32) -> Result<bool, StoreError> {
        Ok(true)
    }

    async fn seed_defaults(&self, _defaults: &ConfigDefaults) -> Result<SeedReport, StoreError> {
        match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(SeedReport {
                inserted: 3,
                present: 0,
            }),
        }
    }
}

#[tokio::test]
async fn a_store_outage_is_reported_as_unavailable_with_the_operation_name() {
    let plugin = StoragePlugin::new(Arc::new(FakeStore {
        major: None,
        fail: Some(StoreError::Unavailable {
            operation: "read schema major",
        }),
    }));
    let err = plugin
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect_err("store down");
    assert_eq!(
        err,
        StorageError::Unavailable("database call failed during read schema major".to_owned())
    );
}

#[tokio::test]
async fn a_fresh_store_records_the_major_and_reports_the_seed() {
    let plugin = StoragePlugin::new(Arc::new(FakeStore {
        major: None,
        fail: None,
    }));
    let report = plugin
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect("bootstrap");
    assert_eq!(
        report,
        SeedReport {
            inserted: 3,
            present: 0
        }
    );
}

#[tokio::test]
async fn first_bootstrap_records_the_major_and_seeds_three_default_rows() {
    let db = test_db().await;
    let plugin = sql_plugin(&db);
    assert_eq!(plugin.installed_major().await.expect("read"), None);

    let report = plugin
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect("fresh bootstrap");
    assert_eq!(report.inserted, 3);
    assert_eq!(report.present, 0);

    let major = i32::try_from(CONTRACT_MAJOR).expect("fits");
    assert_eq!(plugin.installed_major().await.expect("read"), Some(major));
    assert_eq!(count::<schema_meta::Entity>(&db).await, 1);

    let conn = db.conn().expect("connection");
    let defaults = ConfigDefaults::default();
    assert_eq!(
        config_repo::read_default_contention_timeout(&conn)
            .await
            .expect("read"),
        Some(i64::try_from(defaults.contention_timeout_ms).expect("fits"))
    );
    assert_eq!(
        config_repo::read_default_lease_capacity(&conn)
            .await
            .expect("read"),
        Some(i32::try_from(defaults.max_active_leases).expect("fits"))
    );
    assert_eq!(
        config_repo::read_default_idempotency_retention(&conn)
            .await
            .expect("read"),
        Some(i64::try_from(defaults.idempotency_retention_secs).expect("fits"))
    );
}

#[tokio::test]
async fn repeated_bootstrap_is_idempotent_and_keeps_existing_rows() {
    let db = test_db().await;
    let plugin = sql_plugin(&db);
    plugin
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect("first");
    let before = {
        let conn = db.conn().expect("connection");
        lease_capacity_config::Entity::find_by_id((DEFAULT_KEY.to_owned(), DEFAULT_KEY.to_owned()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .expect("read")
            .expect("seeded")
    };

    let mut changed = BootstrapBundle::foundation();
    changed.config_defaults.max_active_leases = 7;
    let report = plugin.bootstrap(&changed).await.expect("second");
    assert_eq!(report.inserted, 0);
    assert_eq!(report.present, 3);

    assert_eq!(
        count::<schema_meta::Entity>(&db).await,
        1,
        "no duplicate schema row"
    );
    assert_eq!(count::<contention_timeout_config::Entity>(&db).await, 1);
    assert_eq!(count::<lease_capacity_config::Entity>(&db).await, 1);
    assert_eq!(count::<idempotency_retention_config::Entity>(&db).await, 1);

    let conn = db.conn().expect("connection");
    let after =
        lease_capacity_config::Entity::find_by_id((DEFAULT_KEY.to_owned(), DEFAULT_KEY.to_owned()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .expect("read")
            .expect("still seeded");
    assert_eq!(after, before, "an existing default row is never rewritten");
    assert_eq!(after.max_active_leases, 1000);
}

#[tokio::test]
async fn schema_major_mismatch_fails_closed_before_seeding() {
    let db = test_db().await;
    {
        let conn = db.conn().expect("connection");
        let foreign = schema_meta::ActiveModel {
            contract_major: ActiveValue::Set(i32::try_from(CONTRACT_MAJOR).expect("fits") + 1),
            applied_at: ActiveValue::Set(OffsetDateTime::now_utc()),
        };
        secure_insert::<schema_meta::Entity>(foreign, &AccessScope::allow_all(), &conn)
            .await
            .expect("pre-existing newer schema");
    }
    let plugin = sql_plugin(&db);
    let err = plugin
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect_err("mismatch must fail");
    assert_eq!(
        err,
        StorageError::SchemaVersionMismatch {
            installed: CONTRACT_MAJOR + 1,
            expected: CONTRACT_MAJOR,
        }
    );
    assert_eq!(
        count::<contention_timeout_config::Entity>(&db).await,
        0,
        "nothing seeded"
    );
    assert_eq!(count::<lease_capacity_config::Entity>(&db).await, 0);
    assert_eq!(count::<idempotency_retention_config::Entity>(&db).await, 0);
}

#[tokio::test]
async fn custom_defaults_are_seeded_with_their_values() {
    let db = test_db().await;
    let plugin = sql_plugin(&db);
    let mut bundle = BootstrapBundle::foundation();
    bundle.config_defaults = ConfigDefaults {
        contention_timeout_ms: 250,
        max_active_leases: 42,
        idempotency_retention_secs: 3_600,
    };
    plugin.bootstrap(&bundle).await.expect("bootstrap");
    let conn = db.conn().expect("connection");
    assert_eq!(
        config_repo::read_default_contention_timeout(&conn)
            .await
            .expect("read"),
        Some(250)
    );
    assert_eq!(
        config_repo::read_default_lease_capacity(&conn)
            .await
            .expect("read"),
        Some(42)
    );
    assert_eq!(
        config_repo::read_default_idempotency_retention(&conn)
            .await
            .expect("read"),
        Some(3_600)
    );
}

#[tokio::test]
async fn a_default_that_does_not_fit_its_column_is_an_internal_error() {
    let db = test_db().await;
    let plugin = sql_plugin(&db);
    let mut bundle = BootstrapBundle::foundation();
    bundle.config_defaults.contention_timeout_ms = u64::MAX;
    let err = plugin.bootstrap(&bundle).await.expect_err("out of range");
    assert!(matches!(err, StorageError::Internal(_)), "{err:?}");
    assert!(err.to_string().contains("contention_timeout_ms"), "{err}");
    assert_eq!(count::<contention_timeout_config::Entity>(&db).await, 0);
}
