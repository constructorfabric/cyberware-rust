//! Postgres-only **concurrency** tests for the reusable-credit (wallet) slice:
//! they race the real `CreditApplicationService` (grant / apply) on a
//! testcontainer Postgres and pin the wallet invariant under contention — the
//! sub-grain no-negative CHECK (`chk_reusable_credit_subbalance_no_negative`) is
//! the backstop that bounds total draw-down at the funded amount. Ignored by
//! default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_credit_concurrency -- --ignored`.
//!
//! Covers: (1) two concurrent `apply_credit`s draining ONE wallet never overspend
//! it — the wallet never goes negative and the total drawn never exceeds the
//! funded amount (a clean reject of the loser is acceptable per Decision O); (2)
//! a grant and an apply touching the SAME `(payer, currency, event_type)`
//! sub-grain run concurrently without deadlock — both complete (or one cleanly
//! rejects) and the final wallet balance is consistent.
//!
//! Self-contained: re-declares the small `boot` / `setup_seller` /
//! `seed_ar_invoice` helpers it needs (mirrors `postgres_payment_concurrency.rs`),
//! so it does not depend on `postgres_credit.rs`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::precedence::Allocated;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::credit::{ApplyRequest, CreditApplicationService, GrantRequest};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Boot a container, migrate on a raw connection, and return a `bss`-search-path
/// `DBProvider`. Mirrors `postgres_payment_concurrency::boot`.
async fn boot() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    sea_orm::DatabaseConnection,
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
    (container, raw, provider)
}

/// Provisioned seller ids for the credit concurrency tests (mirrors
/// `postgres_credit::Seller`).
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    unallocated: Uuid,
    psp_fee: Uuid,
    ar: Uuid,
    reusable_credit: Uuid,
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

/// Provision a seller: USD@2 scale, an OPEN fiscal period for the current month,
/// the four payment-flow chart accounts, and a stream-less REUSABLE_CREDIT credit
/// account (the wallet). Mirrors `postgres_credit::setup_seller`.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        reusable_credit: Uuid::now_v7(),
        period_id: bss_ledger::domain::instant::yyyymm(now),
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
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{}','{}','{}','UTC','OPEN')",
        s.tenant, s.tenant, s.period_id
    )))
    .await
    .unwrap();

    for row in [
        account(s.tenant, s.cash, AccountClass::CashClearing, Side::Debit),
        account(
            s.tenant,
            s.unallocated,
            AccountClass::Unallocated,
            Side::Credit,
        ),
        account(
            s.tenant,
            s.psp_fee,
            AccountClass::PspFeeExpense,
            Side::Debit,
        ),
        account(s.tenant, s.ar, AccountClass::Ar, Side::Debit),
        account(
            s.tenant,
            s.reusable_credit,
            AccountClass::ReusableCredit,
            Side::Credit,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    s
}

fn settle_svc(provider: &DBProvider<DbError>) -> SettlementService {
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

fn credit_svc(provider: &DBProvider<DbError>) -> CreditApplicationService {
    CreditApplicationService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

fn settlement_input(s: &Seller, payment_id: &str, gross: i64) -> SettlementInput {
    SettlementInput {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: payment_id.to_owned(),
        gross_minor: gross,
        fee_minor: 0,
        currency: "USD".to_owned(),
        effective_at: None,
    }
}

/// Fund the payer's unallocated pool, then grant `amount` into the named wallet
/// sub-grain (the apply tests' wallet funding path; grant is the only way to land
/// a `reusable_credit_subbalance` row with a real `first_granted_at`).
async fn fund_wallet(
    provider: &DBProvider<DbError>,
    s: &Seller,
    payment_id: &str,
    credit_application_id: &str,
    event_type: &str,
    amount: i64,
) {
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    settle_svc(provider)
        .settle(&ctx, &scope, settlement_input(s, payment_id, amount))
        .await
        .expect("settle to fund the pool");
    credit_svc(provider)
        .grant_credit(
            &ctx,
            &scope,
            GrantRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                credit_application_id: credit_application_id.to_owned(),
                currency: "USD".to_owned(),
                amount_minor: amount,
                credit_grant_event_type: event_type.to_owned(),
            },
        )
        .await
        .expect("grant to fund the wallet");
}

/// Read one wallet sub-grain's `balance_minor` from the projector cache (the
/// concurrency invariant surface). `0` when the row is absent.
async fn wallet_subgrain(raw: &sea_orm::DatabaseConnection, s: &Seller, event_type: &str) -> i64 {
    raw.query_one_raw(pg(format!(
        "SELECT balance_minor FROM bss.ledger_reusable_credit_subbalance \
         WHERE tenant_id='{}' AND payer_tenant_id='{}' AND currency='USD' \
         AND credit_grant_event_type='{}'",
        s.tenant, s.payer, event_type
    )))
    .await
    .unwrap()
    .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap())
}

async fn ar_invoice_balance(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    invoice_id: &str,
) -> Option<i64> {
    raw.query_one_raw(pg(format!(
        "SELECT balance_minor FROM bss.ledger_ar_invoice_balance \
         WHERE tenant_id='{}' AND invoice_id='{}'",
        s.tenant, invoice_id
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

/// Seed an OPEN AR invoice by a DIRECT raw INSERT into the cache (mirrors the
/// `list_open_ar_invoices` / `allocate_too_large` seed idiom in
/// `postgres_payments.rs` / `postgres_payment_concurrency.rs`): no invoice
/// posting, just a `balance_minor > 0` candidate row the apply's open-AR read
/// will return. Only the NOT-NULL-without-default columns are supplied;
/// `original_posted_at` is set so the oldest-first order is deterministic.
async fn seed_ar_invoice(
    provider: &DBProvider<DbError>,
    s: &Seller,
    invoice_id: &str,
    amount: i64,
    posted_at: OffsetDateTime,
) {
    // Post a REAL balanced invoice (DR AR / CR PSP_FEE_EXPENSE) so ALL three AR
    // grains (account_balance, ar_payer_balance, ar_invoice_balance) are credited
    // — a raw INSERT into only the per-invoice cache would leave the aggregate AR
    // balances at 0, and the apply's CR AR would then drive them negative
    // (masking the wallet-cap race these tests target). Credit apply uses EXPLICIT
    // targets, so the AR posted-at ordering is irrelevant; the period must be the
    // OPEN current month, so callers pass a current-month `posted_at`.
    let posting = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let entry = NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: s.tenant,
        legal_entity_id: s.tenant,
        period_id: s.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: invoice_id.to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: posted_at,
        effective_at: to_naive_date(posted_at),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: s.tenant,
        correlation_id: Uuid::now_v7(),
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![ar_line(s, invoice_id, amount), psp_credit_line(s, amount)];
    posting
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("seed AR invoice post must succeed");
}

fn ar_line(s: &Seller, invoice_id: &str, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: s.payer,
        seller_tenant_id: Some(s.tenant),
        resource_tenant_id: None,
        account_id: s.ar,
        account_class: AccountClass::Ar,
        gl_code: None,
        side: Side::Debit,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: Some(invoice_id.to_owned()),
        due_date: Some(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()),
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

fn psp_credit_line(s: &Seller, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: s.payer,
        seller_tenant_id: Some(s.tenant),
        resource_tenant_id: None,
        account_id: s.psp_fee,
        account_class: AccountClass::PspFeeExpense,
        gl_code: None,
        side: Side::Credit,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

/// Client-side retry of a service call on a projector-level serialization
/// conflict (copied verbatim from `postgres_payment_concurrency.rs`). Under
/// `SERIALIZABLE`, two ops racing the same wallet grain make one abort with a
/// 40001 that the projector stringifies into `DomainError::Internal("…could not
/// serialize…")` — decision O defers in-service recompute-on-retry to the CALLER,
/// so the test models what the SDK/client does: re-run the WHOLE operation until
/// it commits or hits a real business rejection. A genuine deadlock (40P01) is
/// NOT a serialization conflict and propagates — failing the test loudly, which
/// is exactly the deadlock-freedom guarantee.
async fn retry_on_serialization<F, Fut, T>(mut op: F) -> Result<T, DomainError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DomainError>>,
{
    for _ in 0..20 {
        match op().await {
            Err(DomainError::Internal(m)) if m.contains("serialize") => {}
            other => return other,
        }
    }
    op().await
}

/// Credit #1: two concurrent applies draining ONE wallet sub-grain must never
/// overspend it. The wallet is funded at 500 ("promo"); two open AR invoices of
/// 300 each give the pair more receivable headroom (600) than the wallet holds
/// (500), so the WALLET — not the AR — is the binding cap. Each apply draws 300;
/// at most one-and-a-fraction can fit. The no-negative sub-grain CHECK
/// (`chk_reusable_credit_subbalance_no_negative`) is the backstop: a loser racing
/// the same grain serializes (the client retries the 40001) and then either fits
/// in the remaining wallet or is cleanly rejected `CreditExceedsWallet`. INVARIANT:
/// the wallet never goes negative and the total drawn never exceeds the funded
/// 500. Mirrors `postgres_payment_concurrency::concurrent_allocate_respects_per
/// _payment_cap`, scaled to a wallet-cap race.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_applies_drain_one_wallet_without_overspend() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Fund a 500 "promo" wallet, and seed two open AR invoices of 300 each (600
    // receivable headroom > 500 wallet, so the wallet is the binding cap).
    fund_wallet(&provider, &s, "PAY-WAL", "CR-FUND", "promo", 500).await;
    seed_ar_invoice(&provider, &s, "inv-1", 300, OffsetDateTime::now_utc()).await;
    seed_ar_invoice(&provider, &s, "inv-2", 300, OffsetDateTime::now_utc()).await;
    assert_eq!(
        wallet_subgrain(&raw, &s, "promo").await,
        500,
        "wallet funded"
    );

    // Two applies, each draining 300 from the wallet against a DISTINCT invoice
    // (distinct credit_application_id ⇒ the dedup key never collides — both are
    // genuine fresh applies racing the wallet cap). Each wrapped in
    // `retry_on_serialization` so a projector-level 40001 is retried, not failed.
    let apply = |credit_application_id: &'static str, invoice_id: &'static str| {
        let provider = provider.clone();
        let scope = AccessScope::for_tenant(s.tenant);
        let tenant = s.tenant;
        let payer = s.payer;
        tokio::spawn(async move {
            let svc = credit_svc(&provider);
            let ctx = SecurityContext::anonymous();
            retry_on_serialization(|| {
                svc.apply_credit(
                    &ctx,
                    &scope,
                    ApplyRequest {
                        tenant_id: tenant,
                        payer_tenant_id: payer,
                        credit_application_id: credit_application_id.to_owned(),
                        currency: "USD".to_owned(),
                        targets: vec![Allocated {
                            invoice_id: invoice_id.to_owned(),
                            amount_minor: 300,
                        }],
                    },
                )
            })
            .await
        })
    };

    let a = apply("CR-A", "inv-1");
    let b = apply("CR-B", "inv-2");
    let (ra, rb) = tokio::join!(a, b);
    let ra = ra.expect("apply task A must not panic / deadlock");
    let rb = rb.expect("apply task B must not panic / deadlock");

    // Each side either committed or was CLEANLY rejected — and never deadlocked
    // (a deadlock would have propagated out of `retry_on_serialization` and
    // panicked the task). A loser of the wallet-cap race rejects one of two
    // legitimate ways (decision O — a clean reject on a race, not necessarily a
    // clean 400): the orchestrator's pre-check saw the already-reduced wallet
    // (`CreditExceedsWallet`), or the projector's no-negative backstop caught the
    // overdraw at post time on the wallet sub-grain (`NegativeBalance`). Both roll
    // back with no overspend.
    let mut drawn = 0i64;
    for result in [&ra, &rb] {
        match result {
            Ok(outcome) => drawn += outcome.debits.iter().map(|d| d.amount_minor).sum::<i64>(),
            Err(DomainError::CreditExceedsWallet(_) | DomainError::NegativeBalance(_)) => {}
            Err(other) => panic!(
                "a losing concurrent apply must cleanly reject (CreditExceedsWallet or \
                 NegativeBalance), got: {other:?}"
            ),
        }
    }
    // At least one apply made progress.
    assert!(
        ra.is_ok() || rb.is_ok(),
        "at least one concurrent apply must win: {ra:?} / {rb:?}"
    );

    // INVARIANT: the wallet never went negative, and the total drawn never
    // exceeded the funded 500 (no overspend). The remaining balance equals the
    // funded amount minus what the winners drew.
    let remaining = wallet_subgrain(&raw, &s, "promo").await;
    assert!(
        remaining >= 0,
        "the wallet sub-grain must never go negative, got {remaining}"
    );
    assert!(
        drawn <= 500,
        "total drawn ({drawn}) must never exceed the funded wallet (500)"
    );
    assert_eq!(
        remaining,
        500 - drawn,
        "remaining wallet == funded - drawn (no double-spend, no leak)"
    );
}

/// Credit #2: a grant and an apply touching the SAME `(payer, currency,
/// event_type)` sub-grain run concurrently and must serialize WITHOUT deadlock.
/// The wallet starts at 400 "promo"; a grant of +300 and an apply of -300 (against
/// an open AR of 300) both write the same sub-grain row. SERIALIZABLE + the client
/// retry (decision O) must serialize them — both complete (the `tokio::join!`
/// returns with no runtime hang). The two orderings give two consistent end
/// states, both checked: grant-then-apply ⇒ 400 + 300 − 300 = 400; apply-then-
/// grant ⇒ 400 − 300 + 300 = 400. Either way the final balance is 400 and the AR
/// is fully paid. (Should the apply instead lose cleanly — e.g. if it serialized
/// against an empty wallet read — that is acceptable per decision O; the assert
/// admits a clean apply rejection and pins the resulting balance.) Mirrors
/// `postgres_payment_concurrency::allocate_and_invoice_post_serialize_without
/// _deadlock`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn grant_and_apply_on_same_subgrain_serialize_without_deadlock() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Start the "promo" wallet at 400, and fund the pool with extra headroom so a
    // concurrent +300 grant has unallocated cash to draw from. Seed an open AR of
    // 300 the apply pays.
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-SEED", 1000))
        .await
        .expect("settle to fund the pool (400 grant + 300 concurrent grant headroom)");
    credit_svc(&provider)
        .grant_credit(
            &ctx,
            &scope,
            GrantRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                credit_application_id: "CR-SEED".to_owned(),
                currency: "USD".to_owned(),
                amount_minor: 400,
                credit_grant_event_type: "promo".to_owned(),
            },
        )
        .await
        .expect("seed the wallet at 400");
    seed_ar_invoice(&provider, &s, "inv-1", 300, OffsetDateTime::now_utc()).await;
    assert_eq!(
        wallet_subgrain(&raw, &s, "promo").await,
        400,
        "wallet seeded"
    );

    // Race a +300 grant and a -300 apply on the SAME promo sub-grain. Both retry
    // the projector serialization conflict (decision O).
    let grant_provider = provider.clone();
    let grant_scope = scope.clone();
    let grant_tenant = s.tenant;
    let grant_payer = s.payer;
    let grant = tokio::spawn(async move {
        let svc = credit_svc(&grant_provider);
        let ctx = SecurityContext::anonymous();
        retry_on_serialization(|| {
            svc.grant_credit(
                &ctx,
                &grant_scope,
                GrantRequest {
                    tenant_id: grant_tenant,
                    payer_tenant_id: grant_payer,
                    credit_application_id: "CR-CONC-GRANT".to_owned(),
                    currency: "USD".to_owned(),
                    amount_minor: 300,
                    credit_grant_event_type: "promo".to_owned(),
                },
            )
        })
        .await
    });

    let apply_provider = provider.clone();
    let apply_scope = scope.clone();
    let apply_tenant = s.tenant;
    let apply_payer = s.payer;
    let apply = tokio::spawn(async move {
        let svc = credit_svc(&apply_provider);
        let ctx = SecurityContext::anonymous();
        retry_on_serialization(|| {
            svc.apply_credit(
                &ctx,
                &apply_scope,
                ApplyRequest {
                    tenant_id: apply_tenant,
                    payer_tenant_id: apply_payer,
                    credit_application_id: "CR-CONC-APPLY".to_owned(),
                    currency: "USD".to_owned(),
                    targets: vec![Allocated {
                        invoice_id: "inv-1".to_owned(),
                        amount_minor: 300,
                    }],
                },
            )
        })
        .await
    });

    let (grant_res, apply_res) = tokio::join!(grant, apply);
    let grant_res = grant_res.expect("grant task must not panic / deadlock");
    let apply_res = apply_res.expect("apply task must not panic / deadlock");

    // The grant has unallocated headroom regardless of interleaving, so it must
    // commit. The apply commits in both real interleavings (wallet >= 300 before
    // OR after the +300 grant); a clean `CreditExceedsWallet` is tolerated per
    // decision O but not expected here.
    grant_res.expect("the concurrent grant must commit");
    let apply_ok = match apply_res {
        Ok(_) => true,
        Err(DomainError::CreditExceedsWallet(_)) => false,
        Err(other) => panic!("apply must succeed or cleanly reject, got: {other:?}"),
    };

    // Consistent final state: grant added 300; apply (if it ran) removed 300 and
    // paid the AR. The no-negative CHECK was never violated (the tasks did not
    // panic), and the balance reflects exactly the ops that committed.
    let promo = wallet_subgrain(&raw, &s, "promo").await;
    assert!(promo >= 0, "the wallet sub-grain must never go negative");
    if apply_ok {
        assert_eq!(
            promo, 400,
            "grant +300 and apply -300 net to the seeded 400"
        );
        assert_eq!(
            ar_invoice_balance(&raw, &s, "inv-1").await,
            Some(0),
            "the apply fully paid inv-1"
        );
    } else {
        assert_eq!(promo, 700, "only the grant committed: 400 + 300");
        assert_eq!(
            ar_invoice_balance(&raw, &s, "inv-1").await,
            Some(300),
            "the rejected apply left inv-1 open"
        );
    }
}
