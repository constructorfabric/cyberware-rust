//! `UnrealizedRevaluationRun` (design §3.6 / §4.5) — Phase 3 Group H2/H3. A
//! Mode-B ledger (= ledger of record, decision 4) remeasures, at period end,
//! every open foreign-currency **monetary** grain `{AR, UNALLOCATED,
//! REUSABLE_CREDIT}` at the period-end rate against its carried functional value
//! and posts the difference as a dedicated **functional-only** entry
//! (`amount_minor = 0` on every line): one adjusting line per moved grain (it
//! moves the grain's functional carrying value) plus one net `FX_UNREALIZED`
//! contra line so the functional column balances. `CONTRACT_LIABILITY` is
//! excluded (non-monetary, ASC 830 / IAS 21).
//!
//! - **One entry per `(tenant, period, scope, payer)`.** A journal entry may span
//!   only ONE payer tenant (`validate_balanced_entry`'s `MixedPayer` invariant),
//!   so a scope's grains are grouped by payer and each payer gets its own entry.
//!   The idempotency family is `(tenant, FX_REVALUATION, period_id:scope:payer)` —
//!   `PostingService::post` claims it from the entry's
//!   `source_doc_type`/`source_business_id`, so a re-run is a clean replay.
//! - **Reversal** (H3, decision 7): the run reads its own posted revaluation
//!   entries for `period:scope:` (every payer) and posts the **negation** as fresh
//!   `FX_REVAL_REVERSAL` JEs in the next OPEN period — not a line-negation of the
//!   original by id, and posting cleanly after the original period closes (it
//!   targets a different, open period). Only realized FX is permanent.
//! - **Mode gate** (H4): `FxConfig.revaluation_enabled` — fail-safe OFF so a
//!   Mode-A tenant never double-counts vs its ERP. A disabled run is a no-op.
//!
//! Money-critical (the remeasure math is the pure
//! [`crate::domain::fx::revaluation`]); lives in `infra` because it needs repo +
//! posting + rate-source access. The grain enumeration is a cache scan (never a
//! `journal_line` rescan) — `PaymentRepo::list_*_to_revalue`.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::FxConfig;
use crate::domain::error::DomainError;
use crate::domain::fx::revaluation::{
    RevaluationLine, RevaluationPosition, RevaluationScope, remeasure,
};
use crate::domain::fx::translate::translate_amount;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{LineRecord, NewEntry, NewLine};
use crate::domain::period::{period_end_utc, period_start_utc};
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::payloads::{LedgerFxRevaluationCompleted, LedgerFxRevaluationReversed};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::fx::rate_source::RateSource;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use crate::infra::storage::repo::{
    FxRepo, JournalRepo, PaymentRepo, RecognitionRepo, ReferenceRepo, RevaluationGrain,
};
use time::OffsetDateTime;

/// Origin literal stamped on revaluation posts.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// The outcome of one `(tenant, period, scope)` revaluation or reversal attempt
/// (aggregated across the scope's per-payer entries).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeStatus {
    /// Mode-B gate is off (`revaluation_enabled = false`) — no-op.
    Disabled,
    /// No cross-currency grain in scope, or every grain already at the period-end
    /// rate (net zero) — nothing posted.
    NothingToPost,
    /// Revaluation entries posted (`entries` fresh; `grains` moved across them).
    /// A re-run that fully replays reports `entries = 0`.
    Posted { entries: usize, grains: usize },
    /// (Reversal) no original `FX_REVALUATION` entry exists for the period:scope —
    /// nothing to reverse.
    NothingToReverse,
    /// (Reversal) no later OPEN period is available to post the reversal into — the
    /// run retries on its next tick (the `PeriodOpenJob` must open the next period).
    ReversalDeferred,
    /// (Reversal) `FX_REVAL_REVERSAL` entries posted in the next OPEN period
    /// (`entries` fresh; a full replay reports `0`).
    Reversed { entries: usize },
}

/// One scope's outcome within a period run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeOutcome {
    pub scope: RevaluationScope,
    pub status: ScopeStatus,
}

/// The result of a full period run across the three monetary scopes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevaluationReport {
    pub period_id: String,
    pub scopes: Vec<ScopeOutcome>,
}

/// Map a [`RevaluationScope`] to its grain `account_class` (the line the
/// adjusting leg posts on, which routes the functional delta to the grain).
const fn scope_account_class(scope: RevaluationScope) -> AccountClass {
    match scope {
        RevaluationScope::Ar => AccountClass::Ar,
        RevaluationScope::Unallocated => AccountClass::Unallocated,
        RevaluationScope::ReusableCredit => AccountClass::ReusableCredit,
    }
}

/// Mode-B period-end unrealized-revaluation runner.
pub struct UnrealizedRevaluationRun {
    posting: PostingService,
    repo: PaymentRepo,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    rate_source: RateSource,
    /// Read-back of the original revaluation entries for the reversal (H3).
    journal: JournalRepo,
    /// Resolve the next OPEN period for the reversal (`current_open_period`).
    recognition: RecognitionRepo,
    /// Event publisher for the in-txn `fx.revaluation_completed` / `_reversed`
    /// outbox events (threaded into the per-post sidecars). `Arc`-shared with the
    /// posting engine (which owns the `entry.posted` producer).
    publisher: Arc<LedgerEventPublisher>,
}

impl UnrealizedRevaluationRun {
    /// Build the runner over one database provider, the event publisher (threaded
    /// into the posting engine), and the FX config (rate source + Mode-B gate).
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        fx: FxConfig,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let repo = PaymentRepo::new(db.clone());
        let journal = JournalRepo::new(db.clone());
        let recognition = RecognitionRepo::new(db.clone());
        let rate_source = RateSource::new(FxRepo::new(db), fx);
        Self {
            posting,
            repo,
            reference,
            resolver,
            rate_source,
            journal,
            recognition,
            publisher,
        }
    }

    /// Attach a metrics sink so the runner's period-end rate resolves emit the
    /// provider-fallback counter (`ledger_fx_provider_fallback_total{provider}`)
    /// when a non-primary provider is chosen at period end. Builder-style, matching
    /// the gear's `with_metrics` convention; without it the runner records no
    /// fallback metric (the `fallback_order` is still stamped on the snapshot).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn LedgerMetricsPort>) -> Self {
        self.rate_source = self.rate_source.with_metrics(metrics);
        self
    }

    /// Run the unrealized revaluation for `period_id` across the three monetary
    /// scopes `{AR, UNALLOCATED, REUSABLE_CREDIT}`, posting one entry per
    /// `(scope, payer)` that has a net movement. A disabled run (Mode-A) is a
    /// no-op across all scopes.
    ///
    /// # Errors
    /// Propagates the first scope's [`DomainError`] (e.g. a `FxRateUnavailable`
    /// from the period-end rate resolve, or a `PeriodClosed` if `period_id` is
    /// not open) — the caller (job / REST) isolates per tenant.
    pub async fn run_period(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        enabled: bool,
    ) -> Result<RevaluationReport, DomainError> {
        let mut scopes = Vec::with_capacity(RevaluationScope::all().len());
        for rev_scope in RevaluationScope::all() {
            let status = self
                .run_scope(ctx, scope, tenant, period_id, rev_scope, enabled)
                .await?;
            scopes.push(ScopeOutcome {
                scope: rev_scope,
                status,
            });
        }
        Ok(RevaluationReport {
            period_id: period_id.to_owned(),
            scopes,
        })
    }

    /// Run the revaluation for one `(tenant, period, scope)`: enumerate the open
    /// cross-currency grains, group them by payer (one entry per payer — the
    /// `MixedPayer` invariant), remeasure each at the period-end rate, and post a
    /// functional-only entry per moved payer. Idempotent via the engine's dedup on
    /// `(tenant, FX_REVALUATION, period_id:scope:payer)`.
    ///
    /// # Errors
    /// [`DomainError`] on a rate resolve, a malformed period id, a missing
    /// `FX_UNREALIZED` account, or a post failure.
    async fn run_scope(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        rev_scope: RevaluationScope,
        enabled: bool,
    ) -> Result<ScopeStatus, DomainError> {
        // H4 — Mode gate: a Mode-A (or unset) tenant must not revalue. `enabled` is
        // the caller's resolved per-tenant Mode-B decision (VHP-1986).
        if !enabled {
            return Ok(ScopeStatus::Disabled);
        }

        // 1. Enumerate the open cross-currency grains in scope (cache scan, never a
        //    journal_line rescan), grouped by payer (one entry per payer).
        let grains = self.list_grains(scope, tenant, rev_scope).await?;
        if grains.is_empty() {
            return Ok(ScopeStatus::NothingToPost);
        }
        let by_payer = group_by_payer(grains);

        // 2. Period-end instant: the rate in effect at period close (design §4.5).
        let as_of = period_end_utc(period_id).ok_or_else(|| {
            DomainError::Internal(format!("malformed period_id for revaluation: {period_id}"))
        })?;
        let chart = load_chart(&self.reference, scope, tenant).await?;
        let mut rate_cache: BTreeMap<String, i64> = BTreeMap::new();

        let mut entries = 0usize;
        let mut moved = 0usize;
        for (payer, payer_grains) in by_payer {
            if let Some(grains_moved) = self
                .run_payer(
                    ctx,
                    scope,
                    tenant,
                    period_id,
                    rev_scope,
                    payer,
                    &payer_grains,
                    as_of,
                    &chart,
                    &mut rate_cache,
                )
                .await?
            {
                entries += 1;
                moved += grains_moved;
            }
        }
        if entries == 0 {
            Ok(ScopeStatus::NothingToPost)
        } else {
            Ok(ScopeStatus::Posted {
                entries,
                grains: moved,
            })
        }
    }

    /// Remeasure + post one payer's grains within a scope. Returns the count of
    /// grains moved (the entry was posted), or `None` when the payer nets to zero
    /// (nothing posted).
    #[allow(clippy::too_many_arguments)]
    async fn run_payer(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        rev_scope: RevaluationScope,
        payer: Uuid,
        grains: &[RevaluationGrain],
        as_of: OffsetDateTime,
        chart: &ChartIndex,
        rate_cache: &mut BTreeMap<String, i64>,
    ) -> Result<Option<usize>, DomainError> {
        // One functional currency per tenant/legal-entity (F5). A grain set that
        // mixes functional currencies is a multi-LE invariant breach (deferred,
        // decision 5) — fail loud rather than post a mixed-functional entry.
        // The caller (`run_scope`) only routes non-empty payer groups here, but
        // propagate defensively rather than index — an empty set is no movement.
        let functional_ccy = grains
            .first()
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "revaluation called with no grains for tenant {tenant} payer {payer} scope {}",
                    rev_scope.as_token()
                ))
            })?
            .functional_currency
            .clone();
        if grains
            .iter()
            .any(|g| g.functional_currency != functional_ccy)
        {
            return Err(DomainError::Internal(format!(
                "revaluation grains mix functional currencies for tenant {tenant} payer {payer} \
                 scope {} (multi-LE not supported, decision 5)",
                rev_scope.as_token()
            )));
        }

        // Remeasure each grain at the period-end rate (one rate per txn currency).
        let mut positions: Vec<RevaluationPosition> = Vec::with_capacity(grains.len());
        for g in grains {
            let remeasured = if g.currency == functional_ccy {
                g.balance_minor
            } else {
                let rate_micro = if let Some(r) = rate_cache.get(&g.currency) {
                    *r
                } else {
                    let resolved = self
                        .rate_source
                        .resolve(scope, tenant, &g.currency, &functional_ccy, as_of)
                        .await?;
                    rate_cache.insert(g.currency.clone(), resolved.rate_micro);
                    resolved.rate_micro
                };
                translate_amount(g.balance_minor, rate_micro)
                    .map_err(|e| DomainError::Internal(format!("revaluation translate: {e}")))?
            };
            positions.push(RevaluationPosition {
                normal_side: rev_scope.normal_side(),
                carried_functional_minor: g.functional_balance_minor,
                remeasured_functional_minor: remeasured,
            });
        }

        let reval = remeasure(&positions)
            .map_err(|e| DomainError::Internal(format!("revaluation remeasure: {e}")))?;
        let Some(fx_unrealized) = reval.fx_unrealized else {
            return Ok(None); // every grain at the period-end rate: nothing to post
        };

        // Build the functional-only entry: one adjusting line per moved grain + the
        // net FX_UNREALIZED contra (all carrying THIS payer's tenant id).
        let mut lines: Vec<NewLine> = Vec::with_capacity(grains.len() + 1);
        let mut moved = 0usize;
        for (g, leg) in grains.iter().zip(&reval.grain_lines) {
            let Some(leg) = leg else { continue };
            let scale = self.scale_for(scope, tenant, &g.currency).await?;
            lines.push(Self::grain_line(g, rev_scope, leg, scale));
            moved += 1;
        }
        let fx_account = chart
            .resolve(AccountClass::FxUnrealized, &functional_ccy, None)
            .ok_or_else(|| {
                DomainError::AccountClosed(format!(
                    "no provisioned FX_UNREALIZED account for functional currency {functional_ccy}"
                ))
            })?;
        let fx_scale = self.scale_for(scope, tenant, &functional_ccy).await?;
        lines.push(Self::fx_unrealized_line(
            payer,
            fx_account,
            &functional_ccy,
            &fx_unrealized,
            fx_scale,
        ));

        let entry = Self::build_entry(
            ctx,
            tenant,
            period_id,
            period_end_naive(period_id),
            &functional_ccy,
            SourceDocType::FxRevaluation,
            &business_id(period_id, rev_scope, payer),
            None,
            None,
        );
        // Publish `billing.ledger.fx.revaluation_completed` IN the post txn (the
        // transactional outbox) so the event commits atomically with the entry, or
        // rolls back with it. Reached only on the fresh-claim path (a replay returns
        // before the sidecar), so it fires once per posted revaluation entry.
        let sidecar: Arc<dyn PostSidecar> = Arc::new(RevaluationCompletedSidecar {
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
            tenant_id: tenant,
            period_id: period_id.to_owned(),
            scope: rev_scope.as_token().to_owned(),
            payer_id: payer,
            functional_currency: functional_ccy.clone(),
            fx_unrealized_minor: fx_unrealized_signed(
                fx_unrealized.side,
                fx_unrealized.functional_minor,
            ),
            grains_moved: i32::try_from(moved).unwrap_or(i32::MAX),
            posted_at_utc: entry.posted_at_utc,
        });
        self.posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await?;
        Ok(Some(moved))
    }

    /// Reverse the unrealized revaluation for `reval_period_id` across the three
    /// monetary scopes — fresh `FX_REVAL_REVERSAL` JEs in the next OPEN period that
    /// negate the original revaluation entries (decision 7). A disabled run is a
    /// no-op. Idempotent via the engine's dedup on
    /// `(tenant, FX_REVAL_REVERSAL, reval_period_id:scope:payer)`.
    ///
    /// # Errors
    /// Propagates the first scope's [`DomainError`] — the caller isolates per
    /// tenant.
    pub async fn reverse_period(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        reval_period_id: &str,
        enabled: bool,
    ) -> Result<RevaluationReport, DomainError> {
        let mut scopes = Vec::with_capacity(RevaluationScope::all().len());
        for rev_scope in RevaluationScope::all() {
            let status = self
                .reverse_scope(ctx, scope, tenant, reval_period_id, rev_scope, enabled)
                .await?;
            scopes.push(ScopeOutcome {
                scope: rev_scope,
                status,
            });
        }
        Ok(RevaluationReport {
            period_id: reval_period_id.to_owned(),
            scopes,
        })
    }

    /// Reverse one `(tenant, reval_period, scope)`: read every original per-payer
    /// `FX_REVALUATION` entry for `reval_period:scope:`, resolve the next OPEN
    /// period, and post each one's negation as a fresh `FX_REVAL_REVERSAL` JE
    /// there. Deferred when no later OPEN period exists (the reval period must have
    /// closed and a successor opened).
    ///
    /// # Errors
    /// [`DomainError`] on a read fault, a malformed line, or a post failure.
    async fn reverse_scope(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        reval_period_id: &str,
        rev_scope: RevaluationScope,
        enabled: bool,
    ) -> Result<ScopeStatus, DomainError> {
        if !enabled {
            return Ok(ScopeStatus::Disabled);
        }

        // 1. Find every original per-payer revaluation entry to negate.
        let prefix = business_id_prefix(reval_period_id, rev_scope);
        let originals = self
            .journal
            .list_entries_with_lines_by_doc_prefix(
                scope,
                tenant,
                SourceDocType::FxRevaluation.as_str(),
                &prefix,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("list revaluation entries: {e}")))?;
        if originals.is_empty() {
            return Ok(ScopeStatus::NothingToReverse);
        }

        // 2. Resolve the next OPEN period (the lowest open period_id). The reversal
        //    posts there only once the reval period has CLOSED — i.e. the current
        //    open period is strictly later than the reval period. Otherwise defer.
        let Some(open_period) = self
            .recognition
            .current_open_period(scope, tenant)
            .await
            .map_err(|e| DomainError::Internal(format!("current open period: {e}")))?
        else {
            return Ok(ScopeStatus::ReversalDeferred);
        };
        if open_period.as_str() <= reval_period_id {
            return Ok(ScopeStatus::ReversalDeferred);
        }
        let effective_at = period_start_utc(&open_period)
            .map_or_else(|| to_naive_date(OffsetDateTime::UNIX_EPOCH), to_naive_date);

        // 3. Post the negation of each original as a fresh FX_REVAL_REVERSAL JE in
        //    the next open period (idempotent on the original's business_id).
        let mut entries = 0usize;
        for original in &originals {
            let mut lines: Vec<NewLine> = Vec::with_capacity(original.lines.len());
            for r in &original.lines {
                lines.push(reverse_line(r)?);
            }
            // One payer per entry (the `MixedPayer` invariant) — read it off any
            // line; the reversal's net FX_UNREALIZED is the negation of the
            // original's (the reversal flips each leg's side).
            let payer_id = original.lines.first().map_or(tenant, |r| r.payer_tenant_id);
            let fx_signed = reversal_fx_unrealized_signed(&lines);
            let entry = Self::build_entry(
                ctx,
                tenant,
                &open_period,
                effective_at,
                &original.entry_currency,
                SourceDocType::FxRevalReversal,
                &original.source_business_id,
                Some(original.entry_id),
                Some(reval_period_id.to_owned()),
            );
            // Publish `billing.ledger.fx.revaluation_reversed` IN the post txn (the
            // transactional outbox), atomically with the reversal entry. Reached only
            // on the fresh-claim path, so it fires once per posted reversal entry.
            let sidecar: Arc<dyn PostSidecar> = Arc::new(RevaluationReversedSidecar {
                publisher: Arc::clone(&self.publisher),
                ctx: ctx.clone(),
                tenant_id: tenant,
                reverses_entry_id: original.entry_id,
                reval_period_id: reval_period_id.to_owned(),
                reversal_period_id: open_period.clone(),
                scope: rev_scope.as_token().to_owned(),
                payer_id,
                functional_currency: original.entry_currency.clone(),
                fx_unrealized_minor: fx_signed,
                posted_at_utc: entry.posted_at_utc,
            });
            self.posting
                .post(ctx, scope, entry, lines, Some(sidecar))
                .await?;
            entries += 1;
        }
        Ok(ScopeStatus::Reversed { entries })
    }

    /// Enumerate the open cross-currency grains for one scope.
    async fn list_grains(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        rev_scope: RevaluationScope,
    ) -> Result<Vec<RevaluationGrain>, DomainError> {
        let r = match rev_scope {
            RevaluationScope::Ar => self.repo.list_ar_invoices_to_revalue(scope, tenant).await,
            RevaluationScope::Unallocated => {
                self.repo.list_unallocated_to_revalue(scope, tenant).await
            }
            RevaluationScope::ReusableCredit => {
                self.repo
                    .list_reusable_credit_to_revalue(scope, tenant)
                    .await
            }
        };
        r.map_err(|e| DomainError::Internal(format!("list revaluation grains: {e}")))
    }

    /// Resolve a currency's minor-unit scale.
    async fn scale_for(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        currency: &str,
    ) -> Result<u8, DomainError> {
        self.resolver
            .resolve(scope, tenant, currency)
            .await
            .map_err(|e| DomainError::Internal(format!("currency scale resolve: {e}")))
    }

    /// Build a per-grain functional-only adjusting line. `currency` is the grain's
    /// **transaction** currency (the projector's grain key), `amount_minor = 0`,
    /// and the functional movement rides `functional_amount_minor` on `leg.side`
    /// so the projector moves the grain's `functional_balance_minor` by the signed
    /// delta (debit-normal grain rises on DR / falls on CR).
    fn grain_line(
        g: &RevaluationGrain,
        rev_scope: RevaluationScope,
        leg: &RevaluationLine,
        scale: u8,
    ) -> NewLine {
        NewLine {
            line_id: Uuid::now_v7(),
            payer_tenant_id: g.payer_tenant_id,
            seller_tenant_id: None,
            resource_tenant_id: None,
            account_id: g.account_id,
            account_class: scope_account_class(rev_scope),
            gl_code: None,
            side: leg.side,
            amount_minor: 0,
            currency: g.currency.clone(),
            currency_scale: scale,
            invoice_id: g.invoice_id.clone(),
            due_date: None,
            revenue_stream: None,
            mapping_status: MappingStatus::Resolved,
            functional_amount_minor: Some(leg.functional_minor),
            functional_currency: Some(g.functional_currency.clone()),
            tax_jurisdiction: None,
            tax_filing_period: None,
            tax_rate_ref: None,
            legal_entity_id: None,
            invoice_item_ref: None,
            sku_or_plan_ref: None,
            price_id: None,
            pricing_snapshot_ref: None,
            po_allocation_group: None,
            credit_grant_event_type: g.credit_grant_event_type.clone(),
            ar_status: None,
        }
    }

    /// Build the net `FX_UNREALIZED` functional-only contra line (`amount_minor =
    /// 0`, `currency = functional_ccy`). It carries the SAME `payer` as the grain
    /// lines (the `MixedPayer` invariant) and projects onto the `FX_UNREALIZED`
    /// `account_balance` grain only; the reversal next period undoes it.
    fn fx_unrealized_line(
        payer: Uuid,
        account_id: Uuid,
        functional_ccy: &str,
        fx: &RevaluationLine,
        scale: u8,
    ) -> NewLine {
        NewLine {
            line_id: Uuid::now_v7(),
            payer_tenant_id: payer,
            seller_tenant_id: None,
            resource_tenant_id: None,
            account_id,
            account_class: AccountClass::FxUnrealized,
            gl_code: None,
            side: fx.side,
            amount_minor: 0,
            currency: functional_ccy.to_owned(),
            currency_scale: scale,
            invoice_id: None,
            due_date: None,
            revenue_stream: None,
            mapping_status: MappingStatus::Resolved,
            functional_amount_minor: Some(fx.functional_minor),
            functional_currency: Some(functional_ccy.to_owned()),
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

    /// Build the revaluation/reversal entry header. `period_id` is the period the
    /// entry posts INTO (the reval period for a run; the next OPEN period for a
    /// reversal); `effective_at` is the reporting date (period-end for a run,
    /// first-of-period for a reversal). `posted_at_utc` is now.
    #[allow(clippy::too_many_arguments)]
    fn build_entry(
        ctx: &SecurityContext,
        tenant: Uuid,
        period_id: &str,
        effective_at: NaiveDate,
        functional_ccy: &str,
        source_doc_type: SourceDocType,
        source_business_id: &str,
        reverses_entry_id: Option<Uuid>,
        reverses_period_id: Option<String>,
    ) -> NewEntry {
        NewEntry {
            entry_id: Uuid::now_v7(),
            tenant_id: tenant,
            // v1: one legal entity per tenant (F5) — derived server-side.
            legal_entity_id: tenant,
            period_id: period_id.to_owned(),
            entry_currency: functional_ccy.to_owned(),
            source_doc_type,
            source_business_id: source_business_id.to_owned(),
            reverses_entry_id,
            reverses_period_id,
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at,
            origin: ORIGIN_SYSTEM.to_owned(),
            posted_by_actor_id: ctx.subject_id(),
            correlation_id: Uuid::now_v7(),
            rounding_evidence: serde_json::Value::Null,
            rate_snapshot_ref: None,
        }
    }
}

/// Group the scope's grains by payer tenant (one journal entry per payer — the
/// `MixedPayer` invariant). `BTreeMap` for a deterministic per-payer order.
fn group_by_payer(grains: Vec<RevaluationGrain>) -> BTreeMap<Uuid, Vec<RevaluationGrain>> {
    let mut by_payer: BTreeMap<Uuid, Vec<RevaluationGrain>> = BTreeMap::new();
    for g in grains {
        by_payer.entry(g.payer_tenant_id).or_default().push(g);
    }
    by_payer
}

/// The idempotency `business_id` for a per-payer revaluation / reversal:
/// `period_id:scope:payer` (design §4.5 — the run and the reversal share the key
/// family, distinguished by `source_doc_type`; payer-scoped because an entry spans
/// only one payer).
fn business_id(period_id: &str, scope: RevaluationScope, payer: Uuid) -> String {
    format!("{period_id}:{}:{payer}", scope.as_token())
}

/// The `business_id` prefix for ALL payers of one `(period, scope)` — the reversal
/// lookup (`period_id:scope:`).
fn business_id_prefix(period_id: &str, scope: RevaluationScope) -> String {
    format!("{period_id}:{}:", scope.as_token())
}

/// The last calendar day of `period_id` (`YYYYMM`) as a `NaiveDate` — the
/// revaluation's effective date. Falls back to the epoch for a malformed id (the
/// run already validated `period_end_utc`, so this never hits the fallback on the
/// post path).
fn period_end_naive(period_id: &str) -> NaiveDate {
    period_end_utc(period_id)
        .and_then(|end| to_naive_date(end).pred_opt())
        .unwrap_or_else(|| to_naive_date(OffsetDateTime::UNIX_EPOCH))
}

/// Negate one posted revaluation line into a fresh reversal line: flip the side
/// (so the projector unwinds the grain's functional carrying value), keep the
/// account / grain keys / functional value / currency, and mint a new `line_id`.
/// `amount_minor` is `0` on a revaluation line, so the reversal stays
/// functional-only too.
fn reverse_line(record: &LineRecord) -> Result<NewLine, DomainError> {
    let orig_side = Side::from_str(&record.side)
        .map_err(|_| DomainError::Internal(format!("reversal: bad side {}", record.side)))?;
    let flipped = match orig_side {
        Side::Debit => Side::Credit,
        Side::Credit => Side::Debit,
    };
    let account_class = AccountClass::from_str(&record.account_class).map_err(|_| {
        DomainError::Internal(format!(
            "reversal: bad account_class {}",
            record.account_class
        ))
    })?;
    let mapping_status = MappingStatus::from_str(&record.mapping_status).map_err(|_| {
        DomainError::Internal(format!(
            "reversal: bad mapping_status {}",
            record.mapping_status
        ))
    })?;
    let currency_scale = u8::try_from(record.currency_scale).map_err(|_| {
        DomainError::Internal(format!(
            "reversal: bad currency_scale {}",
            record.currency_scale
        ))
    })?;
    Ok(NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: record.payer_tenant_id,
        seller_tenant_id: record.seller_tenant_id,
        resource_tenant_id: record.resource_tenant_id,
        account_id: record.account_id,
        account_class,
        gl_code: record.gl_code.clone(),
        side: flipped,
        amount_minor: record.amount_minor,
        currency: record.currency.clone(),
        currency_scale,
        invoice_id: record.invoice_id.clone(),
        due_date: record.due_date,
        revenue_stream: record.revenue_stream.clone(),
        mapping_status,
        functional_amount_minor: record.functional_amount_minor,
        functional_currency: record.functional_currency.clone(),
        tax_jurisdiction: record.tax_jurisdiction.clone(),
        tax_filing_period: record.tax_filing_period.clone(),
        tax_rate_ref: record.tax_rate_ref.clone(),
        legal_entity_id: record.legal_entity_id,
        invoice_item_ref: record.invoice_item_ref.clone(),
        sku_or_plan_ref: record.sku_or_plan_ref.clone(),
        price_id: record.price_id.clone(),
        pricing_snapshot_ref: record.pricing_snapshot_ref.clone(),
        po_allocation_group: record.po_allocation_group.clone(),
        credit_grant_event_type: record.credit_grant_event_type.clone(),
        ar_status: record.ar_status.clone(),
    })
}

/// Signed functional value of a net `FX_UNREALIZED` contra leg for the event
/// payload: a CREDIT contra is a net unrealized **gain** (`+`), a DEBIT a net
/// **loss** (`−`). The `functional_minor` magnitude is always non-negative (domain
/// invariant), so the sign is carried entirely by the posting side.
const fn fx_unrealized_signed(side: Side, functional_minor: i64) -> i64 {
    match side {
        Side::Credit => functional_minor,
        Side::Debit => -functional_minor,
    }
}

/// The signed `FX_UNREALIZED` functional value carried by a reversal's lines — the
/// negation of the original revaluation's (the reversal flips each leg's side) —
/// for the `revaluation_reversed` event payload. `0` if no `FX_UNREALIZED` line is
/// present (a degenerate entry; never on the real reversal path, which always
/// negates the original's contra).
fn reversal_fx_unrealized_signed(lines: &[NewLine]) -> i64 {
    lines
        .iter()
        .find(|l| l.account_class == AccountClass::FxUnrealized)
        .and_then(|l| {
            l.functional_amount_minor
                .map(|f| fx_unrealized_signed(l.side, f))
        })
        .unwrap_or(0)
}

/// In-txn [`PostSidecar`] that publishes `billing.ledger.fx.revaluation_completed`
/// (the transactional outbox) atomically with the `FX_UNREALIZED` revaluation
/// entry. Carries the run-time payload facts known before the post; `entry_id`
/// comes from the finalized [`PostedFacts`]. The publish-only mirror of
/// [`crate::infra::recognition::sidecar`]'s release event step (no counter rows).
struct RevaluationCompletedSidecar {
    publisher: Arc<LedgerEventPublisher>,
    ctx: SecurityContext,
    tenant_id: Uuid,
    period_id: String,
    scope: String,
    payer_id: Uuid,
    functional_currency: String,
    fx_unrealized_minor: i64,
    grains_moved: i32,
    posted_at_utc: OffsetDateTime,
}

#[async_trait::async_trait]
impl PostSidecar for RevaluationCompletedSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        _scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        self.publisher
            .publish_fx_revaluation_completed(
                &self.ctx,
                txn,
                LedgerFxRevaluationCompleted {
                    tenant_id: self.tenant_id,
                    entry_id: posted.entry_id,
                    period_id: self.period_id.clone(),
                    scope: self.scope.clone(),
                    payer_id: self.payer_id,
                    functional_currency: self.functional_currency.clone(),
                    fx_unrealized_minor: self.fx_unrealized_minor,
                    grains_moved: self.grains_moved,
                    posted_at_utc: self.posted_at_utc,
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish fx_revaluation_completed: {e}")))
    }
}

/// In-txn [`PostSidecar`] that publishes `billing.ledger.fx.revaluation_reversed`
/// (the transactional outbox) atomically with the `FX_REVAL_REVERSAL` entry. The
/// mirror of [`RevaluationCompletedSidecar`] for the reversal post.
struct RevaluationReversedSidecar {
    publisher: Arc<LedgerEventPublisher>,
    ctx: SecurityContext,
    tenant_id: Uuid,
    reverses_entry_id: Uuid,
    reval_period_id: String,
    reversal_period_id: String,
    scope: String,
    payer_id: Uuid,
    functional_currency: String,
    fx_unrealized_minor: i64,
    posted_at_utc: OffsetDateTime,
}

#[async_trait::async_trait]
impl PostSidecar for RevaluationReversedSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        _scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        self.publisher
            .publish_fx_revaluation_reversed(
                &self.ctx,
                txn,
                LedgerFxRevaluationReversed {
                    tenant_id: self.tenant_id,
                    entry_id: posted.entry_id,
                    reverses_entry_id: self.reverses_entry_id,
                    reval_period_id: self.reval_period_id.clone(),
                    reversal_period_id: self.reversal_period_id.clone(),
                    scope: self.scope.clone(),
                    payer_id: self.payer_id,
                    functional_currency: self.functional_currency.clone(),
                    fx_unrealized_minor: self.fx_unrealized_minor,
                    posted_at_utc: self.posted_at_utc,
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish fx_revaluation_reversed: {e}")))
    }
}

#[cfg(test)]
#[path = "revaluation_run_tests.rs"]
mod revaluation_run_tests;
