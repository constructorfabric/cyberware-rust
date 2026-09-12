//! `CreditApplicationService` — the orchestrator that drives the pure
//! reusable-credit (wallet) domain (`crate::domain::payment::credit`) through the
//! foundation engine. It moves money in and out of a tenant's reusable-credit
//! wallet (architecture §5.2) and is the credit counterpart to
//! [`crate::infra::payment::allocate::AllocationService`] — same deps, same
//! header/chart/post plumbing, two flows instead of one.
//!
//! - **grant** ([`grant_credit`](CreditApplicationService::grant_credit)) — parks
//!   unallocated pool cash into the wallet: **DR `UNALLOCATED`** / **CR
//!   `REUSABLE_CREDIT`**. Capped at the payer's live unallocated pool
//!   ([`PaymentRepo::read_unallocated`]) — a grant of more than the pool holds is
//!   rejected with [`DomainError::GrantExceedsUnallocated`] before any post (the
//!   pool can't fund credit it doesn't have).
//! - **apply** ([`apply_credit`](CreditApplicationService::apply_credit)) — spends
//!   the wallet against open receivables: **N×DR `REUSABLE_CREDIT`** (one per drawn
//!   sub-grain) / **M×CR `AR`** (one per receivable). Two caps bound it, each read
//!   from live ledger state: the receivable side is validated against the payer's
//!   open AR candidates ([`validate_credit_targets`] ⇒
//!   [`DomainError::CreditExceedsOpenAr`]), and the wallet side is planned against
//!   the payer's spendable sub-grains oldest-grant-first ([`plan_wallet_debit`] ⇒
//!   [`DomainError::CreditExceedsWallet`]). The drawn total always equals the
//!   target total, so the entry balances by construction.
//!
//! Sequence for one flow:
//! 1. **read the cap state** — the unallocated pool (grant) or the open AR
//!    candidates + spendable wallet sub-grains (apply).
//! 2. **decide / validate** — for apply, validate the caller's targets against the
//!    open candidates and plan the wallet draw-down for their total; grant has no
//!    split to decide (it is a single pool→wallet move).
//! 3. **build** the balanced entry ([`build_grant_entry`] / [`build_apply_entry`]),
//!    **overwrite** its placeholder header, **bind** chart `account_id`s, and
//!    **post**.
//! 4. **emit metrics** — `credit_application` (outcome) + the payment-post
//!    duration under [`PostFlow::CreditApply`].
//!
//! **No sidecar, no settlement gate.** Unlike allocate, credit is *wallet-sourced*,
//! not *payment-sourced*: there is no per-payment money-out cap to enforce in-txn,
//! so no [`PostSidecar`] is threaded (`None` is passed to the engine). The wallet
//! balance is itself a projector grain — the reusable-credit sub-balance cache the
//! engine maintains from the posted lines — so there is no counter table to bump
//! and no settlement row to gate on. Idempotent on `(tenant, CREDIT_APPLY,
//! credit_application_id)`: a replay of the same `credit_application_id` returns the
//! prior entry with no new ledger effect (both grant and apply share the
//! `CREDIT_APPLY` source doc type — see the domain module). Lives in `infra` (not
//! `domain`) because it needs repo + posting access; the domain module it calls
//! stays pure (dylint DE0301).

use std::sync::Arc;
use std::time::Instant;

use bss_ledger_sdk::{PostEntry, PostLine, PostingRef, SourceDocType};
use chrono::Datelike;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::payment::credit::{
    ApplyInput, CreditDebit, GrantInput, build_apply_entry, build_grant_entry, plan_wallet_debit,
    validate_credit_targets,
};
use crate::domain::payment::precedence::{Allocated, Candidate};
use crate::domain::ports::metrics::{LedgerMetricsPort, PostFlow, PostResult};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::idempotency::IdempotencyGate;
use crate::infra::posting::service::{PostSidecar, PostingService};
use crate::infra::storage::repo::{PaymentRepo, ReferenceRepo};
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// One grant request: park `amount_minor` of the payer's unallocated pool into
/// their reusable-credit wallet sub-grain.
pub struct GrantRequest {
    /// The seller tenant whose ledger this posts into.
    pub tenant_id: Uuid,
    /// The tenant whose wallet is credited (the pool owner / single payer).
    pub payer_tenant_id: Uuid,
    /// The `CREDIT_APPLY` idempotency business id.
    pub credit_application_id: String,
    /// ISO currency of the grant.
    pub currency: String,
    /// Amount to park into the wallet, in minor units. Capped at the payer's live
    /// unallocated pool.
    pub amount_minor: i64,
    /// The wallet sub-grain bucket the credit accrues to.
    pub credit_grant_event_type: String,
}

/// One apply request: spend the payer's reusable-credit wallet against the named
/// open receivables.
pub struct ApplyRequest {
    /// The seller tenant whose ledger this posts into.
    pub tenant_id: Uuid,
    /// The tenant whose wallet is spent and whose receivables are paid (the single
    /// payer).
    pub payer_tenant_id: Uuid,
    /// The `CREDIT_APPLY` idempotency business id.
    pub credit_application_id: String,
    /// ISO currency of the application.
    pub currency: String,
    /// The per-invoice receivable shares to apply the wallet to. Validated against
    /// the payer's open AR candidates (presence / per-invoice cap / positivity /
    /// no-duplicate); their total sizes the wallet draw-down.
    pub targets: Vec<Allocated>,
}

/// The result of a grant or apply: the posting handle, and — for an apply — the
/// per-sub-grain wallet draw-downs (`debits`) and the per-invoice receivable
/// shares (`targets`) the post wrote. A grant moves no wallet/AR splits, so both
/// vectors are empty for it.
#[derive(Debug)]
pub struct CreditApplicationOutcome {
    pub posting: PostingRef,
    pub debits: Vec<CreditDebit>,
    pub targets: Vec<Allocated>,
}

/// Orchestrates the reusable-credit domain (grant / apply) over the foundation
/// engine.
pub struct CreditApplicationService {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    repo: PaymentRepo,
    metrics: Arc<dyn LedgerMetricsPort>,
}

impl CreditApplicationService {
    /// Build the service over one database provider, the event publisher
    /// (threaded into the posting engine), and the metrics sink. Same deps as
    /// [`crate::infra::payment::allocate::AllocationService`].
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), publisher);
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let repo = PaymentRepo::new(db);
        Self {
            posting,
            reference,
            resolver,
            repo,
            metrics,
        }
    }

    /// Return the prior posting as a replay when `credit_application_id` already
    /// finalized a `CREDIT_APPLY` post for `tenant` — the idempotency
    /// short-circuit that runs BEFORE the state-dependent caps. Both flows
    /// validate against mutable ledger state (grant against the live unallocated
    /// pool, apply against open AR + the wallet), so without this a
    /// retry-after-success would re-read the now-drained state and reject (e.g. a
    /// full-payment apply retried after the AR closed). The engine's in-txn claim
    /// in `post` remains the authoritative dedup for a concurrent first post; a
    /// `None` here just proceeds into that path. The replayed outcome carries no
    /// `debits`/`targets` (the splits were returned by the original call; a
    /// replay key on the entry id).
    async fn replay_if_posted(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        credit_application_id: &str,
        expected_hash: &str,
    ) -> Result<Option<CreditApplicationOutcome>, DomainError> {
        let Some((entry_id, stored_hash)) = self
            .repo
            .lookup_finalized_post(
                scope,
                tenant,
                SourceDocType::CreditApply,
                credit_application_id,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("idempotency lookup: {e}")))?
        else {
            return Ok(None);
        };
        // A replay must carry the SAME request payload. A reuse of
        // `credit_application_id` with a different kind / amount / currency /
        // targets is an idempotency-key conflict, not a replay — reject it rather
        // than silently returning the prior posting (the engine's in-txn claim
        // makes the same comparison for a concurrent first post).
        if stored_hash != expected_hash {
            return Err(DomainError::IdempotencyConflict(format!(
                "credit_application_id {credit_application_id} reused with a different payload"
            )));
        }
        Ok(Some(CreditApplicationOutcome {
            posting: PostingRef {
                entry_id,
                // A replay carries the prior, finalized entry id; the sequence is
                // not re-read (replay callers key on the id — mirrors the engine's
                // own replay `PostingRef`).
                created_seq: 0,
                replayed: true,
            },
            debits: vec![],
            targets: vec![],
        }))
    }

    /// Grant `amount_minor` of the payer's unallocated pool into their wallet
    /// sub-grain. Returns the posting handle (the `debits`/`targets` are empty —
    /// a grant moves no wallet/AR splits).
    ///
    /// On success emits `credit_application(Posted | Replayed)` + the payment-post
    /// duration; every rejection emits `credit_application(Rejected)` + the
    /// duration.
    ///
    /// # Errors
    /// [`DomainError::GrantExceedsUnallocated`] when the grant amount exceeds the
    /// payer's live unallocated pool; [`DomainError::InvalidRequest`] when the
    /// amount is non-positive or the event-type is empty (the builder's shape
    /// checks); any foundation rejection or [`DomainError::Internal`] on an infra
    /// fault.
    pub async fn grant_credit(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: GrantRequest,
    ) -> Result<CreditApplicationOutcome, DomainError> {
        let started = Instant::now();
        let result = self.grant_credit_inner(ctx, scope, req).await;
        self.record(&result, started);
        result
    }

    /// Run the grant sequence (no metrics — the public wrapper records them).
    async fn grant_credit_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: GrantRequest,
    ) -> Result<CreditApplicationOutcome, DomainError> {
        // 0. Idempotency short-circuit: a retry of an already-finalized grant
        //    returns the prior posting BEFORE the unallocated-pool cap re-reads
        //    the (now-reduced) pool and would spuriously reject. The request-based
        //    hash lets the short-circuit reject a same-id / different-payload reuse.
        let request_hash = grant_request_hash(&req);
        if let Some(replay) = self
            .replay_if_posted(
                scope,
                req.tenant_id,
                &req.credit_application_id,
                &request_hash,
            )
            .await?
        {
            return Ok(replay);
        }

        // 1. Cap the grant at the payer's live unallocated pool — the pool can't
        //    fund credit it doesn't hold. SQL-level BOLA: a foreign tenant reads 0.
        let available = self
            .repo
            .read_unallocated(scope, req.tenant_id, req.payer_tenant_id, &req.currency)
            .await
            .map_err(|e| DomainError::Internal(format!("read unallocated: {e}")))?;
        if req.amount_minor > available {
            return Err(DomainError::GrantExceedsUnallocated(format!(
                "grant {} exceeds available unallocated {} for payer {}",
                req.amount_minor, available, req.payer_tenant_id
            )));
        }

        // 2. Build the balanced grant entry (DR UNALLOCATED / CR REUSABLE_CREDIT),
        //    overwrite the placeholder header, bind chart account_ids, and post —
        //    no sidecar (the wallet balance is a projector grain, not a counter).
        let mut entry = build_grant_entry(&GrantInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            credit_application_id: req.credit_application_id.clone(),
            currency: req.currency.clone(),
            amount_minor: req.amount_minor,
            credit_grant_event_type: req.credit_grant_event_type.clone(),
            // Credit posts effective-now; thread a request field here if a
            // back-dated grant is ever needed.
            effective_at: None,
        })?;
        overwrite_header(&mut entry, ctx);
        self.bind_chart(scope, &mut entry).await?;

        let posting = self
            .post_bound(ctx, scope, entry, None, request_hash)
            .await?;
        Ok(CreditApplicationOutcome {
            posting,
            debits: vec![],
            targets: vec![],
        })
    }

    /// Apply the payer's reusable-credit wallet against the named open
    /// receivables, drawing the wallet down oldest-grant-first. Returns the
    /// posting handle, the per-sub-grain draw-downs, and the validated per-invoice
    /// shares.
    ///
    /// On success emits `credit_application(Posted | Replayed)` + the payment-post
    /// duration; every rejection emits `credit_application(Rejected)` + the
    /// duration.
    ///
    /// # Errors
    /// [`DomainError::CreditExceedsOpenAr`] when a target names an unknown/closed
    /// invoice, over-applies an invoice, repeats one, or is non-positive;
    /// [`DomainError::CreditExceedsWallet`] when the payer's spendable wallet
    /// cannot cover the target total; [`DomainError::InvalidRequest`] when the
    /// built entry fails a shape/balance check; any foundation rejection or
    /// [`DomainError::Internal`] on an infra fault.
    pub async fn apply_credit(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: ApplyRequest,
    ) -> Result<CreditApplicationOutcome, DomainError> {
        let started = Instant::now();
        let result = self.apply_credit_inner(ctx, scope, req).await;
        self.record(&result, started);
        result
    }

    /// Run the apply sequence (no metrics — the public wrapper records them).
    async fn apply_credit_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: ApplyRequest,
    ) -> Result<CreditApplicationOutcome, DomainError> {
        // 0. Idempotency short-circuit: a retry of an already-finalized apply
        //    returns the prior posting BEFORE the open-AR / wallet caps re-read
        //    the (now-drained) state and would spuriously reject (e.g. a
        //    full-payment apply retried after the invoice closed). The request-based
        //    hash lets the short-circuit reject a same-id / different-payload reuse.
        let request_hash = apply_request_hash(&req);
        if let Some(replay) = self
            .replay_if_posted(
                scope,
                req.tenant_id,
                &req.credit_application_id,
                &request_hash,
            )
            .await?
        {
            return Ok(replay);
        }

        // 1. Read the open AR candidate set (oldest-first) — the receivable side's
        //    cap basis. SQL-level BOLA: a foreign tenant yields no rows.
        let rows = self
            .repo
            .list_open_ar_invoices(scope, req.tenant_id, req.payer_tenant_id, &req.currency)
            .await
            .map_err(|e| DomainError::Internal(format!("list open ar invoices: {e}")))?;
        let candidates: Vec<Candidate> = rows
            .into_iter()
            .map(|r| Candidate {
                invoice_id: r.invoice_id,
                open_minor: r.balance_minor,
                original_posted_at: r.original_posted_at,
            })
            .collect();

        // 2. Validate the caller's targets against the open candidates (presence /
        //    per-invoice cap / positivity / no-duplicate); the validated shares are
        //    the CR AR side, in the caller's order.
        let targets = validate_credit_targets(&candidates, &req.targets)?;
        // 3. The target total sizes the wallet draw-down (Σ DR == Σ CR). Sum in
        //    i128 to avoid an i64 overflow on a large validated target set
        //    (mirrors the domain builder's i128 accumulation), then narrow — an
        //    out-of-range total is rejected, never silently wrapped.
        let total_minor: i128 = targets.iter().map(|t| i128::from(t.amount_minor)).sum();
        let total = i64::try_from(total_minor).map_err(|_| {
            DomainError::AmountOutOfRange("credit target total exceeds i64 range".to_owned())
        })?;

        // 4. Read the payer's spendable wallet sub-grains (oldest-grant-first) and
        //    plan the draw-down for the total — the wallet-side cap is enforced
        //    here (Σ available < total ⇒ CreditExceedsWallet).
        let subgrains = self
            .repo
            .list_credit_subgrains(scope, req.tenant_id, req.payer_tenant_id, &req.currency)
            .await
            .map_err(|e| DomainError::Internal(format!("list credit subgrains: {e}")))?;
        let debits = plan_wallet_debit(&subgrains, total)?;

        // 5. Build the balanced apply entry (N×DR REUSABLE_CREDIT / M×CR AR),
        //    overwrite the placeholder header, bind chart account_ids, and post —
        //    no sidecar (the wallet balance is a projector grain, not a counter).
        let mut entry = build_apply_entry(&ApplyInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            credit_application_id: req.credit_application_id.clone(),
            currency: req.currency.clone(),
            debits: debits.clone(),
            targets: targets.clone(),
            // Credit posts effective-now; thread a request field here if a
            // back-dated apply is ever needed.
            effective_at: None,
        })?;
        overwrite_header(&mut entry, ctx);
        self.bind_chart(scope, &mut entry).await?;

        let posting = self
            .post_bound(ctx, scope, entry, None, request_hash)
            .await?;
        Ok(CreditApplicationOutcome {
            posting,
            debits,
            targets,
        })
    }

    /// Bind each line's chart `account_id` from its `(account_class, currency)` —
    /// the credit classes (UNALLOCATED / `REUSABLE_CREDIT` / AR) are stream-less.
    /// An unprovisioned account surfaces as [`DomainError::AccountClosed`].
    async fn bind_chart(
        &self,
        scope: &AccessScope,
        entry: &mut PostEntry,
    ) -> Result<(), DomainError> {
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
        Ok(())
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's
    /// `NewEntry`/`NewLine`, resolving each line's scale, and post. `sidecar` is
    /// always `None` for credit (the wallet balance is a projector grain — no
    /// in-txn counter write to thread). Binds the request-based idempotency hash
    /// (`post_with_request_hash`) so the replay short-circuit can reject a same-id
    /// / different-payload reuse.
    async fn post_bound(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Option<Arc<dyn PostSidecar>>,
        request_hash: String,
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
            // Slice 5: reusable-credit wallet posts are same-currency in v1.
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
            .post_with_request_hash(ctx, scope, new_entry, new_lines, sidecar, request_hash)
            .await
    }

    /// Emit `credit_application(outcome)` + the `CreditApply`-labelled
    /// payment-post duration for one attempt (mirrors `allocate::record`).
    fn record(&self, result: &Result<CreditApplicationOutcome, DomainError>, started: Instant) {
        let outcome = match result {
            Ok(o) if o.posting.replayed => PostResult::Replayed,
            Ok(_) => PostResult::Posted,
            Err(_) => PostResult::Rejected,
        };
        self.metrics.credit_application(outcome);
        self.metrics
            .payment_post_duration(started.elapsed().as_secs_f64(), PostFlow::CreditApply);
    }
}

/// Overwrite the placeholder header fields the pure builder emits: credit posts
/// effective-now, so derive the `period_id` (YYYYMM) and `effective_at` from the
/// wall clock, stamp the actor from the security context, and mint a fresh
/// correlation id. If `period_id` stayed `""` the post would fail the
/// fiscal-period gate, so this overwrite is mandatory (mirrors
/// `allocate::overwrite_header`).
fn overwrite_header(entry: &mut PostEntry, ctx: &SecurityContext) {
    let eff_date = to_naive_date(OffsetDateTime::now_utc());
    entry.effective_at = eff_date;
    entry.period_id = format!("{:04}{:02}", eff_date.year(), eff_date.month());
    entry.posted_by_actor_id = ctx.subject_id();
    entry.correlation_id = Uuid::now_v7();
}

/// Thin `PostLine` adapter over [`ChartIndex::resolve`]: the credit classes
/// (UNALLOCATED / `REUSABLE_CREDIT` / AR) are stream-less, so this resolves on
/// `stream = None` (mirrors `allocate::resolve_line`).
fn resolve_line(chart: &ChartIndex, line: &PostLine) -> Option<Uuid> {
    chart.resolve(
        line.account_class,
        &line.currency,
        line.revenue_stream.as_deref(),
    )
}

/// Map one SDK [`PostLine`] + its resolved scale to the engine's [`NewLine`]
/// (mirrors `allocate::new_line`).
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

/// The request-based idempotency hash for a credit GRANT — `content_hash` over
/// the canonical request fields (a `grant` discriminant + tenant, payer,
/// application id, currency, amount, event type). Stable across the post's
/// state-dependent rebuild, so the dedup row stores it and
/// [`CreditApplicationService::replay_if_posted`] compares it to reject a same
/// `credit_application_id` reused with a different payload.
fn grant_request_hash(req: &GrantRequest) -> String {
    let canonical = format!(
        "grant\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        req.tenant_id,
        req.payer_tenant_id,
        req.credit_application_id,
        req.currency,
        req.amount_minor,
        req.credit_grant_event_type,
    );
    IdempotencyGate::content_hash(&canonical)
}

/// The request-based idempotency hash for a credit APPLY — `content_hash` over
/// the canonical request fields (an `apply` discriminant + tenant, payer,
/// application id, currency, and the per-invoice targets sorted for
/// order-independence). The `grant` / `apply` discriminant means reusing one
/// `credit_application_id` across the two flows is correctly a conflict. See
/// [`grant_request_hash`].
fn apply_request_hash(req: &ApplyRequest) -> String {
    let mut targets: Vec<String> = req
        .targets
        .iter()
        .map(|t| format!("{}\u{1d}{}", t.invoice_id, t.amount_minor))
        .collect();
    targets.sort();
    let canonical = format!(
        "apply\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        req.tenant_id,
        req.payer_tenant_id,
        req.credit_application_id,
        req.currency,
        targets.join("\u{1e}"),
    );
    IdempotencyGate::content_hash(&canonical)
}
