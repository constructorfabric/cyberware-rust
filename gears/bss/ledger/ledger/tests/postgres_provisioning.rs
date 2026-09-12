//! Postgres-only integration tests for the transactional seller-provisioning
//! seed (`ProvisioningService::provision`). Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_provisioning -- --ignored`.
//!
//! Covers: (a) the seed is idempotent + additive across repeated calls
//! (created-vs-existing counts + raw row counts hold); (b) a scale exceeding
//! `i64` headroom rolls back the WHOLE transaction (the account seeded earlier
//! in the same call is gone).

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic
)]

use bss_ledger::domain::error::DomainError;
use bss_ledger::infra::provisioning::service::ProvisioningService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{
    AccountClass, FiscalCalendarSpec, Granularity, ODataQuery, ProvisionAccount,
    ProvisionCurrencyScale, ProvisionRequest, Side,
};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
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

fn account(
    class: AccountClass,
    currency: &str,
    revenue_stream: Option<&str>,
    side: Side,
) -> ProvisionAccount {
    ProvisionAccount {
        account_class: class,
        currency: currency.to_owned(),
        revenue_stream: revenue_stream.map(str::to_owned),
        normal_side: side,
        may_go_negative: false,
    }
}

// A non-ISO currency scale within the default headroom (`plausible_max_major`
// omitted -> 10^12, max scale 6: the guard rejects 10^12 * 10^scale > i64). A
// higher-precision currency (e.g. BTC=8) registers a smaller per-currency max.
fn noniso_scale() -> ProvisionCurrencyScale {
    ProvisionCurrencyScale {
        currency: "QQQ".to_owned(),
        minor_units: 4,
        plausible_max_major: None,
        source: "TENANT".to_owned(),
    }
}

fn utc_calendar() -> FiscalCalendarSpec {
    FiscalCalendarSpec {
        timezone: "UTC".to_owned(),
        granularity: Granularity::Month,
        fy_start_month: 1,
        // S5-F3: seed the legal-entity functional currency so the round-trip read
        // (asserted below) is exercised; an unset value would be a single-currency
        // tenant.
        functional_currency: Some("USD".to_owned()),
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn provision_is_idempotent_and_additive() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    // Raw sea-orm connection for the migrator + bss-qualified assertions.
    let db = Database::connect(&url).await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    // The service connection sets search_path=bss (as the gear config does in
    // prod) so its unqualified entity queries resolve into the bss schema.
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);
    let service = ProvisioningService::new(provider.clone());

    let tenant_id = Uuid::new_v4();
    let expected_period = bss_ledger::domain::instant::yyyymm(OffsetDateTime::now_utc());

    // --- Call #1: seed everything fresh. ---
    let req1 = ProvisionRequest {
        tenant_id,
        accounts: vec![
            account(AccountClass::Ar, "USD", None, Side::Debit),
            account(AccountClass::Revenue, "USD", Some("compute"), Side::Credit),
        ],
        currency_scales: vec![noniso_scale()],
        fiscal_calendar: utc_calendar(),
    };
    let out1 = service.provision(req1).await.expect("first provision ok");
    assert_eq!(out1.accounts_created, 2, "first call: 2 new accounts");
    assert_eq!(out1.accounts_existing, 0);
    assert_eq!(out1.scales_created, 1);
    assert_eq!(out1.scales_existing, 0);
    assert!(out1.calendar_created);
    assert!(out1.period_created);
    assert_eq!(out1.period_id, expected_period);
    // (1) provision returns the accounts it created, each with a non-nil id.
    assert_eq!(out1.accounts.len(), 2);
    assert!(
        out1.accounts.iter().all(|a| !a.account_id.is_nil()),
        "first call: created accounts carry real ids"
    );

    // Rows landed.
    assert_eq!(
        count(
            &db,
            format!("SELECT count(*) FROM bss.ledger_tenant_account WHERE tenant_id='{tenant_id}'")
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &db,
            format!(
                "SELECT count(*) FROM bss.ledger_currency_scale_registry \
                 WHERE tenant_id='{tenant_id}' AND currency='QQQ'"
            )
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &db,
            format!(
                "SELECT count(*) FROM bss.ledger_fiscal_calendar \
                 WHERE tenant_id='{tenant_id}' AND legal_entity_id='{tenant_id}'"
            )
        )
        .await,
        1
    );
    let status_row = db
        .query_one_raw(pg(format!(
            "SELECT status FROM bss.ledger_fiscal_period \
             WHERE tenant_id='{tenant_id}' AND legal_entity_id='{tenant_id}' \
               AND period_id='{expected_period}'"
        )))
        .await
        .unwrap()
        .expect("fiscal_period row must exist");
    let status: String = status_row.try_get("", "status").unwrap();
    assert_eq!(status, "OPEN");

    // --- Call #2: identical request -> everything additive no-op. ---
    let req2 = ProvisionRequest {
        tenant_id,
        accounts: vec![
            account(AccountClass::Ar, "USD", None, Side::Debit),
            account(AccountClass::Revenue, "USD", Some("compute"), Side::Credit),
        ],
        currency_scales: vec![noniso_scale()],
        fiscal_calendar: utc_calendar(),
    };
    let out2 = service.provision(req2).await.expect("second provision ok");
    assert_eq!(out2.accounts_created, 0, "re-call: no new accounts");
    assert_eq!(out2.accounts_existing, 2);
    assert_eq!(out2.scales_created, 0);
    assert_eq!(out2.scales_existing, 1);
    assert!(!out2.calendar_created);
    assert!(!out2.period_created);
    // (1) re-call creates nothing → returns no accounts (the full chart, with the
    // SAME ids, is read via list_accounts below).
    assert!(out2.accounts.is_empty(), "re-call creates nothing");
    // list_accounts returns the full chart with the SAME ids provision created.
    let repo = ReferenceRepo::new(provider.clone());
    let listed = repo
        .list_accounts(
            &AccessScope::for_tenant(tenant_id),
            tenant_id,
            &ODataQuery::default(),
        )
        .await
        .expect("list_accounts ok");
    let mut created_ids: Vec<_> = out1.accounts.iter().map(|a| a.account_id).collect();
    let mut listed_ids: Vec<_> = listed.items.iter().map(|a| a.account_id).collect();
    created_ids.sort();
    listed_ids.sort();
    assert_eq!(
        listed_ids, created_ids,
        "list returns the same ids provision created"
    );

    // S5-F3: provisioning seeded the legal-entity functional currency onto the
    // fiscal-calendar row; it reads back via the dedicated scoped lookup.
    assert_eq!(
        repo.functional_currency(&AccessScope::for_tenant(tenant_id), tenant_id)
            .await
            .expect("functional_currency read ok"),
        Some("USD".to_owned()),
        "the seeded functional currency round-trips"
    );

    // Counts unchanged (no duplicates against uq_tenant_account_coa + PKs).
    assert_eq!(
        count(
            &db,
            format!("SELECT count(*) FROM bss.ledger_tenant_account WHERE tenant_id='{tenant_id}'")
        )
        .await,
        2
    );

    // --- Call #3: add one new account; resubmit existing scale + calendar. ---
    let req3 = ProvisionRequest {
        tenant_id,
        accounts: vec![
            account(AccountClass::CashClearing, "USD", None, Side::Debit),
            account(AccountClass::Ar, "USD", None, Side::Debit),
            account(AccountClass::Revenue, "USD", Some("compute"), Side::Credit),
        ],
        currency_scales: vec![noniso_scale()],
        fiscal_calendar: utc_calendar(),
    };
    let out3 = service.provision(req3).await.expect("third provision ok");
    assert_eq!(out3.accounts_created, 1, "third call: 1 new account");
    assert_eq!(out3.accounts_existing, 2);
    assert_eq!(
        count(
            &db,
            format!("SELECT count(*) FROM bss.ledger_tenant_account WHERE tenant_id='{tenant_id}'")
        )
        .await,
        3
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn provision_rolls_back_on_out_of_range_scale() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let db = Database::connect(&url).await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);
    let service = ProvisioningService::new(provider);

    let tenant_id = Uuid::new_v4();

    // One valid account FOLLOWED BY a scale that exceeds i64 headroom: the
    // whole transaction must roll back, so the account is gone too.
    let req = ProvisionRequest {
        tenant_id,
        accounts: vec![account(AccountClass::Ar, "USD", None, Side::Debit)],
        currency_scales: vec![ProvisionCurrencyScale {
            currency: "BIG".to_owned(),
            minor_units: 18,
            plausible_max_major: None,
            source: "TENANT".to_owned(),
        }],
        fiscal_calendar: utc_calendar(),
    };

    let err = service
        .provision(req)
        .await
        .expect_err("out-of-headroom scale must be rejected");
    match err {
        DomainError::ScaleOutOfRange(c) => assert!(c.contains("BIG"), "got {c}"),
        other => panic!("expected ScaleOutOfRange(BIG), got {other:?}"),
    }

    // Rollback: the account seeded earlier in the SAME txn is gone, and no
    // fiscal period was committed.
    assert_eq!(
        count(
            &db,
            format!("SELECT count(*) FROM bss.ledger_tenant_account WHERE tenant_id='{tenant_id}'")
        )
        .await,
        0,
        "the account from the rolled-back txn must not persist"
    );
    assert_eq!(
        count(
            &db,
            format!("SELECT count(*) FROM bss.ledger_fiscal_period WHERE tenant_id='{tenant_id}'")
        )
        .await,
        0
    );
}
