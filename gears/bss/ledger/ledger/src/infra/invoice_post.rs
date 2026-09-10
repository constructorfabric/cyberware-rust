//! `InvoicePostService` — the orchestrator that drives the pure invoice-post
//! domain (`crate::domain::invoice`) through the foundation engine.
//!
//! It ties the pieces together for one business post:
//! 1. **payer gate** — reject a post for a closed payer (`PAYER_CLOSED`); a
//!    reversal of an already-posted invoice bypasses this (a closed payer must
//!    still be able to have a wrong charge backed out).
//! 2. **map** each item to its GL target (`domain::invoice::mapping::resolve`).
//! 3. **build** the balanced direct-split entry
//!    (`domain::invoice::builder::build_invoice_entry`).
//! 4. **bind** the real chart `account_id` for each line from the provisioned
//!    chart of accounts (the pure builder emits a nil placeholder).
//! 5. **resolve scale** per line and **post** via [`PostingService`].
//! 6. **emit metrics** — `invoice_post` (outcome) + duration on every attempt;
//!    the suspense gauges when the post parks PENDING lines.
//!
//! Lives in `infra` (not `domain`) because it needs repo + posting access; the
//! domain modules it calls stay pure (dylint DE0301). It wraps the `pub`
//! [`PostingService`] + [`ReferenceRepo`] directly (rather than the SDK
//! `LedgerClientV1`, whose in-process impl `LedgerLocalClient::new` is
//! `pub(crate)`), so it is constructible from out-of-crate integration tests.

use std::sync::Arc;
use std::time::Instant;

use bss_ledger_sdk::{MappingStatus, PostEntry, PostLine};
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{FxConfig, RecognitionConfig};
use crate::domain::error::DomainError;
use crate::domain::invoice::builder::{InvoiceItem, PostedInvoice, build_invoice_entry};
use crate::domain::invoice::mapping::{MappedLine, resolve};
use crate::domain::invoice::policy::MissingMappingMode;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::ports::metrics::{LedgerMetricsPort, PostFlow, PostResult};
use crate::domain::recognition::builder::{ScheduleBuilder, ScheduleOutcome, is_immaterial};
use crate::domain::recognition::input::RecognitionInput;
use crate::domain::recognition::ports::{
    DefaultDeferralPolicyResolver, DefaultSspResolver, DefaultVcResolver, RecognitionContext,
};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::payloads::LedgerEntryReversed;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::fx::rate_locker::RateLocker;
use crate::infra::fx::rate_source::RateSource;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::idempotency::IdempotencyGate;
use crate::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use crate::infra::recognition::sidecar::{PlannedScheduleMaterialization, ScheduleBuilderSidecar};
use crate::infra::storage::repo::{FxRepo, PostingPolicyRepo, ReferenceRepo};
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// In-transaction sidecar that emits `billing.ledger.entry.reversed` (architecture
/// §6, VHP-1837) on the explicit reversal path. It rides the post's own
/// transaction (the transactional outbox), so the event row commits atomically
/// with the reversing entry or rolls back with it. It carries the operator
/// `reason` (which is NOT persisted on the entry header) and the original entry
/// id; the reversing entry's id comes from [`PostedFacts`]. A `MAPPING_CORRECTION`
/// does not attach this sidecar — it is a correction, not a §6 reversal.
struct ReversalEventSidecar {
    publisher: Arc<LedgerEventPublisher>,
    ctx: SecurityContext,
    tenant_id: Uuid,
    reverses_entry_id: Uuid,
    reason: String,
}

#[async_trait::async_trait]
impl PostSidecar for ReversalEventSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        _scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        self.publisher
            .publish_entry_reversed(
                &self.ctx,
                txn,
                LedgerEntryReversed {
                    entry_id: posted.entry_id,
                    reverses_entry_id: self.reverses_entry_id,
                    tenant_id: self.tenant_id,
                    reason: self.reason.clone(),
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish entry_reversed: {e}")))
    }
}

/// Write port the journal-entry REST handlers post through. Abstracts the two
/// foundation-engine writes the surface needs — a fresh invoice post (payer
/// gate + map + build + bind) and a pre-built reversal/correction post — so the
/// router tests can stub the post path without a database. The production
/// implementation is [`InvoicePostService`].
#[async_trait::async_trait]
pub trait InvoicePoster: Send + Sync {
    /// Post a fully-recognized invoice (Variant A). `payer_open = false` rejects
    /// with [`DomainError::PayerClosed`] before any ledger effect.
    ///
    /// # Errors
    /// [`DomainError`] on a payer gate / foundation rejection or an infra fault.
    async fn post_invoice(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        inv: &PostedInvoice,
        payer_open: bool,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError>;

    /// Post a pre-bound reversal entry (the caller built it from a read-back
    /// original whose lines already carry real `account_id`s; the payer gate is
    /// intentionally not applied — a reversal must post even for a closed payer).
    /// `reason` is `Some(audit reason)` for an explicit reversal — it is announced
    /// on the `billing.ledger.entry.reversed` event (VHP-1837), not persisted on
    /// the row — or `None` for a mapping-correction's internal reversal leg, which
    /// announces nothing (a correction is not a §6 reversal).
    ///
    /// # Errors
    /// [`DomainError`] on a foundation rejection or an infra fault.
    async fn post_reversal(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        reversal: PostEntry,
        reason: Option<String>,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError>;

    /// Post a corrected re-post (`MAPPING_CORRECTION`) whose lines were freshly
    /// built and carry placeholder nil `account_id`s — so unlike [`post_reversal`]
    /// this binds each line's chart `account_id` from the provisioned chart
    /// before posting. The `source_doc_type` + `reverses_*` header is preserved.
    ///
    /// # Errors
    /// [`DomainError`] on an unmapped account / foundation rejection / infra fault.
    async fn post_correction(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        correction: PostEntry,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError>;
}

/// Orchestrates the invoice-post domain over the foundation engine.
pub struct InvoicePostService {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    metrics: Arc<dyn LedgerMetricsPort>,
    /// ASC 606 recognition tunables (Slice 4): the per-schedule segment ceiling
    /// the pure `ScheduleBuilder` enforces during derivation.
    recognition_config: RecognitionConfig,
    /// The S1 FX lock (Slice 5): resolves + snapshots the locked rate and stamps
    /// the functional translation on a cross-currency invoice entry. Inert for a
    /// single-currency tenant (no functional currency configured).
    rate_locker: RateLocker,
    /// Event publisher, retained so the reversal path can attach a
    /// [`ReversalEventSidecar`] that emits `billing.ledger.entry.reversed` in the
    /// post txn (VHP-1837). The same handle is threaded into the posting engine.
    publisher: Arc<LedgerEventPublisher>,
    /// Tenant posting policy (VHP-1853): the missing-mapping mode (the hard-block
    /// gate below) + the AR-aging buckets. Read effective in the orchestrator
    /// before a post; absent a row the gear default (`SUSPENSE`) applies.
    posting_policy_repo: PostingPolicyRepo,
}

impl InvoicePostService {
    /// Build the service over one database provider, the event publisher
    /// (threaded into the posting engine), the metrics sink, and the recognition
    /// config (the segment ceiling the derivation enforces, Slice 4).
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
        recognition_config: RecognitionConfig,
        fx_config: FxConfig,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        // S1 FX lock: resolve over the local rate store (provider order +
        // staleness from `fx_config`) and freeze a snapshot per cross-currency post.
        let rate_locker = RateLocker::new(
            RateSource::new(FxRepo::new(db.clone()), fx_config).with_metrics(Arc::clone(&metrics)),
            FxRepo::new(db.clone()),
        );
        let posting_policy_repo = PostingPolicyRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db));
        Self {
            posting,
            reference,
            resolver,
            metrics,
            recognition_config,
            rate_locker,
            publisher,
            posting_policy_repo,
        }
    }

    /// Post a fully-recognized invoice (Variant A). `payer_open` is the payer's
    /// lifecycle decision (resolved by the caller): `false` ⇒ the post is
    /// rejected with [`DomainError::PayerClosed`] before any ledger effect.
    ///
    /// On success emits `invoice_post(Posted | Replayed)` + the duration, and —
    /// when the post parked any PENDING line — the suspense gauges. Every
    /// rejection emits `invoice_post(Rejected)` + the duration.
    ///
    /// # Errors
    /// [`DomainError::PayerClosed`] when `!payer_open`; any foundation rejection
    /// (unbalanced/empty/period-closed/account-closed/negative-balance/…) or
    /// [`DomainError::Internal`] on an infrastructure fault.
    pub async fn post_invoice(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        inv: &PostedInvoice,
        payer_open: bool,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
        let started = Instant::now();
        let result = self.post_invoice_inner(ctx, scope, inv, payer_open).await;
        self.record(&result, started, PostFlow::InvoicePost);
        // On a fresh post that parked PENDING lines, surface the suspense backlog
        // as a gauge (age 0 at post time; the tie-out job ages it durably).
        if let Ok(ref posted) = result
            && !posted.replayed
        {
            let pending = pending_line_count(inv);
            if pending > 0 {
                self.metrics
                    .suspense_pending(inv.seller_tenant_id, pending, 0.0);
            }
        }
        result
    }

    /// Build + post the entry (no metrics — the public wrapper records them).
    ///
    /// Slice 4: each item carrying a recognition spec is run through the pure
    /// [`ScheduleBuilder`] derivation FIRST (in [`Self::derive_recognition`]),
    /// which fills the item's `deferred_minor` and yields the schedules to
    /// materialize. The builder then splits each stream's credit into
    /// `CR REVENUE (recognized now)` + `CR CONTRACT_LIABILITY (deferred)`; the
    /// schedules ride a [`ScheduleBuilderSidecar`] threaded into the post so they
    /// materialize in the same serializable transaction (or roll back with the
    /// entry). When NO item defers, the derivation yields no schedules, the
    /// builder emits no Contract-liability line, and the post is threaded a
    /// `None` sidecar — byte-identical to the pre-Slice-4 path.
    async fn post_invoice_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        inv: &PostedInvoice,
        payer_open: bool,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
        if !payer_open {
            return Err(DomainError::PayerClosed(format!(
                "payer {} is closed",
                inv.payer_tenant_id
            )));
        }

        // Recognition derivation (Slice 4): fill each item's deferred amount and
        // collect the schedules to materialize. A derivation Err (SSP / policy /
        // segment-ceiling / PO-gate breach) propagates → the post fails, so no
        // orphan deferral is ever posted. Returns the items with `deferred_minor`
        // set + the per-item-stream schedule plans (empty ⇒ no recognition).
        let (items, schedules) = self.derive_recognition(inv)?;

        // Build over the recognition-augmented items (the deferred split is
        // driven by each item's `deferred_minor`).
        let inv_with_deferral = PostedInvoice {
            items,
            ..inv.clone()
        };
        let mapped: Vec<MappedLine> = inv_with_deferral.items.iter().map(resolve).collect();
        // VHP-1853 missing-mapping policy: when the tenant's effective mode is
        // HARD_BLOCK, an item that resolved to SUSPENSE/PENDING fails the whole
        // post (`ACCOUNT_MAPPING_MISSING`) rather than parking. The default
        // (SUSPENSE) keeps the park-and-reclassify path byte-unchanged.
        if self
            .posting_policy_repo
            .read_effective_policy(scope, inv.seller_tenant_id, OffsetDateTime::now_utc())
            .await
            .map_err(|e| DomainError::Internal(format!("read posting policy: {e}")))?
            .missing_mapping_mode
            == MissingMappingMode::HardBlock
            && mapped
                .iter()
                .any(|m| m.mapping_status == MappingStatus::Pending)
        {
            return Err(DomainError::AccountMappingMissing(format!(
                "invoice {} has an unmapped item and the tenant posting policy is HARD_BLOCK",
                inv.invoice_id
            )));
        }
        let entry = build_invoice_entry(&inv_with_deferral, &mapped);

        // Thread the schedule-materialization sidecar ONLY when ≥1 schedule must
        // materialize; else `None` (byte-identical to the pre-Slice-4 post).
        let sidecar: Option<Arc<dyn PostSidecar>> = if schedules.is_empty() {
            None
        } else {
            Some(Arc::new(ScheduleBuilderSidecar {
                tenant_id: inv.seller_tenant_id,
                payer_tenant_id: inv.payer_tenant_id,
                source_invoice_id: inv.invoice_id.clone(),
                schedules,
                idempotency: IdempotencyGate::new(),
                // The invoice-post mints the FIRST schedule for its keys.
                build_discriminator: None,
            }))
        };
        // S1: the seller tenant's functional currency (S5-F3). `None` (unconfigured)
        // or equal to the entry currency → the RateLocker short-circuits inside
        // `post_prebound` (single-currency, functional NULL — byte-green).
        let functional_ccy = self
            .reference
            .functional_currency(scope, inv.seller_tenant_id)
            .await
            .map_err(|e| DomainError::Internal(format!("functional currency lookup: {e}")))?;
        self.bind_and_post(ctx, scope, entry, sidecar, functional_ccy.as_deref())
            .await
    }

    /// Run the pure recognition derivation for every item that carries a spec,
    /// returning (a) the items with their derived `deferred_minor` filled and
    /// (b) the schedules to materialize (one per deferred item-stream, paired
    /// with the item's `invoice_item_ref`).
    ///
    /// Per item with a [`RecognitionInput`]: build a [`RecognitionContext`] (the
    /// item's ex-tax amount, the invoice period, the invoice gross total, the
    /// currency, the revenue stream) and call [`ScheduleBuilder::derive`]. A
    /// [`ScheduleOutcome::NoDeferral`] leaves `deferred_minor = 0` (no schedule);
    /// a [`ScheduleOutcome::Schedule`] sets `deferred_minor` and is collected.
    ///
    /// Two Slice-4 gates fire here (the derivation owns the trigger conditions,
    /// NOT Slice 1's endpoint):
    /// - **invoice-item-link** (§4.7): a deferred item MUST carry an
    ///   `invoice_item_ref` (the schedule's NOT-NULL `source_invoice_item_ref`,
    ///   the Contract-liability line it draws down). A deferred item without one
    ///   is blocked with [`DomainError::RecognitionWithoutInvoiceLink`] before the
    ///   post (no orphan deferral). [The SSP gate is enforced inside `derive` by
    ///   the `SspResolver`; the policy/segment gates likewise.]
    /// - **PO-allocation-group** (C4, §4.4): a deferred / multi-PO / VC item whose
    ///   PO allocation group cannot be resolved AND cannot be defaulted (Catalog
    ///   default) AND is not R4-exempt ⇒ [`DomainError::MissingPoAllocationGroup`].
    ///   An ordinary point-in-time line auto-defaults / never blocks.
    ///
    /// # Errors
    /// Any block the derivation raises ([`DomainError::SspSnapshotRequired`],
    /// [`DomainError::RecognitionPolicyConflict`], [`DomainError::ScheduleTooLong`]),
    /// the §4.7 invoice-item-link [`DomainError::RecognitionWithoutInvoiceLink`]
    /// gate, or the C4 [`DomainError::MissingPoAllocationGroup`] gate.
    fn derive_recognition(
        &self,
        inv: &PostedInvoice,
    ) -> Result<(Vec<InvoiceItem>, Vec<PlannedScheduleMaterialization>), DomainError> {
        let policy = DefaultDeferralPolicyResolver;
        let ssp = DefaultSspResolver;
        let vc = DefaultVcResolver;
        let builder = ScheduleBuilder::new(&policy, &ssp, &vc, &self.recognition_config);

        // The R4 exemption / invoice-share denominator is the invoice gross.
        let invoice_total_minor = inv.gross_minor();

        let mut items = Vec::with_capacity(inv.items.len());
        let mut schedules = Vec::new();
        for item in &inv.items {
            let mut out_item = item.clone();
            if let Some(input) = &item.recognition {
                // C4 PO-gate: a deferring / multi-PO / VC line needs a resolvable
                // (or defaultable) PO allocation group unless R4-exempt.
                check_po_allocation_group(
                    input,
                    item.amount_minor_ex_tax,
                    invoice_total_minor,
                    &self.recognition_config,
                )?;

                let ctx = RecognitionContext {
                    input,
                    invoice_period_id: &inv.period_id,
                    item_amount_minor_ex_tax: item.amount_minor_ex_tax,
                    invoice_total_minor,
                    currency: &item.currency,
                    revenue_stream: &item.revenue_stream,
                };
                match builder.derive(&ctx)? {
                    ScheduleOutcome::NoDeferral => {}
                    ScheduleOutcome::Schedule(schedule) => {
                        // §4.7 invoice-item-link: a deferred item MUST resolve to
                        // its Contract-liability line via a non-empty
                        // `invoice_item_ref`. Block before the post (no orphan) with
                        // the SPECIFIC `RecognitionWithoutInvoiceLink` (wire
                        // `RECOGNITION_WITHOUT_INVOICE_LINK`, 400) — the §4.7
                        // invariant's own code, not the generic `AmountOutOfRange`.
                        let item_ref = item
                            .invoice_item_ref
                            .as_deref()
                            .filter(|r| !r.is_empty())
                            .ok_or_else(|| {
                            DomainError::RecognitionWithoutInvoiceLink(format!(
                                "deferred recognition line (stream `{}`) must carry an \
                                     invoice_item_ref to anchor its contract-liability schedule",
                                item.revenue_stream
                            ))
                        })?;
                        out_item.deferred_minor = schedule.deferred_minor;
                        schedules.push(PlannedScheduleMaterialization {
                            schedule,
                            source_invoice_item_ref: item_ref.to_owned(),
                        });
                    }
                }
            }
            items.push(out_item);
        }
        Ok((items, schedules))
    }

    /// Post a reversal built from an original entry's [`PostEntry`] projection
    /// (the caller builds it via `domain::invoice::reversal::build_reversal`).
    /// The reversal's lines already carry real `account_id`s (copied from the
    /// original read-back), so only scale resolution + the engine post remain.
    /// The payer gate is intentionally NOT applied — a reversal must post even
    /// for a closed payer.
    ///
    /// # Errors
    /// Any foundation rejection or [`DomainError::Internal`] on an infrastructure
    /// fault.
    pub async fn post_reversal(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        reversal: PostEntry,
        reason: Option<String>,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
        let started = Instant::now();
        // Announce `billing.ledger.entry.reversed` in the post txn (VHP-1837) only
        // for an explicit reversal (`reason` present): the event commits atomically
        // with the reversing entry. A mapping-correction's reversal leg passes
        // `None` (it is a correction, not a §6 reversal) and announces nothing. A
        // reversal carries no recognition sidecar (recognition is materialized on
        // the forward post, never re-derived on a reversal).
        let sidecar: Option<Arc<dyn PostSidecar>> = match (reversal.reverses_entry_id, reason) {
            (Some(orig), Some(reason)) => Some(Arc::new(ReversalEventSidecar {
                publisher: Arc::clone(&self.publisher),
                ctx: ctx.clone(),
                tenant_id: reversal.tenant_id,
                reverses_entry_id: orig,
                reason,
            }) as Arc<dyn PostSidecar>),
            _ => None,
        };
        let result = self
            .post_prebound(ctx, scope, reversal, sidecar, None)
            .await;
        self.record(&result, started, PostFlow::Reversal);
        result
    }

    /// Post a corrected re-post (`MAPPING_CORRECTION`) whose freshly-built lines
    /// carry nil placeholder `account_id`s: binds each from the provisioned chart
    /// (like the invoice-post path) then posts, preserving the correction's
    /// `source_doc_type` + `reverses_*` header. Records `invoice_post` + duration.
    ///
    /// # Errors
    /// [`DomainError`] on an unmapped account / foundation rejection / infra fault.
    pub async fn post_correction(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        correction: PostEntry,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
        let started = Instant::now();
        // A mapping-correction re-post carries no recognition sidecar (it
        // re-books an already-recognized split; recognition schedules are
        // materialized on the original post).
        let result = self.bind_and_post(ctx, scope, correction, None, None).await;
        self.record(&result, started, PostFlow::MappingCorrection);
        result
    }

    /// Resolve each line's chart `account_id` from the provisioned chart, then
    /// resolve scale + post. `sidecar` (Slice 4) materializes recognition
    /// schedules in the same post transaction when present; `None` ⇒ no
    /// in-transaction side effect (the byte-identical non-deferred path + the
    /// reversal/correction paths).
    async fn bind_and_post(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        mut entry: PostEntry,
        sidecar: Option<Arc<dyn PostSidecar>>,
        functional_ccy: Option<&str>,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
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
        self.post_prebound(ctx, scope, entry, sidecar, functional_ccy)
            .await
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's
    /// `NewEntry`/`NewLine`, resolving each line's scale, and post (threading
    /// `sidecar` into the foundation engine).
    async fn post_prebound(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Option<Arc<dyn PostSidecar>>,
        functional_ccy: Option<&str>,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
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
        // S1 FX lock (forward post only — reversal/correction pass `None` and never
        // re-lock; a reversal carries the original rate, spec §4.2 F-8c). When the
        // tenant has a functional currency that DIFFERS from the entry's
        // transaction currency, resolve + snapshot the locked rate and stamp the
        // functional translation on every line; the snapshot id rides the entry
        // (one rate per entry, §4.3) and the journal repo stamps each line from it.
        // Same/None → single-currency: no snapshot, functional stays NULL.
        if let Some(fc) = functional_ccy
            && fc != new_entry.entry_currency
        {
            new_entry.rate_snapshot_ref = self
                .rate_locker
                .lock_and_stamp(
                    scope,
                    new_entry.tenant_id,
                    &mut new_lines,
                    &new_entry.entry_currency,
                    fc,
                    OffsetDateTime::now_utc(),
                )
                .await?;
        }
        self.posting
            .post(ctx, scope, new_entry, new_lines, sidecar)
            .await
    }

    /// Emit `invoice_post(outcome, flow)` + the flow-labelled duration for one
    /// attempt. `flow` keeps reversals/corrections off the invoice-post rate.
    fn record(
        &self,
        result: &Result<bss_ledger_sdk::PostingRef, DomainError>,
        started: Instant,
        flow: PostFlow,
    ) {
        let outcome = match result {
            Ok(r) if r.replayed => PostResult::Replayed,
            Ok(_) => PostResult::Posted,
            Err(_) => PostResult::Rejected,
        };
        self.metrics.invoice_post(outcome, flow);
        self.metrics
            .invoice_post_duration(started.elapsed().as_secs_f64(), flow);
    }
}

/// The production [`InvoicePoster`]: delegates to the inherent methods (which
/// the in-crate integration tests also call on the concrete type). Lets the REST
/// surface hold `Arc<dyn InvoicePoster>` and the router tests stub the writes.
#[async_trait::async_trait]
impl InvoicePoster for InvoicePostService {
    async fn post_invoice(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        inv: &PostedInvoice,
        payer_open: bool,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
        InvoicePostService::post_invoice(self, ctx, scope, inv, payer_open).await
    }

    async fn post_reversal(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        reversal: PostEntry,
        reason: Option<String>,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
        InvoicePostService::post_reversal(self, ctx, scope, reversal, reason).await
    }

    async fn post_correction(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        correction: PostEntry,
    ) -> Result<bss_ledger_sdk::PostingRef, DomainError> {
        InvoicePostService::post_correction(self, ctx, scope, correction).await
    }
}

/// The C4 PO-allocation-group gate (design §4.4, Rev2 N-revrec-3) — owned by
/// Slice 4's recognition orchestration, NOT a mutation of Slice 1's invoice-post
/// endpoint. It fires ONLY for a genuinely ambiguous obligation: a **deferring**
/// (straight-line) / **multi-PO** / **VC** line whose PO allocation group cannot
/// be resolved (absent / blank on the input) AND cannot be defaulted by the
/// Catalog AND is not R4-exempt ⇒ [`DomainError::MissingPoAllocationGroup`].
///
/// An ordinary point-in-time line (the routine-billing common case) is never
/// blocked: it is auto-defaulted (the Catalog default group auto-tags it) and is
/// not a multi-PO / VC obligation, so the gate does not apply. v1 has no Catalog
/// reader, so "cannot be defaulted" reduces to "the input carries no
/// `po_allocation_group`" for an obligation that needs one; a future Catalog-
/// default resolver drops in here without changing the trigger condition.
///
/// The R4 immaterial-one-shot exemption short-circuits the gate (an exempt
/// one-shot recognizes point-in-time and needs no PO group); the materiality
/// threshold itself is re-checked inside the derivation, so here the
/// SKU-eligibility flag is the conservative exemption signal.
///
/// # Errors
/// [`DomainError::MissingPoAllocationGroup`] when the obligation needs a PO
/// allocation group and none is resolvable / defaultable.
fn check_po_allocation_group(
    input: &RecognitionInput,
    item_amount_minor_ex_tax: i64,
    invoice_total_minor: i64,
    config: &RecognitionConfig,
) -> Result<(), DomainError> {
    let needs_group =
        input.timing.is_deferred() || input.multi_po || input.vc_estimate_ref.is_some();
    if !needs_group {
        return Ok(());
    }
    // R4-exempt one-shots recognize point-in-time and need no PO group — but only
    // when ACTUALLY immaterial (the SKU flag AND under the materiality threshold),
    // matching the derivation's R4 check exactly: an over-threshold flagged line
    // still defers, so it must not skip the gate.
    if input.immaterial_one_shot_sku
        && is_immaterial(item_amount_minor_ex_tax, invoice_total_minor, config)
    {
        return Ok(());
    }
    let resolvable = input
        .po_allocation_group
        .as_deref()
        .is_some_and(|g| !g.is_empty());
    if resolvable {
        return Ok(());
    }
    Err(DomainError::MissingPoAllocationGroup(format!(
        "deferring/multi-PO/VC recognition line (policy `{}`) has no resolvable or \
         defaultable po_allocation_group",
        input.policy_ref
    )))
}

/// Thin `PostLine` adapter over [`ChartIndex::resolve`]: projects a built
/// line's `(account_class, currency, revenue_stream)` onto the key-based
/// resolver. Per-stream classes key on the line's stream; the rest resolve
/// stream-less.
fn resolve_line(chart: &ChartIndex, line: &PostLine) -> Option<Uuid> {
    chart.resolve(
        line.account_class,
        &line.currency,
        line.revenue_stream.as_deref(),
    )
}

/// Count the built lines that would park on SUSPENSE/PENDING — the suspense
/// backlog this invoice contributes. Pure over the input (mirrors the mapping
/// resolver) so the gauge needs no read-back.
fn pending_line_count(inv: &PostedInvoice) -> i64 {
    let n = inv
        .items
        .iter()
        .filter(|i| resolve(i).mapping_status == MappingStatus::Pending)
        .count();
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Map one SDK [`PostLine`] + its resolved scale to the engine's [`NewLine`].
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
