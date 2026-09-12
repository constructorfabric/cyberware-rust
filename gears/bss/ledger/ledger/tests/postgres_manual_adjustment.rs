//! Postgres-only integration: the Slice-3 Phase-3 `ManualAdjustmentHandler`
//! (Group 4), driven through the REAL foundation engine (`PostingService` + the
//! in-txn `ManualAdjustmentPostSidecar`). Asserts the design §4.6 durable effects:
//!
//! - a governed `RoundingCorrection` posts its balanced legs (DR/CR over the
//!   parking / clearing classes) through the engine — `PostingRef.replayed == false`
//!   on the fresh post — and a re-post of the SAME `(tenant, MANUAL_ADJUSTMENT,
//!   adjustment_id)` is an idempotent replay (`replayed == true`, no second entry);
//! - a leg whose class is OUTSIDE the action's allow-list is rejected
//!   (`ManualAdjustmentNotAllowed`) with no books effect;
//! - a bare `CONTRA_REVENUE` leg (a disguised bad-debt write-off) is rejected
//!   (`ManualAdjustmentNotAllowed`) with no books effect — `govern`'s write-off
//!   guard, additionally captured + paged out-of-band (the no-op sink + alarm are
//!   not asserted here).
//!
//! The handler is `pub`, so this out-of-crate test drives it directly with a no-op
//! publisher + the no-op secured-audit sink (mirrors `postgres_credit_note.rs`).
//! Ignored by default; run with `-- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines
)]

use std::sync::Arc;

use bss_ledger::domain::adjustment::manual::{
    ManualAdjustmentAction, ManualAdjustmentRequest, ManualLeg,
};
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::adjustment::manual_adjustment_service::ManualAdjustmentHandler;
use bss_ledger::infra::audit::secured_audit_sink::{NoopSecuredAuditSink, SecuredAuditSink};
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, Side};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

async fn scalar_i64(conn: &DatabaseConnection, sql: &str) -> Option<i64> {
    conn.query_one_raw(pg(sql.to_owned()))
        .await
        .unwrap()
        .map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

/// Provisioned seller ids (the parking / clearing / write-off classes a governed
/// manual adjustment may touch).
struct Seller {
    tenant: Uuid,
    // Used by the deferred payer-gate `#[ignore]` test (TODO below): an AR /
    // UNALLOCATED manual leg attributes to this payer. Held now so the seed already
    // carries it.
    #[allow(dead_code)]
    payer: Uuid,
    suspense: Uuid,
    cash_clearing: Uuid,
    ar: Uuid,
    unallocated: Uuid,
    goodwill: Uuid,
    contra_revenue: Uuid,
    tax: Uuid,
    period_id: String,
}

fn account(tenant: Uuid, id: Uuid, class: AccountClass, normal: Side) -> AccountRow {
    AccountRow {
        account_id: id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: "USD".to_owned(),
        revenue_stream: None,
        normal_side: normal.as_str().to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

/// Boot, migrate, seed USD@2 + an OPEN period + the parking/clearing chart.
async fn setup(url: &str) -> (DatabaseConnection, DBProvider<DbError>, Seller) {
    let raw = Database::connect(url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        suspense: Uuid::now_v7(),
        cash_clearing: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        goodwill: Uuid::now_v7(),
        contra_revenue: Uuid::now_v7(),
        tax: Uuid::now_v7(),
        period_id: "202606".to_owned(),
    };

    let reference = ReferenceRepo::new(provider.clone());
    reference
        .upsert_currency_scale(CurrencyScaleRow {
            tenant_id: s.tenant,
            currency: "USD".to_owned(),
            minor_units: 2,
            plausible_max_major: DEFAULT_PLAUSIBLE_MAX_MAJOR,
            source: "iso".to_owned(),
        })
        .await
        .unwrap();
    // Seed BOTH the invoice's period (`s.period_id`) and the CURRENT period: the
    // adjustment handlers post into `OffsetDateTime::now_utc()`'s period (credit/debit-note
    // `eff_date = OffsetDateTime::now_utc()`), so a fixed historical period alone makes the test
    // date-dependent (green only in that calendar month). ON CONFLICT dedups when
    // `now` already equals `s.period_id`.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{t}','{t}','{p}','UTC','OPEN'), ('{t}','{t}','{cur}','UTC','OPEN')
         ON CONFLICT DO NOTHING",
        t = s.tenant,
        p = s.period_id,
        cur = bss_ledger::domain::instant::yyyymm(OffsetDateTime::now_utc())
    )))
    .await
    .unwrap();

    // The parking / clearing classes resolve stream-less (ChartIndex keys non-
    // per-stream classes on stream = None). CONTRA_REVENUE is provisioned only so
    // the write-off test can resolve a chart account — `govern` rejects it BEFORE
    // any post, so the account is never actually debited.
    for row in [
        // SUSPENSE is credit-normal by convention — a holding/clearing account
        // (mirrors REFUND_CLEARING; the unknown_final refund park books +amount to
        // it). Consistent with tests/postgres_refund.rs (the park target).
        account(s.tenant, s.suspense, AccountClass::Suspense, Side::Credit),
        account(
            s.tenant,
            s.cash_clearing,
            AccountClass::CashClearing,
            Side::Debit,
        ),
        account(s.tenant, s.ar, AccountClass::Ar, Side::Debit),
        account(
            s.tenant,
            s.unallocated,
            AccountClass::Unallocated,
            Side::Credit,
        ),
        account(s.tenant, s.goodwill, AccountClass::Goodwill, Side::Debit),
        account(
            s.tenant,
            s.contra_revenue,
            AccountClass::ContraRevenue,
            Side::Debit,
        ),
        account(s.tenant, s.tax, AccountClass::TaxPayable, Side::Credit),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    (raw, provider, s)
}

fn handler(provider: &DBProvider<DbError>) -> ManualAdjustmentHandler {
    let audit: Arc<dyn SecuredAuditSink> = Arc::new(NoopSecuredAuditSink::new());
    ManualAdjustmentHandler::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        audit,
    )
}

async fn bal(raw: &DatabaseConnection, s: &Seller, account: Uuid) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
            s.tenant, account
        ),
    )
    .await
}

async fn entry_count(raw: &DatabaseConnection, s: &Seller, adjustment_id: &str) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_doc_type='MANUAL_ADJUSTMENT' \
             AND source_business_id='{adjustment_id}'",
            s.tenant
        ),
    )
    .await
}

fn leg(class: AccountClass, side: Side, amount_minor: i64) -> ManualLeg {
    ManualLeg {
        account_class: class,
        side,
        amount_minor,
        revenue_stream: None,
    }
}

fn req(
    s: &Seller,
    adjustment_id: &str,
    action: ManualAdjustmentAction,
    payer: Option<Uuid>,
    legs: Vec<ManualLeg>,
) -> ManualAdjustmentRequest {
    ManualAdjustmentRequest {
        tenant_id: s.tenant,
        payer_tenant_id: payer,
        adjustment_id: adjustment_id.to_owned(),
        action,
        currency: "USD".to_owned(),
        legs,
        reason_code: "ROUNDING_RESIDUE".to_owned(),
        preparer_actor_id: Uuid::now_v7(),
        approver_actor_id: None,
        tax: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn rounding_correction_posts_then_replays_idempotently() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // A balanced 1-minor rounding correction between two parking/clearing classes:
    // DR SUSPENSE 1 / CR CASH_CLEARING 1 (both in the RoundingCorrection allow-list;
    // no payer-scoped class, so payer_tenant_id is unnecessary).
    let request = req(
        &s,
        "ADJ-RC-1",
        ManualAdjustmentAction::RoundingCorrection,
        None,
        // DR CASH_CLEARING / CR SUSPENSE: CASH_CLEARING is debit-normal AND guarded
        // (must stay >= 0), so it must be DEBITED here (a CR from a zero balance would
        // drive it negative → NegativeBalance reject). SUSPENSE is credit-normal and
        // NOT guarded, so its CR lands the rounding residue as a clean +1.
        vec![
            leg(AccountClass::CashClearing, Side::Debit, 1),
            leg(AccountClass::Suspense, Side::Credit, 1),
        ],
    );

    let posted = handler(&provider)
        .post_manual_adjustment(&ctx, &scope, request.clone())
        .await
        .expect("rounding correction posts");
    assert!(!posted.replayed, "the fresh post is not a replay");
    assert_eq!(
        entry_count(&raw, &s, "ADJ-RC-1").await,
        Some(1),
        "exactly one manual-adjustment entry posted"
    );
    assert_eq!(
        bal(&raw, &s, s.suspense).await,
        Some(1),
        "SUSPENSE credited +1 (credit-normal holding account)"
    );
    assert_eq!(
        bal(&raw, &s, s.cash_clearing).await,
        Some(1),
        "CASH_CLEARING debited 1 (debit-normal)"
    );

    // Re-post the SAME (tenant, MANUAL_ADJUSTMENT, adjustment_id): the engine claim
    // makes it an idempotent replay — same entry id, replayed == true, no second
    // entry / balance effect.
    let replayed = handler(&provider)
        .post_manual_adjustment(&ctx, &scope, request)
        .await
        .expect("replay returns the prior post");
    assert!(replayed.replayed, "the re-post is an idempotent replay");
    assert_eq!(
        replayed.entry_id, posted.entry_id,
        "the replay returns the original entry id"
    );
    assert_eq!(
        entry_count(&raw, &s, "ADJ-RC-1").await,
        Some(1),
        "no second entry on replay"
    );
    assert_eq!(
        bal(&raw, &s, s.suspense).await,
        Some(1),
        "SUSPENSE unchanged on replay (still +1)"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn class_outside_allow_list_is_not_allowed() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // TAX_PAYABLE is in NO action's allow-list. A balanced DR SUSPENSE 5 / CR
    // TAX_PAYABLE 5 nets to zero + is not a REVENUE/CL/CONTRA shape, so it reaches
    // the allow-list check and is rejected there (NotAllowed) — no books effect.
    let request = req(
        &s,
        "ADJ-BAD-CLASS",
        ManualAdjustmentAction::RoundingCorrection,
        None,
        vec![
            leg(AccountClass::Suspense, Side::Debit, 5),
            leg(AccountClass::TaxPayable, Side::Credit, 5),
        ],
    );

    let err = handler(&provider)
        .post_manual_adjustment(&ctx, &scope, request)
        .await
        .expect_err("a class outside the allow-list must be rejected");
    assert!(
        matches!(err, DomainError::ManualAdjustmentNotAllowed(_)),
        "expected ManualAdjustmentNotAllowed, got {err:?}"
    );
    assert_eq!(
        entry_count(&raw, &s, "ADJ-BAD-CLASS").await,
        Some(0),
        "no entry posted for a rejected class"
    );
    assert!(
        matches!(bal(&raw, &s, s.suspense).await, None | Some(0)),
        "SUSPENSE untouched"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn contra_revenue_write_off_is_not_allowed_and_does_not_post() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // A bare CONTRA_REVENUE leg with no paired same-stream recognized-REVENUE
    // reduction is the disguised bad-debt write-off shape: DR CONTRA_REVENUE 100 /
    // CR SUSPENSE 100. `govern` rejects it as AttemptedWriteOff, mapped to the
    // canonical ManualAdjustmentNotAllowed 400 (and captured + paged out-of-band).
    // No books effect — the CONTRA_REVENUE account stays untouched.
    let request = req(
        &s,
        "ADJ-WRITEOFF",
        ManualAdjustmentAction::SuspenseClear,
        None,
        vec![
            leg(AccountClass::ContraRevenue, Side::Debit, 100),
            leg(AccountClass::Suspense, Side::Credit, 100),
        ],
    );

    let err = handler(&provider)
        .post_manual_adjustment(&ctx, &scope, request)
        .await
        .expect_err("an unpaired CONTRA_REVENUE write-off must be rejected");
    assert!(
        matches!(err, DomainError::ManualAdjustmentNotAllowed(_)),
        "expected ManualAdjustmentNotAllowed, got {err:?}"
    );
    assert_eq!(
        entry_count(&raw, &s, "ADJ-WRITEOFF").await,
        Some(0),
        "no entry posted for an attempted write-off"
    );
    assert!(
        matches!(bal(&raw, &s, s.contra_revenue).await, None | Some(0)),
        "CONTRA_REVENUE untouched — the write-off never posted"
    );
}

// TODO(VHP-1856 Slice 3 Phase 3): a payer-gate `#[ignore]` test — an AR /
// UNALLOCATED leg with `payer_tenant_id = None` must reject with
// `ManualAdjustmentNotAllowed("AR/UNALLOCATED leg requires payer_tenant_id")`, and
// the same legs WITH a payer must post (the AR / UNALLOCATED grain attributing to
// `payer_tenant_id`). Deferred to keep this harness lean; the payer-gate branch is
// a pure pre-post check exercised by the handler unit path.
