//! Postgres-only integration test for `PeriodOpenJob` — fiscal-period-open
//! automation. Boots a container, migrates, seeds a `fiscal_calendar` row via
//! raw SQL, then runs the job and asserts it created the current + next
//! `YYYYMM` periods (`status='OPEN'`) and is idempotent on a re-run.
//! Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_period_open -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic
)]

use bss_ledger::infra::jobs::period_open::PeriodOpenJob;
use bss_ledger::infra::storage::migrations::Migrator;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Run `SELECT count(*) ...` and return the `i64` count (the reference-test
/// extraction idiom: `row.try_get::<i64>("", "count")`).
async fn count(db: &DatabaseConnection, sql: impl Into<String>) -> i64 {
    let row = db
        .query_one_raw(pg(sql))
        .await
        .unwrap()
        .expect("count query must return a row");
    row.try_get::<i64>("", "count").unwrap()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn period_open_creates_current_and_next_and_is_idempotent() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    // Raw sea-orm connection for the migrator + bss-qualified setup/assertions.
    let db = Database::connect(&url).await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    // The job connection sets search_path=bss (as the gear config does in prod)
    // so its unqualified entity queries resolve into the bss schema.
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let legal_entity = Uuid::now_v7();
    let cur = bss_ledger::domain::instant::yyyymm(OffsetDateTime::now_utc());
    let next = {
        // Local YYYYMM +1-month (mirrors `domain::period::next_period_id`) so
        // the assertion is independent of the job's own logic.
        let year: i32 = cur[0..4].parse().unwrap();
        let month: u32 = cur[4..6].parse().unwrap();
        if month == 12 {
            format!("{:04}01", year + 1)
        } else {
            format!("{year:04}{:02}", month + 1)
        }
    };

    // Seed a MONTH calendar for (tenant, le) via raw, bss-qualified SQL.
    db.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_calendar (tenant_id, legal_entity_id, fiscal_tz, granularity, fy_start_month)
         VALUES ('{tenant}','{legal_entity}','UTC','MONTH',1)"
    )))
    .await
    .unwrap();

    // --- Run #1: fresh -> current + next created. ---
    let report = PeriodOpenJob::new(provider.clone())
        .run()
        .await
        .expect("period-open run must succeed");
    assert_eq!(report.periods_created, 2, "current + next period created");

    // Both periods exist, OPEN, for this (tenant, le).
    let open_count = count(
        &db,
        format!(
            "SELECT count(*) FROM bss.ledger_fiscal_period \
             WHERE tenant_id='{tenant}' AND legal_entity_id='{legal_entity}' \
               AND period_id IN ('{cur}','{next}') AND status='OPEN'"
        ),
    )
    .await;
    assert_eq!(open_count, 2, "current + next periods are OPEN");

    let total_before = count(
        &db,
        format!(
            "SELECT count(*) FROM bss.ledger_fiscal_period \
             WHERE tenant_id='{tenant}' AND legal_entity_id='{legal_entity}'"
        ),
    )
    .await;
    assert_eq!(total_before, 2, "exactly two period rows");

    // --- Run #2: idempotent -> nothing new. ---
    let report2 = PeriodOpenJob::new(provider)
        .run()
        .await
        .expect("second period-open run must succeed");
    assert_eq!(report2.periods_created, 0, "re-run creates no periods");

    let total_after = count(
        &db,
        format!(
            "SELECT count(*) FROM bss.ledger_fiscal_period \
             WHERE tenant_id='{tenant}' AND legal_entity_id='{legal_entity}'"
        ),
    )
    .await;
    assert_eq!(
        total_after, 2,
        "row count unchanged after idempotent re-run"
    );
}
