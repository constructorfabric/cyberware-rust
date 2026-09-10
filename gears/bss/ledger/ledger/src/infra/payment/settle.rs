//! `SettlementService` — the orchestrator that drives the pure settlement
//! domain (`crate::domain::payment::settlement`) through the foundation engine
//! (Pattern A). It records the **money-in** side of a payment: a settled
//! receipt lands in the payer's unallocated pool (`CR UNALLOCATED` for the
//! gross), the net cash hits clearing (`DR CASH_CLEARING`), and the processor
//! fee — when any — is expensed (`DR PSP_FEE_EXPENSE`). It does NOT move AR;
//! allocation (`AllocationService`) drains the pool into receivables later.
//!
//! It ties the pieces together for one settle post (mirrors the
//! invoice-post orchestrator's shape):
//! 1. **build** the balanced Pattern-A entry
//!    (`domain::payment::settlement::build_settlement_entry`).
//! 2. **overwrite header** — the pure builder emits placeholder header fields;
//!    the orchestrator stamps the real `period_id` (derived from the effective
//!    date), `effective_at` (a real date for the `None` case), the actor (from
//!    the security context), and a fresh `correlation_id`.
//! 3. **bind** each line's real chart `account_id` from the provisioned chart of
//!    accounts (the pure builder emits a nil placeholder).
//! 4. **resolve scale** per line and **post** via [`PostingService`], threading
//!    the [`SettlementSidecar`] so the `payment_settlement` counter row is seeded
//!    in the same serializable transaction (or rolled back with the entry).
//! 5. **emit metrics** — `payment_settle` (outcome) + the payment-post duration
//!    on every attempt.
//!
//! There is NO payer gate (unlike the invoice-post path): a settlement records
//! money already received and must land even for a closed payer. Lives in
//! `infra` (not `domain`) because it needs repo + posting access; the domain
//! module it calls stays pure (dylint DE0301).

use std::sync::Arc;
use std::time::Instant;

use bss_ledger_sdk::{PostEntry, PostLine, PostingRef};
use chrono::Datelike;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::payment::settlement::{SettlementInput, build_settlement_entry};
use crate::domain::ports::metrics::{LedgerMetricsPort, PostFlow, PostResult};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::fx::rate_locker::RateLocker;
use crate::infra::payment::queue_apply::QueueApplier;
use crate::infra::payment::sidecar::SettlementSidecar;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::service::{PostSidecar, PostingService};
use crate::infra::storage::repo::ReferenceRepo;
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// Per-tenant cap on the drain-on-settle pass (D3): a sane batch ceiling so a
/// settle that unblocks a large backlog doesn't post an unbounded number of
/// allocations inline on the settle path. The periodic sweep (D4) drains the
/// remainder.
const DRAIN_ON_SETTLE_CAP: u64 = 100;

/// Orchestrates the settlement domain (Pattern A) over the foundation engine.
pub struct SettlementService {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    metrics: Arc<dyn LedgerMetricsPort>,
    // Retained so the post-settle drain hook (D3) can build a `QueueApplier`
    // (same db/publisher/metrics deps as `AllocationService`): a settlement is
    // exactly the event that unblocks a queued allocation, so we drain the
    // tenant's queue right after the settle commits.
    db: DBProvider<DbError>,
    publisher: Arc<LedgerEventPublisher>,
    /// The S2 settle FX lock (Slice 5). `None` = single-currency (no FX);
    /// `with_fx` attaches it for a deployment with an FX rate source.
    rate_locker: Option<RateLocker>,
}

impl SettlementService {
    /// Build the service over one database provider, the event publisher
    /// (threaded into the posting engine), and the metrics sink. Mirrors
    /// [`crate::infra::invoice_post::InvoicePostService::new`].
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        Self {
            posting,
            reference,
            resolver,
            metrics,
            db,
            publisher,
            rate_locker: None,
        }
    }

    /// Attach the S2 settle FX lock (Slice 5). A settle of a receipt whose
    /// currency differs from the seller's functional currency then resolves +
    /// snapshots the locked rate and stamps the functional translation on every
    /// line (one rate per entry, §4.3). Builder form so the existing `new` call
    /// sites stay single-currency (FX off) unchanged.
    #[must_use]
    pub fn with_fx(mut self, rate_locker: RateLocker) -> Self {
        self.rate_locker = Some(rate_locker);
        self
    }

    /// Settle a payment (Pattern A): post `DR CASH_CLEARING (net)` +
    /// `DR PSP_FEE_EXPENSE (fee, omitted when zero)` + `CR UNALLOCATED (gross)`,
    /// seeding the `payment_settlement` counter row in the same transaction.
    /// Idempotent on `(tenant, PAYMENT_SETTLE, payment_id)` — a re-settle of the
    /// same payment replays the prior entry with no new ledger effect.
    ///
    /// On success emits `payment_settle(Posted | Replayed)` + the payment-post
    /// duration; every rejection emits `payment_settle(Rejected)` + the duration.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] for an unrepresentable settlement
    /// (`gross < 0`, `fee < 0`, `fee > gross`); [`DomainError::AccountClosed`]
    /// when a required class (`CASH_CLEARING` / `UNALLOCATED` / `PSP_FEE_EXPENSE`) is
    /// not provisioned; any foundation rejection (period-closed / account-closed
    /// / …) or [`DomainError::Internal`] on an infrastructure fault.
    pub async fn settle(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        input: SettlementInput,
    ) -> Result<PostingRef, DomainError> {
        let started = Instant::now();
        let tenant = input.tenant_id;
        let result = self.settle_inner(ctx, scope, input).await;
        self.record(&result, started);

        // D3 — drain-on-settle. A settlement is the event that unblocks a queued
        // allocation (allocate-before-settlement, §4.7): once this settle COMMITS,
        // drain the tenant's queued allocations so a previously-NotReady apply
        // posts immediately rather than waiting for the periodic sweep. ONLY on a
        // FRESH settle (`Ok` and not `replayed`): a rejected settle wrote no
        // settlement, and an idempotent replay wrote nothing new either — the
        // original settle already ran this drain, so re-draining on every retried
        // settle is pure waste (and re-drives any apply-blocked rows on each
        // retry). A drain error MUST NOT fail the settle — log + swallow; the
        // sweep job retries. The drain re-reads the settlement per row, so it is
        // safe even though the just-committed row is now visible out-of-txn.
        if matches!(&result, Ok(posting) if !posting.replayed) {
            let applier = QueueApplier::new(
                self.db.clone(),
                Arc::clone(&self.publisher),
                Arc::clone(&self.metrics),
            );
            match applier.drain(ctx, scope, tenant, DRAIN_ON_SETTLE_CAP).await {
                Ok(report) => {
                    if report.applied > 0 || report.blocked > 0 {
                        tracing::info!(
                            tenant_id = %tenant,
                            applied = report.applied,
                            not_ready = report.not_ready,
                            blocked = report.blocked,
                            "bss-ledger: drain-on-settle applied queued allocations"
                        );
                    }
                }
                Err(e) => tracing::error!(
                    tenant_id = %tenant,
                    error = %e,
                    "bss-ledger: drain-on-settle failed (swallowed; sweep will retry)"
                ),
            }
        }

        result
    }

    /// Build + post the settlement entry (no metrics — the public wrapper records
    /// them).
    async fn settle_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        input: SettlementInput,
    ) -> Result<PostingRef, DomainError> {
        // 1. Build the balanced Pattern-A entry (validates gross/fee invariants).
        let mut entry = build_settlement_entry(&input)?;

        // 2. Overwrite the placeholder header fields the pure builder emits.
        overwrite_header(&mut entry, ctx, input.effective_at);

        // 3. Bind each line's real chart account_id from the provisioned chart.
        let chart = load_chart(&self.reference, scope, entry.tenant_id).await?;
        for line in &mut entry.lines {
            line.account_id = resolve_line(&chart, line).ok_or_else(|| {
                DomainError::AccountClosed(format!(
                    "no provisioned account for class {} / stream {:?} / currency {}",
                    line.account_class.as_str(),
                    line.revenue_stream,
                    line.currency
                ))
            })?;
        }

        // 4. Map to the engine's NewEntry/NewLine (resolving per-line scale) and
        //    post, threading the settlement sidecar so the payment_settlement
        //    counter is seeded atomically with the journal entry.
        let sidecar: Arc<dyn PostSidecar> = Arc::new(SettlementSidecar {
            tenant: input.tenant_id,
            payment_id: input.payment_id.clone(),
            currency: input.currency.clone(),
            gross_minor: input.gross_minor,
            fee_minor: input.fee_minor,
        });
        self.post_bound(ctx, scope, entry, sidecar).await
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's
    /// `NewEntry`/`NewLine`, resolving each line's scale, and post with the
    /// settlement sidecar.
    async fn post_bound(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Arc<dyn PostSidecar>,
    ) -> Result<PostingRef, DomainError> {
        let mut new_entry = NewEntry {
            entry_id: entry.entry_id,
            tenant_id: entry.tenant_id,
            // v1: one legal entity per tenant — derived server-side.
            legal_entity_id: entry.tenant_id,
            period_id: entry.period_id.clone(),
            entry_currency: entry.entry_currency.clone(),
            source_doc_type: entry.source_doc_type,
            source_business_id: entry.source_business_id.clone(),
            reverses_entry_id: entry.reverses_entry_id,
            reverses_period_id: entry.reverses_period_id.clone(),
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: entry.effective_at,
            origin: ORIGIN_SYSTEM.to_owned(),
            posted_by_actor_id: entry.posted_by_actor_id,
            correlation_id: entry.correlation_id,
            rounding_evidence: serde_json::Value::Null,
            // Slice 5: set by the S2 FX lock below on a cross-currency settle.
            rate_snapshot_ref: None,
        };
        let mut new_lines: Vec<NewLine> = Vec::with_capacity(entry.lines.len());
        for line in entry.lines {
            let scale = self
                .resolver
                .resolve(scope, entry.tenant_id, &line.currency)
                .await
                .map_err(|e| DomainError::Internal(format!("currency scale resolve: {e}")))?;
            new_lines.push(new_line(line, scale));
        }
        // S2 settle FX lock: when configured AND the receipt currency differs from
        // the seller's functional currency, resolve + snapshot the locked rate and
        // stamp functional on every line (one rate per entry, §4.3). Inert (None)
        // for a single-currency tenant — existing settles stay byte-green. Mirrors
        // the S1 invoice-post hook.
        if let Some(locker) = &self.rate_locker {
            let functional_ccy = self
                .reference
                .functional_currency(scope, new_entry.tenant_id)
                .await
                .map_err(|e| DomainError::Internal(format!("functional currency lookup: {e}")))?;
            if let Some(fc) = functional_ccy
                && fc != new_entry.entry_currency
            {
                new_entry.rate_snapshot_ref = locker
                    .lock_and_stamp(
                        scope,
                        new_entry.tenant_id,
                        &mut new_lines,
                        &new_entry.entry_currency,
                        &fc,
                        OffsetDateTime::now_utc(),
                    )
                    .await?;
            }
        }
        self.posting
            .post(ctx, scope, new_entry, new_lines, Some(sidecar))
            .await
    }

    /// Emit `payment_settle(outcome)` + the `Settle`-labelled payment-post
    /// duration for one attempt (mirrors `invoice_post::record`).
    fn record(&self, result: &Result<PostingRef, DomainError>, started: Instant) {
        let outcome = match result {
            Ok(r) if r.replayed => PostResult::Replayed,
            Ok(_) => PostResult::Posted,
            Err(_) => PostResult::Rejected,
        };
        self.metrics.payment_settle(outcome);
        self.metrics
            .payment_post_duration(started.elapsed().as_secs_f64(), PostFlow::Settle);
    }
}

/// Overwrite the placeholder header fields the pure builder emits: derive the
/// `period_id` (YYYYMM) and a real `effective_at` from the settlement instant
/// (`None` ⇒ now), stamp the actor from the security context, and mint a fresh
/// correlation id. If `period_id` stayed `""` the post would fail the
/// fiscal-period gate, so this overwrite is mandatory.
fn overwrite_header(
    entry: &mut PostEntry,
    ctx: &SecurityContext,
    effective_at: Option<OffsetDateTime>,
) {
    let eff_instant = effective_at.unwrap_or_else(OffsetDateTime::now_utc);
    let eff_date = to_naive_date(eff_instant);
    entry.effective_at = eff_date;
    entry.period_id = format!("{:04}{:02}", eff_date.year(), eff_date.month());
    entry.posted_by_actor_id = ctx.subject_id();
    entry.correlation_id = Uuid::now_v7();
}

/// Thin `PostLine` adapter over [`ChartIndex::resolve`]: projects a built line's
/// `(account_class, currency, revenue_stream)` onto the key-based resolver. The
/// settlement classes (`CASH_CLEARING` / `UNALLOCATED` / `PSP_FEE_EXPENSE`) are all
/// stream-less, so this resolves on `stream = None`.
fn resolve_line(chart: &ChartIndex, line: &PostLine) -> Option<Uuid> {
    chart.resolve(
        line.account_class,
        &line.currency,
        line.revenue_stream.as_deref(),
    )
}

/// Map one SDK [`PostLine`] + its resolved scale to the engine's [`NewLine`]
/// (mirrors `invoice_post::new_line`).
fn new_line(line: PostLine, scale: u8) -> NewLine {
    NewLine {
        line_id: line.line_id,
        payer_tenant_id: line.payer_tenant_id,
        seller_tenant_id: line.seller_tenant_id,
        resource_tenant_id: line.resource_tenant_id,
        account_id: line.account_id,
        account_class: line.account_class,
        gl_code: line.gl_code,
        side: line.side,
        amount_minor: line.amount_minor,
        currency: line.currency,
        currency_scale: scale,
        invoice_id: line.invoice_id,
        due_date: line.due_date,
        revenue_stream: line.revenue_stream,
        mapping_status: line.mapping_status,
        functional_amount_minor: line.functional_amount_minor,
        functional_currency: line.functional_currency,
        tax_jurisdiction: line.tax_jurisdiction,
        tax_filing_period: line.tax_filing_period,
        tax_rate_ref: line.tax_rate_ref,
        legal_entity_id: None,
        invoice_item_ref: line.invoice_item_ref,
        sku_or_plan_ref: line.sku_or_plan_ref,
        price_id: line.price_id,
        pricing_snapshot_ref: line.pricing_snapshot_ref,
        po_allocation_group: line.po_allocation_group,
        credit_grant_event_type: line.credit_grant_event_type,
        ar_status: line.ar_status,
    }
}
