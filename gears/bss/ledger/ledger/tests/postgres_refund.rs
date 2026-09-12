//! Postgres-only integration: the Slice-3 Phase-2 `RefundHandler` (Group B), driven
//! through the REAL foundation engine (`PostingService` + the in-txn
//! `RefundPostSidecar`) against a real settled payment posted by
//! `SettlementService` (the receipt the refund unwinds). Asserts the design §4.4 /
//! §8 durable effects:
//!
//! - **Pattern A two-stage** (`A_UNALLOCATED`): stage-1 `initiated` posts DR
//!   `UNALLOCATED` · CR `REFUND_CLEARING` (the clearing balance opens); stage-2
//!   `confirmed` posts DR `REFUND_CLEARING` · CR `CASH_CLEARING` — and the
//!   `REFUND_CLEARING` balance **drains back to zero**;
//! - **Pattern B two-stage** (`B_RESTORE_AR`): stage-1 posts DR `AR` (re-opens the
//!   receivable) · CR `REFUND_CLEARING`; stage-2 drains `REFUND_CLEARING` →
//!   `CASH_CLEARING`;
//! - a refund **NEVER touches `CONTRACT_LIABILITY`** (no such account balance row is
//!   ever created);
//! - the `refund` record rows persist (one per phase);
//! - a refund against a **payment with no settlement** is `RefundOriginNotFound`
//!   (404, no books effect); a **currency mismatch** is `CurrencyMismatch`;
//! - a **replay** of the same `(psp_refund_id, phase)` is idempotent (no second
//!   books effect).
//!
//! `RefundHandler::new` is `pub`, so this out-of-crate test drives it +
//! `SettlementService` directly (mirrors `postgres_credit_note.rs` /
//! `postgres_chargebacks.rs`). Ignored by default; run with `-- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::too_many_arguments
)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bss_ledger::domain::adjustment::refund::{
    RefundDirection, RefundPattern, RefundPhase, RefundRequest,
};
use bss_ledger::domain::approval::intent::ApprovalIntent;
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::adjustment::refund_service::{RefundHandler, RefundOutcome};
use bss_ledger::infra::approval::service::{ApprovalExecutor, ApprovalService};
use bss_ledger::infra::audit::secured_audit_sink::{AuditEventType, SecuredAuditSink};
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, Side};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_gts::gts_id;
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

/// Provisioned seller for the refund flow: the chart classes a refund touches —
/// `UNALLOCATED` (the settle pool, Pattern A debit), `AR` (Pattern B restore),
/// `REFUND_CLEARING` (the two-stage clearing), `CASH_CLEARING` (the disbursement) —
/// plus `PSP_FEE_EXPENSE` for the settle fee leg.
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    unallocated: Uuid,
    refund_clearing: Uuid,
    ar: Uuid,
    psp_fee: Uuid,
    suspense: Uuid,
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

/// Boot, migrate, seed USD@2 + an OPEN period (current month) + the refund chart.
async fn setup(url: &str) -> (DatabaseConnection, DBProvider<DbError>, Seller) {
    let raw = Database::connect(url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        refund_clearing: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
        suspense: Uuid::now_v7(),
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
            s.refund_clearing,
            AccountClass::RefundClearing,
            Side::Credit,
        ),
        account(s.tenant, s.ar, AccountClass::Ar, Side::Debit),
        account(
            s.tenant,
            s.psp_fee,
            AccountClass::PspFeeExpense,
            Side::Debit,
        ),
        // The unknown_final park target — SUSPENSE holds the stuck clearing amount
        // (credit-normal, mirroring the REFUND_CLEARING obligation it drains) until
        // a terminal disposition reconciles it (Slice 7).
        account(s.tenant, s.suspense, AccountClass::Suspense, Side::Credit),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    (raw, provider, s)
}

fn settle_svc(provider: &DBProvider<DbError>) -> SettlementService {
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

fn refund_handler(provider: &DBProvider<DbError>) -> RefundHandler {
    RefundHandler::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()))
}

/// Settle `gross` (fee 0) for `payment_id` — seeds the `payment_settlement` row
/// (`settled_minor = gross`) the refund resolves as its origin.
async fn settle(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str, gross: i64) {
    settle_svc(provider)
        .settle(
            &SecurityContext::anonymous(),
            &AccessScope::for_tenant(s.tenant),
            SettlementInput {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: payment_id.to_owned(),
                gross_minor: gross,
                fee_minor: 0,
                currency: "USD".to_owned(),
                effective_at: None,
            },
        )
        .await
        .expect("settle must succeed");
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

/// `refund` row count for a `(psp_refund_id, phase)`.
async fn refund_rows(raw: &DatabaseConnection, s: &Seller, psp: &str, phase: &str) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_refund \
             WHERE tenant_id='{}' AND psp_refund_id='{psp}' AND phase='{phase}'",
            s.tenant
        ),
    )
    .await
}

/// `true` iff ANY CONTRACT_LIABILITY balance row exists for the seller — a refund
/// must never create one (design §4.4).
async fn any_contract_liability(raw: &DatabaseConnection, s: &Seller) -> i64 {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_account_balance b \
             JOIN bss.ledger_tenant_account a ON a.account_id = b.account_id \
             WHERE b.tenant_id='{}' AND a.account_class='CONTRACT_LIABILITY'",
            s.tenant
        ),
    )
    .await
    .unwrap_or(0)
}

/// Read a `payment_settlement` counter column for `(tenant, payment_id)` — the cap
/// basis Group C maintains.
async fn settlement_counter(
    raw: &DatabaseConnection,
    s: &Seller,
    payment_id: &str,
    col: &str,
) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT {col} FROM bss.ledger_payment_settlement \
             WHERE tenant_id='{}' AND payment_id='{payment_id}'",
            s.tenant
        ),
    )
    .await
}

/// Read the `payment_allocation_refund.refunded_minor` for a `(payment, invoice)` —
/// the Pattern-B per-invoice cap counter.
async fn allocation_refunded(
    raw: &DatabaseConnection,
    s: &Seller,
    payment_id: &str,
    invoice_id: &str,
) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT refunded_minor FROM bss.ledger_payment_allocation_refund \
             WHERE tenant_id='{}' AND payment_id='{payment_id}' AND invoice_id='{invoice_id}'",
            s.tenant
        ),
    )
    .await
}

/// The `clearing_state` of a `(psp_refund_id, phase)` refund row.
async fn refund_clearing_state(
    raw: &DatabaseConnection,
    s: &Seller,
    psp: &str,
    phase: &str,
) -> Option<String> {
    raw.query_one_raw(pg(format!(
        "SELECT clearing_state FROM bss.ledger_refund \
         WHERE tenant_id='{}' AND psp_refund_id='{psp}' AND phase='{phase}'",
        s.tenant
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<String>(0).unwrap())
}

/// Seed a `payment_allocation_refund` row directly (the per-`(payment, invoice)`
/// cap basis a Pattern-B refund draws against) — `allocated_minor = allocated`,
/// `refunded_minor = 0`. Stands in for the allocation that would have applied this
/// payment to the invoice (the `AllocationSidecar` seeds this row in the real flow;
/// seeding it directly keeps the refund cap test self-contained).
async fn seed_allocation_refund(
    raw: &DatabaseConnection,
    s: &Seller,
    payment_id: &str,
    invoice_id: &str,
    allocated: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_payment_allocation_refund \
         (tenant_id, payment_id, invoice_id, allocated_minor, refunded_minor, version) \
         VALUES ('{}','{payment_id}','{invoice_id}',{allocated},0,0)",
        s.tenant
    )))
    .await
    .unwrap();
}

/// Bump `payment_settlement.allocated_minor` directly to model a prior allocation
/// of `amount` from the pool (so a Pattern-A refund's `refunded_unallocated` cap +
/// the spendable-headroom CHECK have a non-trivial allocated base). Mirrors what
/// `add_allocated` does, without wiring the whole allocation flow.
async fn bump_allocated(raw: &DatabaseConnection, s: &Seller, payment_id: &str, amount: i64) {
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_payment_settlement \
         SET allocated_minor = allocated_minor + {amount}, version = version + 1 \
         WHERE tenant_id='{}' AND payment_id='{payment_id}'",
        s.tenant
    )))
    .await
    .unwrap();
}

fn refund_req(
    s: &Seller,
    refund_id: &str,
    psp_refund_id: &str,
    payment_id: &str,
    pattern: RefundPattern,
    phase: RefundPhase,
    invoice_id: Option<&str>,
    amount: i64,
) -> RefundRequest {
    RefundRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        refund_id: refund_id.to_owned(),
        psp_refund_id: psp_refund_id.to_owned(),
        phase,
        pattern,
        payment_id: payment_id.to_owned(),
        invoice_id: invoice_id.map(ToOwned::to_owned),
        currency: "USD".to_owned(),
        amount_minor: amount,
        two_stage: true,
        // First-order OUTBOUND refund by default; the refund-of-refund tests build
        // claw-backs via `clawback_req`.
        relates_to_refund_id: None,
        direction: RefundDirection::Outbound,
    }
}

/// A refund-of-refund CLAW-BACK request: references a prior refund + the `Clawback`
/// direction (so the legs invert and the money-out counters DECREMENT, under the
/// underflow guard). Group E. Mirrors `refund_req`'s builder shape.
#[allow(clippy::too_many_arguments)]
fn clawback_req(
    s: &Seller,
    refund_id: &str,
    psp_refund_id: &str,
    payment_id: &str,
    pattern: RefundPattern,
    phase: RefundPhase,
    invoice_id: Option<&str>,
    amount: i64,
    relates_to: &str,
) -> RefundRequest {
    RefundRequest {
        relates_to_refund_id: Some(relates_to.to_owned()),
        direction: RefundDirection::Clawback,
        ..refund_req(
            s,
            refund_id,
            psp_refund_id,
            payment_id,
            pattern,
            phase,
            invoice_id,
            amount,
        )
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pattern_a_two_stage_drains_refund_clearing_to_zero() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle 1000 → UNALLOCATED holds 1000 (CR), CASH_CLEARING holds 1000 (DR).
    settle(&provider, &s, "PAY-A", 1000).await;
    assert_eq!(bal(&raw, &s, s.unallocated).await, Some(1000));
    assert_eq!(bal(&raw, &s, s.cash).await, Some(1000));

    // Stage-1 initiated: DR UNALLOCATED 300 · CR REFUND_CLEARING 300.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-A1",
                "PSP-A",
                "PAY-A",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("stage-1 posts");
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(700),
        "UNALLOCATED drawn down by the refund"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(300),
        "stage-1 opens the REFUND_CLEARING balance"
    );
    assert_eq!(refund_rows(&raw, &s, "PSP-A", "initiated").await, Some(1));

    // Stage-2 confirmed: DR REFUND_CLEARING 300 · CR CASH_CLEARING 300.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-A2",
                "PSP-A",
                "PAY-A",
                RefundPattern::AUnallocated,
                RefundPhase::Confirmed,
                None,
                300,
            ),
        )
        .await
        .expect("stage-2 posts");
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(0),
        "stage-2 drains REFUND_CLEARING back to zero"
    );
    assert_eq!(
        bal(&raw, &s, s.cash).await,
        Some(700),
        "CASH_CLEARING reduced by the disbursed cash (1000 − 300)"
    );
    assert_eq!(refund_rows(&raw, &s, "PSP-A", "confirmed").await, Some(1));
    assert_eq!(
        any_contract_liability(&raw, &s).await,
        0,
        "a refund must never touch CONTRACT_LIABILITY"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pattern_b_two_stage_restores_ar_then_drains_clearing() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-B", 1000).await;
    // Pattern B's per-(payment, invoice) cap reads the payment_allocation_refund
    // row that allocation seeds — the (PAY-B, INV-9) pair was allocated 400 (the
    // restored-AR refund's cap basis), mirroring `pattern_b_per_invoice_cap`.
    seed_allocation_refund(&raw, &s, "PAY-B", "INV-9", 400).await;

    // Stage-1 initiated (Pattern B): DR AR 400 (re-open the receivable) · CR
    // REFUND_CLEARING 400.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-B1",
                "PSP-B",
                "PAY-B",
                RefundPattern::BRestoreAr,
                RefundPhase::Initiated,
                Some("INV-9"),
                400,
            ),
        )
        .await
        .expect("stage-1 posts");
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(400),
        "AR restored (the receivable re-opens)"
    );
    assert_eq!(bal(&raw, &s, s.refund_clearing).await, Some(400));

    // Stage-2 confirmed drains REFUND_CLEARING → CASH_CLEARING.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-B2",
                "PSP-B",
                "PAY-B",
                RefundPattern::BRestoreAr,
                RefundPhase::Confirmed,
                Some("INV-9"),
                400,
            ),
        )
        .await
        .expect("stage-2 posts");
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(0),
        "stage-2 drains REFUND_CLEARING to zero"
    );
    assert_eq!(
        bal(&raw, &s, s.cash).await,
        Some(600),
        "cash disbursed (1000 − 400)"
    );
    assert_eq!(
        any_contract_liability(&raw, &s).await,
        0,
        "Pattern B never touches CONTRACT_LIABILITY"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_against_unsettled_payment_is_origin_not_found() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // No settle for PAY-NONE ⇒ no payment_settlement ⇒ RefundOriginNotFound (404).
    let err = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-NF",
                "PSP-NF",
                "PAY-NONE",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                100,
            ),
        )
        .await
        .expect_err("a refund against an unsettled payment must be rejected");
    assert!(
        matches!(err, DomainError::RefundOriginNotFound(_)),
        "expected RefundOriginNotFound, got {err:?}"
    );
    // No books / record effect.
    assert_eq!(refund_rows(&raw, &s, "PSP-NF", "initiated").await, Some(0));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_currency_mismatch_is_rejected() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-CUR", 1000).await; // settled in USD

    let mut req = refund_req(
        &s,
        "RF-CUR",
        "PSP-CUR",
        "PAY-CUR",
        RefundPattern::AUnallocated,
        RefundPhase::Initiated,
        None,
        100,
    );
    req.currency = "EUR".to_owned(); // refund currency ≠ the USD settlement
    let err = refund_handler(&provider)
        .post_refund(&ctx, &scope, req)
        .await
        .expect_err("a currency-mismatched refund must be rejected");
    assert!(
        matches!(err, DomainError::CurrencyMismatch(_)),
        "expected CurrencyMismatch, got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_stage_is_idempotent_on_psp_phase() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-IDEM", 1000).await;

    let req = || {
        refund_req(
            &s,
            "RF-IDEM",
            "PSP-IDEM",
            "PAY-IDEM",
            RefundPattern::AUnallocated,
            RefundPhase::Initiated,
            None,
            250,
        )
    };
    let first = refund_handler(&provider)
        .post_refund(&ctx, &scope, req())
        .await
        .expect("first post");
    assert!(!first.replayed, "first post is fresh");
    assert_eq!(bal(&raw, &s, s.refund_clearing).await, Some(250));

    // Replay the SAME (psp_refund_id, phase) ⇒ idempotent: the engine returns the
    // prior posting, the sidecar does not run again, and the books are unchanged.
    let replay = refund_handler(&provider)
        .post_refund(&ctx, &scope, req())
        .await
        .expect("replay returns the prior posting");
    assert!(replay.replayed, "second post is a replay");
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(250),
        "no second books effect on replay"
    );
    assert_eq!(
        refund_rows(&raw, &s, "PSP-IDEM", "initiated").await,
        Some(1),
        "exactly one refund row (the replay did not insert a second)"
    );
}

// ---------------------------------------------------------------------------
// Group C — caps at stage-1 + the stage-1 reversal (rejected/voided).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stage1_initiation_reserves_refunded_cap_both_patterns() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-CAP", 1000).await;
    // Pattern A stage-1: reserves refunded_minor AND refunded_unallocated_minor.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-CAP1",
                "PSP-CAP",
                "PAY-CAP",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("stage-1 reserves the cap");
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CAP", "refunded_minor").await,
        Some(300),
        "stage-1 bumps total money-out refunded_minor"
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CAP", "refunded_unallocated_minor").await,
        Some(300),
        "Pattern A also bumps the spendable-headroom refunded_unallocated_minor"
    );

    // Stage-2 confirmed must NOT move the counters (the cash was capped at stage-1).
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-CAP2",
                "PSP-CAP",
                "PAY-CAP",
                RefundPattern::AUnallocated,
                RefundPhase::Confirmed,
                None,
                300,
            ),
        )
        .await
        .expect("stage-2 drains clearing");
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CAP", "refunded_minor").await,
        Some(300),
        "stage-2 confirmed leaves refunded_minor unchanged (no double count)"
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CAP", "refunded_unallocated_minor").await,
        Some(300),
        "stage-2 confirmed leaves refunded_unallocated_minor unchanged"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stage1_over_settled_is_refund_exceeds_settled() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-OVER", 1000).await;
    // A stage-1 refund of 1500 > 1000 settled: the rank-1 cap CHECK rejects it
    // BEFORE any cash leaves — REFUND_EXCEEDS_SETTLED, no books / record effect.
    let err = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-OVER",
                "PSP-OVER",
                "PAY-OVER",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                1500,
            ),
        )
        .await
        .expect_err("an over-settled stage-1 refund must be rejected by the cap");
    assert!(
        matches!(err, DomainError::RefundExceedsSettled(_)),
        "expected RefundExceedsSettled, got {err:?}"
    );
    // The whole post rolled back: no refund row, no REFUND_CLEARING balance, cap 0.
    assert_eq!(
        refund_rows(&raw, &s, "PSP-OVER", "initiated").await,
        Some(0)
    );
    assert_eq!(bal(&raw, &s, s.refund_clearing).await, None);
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-OVER", "refunded_minor").await,
        Some(0),
        "the rejected reservation left refunded_minor at 0"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pattern_a_refund_consumes_unallocated_headroom() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-HR", 1000).await;
    // Model a prior allocation of 800 from the pool (spendable headroom now 200).
    bump_allocated(&raw, &s, "PAY-HR", 800).await;

    // A Pattern-A refund of 200 fits the spendable headroom (allocated 800 +
    // refunded_unallocated 200 = 1000 <= settled 1000).
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-HR1",
                "PSP-HR1",
                "PAY-HR",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                200,
            ),
        )
        .await
        .expect("refund within spendable headroom succeeds");

    // A further Pattern-A refund of even 1 would push allocated 800 +
    // refunded_unallocated 201 = 1001 > 1000 ⇒ the spendable-headroom CHECK rejects
    // it: the refunded on-account cash can no longer also be allocated.
    let err = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-HR2",
                "PSP-HR2",
                "PAY-HR",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                1,
            ),
        )
        .await
        .expect_err("a refund past the spendable headroom must be rejected");
    assert!(
        matches!(err, DomainError::RefundExceedsSettled(_)),
        "expected RefundExceedsSettled (spendable headroom), got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pattern_b_per_invoice_cap_blocks_over_allocated() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-PB", 1000).await;
    // The (PAY-PB, INV-PB) pair was allocated 400 (the per-invoice cap basis).
    seed_allocation_refund(&raw, &s, "PAY-PB", "INV-PB", 400).await;

    // A Pattern-B refund of 400 fits the per-invoice cap (refunded 400 <= allocated
    // 400) and bumps payment_allocation_refund.refunded_minor.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-PB1",
                "PSP-PB1",
                "PAY-PB",
                RefundPattern::BRestoreAr,
                RefundPhase::Initiated,
                Some("INV-PB"),
                400,
            ),
        )
        .await
        .expect("Pattern-B refund within the per-invoice cap succeeds");
    assert_eq!(
        allocation_refunded(&raw, &s, "PAY-PB", "INV-PB").await,
        Some(400),
        "Pattern B bumps the per-(payment, invoice) refunded_minor"
    );

    // A further Pattern-B refund of 1 ⇒ refunded 401 > 400 allocated ⇒ the
    // per-invoice cap CHECK rejects it as REFUND_EXCEEDS_ALLOCATED.
    let err = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-PB2",
                "PSP-PB2",
                "PAY-PB",
                RefundPattern::BRestoreAr,
                RefundPhase::Initiated,
                Some("INV-PB"),
                1,
            ),
        )
        .await
        .expect_err("a Pattern-B refund past the allocated amount must be rejected");
    assert!(
        matches!(err, DomainError::RefundExceedsAllocated(_)),
        "expected RefundExceedsAllocated, got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn rejected_stage1_reverses_and_frees_cap() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-REJ", 1000).await;

    // Stage-1 Pattern A initiated: DR UNALLOCATED 350 · CR REFUND_CLEARING 350, cap
    // reserved.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-REJ1",
                "PSP-REJ",
                "PAY-REJ",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                350,
            ),
        )
        .await
        .expect("stage-1 posts + reserves cap");
    assert_eq!(bal(&raw, &s, s.unallocated).await, Some(650));
    assert_eq!(bal(&raw, &s, s.refund_clearing).await, Some(350));
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-REJ", "refunded_minor").await,
        Some(350)
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-REJ", "refunded_unallocated_minor").await,
        Some(350)
    );

    // PSP rejected the initiated refund: the stage-1 reversal line-negates
    // (DR REFUND_CLEARING 350 · CR UNALLOCATED 350), decrements the caps, and drains
    // REFUND_CLEARING to zero.
    let posting = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-REJ2",
                "PSP-REJ",
                "PAY-REJ",
                RefundPattern::AUnallocated,
                RefundPhase::Rejected,
                None,
                350,
            ),
        )
        .await
        .expect("stage-1 reversal posts");
    assert!(!posting.replayed, "the reversal is a fresh post");

    // REFUND_CLEARING drained to zero; UNALLOCATED restored to the pre-refund 1000.
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(0),
        "the reversal drains REFUND_CLEARING to zero"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(1000),
        "the reversal restores the drawn-down UNALLOCATED pool"
    );
    // The caps are released back to the pre-initiation 0 (the cap is freed).
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-REJ", "refunded_minor").await,
        Some(0),
        "the reversal frees refunded_minor back to 0"
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-REJ", "refunded_unallocated_minor").await,
        Some(0),
        "the reversal frees refunded_unallocated_minor back to 0"
    );
    // The reversal refund row is REVERSED and links the stage-1 entry.
    assert_eq!(
        refund_clearing_state(&raw, &s, "PSP-REJ", "rejected").await,
        Some("REVERSED".to_owned()),
        "the rejected refund row is clearing_state = REVERSED"
    );
    let reverses = scalar_i64(
        &raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_refund \
             WHERE tenant_id='{}' AND psp_refund_id='PSP-REJ' AND phase='rejected' \
             AND reverses_entry_id IS NOT NULL",
            s.tenant
        ),
    )
    .await;
    assert_eq!(
        reverses,
        Some(1),
        "the rejected refund row carries the stage-1 reverses_entry_id"
    );

    // The cap is now fully re-opened: a fresh full refund of 1000 succeeds.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-REJ3",
                "PSP-REJ-NEW",
                "PAY-REJ",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                1000,
            ),
        )
        .await
        .expect("the freed cap admits a fresh full refund");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reject_without_stage1_is_invalid_request() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-NOSTG", 1000).await;
    // A `rejected` with NO prior stage-1 `initiated` entry has nothing to reverse —
    // an upstream contract violation, rejected as InvalidRequest (no books effect).
    let err = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-NOSTG",
                "PSP-NOSTG",
                "PAY-NOSTG",
                RefundPattern::AUnallocated,
                RefundPhase::Rejected,
                None,
                100,
            ),
        )
        .await
        .expect_err("a reject with no stage-1 to reverse must be rejected");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
    assert_eq!(
        refund_rows(&raw, &s, "PSP-NOSTG", "rejected").await,
        Some(0)
    );
}

// ─────────────────────────── dual-control (Group D) ───────────────────────────
//
// A refund whose returned cash crosses the tenant's D2 threshold (default
// $1000 = 100_000 minor; no tenant policy row ⇒ ratified defaults) must NOT post
// inline — it parks a PENDING approval and returns `DualControlRequired` (409). A
// SECOND actor's approve then re-drives the held refund through the executor
// (`post_refund_approved`), moving the books exactly once; the preparer cannot
// approve their own (distinct-approver CHECK). A below-threshold refund posts
// inline, unchanged. Mirrors `postgres_dual_control.rs` but against the REAL
// refund-posting path (chart + settled origin), so it asserts the books actually
// move on approve and the caps are taken.

/// An `ApprovalExecutor` that replays the held refund through the un-gated
/// `RefundHandler` (`post_refund_approved`) — the real Group-D executor arm, minus
/// the `LedgerClientV1`/`InvoicePoster` wiring the full `LedgerApprovalExecutor`
/// needs. Counts executions so a test can assert "posted exactly once".
#[derive(Clone)]
struct RefundReplayExecutor {
    refund: Arc<RefundHandler>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ApprovalExecutor for RefundReplayExecutor {
    async fn execute(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        intent: &ApprovalIntent,
    ) -> Result<(), DomainError> {
        match intent {
            ApprovalIntent::Refund(i) => {
                let req = RefundRequest::try_from(i)?;
                self.refund.post_refund_approved(ctx, scope, req).await?;
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            other => Err(DomainError::Internal(format!(
                "unexpected intent in refund replay test: {other:?}"
            ))),
        }
    }
}

/// An authed `SecurityContext` for `subject` in `tenant` (the dual-control engine
/// reads `subject_id` / `subject_tenant_id` for the preparer/approver identity).
fn dc_ctx(subject: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

/// Build the gated handler + the approval service sharing the un-gated replay
/// handler over the same provider. Returns `(gated_handler, approval_service,
/// executor)`.
fn dual_control_wiring(
    provider: &DBProvider<DbError>,
) -> (RefundHandler, Arc<ApprovalService>, RefundReplayExecutor) {
    // The un-gated handler the executor replays through (never re-gates).
    let replay_handler = Arc::new(RefundHandler::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
    ));
    let exec = RefundReplayExecutor {
        refund: Arc::clone(&replay_handler),
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let svc = Arc::new(ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    ));
    // The GATED handler the preparer hits: same db, dual-control attached.
    let gated = RefundHandler::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()))
        .with_approval(Arc::clone(&svc));
    (gated, svc, exec)
}

/// `count(*)` of PENDING REFUND approvals for the tenant.
async fn pending_refund_approvals(raw: &DatabaseConnection, s: &Seller) -> i64 {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_approval \
             WHERE tenant_id='{}' AND kind='REFUND' AND state='PENDING'",
            s.tenant
        ),
    )
    .await
    .unwrap_or(0)
}

/// Above D2: an over-threshold refund does NOT post — it returns
/// `DualControlRequired` and creates exactly one PENDING REFUND approval (no books
/// effect yet). A second actor approves → the held refund posts (books move, the
/// `refund` row + cap land), exactly once.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_over_threshold_gates_then_a_second_actor_approve_posts_it() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let scope = AccessScope::for_tenant(s.tenant);
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();

    // Settle 200_000 so a 150_000 (> 100_000 D2) Pattern-A refund has headroom.
    settle(&provider, &s, "PAY-DC", 200_000).await;

    let (gated, svc, exec) = dual_control_wiring(&provider);

    // Preparer attempts a 150_000 refund → gated (above the 100_000 default D2).
    let err = gated
        .post_refund(
            &dc_ctx(preparer, s.tenant),
            &scope,
            refund_req(
                &s,
                "RF-DC",
                "PSP-DC",
                "PAY-DC",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                150_000,
            ),
        )
        .await
        .expect_err("an over-threshold refund must gate, not post");
    assert!(
        matches!(err, DomainError::DualControlRequired(_)),
        "expected DualControlRequired (409), got {err:?}"
    );
    assert_eq!(
        pending_refund_approvals(&raw, &s).await,
        1,
        "the gate created exactly one PENDING REFUND approval"
    );
    // No books moved yet (UNALLOCATED still holds the full settled 200_000).
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(200_000),
        "gating must not move the books"
    );
    assert_eq!(refund_rows(&raw, &s, "PSP-DC", "initiated").await, Some(0));

    // Resolve the PENDING approval id (the only one for the tenant).
    let approval_id = svc
        .list(
            &dc_ctx(approver, s.tenant),
            &scope,
            Some("PENDING"),
            Some("REFUND"),
        )
        .await
        .expect("list approvals")
        .first()
        .map(|m| m.approval_id)
        .expect("one pending approval");

    // A SECOND actor approves → the held refund posts (execute-then-mark).
    svc.approve(&dc_ctx(approver, s.tenant), &scope, approval_id)
        .await
        .expect("approve");

    assert_eq!(
        exec.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the held refund posts exactly once"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(50_000),
        "approve drew UNALLOCATED down by the refund (200_000 − 150_000)"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(150_000),
        "the stage-1 REFUND_CLEARING balance opened on approve"
    );
    assert_eq!(refund_rows(&raw, &s, "PSP-DC", "initiated").await, Some(1));
    // The money-out cap was taken (refunded_minor bumped under the settlement lock).
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-DC", "refunded_minor").await,
        Some(150_000),
        "the stage-1 reservation moved the money-out cap on approve"
    );
    // The approval is now APPROVED, approver stamped.
    let row = svc
        .get(&dc_ctx(approver, s.tenant), &scope, approval_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "APPROVED");
    assert_eq!(row.approved_by, Some(approver));
}

/// The preparer cannot approve their OWN over-threshold refund: the distinct-actor
/// rule rejects it (`SelfApprovalForbidden`) and the held refund never posts.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preparer_cannot_self_approve_their_refund() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let scope = AccessScope::for_tenant(s.tenant);
    let preparer = Uuid::now_v7();

    settle(&provider, &s, "PAY-SELF", 200_000).await;
    let (gated, svc, exec) = dual_control_wiring(&provider);

    let err = gated
        .post_refund(
            &dc_ctx(preparer, s.tenant),
            &scope,
            refund_req(
                &s,
                "RF-SELF",
                "PSP-SELF",
                "PAY-SELF",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                150_000,
            ),
        )
        .await
        .expect_err("over-threshold gates");
    assert!(
        matches!(err, DomainError::DualControlRequired(_)),
        "got {err:?}"
    );

    let approval_id = svc
        .list(
            &dc_ctx(preparer, s.tenant),
            &scope,
            Some("PENDING"),
            Some("REFUND"),
        )
        .await
        .expect("list")
        .first()
        .map(|m| m.approval_id)
        .expect("one pending");

    // The PREPARER approves their own → forbidden, nothing posts.
    let err = svc
        .approve(&dc_ctx(preparer, s.tenant), &scope, approval_id)
        .await
        .expect_err("self-approval forbidden");
    assert!(
        matches!(err, DomainError::SelfApprovalForbidden(_)),
        "got {err:?}"
    );
    assert_eq!(
        exec.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a forbidden self-approval must not post the refund"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(200_000),
        "the books are untouched"
    );
    let row = svc
        .get(&dc_ctx(preparer, s.tenant), &scope, approval_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "PENDING", "the refund approval is still pending");
}

/// Below D2: an under-threshold refund posts INLINE through the gated handler —
/// no approval row, books move immediately (the gate returns `None`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_under_threshold_posts_inline_without_an_approval() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let scope = AccessScope::for_tenant(s.tenant);
    let preparer = Uuid::now_v7();

    settle(&provider, &s, "PAY-SMALL", 200_000).await;
    let (gated, _svc, _exec) = dual_control_wiring(&provider);

    // 50_000 < 100_000 D2 → inline.
    gated
        .post_refund(
            &dc_ctx(preparer, s.tenant),
            &scope,
            refund_req(
                &s,
                "RF-SMALL",
                "PSP-SMALL",
                "PAY-SMALL",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                50_000,
            ),
        )
        .await
        .expect("a below-threshold refund posts inline");
    assert_eq!(
        pending_refund_approvals(&raw, &s).await,
        0,
        "no approval is created below threshold"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(150_000),
        "the inline refund drew UNALLOCATED down immediately (200_000 − 50_000)"
    );
    assert_eq!(
        refund_rows(&raw, &s, "PSP-SMALL", "initiated").await,
        Some(1)
    );
}

// ---------------------------------------------------------------------------
// Group E — refund-of-refund (claw-back vs additional-outbound) + out-of-order
// claw-back defer/retry/escalate (design §4.4 / Rev3 / S3-F1).
// ---------------------------------------------------------------------------

/// Count REFUND_CLAWBACK queue rows for the tenant in a given status.
async fn clawback_queue_rows(raw: &DatabaseConnection, s: &Seller, status: &str) -> i64 {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_pending_event_queue \
             WHERE tenant_id='{}' AND flow='REFUND_CLAWBACK' AND status='{status}'",
            s.tenant
        ),
    )
    .await
    .unwrap_or(0)
}

/// Force a REFUND_CLAWBACK queue row to look aged: backdate `queued_at` well past
/// the 7-day aging horizon AND clear `apply_after` so it is immediately claimable.
async fn age_clawback_row(raw: &DatabaseConnection, s: &Seller) {
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_pending_event_queue \
         SET queued_at = now() - interval '30 days', apply_after = NULL \
         WHERE tenant_id='{}' AND flow='REFUND_CLAWBACK'",
        s.tenant
    )))
    .await
    .unwrap();
}

/// Claw-back AFTER a matching outbound refund stage-1: the decrement nets
/// `refunded_minor` back down (so the total money-out cap reflects the NET refunded
/// and does not falsely trip), and REFUND_CLEARING drains in the opposite direction.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn clawback_after_outbound_decrements_net_refunded() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-CB", 1000).await;

    // Outbound stage-1 refund of 400 → refunded_minor = 400; UNALLOCATED 1000→600.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-OUT",
                "PSP-OUT",
                "PAY-CB",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
            ),
        )
        .await
        .expect("outbound stage-1 posts");
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CB", "refunded_minor").await,
        Some(400)
    );
    assert_eq!(bal(&raw, &s, s.unallocated).await, Some(600));
    assert_eq!(bal(&raw, &s, s.refund_clearing).await, Some(400));

    // Claw-back stage-1 of 400 (the PSP returned the cash): DECREMENTS refunded_minor
    // 400→0 (net refunded), inverts the legs (DR REFUND_CLEARING · CR UNALLOCATED)
    // so REFUND_CLEARING drains 400→0 and UNALLOCATED is restored 600→1000.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CB",
                "PSP-CB",
                "PAY-CB",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-OUT",
            ),
        )
        .await
        .expect("in-order claw-back posts");
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CB", "refunded_minor").await,
        Some(0),
        "claw-back decrements money-out back to the NET refunded (400 − 400)"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(0),
        "claw-back drains REFUND_CLEARING the opposite way"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(1000),
        "claw-back restores the drawn-down UNALLOCATED"
    );
    // The claw-back refund row carries the relates_to link.
    let relates: Option<String> = raw
        .query_one_raw(pg(format!(
            "SELECT relates_to_refund_id FROM bss.ledger_refund \
             WHERE tenant_id='{}' AND psp_refund_id='PSP-CB' AND phase='initiated'",
            s.tenant
        )))
        .await
        .unwrap()
        .and_then(|r| r.try_get_by_index::<Option<String>>(0).unwrap());
    assert_eq!(
        relates.as_deref(),
        Some("RF-OUT"),
        "claw-back row links the prior refund"
    );
    assert_eq!(
        clawback_queue_rows(&raw, &s, "QUEUED").await,
        0,
        "in-order claw-back never queues"
    );
}

/// An ADDITIONAL-OUTBOUND refund-of-refund (cash out again) INCREMENTS the money-out
/// counter like a plain stage-1, under the SAME cap.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn additional_outbound_refund_of_refund_increments_under_cap() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-AO", 1000).await;

    // First outbound refund of 300.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-1",
                "PSP-1",
                "PAY-AO",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("first outbound posts");

    // An ADDITIONAL OUTBOUND refund-of-refund of 200 (direction = Outbound, with a
    // relates_to link): INCREMENTS refunded_minor 300→500 under the same cap.
    let mut additional = refund_req(
        &s,
        "RF-2",
        "PSP-2",
        "PAY-AO",
        RefundPattern::AUnallocated,
        RefundPhase::Initiated,
        None,
        200,
    );
    additional.relates_to_refund_id = Some("RF-1".to_owned());
    additional.direction = RefundDirection::Outbound;
    refund_handler(&provider)
        .post_refund(&ctx, &scope, additional)
        .await
        .expect("additional-outbound posts");
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-AO", "refunded_minor").await,
        Some(500),
        "additional-outbound increments under the SAME money-out cap (300 + 200)"
    );
    assert_eq!(
        clawback_queue_rows(&raw, &s, "QUEUED").await,
        0,
        "outbound never queues"
    );
}

/// An OUT-OF-ORDER claw-back (no matching outbound refund stage-1 yet): the decrement
/// would underflow → DEFERRED to the REFUND_CLAWBACK queue (NOT aborted, NOT applied),
/// `refunded_minor` untouched. After the matching outbound lands, the drain applies it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn out_of_order_clawback_defers_then_applies_after_outbound() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-OOO", 1000).await;

    // Claw-back of 400 arrives FIRST (no outbound yet) → refunded_minor would go
    // 0 − 400 < 0 → DEFER. The call surfaces RefundClawbackDeferred (not a hard
    // error), nothing posts, and the row is QUEUED.
    let deferred = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CB",
                "PSP-CB",
                "PAY-OOO",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-OUT",
            ),
        )
        .await;
    assert!(
        matches!(deferred, Err(DomainError::RefundClawbackDeferred(_))),
        "out-of-order claw-back defers (not a hard fail): {deferred:?}"
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-OOO", "refunded_minor").await,
        Some(0),
        "the deferred claw-back applied NO decrement"
    );
    assert_eq!(
        clawback_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "claw-back is durably QUEUED"
    );
    assert_eq!(
        refund_rows(&raw, &s, "PSP-CB", "initiated").await,
        Some(0),
        "no refund row posted"
    );

    // The matching OUTBOUND refund stage-1 lands → refunded_minor 0→400.
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-OUT",
                "PSP-OUT",
                "PAY-OOO",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
            ),
        )
        .await
        .expect("outbound posts");
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-OOO", "refunded_minor").await,
        Some(400)
    );

    // Drain the claw-back queue: the decrement now FITS → APPLIED (refunded_minor
    // 400→0), the queue row flips →APPLIED, the claw-back refund row posts.
    let report = refund_handler(&provider)
        .drain_clawbacks(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.applied, 1, "the reconciled claw-back applied");
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-OOO", "refunded_minor").await,
        Some(0),
        "the drained claw-back decremented to the net refunded"
    );
    assert_eq!(clawback_queue_rows(&raw, &s, "QUEUED").await, 0);
    assert_eq!(
        clawback_queue_rows(&raw, &s, "APPLIED").await,
        1,
        "queue row flipped →APPLIED"
    );
    assert_eq!(
        refund_rows(&raw, &s, "PSP-CB", "initiated").await,
        Some(1),
        "claw-back row now posted"
    );
}

/// A claw-back that NEVER reconciles past the aging horizon: the drain flips it
/// →CANCELLED + escalates (the CLAWBACK_UNDERFLOW alarm fires via the noop publisher;
/// here we assert the durable CANCELLED transition + that no decrement was applied).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn never_reconciled_clawback_is_cancelled_and_escalated() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-ORPH", 1000).await;

    // Orphan claw-back: no matching outbound ever → defers, QUEUED.
    let _ = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CB",
                "PSP-CB",
                "PAY-ORPH",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-OUT",
            ),
        )
        .await;
    assert_eq!(clawback_queue_rows(&raw, &s, "QUEUED").await, 1);

    // Age the row past the 7-day horizon, then drain: it STILL underflows (no
    // outbound landed) AND is aged → CANCELLED + escalated.
    age_clawback_row(&raw, &s).await;
    let report = refund_handler(&provider)
        .drain_clawbacks(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.escalated, 1, "the orphan claw-back escalated");
    assert_eq!(report.applied, 0, "nothing applied");
    assert_eq!(
        clawback_queue_rows(&raw, &s, "CANCELLED").await,
        1,
        "queue row flipped →CANCELLED"
    );
    assert_eq!(clawback_queue_rows(&raw, &s, "QUEUED").await, 0);
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-ORPH", "refunded_minor").await,
        Some(0),
        "the never-reconciled claw-back applied NO decrement (the CHECK never fired)"
    );
}

// ---------------------------------------------------------------------------
// Group F: the `unknown_final` terminal disposition (loss-line write-off +
// secured-audit append).
// ---------------------------------------------------------------------------

/// One append the spy [`SecuredAuditSink`] captured (the fields the assertions
/// check). No durable persistence — the spy stands in for Slice 6's store.
#[derive(Clone, Debug)]
struct AuditCall {
    tenant: Uuid,
    event_type: String,
    actor_ref: Option<String>,
    reason_code: Option<String>,
    before_after: serde_json::Value,
}

/// A spy secured-audit sink: records every `append` (count + the captured calls)
/// so the `unknown_final` test can assert the disposition wrote exactly one
/// secured-audit record with the right event type / reason / payload. Like the
/// production `NoopSecuredAuditSink` it persists NOTHING durable + never fails (so
/// it cannot roll the disposition back) — it only observes the call.
#[derive(Default)]
struct SpyAuditSink {
    count: AtomicU64,
    calls: Mutex<Vec<AuditCall>>,
}

#[async_trait::async_trait]
impl SecuredAuditSink for SpyAuditSink {
    async fn append(
        &self,
        _txn: &DbTx<'_>,
        _scope: &AccessScope,
        tenant: Uuid,
        event_type: AuditEventType,
        actor_ref: Option<&str>,
        reason_code: Option<&str>,
        before_after: &serde_json::Value,
        _correlation_id: Option<Uuid>,
        _retain_until: Option<OffsetDateTime>,
    ) -> Result<Uuid, DbError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.calls.lock().unwrap().push(AuditCall {
            tenant,
            event_type: event_type.as_str().to_owned(),
            actor_ref: actor_ref.map(ToOwned::to_owned),
            reason_code: reason_code.map(ToOwned::to_owned),
            before_after: before_after.clone(),
        });
        Ok(Uuid::now_v7())
    }
}

/// The `unknown_final` dual-control disposition (design §4.4 / K-1): a two-stage
/// refund's stage-1 leaves `REFUND_CLEARING` open; the PSP then produces NO final
/// state, so the ledger-side disposition PARKS the stuck clearing on SUSPENSE
/// (`DR REFUND_CLEARING · CR SUSPENSE`) — draining clearing to zero, NOT booking a
/// premature loss/gain (the outcome is unknown) — AND writes a secured-audit record
/// (asserted via the spy sink).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unknown_final_parks_refund_clearing_to_suspense_and_audits() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // The SUSPENSE park target is seeded by `setup` (credit-normal). Settle 1000,
    // then a Pattern-A stage-1 of 300 → REFUND_CLEARING opens at 300.
    settle(&provider, &s, "PAY-UF", 1000).await;
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-UF1",
                "PSP-UF",
                "PAY-UF",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("stage-1 posts");
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(300),
        "stage-1 opens the REFUND_CLEARING balance"
    );

    // The unknown_final disposition: gated handler is NOT used here (no approval
    // engine attached ⇒ inline, like the executor's approved replay), with the spy
    // audit sink attached.
    let spy = Arc::new(SpyAuditSink::default());
    let handler = RefundHandler::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()))
        .with_audit_sink(Arc::clone(&spy) as Arc<dyn SecuredAuditSink>)
        .with_metrics(Arc::new(NoopLedgerMetrics));
    handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-UF-DISP",
                "PSP-UF",
                "PAY-UF",
                RefundPattern::AUnallocated,
                RefundPhase::UnknownFinal,
                None,
                300,
            ),
        )
        .await
        .expect("unknown_final disposition posts");

    // REFUND_CLEARING drained to zero (DR cancelled the stage-1 CR) …
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(0),
        "unknown_final drains REFUND_CLEARING to zero"
    );
    // … parked onto SUSPENSE (CR SUSPENSE 300, credit-normal → +300) pending
    // reconciliation — NOT booked to loss/gain (the outcome is unknown) …
    assert_eq!(
        bal(&raw, &s, s.suspense).await,
        Some(300),
        "the stuck clearing was parked to SUSPENSE pending reconciliation"
    );
    // … a refund row recorded SETTLED on the unknown_final phase grain …
    assert_eq!(
        refund_rows(&raw, &s, "PSP-UF", "unknown_final").await,
        Some(1)
    );
    assert_eq!(
        refund_clearing_state(&raw, &s, "PSP-UF", "unknown_final")
            .await
            .as_deref(),
        Some("SETTLED"),
        "the disposition drains REFUND_CLEARING (parked to SUSPENSE) → SETTLED"
    );
    // … and a NEVER touched CONTRACT_LIABILITY (a refund never restates revenue).
    assert_eq!(any_contract_liability(&raw, &s).await, 0);

    // The secured-audit append fired EXACTLY once with the K-1 contract.
    assert_eq!(
        spy.count.load(Ordering::SeqCst),
        1,
        "exactly one secured-audit record"
    );
    let call = spy.calls.lock().unwrap()[0].clone();
    assert_eq!(call.tenant, s.tenant);
    assert_eq!(call.event_type, "MANUAL_ADJUSTMENT");
    assert_eq!(call.reason_code.as_deref(), Some("REFUND_UNKNOWN_FINAL"));
    assert!(call.actor_ref.is_some(), "the acting subject is captured");
    assert_eq!(call.before_after["disposition"], "REFUND_UNKNOWN_FINAL");
    assert_eq!(call.before_after["after"]["parked_minor"], 300);
    assert_eq!(call.before_after["after"]["park_account_class"], "SUSPENSE");

    // Idempotent replay: re-posting the same disposition does NOT double-park (the
    // (tenant, REFUND, PSP-UF:unknown_final) claim short-circuits before the
    // sidecar) — and the spy records NO second append.
    handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-UF-DISP",
                "PSP-UF",
                "PAY-UF",
                RefundPattern::AUnallocated,
                RefundPhase::UnknownFinal,
                None,
                300,
            ),
        )
        .await
        .expect("replay is idempotent");
    assert_eq!(
        bal(&raw, &s, s.suspense).await,
        Some(300),
        "no double park on replay"
    );
    assert_eq!(
        spy.count.load(Ordering::SeqCst),
        1,
        "replay appends no second audit record"
    );
}

// ---------------------------------------------------------------------------
// Group G — refund-before-payment QUARANTINE de-quarantine drain (design §4.4).
// The intake (quarantine) is exercised elsewhere; this drives the
// `drain_quarantine → apply_quarantined` state machine that had ZERO coverage:
// the four terminal shapes (Released / AwaitingApproval / StillMissing /
// Escalated) plus the dispute-held hand-off. Also the claw-back replay
// short-circuit (QUEUED re-signals deferred, POSTED replays) and the AC #19
// idempotency-conflict capture (Z14-1).
// ---------------------------------------------------------------------------

/// Count REFUND_QUARANTINE work-state queue rows for the tenant in a given status.
async fn quarantine_queue_rows(raw: &DatabaseConnection, s: &Seller, status: &str) -> i64 {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_pending_event_queue \
             WHERE tenant_id='{}' AND flow='REFUND_QUARANTINE' AND status='{status}'",
            s.tenant
        ),
    )
    .await
    .unwrap_or(0)
}

/// Count REFUND_DISPUTE_HOLD work-state queue rows for the tenant in a given status —
/// a de-quarantined refund whose now-present origin has an OPEN dispute is handed off
/// to this queue (the de-quarantine terminates, the dispute-hold drain owns it).
async fn dispute_hold_queue_rows(raw: &DatabaseConnection, s: &Seller, status: &str) -> i64 {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_pending_event_queue \
             WHERE tenant_id='{}' AND flow='REFUND_DISPUTE_HOLD' AND status='{status}'",
            s.tenant
        ),
    )
    .await
    .unwrap_or(0)
}

/// Force the REFUND_QUARANTINE queue row to look aged: backdate `queued_at` well past
/// the 14-day quarantine aging horizon AND clear `apply_after` so it is immediately
/// claimable. Mirrors `age_clawback_row`.
async fn age_quarantine_row(raw: &DatabaseConnection, s: &Seller) {
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_pending_event_queue \
         SET queued_at = now() - interval '30 days', apply_after = NULL \
         WHERE tenant_id='{}' AND flow='REFUND_QUARANTINE'",
        s.tenant
    )))
    .await
    .unwrap();
}

/// Seed an OPEN dispute on `payment_id` directly (the simplest reliable way to put the
/// origin payment sub judice). Mirrors `postgres_refund_dispute_hold.rs::open_dispute`.
async fn open_dispute(
    raw: &DatabaseConnection,
    s: &Seller,
    dispute_id: &str,
    payment_id: &str,
    disputed: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_dispute \
         (tenant_id, dispute_id, payment_id, currency, variant, last_phase, cycle, \
          disputed_amount_minor, cash_hold_minor, version) \
         VALUES ('{}','{dispute_id}','{payment_id}','USD','CASH_HOLD','OPENED',1,{disputed},{disputed},0)",
        s.tenant
    )))
    .await
    .unwrap();
}

/// De-quarantine happy path: a refund-before-payment is QUARANTINED (origin absent),
/// then the origin payment lands; the next drain re-validates, posts the now-allowed
/// stage-1 inline (under threshold), and flips the queue row `→APPLIED` (`Released`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn quarantine_then_settle_drains_and_posts() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    // PAY-Q has no settlement yet → the refund is quarantined (refund-before-payment),
    // never posted.
    let outcome = handler
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-Q",
                "PSP-Q",
                "PAY-Q",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("record_refund quarantines a refund-before-payment");
    assert!(
        matches!(outcome, RefundOutcome::Quarantined(_)),
        "an absent origin quarantines, got {outcome:?}"
    );
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 1);
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "a quarantined refund posts nothing"
    );

    // The origin payment lands.
    settle(&provider, &s, "PAY-Q", 1000).await;

    // Drain: origin now resolvable + under threshold ⇒ posts inline, row →APPLIED.
    let report = handler
        .drain_quarantine(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.released, 1, "the de-quarantined refund posted");
    assert_eq!(report.still_missing, 0);
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 0);
    assert_eq!(
        quarantine_queue_rows(&raw, &s, "APPLIED").await,
        1,
        "the quarantine row flipped →APPLIED"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(300),
        "the de-quarantined stage-1 opened REFUND_CLEARING"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(700),
        "UNALLOCATED drawn down by the now-posted refund"
    );
    assert_eq!(refund_rows(&raw, &s, "PSP-Q", "initiated").await, Some(1));
}

/// De-quarantine back-off: the origin is STILL absent and the row is within the aging
/// horizon ⇒ the drain leaves it `QUEUED` (`StillMissing`), posts nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn quarantine_drain_still_missing_backs_off() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    handler
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-QM",
                "PSP-QM",
                "PAY-QM",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("quarantine");
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 1);

    // No settle ⇒ origin still missing; fresh row (not aged) ⇒ back off, stay QUEUED.
    let report = handler
        .drain_quarantine(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.still_missing, 1, "origin absent ⇒ back off");
    assert_eq!(report.released, 0);
    assert_eq!(report.escalated, 0);
    assert_eq!(
        quarantine_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the row stays QUEUED for a later retry"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "nothing posted while the origin is missing"
    );
}

/// De-quarantine give-up: the origin never landed past the 14-day aging horizon ⇒ the
/// drain flips the row `→CANCELLED` + escalates (`Escalated`). The Critical
/// `RefundQuarantined` alarm fires via the noop publisher; here the durable CANCELLED
/// transition is the assertion (mirrors `never_reconciled_clawback_*`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn quarantine_aged_out_is_cancelled_and_escalated() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    handler
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-QA",
                "PSP-QA",
                "PAY-QA",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("quarantine");
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 1);

    // Age the row past the 14-day horizon, then drain: origin STILL absent AND aged ⇒
    // CANCELLED + escalated.
    age_quarantine_row(&raw, &s).await;
    let report = handler
        .drain_quarantine(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.escalated, 1, "the aged-out quarantine escalated");
    assert_eq!(report.released, 0);
    assert_eq!(report.still_missing, 0);
    assert_eq!(
        quarantine_queue_rows(&raw, &s, "CANCELLED").await,
        1,
        "the quarantine row flipped →CANCELLED"
    );
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 0);
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "an abandoned quarantine posts nothing"
    );
}

/// De-quarantine over D2: the origin lands but the refund crosses the THEN-CURRENT D2
/// threshold ⇒ the gated drain opens an approval (`AwaitingApproval`) and the row
/// stays `QUEUED` — it NEVER auto-posts over threshold.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn quarantine_over_threshold_awaits_approval() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    // The drain re-drives through the GATED post path, where the gate creates a
    // PENDING approval keyed on the acting subject — so the drive MUST carry an
    // AUTHED context (an anonymous ctx has no subject and the gate no-ops).
    let ctx = dc_ctx(Uuid::now_v7(), s.tenant);
    let scope = AccessScope::for_tenant(s.tenant);
    // The GATED handler the drain re-drives through (over D2 ⇒ opens an approval).
    let (gated, _svc, _exec) = dual_control_wiring(&provider);

    // Quarantine a 150_000 refund (origin absent) — intake never gates (the gate is
    // re-checked at drain time, against the THEN-CURRENT threshold).
    let outcome = gated
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-QDC",
                "PSP-QDC",
                "PAY-QDC",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                150_000,
            ),
        )
        .await
        .expect("quarantine");
    assert!(matches!(outcome, RefundOutcome::Quarantined(_)));

    // The origin lands with headroom (200_000 settled), then drain: 150_000 > 100_000
    // D2 ⇒ the drain opens an approval, the row stays QUEUED.
    settle(&provider, &s, "PAY-QDC", 200_000).await;
    let report = gated
        .drain_quarantine(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(
        report.awaiting_approval, 1,
        "an over-threshold de-quarantine awaits approval"
    );
    assert_eq!(report.released, 0);
    assert_eq!(
        pending_refund_approvals(&raw, &s).await,
        1,
        "the gated drain opened exactly one PENDING REFUND approval"
    );
    assert_eq!(
        quarantine_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the row stays QUEUED — it never auto-posts over threshold"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "nothing posts while awaiting approval"
    );
}

/// De-quarantine into an OPEN dispute: the origin lands but the payment now has an
/// OPEN dispute ⇒ the re-driven refund is HELD on the dispute-hold queue. The
/// quarantine concern is resolved (origin present), so the quarantine row flips
/// `→APPLIED` (`Released`) and the dispute-hold drain owns it from here. Nothing posts.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn quarantine_drain_into_open_dispute_marks_applied() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    handler
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-QOD",
                "PSP-QOD",
                "PAY-QOD",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("quarantine");

    // The origin lands, but a dispute opens on it before the drain runs.
    settle(&provider, &s, "PAY-QOD", 1000).await;
    open_dispute(&raw, &s, "DISP-QOD", "PAY-QOD", 1000).await;

    let report = handler
        .drain_quarantine(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(
        report.released, 1,
        "the quarantine terminates cleanly (origin present, now dispute-held)"
    );
    assert_eq!(
        quarantine_queue_rows(&raw, &s, "APPLIED").await,
        1,
        "the quarantine row flipped →APPLIED (the dispute-hold queue owns it now)"
    );
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 0);
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the now-disputed refund was handed to the dispute-hold queue"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "a dispute-held refund posts nothing"
    );
    assert_eq!(refund_rows(&raw, &s, "PSP-QOD", "initiated").await, Some(0));
}

/// Claw-back replay (QUEUED): a deferred claw-back re-submitted under the same
/// `(psp_refund_id, initiated)` engine grain re-signals `RefundClawbackDeferred` via
/// the replay short-circuit — WITHOUT enqueuing a second row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn out_of_order_clawback_replay_resignals_deferred() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    settle(&provider, &s, "PAY-CBR", 1000).await;

    // Out-of-order claw-back (no matching outbound) defers → one QUEUED row.
    let first = handler
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CB",
                "PSP-CBR",
                "PAY-CBR",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-OUT",
            ),
        )
        .await;
    assert!(
        matches!(first, Err(DomainError::RefundClawbackDeferred(_))),
        "out-of-order claw-back defers: {first:?}"
    );
    assert_eq!(clawback_queue_rows(&raw, &s, "QUEUED").await, 1);

    // Re-submit the SAME claw-back: the engine dedup (REFUND, PSP-CBR:initiated) is
    // QUEUED, so the short-circuit re-signals deferred WITHOUT a second enqueue.
    let replay = handler
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CB",
                "PSP-CBR",
                "PAY-CBR",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-OUT",
            ),
        )
        .await;
    assert!(
        matches!(replay, Err(DomainError::RefundClawbackDeferred(_))),
        "a re-submitted deferred claw-back re-signals deferred: {replay:?}"
    );
    assert_eq!(
        clawback_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the short-circuit intercepted before a second enqueue (still one row)"
    );
}

/// Claw-back replay (POSTED): once a deferred claw-back reconciles + applies, a
/// re-submission under the same grain returns the prior posting (`replayed`) via the
/// short-circuit — no second decrement.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn applied_clawback_replay_returns_posted() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    settle(&provider, &s, "PAY-CBP", 1000).await;

    // Claw-back defers (no outbound yet).
    let _ = handler
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CBP",
                "PSP-CBP",
                "PAY-CBP",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-OUTP",
            ),
        )
        .await;
    assert_eq!(clawback_queue_rows(&raw, &s, "QUEUED").await, 1);

    // The matching outbound lands (refunded_minor 0→400), then drain applies the
    // claw-back (refunded_minor 400→0, dedup →POSTED, queue row →APPLIED).
    handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-OUTP",
                "PSP-OUTP",
                "PAY-CBP",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
            ),
        )
        .await
        .expect("outbound posts");
    let report = handler
        .drain_clawbacks(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.applied, 1);
    assert_eq!(clawback_queue_rows(&raw, &s, "APPLIED").await, 1);
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CBP", "refunded_minor").await,
        Some(0),
        "the applied claw-back netted refunded_minor back to zero"
    );

    // Re-submit the now-APPLIED claw-back: the short-circuit reads POSTED ⇒ returns
    // the prior posting (replayed), no second decrement.
    let replay = handler
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CBP",
                "PSP-CBP",
                "PAY-CBP",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-OUTP",
            ),
        )
        .await
        .expect("an applied claw-back replays harmlessly");
    assert!(
        replay.replayed,
        "the applied claw-back returns the prior posting (replayed)"
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-CBP", "refunded_minor").await,
        Some(0),
        "no second decrement on replay"
    );
}

/// Idempotency-conflict capture (AC #19, Z14-1): a same-key
/// `(psp_refund_id, phase)` quarantine re-intake with a DIFFERENT payload is a
/// `IdempotencyConflict` and the conflict is captured on the secured-audit sink; an
/// IDENTICAL re-intake is idempotent (`AlreadyQuarantined`, no error, no new row, no
/// new audit). Drives the shared `capture_idempotency_conflict` + both Replay arms.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn quarantine_conflict_payload_is_idempotency_conflict() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let spy = Arc::new(SpyAuditSink::default());
    let handler = RefundHandler::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()))
        .with_audit_sink(Arc::clone(&spy) as Arc<dyn SecuredAuditSink>);

    // First quarantine (origin PAY-QC-NONE absent).
    let first = handler
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-QC",
                "PSP-QC",
                "PAY-QC-NONE",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("first quarantine");
    assert!(matches!(first, RefundOutcome::Quarantined(_)));
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 1);

    // Re-quarantine the SAME (psp_refund_id, phase) with a DIFFERENT payload (amount
    // 500 ≠ 300) ⇒ IdempotencyConflict + a secured-audit capture, no second row.
    let conflict = handler
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-QC",
                "PSP-QC",
                "PAY-QC-NONE",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                500,
            ),
        )
        .await
        .expect_err("a different payload under the same key is a conflict");
    assert!(
        matches!(conflict, DomainError::IdempotencyConflict(_)),
        "expected IdempotencyConflict, got {conflict:?}"
    );
    assert_eq!(
        quarantine_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the conflicting re-intake created no second queue row"
    );
    assert_eq!(
        spy.count.load(Ordering::SeqCst),
        1,
        "the conflict was captured on the secured-audit sink (exactly once)"
    );
    let call = spy.calls.lock().unwrap()[0].clone();
    assert_eq!(call.tenant, s.tenant);
    assert_eq!(call.event_type, "MANUAL_ADJUSTMENT");
    assert_eq!(call.reason_code.as_deref(), Some("IDEMPOTENCY_CONFLICT"));
    assert_eq!(call.before_after["event"], "IDEMPOTENCY_CONFLICT");

    // An IDENTICAL re-intake is idempotent: AlreadyQuarantined ⇒ Ok, no new row, no
    // new audit record.
    let replay = handler
        .record_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-QC",
                "PSP-QC",
                "PAY-QC-NONE",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("an identical re-quarantine is idempotent");
    assert!(matches!(replay, RefundOutcome::Quarantined(_)));
    assert_eq!(quarantine_queue_rows(&raw, &s, "QUEUED").await, 1);
    assert_eq!(
        spy.count.load(Ordering::SeqCst),
        1,
        "an idempotent re-quarantine appends no second audit record"
    );
}
