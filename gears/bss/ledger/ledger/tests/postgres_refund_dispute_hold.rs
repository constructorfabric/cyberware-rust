//! Postgres-only integration: the Slice-3 / §5 REFUND DISPUTE-HOLD control
//! ([`DomainError::RefundDisputeHeld`]), driven through the REAL foundation engine
//! (`SettlementService` + `RefundHandler`) against a real settled payment with an
//! OPEN dispute seeded directly into `bss.ledger_dispute`.
//!
//! A refund is money-OUT; a forward money-OUT post on a payment whose origin has an
//! OPEN dispute is sub judice — the disputed funds may be clawed back, so paying the
//! refund out now would double-spend. The handler's dispute-hold gate (Z5-2) HOLDS
//! such a refund: it claims a `(tenant, REFUND_DISPUTE_HOLD, psp_refund_id:phase)`
//! dedup row `QUEUED`, inserts the work-state queue row, posts NOTHING, and returns
//! `Err(DomainError::RefundDisputeHeld(token))`. The exclusion is by
//! `is_dispute_holdable`: only a FORWARD money-OUT post is held — a claw-back
//! (money-IN), a `rejected`/`voided` reversal (cap release), and the
//! `unknown_final` write-off are NOT.
//!
//! This file covers the two control arms that had ZERO executable coverage:
//! - a forward stage-1 `initiated` refund on a payment with an OPEN dispute is HELD
//!   (the dedup/queue row lands, the books DO NOT move, the money-out cap is
//!   unchanged);
//! - a CLAW-BACK on the SAME open-dispute setup is NOT held (it skips the hold gate
//!   — `is_dispute_holdable` is false for money-IN — and falls through to its own
//!   defer path), locking the money-IN exclusion.
//!
//! Ignored by default; run with `-- --ignored` (needs Docker / testcontainers).
//! Mirrors the helpers + setup of `postgres_refund.rs` (self-contained by
//! convention — duplication across these test files is normal).

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
use std::sync::atomic::{AtomicUsize, Ordering};

use bss_ledger::domain::adjustment::refund::{
    RefundDirection, RefundPattern, RefundPhase, RefundRequest,
};
use bss_ledger::domain::approval::intent::ApprovalIntent;
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::adjustment::refund_service::RefundHandler;
use bss_ledger::infra::approval::service::{ApprovalExecutor, ApprovalService};
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, Side};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
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

/// Read a `payment_settlement` counter column for `(tenant, payment_id)` — the cap
/// basis Group C maintains (here used to assert a HELD refund moved NO cap).
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

/// The `clearing_state` of a `(psp_refund_id, phase)` refund row (used to confirm a
/// HELD refund left NO posted row at all — `None`).
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

/// `count(*)` of `REFUND_DISPUTE_HOLD` ENGINE dedup rows in `status` for the tenant,
/// keyed on the refund's `(psp_refund_id, phase)` business id. The hold intake
/// claims this row `QUEUED` before inserting the work-state queue row.
async fn dispute_hold_dedup_rows(
    raw: &DatabaseConnection,
    s: &Seller,
    psp: &str,
    phase: &str,
    status: &str,
) -> i64 {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_idempotency_dedup \
             WHERE tenant_id='{}' AND flow='REFUND_DISPUTE_HOLD' \
             AND business_id='{psp}:{phase}' AND status='{status}'",
            s.tenant
        ),
    )
    .await
    .unwrap_or(0)
}

/// `count(*)` of `REFUND_DISPUTE_HOLD` work-state QUEUE rows in `status` for the
/// tenant (the durable hold the drain later re-reads).
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

/// Seed an OPEN dispute on `payment_id` directly (the simplest reliable way to put
/// the origin payment sub judice). `last_phase = 'OPENED'` is what
/// `read_open_dispute_for_payment` filters on; `variant = 'CASH_HOLD'` +
/// `cash_hold_minor <= disputed_amount_minor` satisfy the table CHECKs.
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

/// Resolve an existing dispute to a terminal `last_phase` (e.g. `WON`) — flips the
/// row so a subsequent `read_open_dispute_for_payment` finds NO open dispute.
async fn resolve_dispute(raw: &DatabaseConnection, s: &Seller, dispute_id: &str, last_phase: &str) {
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_dispute SET last_phase='{last_phase}', version = version + 1 \
         WHERE tenant_id='{}' AND dispute_id='{dispute_id}'",
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
        // First-order OUTBOUND refund by default; the claw-back test builds the
        // `Clawback` variant via `clawback_req`.
        relates_to_refund_id: None,
        direction: RefundDirection::Outbound,
    }
}

/// A refund-of-refund CLAW-BACK request: references a prior refund + the `Clawback`
/// direction (money-IN). `validate_shape` requires the `relates_to_refund_id` link
/// for a `Clawback`. Mirrors `postgres_refund.rs`'s `clawback_req`.
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

/// A forward stage-1 `initiated` refund on a payment with an OPEN dispute is HELD:
/// it returns `RefundDisputeHeld`, claims the `REFUND_DISPUTE_HOLD` dedup row +
/// inserts the work-state queue row, and posts NOTHING (no `REFUND_CLEARING`
/// balance, no `refund` record row, the money-out cap unchanged).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn forward_refund_on_payment_with_open_dispute_is_held() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle 1000 → UNALLOCATED holds 1000 (CR), CASH_CLEARING holds 1000 (DR).
    settle(&provider, &s, "PAY-DH", 1000).await;
    assert_eq!(bal(&raw, &s, s.unallocated).await, Some(1000));

    // Open a dispute on PAY-DH (the disputed funds are now sub judice).
    open_dispute(&raw, &s, "DISP-DH", "PAY-DH", 1000).await;

    // A forward stage-1 `initiated` refund of 300 must be HELD (not posted).
    let err = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-DH1",
                "PSP-DH",
                "PAY-DH",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect_err("a forward refund on a payment with an OPEN dispute must be held");
    assert!(
        matches!(err, DomainError::RefundDisputeHeld(_)),
        "expected RefundDisputeHeld, got {err:?}"
    );

    // The hold is DURABLE: the engine dedup row is QUEUED and the work-state queue
    // row landed (both under the REFUND_DISPUTE_HOLD flow, keyed on PSP-DH:initiated).
    assert_eq!(
        dispute_hold_dedup_rows(&raw, &s, "PSP-DH", "initiated", "QUEUED").await,
        1,
        "the hold claimed exactly one QUEUED REFUND_DISPUTE_HOLD dedup row"
    );
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the hold inserted exactly one QUEUED work-state queue row"
    );

    // NOTHING posted: no REFUND_CLEARING balance, no `refund` record row, the
    // money-out cap untouched (the books did not move).
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "a held refund opens NO REFUND_CLEARING balance"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(1000),
        "a held refund does NOT draw the UNALLOCATED pool down"
    );
    assert_eq!(
        refund_rows(&raw, &s, "PSP-DH", "initiated").await,
        Some(0),
        "a held refund persists NO `refund` record row"
    );
    assert_eq!(
        refund_clearing_state(&raw, &s, "PSP-DH", "initiated").await,
        None,
        "a held refund has no clearing_state (no row at all)"
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-DH", "refunded_minor").await,
        Some(0),
        "a held refund reserves NO money-out cap (refunded_minor unchanged)"
    );
    assert_eq!(
        settlement_counter(&raw, &s, "PAY-DH", "refunded_unallocated_minor").await,
        Some(0),
        "a held refund moves NO spendable-headroom cap either"
    );

    // Sanity: once the dispute resolves WON, the gate no longer holds — a fresh
    // forward refund posts inline (this is the same gated path the hold drain
    // re-drives through). Proves the hold is specifically the OPEN-dispute state,
    // not a permanent block on the payment.
    resolve_dispute(&raw, &s, "DISP-DH", "WON").await;
    refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-DH2",
                "PSP-DH-WON",
                "PAY-DH",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await
        .expect("with the dispute resolved WON the forward refund posts inline");
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(300),
        "post-resolution the stage-1 REFUND_CLEARING balance opens"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(700),
        "post-resolution UNALLOCATED is drawn down by the now-allowed refund"
    );
}

/// A CLAW-BACK (money-IN refund-of-refund) on the SAME open-dispute setup is NOT
/// dispute-held: `is_dispute_holdable` is false for a claw-back, so the hold gate is
/// skipped and the claw-back falls through to its own defer path
/// (`RefundClawbackDeferred`, because no matching outbound refund exists to
/// decrement). The key assertion is the NEGATIVE: it is NOT `RefundDisputeHeld`, and
/// NO `REFUND_DISPUTE_HOLD` row is created. This locks the money-IN exclusion.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn clawback_on_payment_with_open_dispute_is_not_held() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-CBDH", 1000).await;
    // Same OPEN-dispute setup as the held test.
    open_dispute(&raw, &s, "DISP-CBDH", "PAY-CBDH", 1000).await;

    // A claw-back stage-1 (money-IN) referencing a prior refund. There is no matching
    // outbound refund stage-1, so the money-out decrement would underflow → the
    // handler DEFERS it (Group E). Crucially it is NOT dispute-held: the claw-back
    // skips the hold gate entirely (money-IN does not pay the customer).
    let err = refund_handler(&provider)
        .post_refund(
            &ctx,
            &scope,
            clawback_req(
                &s,
                "RF-CBDH1",
                "PSP-CBDH",
                "PAY-CBDH",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                400,
                "RF-PRIOR-OUTBOUND",
            ),
        )
        .await
        .expect_err("a claw-back with no matching outbound refund defers");

    // The money-IN exclusion: a claw-back is NEVER dispute-held, even with an OPEN
    // dispute on the origin payment.
    assert!(
        !matches!(err, DomainError::RefundDisputeHeld(_)),
        "a claw-back must NOT be dispute-held (money-IN is excluded), got {err:?}"
    );
    // It took a NON-held path (an Err, not a posted refund). By design the money-IN
    // outcome is the underflow defer (`RefundClawbackDeferred`): the refund
    // sidecar's cap/underflow CHECK now runs BEFORE balance projection (the
    // `run_before_projection` fix), so an out-of-order claw-back surfaces the
    // canonical defer, not a raw `NegativeBalance` from the projector's no-negative
    // guard.
    assert!(
        matches!(err, DomainError::RefundClawbackDeferred(_)),
        "expected the money-IN underflow defer (RefundClawbackDeferred), got {err:?}"
    );

    // No dispute-hold artefacts were created (neither the dedup nor the queue row):
    // the hold gate never fired for the claw-back.
    assert_eq!(
        dispute_hold_dedup_rows(&raw, &s, "PSP-CBDH", "initiated", "QUEUED").await,
        0,
        "a claw-back creates NO REFUND_DISPUTE_HOLD dedup row"
    );
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "QUEUED").await,
        0,
        "a claw-back creates NO REFUND_DISPUTE_HOLD work-state queue row"
    );
}

// ---------------------------------------------------------------------------
// §5 — dispute-hold DRAIN (Z5-2): the `drain_dispute_hold → apply_dispute_hold`
// state machine that had ZERO coverage. The five terminal shapes: WON re-drives
// + posts (`Released`), LOST cancels + never posts (`Cancelled`, double-pay
// guard), still-OPEN backs off (`StillDisputed`), aged-out escalates
// (`Escalated`), WON-but-over-D2 awaits approval (`AwaitingApproval`).
// ---------------------------------------------------------------------------

/// An `ApprovalExecutor` that replays the held refund through the un-gated
/// `RefundHandler` (`post_refund_approved`) — the real Group-D executor arm.
/// Mirrors `postgres_refund.rs::RefundReplayExecutor`.
#[derive(Clone)]
struct RefundReplayExecutor {
    refund: Arc<RefundHandler>,
    calls: Arc<AtomicUsize>,
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
                self.calls.fetch_add(1, Ordering::SeqCst);
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

/// Build the gated handler + the approval service sharing the un-gated replay handler
/// over the same provider. Mirrors `postgres_refund.rs::dual_control_wiring`.
fn dual_control_wiring(
    provider: &DBProvider<DbError>,
) -> (RefundHandler, Arc<ApprovalService>, RefundReplayExecutor) {
    let replay_handler = Arc::new(RefundHandler::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
    ));
    let exec = RefundReplayExecutor {
        refund: Arc::clone(&replay_handler),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let svc = Arc::new(ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    ));
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

/// Force the REFUND_DISPUTE_HOLD queue row to look aged: backdate `queued_at` well
/// past the 30-day dispute-hold aging horizon AND clear `apply_after`. Mirrors
/// `postgres_refund.rs::age_clawback_row`.
async fn age_dispute_hold_row(raw: &DatabaseConnection, s: &Seller) {
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_pending_event_queue \
         SET queued_at = now() - interval '60 days', apply_after = NULL \
         WHERE tenant_id='{}' AND flow='REFUND_DISPUTE_HOLD'",
        s.tenant
    )))
    .await
    .unwrap();
}

/// Hold a forward refund behind an OPEN dispute, then resolve it WON: the next drain
/// re-reads the dispute, re-drives the (now-allowed) refund through the gated path,
/// posts it, and flips the hold row `→APPLIED` (`Released`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dispute_hold_drain_won_redrives_and_posts() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    settle(&provider, &s, "PAY-WON", 1000).await;
    open_dispute(&raw, &s, "DISP-WON", "PAY-WON", 1000).await;

    // Forward refund on the disputed payment is HELD (nothing posts).
    let held = handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-WON",
                "PSP-WON",
                "PAY-WON",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await;
    assert!(matches!(held, Err(DomainError::RefundDisputeHeld(_))));
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 1);
    assert_eq!(bal(&raw, &s, s.refund_clearing).await, None);

    // The dispute resolves WON (the payment stands — the refund is owed).
    resolve_dispute(&raw, &s, "DISP-WON", "WON").await;

    let report = handler
        .drain_dispute_hold(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.released, 1, "the WON-resolved refund posted");
    assert_eq!(report.still_disputed, 0);
    assert_eq!(report.cancelled, 0);
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "APPLIED").await,
        1,
        "the hold row flipped →APPLIED"
    );
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 0);
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        Some(300),
        "the released stage-1 opened REFUND_CLEARING"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(700),
        "UNALLOCATED drawn down by the now-posted refund"
    );
    assert_eq!(refund_rows(&raw, &s, "PSP-WON", "initiated").await, Some(1));
}

/// Hold a forward refund behind an OPEN dispute, then resolve it LOST: the drain
/// CANCELS the hold and NEVER posts (a lost chargeback already returned the money —
/// posting the refund too would double-pay). The Critical `RefundQuarantined` alarm
/// fires via the noop publisher; the durable CANCELLED transition is the assertion.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dispute_hold_drain_lost_cancels_never_posts() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    settle(&provider, &s, "PAY-LOST", 1000).await;
    open_dispute(&raw, &s, "DISP-LOST", "PAY-LOST", 1000).await;

    let held = handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-LOST",
                "PSP-LOST",
                "PAY-LOST",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await;
    assert!(matches!(held, Err(DomainError::RefundDisputeHeld(_))));
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 1);

    // The dispute resolves LOST (a chargeback already returned the money).
    resolve_dispute(&raw, &s, "DISP-LOST", "LOST").await;

    let report = handler
        .drain_dispute_hold(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.cancelled, 1, "a LOST dispute cancels the hold");
    assert_eq!(report.released, 0, "a LOST dispute NEVER posts the refund");
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "CANCELLED").await,
        1,
        "the hold row flipped →CANCELLED"
    );
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 0);
    // Double-pay guard: nothing posted, the books are untouched.
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "a LOST-cancelled refund posts nothing"
    );
    assert_eq!(
        bal(&raw, &s, s.unallocated).await,
        Some(1000),
        "the UNALLOCATED pool is untouched"
    );
    assert_eq!(
        refund_rows(&raw, &s, "PSP-LOST", "initiated").await,
        Some(0)
    );
}

/// Hold a forward refund behind an OPEN dispute that is STILL open at drain time: the
/// drain backs off (`StillDisputed`), the row stays `QUEUED`, the cash stays held.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dispute_hold_drain_still_open_backs_off() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    settle(&provider, &s, "PAY-OPEN", 1000).await;
    open_dispute(&raw, &s, "DISP-OPEN", "PAY-OPEN", 1000).await;

    let held = handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-OPEN",
                "PSP-OPEN",
                "PAY-OPEN",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await;
    assert!(matches!(held, Err(DomainError::RefundDisputeHeld(_))));
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 1);

    // Drain WITHOUT resolving: the dispute is still OPENED, the row is fresh ⇒ back off.
    let report = handler
        .drain_dispute_hold(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.still_disputed, 1, "a still-OPEN dispute backs off");
    assert_eq!(report.released, 0);
    assert_eq!(report.cancelled, 0);
    assert_eq!(report.escalated, 0);
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the row stays QUEUED (the cash stays held)"
    );
    assert_eq!(bal(&raw, &s, s.refund_clearing).await, None);
}

/// Hold a forward refund behind an OPEN dispute that NEVER resolves past the 30-day
/// aging horizon: the drain CANCELS the hold + escalates (`Escalated`). NEVER posts
/// (the dispute is still OPEN).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dispute_hold_aged_out_escalates() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let handler = refund_handler(&provider);

    settle(&provider, &s, "PAY-AGE", 1000).await;
    open_dispute(&raw, &s, "DISP-AGE", "PAY-AGE", 1000).await;

    let held = handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &s,
                "RF-AGE",
                "PSP-AGE",
                "PAY-AGE",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                300,
            ),
        )
        .await;
    assert!(matches!(held, Err(DomainError::RefundDisputeHeld(_))));
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 1);

    // Age the hold row past 30 days; the dispute is STILL OPEN ⇒ aged-out escalate.
    age_dispute_hold_row(&raw, &s).await;
    let report = handler
        .drain_dispute_hold(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(report.escalated, 1, "the never-resolved hold escalated");
    assert_eq!(report.released, 0);
    assert_eq!(report.cancelled, 0);
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "CANCELLED").await,
        1,
        "the aged-out hold row flipped →CANCELLED"
    );
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 0);
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "an aged-out hold posts nothing"
    );
}

/// Hold an OVER-D2 forward refund behind an OPEN dispute, then resolve it WON: the
/// gated drain re-drives but the refund now crosses D2 ⇒ it opens an approval
/// (`AwaitingApproval`) and the row stays `QUEUED` — it NEVER auto-posts over threshold.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dispute_hold_won_over_threshold_awaits_approval() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    // The drain re-drives through the GATED post path, where the gate creates a
    // PENDING approval keyed on the acting subject — so the drive MUST carry an
    // AUTHED context (an anonymous ctx has no subject and the gate no-ops).
    let ctx = dc_ctx(Uuid::now_v7(), s.tenant);
    let scope = AccessScope::for_tenant(s.tenant);
    // The GATED handler holds at intake (dispute) and re-drives over D2 at drain.
    let (gated, _svc, _exec) = dual_control_wiring(&provider);

    settle(&provider, &s, "PAY-WONDC", 200_000).await;
    open_dispute(&raw, &s, "DISP-WONDC", "PAY-WONDC", 200_000).await;

    // A 150_000 forward refund is HELD by the dispute (the hold precedes the gate).
    let held = gated
        .post_refund(
            &dc_ctx(Uuid::now_v7(), s.tenant),
            &scope,
            refund_req(
                &s,
                "RF-WONDC",
                "PSP-WONDC",
                "PAY-WONDC",
                RefundPattern::AUnallocated,
                RefundPhase::Initiated,
                None,
                150_000,
            ),
        )
        .await;
    assert!(matches!(held, Err(DomainError::RefundDisputeHeld(_))));
    assert_eq!(dispute_hold_queue_rows(&raw, &s, "QUEUED").await, 1);

    // Dispute resolves WON; the gated drain re-drives → 150_000 > 100_000 D2 ⇒ an
    // approval opens, the row stays QUEUED (never auto-posts over threshold).
    resolve_dispute(&raw, &s, "DISP-WONDC", "WON").await;
    let report = gated
        .drain_dispute_hold(&ctx, &scope, s.tenant, 100)
        .await
        .expect("drain succeeds");
    assert_eq!(
        report.awaiting_approval, 1,
        "an over-threshold WON release awaits approval"
    );
    assert_eq!(report.released, 0);
    assert_eq!(
        pending_refund_approvals(&raw, &s).await,
        1,
        "the gated drain opened exactly one PENDING REFUND approval"
    );
    assert_eq!(
        dispute_hold_queue_rows(&raw, &s, "QUEUED").await,
        1,
        "the row stays QUEUED — it never auto-posts over threshold"
    );
    assert_eq!(
        bal(&raw, &s, s.refund_clearing).await,
        None,
        "nothing posts while awaiting approval"
    );
}
