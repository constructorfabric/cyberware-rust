//! `SettlementReturnService` — orchestrates the settlement-return domain
//! (`crate::domain::payment::settlement_return`) over the foundation engine. It
//! records a clawed-back receipt as the SYMMETRIC reverse of settle (Model N,
//! D1): `DR UNALLOCATED amount / CR CASH_CLEARING (amount − fee_share) / CR
//! PSP_FEE_EXPENSE fee_share`, decrementing BOTH the original payment's
//! `settled_minor` (by the gross) and `fee_minor` (by the proportional
//! `fee_share`) in the same serializable transaction (via
//! [`SettlementReturnSidecar`]), and publishing `settlement.returned` in-txn.
//!
//! Mirrors [`crate::infra::payment::settle::SettlementService`] minus the
//! drain-on-settle hook (a return unblocks no queued allocation): read the
//! settlement + compute the proportional `fee_share` → build → overwrite header →
//! bind chart → resolve scale + post with the sidecar → emit metrics. The
//! `fee_share = fee_minor × amount / settled_minor` is computed against the
//! CURRENT remaining balances so repeated partial returns stay proportional.
//! There is NO payer gate (a return records money already moved and must land
//! even for a closed payer). Idempotent on `(tenant, SETTLEMENT_RETURN,
//! psp_return_id)` — a re-posted return replays the prior entry with no new
//! ledger effect. Lives in `infra` (needs repo + posting access); the domain
//! builder it calls stays pure (dylint DE0301).

use std::sync::Arc;
use std::time::Instant;

use bss_ledger_sdk::{AccountClass, PostEntry, PostLine, PostingRef};
use chrono::Datelike;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::fx::realized::carried_relief;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::payment::settlement_return::{
    SettlementReturnInput, build_settlement_return_entry,
};
use crate::domain::ports::metrics::{LedgerMetricsPort, PostFlow, PostResult};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::exception::ExceptionRouter;
use crate::infra::payment::sidecar::SettlementReturnSidecar;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::service::{PostSidecar, PostingService};
use crate::infra::storage::repo::{PaymentRepo, ReferenceRepo};
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// Orchestrates the settlement-return domain over the foundation engine.
pub struct SettlementReturnService {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    // The payment counter repo: reads the settlement pre-build to size the
    // proportional `fee_share` for the symmetric reverse (Model N, D1).
    payment_repo: PaymentRepo,
    // The event publisher — threaded into the posting engine AND held so the
    // sidecar can publish `settlement.returned` in-txn.
    publisher: Arc<LedgerEventPublisher>,
    metrics: Arc<dyn LedgerMetricsPort>,
    // Slice 7 Phase 2: routes the `SETTLEMENT_RETURN_OVER_ALLOCATED` stub to a
    // durable close-blocking exception row (ADDITIVE beside the rejection). `None`
    // until `with_exceptions` wires it (so existing constructions are unchanged).
    exceptions: Option<Arc<ExceptionRouter>>,
}

impl SettlementReturnService {
    /// Build the service over one database provider, the event publisher
    /// (threaded into the posting engine + the sidecar's in-txn publish), and the
    /// metrics sink. Mirrors [`crate::infra::payment::settle::SettlementService::new`].
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let payment_repo = PaymentRepo::new(db);
        Self {
            posting,
            reference,
            resolver,
            payment_repo,
            publisher,
            metrics,
            exceptions: None,
        }
    }

    /// Attach the exception router (Slice 7 Phase 2) so a `SETTLEMENT_RETURN_OVER_ALLOCATED`
    /// rejection also opens a durable close-blocking exception row. Additive — the
    /// existing rejection is unchanged.
    #[must_use]
    pub fn with_exceptions(mut self, exceptions: Arc<ExceptionRouter>) -> Self {
        self.exceptions = Some(exceptions);
        self
    }

    /// Post a settlement return (clawback) as the symmetric reverse of settle
    /// (Model N): `DR UNALLOCATED amount / CR CASH_CLEARING (amount − fee_share) /
    /// CR PSP_FEE_EXPENSE fee_share`, decrementing BOTH the original payment's
    /// `settled_minor` and `fee_minor` in the same transaction. Idempotent on
    /// `(tenant, SETTLEMENT_RETURN, psp_return_id)`.
    ///
    /// On success emits `settlement_return(Posted | Replayed)` + the
    /// payment-post duration; every rejection emits `settlement_return(Rejected)`
    /// + the duration.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] for a non-positive amount;
    /// [`DomainError::SettlementReturnOverAllocated`] when the return (or its fee
    /// reverse) exceeds the still-returnable settled amount;
    /// [`DomainError::AccountClosed`] when a required class (`UNALLOCATED` /
    /// `CASH_CLEARING` / `PSP_FEE_EXPENSE`) is not provisioned; any foundation
    /// rejection (period-closed / negative-balance / …) or
    /// [`DomainError::Internal`] on an infrastructure fault (incl. a return
    /// against a payment that never settled).
    pub async fn return_settlement(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        input: SettlementReturnInput,
    ) -> Result<PostingRef, DomainError> {
        let started = Instant::now();
        let result = self.return_inner(ctx, scope, input).await;
        self.record(&result, started);
        result
    }

    /// Build + post the settlement-return entry (no metrics — the public wrapper
    /// records them).
    async fn return_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        input: SettlementReturnInput,
    ) -> Result<PostingRef, DomainError> {
        // `fee_share` is sized off the payment's CURRENT remaining
        // `(settled, fee)`, read OUT-OF-TXN (the chart + scale reads the build
        // needs likewise run out-of-txn — Postgres forbids them inside the post's
        // serializable transaction), while the sidecar decrements those counters
        // IN-TXN. Two CONCURRENT partial returns reading the same pre-decrement
        // snapshot would size identical fee shares; the second to commit can then
        // trip the `fee_minor <= settled_minor` cap CHECK (a FALSE
        // `SettlementReturnOverAllocated`) even though, applied serially, it fits.
        //
        // Recompute + retry on that cap: a GENUINE over-return re-fails on an
        // UNCHANGED snapshot and propagates immediately; a concurrent shift
        // re-sizes `fee_share` against the now-committed counters and succeeds.
        // The row-level lock the counter UPDATEs take already serializes the
        // decrements — this loop only re-aligns the out-of-txn `fee_share` read
        // with the committed state. Bounded so a true over-return can't spin.
        const MAX_RECOMPUTE: u32 = 8;
        let mut prev_snapshot: Option<(i64, i64)> = None;
        for _ in 0..=MAX_RECOMPUTE {
            // 1. Read the settlement and size the proportional fee slice this
            //    return reverses (Model N, D1): `fee_share = fee × amount /
            //    settled` (i128 intermediate; against the CURRENT remaining
            //    balances). Also returns the `(settled, fee)` snapshot the retry
            //    guard below compares to tell a genuine over-return from a race.
            let (fee_share_minor, snapshot) = self.fee_share(scope, &input).await?;

            // 2. Build the balanced symmetric-reverse entry (validates amount > 0
            //    and 0 <= fee_share <= amount).
            let mut entry = build_settlement_return_entry(&input, fee_share_minor)?;

            // 3. Overwrite the placeholder header fields the pure builder emits.
            overwrite_header(&mut entry, ctx, input.effective_at);

            // 4. Bind each line's real chart account_id from the provisioned chart.
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

            // 4b. Stamp the functional carry-forward on the bound legs (Slice 5,
            //     decision 8): a cross-currency settle stamped the UNALLOCATED
            //     pool's functional at the locked rate, so this symmetric reverse
            //     relieves that SAME carried basis (no new lock, no realized FX).
            //     Single-currency pool ⇒ leaves functional NULL on every leg.
            self.stamp_fx_carry_forward(scope, &input, fee_share_minor, &mut entry.lines)
                .await?;

            // 5. Post, threading the return sidecar so BOTH `settled_minor` and
            //    `fee_minor` are decremented — and `settlement.returned` is
            //    published — atomically with the journal entry (or rolled back).
            let sidecar: Arc<dyn PostSidecar> = Arc::new(SettlementReturnSidecar {
                tenant: input.tenant_id,
                payment_id: input.payment_id.clone(),
                psp_return_id: input.psp_return_id.clone(),
                amount_minor: input.amount_minor,
                fee_share_minor,
                currency: input.currency.clone(),
                publisher: Arc::clone(&self.publisher),
                ctx: ctx.clone(),
            });
            match self.post_bound(ctx, scope, entry, sidecar).await {
                Ok(reference) => return Ok(reference),
                // A cap rejection on a snapshot that CHANGED since our last
                // attempt is a concurrent partial return that shifted the
                // counters under us — re-size and retry. The SAME snapshot twice
                // is a genuine over-return — propagate it.
                Err(DomainError::SettlementReturnOverAllocated(_))
                    if prev_snapshot != Some(snapshot) =>
                {
                    prev_snapshot = Some(snapshot);
                }
                // The SAME snapshot twice is a GENUINE over-return (the return truly
                // cannot fit, not a transient race): Slice 7 Phase 2 — ADDITIVELY open
                // a durable close-blocking exception row beside the rejection before
                // propagating it. (The contention-exhaustion fall-through below is a
                // retryable timeout, NOT a genuine over-allocation, so it does NOT
                // route — that would be a spurious close-blocker on a return the PSP
                // retries successfully.)
                Err(e @ DomainError::SettlementReturnOverAllocated(_)) => {
                    if let Some(ex) = &self.exceptions {
                        ex.route(
                            input.tenant_id,
                            crate::domain::exception::ExceptionType::SettlementReturnOverAllocated,
                            &input.psp_return_id,
                            Some(serde_json::json!({
                                "psp_return_id": input.psp_return_id,
                                "payment_id": input.payment_id,
                            })),
                        )
                        .await;
                    }
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        // Exhausted recomputes under sustained contention: surface the cap rejection
        // rather than spin (the caller / PSP webhook can retry). NOT routed to the
        // exception queue — this is a transient contention timeout, not a genuine
        // over-allocation (see the genuine arm above).
        Err(DomainError::SettlementReturnOverAllocated(format!(
            "settlement return on payment {} kept losing a race to concurrent \
             returns after {MAX_RECOMPUTE} recomputes",
            input.payment_id
        )))
    }

    /// Compute the proportional fee slice a return of `amount` reverses (Model N,
    /// D1): `fee_share = fee_minor × amount_minor / settled_minor`, using an i128
    /// intermediate so the product can't overflow i64. Reads the settlement
    /// out-of-txn before the build, and returns the `(settled_minor, fee_minor)`
    /// snapshot alongside `fee_share` so the caller's recompute-on-conflict loop
    /// can tell a concurrent counter shift from a genuine over-return.
    ///
    /// # Errors
    /// [`DomainError`] when the settlement is ABSENT or has `settled_minor == 0`
    /// — a return against a payment that never settled (or settled nothing) is an
    /// upstream contract violation (the cash it claims to claw back never moved),
    /// mirroring [`crate::infra::payment::chargeback::ChargebackService`]'s
    /// missing-settlement guard.
    async fn fee_share(
        &self,
        scope: &AccessScope,
        input: &SettlementReturnInput,
    ) -> Result<(i64, (i64, i64)), DomainError> {
        let settlement = self
            .payment_repo
            .read_settlement(scope, input.tenant_id, &input.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read settlement: {e}")))?
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "settlement return on payment {} has no settlement row \
                     (cannot size the fee share)",
                    input.payment_id
                ))
            })?;
        // The return must be denominated in the SETTLED currency: this read sizes
        // the fee share off `settlement.{fee,settled}_minor` and the sidecar
        // decrements those same counters, while the journal legs post in
        // `input.currency`. A mismatch (mistyped or malicious) would post foreign
        // legs against the original payment's counters — reject before sizing
        // (mirrors `AllocateService`'s settlement-currency gate).
        if settlement.currency != input.currency {
            return Err(DomainError::CurrencyMismatch(format!(
                "settlement-return currency {} != settled currency {} for payment {}",
                input.currency, settlement.currency, input.payment_id
            )));
        }
        // Guard against a zero settled total: it would both be a contract
        // violation (nothing was ever settled) and a divide-by-zero below.
        if settlement.settled_minor <= 0 {
            return Err(DomainError::Internal(format!(
                "settlement return on payment {} has settled_minor={} \
                 (cannot size the fee share against a zero/negative settlement)",
                input.payment_id, settlement.settled_minor
            )));
        }
        // i128 intermediate: `fee × amount` can exceed i64 for large minor-unit
        // values; the quotient is provably back in i64 range (`fee_share <= fee
        // <= settled <= i64::MAX`), so `try_from` never errors here — guard it
        // defensively rather than an unchecked `as` cast (clippy
        // cast_possible_truncation).
        let fee_share_raw = i128::from(settlement.fee_minor) * i128::from(input.amount_minor)
            / i128::from(settlement.settled_minor);
        let fee_share_minor = i64::try_from(fee_share_raw).map_err(|_| {
            DomainError::Internal(format!(
                "settlement return fee_share {fee_share_raw} overflows i64 (payment {})",
                input.payment_id
            ))
        })?;
        // The `(settled, fee)` snapshot the build was sized against — the retry
        // guard compares it across attempts to separate a race from an over-return.
        Ok((
            fee_share_minor,
            (settlement.settled_minor, settlement.fee_minor),
        ))
    }

    /// Stamp the functional carry-forward on the symmetric-reverse legs (Slice 5,
    /// decision 8). A cross-currency settle stamped the `UNALLOCATED` pool's
    /// functional at the locked rate; this return claws `amount` back out of that
    /// pool, so it relieves the pool's functional at the SAME carried basis (WAC
    /// pro-rata) and carries that basis onto the cash + fee legs — a reversal, NOT
    /// a realized-FX point, so the functional column nets to zero with no
    /// `FX_GAIN_LOSS` line. The cash leg takes the exact residual so
    /// `SUM(DR.functional) = SUM(CR.functional)` holds under banker's rounding.
    ///
    /// A single-currency pool (functional NULL) leaves functional NULL on every
    /// leg (the projector + the `check_entry_balanced` trigger then treat the entry
    /// single-currency). A non-positive pool or an over-claw skips the carry-forward
    /// — the post is rejected on the transaction balance (projector `NegativeBalance`
    /// / the `settled_minor` cap) before it commits, so leaving functional NULL
    /// drifts nothing.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a carried-read fault or a [`carried_relief`]
    /// misuse (a malformed grain value — an internal invariant breach).
    async fn stamp_fx_carry_forward(
        &self,
        scope: &AccessScope,
        input: &SettlementReturnInput,
        fee_share_minor: i64,
        lines: &mut [PostLine],
    ) -> Result<(), DomainError> {
        let pool = self
            .payment_repo
            .read_unallocated_carried(
                scope,
                input.tenant_id,
                input.payer_tenant_id,
                &input.currency,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("read unallocated carried: {e}")))?;

        // Single-currency pool ⇒ leave functional NULL on every leg.
        let (Some(pool_functional), Some(functional_ccy)) =
            (pool.functional_balance_minor, pool.functional_currency)
        else {
            return Ok(());
        };

        // A non-positive pool or an over-claw ⇒ skip; the post is rejected on the
        // transaction balance before it commits, so leaving functional NULL is safe.
        if pool.balance_minor <= 0 || input.amount_minor > pool.balance_minor {
            return Ok(());
        }

        // Relieve the pool (DR UNALLOCATED) at its WAC, then split the SAME basis
        // across the CR legs so the functional column nets to zero: the fee leg
        // pro-rata, the cash leg the exact residual.
        let dr_func = carried_relief(pool_functional, pool.balance_minor, input.amount_minor)
            .map_err(|e| {
                DomainError::Internal(format!("settlement-return FX carry-forward: {e}"))
            })?;
        let fee_func = if fee_share_minor > 0 {
            carried_relief(pool_functional, pool.balance_minor, fee_share_minor).map_err(|e| {
                DomainError::Internal(format!("settlement-return FX fee carry-forward: {e}"))
            })?
        } else {
            0
        };
        let cash_func = dr_func - fee_func;

        for line in lines.iter_mut() {
            let func = match line.account_class {
                AccountClass::Unallocated => dr_func,
                AccountClass::PspFeeExpense => fee_func,
                AccountClass::CashClearing => cash_func,
                // The builder emits only those three classes; an unexpected leg
                // would surface as FUNCTIONAL_PARTIAL at the balance trigger
                // (fail loud, never silently drift).
                _ => continue,
            };
            line.functional_amount_minor = Some(func);
            line.functional_currency = Some(functional_ccy.clone());
        }
        Ok(())
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's
    /// `NewEntry`/`NewLine`, resolving each line's scale, and post with the
    /// settlement-return sidecar.
    async fn post_bound(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Arc<dyn PostSidecar>,
    ) -> Result<PostingRef, DomainError> {
        let new_entry = NewEntry {
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
            // Settlement-return carries the pool's functional basis forward (Slice 5,
            // decision 8) — no NEW rate is locked, so the entry stamps no fresh
            // snapshot ref; the carried basis traces to the original settle's
            // snapshot through the relieved UNALLOCATED grain.
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
        self.posting
            .post(ctx, scope, new_entry, new_lines, Some(sidecar))
            .await
    }

    /// Emit `settlement_return(outcome)` + the `SettlementReturn`-labelled
    /// payment-post duration for one attempt.
    fn record(&self, result: &Result<PostingRef, DomainError>, started: Instant) {
        let outcome = match result {
            Ok(r) if r.replayed => PostResult::Replayed,
            Ok(_) => PostResult::Posted,
            Err(_) => PostResult::Rejected,
        };
        self.metrics.settlement_return(outcome);
        self.metrics
            .payment_post_duration(started.elapsed().as_secs_f64(), PostFlow::SettlementReturn);
    }
}

/// Overwrite the placeholder header fields the pure builder emits: derive the
/// `period_id` (YYYYMM) and a real `effective_at` from the return instant
/// (`None` ⇒ now), stamp the actor from the security context, and mint a fresh
/// correlation id. Mandatory — a `""` `period_id` would fail the fiscal-period
/// gate. Mirrors [`crate::infra::payment::settle`]'s overwrite.
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

/// Thin `PostLine` adapter over [`ChartIndex::resolve`]. The return classes
/// (`UNALLOCATED` / `CASH_CLEARING`) are stream-less, so this resolves on
/// `stream = None`.
fn resolve_line(chart: &ChartIndex, line: &PostLine) -> Option<Uuid> {
    chart.resolve(
        line.account_class,
        &line.currency,
        line.revenue_stream.as_deref(),
    )
}

/// Map one SDK [`PostLine`] + its resolved scale to the engine's [`NewLine`]
/// (mirrors `settle::new_line`).
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
