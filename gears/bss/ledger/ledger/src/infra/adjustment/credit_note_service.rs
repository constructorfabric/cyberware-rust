//! `CreditNoteHandler` — the Slice-3 credit-note orchestrator (design §4.2, Group
//! C). It posts a credit note's balanced compensating entry through the invariant
//! [`PostingService`] and, in the SAME serializable transaction (via a
//! [`PostSidecar`]), atomically updates every guarded counter/schedule the note
//! touches:
//!
//! 1. **read state** (out-of-txn, oldest-first like
//!    [`CreditApplicationService`](crate::infra::payment::credit)): the targeted
//!    obligation's ACTIVE per-stream `recognition_schedule` state + the invoice's
//!    posted-AR-incl-tax (headroom seed basis) + current open AR (the AR-vs-wallet
//!    credit-leg cap). The authoritative in-txn backstops (the headroom CHECK, the
//!    schedule cap CHECK, the AR no-negative CHECK) cover a concurrent race the
//!    lockless reads cannot see — exactly the Slice-2 credit-apply discipline.
//! 2. **split** the ex-tax revenue amount across the schedule state via the pure
//!    [`RecognizedDeferredSplitter`] (block-on-ambiguous; never pro-rata).
//! 3. **build** the balanced leg plan ([`build_credit_note_legs`]): DR
//!    `CONTRA_REVENUE` (or `GOODWILL`) + per-stream DR `CONTRACT_LIABILITY` + DR
//!    `TAX_PAYABLE`; CR `AR` (capped at open AR) + CR `REUSABLE_CREDIT` (remainder,
//!    K-2).
//! 4. **post** the entry via [`PostingService::post`] with the
//!    [`CreditNotePostSidecar`], which in the post txn: **(a)** reduces each
//!    touched schedule's `total_deferred_minor` (negative Δ; the
//!    `recognized_minor <= total_deferred_minor` CHECK is the over-reduction
//!    guard), **(b)** seeds + bumps the `invoice_exposure` headroom counter (the
//!    `chk_ledger_invoice_exposure_headroom` CHECK is the over-cap guard → 400),
//!    and **(c)** persists the `credit_note` row. The wallet remainder seed **(d)**
//!    needs NO sidecar work: the `CR REUSABLE_CREDIT` leg carries
//!    `credit_grant_event_type = CREDIT_NOTE`, so the engine's `BalanceProjector`
//!    seeds the `reusable_credit_subbalance` sub-grain from the posted line itself.
//!
//! **Idempotency** is the engine's `(tenant, CREDIT_NOTE, credit_note_id)` claim:
//! the entry's `source_doc_type = CreditNote` + `source_business_id =
//! credit_note_id` make [`PostingService::post`]'s `Fresh` claim the at-most-once
//! gate (a replay returns before the sidecar — byte-identical to how
//! `InvoicePostService` keys invoice-post dedup).
//!
//! **Block-on-ambiguous** (C3): a [`DomainError::CreditNoteSplitAmbiguous`] from
//! the splitter propagates out unchanged (the REST layer maps it to
//! `CREDIT_NOTE_SPLIT_AMBIGUOUS`, Group E); the alarm + exception-queue routing is
//! marked here (Group F / Slice 7) but not emitted in this group.
//!
//! Lives in `infra` (not `domain`): it needs repo + posting access; the domain
//! modules it calls stay pure (dylint DE0301). Wraps the `pub` [`PostingService`]
//! and repos directly (like [`InvoicePostService`](crate::infra::invoice_post)) so
//! it is constructible from out-of-crate integration tests.

use std::collections::HashMap;
use std::sync::Arc;

use bss_ledger_sdk::{AccountClass, MappingStatus, PostingRef, Side, SourceDocType};
use chrono::Datelike;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::adjustment::credit_note::{
    CreditNoteLegPlan, CreditNoteRequest, PlannedLeg, build_credit_note_legs, validate_shape,
};
use crate::domain::adjustment::splitter::{
    RecognizedDeferredSplitter, ScheduleStreamState, SplitInput,
};
use crate::domain::approval::ApprovalKind;
use crate::domain::approval::intent::{ApprovalIntent, CreditNoteIntent};
use crate::domain::approval::policy::OperationFacts;
use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::ports::metrics::{LedgerMetricsPort, NoteOutcome};
use crate::domain::status::{LIFECYCLE_OPEN, SCHEDULE_STATUS_ACTIVE};
use crate::infra::approval::service::ApprovalService;
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::payloads::{
    AffectedItem, AlarmCategory, AlarmSeverity, CreditNotePosted, LedgerInvariantAlarm,
};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::exception::ExceptionRouter;
use crate::infra::posting::chart::load_chart;
use crate::infra::posting::idempotency::{ClaimOutcome, IdempotencyGate};
use crate::infra::posting::projector::BalanceProjector;
use crate::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use crate::infra::storage::entity::recognition_schedule;
use crate::infra::storage::repo::adjustment_repo::NewCreditNote;
use crate::infra::storage::repo::{AdjustmentRepo, JournalRepo, RecognitionRepo, ReferenceRepo};
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service (mirrors the peer
/// orchestrators).
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// Orchestrates the credit-note domain over the foundation engine (design §4.2).
pub struct CreditNoteHandler {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    recognition: RecognitionRepo,
    adjustment: AdjustmentRepo,
    /// Append-only journal insert — used by the COMPOSITE path (Group G): the
    /// `refund-with-credit-note` posts the credit-note entry as the SECOND entry
    /// inside the refund's post txn, so the credit note inserts its header+lines
    /// here directly rather than through `PostingService::post` (which would open
    /// its own txn). A cheap clone of the same provider.
    journal: JournalRepo,
    /// The event publisher — threaded into the posting engine AND held so the
    /// in-txn sidecar can publish `billing.ledger.credit_note.posted`, and so the
    /// split-ambiguous path can raise the `CreditNoteSplitBlocked` alarm
    /// out-of-band (Group F).
    publisher: Arc<LedgerEventPublisher>,
    /// Metrics sink (`ledger_credit_note_total{outcome}`, Group F): one increment
    /// per attempt, labelled posted / replayed / `blocked_split` / `blocked_headroom`
    /// / rejected.
    metrics: Arc<dyn LedgerMetricsPort>,
    /// The dual-control engine (VHP-1852). `Some` ⇒ a credit note whose amount
    /// crosses the tenant's D2 threshold is gated to the preparer→approver queue
    /// ([`DomainError::DualControlRequired`]) instead of posting inline; `None` ⇒
    /// gating is disabled (the executor's approved replay, and the unit tests that
    /// construct the handler without the engine). Wired in `module` via
    /// [`Self::with_approval`]; mirrors the
    /// [`RefundHandler`](super::refund_service::RefundHandler)'s `approval` seam.
    approval: Option<Arc<ApprovalService>>,
    // Slice 7 Phase 2: routes the `CREDIT_NOTE_SPLIT_AMBIGUOUS` stub to a durable
    // close-blocking exception row (ADDITIVE beside the rejection/alarm). `None`
    // until `with_exceptions` wires it (so existing constructions are unchanged).
    exceptions: Option<Arc<ExceptionRouter>>,
}

impl CreditNoteHandler {
    /// Build the handler over one database provider + the event publisher
    /// (threaded into the posting engine + the sidecar's in-txn publish + the
    /// split-blocked alarm) + the metrics sink. Same `db` / `publisher` /
    /// `metrics` deps as the peer
    /// [`InvoicePostService`](crate::infra::invoice_post::InvoicePostService) /
    /// [`SettlementReturnService`](crate::infra::payment::settlement_return::SettlementReturnService).
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let recognition = RecognitionRepo::new(db.clone());
        let adjustment = AdjustmentRepo::new(db.clone());
        let journal = JournalRepo::new(db);
        Self {
            posting,
            reference,
            resolver,
            recognition,
            adjustment,
            journal,
            publisher,
            metrics,
            approval: None,
            exceptions: None,
        }
    }

    /// Attach the exception router (Slice 7 Phase 2) so a `CREDIT_NOTE_SPLIT_AMBIGUOUS`
    /// rejection also opens a durable close-blocking exception row. Additive — the
    /// existing rejection/alarm is unchanged.
    #[must_use]
    pub fn with_exceptions(mut self, exceptions: Arc<ExceptionRouter>) -> Self {
        self.exceptions = Some(exceptions);
        self
    }

    /// Attach the dual-control engine (VHP-1852): a credit note whose amount crosses
    /// the tenant's D2 threshold is then gated to the preparer→approver queue
    /// ([`DomainError::DualControlRequired`]) rather than posting inline. The approved
    /// replay re-enters through [`Self::post_credit_note_approved`], which skips the
    /// gate. Builder form (not a `new` arg) so the executor's un-gated handler and the
    /// unit tests stay source-compatible; mirrors the
    /// [`RefundHandler::with_approval`](super::refund_service::RefundHandler::with_approval).
    #[must_use]
    pub fn with_approval(mut self, approval: Arc<ApprovalService>) -> Self {
        self.approval = Some(approval);
        self
    }

    /// Post a credit note (design §4.2). Validates the request shape, reads the
    /// obligation's schedule state + the invoice's open AR, drives the
    /// recognized-vs-deferred split, builds the balanced compensating legs, and
    /// posts them with the in-txn [`CreditNotePostSidecar`] (schedule reduction +
    /// headroom seed/bump + `credit_note` row). Idempotent on
    /// `(tenant, CREDIT_NOTE, credit_note_id)`.
    ///
    /// # Errors
    /// [`DomainError::AmountOutOfRange`] / [`DomainError::InvalidRequest`] (shape);
    /// [`DomainError::CreditNoteSplitAmbiguous`] (indeterminable split — propagated
    /// for the RFC 9457 `CREDIT_NOTE_SPLIT_AMBIGUOUS`, C3);
    /// [`DomainError::CreditNoteExceedsHeadroom`] (the `invoice_exposure` headroom
    /// CHECK rejected the bump → `CREDIT_NOTE_EXCEEDS_HEADROOM`);
    /// [`DomainError::NegativeBalance`] (a goodwill / over-credit that would drive
    /// AR negative — the Slice 1 AR floor, D3); any foundation rejection or
    /// [`DomainError::Internal`] on an infra fault.
    pub async fn post_credit_note(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: CreditNoteRequest,
    ) -> Result<PostingRef, DomainError> {
        let result = self
            .post_credit_note_inner(ctx, scope, req, /* gate */ true)
            .await;
        // One `ledger_credit_note_total{outcome}` increment per attempt (Group F).
        // The split-blocked alarm is raised at its source inside `_inner` (in
        // addition to this metric); here we only classify the outcome.
        self.metrics.credit_note(note_outcome(&result));
        result
    }

    /// The approved-replay entry (VHP-1852): re-drive a held credit note WITHOUT the
    /// dual-control gate. Called only by the `ApprovalExecutor` after a second actor
    /// approves the PENDING credit-note approval — the threshold was already crossed
    /// at gate time, so re-checking it would re-open a second approval (an infinite
    /// loop). Idempotent on the engine's `(tenant, CREDIT_NOTE, credit_note_id)`
    /// claim: a re-approve replays the post harmlessly (the dedup short-circuits a
    /// committed entry before the sidecar), so execute-then-mark is safe. Mirrors
    /// [`RefundHandler::post_refund_approved`](super::refund_service::RefundHandler::post_refund_approved).
    ///
    /// # Errors
    /// As [`Self::post_credit_note`], minus the dual-control gate (never returns
    /// [`DomainError::DualControlRequired`]).
    pub async fn post_credit_note_approved(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: CreditNoteRequest,
    ) -> Result<PostingRef, DomainError> {
        let result = self
            .post_credit_note_inner(ctx, scope, req, /* gate */ false)
            .await;
        self.metrics.credit_note(note_outcome(&result));
        result
    }

    /// Build + post the credit note (no metrics — the public wrappers record
    /// them). Carries the F4 link gate, the dual-control gate (over D2, `gate ==
    /// true` only), the F3 split-blocked alarm, and the in-txn `credit_note.posted`
    /// event publish.
    async fn post_credit_note_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: CreditNoteRequest,
        gate: bool,
    ) -> Result<PostingRef, DomainError> {
        // 1. Pure shape gate (amounts, goodwill shape) — a clean 400 before any read.
        validate_shape(&req)?;

        // A zero-amount note has no compensating effect and would fail the engine's
        // empty-entry validation; reject up-front (inherited S1 / AC #4 forbids a
        // zero placeholder entry just as it forbids a zero placeholder line).
        if req.amount_minor == 0 {
            return Err(DomainError::InvalidRequest(
                "credit note amount_minor must be > 0".to_owned(),
            ));
        }

        // 1b. Originating-invoice link gate (F4, design §4.2 / §5): a credit note
        //     MUST link a posted invoice (an `INVOICE_POST` entry for the
        //     origin_invoice_id). No posted invoice ⇒ `NOTE_INVOICE_NOT_FOUND`
        //     (404), BEFORE any read/build/post (no orphan compensating entry).
        //     Scoped existence (SQL-level BOLA) — a foreign-tenant invoice reads as
        //     absent, the same 404, no existence leak.
        if !self
            .adjustment
            .posted_invoice_exists_out_of_txn(scope, req.tenant_id, &req.origin_invoice_id)
            .await
            .map_err(|e| DomainError::Internal(format!("posted-invoice existence: {e}")))?
        {
            return Err(DomainError::NoteInvoiceNotFound(format!(
                "credit note {} references invoice {} which has no posted INVOICE_POST entry",
                req.credit_note_id, req.origin_invoice_id
            )));
        }

        // 1c. Dual-control gate (VHP-1852, design §1.4 D2 / §4.2). Gated on the
        //     credit note's amount crossing the tenant's D2 threshold, AFTER the
        //     shape gate (a malformed request 400s) AND the originating-invoice link
        //     gate (an absent origin 404s) — so neither opens an approval — but
        //     BEFORE any read/split/build/post. Above the threshold ⇒ a PENDING
        //     approval is created and `DualControlRequired` is returned (the REST
        //     handler maps it to 409); at/under threshold ⇒ inline, unchanged. The
        //     approved replay (`gate == false`) skips this — the threshold was
        //     already crossed at gate time. Mirrors the refund gate.
        if gate && let Some(approval) = &self.approval {
            let intent = ApprovalIntent::CreditNote(CreditNoteIntent::from(&req));
            let facts = OperationFacts {
                kind: ApprovalKind::CreditNote,
                // FX-SIMPLIFICATION (DC10 / FX = Slice 5): transaction-currency minor,
                // not USD-eq. Single-currency until the FX slice lands; mirrors the
                // refund gate's comment.
                amount_usd_eq_minor: Some(req.amount_minor),
                effective_at: None,
                has_outstanding_balance: false,
            };
            if let Some(approval_id) = approval
                .gate(ctx, scope, intent, facts, "credit_note".to_owned())
                .await?
            {
                return Err(DomainError::DualControlRequired(format!(
                    "credit note requires dual-control approval: {approval_id}"
                )));
            }
        }

        // 2. Read the targeted obligation's ACTIVE per-stream schedule state
        //    (out-of-txn; the schedule cap CHECK is the in-txn backstop). A goodwill
        //    credit reduces no obligation, so it runs the split over an EMPTY state
        //    set (and carries no deferred part — validate_shape guaranteed it), so
        //    the whole ex-tax amount is the recognized part the GOODWILL leg debits.
        let streams = if req.goodwill {
            Vec::new()
        } else {
            self.read_schedule_states(scope, &req).await?
        };

        // 3. Read the invoice's posted-AR-incl-tax (headroom seed basis) + current
        //    open AR (the AR-vs-wallet credit cap). Read out-of-txn here to build
        //    the entry; re-seeded/guarded in-txn by the sidecar (the headroom CHECK)
        //    and the AR no-negative CHECK (the floor), mirroring credit-apply.
        let (posted_ar_incl_tax, open_ar) = self.read_ar_caps(scope, &req).await?;

        // 4. Headroom pre-check — the out-of-txn mirror of
        //    `chk_ledger_invoice_exposure_headroom`
        //    (credit_note_total + amount <= original_total + debit_note_total). Run
        //    BEFORE the split so a note that exceeds the invoice's remaining headroom
        //    surfaces as the canonical `CreditNoteExceedsHeadroom`, instead of being
        //    mis-attributed to `CreditNoteSplitAmbiguous` by the splitter's releasable
        //    cap: a fully-deferred over-cap note (requested_deferred == amount >
        //    releasable) would otherwise trip the split's releasable bound first. The
        //    in-txn CHECK in the sidecar stays the AUTHORITATIVE backstop (it owns the
        //    read-then-post race). With no `invoice_exposure` row yet (the invoice's
        //    first note — seeded in-txn), the headroom is the posted AR.
        let remaining_headroom = match self
            .adjustment
            .read_exposure_out_of_txn(scope, req.tenant_id, &req.origin_invoice_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read invoice_exposure: {e}")))?
        {
            Some(exp) => {
                exp.original_total_minor + exp.debit_note_total_minor - exp.credit_note_total_minor
            }
            None => posted_ar_incl_tax,
        };
        if req.amount_minor > remaining_headroom {
            return Err(DomainError::CreditNoteExceedsHeadroom(format!(
                "credit note {} incl-tax amount {} exceeds the invoice's remaining \
                 headroom {remaining_headroom}",
                req.credit_note_id, req.amount_minor
            )));
        }

        // 5. Pure recognized-vs-deferred split. Block-on-ambiguous (C3):
        //    DomainError::CreditNoteSplitAmbiguous propagates UNCHANGED — the REST
        //    layer maps it to CREDIT_NOTE_SPLIT_AMBIGUOUS (Group E).
        let split = RecognizedDeferredSplitter::split(&SplitInput {
            source_invoice_item_ref: req.origin_invoice_item_ref.as_deref().unwrap_or(""),
            po_allocation_group: req.po_allocation_group.as_deref(),
            streams: &streams,
            amount_minor_ex_tax: req.amount_minor_ex_tax(),
            requested_deferred_minor: req.requested_deferred_minor,
        });
        let split = match split {
            Ok(s) => s,
            Err(e @ DomainError::CreditNoteSplitAmbiguous(_)) => {
                // exception stub (full exception_queue is Slice 7)
                // Raise the `CreditNoteSplitBlocked` alarm out-of-band (Group F),
                // IN ADDITION to the RFC 9457 `CREDIT_NOTE_SPLIT_AMBIGUOUS` reject:
                // the note is still rejected (the error propagates as-is — the REST
                // mapping already exists), but an operator gets a triage signal for
                // the missing split basis. Fire-and-forget on its own committed
                // connection (never fails the reject); no books effect occurred.
                self.emit_split_blocked_alarm(ctx, &req, &e).await;
                return Err(e);
            }
            Err(other) => return Err(other),
        };

        // 6. Build the balanced compensating leg plan (DR contra/goodwill + CL +
        //    tax; CR AR capped at open AR + REUSABLE_CREDIT remainder, K-2).
        let plan = build_credit_note_legs(&req, &split, open_ar)?;

        // 7. Resolve chart accounts + scales, assemble the engine entry + lines.
        let (entry, lines) = self.assemble_post(ctx, scope, &req, &plan).await?;

        // 8. Post via the invariant engine with the in-txn sidecar (schedule
        //    reduction + headroom seed/bump + credit_note row). The engine's Fresh
        //    claim on (tenant, CREDIT_NOTE, credit_note_id) is the idempotency gate.
        //    The sidecar is the SAME one the Group-G composite path builds (factored
        //    into `build_post_sidecar`), so the standalone and composite credit-note
        //    posts cannot drift.
        let sidecar: Arc<dyn PostSidecar> =
            Arc::new(self.build_post_sidecar(ctx, &req, &plan, posted_ar_incl_tax));

        self.posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await
    }

    /// Prepare a credit note for the ATOMIC composite post (Group G,
    /// `refund-with-credit-note`, K-3): do ALL the out-of-txn work — shape gate,
    /// originating-invoice link gate, schedule-state + AR-cap reads, the
    /// recognized-vs-deferred split, the leg build, chart/scale resolution, and the
    /// `normal_side` load — and return a [`PreparedCreditNote`] the composite
    /// orchestrator posts as the SECOND entry inside the refund's post txn (via
    /// [`Self::apply_in_txn`]). No txn is opened here; the heavy reads run on fresh
    /// scoped connections exactly as [`Self::post_credit_note_inner`] does. The
    /// in-txn backstops (headroom CHECK, schedule cap CHECK, AR no-negative CHECK)
    /// still apply when [`Self::apply_in_txn`] projects + writes.
    ///
    /// # Errors
    /// As [`Self::post_credit_note`]'s validation/read/split errors (shape,
    /// `NoteInvoiceNotFound`, `CreditNoteSplitAmbiguous`, infra).
    pub(crate) async fn prepare(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &CreditNoteRequest,
    ) -> Result<PreparedCreditNote, DomainError> {
        validate_shape(req)?;
        if req.amount_minor == 0 {
            return Err(DomainError::InvalidRequest(
                "credit note amount_minor must be > 0".to_owned(),
            ));
        }
        if !self
            .adjustment
            .posted_invoice_exists_out_of_txn(scope, req.tenant_id, &req.origin_invoice_id)
            .await
            .map_err(|e| DomainError::Internal(format!("posted-invoice existence: {e}")))?
        {
            return Err(DomainError::NoteInvoiceNotFound(format!(
                "credit note {} references invoice {} which has no posted INVOICE_POST entry",
                req.credit_note_id, req.origin_invoice_id
            )));
        }
        let streams = if req.goodwill {
            Vec::new()
        } else {
            self.read_schedule_states(scope, req).await?
        };
        let split = RecognizedDeferredSplitter::split(&SplitInput {
            source_invoice_item_ref: req.origin_invoice_item_ref.as_deref().unwrap_or(""),
            po_allocation_group: req.po_allocation_group.as_deref(),
            streams: &streams,
            amount_minor_ex_tax: req.amount_minor_ex_tax(),
            requested_deferred_minor: req.requested_deferred_minor,
        });
        let split = match split {
            Ok(s) => s,
            Err(e @ DomainError::CreditNoteSplitAmbiguous(_)) => {
                self.emit_split_blocked_alarm(ctx, req, &e).await;
                return Err(e);
            }
            Err(other) => return Err(other),
        };
        let (posted_ar_incl_tax, open_ar) = self.read_ar_caps(scope, req).await?;
        let plan = build_credit_note_legs(req, &split, open_ar)?;
        let (entry, lines) = self.assemble_post(ctx, scope, req, &plan).await?;
        // Pre-load each DISTINCT account's normal_side (the composite's in-txn
        // projection needs it; loaded out-of-txn on a fresh connection, exactly as
        // `PostingService::run_post` does before opening its txn).
        let normal_sides = self.load_normal_sides(scope, &lines).await?;
        let sidecar = self.build_post_sidecar(ctx, req, &plan, posted_ar_incl_tax);
        Ok(PreparedCreditNote {
            entry,
            lines,
            normal_sides,
            sidecar,
        })
    }

    /// Apply a [`PreparedCreditNote`] as a SECOND entry inside an ALREADY-OPEN post
    /// txn (Group G composite, K-3): claim the credit note's own
    /// `(tenant, CREDIT_NOTE, credit_note_id)` dedup row, insert its header+lines,
    /// project balances (the AR no-negative guard), run its in-txn sidecar (schedule
    /// reduction + headroom seed/bump + `credit_note` row + `credit_note.posted`
    /// event), and finalize the dedup row — ALL on the `txn` the refund opened, so
    /// the credit-note entry commits atomically with the refund entry (AR is never
    /// overstated between them). Returns the posted credit-note entry id.
    ///
    /// Idempotency: a re-claim that finds the credit note's dedup row already
    /// present (a prior composite committed it) returns the prior entry id WITHOUT
    /// re-posting — the composite is then a replay. The refund's own dedup (claimed
    /// by the engine in the same txn) is the composite's primary idempotency grain.
    ///
    /// # Errors
    /// [`DomainError::IdempotencyConflict`] on a same-id / different-payload reuse;
    /// [`DomainError::CreditNoteExceedsHeadroom`] / [`DomainError::OverRecognition`]
    /// / [`DomainError::NegativeBalance`] on an in-txn CHECK; [`DomainError::Internal`]
    /// on an infra fault (any of which rolls the WHOLE composite — refund included —
    /// back).
    pub(crate) async fn apply_in_txn(
        &self,
        _ctx: &SecurityContext,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        prepared: &PreparedCreditNote,
    ) -> Result<CompositeCreditNoteOutcome, DomainError> {
        // Borrow the prepared parts (taken by reference so a serializable RETRY of
        // the outer refund post re-runs this safely — nothing is consumed). The
        // entry + lines are cloned for the insert; the sidecar runs by reference.
        let PreparedCreditNote {
            entry,
            lines,
            normal_sides,
            sidecar,
        } = prepared;
        let tenant = entry.tenant_id;
        let business_id = entry.source_business_id.clone();
        let gate = IdempotencyGate::new();
        let payload_hash = IdempotencyGate::payload_hash(entry, lines);

        // Claim the credit note's OWN dedup row in the shared txn. A conflict is a
        // replay: the credit note already posted on a prior composite (return its
        // entry id, post nothing — the refund half short-circuits on its own dedup).
        match gate
            .claim(
                txn,
                tenant,
                SourceDocType::CreditNote.as_str(),
                &business_id,
                &payload_hash,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("composite credit-note claim: {e}")))?
        {
            ClaimOutcome::Claimed => {}
            ClaimOutcome::Replay(row) => {
                if row.payload_hash != payload_hash {
                    return Err(DomainError::IdempotencyConflict(
                        "credit_note_id reused with a different payload".to_owned(),
                    ));
                }
                let entry_id = row.result_entry_id.ok_or_else(|| {
                    DomainError::Internal(
                        "composite credit-note replay: dedup row not finalized".to_owned(),
                    )
                })?;
                return Ok(CompositeCreditNoteOutcome { entry_id });
            }
        }

        let entry_ref = self
            .journal
            .insert_entry_with_lines(txn, entry.clone(), lines.clone())
            .await
            .map_err(|e| DomainError::Internal(format!("composite insert credit-note: {e}")))?;

        // Balance projection (AR no-negative guard) on the credit-note entry, in the
        // shared txn — the same guard `PostingService::post` runs for a standalone
        // credit note.
        BalanceProjector::new()
            .project(
                txn,
                scope,
                entry,
                lines,
                normal_sides,
                entry_ref.created_seq,
            )
            .await
            .map_err(map_project_err)?;

        // The credit-note in-txn sidecar (schedule reduction + headroom + record +
        // `credit_note.posted` event), against the posted facts.
        sidecar
            .run(
                txn,
                scope,
                &PostedFacts {
                    entry_id: entry_ref.entry_id,
                    created_seq: entry_ref.created_seq,
                },
            )
            .await?;

        gate.finalize(
            txn,
            tenant,
            SourceDocType::CreditNote.as_str(),
            &business_id,
            entry_ref.entry_id,
            entry_ref.created_seq,
        )
        .await
        .map_err(|e| DomainError::Internal(format!("composite credit-note finalize: {e}")))?;

        Ok(CompositeCreditNoteOutcome {
            entry_id: entry_ref.entry_id,
        })
    }

    /// Build the in-txn [`CreditNotePostSidecar`] from the plan + the posted-AR seed
    /// — factored out of [`Self::post_credit_note_inner`] so the composite path
    /// ([`Self::prepare`]) builds the same sidecar. The standalone post path keeps
    /// its inline construction (unchanged).
    fn build_post_sidecar(
        &self,
        ctx: &SecurityContext,
        req: &CreditNoteRequest,
        plan: &CreditNoteLegPlan,
        posted_ar_incl_tax: i64,
    ) -> CreditNotePostSidecar {
        CreditNotePostSidecar {
            tenant_id: req.tenant_id,
            origin_invoice_id: req.origin_invoice_id.clone(),
            currency: req.currency.clone(),
            posted_ar_incl_tax,
            credit_note_amount_minor: req.amount_minor,
            credit_note_id: req.credit_note_id.clone(),
            recognized_part_minor: plan.recognized_part_minor,
            deferred_part_minor: plan.deferred_part_minor,
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
            schedule_reductions: plan
                .legs
                .iter()
                .filter_map(planned_schedule_reduction)
                .collect(),
            credit_note_row: NewCreditNote {
                tenant_id: req.tenant_id,
                credit_note_id: req.credit_note_id.clone(),
                origin_invoice_id: req.origin_invoice_id.clone(),
                origin_invoice_item_ref: req.origin_invoice_item_ref.clone(),
                revenue_stream: req.revenue_stream.clone(),
                currency: req.currency.clone(),
                amount_minor: req.amount_minor,
                recognized_part_minor: plan.recognized_part_minor,
                deferred_part_minor: plan.deferred_part_minor,
                split_basis_ref: Some(plan.split_basis_ref.clone()),
                reason_code: req.reason_code.clone(),
                created_at_utc: OffsetDateTime::now_utc(),
            },
        }
    }

    /// Load each DISTINCT account's `normal_side` for the credit-note lines,
    /// asserting it is provisioned + `OPEN` (the composite's in-txn projection input
    /// — mirrors `PostingService::load_normal_sides`, read out-of-txn).
    async fn load_normal_sides(
        &self,
        scope: &AccessScope,
        lines: &[NewLine],
    ) -> Result<HashMap<Uuid, Side>, DomainError> {
        let mut normal_sides: HashMap<Uuid, Side> = HashMap::new();
        for line in lines {
            if normal_sides.contains_key(&line.account_id) {
                continue;
            }
            let account = self
                .reference
                .find_account(scope, line.account_id)
                .await
                .map_err(|e| DomainError::Internal(format!("composite find_account: {e}")))?
                .ok_or_else(|| {
                    DomainError::AccountClosed(format!(
                        "account {} is not provisioned",
                        line.account_id
                    ))
                })?;
            if account.lifecycle_state != LIFECYCLE_OPEN {
                return Err(DomainError::AccountClosed(format!(
                    "account {} is not OPEN",
                    line.account_id
                )));
            }
            let side = match account.normal_side.as_str() {
                "DR" => Side::Debit,
                "CR" => Side::Credit,
                other => {
                    return Err(DomainError::Internal(format!(
                        "account {} has an invalid normal_side {other:?}",
                        line.account_id
                    )));
                }
            };
            normal_sides.insert(line.account_id, side);
        }
        Ok(normal_sides)
    }

    /// Read the targeted obligation's ACTIVE per-stream `recognition_schedule`
    /// state — the splitter input (one entry per revenue stream, Slice 4 §4.5).
    /// Narrowed to the request's `(origin_invoice_id, revenue_stream)` and filtered
    /// to the targeted `origin_invoice_item_ref` (when set) + `ACTIVE` status. v1
    /// is one ACTIVE schedule per `(invoice, item, stream)`, so this yields 0 or 1
    /// state for the single requested stream. Out-of-txn (SQL-level BOLA); the
    /// schedule cap CHECK is the authoritative in-txn guard.
    async fn read_schedule_states(
        &self,
        scope: &AccessScope,
        req: &CreditNoteRequest,
    ) -> Result<Vec<ScheduleStreamState>, DomainError> {
        let (rows, truncated) = self
            .recognition
            .list_schedules(
                scope,
                req.tenant_id,
                Some(&req.origin_invoice_id),
                Some(&req.revenue_stream),
            )
            .await
            .map_err(|e| DomainError::Internal(format!("list recognition schedules: {e}")))?;
        if truncated {
            // A single (invoice, stream) result is tiny — a truncation here means a
            // pathological schedule lineage; surface as Internal rather than risk a
            // partial split basis (the splitter must see the full per-stream state).
            return Err(DomainError::Internal(format!(
                "recognition-schedule read for invoice {} / stream {} was truncated",
                req.origin_invoice_id, req.revenue_stream
            )));
        }
        let states = rows
            .into_iter()
            .filter(|s| {
                // Targeted item (when the request pins one) + only ACTIVE schedules
                // carry a reducible deferred remainder (the splitter floors a
                // non-ACTIVE one at 0 anyway; filtering keeps the basis clean).
                req.origin_invoice_item_ref
                    .as_deref()
                    .is_none_or(|item| s.source_invoice_item_ref == item)
                    && s.status == SCHEDULE_STATUS_ACTIVE
            })
            .map(map_schedule_state)
            .collect();
        Ok(states)
    }

    /// Read the invoice's posted-AR-incl-tax (the headroom seed basis) and its
    /// current open AR (the AR-vs-wallet credit cap). Both are scoped reads; the
    /// posted-AR is the original receivable from the journal (the headroom basis,
    /// independent of payments), the open AR is the payment-reduced cache. Run
    /// out-of-txn to build the entry (the headroom + AR-no-negative CHECKs are the
    /// in-txn backstops).
    async fn read_ar_caps(
        &self,
        scope: &AccessScope,
        req: &CreditNoteRequest,
    ) -> Result<(i64, i64), DomainError> {
        // Out-of-txn scoped reads on a fresh connection (no active post txn yet),
        // mirroring how the engine reads `normal_sides` before the post txn and how
        // `CreditApplicationService` reads its open-AR candidates pre-txn — the
        // headroom + AR no-negative CHECKs are the authoritative in-txn backstops.
        let posted_ar = self
            .adjustment
            .read_posted_ar_incl_tax_out_of_txn(scope, req.tenant_id, &req.origin_invoice_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read posted AR: {e}")))?;
        let open_ar = self
            .adjustment
            .read_open_ar_for_invoice_out_of_txn(scope, req.tenant_id, &req.origin_invoice_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read open AR: {e}")))?;
        Ok((posted_ar, open_ar))
    }

    /// Resolve each planned leg's chart `account_id` + currency scale and assemble
    /// the engine [`NewEntry`] + [`NewLine`] vector. Per-stream classes
    /// (`CONTRA_REVENUE`/`CONTRACT_LIABILITY`) resolve on their stream; the rest
    /// resolve stream-less. The header is `source_doc_type = CreditNote` +
    /// `source_business_id = credit_note_id` (the engine's idempotency key).
    async fn assemble_post(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &CreditNoteRequest,
        plan: &CreditNoteLegPlan,
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
            source_doc_type: SourceDocType::CreditNote,
            // The engine's `(tenant, CREDIT_NOTE, credit_note_id)` idempotency key.
            source_business_id: req.credit_note_id.clone(),
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
    /// [`NewLine`]. AR / `REUSABLE_CREDIT` carry `payer_tenant_id` + `invoice_id`
    /// (their cache grains key on them); the `CR REUSABLE_CREDIT` leg carries
    /// `credit_grant_event_type` so the projector seeds the wallet sub-grain.
    fn mk_line(req: &CreditNoteRequest, leg: &PlannedLeg, account_id: Uuid, scale: u8) -> NewLine {
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
            // single dimensionless tax leg. The amount is the reversed tax (never
            // recomputed here).
            tax_jurisdiction: leg.tax_jurisdiction.clone(),
            tax_filing_period: leg.tax_filing_period.clone(),
            tax_rate_ref: leg.tax_rate_ref.clone(),
            legal_entity_id: None,
            invoice_item_ref: req.origin_invoice_item_ref.clone(),
            sku_or_plan_ref: None,
            price_id: None,
            pricing_snapshot_ref: None,
            po_allocation_group: req.po_allocation_group.clone(),
            credit_grant_event_type: leg.credit_grant_event_type.clone(),
            ar_status: None,
        }
    }

    /// Raise the `CreditNoteSplitBlocked` invariant alarm out-of-band on the
    /// split-ambiguous reject (Group F / Slice 6 catalog). Fire-and-forget on the
    /// publisher's own committed connection (the note was rejected with NO books
    /// effect, so there is no post txn to ride); `Warn` severity (a config-gap
    /// triage signal, not an integrity breach). Ids only, no PII — the
    /// `origin_invoice_item_ref` is an internal ref, mirrored into the `affected`
    /// list so an operator can locate the obligation whose split basis is missing.
    async fn emit_split_blocked_alarm(
        &self,
        ctx: &SecurityContext,
        req: &CreditNoteRequest,
        err: &DomainError,
    ) {
        let alarm = LedgerInvariantAlarm {
            category: AlarmCategory::CreditNoteSplitBlocked,
            severity: AlarmSeverity::Warn,
            tenant_id: req.tenant_id,
            scope: format!(
                "tenant:{}/flow:CREDIT_NOTE/business:{}",
                req.tenant_id, req.credit_note_id
            ),
            code: "CREDIT_NOTE_SPLIT_AMBIGUOUS".to_owned(),
            detail: err.to_string(), // internal diagnostic — no PII
            affected: vec![AffectedItem {
                id: format!(
                    "invoice:{}/stream:{}/item:{}",
                    req.origin_invoice_id,
                    req.revenue_stream,
                    req.origin_invoice_item_ref.as_deref().unwrap_or("")
                ),
                currency: req.currency.clone(),
                expected_minor: 0,
                actual_minor: 0,
            }],
        };
        self.publisher.emit_invariant_alarm(ctx, alarm).await;

        // Slice 7 Phase 2: ADDITIVELY open a durable close-blocking exception row
        // beside the alarm above (the split-ambiguous note is still rejected; this
        // only makes the condition block the next close until resolved).
        if let Some(ex) = &self.exceptions {
            ex.route(
                req.tenant_id,
                crate::domain::exception::ExceptionType::SplitAmbiguous,
                &req.credit_note_id,
                Some(serde_json::json!({
                    "credit_note_id": req.credit_note_id,
                    "origin_invoice_id": req.origin_invoice_id,
                })),
            )
            .await;
        }
    }
}

/// Classify a credit-note attempt result into its `ledger_credit_note_total`
/// `outcome` label (Group F): the two block reasons get their own labels (the
/// split-ambiguous + headroom-cap rejects), a replay is `replayed`, a fresh post
/// is `posted`, and every other rejection (shape / not-found / negative-balance /
/// infra) folds into `rejected`.
fn note_outcome(result: &Result<PostingRef, DomainError>) -> NoteOutcome {
    match result {
        Ok(r) if r.replayed => NoteOutcome::Replayed,
        Ok(_) => NoteOutcome::Posted,
        Err(DomainError::CreditNoteSplitAmbiguous(_)) => NoteOutcome::BlockedSplit,
        Err(DomainError::CreditNoteExceedsHeadroom(_)) => NoteOutcome::BlockedHeadroom,
        Err(_) => NoteOutcome::Rejected,
    }
}

/// A credit note prepared out-of-txn for the ATOMIC composite post (Group G,
/// `refund-with-credit-note`): the engine entry + lines, the pre-loaded
/// `normal_side`s for the in-txn projection, and the in-txn
/// [`CreditNotePostSidecar`] (schedule reduction + headroom + record + event). The
/// composite orchestrator posts this as the SECOND entry inside the refund's post
/// txn via [`CreditNoteHandler::apply_in_txn`]. Carried by value (moved into the
/// refund's composite sidecar).
pub(crate) struct PreparedCreditNote {
    entry: NewEntry,
    lines: Vec<NewLine>,
    normal_sides: std::collections::HashMap<Uuid, Side>,
    sidecar: CreditNotePostSidecar,
}

/// The outcome of applying a [`PreparedCreditNote`] in the composite txn
/// ([`CreditNoteHandler::apply_in_txn`]): the posted (or idempotently replayed)
/// credit-note entry id.
pub(crate) struct CompositeCreditNoteOutcome {
    pub entry_id: Uuid,
}

/// Map a [`ProjectError`](crate::infra::posting::projector::ProjectError) from the
/// composite credit-note projection into the handler's [`DomainError`]: a negative
/// AR balance is the Slice-1 floor (`NEGATIVE_BALANCE`, D3); everything else is an
/// infra fault that rolls the WHOLE composite (refund included) back. Mirrors the
/// `project_to_db` mapping `PostingService` uses for a standalone post.
fn map_project_err(e: crate::infra::posting::projector::ProjectError) -> DomainError {
    use crate::infra::posting::projector::ProjectError;
    match e {
        ProjectError::NegativeBalance {
            account_id,
            balance_minor,
        } => DomainError::NegativeBalance(format!(
            "balance for account {account_id} would go negative ({balance_minor})"
        )),
        ProjectError::MissingNormalSide(id) => {
            DomainError::AccountClosed(format!("missing normal_side for account {id}"))
        }
        ProjectError::MissingCreditEventType(id) => DomainError::Internal(format!(
            "REUSABLE_CREDIT line {id} missing credit_grant_event_type"
        )),
        ProjectError::Overflow {
            account_id,
            currency,
            field,
        } => DomainError::AmountOutOfRange(format!(
            "coalesced money delta overflowed i64 for account {account_id} ({currency}, {field})"
        )),
        ProjectError::Db(e) => DomainError::Internal(format!("composite projector: {e}")),
    }
}

/// One in-txn schedule deferred reduction the sidecar applies: the schedule id +
/// the ex-tax deferred amount to subtract from its `total_deferred_minor`. Derived
/// from a planned `DR CONTRACT_LIABILITY` leg (which carries the owning
/// `schedule_id`).
#[derive(Clone, Debug)]
struct ScheduleReduction {
    schedule_id: String,
    amount_minor: i64,
}

/// Project a planned `DR CONTRACT_LIABILITY` leg into its schedule reduction (it
/// carries the owning `schedule_id`); every other leg yields `None`.
fn planned_schedule_reduction(leg: &PlannedLeg) -> Option<ScheduleReduction> {
    match (leg.account_class, leg.side, leg.schedule_id.as_ref()) {
        (AccountClass::ContractLiability, Side::Debit, Some(schedule_id)) => {
            Some(ScheduleReduction {
                schedule_id: schedule_id.clone(),
                amount_minor: leg.amount_minor,
            })
        }
        _ => None,
    }
}

/// Map a stored `recognition_schedule` row into the splitter's pure
/// [`ScheduleStreamState`] input.
fn map_schedule_state(s: recognition_schedule::Model) -> ScheduleStreamState {
    ScheduleStreamState {
        revenue_stream: s.revenue_stream,
        schedule_id: s.schedule_id,
        total_deferred_minor: s.total_deferred_minor,
        recognized_minor: s.recognized_minor,
        status: s.status,
        version: s.version,
    }
}

/// The in-transaction [`PostSidecar`] for a credit note: runs AFTER balance
/// projection and BEFORE the dedup finalize (fresh-claim path only — a replay
/// returns before the sidecar), so all its writes commit atomically with the
/// journal entry or roll back with it (design §4.2 / §4.7). It performs, in the
/// §4.7 lock order (`recognition_schedule`, then `invoice_exposure)`:
///
/// 1. **Schedule reduction** (rank 6): for each touched schedule, decrement
///    `total_deferred_minor` by the deferred ex-tax part. The
///    `recognized_minor <= total_deferred_minor` CHECK is the over-reduction guard
///    (a later run cannot re-recognize the credited-back amount); a breach surfaces
///    as `MoneyOutCapExceeded`, refined here to `OverRecognition`.
/// 2. **Headroom** (rank 10): first-touch seed `invoice_exposure`
///    (`original_total_minor` = posted AR), then bump `credit_note_total_minor` by
///    the note's incl-tax amount. The `chk_ledger_invoice_exposure_headroom` CHECK
///    is the authoritative over-cap guard; a breach surfaces as `MoneyOutCapExceeded`,
///    refined here to `CreditNoteExceedsHeadroom` (→ `CREDIT_NOTE_EXCEEDS_HEADROOM`).
/// 3. **Persist** the `credit_note` row.
///
/// **Wallet remainder (d)** needs no work here: the `CR REUSABLE_CREDIT` leg
/// carries `credit_grant_event_type = CREDIT_NOTE`, so the engine's `BalanceProjector`
/// already seeded the `reusable_credit_subbalance` sub-grain from the posted line
/// (this sidecar runs after projection).
pub struct CreditNotePostSidecar {
    tenant_id: Uuid,
    origin_invoice_id: String,
    currency: String,
    /// The posted AR incl. tax — the `invoice_exposure.original_total_minor` seed.
    posted_ar_incl_tax: i64,
    /// The note's incl-tax amount — the `credit_note_total_minor` bump delta +
    /// the published event's `amount_minor`.
    credit_note_amount_minor: i64,
    /// The note's business id — the published event's `credit_note_id`.
    credit_note_id: String,
    /// The ex-tax recognized part — the published event's `recognized_part_minor`.
    recognized_part_minor: i64,
    /// The ex-tax deferred part — the published event's `deferred_part_minor`.
    deferred_part_minor: i64,
    /// The event publisher: `billing.ledger.credit_note.posted` is published IN
    /// this post txn (the transactional outbox) so it commits atomically with the
    /// entry + counters, or rolls back with them. Mirrors
    /// [`SettlementReturnSidecar`](crate::infra::payment::sidecar::SettlementReturnSidecar).
    publisher: Arc<LedgerEventPublisher>,
    /// The security context for the in-txn outbox publish (cloned by the handler).
    ctx: SecurityContext,
    /// The per-schedule deferred reductions (one per touched `CONTRACT_LIABILITY`
    /// leg); empty for a goodwill / fully-recognized note (no schedule touch).
    schedule_reductions: Vec<ScheduleReduction>,
    /// The `credit_note` record to persist.
    credit_note_row: NewCreditNote,
}

#[async_trait::async_trait]
impl PostSidecar for CreditNotePostSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. Schedule reduction (rank 6) — reduce each touched schedule's deferred
        //    total over its unreleased remainder. The CHECK is the over-reduction
        //    backstop (refined to OverRecognition).
        for r in &self.schedule_reductions {
            RecognitionRepo::reduce_deferred(
                txn,
                scope,
                self.tenant_id,
                &r.schedule_id,
                r.amount_minor,
            )
            .await
            .map_err(map_schedule_repo_err)?;
        }

        // 2. Headroom (rank 10) — first-touch seed then bump. The headroom CHECK is
        //    the authoritative over-cap guard, refined to CreditNoteExceedsHeadroom
        //    (→ CREDIT_NOTE_EXCEEDS_HEADROOM, 400).
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
        AdjustmentRepo::add_credit_note_total(
            txn,
            scope,
            self.tenant_id,
            &self.origin_invoice_id,
            self.credit_note_amount_minor,
        )
        .await
        .map_err(map_headroom_repo_err)?;

        // 3. Persist the credit_note record row.
        AdjustmentRepo::insert_credit_note(txn, scope, &self.credit_note_row)
            .await
            .map_err(|e| DomainError::Internal(format!("insert credit_note: {e}")))?;

        // 4. Publish `billing.ledger.credit_note.posted` into the SAME post txn
        //    (transactional outbox): the event row commits atomically with the
        //    entry + the schedule/headroom/record writes, or a publish failure
        //    rolls the whole post back. Never on replay (a replay returns before
        //    the sidecar). Ids + amount + split parts only (no PII).
        self.publisher
            .publish_credit_note_posted(
                &self.ctx,
                txn,
                CreditNotePosted {
                    tenant_id: self.tenant_id,
                    credit_note_id: self.credit_note_id.clone(),
                    origin_invoice_id: self.origin_invoice_id.clone(),
                    entry_id: posted.entry_id,
                    currency: self.currency.clone(),
                    amount_minor: self.credit_note_amount_minor,
                    recognized_part_minor: self.recognized_part_minor,
                    deferred_part_minor: self.deferred_part_minor,
                    posted_at_utc: OffsetDateTime::now_utc(),
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish credit_note_posted: {e}")))?;

        Ok(())
    }
}

/// Map a schedule-counter [`RepoError`](crate::domain::model::RepoError) into the
/// sidecar's [`DomainError`]: the `recognized_minor <= total_deferred_minor` CHECK
/// violation (an over-reduction of an in-flight schedule) becomes
/// [`DomainError::OverRecognition`] (the `OVER_RECOGNITION` 409 — the credited-back
/// amount cannot exceed the unreleased remainder, mirroring the recognition stamp
/// sidecar); every other repo failure is an infrastructure fault that rolls the
/// post back.
fn map_schedule_repo_err(e: crate::domain::model::RepoError) -> DomainError {
    use crate::domain::model::RepoError;
    match e {
        RepoError::MoneyOutCapExceeded(m) => DomainError::OverRecognition(format!(
            "credit-note deferred reduction exceeds the schedule's releasable remainder: {m}"
        )),
        other => DomainError::Internal(format!("credit-note schedule reduction: {other}")),
    }
}

/// Map a headroom-counter [`RepoError`](crate::domain::model::RepoError) into the
/// sidecar's [`DomainError`]: the `invoice_exposure` headroom CHECK violation
/// becomes [`DomainError::CreditNoteExceedsHeadroom`] (the `CREDIT_NOTE_EXCEEDS_HEADROOM`
/// 400 — design §4.2 / AC #24; over-cap routes via goodwill/non-revenue, never
/// silently through S3); every other repo failure is an infra fault.
fn map_headroom_repo_err(e: crate::domain::model::RepoError) -> DomainError {
    use crate::domain::model::RepoError;
    match e {
        RepoError::MoneyOutCapExceeded(m) => DomainError::CreditNoteExceedsHeadroom(format!(
            "credit note would exceed the invoice's remaining headroom: {m}"
        )),
        other => DomainError::Internal(format!("credit-note headroom bump: {other}")),
    }
}
