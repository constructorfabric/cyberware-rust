//! Postgres tests for `TieOutJob::run()` + `emit()` — the cross-tenant sweep.
//! Boots a container, seeds a clean and a drifted tenant, calls `job.run()`,
//! asserts it completes and that a drifted tenant produces a non-clean report.
//! Uses `noop()` publisher (no broker/assert-via-report).
//!
//! Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --lib 'infra::jobs::tieout::tests' -- --ignored`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::inconsistent_struct_constructor
)]

use std::collections::HashMap;
use std::sync::Arc;

use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    AccountBalanceVariance, EntryBackstopAcc, ImbalancedEntry, NegativeGrain, PaymentCounterAcc,
    PaymentCounterVariance, SubGrainAcc, SubGrainVariance, TieOutReport, cache_baseline_rows,
    cache_grains, fold_grains, key_account, negative_grains, settle_index, verify_incremental,
};
use crate::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use crate::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::jobs::tieout::TieOutJob;
use crate::infra::posting::service::PostingService;
use crate::infra::storage::entity::verified_balance::GRAIN_ACCOUNT;
use crate::infra::storage::entity::{
    account_balance, ar_invoice_balance, ar_payer_balance, journal_entry, journal_line,
    payment_allocation, payment_settlement, reusable_credit_subbalance, tax_subbalance,
    unallocated_balance,
};
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::repo::ReferenceRepo;
use time::OffsetDateTime;

fn bal(account_id: u128, class: &str, balance_minor: i64) -> account_balance::Model {
    account_balance::Model {
        tenant_id: Uuid::from_u128(0xA1),
        account_id: Uuid::from_u128(account_id),
        currency: "USD".to_owned(),
        account_class: class.to_owned(),
        normal_side: "DR".to_owned(),
        balance_minor,
        functional_balance_minor: None,
        functional_currency: None,
        last_entry_seq: None,
        version: 0,
    }
}

#[test]
fn negative_grains_flags_guarded_and_unknown_not_legal_negative_class() {
    // `AR` is guarded (must stay `>= 0`) — a negative AR balance is a defect.
    // `REVENUE` may legitimately go negative — not a defect. An unknown /
    // corrupt class that is negative is flagged (fail loud). A non-negative
    // guarded balance is fine.
    let ar_neg = bal(1, "AR", -100);
    let revenue_neg = bal(2, "REVENUE", -100);
    let ar_ok = bal(3, "AR", 50);
    let unknown_neg = bal(4, "NOT_A_REAL_CLASS", -100);
    let grains = negative_grains(&[ar_neg, revenue_neg, ar_ok, unknown_neg]);
    let mut flagged: Vec<Uuid> = grains.iter().map(|g| g.account_id).collect();
    flagged.sort_unstable();
    assert_eq!(
        flagged,
        vec![Uuid::from_u128(1), Uuid::from_u128(4)],
        "negative guarded (AR) and unknown classes are defects; legal REVENUE is not"
    );
}

fn line_for(entry_id: Uuid, side: &str, amount_minor: i64) -> journal_line::Model {
    journal_line::Model {
        line_id: Uuid::now_v7(),
        entry_id,
        tenant_id: Uuid::from_u128(0xA1),
        period_id: "2025-01".to_owned(),
        payer_tenant_id: Uuid::from_u128(0xA1),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::from_u128(0xBB),
        account_class: "AR".to_owned(),
        gl_code: None,
        side: side.to_owned(),
        amount_minor,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: "RESOLVED".to_owned(),
        functional_amount_minor: None,
        functional_currency: None,
        rate_snapshot_ref: None,
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

fn clean_report() -> TieOutReport {
    TieOutReport {
        tenant_id: Uuid::from_u128(0xA1),
        posted_line_count: 0,
        account_balance_variances: vec![],
        sub_grain_variances: vec![],
        imbalanced_entries: vec![],
        negative_grains: vec![],
        payment_counter_variances: vec![],
        pending_lines: 0,
    }
}

/// Every defect vector on its own, because `is_clean` is a conjunction.
///
/// Seeding one vector leaves every other conjunct free, so a chain that has lost
/// one still answers `false` for the vector that is populated. With only
/// `negative_grains` seeded, `sub_grain_variances`, `imbalanced_entries` and
/// `payment_counter_variances` can each be dropped from the `&&` with the whole
/// suite green, and a diverged sub-grain cache then reads as clean books.
#[test]
fn is_clean_true_only_when_all_defect_vecs_empty() {
    type Seed = fn(&mut TieOutReport);

    let clean = clean_report();
    assert!(clean.is_clean());

    let seeds: Vec<(&str, Seed)> = vec![
        ("account_balance_variances", |report| {
            report
                .account_balance_variances
                .push(AccountBalanceVariance {
                    account_id: Uuid::from_u128(1),
                    currency: "USD".to_owned(),
                    computed: 100,
                    cached: 90,
                });
        }),
        ("sub_grain_variances", |report| {
            report.sub_grain_variances.push(SubGrainVariance {
                grain: "ar_payer_balance",
                key: "payer=1".to_owned(),
                computed: 100,
                cached: 90,
            });
        }),
        ("imbalanced_entries", |report| {
            report.imbalanced_entries.push(ImbalancedEntry {
                entry_id: Uuid::from_u128(2),
                currency: "USD".to_owned(),
                net_minor: 10,
                line_count: 2,
                payer_count: 1,
            });
        }),
        ("negative_grains", |report| {
            report.negative_grains.push(NegativeGrain {
                account_id: Uuid::from_u128(1),
                currency: "USD".to_owned(),
                balance_minor: -50,
            });
        }),
        ("payment_counter_variances", |report| {
            report
                .payment_counter_variances
                .push(PaymentCounterVariance {
                    payment_id: "pay-1".to_owned(),
                    counter: "allocated_minor",
                    computed: 100,
                    cached: 90,
                });
        }),
    ];

    for (vector, seed) in seeds {
        let mut dirty = clean_report();
        seed(&mut dirty);
        assert!(
            !dirty.is_clean(),
            "a report carrying one {vector} entry is not clean"
        );
    }

    let mut negative = clean_report();
    negative.negative_grains.push(NegativeGrain {
        account_id: Uuid::from_u128(1),
        currency: "USD".to_owned(),
        balance_minor: -50,
    });
    assert!(
        negative.summary().contains("negative_grains=1"),
        "the summary carries the count, not just the word: {}",
        negative.summary()
    );
}

#[test]
fn is_clean_false_on_pending_lines() {
    let mut r = clean_report();
    r.pending_lines = 1;
    assert!(!r.is_clean());
}

#[test]
fn entry_backstop_flags_unbalanced_entry() {
    let entry_id = Uuid::now_v7();
    let lines = vec![
        line_for(entry_id, "DR", 1000),
        line_for(entry_id, "CR", 999),
    ];
    let flagged = entry_backstop(&lines);
    assert_eq!(flagged.len(), 1, "1-minor drift must be caught");
    assert_eq!(flagged[0].entry_id, entry_id);
    assert_eq!(flagged[0].net_minor, 1); // DR 1000 - CR 999 = +1
}

#[test]
fn entry_backstop_passes_balanced_entry() {
    let entry_id = Uuid::now_v7();
    let lines = vec![
        line_for(entry_id, "DR", 1000),
        line_for(entry_id, "CR", 1000),
    ];
    assert!(entry_backstop(&lines).is_empty());
}

#[test]
fn entry_backstop_empty_input() {
    assert!(entry_backstop(&[]).is_empty());
}

// ---------------------------------------------------------------------------
// Docker (testcontainers) helpers
// ---------------------------------------------------------------------------

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

struct Fixture {
    tenant: Uuid,
    ar_account: Uuid,
    cash_account: Uuid,
    legal_entity: Uuid,
    period_id: String,
}

/// Boot, migrate, seed USD@2 + OPEN period + AR/CASH accounts.
async fn setup(
    container_url: &str,
) -> (
    DatabaseConnection,
    PostingService,
    DBProvider<DbError>,
    Fixture,
) {
    let raw = Database::connect(container_url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{container_url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let legal_entity = tenant;
    let period_id = "202606".to_owned();
    let ar_account = Uuid::now_v7();
    let cash_account = Uuid::now_v7();

    let reference = ReferenceRepo::new(provider.clone());
    reference
        .upsert_currency_scale(CurrencyScaleRow {
            tenant_id: tenant,
            currency: "USD".to_owned(),
            minor_units: 2,
            plausible_max_major: DEFAULT_PLAUSIBLE_MAX_MAJOR,
            source: "iso".to_owned(),
        })
        .await
        .unwrap();

    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{tenant}','{legal_entity}','{period_id}','UTC','OPEN')"
    )))
    .await
    .unwrap();

    reference
        .insert_account(AccountRow {
            account_id: ar_account,
            tenant_id: tenant,
            legal_entity_id: legal_entity,
            account_class: "AR".to_owned(),
            currency: "USD".to_owned(),
            revenue_stream: None,
            normal_side: "DR".to_owned(),
            may_go_negative: false,
            lifecycle_state: "OPEN".to_owned(),
        })
        .await
        .unwrap();
    reference
        .insert_account(AccountRow {
            account_id: cash_account,
            tenant_id: tenant,
            legal_entity_id: legal_entity,
            account_class: "CASH_CLEARING".to_owned(),
            currency: "USD".to_owned(),
            revenue_stream: None,
            normal_side: "CR".to_owned(),
            may_go_negative: false,
            lifecycle_state: "OPEN".to_owned(),
        })
        .await
        .unwrap();

    let service = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    (
        raw,
        service,
        provider,
        Fixture {
            tenant,
            ar_account,
            cash_account,
            legal_entity,
            period_id,
        },
    )
}

fn balanced_entry(f: &Fixture, business_id: &str, amount: i64) -> (NewEntry, Vec<NewLine>) {
    let entry_id = Uuid::now_v7();
    let entry = NewEntry {
        entry_id,
        tenant_id: f.tenant,
        legal_entity_id: f.legal_entity,
        period_id: f.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::ManualAdjustment,
        source_business_id: business_id.to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: f.tenant,
        correlation_id: f.tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![
        new_line(f, f.ar_account, AccountClass::Ar, Side::Debit, amount),
        new_line(
            f,
            f.cash_account,
            AccountClass::CashClearing,
            Side::Credit,
            amount,
        ),
    ];
    (entry, lines)
}

fn new_line(f: &Fixture, account: Uuid, class: AccountClass, side: Side, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: f.tenant,
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: account,
        account_class: class,
        gl_code: None,
        side,
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

/// Boot + seed + post one balanced entry; return raw conn, provider, fixture.
async fn setup_with_one_balanced_post(
    url: &str,
) -> (DatabaseConnection, DBProvider<DbError>, Fixture) {
    let (raw, service, provider, f) = setup(url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();
    let (entry, lines) = balanced_entry(&f, "biz-run-1", 1000);
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("balanced post must succeed");
    (raw, provider, f)
}

// ---------------------------------------------------------------------------
// Docker tests (ignored by default — require Docker / testcontainers)
// ---------------------------------------------------------------------------

/// `run()` over a single clean tenant completes with `Ok(())`.
/// Per-tenant report is also verified to be clean (confirms `run` calls
/// `tie_out_tenant` and no defect triggers `emit`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn run_over_clean_tenant_completes() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (_raw, provider, f) = setup_with_one_balanced_post(&url).await;

    let job = TieOutJob::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));

    // `run()` must return Ok and trigger no defects.
    job.run()
        .await
        .expect("run must succeed for a clean tenant");

    // Independently verify the per-tenant report is clean (pins the
    // `tie_out_tenant` call within `run`).
    let report = TieOutJob::new(provider, Arc::new(LedgerEventPublisher::noop()))
        .tie_out_tenant(f.tenant)
        .await
        .expect("tie_out_tenant must succeed");
    assert!(
        report.is_clean(),
        "clean books must tie out: {}",
        report.summary()
    );
}

/// `run()` over a drifted tenant completes with `Ok(())` AND the per-tenant
/// report is NOT clean, which means `run` reaches its `emit()` branch for that
/// tenant.
///
/// Observation limit: the alarm itself is not captured. `emit()` is observable
/// only through the broker (`LedgerEventPublisher::new` needs two live
/// `AsyncProducer`s) or its metrics mirror (the `noop` publisher carries
/// `metrics: None`); a capturing double would require standing up the outbox or
/// adding a production-only test constructor, both out of scope for a test-only
/// change. This test therefore asserts (a) `run` completes `Ok` over a drifted
/// tenant and (b) the exact divergence that drives `emit` — executing the
/// emit-dispatch line for coverage — but does not assert the emitted category.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn run_over_drifted_tenant_emits_alarm() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;

    // Corrupt the AR balance cache grain (add 1 so no-negative check stays
    // satisfied while creating a variance for the tie-out).
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_account_balance SET balance_minor = balance_minor + 1 \
         WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
        f.tenant, f.ar_account
    )))
    .await
    .unwrap();

    let job = TieOutJob::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));

    // `run()` must complete Ok even when a tenant is dirty — it logs/alarms, not
    // errors.
    job.run()
        .await
        .expect("run must succeed even for a drifted tenant");

    // Confirm the report is NOT clean — the run's `emit()` branch was reached.
    let report = TieOutJob::new(provider, Arc::new(LedgerEventPublisher::noop()))
        .tie_out_tenant(f.tenant)
        .await
        .expect("tie_out_tenant must succeed");
    assert!(
        !report.is_clean(),
        "drifted books must NOT tie out: {}",
        report.summary()
    );
    assert_eq!(
        report.account_balance_variances.len(),
        1,
        "exactly one grain diverged"
    );
}

// ---------------------------------------------------------------------------
// Group B unit tests — the new in-memory reconciles (no Docker). Mirror the
// existing pure-function tests (`negative_grains` / `entry_backstop`): build
// `Vec<Model>` fixtures + a normal_side map and assert the variance set. The
// fixtures are internally consistent — whatever `normal_side` is declared,
// `signed()` derives the delta and the "clean" cache is set to that same fold,
// so the assertions hold independent of the production chart's actual sides.
// ---------------------------------------------------------------------------

const PAYER: u128 = 0xBEEF;
const ACCT: u128 = 0xACC7;

/// A fully-specified journal line for the sub-grain / counter recomputes.
#[allow(clippy::too_many_arguments)]
fn jl(
    entry_id: Uuid,
    account_id: Uuid,
    account_class: &str,
    side: &str,
    amount_minor: i64,
    invoice_id: Option<&str>,
    ar_status: Option<&str>,
    credit_grant_event_type: Option<&str>,
) -> journal_line::Model {
    journal_line::Model {
        line_id: Uuid::now_v7(),
        entry_id,
        tenant_id: Uuid::from_u128(0xA1),
        period_id: "2026-06".to_owned(),
        payer_tenant_id: Uuid::from_u128(PAYER),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id,
        account_class: account_class.to_owned(),
        gl_code: None,
        side: side.to_owned(),
        amount_minor,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: invoice_id.map(ToOwned::to_owned),
        due_date: None,
        revenue_stream: None,
        mapping_status: "RESOLVED".to_owned(),
        functional_amount_minor: None,
        functional_currency: None,
        rate_snapshot_ref: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: credit_grant_event_type.map(ToOwned::to_owned),
        ar_status: ar_status.map(ToOwned::to_owned),
    }
}

/// `account_id -> "DR"` for every account a fixture's lines touch (all
/// debit-normal — the recompute is self-consistent regardless; see the module
/// note above).
fn dr_sides(account_ids: &[Uuid]) -> std::collections::HashMap<Uuid, String> {
    account_ids.iter().map(|a| (*a, "DR".to_owned())).collect()
}

fn ar_invoice_row(
    account_id: Uuid,
    invoice_id: &str,
    balance_minor: i64,
    disputed_minor: i64,
) -> ar_invoice_balance::Model {
    ar_invoice_balance::Model {
        tenant_id: Uuid::from_u128(0xA1),
        payer_tenant_id: Uuid::from_u128(PAYER),
        account_id,
        invoice_id: invoice_id.to_owned(),
        currency: "USD".to_owned(),
        balance_minor,
        disputed_minor,
        functional_balance_minor: None,
        functional_currency: None,
        original_posted_at: None,
        due_date: None,
        last_entry_seq: None,
        version: 0,
    }
}

fn unallocated_row(account_id: Uuid, balance_minor: i64) -> unallocated_balance::Model {
    unallocated_balance::Model {
        tenant_id: Uuid::from_u128(0xA1),
        payer_tenant_id: Uuid::from_u128(PAYER),
        account_id,
        currency: "USD".to_owned(),
        balance_minor,
        functional_balance_minor: None,
        functional_currency: None,
        last_entry_seq: None,
        version: 0,
    }
}

fn reusable_row(
    account_id: Uuid,
    event_type: &str,
    balance_minor: i64,
) -> reusable_credit_subbalance::Model {
    reusable_credit_subbalance::Model {
        tenant_id: Uuid::from_u128(0xA1),
        payer_tenant_id: Uuid::from_u128(PAYER),
        account_id,
        currency: "USD".to_owned(),
        credit_grant_event_type: event_type.to_owned(),
        first_granted_at: None,
        balance_minor,
        functional_balance_minor: None,
        functional_currency: None,
        last_entry_seq: None,
        version: 0,
    }
}

/// `recompute_sub_grain_variances` with the new arity but only the new caches
/// populated (AR-payer / AR-invoice-balance / tax left clean & empty).
fn sub_grain(
    lines: &[journal_line::Model],
    sides: &std::collections::HashMap<Uuid, String>,
    ar_invoice_cache: &[ar_invoice_balance::Model],
    unallocated_cache: &[unallocated_balance::Model],
    reusable_credit_cache: &[reusable_credit_subbalance::Model],
) -> Vec<super::SubGrainVariance> {
    recompute_sub_grain_variances(
        lines,
        sides,
        &[] as &[ar_payer_balance::Model],
        ar_invoice_cache,
        &[] as &[tax_subbalance::Model],
        unallocated_cache,
        reusable_credit_cache,
    )
}

#[test]
fn disputed_minor_clean_when_cache_matches_disputed_legs() {
    // An AR-reclass open: DR AR DISPUTED 300 + CR AR ACTIVE 300 on invoice INV1.
    // balance_minor nets 0 (DR +300, CR -300); disputed_minor folds only the
    // DISPUTED leg (+300). Cache balance=0, disputed=300 ⇒ clean.
    let acct = Uuid::from_u128(ACCT);
    let entry = Uuid::now_v7();
    let lines = vec![
        jl(
            entry,
            acct,
            "AR",
            "DR",
            300,
            Some("INV1"),
            Some("DISPUTED"),
            None,
        ),
        jl(
            entry,
            acct,
            "AR",
            "CR",
            300,
            Some("INV1"),
            Some("ACTIVE"),
            None,
        ),
    ];
    let cache = vec![ar_invoice_row(acct, "INV1", 0, 300)];
    let v = sub_grain(&lines, &dr_sides(&[acct]), &cache, &[], &[]);
    assert!(
        v.is_empty(),
        "balance nets 0 and disputed=+300 must tie out: {v:?}"
    );
}

#[test]
fn disputed_minor_flags_seeded_divergence() {
    // Same disputed legs (computed disputed=+300) but the cache says 250 ⇒ a
    // `ar_invoice_disputed` variance (and balance_minor still ties out).
    let acct = Uuid::from_u128(ACCT);
    let entry = Uuid::now_v7();
    let lines = vec![
        jl(
            entry,
            acct,
            "AR",
            "DR",
            300,
            Some("INV1"),
            Some("DISPUTED"),
            None,
        ),
        jl(
            entry,
            acct,
            "AR",
            "CR",
            300,
            Some("INV1"),
            Some("ACTIVE"),
            None,
        ),
    ];
    let cache = vec![ar_invoice_row(acct, "INV1", 0, 250)];
    let v = sub_grain(&lines, &dr_sides(&[acct]), &cache, &[], &[]);
    assert_eq!(v.len(), 1, "exactly the disputed grain diverges: {v:?}");
    assert_eq!(v[0].grain, "ar_invoice_disputed");
    assert_eq!(v[0].computed, 300);
    assert_eq!(v[0].cached, 250);
}

#[test]
fn unallocated_clean_then_flags_divergence() {
    // CR UNALLOCATED 1000 (settle) then DR UNALLOCATED 400 (allocate) ⇒
    // signed fold with a DR-normal account: +400 - 1000 = -600.
    let acct = Uuid::from_u128(ACCT);
    let e1 = Uuid::now_v7();
    let e2 = Uuid::now_v7();
    let lines = vec![
        jl(e1, acct, "UNALLOCATED", "CR", 1000, None, None, None),
        jl(e2, acct, "UNALLOCATED", "DR", 400, None, None, None),
    ];
    let sides = dr_sides(&[acct]);

    let clean = sub_grain(&lines, &sides, &[], &[unallocated_row(acct, -600)], &[]);
    assert!(
        clean.is_empty(),
        "unallocated fold (-600) must tie out: {clean:?}"
    );

    let dirty = sub_grain(&lines, &sides, &[], &[unallocated_row(acct, -500)], &[]);
    assert_eq!(dirty.len(), 1, "seeded divergence flagged: {dirty:?}");
    assert_eq!(dirty[0].grain, "unallocated_balance");
    assert_eq!(dirty[0].computed, -600);
    assert_eq!(dirty[0].cached, -500);
}

#[test]
fn reusable_credit_keys_by_event_type_and_flags_divergence() {
    // Two REUSABLE_CREDIT grants on the same account but different event types;
    // a None event type keys as "". Each is its own grain.
    let acct = Uuid::from_u128(ACCT);
    let e = Uuid::now_v7();
    let lines = vec![
        jl(
            e,
            acct,
            "REUSABLE_CREDIT",
            "CR",
            500,
            None,
            None,
            Some("PROMO"),
        ),
        jl(e, acct, "REUSABLE_CREDIT", "CR", 200, None, None, None),
    ];
    let sides = dr_sides(&[acct]);

    // DR-normal account, CR leg ⇒ -amount. PROMO=-500, ""=-200.
    let clean = sub_grain(
        &lines,
        &sides,
        &[],
        &[],
        &[
            reusable_row(acct, "PROMO", -500),
            reusable_row(acct, "", -200),
        ],
    );
    assert!(
        clean.is_empty(),
        "both event-type grains tie out: {clean:?}"
    );

    // Drop the empty-event-type cache row ⇒ that grain strays (computed -200 vs 0).
    let dirty = sub_grain(
        &lines,
        &sides,
        &[],
        &[],
        &[reusable_row(acct, "PROMO", -500)],
    );
    assert_eq!(dirty.len(), 1, "the \"\" grain diverges: {dirty:?}");
    assert_eq!(dirty[0].grain, "reusable_credit_subbalance");
    assert_eq!(dirty[0].computed, -200);
    assert_eq!(dirty[0].cached, 0);
    assert!(
        dirty[0].key.contains("event_type="),
        "key names the event-type dim: {}",
        dirty[0].key
    );
}

// --- payment-counter reconcile (B2) ---

fn settle_entry(entry_id: Uuid, payment_id: &str) -> journal_entry::Model {
    journal_entry::Model {
        entry_id,
        tenant_id: Uuid::from_u128(0xA1),
        legal_entity_id: Uuid::from_u128(0xA1),
        period_id: "2026-06".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: "PAYMENT_SETTLE".to_owned(),
        source_business_id: payment_id.to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: Uuid::from_u128(0xA1),
        correlation_id: Uuid::from_u128(0xA1),
        rounding_evidence: serde_json::Value::Null,
        created_seq: 1,
        row_hash: None,
        prev_hash: None,
        prev_entry_id: None,
        prev_period_id: None,
    }
}

fn return_entry(entry_id: Uuid, psp_return_id: &str) -> journal_entry::Model {
    journal_entry::Model {
        source_doc_type: "SETTLEMENT_RETURN".to_owned(),
        source_business_id: psp_return_id.to_owned(),
        ..settle_entry(entry_id, psp_return_id)
    }
}

#[allow(clippy::too_many_arguments)]
fn settlement_row(
    payment_id: &str,
    settled_minor: i64,
    fee_minor: i64,
    allocated_minor: i64,
) -> payment_settlement::Model {
    payment_settlement::Model {
        tenant_id: Uuid::from_u128(0xA1),
        payment_id: payment_id.to_owned(),
        currency: "USD".to_owned(),
        settled_minor,
        fee_minor,
        allocated_minor,
        refunded_minor: 0,
        refunded_unallocated_minor: 0,
        clawed_back_minor: 0,
        version: 0,
    }
}

fn alloc_row(payment_id: &str, invoice_id: &str, amount_minor: i64) -> payment_allocation::Model {
    payment_allocation::Model {
        tenant_id: Uuid::from_u128(0xA1),
        allocation_id: Uuid::now_v7(),
        invoice_id: invoice_id.to_owned(),
        payer_tenant_id: Uuid::from_u128(PAYER),
        payment_id: payment_id.to_owned(),
        amount_minor,
        currency: "USD".to_owned(),
        precedence_policy_ref: "p".to_owned(),
        allocated_at_utc: OffsetDateTime::now_utc(),
    }
}

#[test]
fn payment_counters_clean_when_journal_and_rows_agree() {
    // Settle gross 1000 / fee 30 ⇒ DR CASH_CLEARING 970 + DR PSP_FEE_EXPENSE 30
    // + CR UNALLOCATED 1000. Two allocations summing 600. Cache agrees.
    let acct = Uuid::from_u128(ACCT);
    let settle = Uuid::now_v7();
    let lines = vec![
        jl(settle, acct, "CASH_CLEARING", "DR", 970, None, None, None),
        jl(settle, acct, "PSP_FEE_EXPENSE", "DR", 30, None, None, None),
        jl(settle, acct, "UNALLOCATED", "CR", 1000, None, None, None),
    ];
    let entries = vec![settle_entry(settle, "PAY1")];
    let allocs = vec![
        alloc_row("PAY1", "INV1", 400),
        alloc_row("PAY1", "INV2", 200),
    ];
    let cache = vec![settlement_row("PAY1", 1000, 30, 600)];

    let v = recompute_payment_counter_variances(&entries, &lines, &allocs, &cache);
    assert!(v.is_empty(), "all three reconciled counters tie out: {v:?}");
}

#[test]
fn payment_counters_flag_each_diverged_counter() {
    // settled journal=1000 vs cache 900; fee journal=30 vs cache 30 (ok);
    // allocated rows=600 vs cache 550. ⇒ settled_minor + allocated_minor flagged.
    let acct = Uuid::from_u128(ACCT);
    let settle = Uuid::now_v7();
    let lines = vec![
        jl(settle, acct, "CASH_CLEARING", "DR", 970, None, None, None),
        jl(settle, acct, "PSP_FEE_EXPENSE", "DR", 30, None, None, None),
        jl(settle, acct, "UNALLOCATED", "CR", 1000, None, None, None),
    ];
    let entries = vec![settle_entry(settle, "PAY1")];
    let allocs = vec![alloc_row("PAY1", "INV1", 600)];
    let cache = vec![settlement_row("PAY1", 900, 30, 550)];

    let mut v = recompute_payment_counter_variances(&entries, &lines, &allocs, &cache);
    v.sort_by(|a, b| a.counter.cmp(b.counter));
    assert_eq!(
        v.len(),
        2,
        "settled + allocated diverge, fee ties out: {v:?}"
    );
    let counters: Vec<&str> = v.iter().map(|x| x.counter).collect();
    assert_eq!(counters, vec!["allocated_minor", "settled_minor"]);
    let settled = v.iter().find(|x| x.counter == "settled_minor").unwrap();
    assert_eq!((settled.computed, settled.cached), (1000, 900));
    let alloc = v.iter().find(|x| x.counter == "allocated_minor").unwrap();
    assert_eq!((alloc.computed, alloc.cached), (600, 550));
    assert!(v.iter().all(|x| x.payment_id == "PAY1"));
}

#[test]
fn payment_settled_and_fee_reconcile_skipped_when_tenant_has_a_settlement_return() {
    // A SETTLEMENT_RETURN in the tenant's journal makes BOTH `settled_minor` and
    // `fee_minor` un-mappable (Model N D1: the return reverses both on the same
    // psp_return_id-keyed entry, no payment_id), so BOTH reconciles are skipped
    // tenant-wide even though the cache differs from the gross settle journal.
    // `allocated_minor` still reconciles (the allocation rows are the truth).
    let acct = Uuid::from_u128(ACCT);
    let settle = Uuid::now_v7();
    let ret = Uuid::now_v7();
    let lines = vec![
        jl(settle, acct, "CASH_CLEARING", "DR", 970, None, None, None),
        jl(settle, acct, "PSP_FEE_EXPENSE", "DR", 30, None, None, None),
        jl(settle, acct, "UNALLOCATED", "CR", 1000, None, None, None),
        // The return legs (Model N symmetric reverse of a partial return): DR
        // UNALLOCATED 100 / CR CASH_CLEARING 97 / CR PSP_FEE_EXPENSE 3 (keyed by
        // psp_return_id; no payment_id) — present only to trip the gate.
        jl(ret, acct, "UNALLOCATED", "DR", 100, None, None, None),
        jl(ret, acct, "CASH_CLEARING", "CR", 97, None, None, None),
        jl(ret, acct, "PSP_FEE_EXPENSE", "CR", 3, None, None, None),
    ];
    let entries = vec![settle_entry(settle, "PAY1"), return_entry(ret, "RET1")];
    let allocs = vec![alloc_row("PAY1", "INV1", 600)];
    // Cache: settled decremented to 900 by the return AND fee decremented to 27
    // (30 − 3) — BOTH differ from the gross settle journal (1000 / 30). If either
    // reconcile ran, it would falsely flag; the gate must skip both.
    let cache = vec![settlement_row("PAY1", 900, 27, 600)];

    let v = recompute_payment_counter_variances(&entries, &lines, &allocs, &cache);
    assert!(
        v.iter().all(|x| x.counter != "settled_minor"),
        "settled_minor must be skipped when a SETTLEMENT_RETURN exists: {v:?}"
    );
    assert!(
        v.iter().all(|x| x.counter != "fee_minor"),
        "fee_minor must ALSO be skipped when a SETTLEMENT_RETURN exists (Model N): {v:?}"
    );
    assert!(
        v.is_empty(),
        "allocated ties out; settled + fee skipped: {v:?}"
    );
}

#[test]
fn payment_allocation_with_no_settlement_row_is_flagged() {
    // An allocation row whose payment has NO settlement counter row is an orphan
    // (the seed that should anchor it is missing) ⇒ allocated_minor variance with
    // cached = 0.
    let allocs = vec![alloc_row("GHOST", "INV1", 250)];
    let v = recompute_payment_counter_variances(&[], &[], &allocs, &[]);
    assert_eq!(v.len(), 1, "orphan allocation flagged: {v:?}");
    assert_eq!(v[0].payment_id, "GHOST");
    assert_eq!(v[0].counter, "allocated_minor");
    assert_eq!((v[0].computed, v[0].cached), (250, 0));
}

// ────────────────────────────────────────────────────────────────────────────
// VHP-1843 incremental tie-out — pure projection unit tests (no container).
// Exercise the `(grain, grain_key)` string-space helpers that the daily / recon
// incremental path shares with the close-time baseline snapshot.
// ────────────────────────────────────────────────────────────────────────────

/// A clean tenant verifies clean: `baseline (closed) + fold(open) == cache`.
#[test]
fn incremental_clean_when_baseline_plus_open_equals_cache() {
    let acc = Uuid::from_u128(0xC1);
    let e = Uuid::now_v7();
    // Open period: REVENUE net +100 (DR 300, CR 200) on the account grain.
    let open = vec![
        jl(e, acc, "REVENUE", "DR", 300, None, None, None),
        jl(e, acc, "REVENUE", "CR", 200, None, None, None),
    ];
    let fold = fold_grains(&open, &dr_sides(&[acc]));
    // Closed-period contribution carried by the baseline.
    let mut baseline = std::collections::HashMap::new();
    baseline.insert((GRAIN_ACCOUNT, key_account(acc, "USD")), 500_i64);
    // Cache (all-time) = 600 = baseline 500 + open 100.
    let cache = cache_grains(&[bal(0xC1, "REVENUE", 600)], &[], &[], &[], &[], &[]);
    assert!(
        verify_incremental(&baseline, &fold, &cache).is_empty(),
        "500 baseline + 100 open == 600 cache is clean"
    );
}

/// Baseline drift surfaces as a variance with `computed = baseline + open`.
#[test]
fn incremental_flags_baseline_drift() {
    let acc = Uuid::from_u128(0xC2);
    let e = Uuid::now_v7();
    let open = vec![jl(e, acc, "REVENUE", "DR", 100, None, None, None)];
    let fold = fold_grains(&open, &dr_sides(&[acc]));
    let mut baseline = std::collections::HashMap::new();
    baseline.insert((GRAIN_ACCOUNT, key_account(acc, "USD")), 500_i64);
    // Cache claims 700 but baseline(500) + open(100) = 600 → a 100 divergence.
    let cache = cache_grains(&[bal(0xC2, "REVENUE", 700)], &[], &[], &[], &[], &[]);
    let v = verify_incremental(&baseline, &fold, &cache);
    assert_eq!(v.len(), 1, "one grain diverges: {v:?}");
    assert_eq!((v[0].computed, v[0].cached), (600, 700));
}

/// A sub-grain (unallocated) folds + projects into the same key space and
/// verifies clean against a matching cache (empty baseline = an all-open tenant).
#[test]
fn incremental_subgrain_clean_with_matching_cache() {
    let acc = Uuid::from_u128(0xC5);
    let e = Uuid::now_v7();
    // An UNALLOCATED line touches the account grain AND the unallocated sub-grain.
    let lines = vec![jl(e, acc, "UNALLOCATED", "DR", 250, None, None, None)];
    let fold = fold_grains(&lines, &dr_sides(&[acc]));
    let cache = cache_grains(
        &[bal(0xC5, "UNALLOCATED", 250)],
        &[],
        &[],
        &[],
        &[unallocated_row(acc, 250)],
        &[],
    );
    assert!(
        verify_incremental(&std::collections::HashMap::new(), &fold, &cache).is_empty(),
        "matching account + unallocated grains verify clean"
    );
}

/// `cache_baseline_rows` round-trips a cache projection into baseline rows (the
/// close-time snapshot): same grain discriminator, key, and absolute balance.
#[test]
fn cache_baseline_rows_roundtrip() {
    let acc = Uuid::from_u128(0xC4);
    let cache = cache_grains(&[bal(0xC4, "REVENUE", 123)], &[], &[], &[], &[], &[]);
    let rows = cache_baseline_rows(&cache);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].grain, GRAIN_ACCOUNT);
    assert_eq!(rows[0].grain_key, key_account(acc, "USD"));
    assert_eq!(rows[0].balance_minor, 123);
}

/// VHP-1843 (PG) — the incremental tie-out equals the full fold across a CLOSED +
/// OPEN period split: post in period 1, close it + snapshot the baseline (what
/// period-close does), post in period 2; the incremental path (baseline[p1] +
/// open-fold[p2]) must reconcile clean and agree with the full all-time fold.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn incremental_tie_out_equals_full_across_periods() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // Post in period 1 (the fixture's 202606).
    let (e1, l1) = balanced_entry(&f, "biz-p1", 1000);
    service.post(&ctx, &scope, e1, l1, None).await.unwrap();

    // Close period 1 and snapshot its verified baseline (mirrors period-close).
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_fiscal_period SET status='CLOSED' \
         WHERE tenant_id='{}' AND period_id='{}'",
        f.tenant, f.period_id
    )))
    .await
    .unwrap();
    let conn = provider.conn().unwrap();
    let job = TieOutJob::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    job.snapshot_baseline(&conn, f.tenant, &f.period_id)
        .await
        .expect("snapshot baseline at close");

    // Open period 2 and post into it.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status) \
         VALUES ('{}','{}','202607','UTC','OPEN')",
        f.tenant, f.legal_entity
    )))
    .await
    .unwrap();
    let (mut e2, l2) = balanced_entry(&f, "biz-p2", 500);
    e2.period_id = "202607".to_owned();
    e2.effective_at = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
    service.post(&ctx, &scope, e2, l2, None).await.unwrap();

    // Incremental reconciles clean: baseline[p1] + open-fold[p2] == cache[all].
    let inc = job
        .tie_out_incremental(&conn, f.tenant)
        .await
        .unwrap()
        .expect("a stored baseline ⇒ the incremental path runs");
    assert!(
        inc.is_clean(),
        "incremental clean on consistent books: {:?}",
        inc.sub_grain_variances
    );
    // The open fold covered only period 2's lines, not all-time.
    assert!(inc.open_line_count >= 2, "period-2 lines folded: {inc:?}");

    // And it agrees with the full all-time fold.
    let full = job.tie_out_on(&conn, f.tenant).await.unwrap();
    assert!(full.is_clean(), "the full fold is clean too");
}

// ────────────────────────────────────────────────────────────────────────────
// Whole-slice reference wrappers over the streaming accumulators. Test-only
// (DE1101: test code lives in the companion, not the production file); they lock
// the fold/finalize semantics the paginated production path relies on.
// ────────────────────────────────────────────────────────────────────────────

/// Whole-slice reference wrapper over [`SubGrainAcc`].
fn recompute_sub_grain_variances(
    lines: &[journal_line::Model],
    normal_side_map: &HashMap<Uuid, String>,
    ar_payer_cache: &[ar_payer_balance::Model],
    ar_invoice_cache: &[ar_invoice_balance::Model],
    tax_cache: &[tax_subbalance::Model],
    unallocated_cache: &[unallocated_balance::Model],
    reusable_credit_cache: &[reusable_credit_subbalance::Model],
) -> Vec<SubGrainVariance> {
    let mut acc = SubGrainAcc::default();
    acc.fold(lines, normal_side_map);
    acc.finalize(
        ar_payer_cache,
        ar_invoice_cache,
        tax_cache,
        unallocated_cache,
        reusable_credit_cache,
    )
}

/// Whole-slice reference wrapper over [`PaymentCounterAcc`].
fn recompute_payment_counter_variances(
    entries: &[journal_entry::Model],
    lines: &[journal_line::Model],
    allocations: &[payment_allocation::Model],
    settlement_cache: &[payment_settlement::Model],
) -> Vec<PaymentCounterVariance> {
    let (settle_payment_by_entry, has_settlement_return) = settle_index(entries);
    let mut acc = PaymentCounterAcc::default();
    acc.fold(lines, &settle_payment_by_entry);
    acc.finalize(allocations, settlement_cache, has_settlement_return)
}

/// Whole-slice reference wrapper over [`EntryBackstopAcc`].
fn entry_backstop(lines: &[journal_line::Model]) -> Vec<ImbalancedEntry> {
    let mut acc = EntryBackstopAcc::default();
    acc.fold(lines);
    acc.finalize()
}
