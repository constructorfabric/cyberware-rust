//! Postgres-only integration tests for Group B — the per-tenant FX
//! revaluation-mode repository (`FxRevaluationModeRepo`, VHP-1986). Exercises the
//! effective-dated read/write against a testcontainer Postgres through the REAL
//! `SecureORM` scoping:
//!
//! - no row ⇒ the fail-safe gear default (`ModeA`, revaluation off);
//! - `write_version` mints `0`, then a read in effect returns the written mode;
//! - two versions at different `effective_from` ⇒ the latest one whose
//!   `effective_from <= now` wins; a future row is not yet in effect;
//! - a cross-tenant read is blocked at the SQL level (no leak of another
//!   tenant's `ModeB`).
//!
//! Ignored by default (Docker/testcontainers); run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_fx_revaluation_mode -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic
)]

use bss_ledger::domain::fx::revaluation_mode::RevaluationMode;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::FxRevaluationModeRepo;

use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::Duration;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// Boot a container, run the migration chain, and return a `bss`-search-path
/// `DBProvider` for the repo (the payments-test idiom).
async fn boot() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    DBProvider<DbError>,
) {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);
    (container, provider)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn no_row_defaults_to_mode_a_fail_safe() {
    let (_c, provider) = boot().await;
    let repo = FxRevaluationModeRepo::new(provider);
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let mode = repo
        .read_effective_mode(&scope, tenant, OffsetDateTime::now_utc())
        .await
        .expect("read mode");
    assert_eq!(mode, None, "an un-configured tenant has no row");
    // The caller applies the fleet default; with the global flag off it is ModeA.
    assert_eq!(
        mode.unwrap_or(RevaluationMode::fleet_default(false)),
        RevaluationMode::ModeA,
        "un-configured + fleet-off resolves to the fail-safe ModeA"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn write_then_read_mode_b() {
    let (_c, provider) = boot().await;
    let repo = FxRevaluationModeRepo::new(provider);
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let version = repo
        .write_version(
            &scope,
            tenant,
            RevaluationMode::ModeB,
            OffsetDateTime::now_utc() - Duration::hours(1),
        )
        .await
        .expect("write ModeB");
    assert_eq!(version, 0, "the first version is 0");

    let mode = repo
        .read_effective_mode(&scope, tenant, OffsetDateTime::now_utc())
        .await
        .expect("read mode");
    assert_eq!(mode, Some(RevaluationMode::ModeB));
    assert!(mode.expect("a row exists").revalues(), "ModeB revalues");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn latest_effective_version_wins_and_future_not_yet() {
    let (_c, provider) = boot().await;
    let repo = FxRevaluationModeRepo::new(provider);
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // v0: ModeB, effective 2h ago (superseded).
    assert_eq!(
        repo.write_version(
            &scope,
            tenant,
            RevaluationMode::ModeB,
            OffsetDateTime::now_utc() - Duration::hours(2),
        )
        .await
        .expect("v0"),
        0
    );
    // v1: ModeA, effective 1h ago (the one in effect now).
    assert_eq!(
        repo.write_version(
            &scope,
            tenant,
            RevaluationMode::ModeA,
            OffsetDateTime::now_utc() - Duration::hours(1),
        )
        .await
        .expect("v1"),
        1
    );
    // v2: ModeB, effective TOMORROW (not yet in effect).
    assert_eq!(
        repo.write_version(
            &scope,
            tenant,
            RevaluationMode::ModeB,
            OffsetDateTime::now_utc() + Duration::days(1),
        )
        .await
        .expect("v2"),
        2
    );

    let mode = repo
        .read_effective_mode(&scope, tenant, OffsetDateTime::now_utc())
        .await
        .expect("read mode");
    assert_eq!(
        mode,
        Some(RevaluationMode::ModeA),
        "the latest effective-now version (v1 ModeA) wins; the future v2 ModeB is not yet in effect"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_tenant_read_is_blocked_at_sql_level() {
    let (_c, provider) = boot().await;
    let repo = FxRevaluationModeRepo::new(provider);
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();

    repo.write_version(
        &AccessScope::for_tenant(tenant_a),
        tenant_a,
        RevaluationMode::ModeB,
        OffsetDateTime::now_utc() - Duration::hours(1),
    )
    .await
    .expect("tenant A writes ModeB");

    // Tenant B's scope attempts to read tenant A's mode: SQL-level BOLA yields no
    // rows, so the read falls back to the fail-safe default — never A's ModeB.
    let leaked = repo
        .read_effective_mode(
            &AccessScope::for_tenant(tenant_b),
            tenant_a,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("scoped read");
    assert_eq!(
        leaked, None,
        "a cross-tenant read yields no row — no leak of tenant A's ModeB"
    );
}
