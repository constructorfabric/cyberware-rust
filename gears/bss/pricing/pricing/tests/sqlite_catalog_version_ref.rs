//! `list_for_pending_ref` — the handle-wide read the status route uses.
//!
//! `find` is one subject. A publish unit records one handle against one, two
//! or three subjects, and a caller holding the receipt has only the handle.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::read_model::SubjectRef;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{PendingVersionRow, catalog_version_ref_repo};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x1111_1111);
const OTHER: Uuid = Uuid::from_u128(0x2222_2222);
const PLAN: Uuid = Uuid::from_u128(0xAAAA_0001);
const OVERLAY: Uuid = Uuid::from_u128(0xAAAA_0002);
const HANDLE: &str = "dev-local-v7";

fn at() -> OffsetDateTime {
    utc_ymd_hms(2099, 1, 1, 10, 0, 0)
}

async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("migrate");
    DBProvider::<DbError>::new(db)
}

async fn record(
    conn: &impl toolkit_db::secure::DBRunner,
    tenant: Uuid,
    handle: &str,
    subject: SubjectRef,
) {
    catalog_version_ref_repo::record_pending(
        conn,
        &AccessScope::allow_all(),
        PendingVersionRow::for_subject(
            tenant,
            handle.to_owned(),
            &subject,
            Some(0),
            Some(LifecycleState::Published),
            at(),
        ),
    )
    .await
    .expect("record");
}

#[tokio::test]
async fn list_for_pending_ref_returns_every_subject_of_the_handle() {
    let db = provider().await;
    let conn = db.conn().expect("conn");
    record(&conn, TENANT, HANDLE, SubjectRef::Plan(PLAN)).await;
    record(&conn, TENANT, HANDLE, SubjectRef::PriceOverlay(OVERLAY)).await;
    record(&conn, TENANT, "other-handle", SubjectRef::Plan(PLAN)).await;

    let rows = catalog_version_ref_repo::list_for_pending_ref(
        &conn,
        &AccessScope::allow_all(),
        TENANT,
        HANDLE,
    )
    .await
    .expect("the read succeeds");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.pending_ref == HANDLE));
    let kinds: Vec<_> = rows.iter().map(|row| row.subject_kind.as_str()).collect();
    assert_eq!(kinds, ["plan", "price_overlay"]);
}

#[tokio::test]
async fn list_for_pending_ref_is_empty_when_the_handle_does_not_exist() {
    let db = provider().await;
    let conn = db.conn().expect("conn");
    let rows = catalog_version_ref_repo::list_for_pending_ref(
        &conn,
        &AccessScope::allow_all(),
        TENANT,
        HANDLE,
    )
    .await
    .expect("the read succeeds");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn list_for_pending_ref_is_empty_under_another_tenants_scope() {
    let db = provider().await;
    let conn = db.conn().expect("conn");
    record(&conn, TENANT, HANDLE, SubjectRef::Plan(PLAN)).await;

    let rows = catalog_version_ref_repo::list_for_pending_ref(
        &conn,
        &AccessScope::for_tenant(OTHER),
        OTHER,
        HANDLE,
    )
    .await
    .expect("the read succeeds");
    assert!(rows.is_empty(), "a foreign tenant's handle must not leak");
}
