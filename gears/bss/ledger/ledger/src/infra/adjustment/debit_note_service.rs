//! `DebitNoteHandler` — the Slice-3 debit-note orchestrator (design §4.3, Group D).
//! A debit note is an **additional charge** against an already-posted invoice; it
//! is a **DIRECT split that mirrors the Slice-1 invoice-post** (`crate::infra::invoice_post`),
//! NOT a compensating reduction — so it does NOT use the
//! [`RecognizedDeferredSplitter`](crate::domain::adjustment::splitter) (that is the
//! credit-note's reducer). It books fresh:
//!
//! | Line | Side | Account class |
//! |------|------|---------------|
//! | Additional AR (incl. tax) | DR | `AR` |
//! | Revenue recognized at post (ex-tax) | CR | `REVENUE` |
//! | Contract liability deferred per PO (ex-tax, if any) | CR | `CONTRACT_LIABILITY` |
//! | Tax | CR | `TAX_PAYABLE` |
//!
//! and, in the SAME serializable transaction (via a [`PostSidecar`]), atomically:
//!
//! 1. **Schedule build (D4)** — when the note defers (`deferred_part_minor > 0`),
//!    it runs the SAME recognition [`ScheduleBuilder`] path the invoice-post uses
//!    (`crate::infra::invoice_post::InvoicePostService::derive_recognition`),
//!    yielding a [`PlannedScheduleMaterialization`], and reuses the **identical**
//!    [`ScheduleBuilderSidecar`] to insert the `recognition_schedule` + segments —
//!    so the new deferred Contract-liability balance is immediately recognizable by
//!    a later S6 run (no stuck liability), exactly the rule Slice 1 applies.
//! 2. **Headroom raise** — first-touch seed `invoice_exposure` (`original_total_minor`
//!    = the invoice's posted AR) then bump `debit_note_total_minor += amount`. A
//!    debit note **raises** the headroom (the RHS of the
//!    `credit_note_total_minor <= original_total_minor + debit_note_total_minor`
//!    CHECK), so this can never trip the cap — it only widens the room for later
//!    credit notes.
//! 3. **Persist** the `debit_note` row.
//!
//! **Lock order (§4.7).** The sidecar takes `recognition_schedule` /
//! `recognition_segment` (the schedule build) BEFORE `invoice_exposure` (the
//! headroom) — the global rank order, so it never inverts vs the credit-note /
//! recognition / payment paths.
//!
//! **Idempotency** is the engine's `(tenant, DEBIT_NOTE, debit_note_id)` claim:
//! the entry's `source_doc_type = DebitNote` + `source_business_id = debit_note_id`
//! make [`PostingService::post`]'s `Fresh` claim the at-most-once gate (a replay
//! returns before the sidecar — byte-identical to how `InvoicePostService` keys
//! invoice-post dedup, and to the peer `CreditNoteHandler`).
//!
//! **Payer-close gate (A-2, design §7).** A debit note inherits the Foundation
//! §4.2 payer-close gate exactly as the invoice-post does: a `payer_open = false`
//! rejects with [`DomainError::PayerClosed`] (`PAYER_CLOSED`, 409 — `failed_precondition`) before any
//! ledger effect (you cannot charge a closed payer). The gate is resolved by the
//! caller (the Group E REST seam, mirroring `journal_entries.rs`'s `payer_open`
//! seam); the foundation account-lifecycle invariant (a closed AR account) is the
//! authoritative backstop at post time.
//!
//! Lives in `infra` (not `domain`): it needs repo + posting access; the domain
//! modules it calls stay pure (dylint DE0301). Wraps the `pub` [`PostingService`] +
//! repos directly (like [`InvoicePostService`](crate::infra::invoice_post) /
//! [`CreditNoteHandler`](super::credit_note_service)) so it is constructible from
//! out-of-crate integration tests.

use std::sync::Arc;

use bss_ledger_sdk::{MappingStatus, PostingRef, SourceDocType};
use chrono::Datelike;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::RecognitionConfig;
use crate::domain::adjustment::debit_note::{
    DebitNoteLegPlan, DebitNoteRequest, PlannedLeg, build_debit_note_legs, validate_shape,
};
use crate::domain::approval::ApprovalKind;
use crate::domain::approval::intent::{ApprovalIntent, DebitNoteIntent};
use crate::domain::approval::policy::OperationFacts;
use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::ports::metrics::{LedgerMetricsPort, NoteOutcome};
use crate::domain::recognition::builder::{ScheduleBuilder, ScheduleOutcome};
use crate::domain::recognition::input::RecognitionInput;
use crate::domain::recognition::ports::{
    DefaultDeferralPolicyResolver, DefaultSspResolver, DefaultVcResolver, RecognitionContext,
};
use crate::infra::approval::service::ApprovalService;
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::payloads::DebitNotePosted;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::exception::ExceptionRouter;
use crate::infra::posting::chart::load_chart;
use crate::infra::posting::idempotency::IdempotencyGate;
use crate::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use crate::infra::recognition::sidecar::{PlannedScheduleMaterialization, ScheduleBuilderSidecar};
use crate::infra::storage::repo::adjustment_repo::NewDebitNote;
use crate::infra::storage::repo::{AdjustmentRepo, ReferenceRepo};
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service (mirrors the peer
/// orchestrators).
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// Orchestrates the debit-note domain over the foundation engine (design §4.3).
pub struct DebitNoteHandler {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    adjustment: AdjustmentRepo,
    /// ASC 606 recognition tunables (Slice 4): the per-schedule segment ceiling the
    /// pure [`ScheduleBuilder`] enforces when the note defers (D4).
    recognition_config: RecognitionConfig,
    /// The event publisher — threaded into the posting engine AND held so the
    /// in-txn sidecar can publish `billing.ledger.debit_note.posted` (Group F).
    publisher: Arc<LedgerEventPublisher>,
    /// Metrics sink (`ledger_debit_note_total{outcome}`, Group F): one increment
    /// per attempt, labelled posted / replayed / rejected.
    metrics: Arc<dyn LedgerMetricsPort>,
    /// The dual-control engine (VHP-1852). `Some` ⇒ a debit note whose amount
    /// crosses the tenant's D2 threshold is gated to the preparer→approver queue
    /// ([`DomainError::DualControlRequired`]) instead of posting inline; `None` ⇒
    /// gating is disabled (the executor's approved replay, and the unit tests that
    /// construct the handler without the engine). Wired in `module` via
    /// [`Self::with_approval`]; mirrors the
    /// [`RefundHandler`](super::refund_service::RefundHandler)'s `approval` seam.
    approval: Option<Arc<ApprovalService>>,
    // Slice 7 Phase 2: routes the `RECOGNITION_POLICY_CONFLICT` stub to a durable
    // close-blocking exception row (ADDITIVE beside the rejection). `None` until
    // `with_exceptions` wires it (so existing constructions are unchanged).
    exceptions: Option<Arc<ExceptionRouter>>,
}

impl DebitNoteHandler {
    /// Build the handler over one database provider + the event publisher
    /// (threaded into the posting engine + the sidecar's in-txn publish) + the
    /// metrics sink + the recognition config (the segment ceiling the schedule
    /// build enforces when the note defers). Same `db` / `publisher` / `metrics`
    /// deps as the peer
    /// [`InvoicePostService`](crate::infra::invoice_post::InvoicePostService) /
    /// [`CreditNoteHandler`](super::credit_note_service::CreditNoteHandler), plus
    /// the [`RecognitionConfig`] the invoice-post also takes (a deferred debit note
    /// runs the same derivation).
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
        recognition_config: RecognitionConfig,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let adjustment = AdjustmentRepo::new(db);
        Self {
            posting,
            reference,
            resolver,
            adjustment,
            recognition_config,
            publisher,
            metrics,
            approval: None,
            exceptions: None,
        }
    }

    /// Attach the exception router (Slice 7 Phase 2) so a `RECOGNITION_POLICY_CONFLICT`
    /// rejection also opens a durable close-blocking exception row. Additive — the
    /// existing rejection is unchanged.
    #[must_use]
    pub fn with_exceptions(mut self, exceptions: Arc<ExceptionRouter>) -> Self {
        self.exceptions = Some(exceptions);
        self
    }

    /// Attach the dual-control engine (VHP-1852): a debit note whose amount crosses
    /// the tenant's D2 threshold is then gated to the preparer→approver queue
    /// ([`DomainError::DualControlRequired`]) rather than posting inline. The approved
    /// replay re-enters through [`Self::post_debit_note_approved`], which skips the
    /// gate. Builder form (not a `new` arg) so the executor's un-gated handler and the
    /// unit tests stay source-compatible; mirrors the
    /// [`RefundHandler::with_approval`](super::refund_service::RefundHandler::with_approval).
    #[must_use]
    pub fn with_approval(mut self, approval: Arc<ApprovalService>) -> Self {
        self.approval = Some(approval);
        self
    }

    /// Post a debit note (design §4.3). Validates the request shape, derives the
    /// deferred split + (when deferring) the schedule plan via the SAME recognition
    /// [`ScheduleBuilder`] path the invoice-post uses, builds the balanced
    /// direct-split legs, and posts them with the in-txn [`DebitNotePostSidecar`]
    /// (schedule build + headroom seed/raise + `debit_note` row). Idempotent on
    /// `(tenant, DEBIT_NOTE, debit_note_id)`.
    ///
    /// `payer_open` is the payer's lifecycle decision (resolved by the caller, A-2):
    /// `false` ⇒ the post is rejected with [`DomainError::PayerClosed`] before any
    /// ledger effect (a closed payer cannot be charged).
    ///
    /// # Errors
    /// [`DomainError::PayerClosed`] when `!payer_open`;
    /// [`DomainError::AmountOutOfRange`] / [`DomainError::InvalidRequest`] (shape);
    /// any recognition-derivation block when the note defers
    /// ([`DomainError::SspSnapshotRequired`], [`DomainError::RecognitionPolicyConflict`],
    /// [`DomainError::ScheduleTooLong`], [`DomainError::RecognitionWithoutInvoiceLink`]);
    /// any foundation rejection (unbalanced/empty/period-closed/account-closed/…) or
    /// [`DomainError::Internal`] on an infra fault.
    pub async fn post_debit_note(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: DebitNoteRequest,
        payer_open: bool,
    ) -> Result<PostingRef, DomainError> {
        let result = self
            .post_debit_note_inner(ctx, scope, req, payer_open, /* gate */ true)
            .await;
        // One `ledger_debit_note_total{outcome}` increment per attempt (Group F).
        self.metrics.debit_note(note_outcome(&result));
        result
    }

    /// The approved-replay entry (VHP-1852): re-drive a held debit note WITHOUT the
    /// dual-control gate. Called only by the `ApprovalExecutor` after a second actor
    /// approves the PENDING debit-note approval — the threshold was already crossed
    /// at gate time, so re-checking it would re-open a second approval (an infinite
    /// loop). Idempotent on the engine's `(tenant, DEBIT_NOTE, debit_note_id)` claim:
    /// a re-approve replays the post harmlessly (the dedup short-circuits a committed
    /// entry before the sidecar), so execute-then-mark is safe. Mirrors
    /// [`RefundHandler::post_refund_approved`](super::refund_service::RefundHandler::post_refund_approved).
    ///
    /// # Errors
    /// As [`Self::post_debit_note`], minus the dual-control gate (never returns
    /// [`DomainError::DualControlRequired`]).
    pub async fn post_debit_note_approved(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: DebitNoteRequest,
        payer_open: bool,
    ) -> Result<PostingRef, DomainError> {
        let result = self
            .post_debit_note_inner(ctx, scope, req, payer_open, /* gate */ false)
            .await;
        self.metrics.debit_note(note_outcome(&result));
        result
    }

    /// Build + post the debit note (no metrics — the public wrappers record
    /// them). Carries the payer-close gate, the F4 link gate, the dual-control gate
    /// (over D2, `gate == true` only), and the in-txn `debit_note.posted` event
    /// publish.
    async fn post_debit_note_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: DebitNoteRequest,
        payer_open: bool,
        gate: bool,
    ) -> Result<PostingRef, DomainError> {
        // Payer-close gate (A-2, §7) — before any read/effect, mirroring
        // `InvoicePostService::post_invoice`. A closed payer cannot be charged.
        if !payer_open {
            return Err(DomainError::PayerClosed(format!(
                "payer {} is closed",
                req.payer_tenant_id
            )));
        }

        // 1. Pure shape gate (amounts, deferral/recognition shape) — a clean 400
        //    before any read.
        validate_shape(&req)?;

        // A zero-amount note has no charge and would fail the engine's empty-entry
        // validation; reject up-front (inherited S1 / AC #4 — no zero placeholder).
        if req.amount_minor == 0 {
            return Err(DomainError::InvalidRequest(
                "debit note amount_minor must be > 0".to_owned(),
            ));
        }

        // 1b. Originating-invoice link gate (F4, design §4.3 / §5): a debit note
        //     MUST link a posted invoice (an `INVOICE_POST` entry for the
        //     origin_invoice_id). No posted invoice ⇒ `NOTE_INVOICE_NOT_FOUND`
        //     (404), BEFORE any read/build/post (no orphan charge entry). Scoped
        //     existence (SQL-level BOLA) — a foreign-tenant invoice reads as
        //     absent, the same 404, no existence leak.
        if !self
            .adjustment
            .posted_invoice_exists_out_of_txn(scope, req.tenant_id, &req.origin_invoice_id)
            .await
            .map_err(|e| DomainError::Internal(format!("posted-invoice existence: {e}")))?
        {
            return Err(DomainError::NoteInvoiceNotFound(format!(
                "debit note {} references invoice {} which has no posted INVOICE_POST entry",
                req.debit_note_id, req.origin_invoice_id
            )));
        }

        // 1c. Dual-control gate (VHP-1852, design §1.4 D2 / §4.3). Gated on the debit
        //     note's amount crossing the tenant's D2 threshold, AFTER the payer-close
        //     gate + shape gate (a malformed request 400s) AND the originating-invoice
        //     link gate (an absent origin 404s) — so none of those open an approval —
        //     but BEFORE any recognition-derivation/build/post. Above the threshold ⇒
        //     a PENDING approval is created and `DualControlRequired` is returned (the
        //     REST handler maps it to 409); at/under threshold ⇒ inline, unchanged. The
        //     approved replay (`gate == false`) skips this — the threshold was already
        //     crossed at gate time. Mirrors the refund gate.
        if gate && let Some(approval) = &self.approval {
            let intent = ApprovalIntent::DebitNote(DebitNoteIntent::from(&req));
            let facts = OperationFacts {
                kind: ApprovalKind::DebitNote,
                // FX-SIMPLIFICATION (DC10 / FX = Slice 5): transaction-currency minor,
                // not USD-eq. Single-currency until the FX slice lands; mirrors the
                // refund gate's comment.
                amount_usd_eq_minor: Some(req.amount_minor),
                effective_at: None,
                has_outstanding_balance: false,
            };
            if let Some(approval_id) = approval
                .gate(ctx, scope, intent, facts, "debit_note".to_owned())
                .await?
            {
                return Err(DomainError::DualControlRequired(format!(
                    "debit note requires dual-control approval: {approval_id}"
                )));
            }
        }

        // 2. Recognition derivation (D4) — when the note defers, run the SAME pure
        //    ScheduleBuilder path the invoice-post uses to build the schedule that
        //    will release the deferred Contract-liability. Returns the schedule plan
        //    (empty when fully recognized). A derivation block (SSP / policy /
        //    segment-ceiling) propagates → the post fails, so no orphan deferral.
        //
        // Slice 7 Phase 2: the policy-conflict block surfaces here from the sync
        // `derive_schedule` helper (where `.await` is not reachable); ADDITIVELY open
        // a durable close-blocking exception row beside the rejection before
        // propagating it (the note is still rejected; this only makes the
        // recognition-policy conflict block the next close until resolved).
        let schedules = match self.derive_schedule(&req) {
            Ok(s) => s,
            Err(e @ DomainError::RecognitionPolicyConflict(_)) => {
                if let Some(ex) = &self.exceptions {
                    ex.route(
                        req.tenant_id,
                        crate::domain::exception::ExceptionType::RecognitionPolicyConflict,
                        &req.debit_note_id,
                        Some(serde_json::json!({
                            "debit_note_id": req.debit_note_id,
                            "origin_invoice_id": req.origin_invoice_id,
                        })),
                    )
                    .await;
                }
                return Err(e);
            }
            Err(other) => return Err(other),
        };

        // 3. Build the balanced direct-split leg plan (DR AR / CR REVENUE / CR CL /
        //    CR TAX) — a mirror of the S1 invoice-post split for one charge line.
        let plan = build_debit_note_legs(&req)?;

        // 4. Resolve chart accounts + scale, assemble the engine entry + lines.
        let (entry, lines) = self.assemble_post(ctx, scope, &req, &plan).await?;

        // The posted AR incl. tax this note raises the headroom against — the
        // first-touch seed basis (out-of-txn; the row is seeded/raised in-txn by the
        // sidecar). A no-op on a re-seed (a prior note on this invoice), so the
        // original_total stays the invoice's posted AR; the debit note only bumps
        // debit_note_total_minor.
        let posted_ar_incl_tax = self.read_posted_ar(scope, &req).await?;

        // 5. Post via the invariant engine with the in-txn sidecar (schedule build +
        //    headroom seed/raise + debit_note row). The engine's Fresh claim on
        //    (tenant, DEBIT_NOTE, debit_note_id) is the idempotency gate.
        let schedule_sidecar = if schedules.is_empty() {
            None
        } else {
            Some(ScheduleBuilderSidecar {
                tenant_id: req.tenant_id,
                payer_tenant_id: req.payer_tenant_id,
                source_invoice_id: req.origin_invoice_id.clone(),
                schedules,
                idempotency: IdempotencyGate::new(),
                // A debit note EXTENDS the live schedule for its key — discriminate
                // its SCHEDULE_BUILD claim by the note id so it does not replay (and
                // skip) the base build.
                build_discriminator: Some(req.debit_note_id.clone()),
            })
        };
        let sidecar: Arc<dyn PostSidecar> = Arc::new(DebitNotePostSidecar {
            tenant_id: req.tenant_id,
            origin_invoice_id: req.origin_invoice_id.clone(),
            currency: req.currency.clone(),
            posted_ar_incl_tax,
            debit_note_amount_minor: req.amount_minor,
            // The event payload's identity + recognized/deferred split parts (the
            // posted entry id is filled in-txn from `PostedFacts`).
            debit_note_id: req.debit_note_id.clone(),
            recognized_part_minor: plan.recognized_part_minor,
            deferred_part_minor: plan.deferred_part_minor,
            // Published in-txn (transactional outbox) so the event commits
            // atomically with the entry + counters, or rolls back with them.
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
            schedule_sidecar,
            debit_note_row: NewDebitNote {
                tenant_id: req.tenant_id,
                debit_note_id: req.debit_note_id.clone(),
                origin_invoice_id: req.origin_invoice_id.clone(),
                currency: req.currency.clone(),
                amount_minor: req.amount_minor,
                recognized_part_minor: plan.recognized_part_minor,
                deferred_part_minor: plan.deferred_part_minor,
                created_at_utc: OffsetDateTime::now_utc(),
            },
        });

        self.posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await
    }

    /// Run the pure recognition derivation for a deferring debit note, returning the
    /// schedule(s) to materialize (empty when the note is fully recognized). Mirrors
    /// [`InvoicePostService::derive_recognition`](crate::infra::invoice_post) for a
    /// single charge line: build a [`RecognitionContext`] over the note's ex-tax
    /// amount + the post period, call [`ScheduleBuilder::derive`], and on a
    /// [`ScheduleOutcome::Schedule`] pair it with the targeted invoice-item ref
    /// (§4.7) into a [`PlannedScheduleMaterialization`].
    ///
    /// The note's own `deferred_minor` is the authority on HOW MUCH defers (the leg
    /// builder uses it directly); the derivation here owns the schedule SHAPE
    /// (segments / `policy_ref` / refs) and the trigger gates. A non-deferred note
    /// (`deferred_minor == 0`) yields no schedule even if it carried a spec; a
    /// deferring note that the policy resolves to point-in-time is an invalid mix
    /// the derivation surfaces as an `AmountOutOfRange` / policy block.
    ///
    /// # Errors
    /// Any block the derivation raises ([`DomainError::SspSnapshotRequired`],
    /// [`DomainError::RecognitionPolicyConflict`], [`DomainError::ScheduleTooLong`],
    /// [`DomainError::AmountOutOfRange`]), or the §4.7 invoice-item-link
    /// [`DomainError::RecognitionWithoutInvoiceLink`] gate (a deferred note must
    /// carry the ref its schedule draws down).
    fn derive_schedule(
        &self,
        req: &DebitNoteRequest,
    ) -> Result<Vec<PlannedScheduleMaterialization>, DomainError> {
        // Fully-recognized note ⇒ no schedule (validate_shape already guaranteed a
        // deferring note carries a spec; a non-deferring note ignores any spec).
        if req.deferred_minor == 0 {
            return Ok(Vec::new());
        }
        let Some(input) = &req.recognition else {
            // Unreachable once validate_shape passed (a deferring note has a spec);
            // guard rather than unwrap.
            return Err(DomainError::InvalidRequest(
                "deferred debit note missing recognition spec".to_owned(),
            ));
        };

        let policy = DefaultDeferralPolicyResolver;
        let ssp = DefaultSspResolver;
        let vc = DefaultVcResolver;
        let builder = ScheduleBuilder::new(&policy, &ssp, &vc, &self.recognition_config);

        let ex_tax = req.amount_minor_ex_tax();
        let period_id = current_period_id();
        let ctx = RecognitionContext {
            input,
            invoice_period_id: &period_id,
            // Only the DEFERRED part defers — the recognized part books to REVENUE
            // now. The builder lays `item_amount_minor_ex_tax` out across the
            // schedule segments, so pass the note's `deferred_minor` (clamped), NOT
            // the whole ex-tax amount, else the schedule over-defers (total_deferred
            // would be the full ex-tax, not the deferred split). Matches the CL leg
            // `build_debit_note_legs` books (it defers the same clamped amount).
            item_amount_minor_ex_tax: req.deferred_minor.clamp(0, ex_tax),
            // A debit note is a single charge line; its own ex-tax is the
            // R4-materiality denominator (no surrounding invoice total here).
            invoice_total_minor: ex_tax,
            currency: &req.currency,
            revenue_stream: &req.revenue_stream,
        };
        match builder.derive(&ctx)? {
            ScheduleOutcome::NoDeferral => {
                // The note asked to defer (`deferred_minor > 0`) but the policy
                // resolved point-in-time — a contradictory request. Reject rather
                // than silently posting a deferred CL with no releasing schedule.
                Err(DomainError::RecognitionPolicyConflict(format!(
                    "debit note requests a deferred part ({}) but its recognition policy `{}` \
                     resolves point-in-time",
                    req.deferred_minor, input.policy_ref
                )))
            }
            ScheduleOutcome::Schedule(schedule) => {
                // §4.7 invoice-item-link: the schedule's NOT-NULL
                // source_invoice_item_ref must resolve to a non-empty ref (the
                // Contract-liability line this post creates). Block before the post
                // with the specific RecognitionWithoutInvoiceLink (no orphan).
                let item_ref = req
                    .origin_invoice_item_ref
                    .as_deref()
                    .filter(|r| !r.is_empty())
                    .ok_or_else(|| {
                        DomainError::RecognitionWithoutInvoiceLink(format!(
                            "deferred debit note (stream `{}`) must carry an \
                             origin_invoice_item_ref to anchor its contract-liability schedule",
                            req.revenue_stream
                        ))
                    })?;
                Ok(vec![PlannedScheduleMaterialization {
                    schedule,
                    source_invoice_item_ref: item_ref.to_owned(),
                }])
            }
        }
    }

    /// Read the invoice's posted-AR-incl-tax — the `invoice_exposure.original_total_minor`
    /// first-touch seed basis (design §4.7). Out-of-txn scoped read on a fresh
    /// connection (the headroom row is seeded in-txn by the sidecar); identical to
    /// the basis the [`CreditNoteHandler`](super::credit_note_service) seeds. A
    /// debit note on an invoice with no posted AR line reads `0` — the headroom then
    /// floors on the debit-note total the bump adds.
    async fn read_posted_ar(
        &self,
        scope: &AccessScope,
        req: &DebitNoteRequest,
    ) -> Result<i64, DomainError> {
        self.adjustment
            .read_posted_ar_incl_tax_out_of_txn(scope, req.tenant_id, &req.origin_invoice_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read posted AR: {e}")))
    }

    /// Resolve each planned leg's chart `account_id` + currency scale and assemble
    /// the engine [`NewEntry`] + [`NewLine`] vector. The per-stream classes
    /// (`REVENUE` / `CONTRACT_LIABILITY`) resolve on their stream; the rest resolve
    /// stream-less. The header is `source_doc_type = DebitNote` +
    /// `source_business_id = debit_note_id` (the engine's idempotency key). Mirrors
    /// [`CreditNoteHandler::assemble_post`](super::credit_note_service).
    async fn assemble_post(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &DebitNoteRequest,
        plan: &DebitNoteLegPlan,
    ) -> Result<(NewEntry, Vec<NewLine>), DomainError> {
        let chart = load_chart(&self.reference, scope, req.tenant_id).await?;
        let scale = self
            .resolver
            .resolve(scope, req.tenant_id, &req.currency)
            .await
            .map_err(|e| DomainError::Internal(format!("currency scale resolve: {e}")))?;

        let eff_date = to_naive_date(OffsetDateTime::now_utc());
        let period_id = format!("{:04}{:02}", eff_date.year(), eff_date.month());

        let mut lines: Vec<NewLine> = Vec::with_capacity(plan.legs.len());
        for leg in &plan.legs {
            let account_id = chart
                .resolve(
                    leg.account_class,
                    &req.currency,
                    leg.revenue_stream.as_deref(),
                )
                .ok_or_else(|| {
                    DomainError::AccountClosed(format!(
                        "no provisioned account for class {} / stream {:?} / currency {}",
                        leg.account_class.as_str(),
                        leg.revenue_stream,
                        req.currency
                    ))
                })?;
            lines.push(Self::mk_line(req, leg, account_id, scale));
        }

        let entry = NewEntry {
            entry_id: Uuid::now_v7(),
            tenant_id: req.tenant_id,
            // v1: one legal entity per tenant — derived server-side.
            legal_entity_id: req.tenant_id,
            period_id,
            entry_currency: req.currency.clone(),
            source_doc_type: SourceDocType::DebitNote,
            // The engine's `(tenant, DEBIT_NOTE, debit_note_id)` idempotency key.
            source_business_id: req.debit_note_id.clone(),
            reverses_entry_id: None,
            reverses_period_id: None,
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: eff_date,
            origin: ORIGIN_SYSTEM.to_owned(),
            posted_by_actor_id: ctx.subject_id(),
            correlation_id: Uuid::now_v7(),
            rounding_evidence: serde_json::Value::Null,
            // Slice 5: same-currency in v1 (no FX lock on this path).
            rate_snapshot_ref: None,
        };
        Ok((entry, lines))
    }

    /// Map one [`PlannedLeg`] + its resolved chart account/scale to the engine
    /// [`NewLine`]. The DR `AR` carries `payer_tenant_id` + `invoice_id` (its cache
    /// grain keys on them); the per-stream `REVENUE` / `CONTRACT_LIABILITY` legs
    /// carry their `revenue_stream` (the per-stream account + the DB CHECK need it).
    fn mk_line(req: &DebitNoteRequest, leg: &PlannedLeg, account_id: Uuid, scale: u8) -> NewLine {
        NewLine {
            line_id: Uuid::now_v7(),
            payer_tenant_id: req.payer_tenant_id,
            seller_tenant_id: Some(req.tenant_id),
            resource_tenant_id: None,
            account_id,
            account_class: leg.account_class,
            gl_code: None,
            side: leg.side,
            amount_minor: leg.amount_minor,
            currency: req.currency.clone(),
            currency_scale: scale,
            invoice_id: Some(req.origin_invoice_id.clone()),
            due_date: None,
            revenue_stream: leg.revenue_stream.clone(),
            mapping_status: MappingStatus::Resolved,
            functional_amount_minor: None,
            functional_currency: None,
            // Tax dims (jurisdiction / filing-period / rate) carry the posted
            // TaxBreakdown evidence the leg routed: `Some` on a per-component
            // TAX_PAYABLE leg (so the projector disaggregates `tax_subbalance` per
            // (jurisdiction, filing), §4.5), `None` on every other leg + the legacy
            // single dimensionless tax leg. The amount is the posted tax (never
            // recomputed here, §4.3).
            tax_jurisdiction: leg.tax_jurisdiction.clone(),
            tax_filing_period: leg.tax_filing_period.clone(),
            tax_rate_ref: leg.tax_rate_ref.clone(),
            legal_entity_id: None,
            // The deferred CL line's schedule draws down this ref (§4.7); carried on
            // every leg for lineage (the schedule was built against it).
            invoice_item_ref: req.origin_invoice_item_ref.clone(),
            sku_or_plan_ref: None,
            price_id: None,
            pricing_snapshot_ref: None,
            po_allocation_group: req.recognition.as_ref().and_then(po_group),
            credit_grant_event_type: None,
            ar_status: None,
        }
    }
}

/// The PO/allocation group of a recognition spec (the line dim the deferred CL
/// books under, §4.7) — `None` for a spec with no group.
fn po_group(input: &RecognitionInput) -> Option<String> {
    input.po_allocation_group.clone()
}

/// Classify a debit-note attempt result into its `ledger_debit_note_total`
/// `outcome` label (Group F): a replay is `replayed`, a fresh post is `posted`,
/// and every rejection (payer-closed / shape / not-found / recognition block /
/// foundation / infra) folds into `rejected`. A debit note has no split-ambiguous
/// or headroom-cap block (it raises the headroom), so it never reports
/// `blocked_*` — unlike the credit-note classifier.
fn note_outcome(result: &Result<PostingRef, DomainError>) -> NoteOutcome {
    match result {
        Ok(r) if r.replayed => NoteOutcome::Replayed,
        Ok(_) => NoteOutcome::Posted,
        Err(_) => NoteOutcome::Rejected,
    }
}

/// The current fiscal `period_id` (`YYYYMM`) — the period the debit note posts
/// into + the schedule's first-segment default when the spec leaves it open. The
/// post itself derives the same value in `assemble_post`; the schedule build reads
/// it here as the derivation's invoice-period.
fn current_period_id() -> String {
    let now = to_naive_date(OffsetDateTime::now_utc());
    format!("{:04}{:02}", now.year(), now.month())
}

/// The in-transaction [`PostSidecar`] for a debit note: runs AFTER balance
/// projection and BEFORE the dedup finalize (fresh-claim path only — a replay
/// returns before the sidecar), so all its writes commit atomically with the
/// journal entry or roll back with it (design §4.3 / §4.7). It performs, in the
/// §4.7 lock order (`recognition_schedule` / `recognition_segment` BEFORE
/// `invoice_exposure`):
///
/// 1. **Schedule build (D4)** — when the note defers, delegates to the SAME
///    [`ScheduleBuilderSidecar`] the invoice-post uses (held as
///    [`Self::schedule_sidecar`]): claims `SCHEDULE_BUILD` idempotency, mints the
///    `schedule_id`, inserts the `recognition_schedule` + segments. A
///    fully-recognized note carries `None` here (no schedule touch).
/// 2. **Headroom raise** — first-touch seed `invoice_exposure`
///    (`original_total_minor` = posted AR), then bump `debit_note_total_minor` by
///    the note's incl-tax amount. This RAISES the cap (the RHS of the headroom
///    CHECK), so it can never be rejected by the headroom guard.
/// 3. **Persist** the `debit_note` row.
pub struct DebitNotePostSidecar {
    tenant_id: Uuid,
    origin_invoice_id: String,
    currency: String,
    /// The posted AR incl. tax — the `invoice_exposure.original_total_minor` seed
    /// (a no-op on a re-seed; the running counters are never reset).
    posted_ar_incl_tax: i64,
    /// The note's incl-tax amount — the `debit_note_total_minor` raise delta +
    /// the published event's `amount_minor`.
    debit_note_amount_minor: i64,
    /// The note's business id — the published event's `debit_note_id`.
    debit_note_id: String,
    /// The ex-tax recognized part — the published event's `recognized_part_minor`.
    recognized_part_minor: i64,
    /// The ex-tax deferred part — the published event's `deferred_part_minor`.
    deferred_part_minor: i64,
    /// The event publisher: `billing.ledger.debit_note.posted` is published IN
    /// this post txn (the transactional outbox) so it commits atomically with the
    /// entry + counters, or rolls back with them. Mirrors the credit-note sidecar.
    publisher: Arc<LedgerEventPublisher>,
    /// The security context for the in-txn outbox publish (cloned by the handler).
    ctx: SecurityContext,
    /// The schedule-build sidecar to run FIRST (lock order) when the note defers;
    /// `None` for a fully-recognized note. The SAME type the invoice-post threads,
    /// reused 1:1 (no duplication of the build/idempotency logic).
    schedule_sidecar: Option<ScheduleBuilderSidecar>,
    /// The `debit_note` record to persist.
    debit_note_row: NewDebitNote,
}

#[async_trait::async_trait]
impl PostSidecar for DebitNotePostSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. Schedule build (D4) — FIRST in the lock order (recognition_schedule /
        //    recognition_segment, ranks before invoice_exposure). Delegate to the
        //    identical invoice-post sidecar: it claims SCHEDULE_BUILD, mints the
        //    schedule id, and inserts schedule + segments in THIS txn (or rolls the
        //    whole post back). `None` ⇒ fully-recognized note, no schedule.
        if let Some(sc) = &self.schedule_sidecar {
            sc.run(txn, scope, posted).await?;
        }

        // 2. Headroom raise (rank after the recognition rows). First-touch seed the
        //    invoice_exposure row (original_total = posted AR), then RAISE
        //    debit_note_total_minor by the note's incl-tax amount. Raising the RHS
        //    of the headroom CHECK can never trip it (it only widens the cap), so
        //    this is a plain bump with no cap-refinement mapping.
        AdjustmentRepo::seed_exposure_first_touch(
            txn,
            scope,
            self.tenant_id,
            &self.origin_invoice_id,
            &self.currency,
            self.posted_ar_incl_tax,
        )
        .await
        .map_err(|e| DomainError::Internal(format!("seed invoice_exposure: {e}")))?;
        AdjustmentRepo::add_debit_note_total(
            txn,
            scope,
            self.tenant_id,
            &self.origin_invoice_id,
            self.debit_note_amount_minor,
        )
        .await
        .map_err(|e| DomainError::Internal(format!("raise debit_note_total: {e}")))?;

        // 3. Persist the debit_note record row.
        AdjustmentRepo::insert_debit_note(txn, scope, &self.debit_note_row)
            .await
            .map_err(|e| DomainError::Internal(format!("insert debit_note: {e}")))?;

        // 4. Publish `billing.ledger.debit_note.posted` into the SAME post txn
        //    (transactional outbox): the event row commits atomically with the
        //    entry + the schedule/headroom/record writes, or a publish failure
        //    rolls the whole post back. Never on replay (a replay returns before
        //    the sidecar). Ids + amount + split parts only (no PII).
        self.publisher
            .publish_debit_note_posted(
                &self.ctx,
                txn,
                DebitNotePosted {
                    tenant_id: self.tenant_id,
                    debit_note_id: self.debit_note_id.clone(),
                    origin_invoice_id: self.origin_invoice_id.clone(),
                    entry_id: posted.entry_id,
                    currency: self.currency.clone(),
                    amount_minor: self.debit_note_amount_minor,
                    recognized_part_minor: self.recognized_part_minor,
                    deferred_part_minor: self.deferred_part_minor,
                    posted_at_utc: OffsetDateTime::now_utc(),
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish debit_note_posted: {e}")))?;

        Ok(())
    }
}
