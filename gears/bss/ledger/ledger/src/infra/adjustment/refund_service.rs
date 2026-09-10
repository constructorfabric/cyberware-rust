//! `RefundHandler` — the Slice-3 Phase-2 refund orchestrator (design §4.4, Groups
//! B + C). It posts a refund stage's balanced two-leg entry through the invariant
//! [`PostingService`] and, in the SAME serializable transaction (via a
//! [`PostSidecar`]), maintains the `payment_settlement` /
//! `payment_allocation_refund` money-out CAPS and persists the `refund` record
//! row:
//!
//! 1. **shape gate** ([`validate_shape`]): amounts + the Pattern-A/B `invoice_id`
//!    rule + the single-step/`confirmed` rule — a clean 400 before any read.
//! 2. **resolve the origin settlement** (out-of-txn, scoped): a refund unwinds a
//!    settled receipt, so the origin `payment_settlement` MUST exist for
//!    `(tenant, payment_id)` and its currency MUST match the refund's
//!    (`RefundOriginNotFound` / `CurrencyMismatch`). The settlement counters are
//!    the cap basis Group C guards; the lockless read is the pre-flight, the in-txn
//!    CHECKs are the authoritative backstop.
//! 3. **route by phase**:
//!    - `initiated` / `confirmed` → **forward post** ([`Self::post_forward`]):
//!      build the two-leg plan ([`build_refund_legs`]), post it with the
//!      [`RefundPostSidecar`] in [`CapMode::Initiate`] (stage-1, both patterns) /
//!      [`CapMode::None`] (stage-2 `confirmed` — the cash was already capped at
//!      stage-1; stage-2 only drains `REFUND_CLEARING`).
//!    - `rejected` / `voided` → **stage-1 reversal** ([`Self::post_reversal`],
//!      Group C): resolve the stage-1 `initiated` entry, post its STRICT
//!      line-negation (`reverses_entry_id = <stage-1 entry id>`; the legs are the
//!      stage-1 legs with sides inverted — `DR REFUND_CLEARING` against the
//!      restored `UNALLOCATED`(A) / `AR`(B)), and run the sidecar in
//!      [`CapMode::Release`] to DECREMENT the same counters stage-1 reserved + drain
//!      the `REFUND_CLEARING` balance to zero.
//!    - `unknown_final` → the terminal **dual-control disposition** (Group F,
//!      [`Self::post_unknown_final`]): PARKS the stuck `REFUND_CLEARING` on the
//!      SUSPENSE holding account (`DR REFUND_CLEARING · CR SUSPENSE` — not a
//!      premature loss/gain, the outcome is unknown) and writes a
//!      `secured_audit_record` (via the Slice-6 [`SecuredAuditSink`] port) in the
//!      same txn. Gated like a stage-1 cash commitment.
//!
//! **Caps (design §4.4 / §4.7, Group C).** A refund is money-OUT; the cap is
//! reserved at stage-1 initiation (before the cash leaves) and released on a
//! stage-1 reversal, ALL under the rank-1 `payment_settlement` lock with the
//! post-delta CHECKs as the authoritative over-refund backstop:
//! - **both patterns** bump `payment_settlement.refunded_minor` (total money-out:
//!   `refunded + clawed_back <= settled`);
//! - **Pattern A** additionally bumps `refunded_unallocated_minor` (spendable
//!   headroom: `allocated + refunded_unallocated <= settled` — refunded on-account
//!   cash can no longer be allocated);
//! - **Pattern B** additionally bumps `payment_allocation_refund.refunded_minor`
//!   per `(payment, invoice)` (`refunded <= allocated`).
//!
//! A cap-CHECK violation surfaces as [`RepoError::MoneyOutCapExceeded`], refined to
//! [`DomainError::RefundExceedsSettled`] (the settlement caps) /
//! [`DomainError::RefundExceedsAllocated`] (the per-invoice cap).
//!
//! **Idempotency** is the engine's `(tenant, REFUND, source_business_id)` claim
//! with `source_business_id = "{psp_refund_id}:{phase}"` (design §7: one PSP refund
//! advances through several phase rows — `initiated`, `confirmed`, OR
//! `rejected`/`voided` — each idempotent on `(psp_refund_id, phase)`). The `refund`
//! row's surrogate `(tenant, refund_id)` PK + the natural `UNIQUE (tenant,
//! psp_refund_id, phase)` index are the durable backstop.
//!
//! **Scope.** Groups B–F are in this handler: the two-leg posts + caps (B/C),
//! dual-control over the D2 threshold (D), refund-of-refund + the out-of-order /
//! underflow defer (E), and the `unknown_final` loss-clearing disposition +
//! secured-audit (F). REST (Group G) is the remaining surface.
//!
//! Lives in `infra` (not `domain`): it needs repo + posting access; the
//! [`refund`](crate::domain::adjustment::refund) domain it calls stays pure (dylint
//! DE0301). Wraps the `pub` [`PostingService`] + repos directly (like
//! [`CreditNoteHandler`](super::credit_note_service::CreditNoteHandler)) so it is
//! constructible from out-of-crate integration tests.

use std::sync::Arc;

use bss_ledger_sdk::{AccountClass, MappingStatus, PostingRef, Side, SourceDocType};
use chrono::Datelike;
use sea_orm::DbErr;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::adjustment::refund::{
    CLEARING_STATE_PENDING, CLEARING_STATE_REVERSED, CLEARING_STATE_SETTLED, PlannedLeg,
    RefundDirection, RefundLegPlan, RefundPattern, RefundPhase, RefundRequest, build_refund_legs,
    validate_shape,
};
use crate::domain::approval::intent::{ApprovalIntent, RefundIntent, RefundWithCreditNoteIntent};
use crate::domain::approval::policy::OperationFacts;
use crate::domain::error::DomainError;
use crate::domain::fx::realized::carried_relief;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine, RepoError};
use crate::domain::payment::chargeback::DisputePhase;
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::adjustment::credit_note_service::{
    CompositeCreditNoteOutcome, CreditNoteHandler, PreparedCreditNote,
};
use crate::infra::approval::service::ApprovalService;
use crate::infra::audit::secured_audit_sink::{
    AuditEventType, NoopSecuredAuditSink, SecuredAuditSink,
};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::payloads::{
    AffectedItem, AlarmCategory, AlarmSeverity, LedgerInvariantAlarm, RefundRecorded,
};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::exception::ExceptionRouter;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::idempotency::{
    ClaimOutcome, IdempotencyGate, STATUS_POSTED, STATUS_QUEUED,
};
use crate::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use crate::infra::storage::entity::pending_event_queue;
use crate::infra::storage::repo::adjustment_repo::NewRefund;
use crate::infra::storage::repo::{
    AdjustmentRepo, DisputeRepo, JournalRepo, NewQueueRow, PaymentRepo, PendingQueueRepo,
    ReferenceRepo,
};
use time::Duration;
use time::OffsetDateTime;

/// The WORK-STATE queue `flow` for a refund-of-refund claw-back that DEFERRED on an
/// out-of-order / would-underflow money-out decrement (Group E, design §4.4). This
/// partitions the `ledger_pending_event_queue` ROWS so [`RefundHandler::drain_clawbacks`]
/// (and the periodic sweep) claim ONLY claw-back rows — the chargeback / allocation
/// sweeps never pick one up, and vice-versa. Reuses the unconstrained `flow
/// varchar(64)` — no new DDL (mirrors how `CHARGEBACK` reused it).
///
/// NOTE: this is the QUEUE-row flow, NOT the engine dedup flow. The deferred apply
/// re-drives the claw-back through [`PostingService::post_queued_apply`], whose
/// dedup lookup keys on the ENTRY's source-doc — `REFUND` ([`FLOW_REFUND_ENGINE`]) —
/// so the DEDUP row is claimed under `REFUND` (matching the post), while the
/// work-state row lives under `REFUND_CLAWBACK`. A claw-back carries its OWN
/// `psp_refund_id` (distinct from the outbound it claws back), so the
/// `(tenant, REFUND, clawback_psp:initiated)` engine claim never collides with the
/// outbound stage-1's `(tenant, REFUND, outbound_psp:initiated)`.
const FLOW_REFUND_CLAWBACK: &str = "REFUND_CLAWBACK";

/// The WORK-STATE queue `flow` for a QUARANTINED refund-before-payment (Group G,
/// design §4.4 / PRD L668 / Rev2 E-11). Distinct from every other flow so the
/// allocation / chargeback / claw-back sweeps never pick a quarantined refund up,
/// and the de-quarantine drain ([`RefundHandler::drain_quarantine`]) claims ONLY
/// these rows. A quarantined refund is NEVER posted from the queue blindly:
/// de-quarantine RE-VALIDATES the origin settlement + all §4.7 caps + the
/// THEN-CURRENT D2 threshold + the dispute state before any post — over-threshold
/// routes to approval, never auto-posts (this is the ESSENTIAL difference from the
/// queue-and-apply `REFUND_CLAWBACK` / allocation flows, which DO auto-apply on
/// drain). Reuses the unconstrained `flow varchar(64)` — no new DDL.
const FLOW_REFUND_QUARANTINE: &str = "REFUND_QUARANTINE";

/// The `QUEUED` dedup/queue-row status the quarantine drain targets (lockstep with
/// the engine `STATUS_QUEUED`). A quarantined refund's work-state row sits `QUEUED`
/// until de-quarantine posts it (→ the row is left for the post path, which keys on
/// the refund's own `(tenant, REFUND, psp_refund_id:phase)` engine dedup) or gives
/// it up.
const QUARANTINE_AGING_SECS: i64 = 14 * 24 * 60 * 60;

/// The WORK-STATE queue `flow` for a refund HELD because its origin payment has an
/// OPEN dispute (Z5-2, design §5). DISTINCT from every other flow so the allocation
/// / chargeback / claw-back / quarantine sweeps never pick a dispute-held refund up,
/// and the dispute-hold drain ([`RefundHandler::drain_dispute_hold`]) claims ONLY
/// these rows. A dispute-held refund is NEVER posted from the queue blindly: the
/// drain RE-READS the dispute and only re-drives the refund post once the dispute
/// resolves WON (the payment stands); a LOST resolution CANCELS the hold (a lost
/// chargeback already returned the money — posting the refund too would double-pay).
/// Reuses the unconstrained `flow varchar(64)` — no new DDL (mirrors how
/// `REFUND_QUARANTINE` reused it). The held payload re-drives through the refund's
/// OWN `(tenant, REFUND, psp_refund_id:phase)` engine claim, so the post path
/// finalizes that dedup row while the work-state row lives under
/// `REFUND_DISPUTE_HOLD`.
const FLOW_REFUND_DISPUTE_HOLD: &str = "REFUND_DISPUTE_HOLD";

/// Aging horizon for a dispute-held refund (design §5 / §13 — a dispute that never
/// resolves should not strand a refund silently). A hold row that has sat `QUEUED`
/// longer than this with its dispute STILL `OPENED` is CANCELLED at the next drain +
/// escalated to the exception stub + a `RefundQuarantined` Critical alarm (no
/// dispute-specific category exists in the vendored schema; `RefundQuarantined` is
/// the closest refund-held-out-of-band signal — see the alarm raise). 30 days —
/// card-network dispute lifecycles routinely run weeks, so this is generous enough
/// that only a genuinely stuck dispute escalates. A const for now (wire to a
/// `jobs.dispute_hold_aging_secs` config when per-deployment tuning is needed —
/// deferred, mirrors `QUARANTINE_AGING_SECS`).
const DISPUTE_HOLD_AGING_SECS: i64 = 30 * 24 * 60 * 60;

/// The ENGINE idempotency-dedup `flow` for a claw-back — the `REFUND` source-doc
/// literal (lockstep with [`SourceDocType::Refund`]). The defer intake claims the
/// `QUEUED` dedup row under THIS flow (so `post_queued_apply`, which derives the
/// flow from the entry's `source_doc_type = Refund`, reads + finalizes the SAME
/// row). `as_str` is not `const`, so it can't be derived in a `const` initializer.
const FLOW_REFUND_ENGINE: &str = "REFUND";

/// Aging horizon for a deferred claw-back (design §4.4 — "never hard-fail; ESCALATE
/// when it never reconciles"). A claw-back row that has sat `QUEUED` longer than
/// this without its matching outbound refund stage-1 landing is CANCELLED at the
/// next drain + escalated to the exception stub + a `CLAWBACK_UNDERFLOW` alarm.
/// 7 days — generous relative to the minutes-to-hours a normal outbound/claw-back
/// pair takes to reconcile, so only a genuinely orphaned claw-back escalates. A
/// const for now (wire to `jobs.clawback_aging_secs` config when per-deployment
/// tuning is needed — deferred, mirrors `AgedAlarmJob`).
const CLAWBACK_AGING_SECS: i64 = 7 * 24 * 60 * 60;

/// Origin literal stamped on posts made through this service (mirrors the peer
/// orchestrators).
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// The account class the `unknown_final` disposition PARKS the stuck
/// `REFUND_CLEARING` amount onto (design §4.4 / K-1). `unknown_final` means the PSP
/// could produce NO final state — so the outcome (paid → cash, cancelled → release,
/// or genuinely lost → write-off) is UNKNOWN. Booking it straight to a loss (or a
/// gain) would assert an outcome we do not yet know, so the disposition PARKS the
/// amount on the `SUSPENSE` clearing account — the standard "known amount, unknown
/// attribution" holding account — pending reconciliation. The terminal disposition
/// (loss / release / paid) is a separate governed step once the status resolves
/// (the `exception_queue`, Slice 7 / VHP-1859). `SUSPENSE` already exists in the
/// chart (no SDK enum / provisioning change), and the park carries no P&L sign, so
/// it neither prematurely recognizes a loss nor a gain.
const UNKNOWN_FINAL_PARK_CLASS: AccountClass = AccountClass::Suspense;

/// The closed reason code stamped on the `unknown_final` disposition's secured
/// audit record (design §4.4 — a governed write-off carries a mandatory reason
/// code + actor, AC #14). A stable literal (NOT free text).
const REASON_REFUND_UNKNOWN_FINAL: &str = "REFUND_UNKNOWN_FINAL";

/// The closed reason code stamped on the secured-audit record captured when a
/// refund intake hits an idempotency-key reuse with a DIFFERENT payload (AC #19,
/// Z14-1 — the secured-audit trail records the attempted conflicting reuse). A
/// stable literal (NOT free text).
const REASON_IDEMPOTENCY_CONFLICT: &str = "IDEMPOTENCY_CONFLICT";

/// Whether (and how) the [`RefundPostSidecar`] moves the per-payment money-out cap
/// counters (design §4.4 / §4.7, Group C). The cap is reserved at stage-1
/// initiation and released on a stage-1 reversal; stage-2 `confirmed` (the cash was
/// already capped at stage-1) leaves the counters untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapMode {
    /// Stage-1 initiation (both patterns): INCREMENT the caps (positive Δ). The
    /// post-delta CHECKs are the over-refund backstop. Used by an OUTBOUND refund —
    /// a plain stage-1 OR an additional-outbound refund-of-refund (cash out again),
    /// which rides the same money-out cap.
    Initiate,
    /// Stage-1 reversal (PSP `rejected`/`voided`): DECREMENT the caps (negative Δ)
    /// by exactly the stage-1 amount — releasing the reservation. A decrement never
    /// trips a cap CHECK and cannot underflow the nonneg CHECK (it backs out the
    /// matching initiation).
    Release,
    /// Refund-of-refund CLAW-BACK stage-1 (Group E, design §4.4 / Rev3): DECREMENT
    /// the origin money-out counters (negative Δ) so the total money-out cap
    /// reflects the NET refunded, mirroring [`CapMode::Release`]'s arithmetic. The
    /// DIFFERENCE from `Release` is the GUARD: a claw-back's decrement is NOT
    /// guaranteed to back out a matching prior increment (the PSP claw-back may
    /// arrive out-of-order / over-claw), so the handler PRE-CHECKS the underflow
    /// under the rank-1 lock BEFORE constructing this mode and DEFERS instead of
    /// applying when `current - amount < 0` (the `refunded_minor >= 0` CHECK stays a
    /// backstop that must never fire). By the time this mode is built the underflow
    /// pre-check has passed, so the decrement is safe.
    Clawback,
    /// Stage-2 `confirmed`: NO counter movement (the cash was capped at stage-1;
    /// stage-2 only drains `REFUND_CLEARING`). Also the claw-back stage-2 (the
    /// counters moved at the claw-back stage-1).
    None,
}

/// Orchestrates the refund domain (design §4.4) over the foundation engine.
pub struct RefundHandler {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    /// Reads the origin `payment_settlement` row (existence + currency + the cap
    /// basis the §4.7 counters guard; AND the claw-back underflow pre-read under the
    /// rank-1 lock, Group E).
    payment: PaymentRepo,
    /// Resolves the stage-1 `initiated` journal entry id a stage-1 reversal negates
    /// (`reverses_entry_id`), by its `(tenant, REFUND, psp_refund_id:initiated)`
    /// business id (Group C).
    journal: JournalRepo,
    /// Reads the OPEN dispute on the origin payment, if any — the refund
    /// dispute-hold pre-read (Z5-2, design §5). A refund must NOT move cash on a
    /// payment with an OPEN dispute (the disputed funds are sub judice); the handler
    /// holds the cash leg until the dispute resolves. Also re-read by the hold drain
    /// to decide WON (re-drive) vs LOST (cancel) vs still-OPEN (back off).
    dispute: DisputeRepo,
    /// Reads the live `refund` row by its `(tenant, psp_refund_id, phase)` grain —
    /// the `unknown_final` disposition's REAL clearing-state pre-read (Z5-4). The
    /// disposition writes off the STAGE-1 (`initiated`) row's actual open clearing,
    /// not a hardcoded `PENDING` / request amount; an already-resolved
    /// (`SETTLED`/`REVERSED`) stage-1 makes the disposition a no-op rather than an
    /// over-DR.
    adjustment: AdjustmentRepo,
    /// The deferred-apply queue (work-state SoT): a refund-of-refund claw-back whose
    /// money-out decrement would underflow (out-of-order PSP claw-back) is enqueued
    /// here at intake (Group E, design §4.4) and drained later by
    /// [`Self::drain_clawbacks`] when the matching outbound refund stage-1 lands.
    pending_queue: PendingQueueRepo,
    /// One database provider, retained so the claw-back defer intake + the drain
    /// claim can open their own `db.transaction` (mirrors `ChargebackService`).
    db: DBProvider<DbError>,
    /// The event publisher — retained so the never-reconcile escalation can raise
    /// the out-of-band `ClawbackUnderflow` alarm (Group E). The posting engine also
    /// holds its own clone (threaded at `new`).
    publisher: Arc<LedgerEventPublisher>,
    /// The dual-control engine (VHP-1852, Group D). `Some` ⇒ a forward refund whose
    /// cash crosses the tenant's D2 threshold is gated to the preparer→approver
    /// queue (`DualControlRequired`) instead of posting inline; `None` ⇒ gating is
    /// disabled (the executor's approved replay, and the Group-B unit tests that
    /// construct the handler without the engine). Wired in `module` (Group G).
    approval: Option<Arc<ApprovalService>>,
    /// The secured-audit sink (Slice 6 seam, Group F). The `unknown_final`
    /// disposition writes one `secured_audit_record` IN the post txn (atomic with
    /// the loss-clearing entry) via [`SecuredAuditSink::append`]. Until Slice 6
    /// merges this is the [`NoopSecuredAuditSink`] (logs + metric, nothing
    /// durable); the real `SecuredAuditStore` binds here at merge. Defaulted at
    /// `new` (so the Group-B callers/tests stay source-compatible) and overridable
    /// via [`Self::with_audit_sink`].
    audit: Arc<dyn SecuredAuditSink>,
    /// Metrics sink (Group F + G): `ledger_refund_unknown_final_total` on a
    /// disposition, `ledger_refund_total{phase,pattern}` on a fresh post, and
    /// `ledger_refund_quarantine_depth` on the quarantine sweep. Defaulted to the
    /// no-op at `new`; the wired meter binds via [`Self::with_metrics`].
    metrics: Arc<dyn LedgerMetricsPort>,
    /// The credit-note orchestrator (Group G composite). `Some` ⇒ the
    /// `refund-with-credit-note` atomic composite can post the paired S3 credit
    /// note as the SECOND entry inside the refund's post txn; `None` ⇒ the composite
    /// is unavailable (the executor's un-gated handler, and the Group-B/E unit tests
    /// that construct the handler without it). Wired in `module` (Group G) via
    /// [`Self::with_credit_note_handler`].
    credit_note: Option<Arc<super::credit_note_service::CreditNoteHandler>>,
    // Slice 7 Phase 2: routes the `CLAWBACK_UNDERFLOW` escalation stub to a durable
    // close-blocking exception row (ADDITIVE beside the alarm). `None` until
    // `with_exceptions` wires it (so existing constructions are unchanged).
    exceptions: Option<Arc<ExceptionRouter>>,
}

impl RefundHandler {
    /// Build the handler over one database provider + the event publisher
    /// (threaded into the posting engine — the engine publishes its own
    /// post-committed facts). The publisher is ALSO retained (Group E) so the
    /// never-reconcile claw-back escalation can raise the out-of-band
    /// `ClawbackUnderflow` alarm; Group G re-threads it for
    /// `billing.ledger.refund.recorded`.
    #[must_use]
    pub fn new(db: DBProvider<DbError>, publisher: Arc<LedgerEventPublisher>) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let payment = PaymentRepo::new(db.clone());
        let journal = JournalRepo::new(db.clone());
        let dispute = DisputeRepo::new(db.clone());
        let adjustment = AdjustmentRepo::new(db.clone());
        let pending_queue = PendingQueueRepo::new(db.clone());
        Self {
            posting,
            reference,
            resolver,
            payment,
            journal,
            dispute,
            adjustment,
            pending_queue,
            db,
            publisher,
            approval: None,
            // Default to the pre-Slice-6 no-op sink + the no-op metrics. `module`
            // overrides via `with_audit_sink` / `with_metrics` (Group G); the
            // Group-B/F unit + integration tests inject a spy sink to assert the
            // `unknown_final` disposition's secured-audit append.
            audit: Arc::new(NoopSecuredAuditSink::new()),
            metrics: Arc::new(crate::domain::ports::metrics::NoopLedgerMetrics),
            credit_note: None,
            exceptions: None,
        }
    }

    /// Attach the exception router (Slice 7 Phase 2) so a `CLAWBACK_UNDERFLOW`
    /// escalation also opens a durable close-blocking exception row. Additive — the
    /// existing alarm is unchanged.
    #[must_use]
    pub fn with_exceptions(mut self, exceptions: Arc<ExceptionRouter>) -> Self {
        self.exceptions = Some(exceptions);
        self
    }

    /// Attach the credit-note orchestrator (Group G): enables the
    /// `refund-with-credit-note` atomic composite ([`Self::post_refund_with_credit_note`]),
    /// which posts the paired S3 credit note as the SECOND entry inside the refund's
    /// post txn (K-3 — both commit or neither, AR never overstated between them).
    /// Builder form (defaults to `None` at `new`) so the executor's un-gated handler
    /// and the unit tests stay source-compatible; `module` wires it onto the REST
    /// handler.
    #[must_use]
    pub fn with_credit_note_handler(
        mut self,
        credit_note: Arc<super::credit_note_service::CreditNoteHandler>,
    ) -> Self {
        self.credit_note = Some(credit_note);
        self
    }

    /// Bind the secured-audit sink (Slice 6 seam, Group F). The `unknown_final`
    /// disposition appends a `secured_audit_record` through it in the post txn.
    /// Builder form (defaults to [`NoopSecuredAuditSink`] at `new`) so the
    /// Group-B callers + unit tests stay source-compatible; a postgres test
    /// injects a spy sink to assert the append fired.
    #[must_use]
    pub fn with_audit_sink(mut self, audit: Arc<dyn SecuredAuditSink>) -> Self {
        self.audit = audit;
        self
    }

    /// Bind the metrics sink (Group F): `ledger_refund_unknown_final_total` on a
    /// disposition. Builder form (defaults to the no-op at `new`).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn LedgerMetricsPort>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Attach the dual-control engine (Group D): a forward refund whose returned
    /// cash crosses the tenant's D2 threshold is then gated to the preparer→approver
    /// queue ([`DomainError::DualControlRequired`]) rather than posting inline. The
    /// approved replay re-enters through [`Self::post_refund_approved`], which skips
    /// the gate. Builder form (not a `new` arg) so the Group-B `post_refund` callers
    /// and the unit tests stay source-compatible.
    #[must_use]
    pub fn with_approval(mut self, approval: Arc<ApprovalService>) -> Self {
        self.approval = Some(approval);
        self
    }

    /// Post one refund phase (design §4.4 / §4.7). Validates the request shape,
    /// resolves the origin `payment_settlement` (existence + currency), then routes
    /// by phase: the `initiated`/`confirmed` forward post (with the stage-1 cap
    /// reservation) or the `rejected`/`voided` stage-1 reversal (line-negation + cap
    /// release + `REFUND_CLEARING` drain). Idempotent on
    /// `(tenant, REFUND, psp_refund_id:phase)`.
    ///
    /// # Errors
    /// [`DomainError::AmountOutOfRange`] / [`DomainError::InvalidRequest`] (shape);
    /// [`DomainError::RefundOriginNotFound`] (no settled origin payment, 404);
    /// [`DomainError::CurrencyMismatch`] (refund currency ≠ the origin settlement's,
    /// 400); [`DomainError::RefundExceedsSettled`] / [`DomainError::RefundExceedsAllocated`]
    /// (a stage-1 cap CHECK rejected the reservation); [`DomainError::AccountClosed`]
    /// when a required class (`UNALLOCATED` / `AR` / `REFUND_CLEARING` /
    /// `CASH_CLEARING`) is not provisioned; any foundation rejection or
    /// [`DomainError::Internal`] on an infra fault.
    pub async fn post_refund(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: RefundRequest,
    ) -> Result<PostingRef, DomainError> {
        self.post_refund_inner(ctx, scope, req, /* gate */ true)
            .await
    }

    /// The approved-replay entry (Group D): re-drive a held refund WITHOUT the
    /// dual-control gate. Called only by the `ApprovalExecutor` after a second actor
    /// approves the PENDING refund approval — the threshold was already crossed at
    /// gate time, so re-checking it would re-open a second approval (an infinite
    /// loop). Idempotent on the engine's `(tenant, REFUND, psp_refund_id:phase)`
    /// claim: a re-approve replays the post harmlessly (the dedup short-circuits a
    /// committed entry before the sidecar), so execute-then-mark is safe.
    ///
    /// # Errors
    /// As [`Self::post_refund`], minus the dual-control gate (never returns
    /// [`DomainError::DualControlRequired`]).
    pub async fn post_refund_approved(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: RefundRequest,
    ) -> Result<PostingRef, DomainError> {
        self.post_refund_inner(ctx, scope, req, /* gate */ false)
            .await
    }

    /// The REST entry for `POST /refunds` (Group G): record one refund phase,
    /// QUARANTINING a refund-before-payment instead of 404-ing it (design §4.4 /
    /// PRD L668 / Rev2 E-11). Resolves the origin `payment_settlement` out-of-txn
    /// FIRST:
    /// - **origin resolvable** ⇒ post via the gated [`Self::post_refund`] path
    ///   (which itself routes by phase, reserves/releases caps, and gates over D2);
    ///   returns [`RefundOutcome::Posted`].
    /// - **origin NOT resolvable** (no settled receipt for `(tenant, payment_id)`)
    ///   ⇒ DURABLY QUARANTINE the payload on the `REFUND_QUARANTINE` queue +
    ///   out-of-band `RefundQuarantined` alarm, and return
    ///   [`RefundOutcome::Quarantined`] (the REST handler maps it to a 202 +
    ///   `refund-quarantined` body token). NEVER posts. De-quarantine
    ///   ([`Self::drain_quarantine`]) re-validates everything before any post.
    ///
    /// This DIFFERS from [`Self::post_refund`] (used by the approved replay), which
    /// 404s an absent origin (`RefundOriginNotFound`): the REST surface quarantines,
    /// the executor's approved replay does not (its origin existed at gate time).
    ///
    /// # Errors
    /// As [`Self::post_refund`] for the posted path (shape, currency-mismatch, cap,
    /// dual-control 409); [`DomainError::Internal`] on a quarantine-intake infra
    /// fault. A `currency`-mismatch on a resolvable origin is still a 400 (not a
    /// quarantine — the payment exists, the request is malformed).
    pub async fn record_refund(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: RefundRequest,
    ) -> Result<RefundOutcome, DomainError> {
        // Shape gate up-front (a malformed request is a clean 400, never a
        // quarantine). A zero amount is rejected here too (mirrors `post_refund_inner`).
        validate_shape(&req)?;
        if req.amount_minor == 0 {
            return Err(DomainError::InvalidRequest(
                "refund amount_minor must be > 0".to_owned(),
            ));
        }

        // Resolve the origin settlement (out-of-txn, scoped). ABSENT ⇒ quarantine
        // (refund-before-payment), NOT a 404. A foreign-tenant payment reads as
        // absent (the same quarantine, no existence leak).
        let settlement = self
            .payment
            .read_settlement(scope, req.tenant_id, &req.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read origin settlement: {e}")))?;
        let Some(settlement) = settlement else {
            return self.quarantine_refund(ctx, scope, &req).await;
        };
        // The payment EXISTS but the currency disagrees: a malformed request, not a
        // quarantine — surface the 400 (mirrors `post_refund_inner`).
        if settlement.currency != req.currency {
            return Err(DomainError::CurrencyMismatch(format!(
                "refund {} currency {} does not match the origin payment {} settlement currency {}",
                req.refund_id, req.currency, req.payment_id, settlement.currency
            )));
        }

        // Origin resolvable ⇒ the gated post path (it re-resolves the settlement
        // in-txn under the rank-1 lock; the out-of-txn read above is the quarantine
        // pre-flight only). A forward money-OUT post on a payment with an OPEN dispute
        // is HELD inside `post_refund_inner` (Z5-2): the hold intake durably enqueues
        // the payload + signals `RefundDisputeHeld`, which we surface here as a
        // `DisputeHeld` 202 (mirroring the `Quarantined` 202) rather than a raw error.
        let held_at = OffsetDateTime::now_utc();
        match self.post_refund(ctx, scope, req).await {
            Ok(posting) => Ok(RefundOutcome::Posted(posting)),
            Err(DomainError::RefundDisputeHeld(business_id)) => {
                Ok(RefundOutcome::DisputeHeld(DisputeHoldHandle {
                    flow: FLOW_REFUND_DISPUTE_HOLD.to_owned(),
                    business_id,
                    held_at,
                }))
            }
            Err(other) => Err(other),
        }
    }

    /// The `POST /refund-with-credit-note` ATOMIC composite (Group G / Rev2 K-3):
    /// post a S5 refund AND its paired S3 credit note in ONE transaction as TWO
    /// linked entries — both commit or neither, so AR is NEVER overstated between
    /// them. The refund is the OUTER post ([`PostingService::post`]); its composite
    /// sidecar (a) does the normal refund work (caps + `refund` row +
    /// `refund.recorded` event) AND (b) posts the credit-note entry inline via
    /// [`CreditNoteHandler::apply_in_txn`] — both inside the refund's serializable
    /// post txn. The refund's `(tenant, REFUND, psp_refund_id:phase)` engine claim
    /// is the composite's primary idempotency grain (a replay returns before the
    /// sidecar, so neither entry re-posts); the credit note's own
    /// `(tenant, CREDIT_NOTE, credit_note_id)` claim is the secondary guard.
    ///
    /// The refund leg is GATED over D2 like a plain stage-1 (the credit note rides
    /// the same approval — a high-value composite routes to the queue as a unit; the
    /// approved replay re-drives this composite). The refund's origin settlement
    /// MUST resolve (a composite is a real refund of a real payment — it is NOT a
    /// quarantine path; an absent origin 404s `RefundOriginNotFound`).
    ///
    /// # Errors
    /// [`DomainError::Internal`] if the credit-note handler is not wired
    /// (`with_credit_note_handler`); the union of [`Self::post_refund`]'s and
    /// [`CreditNoteHandler::post_credit_note`]'s rejections — any of which rolls the
    /// WHOLE composite back (both entries).
    pub async fn post_refund_with_credit_note(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        refund: RefundRequest,
        credit_note: crate::domain::adjustment::credit_note::CreditNoteRequest,
    ) -> Result<RefundWithCreditNoteOutcome, DomainError> {
        self.post_refund_with_credit_note_inner(
            ctx,
            scope,
            refund,
            credit_note,
            /* gate */ true,
        )
        .await
    }

    /// Approved-replay entry for an over-D2 composite (Z5-1 fix): the executor
    /// re-drives the WHOLE composite (refund + credit note) from a
    /// [`ApprovalIntent::RefundWithCreditNote`] snapshot — NOT a bare refund — so both
    /// entries post atomically exactly as the gated path would (the prior bug gated
    /// the composite as a plain `Refund` intent, dropping the credit note on replay →
    /// AR overstated). Skips the gate (the threshold was crossed at gate time); the
    /// refund + credit-note engine claims make the replay at-most-once.
    ///
    /// # Errors
    /// The union of [`Self::post_refund`]'s and [`CreditNoteHandler::post_credit_note`]'s
    /// rejections — any of which rolls the WHOLE composite back (both entries); and
    /// [`DomainError::Internal`] if the credit-note handler is not wired. Identical to
    /// [`Self::post_refund_with_credit_note`] (the approved replay shares the same
    /// `_inner`, only skipping the gate).
    pub async fn post_refund_with_credit_note_approved(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        refund: RefundRequest,
        credit_note: crate::domain::adjustment::credit_note::CreditNoteRequest,
    ) -> Result<RefundWithCreditNoteOutcome, DomainError> {
        self.post_refund_with_credit_note_inner(
            ctx,
            scope,
            refund,
            credit_note,
            /* gate */ false,
        )
        .await
    }

    async fn post_refund_with_credit_note_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        refund: RefundRequest,
        credit_note: crate::domain::adjustment::credit_note::CreditNoteRequest,
        gate: bool,
    ) -> Result<RefundWithCreditNoteOutcome, DomainError> {
        let credit_handler = self.credit_note.as_ref().ok_or_else(|| {
            DomainError::Internal(
                "refund-with-credit-note composite requires a wired CreditNoteHandler".to_owned(),
            )
        })?;

        // 1. Pure refund shape gate + zero guard (a clean 400 before any read).
        validate_shape(&refund)?;
        if refund.amount_minor == 0 {
            return Err(DomainError::InvalidRequest(
                "refund amount_minor must be > 0".to_owned(),
            ));
        }
        // The composite is a money-out refund; only a forward stage-1 (or a
        // single-step `initiated`) makes sense paired with a fresh credit note. A
        // reversal / disposition composite is out of scope.
        if !matches!(
            refund.phase,
            RefundPhase::Initiated | RefundPhase::Confirmed
        ) {
            return Err(DomainError::InvalidRequest(format!(
                "refund-with-credit-note composite supports only initiated/confirmed refund \
                 phases, got {}",
                refund.phase.as_str()
            )));
        }

        // 2. Resolve the origin settlement (out-of-txn). A composite refunds a REAL
        //    settled receipt — an absent origin is a hard 404 here (NOT a quarantine:
        //    the credit note half has no meaning without the refund's origin).
        let settlement = self
            .payment
            .read_settlement(scope, refund.tenant_id, &refund.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read origin settlement: {e}")))?
            .ok_or_else(|| {
                DomainError::RefundOriginNotFound(format!(
                    "refund {} references payment {} which has no settlement",
                    refund.refund_id, refund.payment_id
                ))
            })?;
        if settlement.currency != refund.currency {
            return Err(DomainError::CurrencyMismatch(format!(
                "refund {} currency {} does not match the origin payment {} settlement currency {}",
                refund.refund_id, refund.currency, refund.payment_id, settlement.currency
            )));
        }

        // 2c. Dispute-hold (Z5-2, composite path). A forward composite refund moves
        //     cash OUT, so — like a plain refund (`post_refund_inner`) — it must NOT
        //     post on a payment with an OPEN dispute (double-payout if the dispute
        //     later resolves LOST). Unlike a plain refund it is NOT enqueued on
        //     `FLOW_REFUND_DISPUTE_HOLD`: that queue replays a PLAIN refund on
        //     WON-resolution, which would DROP the paired credit note (the Z5-1 class).
        //     So the WHOLE composite is REFUSED up front (both-or-neither — neither
        //     entry posts) with a `RefundDisputeHeld` abort (409); the caller resubmits
        //     the composite once the dispute resolves. Checked before the gate so a
        //     held composite never opens a then-blocked approval. `is_dispute_holdable`
        //     is true for the forward initiated/confirmed a composite supports (a
        //     composite is never a claw-back).
        if Self::is_dispute_holdable(&refund)
            && let Some(open) = self
                .dispute
                .read_open_dispute_for_payment(scope, refund.tenant_id, &refund.payment_id)
                .await?
        {
            return Err(DomainError::RefundDisputeHeld(format!(
                "refund-with-credit-note {} held: origin payment {} has an OPEN dispute \
                 {} (cycle {}); resubmit the composite after it resolves",
                refund.refund_id, refund.payment_id, open.dispute_id, open.cycle
            )));
        }

        // 3. Dual-control gate (the composite) — the same D2 gate `post_refund_inner`
        //    runs for a plain stage-1, valued at the LARGER of the two legs so a
        //    high-value credit note cannot ride under the threshold behind a small
        //    refund. The standalone credit-note path gates on the note's own amount, so
        //    the composite MUST consider it too (segregation of duties — else a
        //    >D2 credit note paired with a <D2 refund posts with no approver). Gated on
        //    the forward `initiated`.
        if gate
            && matches!(refund.phase, RefundPhase::Initiated)
            && let Some(approval) = &self.approval
        {
            // Z5-1: gate the composite AS A COMPOSITE — carry BOTH snapshots so the
            // approved replay re-drives the credit note too. (The prior code gated a
            // plain `Refund` intent, so the executor replayed a bare refund and the
            // paired credit note was silently dropped → AR overstated, breaking K-3.)
            let intent = ApprovalIntent::RefundWithCreditNote(
                RefundWithCreditNoteIntent::from_requests(&refund, &credit_note),
            );
            let facts = OperationFacts {
                kind: crate::domain::approval::ApprovalKind::Refund,
                // Value at the larger leg: a refund OR a credit note above D2 must
                // route to dual-control (both legs are same-currency, same payment).
                amount_usd_eq_minor: Some(refund.amount_minor.max(credit_note.amount_minor)),
                effective_at: None,
                has_outstanding_balance: false,
            };
            if let Some(approval_id) = approval
                .gate(ctx, scope, intent, facts, "refund".to_owned())
                .await?
            {
                return Err(DomainError::DualControlRequired(format!(
                    "refund-with-credit-note requires dual-control approval: {approval_id}"
                )));
            }
        }

        // 4. Prepare the credit note OUT-OF-TXN (reads + split + legs + chart/scale +
        //    normal_sides). A split-ambiguous / headroom-basis problem fails HERE,
        //    before the refund entry is posted (no half-posted composite).
        let prepared = credit_handler.prepare(ctx, scope, &credit_note).await?;

        // 5. Build the refund's forward plan + entry, then post it with the COMPOSITE
        //    sidecar that also applies the prepared credit note in the same txn.
        let plan = build_refund_legs(&refund)?;
        let cap_mode = match refund.phase {
            RefundPhase::Initiated => CapMode::Initiate,
            // `confirmed` (the cash was already capped at stage-1) moves no
            // counters; every other phase is guarded out above, so the wildcard
            // shares the `confirmed` body (no counter movement).
            _ => CapMode::None,
        };
        let business_id = refund_business_id(&refund.psp_refund_id, refund.phase.as_str());
        let (entry, lines) = self
            .assemble_post(ctx, scope, &refund, &plan, &business_id, None)
            .await?;

        // The composite outcome is filled in-txn by the sidecar (the credit-note
        // entry id) and read back after the post commits.
        let cn_entry_id: Arc<std::sync::Mutex<Option<CompositeCreditNoteOutcome>>> =
            Arc::new(std::sync::Mutex::new(None));
        let sidecar: Arc<dyn PostSidecar> = Arc::new(RefundWithCreditNoteSidecar {
            refund: RefundPostSidecar {
                cap_mode,
                cap: RefundCap::for_request(&refund),
                refund_row: Self::refund_row(&refund, plan.clearing_state, None),
                payment: self.payment.clone(),
                publisher: Arc::clone(&self.publisher),
                ctx: ctx.clone(),
            },
            credit_note: credit_handler.clone(),
            prepared: Arc::new(prepared),
            ctx: ctx.clone(),
            outcome: Arc::clone(&cn_entry_id),
        });

        let refund_posting = self
            .posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await?;

        // `ledger_refund_total{phase,pattern}` on a fresh composite refund post.
        if !refund_posting.replayed {
            self.metrics
                .refund(refund.phase.as_str(), refund.pattern.as_str());
        }

        // The credit-note entry id the sidecar recorded in-txn. On a refund REPLAY
        // the sidecar never ran, so re-read the prior credit-note entry id by its
        // business id (the composite committed both on the original post).
        let cn = cn_entry_id
            .lock()
            .map_err(|_| DomainError::Internal("composite outcome mutex poisoned".to_owned()))?
            .take();
        let credit_note_entry_id = match cn {
            Some(o) => o.entry_id,
            None => {
                self.credit_note_entry_id_for_replay(scope, &credit_note)
                    .await?
            }
        };
        Ok(RefundWithCreditNoteOutcome {
            refund_entry_id: refund_posting.entry_id,
            credit_note_entry_id,
            replayed: refund_posting.replayed,
        })
    }

    /// On a composite REPLAY (the refund's engine claim short-circuited the post, so
    /// the sidecar never ran), resolve the paired credit note's entry id by its
    /// `(tenant, CREDIT_NOTE, credit_note_id)` business id (the original composite
    /// committed it). Exactly one such entry exists.
    async fn credit_note_entry_id_for_replay(
        &self,
        scope: &AccessScope,
        credit_note: &crate::domain::adjustment::credit_note::CreditNoteRequest,
    ) -> Result<Uuid, DomainError> {
        let mut ids = self
            .journal
            .entry_ids_for_business_id(scope, credit_note.tenant_id, &credit_note.credit_note_id)
            .await
            .map_err(|e| {
                DomainError::Internal(format!("resolve composite credit-note entry: {e}"))
            })?;
        ids.pop().ok_or_else(|| {
            DomainError::Internal(format!(
                "composite replay: no committed credit-note entry for {}",
                credit_note.credit_note_id
            ))
        })
    }

    /// Shared body for [`Self::post_refund`] (gated) + [`Self::post_refund_approved`]
    /// (the approved replay). `gate` ⇒ a forward refund over the D2 threshold routes
    /// to dual-control instead of posting.
    async fn post_refund_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: RefundRequest,
        gate: bool,
    ) -> Result<PostingRef, DomainError> {
        // 1. Pure shape gate — a clean 400 before any read.
        validate_shape(&req)?;

        // A zero-amount refund moves no cash and would fail the engine's empty-entry
        // validation; reject up-front (inherited S1 / AC #4 forbids a zero
        // placeholder entry just as it forbids a zero placeholder line).
        if req.amount_minor == 0 {
            return Err(DomainError::InvalidRequest(
                "refund amount_minor must be > 0".to_owned(),
            ));
        }

        // 2. Resolve the origin settlement (out-of-txn, scoped). A refund unwinds a
        //    settled receipt: the `payment_settlement` row MUST exist for
        //    `(tenant, payment_id)` and its currency MUST match the refund's. Scoped
        //    existence (SQL-level BOLA) — a foreign-tenant payment reads as absent,
        //    the same 404, no existence leak. (The settlement *counters* are the cap
        //    basis; Group C re-reads + guards them in-txn under the rank-1 lock.)
        //    Resolved BEFORE the dual-control gate, so a bad-origin refund 404s
        //    without ever opening an approval.
        let settlement = self
            .payment
            .read_settlement(scope, req.tenant_id, &req.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read origin settlement: {e}")))?
            .ok_or_else(|| {
                DomainError::RefundOriginNotFound(format!(
                    "refund {} references payment {} which has no settlement",
                    req.refund_id, req.payment_id
                ))
            })?;
        if settlement.currency != req.currency {
            return Err(DomainError::CurrencyMismatch(format!(
                "refund {} currency {} does not match the origin payment {} settlement currency {}",
                req.refund_id, req.currency, req.payment_id, settlement.currency
            )));
        }

        // 2a. Dispute-hold gate (Z5-2, design §5 — the missing control). A refund
        //     must NOT move cash on a payment with an OPEN dispute: the disputed
        //     funds are sub judice (held in `DISPUTE_HOLD` for a `CASH_HOLD` dispute,
        //     or reclassed `DISPUTED` for `AR_RECLASS`), so paying the refund out now
        //     would double-spend funds the chargeback may claw back. Held on the CASH
        //     LEG only — a forward money-OUT post (`Initiated` non-clawback stage-1 /
        //     single-step, OR `Confirmed` stage-2 where the cash actually leaves). A
        //     `rejected`/`voided` reversal RELEASES caps (it does not pay the
        //     customer), a claw-back is money-IN (it returns cash), and the
        //     `unknown_final` disposition PARKS the stuck clearing on SUSPENSE (it
        //     never reaches the customer) — none of those move cash OUT to the payer,
        //     so none are dispute-held. Checked BEFORE the dual-control gate so a
        //     held refund never opens a (then-bypassed) approval; the hold drain
        //     re-drives through this gated path once the dispute resolves WON, which
        //     re-runs the (now-passing) check. Independent of `gate` — a hold is a
        //     hard money-movement block, not a governance decision.
        if Self::is_dispute_holdable(&req)
            && let Some(open) = self
                .dispute
                .read_open_dispute_for_payment(scope, req.tenant_id, &req.payment_id)
                .await?
        {
            let token = self.hold_for_dispute(ctx, scope, &req, &open).await?;
            return Err(DomainError::RefundDisputeHeld(token));
        }

        // 2b. Dual-control gate (VHP-1852, Group D / design §1.4 D2 / §4.4). Gated
        //     on the STAGE-1 cash commitment ONLY (`initiated` — where the money-out
        //     cap is reserved and the human decision belongs). `confirmed` is the
        //     mechanical stage-2 drain of an ALREADY-approved disbursement (no fresh
        //     human decision), so it is NOT re-gated — re-gating it would force a
        //     redundant second approval per refund. A `rejected`/`voided` reversal
        //     RELEASES caps + unwinds the stage-1 post: it must never be blocked
        //     behind an approval (that would strand the REFUND_CLEARING balance). A
        //     single-step refund (`two_stage == false`) is one `initiated` entry, so
        //     it is gated here exactly once. The approved replay (`gate == false`)
        //     skips this. Above the tenant's D2 threshold ⇒ a PENDING approval is
        //     created and `DualControlRequired` is returned (the REST handler maps it
        //     to 409); at/under threshold ⇒ inline, unchanged.
        // The `unknown_final` disposition is ALSO gated here (Group F): it is a
        // terminal, ledger-side GOVERNED disposition of a stuck `REFUND_CLEARING`
        // (parked to SUSPENSE; design §4.4 / K-1 — "a dual-control disposition"), so it
        // crosses the same preparer→approver gate as a stage-1 cash commitment. It
        // rides the SAME D2 policy row as a forward refund (gated on the open
        // clearing amount it writes off), reusing Group D's gate verbatim — no
        // bespoke always-gate path. The approved replay (`gate == false`) skips it.
        // Z5-3: a claw-back (refund-of-refund money-IN decrement) is NOT gated at
        // intake. It reduces net money-out (the safe direction), and an out-of-order
        // claw-back that defers is drained by the UN-gated `REFUND_CLAWBACK` sweep — so
        // gating it here would strand a PENDING approval the sweep then bypasses. Gate
        // only a forward money-OUT stage-1 (`Initiated` non-clawback — a first-order
        // refund or an additional-outbound refund-of-refund) and the `unknown_final`
        // governed disposition.
        if gate
            && (matches!(req.phase, RefundPhase::UnknownFinal)
                || (matches!(req.phase, RefundPhase::Initiated) && !req.is_clawback()))
            && let Some(approval) = &self.approval
        {
            let intent = ApprovalIntent::Refund(RefundIntent::from(&req));
            let facts = OperationFacts {
                kind: crate::domain::approval::ApprovalKind::Refund,
                // DC10 / FX: pass the refund's TRANSACTION-currency minor; the
                // dual-control gate translates it to the tenant's FUNCTIONAL
                // (reporting) currency at the current rate before the threshold
                // compare (it reads the operation currency off the intent via
                // `ApprovalIntent::transaction_currency`). Single-currency tenants
                // compare unchanged.
                amount_usd_eq_minor: Some(req.amount_minor),
                effective_at: None,
                has_outstanding_balance: false,
            };
            if let Some(approval_id) = approval
                .gate(ctx, scope, intent, facts, "refund".to_owned())
                .await?
            {
                return Err(DomainError::DualControlRequired(format!(
                    "refund requires dual-control approval: {approval_id}"
                )));
            }
        }

        // 3. Route by phase (+ direction). A refund-of-refund CLAW-BACK `initiated`
        //    takes the defer-aware path (Group E): its money-out DECREMENT may
        //    underflow if the PSP claw-back arrived before/without the matching
        //    outbound refund stage-1, in which case it is DEFERRED, never failed.
        //    Everything else (outbound initiated/confirmed, claw-back confirmed) is
        //    a plain forward post; reject/void is the stage-1 reversal (Group C).
        let result = match req.phase {
            RefundPhase::Initiated if req.is_clawback() => {
                self.post_clawback(ctx, scope, &req).await
            }
            RefundPhase::Initiated | RefundPhase::Confirmed => {
                self.post_forward(ctx, scope, &req).await
            }
            RefundPhase::Rejected | RefundPhase::Voided => {
                self.post_reversal(ctx, scope, &req).await
            }
            // The terminal `unknown_final` disposition (Group F, design §4.4 /
            // K-1): park the open `REFUND_CLEARING` on SUSPENSE +
            // write a secured-audit record. Governed (gated above).
            RefundPhase::UnknownFinal => self.post_unknown_final(ctx, scope, &req).await,
        };

        // `ledger_refund_total{phase,pattern}` (design §9 / Group G): one increment
        // per FRESH refund post — never on a replay (which re-returns the prior
        // handle but applied nothing). The `unknown_final` disposition already bumps
        // its own dedicated `ledger_refund_unknown_final_total` in `post_unknown_final`
        // (in ADDITION to this generic per-phase counter). A claw-back DEFER
        // (`RefundClawbackDeferred`) is not a post and is not counted here.
        if let Ok(posting) = &result
            && !posting.replayed
        {
            self.metrics
                .refund(req.phase.as_str(), req.pattern.as_str());
        }
        result
    }

    /// Does this refund phase move cash OUT to the payer (so an OPEN dispute on the
    /// origin payment must HOLD it, Z5-2 / design §5)? `true` ONLY for a forward
    /// (OUTBOUND) money-OUT cash post:
    /// - a forward stage-1 `Initiated` non-clawback (a first-order refund or an
    ///   additional-outbound refund-of-refund) — covers the single-step shape too
    ///   (single-step is one `Initiated` entry that posts straight to `CASH_CLEARING`);
    /// - a forward stage-2 `Confirmed` (the drain where the cash actually leaves —
    ///   `DR REFUND_CLEARING · CR CASH_CLEARING`).
    ///
    /// `false` (NOT held — these do NOT pay the customer, so an open dispute must not
    /// block them) for:
    /// - `Rejected` / `Voided` — a stage-1 reversal that RELEASES caps + drains
    ///   `REFUND_CLEARING`; blocking it would strand the clearing balance;
    /// - ANY CLAW-BACK phase — money-IN (cash returns to the merchant: a claw-back
    ///   stage-1 `DR REFUND_CLEARING · CR pattern.debit`, stage-2 `DR CASH_CLEARING ·
    ///   CR REFUND_CLEARING`); holding one would deadlock (a dispute can only resolve
    ///   AFTER the claw-back it depends on lands);
    /// - `UnknownFinal` — a governed disposition that parks the stuck clearing on
    ///   SUSPENSE (the cash never reaches the customer).
    ///
    /// CRITICAL SAFETY: the hold is on the OUTBOUND CASH leg only. `is_clawback()`
    /// (direction == `Clawback` AND a `relates_to_refund_id` link) excludes BOTH
    /// claw-back stages — a `Confirmed` claw-back is money-IN, never held.
    fn is_dispute_holdable(req: &RefundRequest) -> bool {
        match req.phase {
            // Forward stage-1 / single-step / stage-2 drain (money OUT) — but NEVER a
            // claw-back, in either stage (money IN). The single direction predicate
            // gates both forward phases symmetrically.
            RefundPhase::Initiated | RefundPhase::Confirmed => !req.is_clawback(),
            // Reversal (release), disposition (park to SUSPENSE): never held.
            RefundPhase::Rejected | RefundPhase::Voided | RefundPhase::UnknownFinal => false,
        }
    }

    /// Durably HOLD a refund whose origin payment has an OPEN dispute (Z5-2, design
    /// §5): claim the `(tenant, REFUND_DISPUTE_HOLD, psp_refund_id:phase)` dedup row
    /// as `QUEUED` and insert the work-state queue row carrying the PII-free
    /// [`DisputeHeldRefundPayload`] (+ the dispute id/cycle the drain re-reads), in
    /// ONE `db.transaction` (mirrors [`Self::quarantine_refund`]). Raises the
    /// out-of-band `RefundQuarantined` alarm at `Warn` (no dispute-specific category
    /// exists in the vendored schema — `RefundQuarantined` is the closest
    /// refund-held-out-of-band signal; the code/detail name the dispute). Returns the
    /// kebab queue token (the REST 202 `refund-dispute-held` handle). NEVER posts —
    /// [`Self::drain_dispute_hold`] is the only path that can later post it, and only
    /// after the dispute resolves WON.
    async fn hold_for_dispute(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
        open: &crate::infra::storage::entity::dispute::Model,
    ) -> Result<String, DomainError> {
        let now = OffsetDateTime::now_utc();
        let business_id = refund_business_id(&req.psp_refund_id, req.phase.as_str());
        let payload = DisputeHeldRefundPayload::from_request(req, &open.dispute_id, open.cycle);
        let payload_json = serde_json::to_value(&payload)
            .map_err(|e| DomainError::Internal(format!("serialize dispute-hold payload: {e}")))?;
        let payload_hash = {
            let canonical = serde_json::to_string(&payload).map_err(|e| {
                DomainError::Internal(format!("canonicalize dispute-hold payload: {e}"))
            })?;
            IdempotencyGate::content_hash(&canonical)
        };

        let tenant = req.tenant_id;
        let gate = IdempotencyGate::new();
        let scope_owned = scope.clone();
        let business_id_owned = business_id.clone();
        // The closure captures by `move`; keep an un-moved copy of the incoming hash
        // for the AC #19 conflict capture after the txn (Z14-1).
        let incoming_hash = payload_hash.clone();
        let outcome = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    // Claim the DISPUTE_HOLD-flow dedup row as QUEUED. A re-hold of the
                    // SAME key is idempotent (Replay); a different payload is a
                    // conflict (mirrors the quarantine intake).
                    let claim = gate
                        .claim_queued(
                            txn,
                            tenant,
                            FLOW_REFUND_DISPUTE_HOLD,
                            &business_id_owned,
                            &payload_hash,
                        )
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                    match claim {
                        ClaimOutcome::Claimed => {
                            PendingQueueRepo::insert_queued(
                                txn,
                                &scope_owned,
                                &NewQueueRow {
                                    tenant_id: tenant,
                                    flow: FLOW_REFUND_DISPUTE_HOLD.to_owned(),
                                    business_id: business_id_owned.clone(),
                                    payload: payload_json,
                                    queued_at: now,
                                    apply_after: None,
                                },
                            )
                            .await
                            .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                            Ok::<DisputeHoldIntake, DbError>(DisputeHoldIntake::Held)
                        }
                        ClaimOutcome::Replay(row) => {
                            if row.payload_hash == payload_hash {
                                Ok(DisputeHoldIntake::AlreadyHeld)
                            } else {
                                Ok(DisputeHoldIntake::Conflict {
                                    stored_hash: row.payload_hash,
                                })
                            }
                        }
                    }
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("refund dispute-hold intake: {e}")))?;

        if let DisputeHoldIntake::Conflict { stored_hash } = &outcome {
            // AC #19 (Z14-1): capture the conflicting reuse on the secured-audit sink
            // (best-effort, own txn) BEFORE returning the hard error.
            self.capture_idempotency_conflict(
                ctx,
                scope,
                req.tenant_id,
                FLOW_REFUND_DISPUTE_HOLD,
                &business_id,
                stored_hash,
                &incoming_hash,
            )
            .await;
            return Err(DomainError::IdempotencyConflict(format!(
                "dispute-held refund {business_id} reused with a different payload"
            )));
        }

        // Raise the alarm out-of-band (Warn — a control signal, nothing posted). Only
        // on a FRESH hold (an idempotent re-hold does not re-raise — mirrors how a
        // replay raises no alarm). Reuses `RefundQuarantined` (closest refund-held
        // category; the code/detail name the dispute hold explicitly).
        if matches!(outcome, DisputeHoldIntake::Held) {
            let alarm = LedgerInvariantAlarm {
                category: AlarmCategory::RefundQuarantined,
                severity: AlarmSeverity::Warn,
                tenant_id: req.tenant_id,
                scope: format!(
                    "tenant:{}/flow:{FLOW_REFUND_DISPUTE_HOLD}/business:{business_id}",
                    req.tenant_id
                ),
                code: "REFUND_DISPUTE_HELD".to_owned(),
                detail: format!(
                    "refund {} (psp_refund_id {}) on payment {} held — payment has an OPEN \
                     dispute {} (cycle {}); the cash leg is held until the dispute resolves",
                    req.refund_id, req.psp_refund_id, req.payment_id, open.dispute_id, open.cycle
                ),
                affected: vec![AffectedItem {
                    id: format!(
                        "payment:{}/psp_refund:{}/dispute:{}",
                        req.payment_id, req.psp_refund_id, open.dispute_id
                    ),
                    currency: req.currency.clone(),
                    expected_minor: req.amount_minor,
                    actual_minor: 0,
                }],
            };
            self.publisher.emit_invariant_alarm(ctx, alarm).await;
        }

        Ok(business_id)
    }

    /// Drain up to `limit` due DISPUTE-HELD refunds for one tenant (Z5-2, design §5):
    /// claim them under `SKIP LOCKED`, then RE-READ the dispute for each in its own
    /// txn. A dispute-held refund is NOT a blind apply — for each row it:
    /// 1. reconstructs the [`RefundRequest`] from the payload;
    /// 2. re-reads the dispute by its `(tenant, dispute_id)`:
    ///    - STILL `OPENED` ⇒ leave `QUEUED` (back off) UNLESS it has aged out past
    ///      [`DISPUTE_HOLD_AGING_SECS`], in which case CANCEL + escalate (exception
    ///      stub + alarm);
    ///    - resolved WON (the payment stands — the refund is owed) ⇒ re-drive through
    ///      the gated [`Self::post_refund`] path (which re-checks the §4.7 caps + the
    ///      THEN-CURRENT D2 threshold; an over-threshold release opens an approval and
    ///      the row stays `QUEUED` — never auto-posts over threshold), flip
    ///      `→APPLIED` on a post;
    ///    - resolved LOST (a chargeback returned the money to the customer) ⇒ CANCEL
    ///      the hold + raise an alarm + escalate to the exception stub. NEVER
    ///      auto-post: a lost chargeback already refunded the customer, so posting the
    ///      refund too would DOUBLE-PAY.
    ///
    /// Public so the periodic sweep can drive it. Per-row faults are isolated.
    ///
    /// # Errors
    /// [`DomainError::Internal`] only if the initial claim txn fails; per-row faults
    /// are isolated inside the pass.
    pub async fn drain_dispute_hold(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        limit: u64,
    ) -> Result<DisputeHoldDrainReport, DomainError> {
        let now = OffsetDateTime::now_utc();
        let pending_queue = self.pending_queue.clone();
        let scope_owned = scope.clone();
        let claimed: Vec<pending_event_queue::Model> = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    pending_queue
                        .claim_due(
                            txn,
                            &scope_owned,
                            tenant,
                            FLOW_REFUND_DISPUTE_HOLD,
                            now,
                            limit,
                        )
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("refund dispute-hold drain claim: {e}")))?;

        let mut report = DisputeHoldDrainReport::default();
        for row in claimed {
            match self.apply_dispute_hold(ctx, scope, &row).await {
                Ok(DisputeHoldApply::Released) => report.released += 1,
                Ok(DisputeHoldApply::AwaitingApproval) => report.awaiting_approval += 1,
                Ok(DisputeHoldApply::StillDisputed) => report.still_disputed += 1,
                Ok(DisputeHoldApply::Cancelled) => report.cancelled += 1,
                Ok(DisputeHoldApply::Escalated) => report.escalated += 1,
                Err(e) => tracing::error!(
                    tenant_id = %tenant, business_id = %row.business_id, error = %e,
                    "bss-ledger: dispute-held refund apply failed (infra); continuing"
                ),
            }
        }
        Ok(report)
    }

    /// Re-read the dispute + (maybe) release / cancel ONE dispute-held refund row.
    /// See [`Self::drain_dispute_hold`] for the five terminal shapes. The flip
    /// `→APPLIED` (release) and `→CANCELLED` (lost / aged-out) is its OWN short txn.
    async fn apply_dispute_hold(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        row: &pending_event_queue::Model,
    ) -> Result<DisputeHoldApply, DomainError> {
        let payload: DisputeHeldRefundPayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| DomainError::Internal(format!("deserialize dispute-held refund: {e}")))?;
        let dispute_id = payload.dispute_id.clone();
        let req = payload.into_request()?;

        // Re-read the dispute by its `(tenant, dispute_id)` current state.
        let dispute = self
            .dispute
            .read_dispute(scope, req.tenant_id, &dispute_id)
            .await?;
        // The held dispute row vanished (a tenant purge / data fix) — treat as no
        // longer disputed and re-drive (the payment now stands by absence). Rare; the
        // post path re-validates everything regardless.
        let last_phase = dispute
            .as_ref()
            .and_then(|d| DisputePhase::parse(&d.last_phase));

        match last_phase {
            // STILL OPEN ⇒ back off (or escalate if aged out). The cash stays held.
            Some(DisputePhase::Opened) => {
                let aged = (OffsetDateTime::now_utc() - row.queued_at)
                    >= Duration::seconds(DISPUTE_HOLD_AGING_SECS);
                if aged {
                    self.escalate_dispute_hold(ctx, scope, &req, &dispute_id, &row.business_id)
                        .await?;
                    Ok(DisputeHoldApply::Escalated)
                } else {
                    Ok(DisputeHoldApply::StillDisputed)
                }
            }
            // LOST ⇒ a chargeback already returned the money to the customer. CANCEL
            // the hold + escalate — NEVER auto-post (posting the refund too would
            // DOUBLE-PAY). The exception stub flags it for an operator (a refund that
            // can no longer post because the dispute clawed the money back).
            Some(DisputePhase::Lost) => {
                self.cancel_dispute_hold_lost(ctx, scope, &req, &dispute_id, &row.business_id)
                    .await?;
                Ok(DisputeHoldApply::Cancelled)
            }
            // PARTIAL is behind a flag and NOT implemented — the chargeback handler
            // rejects the transition, so a held refund can never observe it today. Guard
            // it EXPLICITLY (not folded into WON): a partial clawback returns PART of the
            // payment, so re-driving the FULL held refund would double-pay that part.
            // When split-chargeback lands, escalate for an operator rather than auto-post.
            Some(DisputePhase::Partial) => {
                self.escalate_dispute_hold(ctx, scope, &req, &dispute_id, &row.business_id)
                    .await?;
                Ok(DisputeHoldApply::Escalated)
            }
            // WON (the payment stands — the refund is genuinely owed) OR the dispute
            // row is gone / a non-terminal-but-unexpected phase ⇒ re-drive through the
            // gated posted path. This re-checks the §4.7 caps in-txn + the
            // THEN-CURRENT D2 threshold AND re-runs the dispute-hold gate (now
            // passing — no OPEN dispute). An over-threshold release opens an approval
            // (DualControlRequired): the row stays QUEUED until the approved replay
            // posts — it NEVER auto-posts over threshold.
            Some(DisputePhase::Won) | None => {
                match self.post_refund(ctx, scope, req).await {
                    Ok(_) => {
                        self.mark_dispute_hold_applied(scope, row).await?;
                        Ok(DisputeHoldApply::Released)
                    }
                    Err(DomainError::DualControlRequired(_)) => {
                        Ok(DisputeHoldApply::AwaitingApproval)
                    }
                    // A still-OPEN dispute raced back in between the re-read and the
                    // post (re-held under the same key — idempotent): leave QUEUED.
                    Err(DomainError::RefundDisputeHeld(_)) => Ok(DisputeHoldApply::StillDisputed),
                    Err(other) => Err(other),
                }
            }
        }
    }

    /// Flip one dispute-hold row `→APPLIED` (the dispute resolved WON + the refund
    /// posted) in its own short txn (the post already committed). Mirrors
    /// [`Self::mark_quarantine_applied`].
    async fn mark_dispute_hold_applied(
        &self,
        scope: &AccessScope,
        row: &pending_event_queue::Model,
    ) -> Result<(), DomainError> {
        let scope_owned = scope.clone();
        let tenant = row.tenant_id;
        let business_id_owned = row.business_id.clone();
        self.db
            .transaction(move |txn| {
                Box::pin(async move {
                    PendingQueueRepo::mark_applied(
                        txn,
                        &scope_owned,
                        tenant,
                        FLOW_REFUND_DISPUTE_HOLD,
                        &business_id_owned,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("mark dispute-hold applied: {e}")))
    }

    /// A dispute-held refund whose dispute resolved LOST: flip the row `→CANCELLED`
    /// (terminal — the refund can NEVER post; the chargeback already returned the
    /// money) + escalate (exception stub + a `RefundQuarantined` Critical alarm). The
    /// CANCELLED flip is its own short txn. NEVER auto-posts (double-pay guard).
    async fn cancel_dispute_hold_lost(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
        dispute_id: &str,
        business_id: &str,
    ) -> Result<(), DomainError> {
        // exception stub (full exception_queue is Slice 7)
        tracing::error!(
            tenant_id = %req.tenant_id,
            payment_id = %req.payment_id,
            psp_refund_id = %req.psp_refund_id,
            dispute_id = %dispute_id,
            amount_minor = req.amount_minor,
            "bss-ledger: dispute-held refund's dispute resolved LOST (chargeback returned the \
             money) — cancelling the hold, NOT posting (double-pay guard); escalating \
             (full exception_queue is Slice 7)"
        );
        self.cancel_dispute_hold_row(scope, req.tenant_id, business_id)
            .await?;
        // Critical — a refund that can no longer be paid because the dispute clawed
        // the money back; Finance must reconcile (the outbound the PSP may have
        // already actioned vs the chargeback). Reuses `RefundQuarantined` (closest
        // refund-held category; code/detail name the lost-dispute cancel).
        let alarm = LedgerInvariantAlarm {
            category: AlarmCategory::RefundQuarantined,
            severity: AlarmSeverity::Critical,
            tenant_id: req.tenant_id,
            scope: format!(
                "tenant:{}/flow:{FLOW_REFUND_DISPUTE_HOLD}/business:{business_id}",
                req.tenant_id
            ),
            code: "REFUND_DISPUTE_LOST".to_owned(),
            detail: format!(
                "dispute-held refund {} (psp_refund_id {}) on payment {} cancelled — dispute {} \
                 resolved LOST (the chargeback already returned the money; posting the refund too \
                 would double-pay)",
                req.refund_id, req.psp_refund_id, req.payment_id, dispute_id
            ),
            affected: vec![AffectedItem {
                id: format!(
                    "payment:{}/psp_refund:{}/dispute:{}",
                    req.payment_id, req.psp_refund_id, dispute_id
                ),
                currency: req.currency.clone(),
                expected_minor: req.amount_minor,
                actual_minor: 0,
            }],
        };
        self.publisher.emit_invariant_alarm(ctx, alarm).await;
        Ok(())
    }

    /// A dispute-held refund whose dispute NEVER resolved past the aging horizon
    /// ([`DISPUTE_HOLD_AGING_SECS`]): flip the row `→CANCELLED` + escalate (exception
    /// stub + a `RefundQuarantined` Critical alarm — the dispute is stuck). Mirrors
    /// [`Self::escalate_quarantine`]. NEVER auto-posts (the dispute is still OPEN).
    async fn escalate_dispute_hold(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
        dispute_id: &str,
        business_id: &str,
    ) -> Result<(), DomainError> {
        // exception stub (full exception_queue is Slice 7)
        tracing::error!(
            tenant_id = %req.tenant_id,
            payment_id = %req.payment_id,
            psp_refund_id = %req.psp_refund_id,
            dispute_id = %dispute_id,
            amount_minor = req.amount_minor,
            "bss-ledger: dispute-held refund's dispute never resolved past the aging horizon \
             — cancelling the hold + escalating (REFUND_DISPUTE_HELD; full exception_queue is \
             Slice 7)"
        );
        self.cancel_dispute_hold_row(scope, req.tenant_id, business_id)
            .await?;
        let alarm = LedgerInvariantAlarm {
            category: AlarmCategory::RefundQuarantined,
            severity: AlarmSeverity::Critical,
            tenant_id: req.tenant_id,
            scope: format!(
                "tenant:{}/flow:{FLOW_REFUND_DISPUTE_HOLD}/business:{business_id}",
                req.tenant_id
            ),
            code: "REFUND_DISPUTE_HELD".to_owned(),
            detail: format!(
                "dispute-held refund {} (psp_refund_id {}) on payment {} never released — \
                 dispute {} stayed OPEN past the aging horizon",
                req.refund_id, req.psp_refund_id, req.payment_id, dispute_id
            ),
            affected: vec![AffectedItem {
                id: format!(
                    "payment:{}/psp_refund:{}/dispute:{}",
                    req.payment_id, req.psp_refund_id, dispute_id
                ),
                currency: req.currency.clone(),
                expected_minor: req.amount_minor,
                actual_minor: 0,
            }],
        };
        self.publisher.emit_invariant_alarm(ctx, alarm).await;
        Ok(())
    }

    /// Capture an idempotency-conflict (AC #19, Z14-1) on the wired
    /// [`SecuredAuditSink`], BEFORE returning [`DomainError::IdempotencyConflict`].
    /// A conflict is a same-business-key reuse with a DIFFERENT payload (a possible
    /// replay-attack / client bug), so the secured-audit trail must record WHO
    /// attempted it against WHICH stored record — PII-free: tenant, the business id,
    /// the stored-vs-incoming payload hashes, the actor, and the flow. BEST-EFFORT on
    /// its OWN short txn (the conflict already rolled the intake back): a capture
    /// failure is logged + SWALLOWED so it never masks the hard `IdempotencyConflict`
    /// the caller must still see (mirrors the `unknown_final` / write-off capture
    /// pattern — the audit append rides the disposition there, but a CONFLICT has no
    /// post to ride, so the capture opens its own txn and tolerates failure).
    #[allow(clippy::too_many_arguments)] // a PII-free forensic capture; grouping the ids/hashes into a struct adds churn
    async fn capture_idempotency_conflict(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
        stored_hash: &str,
        incoming_hash: &str,
    ) {
        // PII-free: ids + enum/flow literals + hashes + actor only (no names / free
        // text / payload bodies). The hashes let an auditor prove the two payloads
        // differed without storing either.
        let before_after = serde_json::json!({
            "event": "IDEMPOTENCY_CONFLICT",
            "flow": flow,
            "business_id": business_id,
            "stored_payload_hash": stored_hash,
            "incoming_payload_hash": incoming_hash,
        });
        let actor_ref = Some(ctx.subject_id().to_string());
        let scope_owned = scope.clone();
        let audit = Arc::clone(&self.audit);
        let result = self
            .db
            .transaction(move |txn| {
                let before_after = before_after.clone();
                let actor_ref = actor_ref.clone();
                Box::pin(async move {
                    audit
                        .append(
                            txn,
                            &scope_owned,
                            tenant,
                            AuditEventType::ManualAdjustment,
                            actor_ref.as_deref(),
                            Some(REASON_IDEMPOTENCY_CONFLICT),
                            &before_after,
                            // No posted entry to correlate — a conflict has no books
                            // effect (the intake rolled back).
                            None,
                            None,
                        )
                        .await
                        .map(|_id| ())
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await;
        if let Err(e) = result {
            // Swallow — the conflict capture is best-effort; never mask the hard
            // `IdempotencyConflict` the caller must still observe.
            tracing::error!(
                tenant_id = %tenant,
                flow = %flow,
                business_id = %business_id,
                error = %e,
                "bss-ledger: secured-audit capture of idempotency conflict failed (swallowed; \
                 the IdempotencyConflict is still returned)"
            );
        }
    }

    /// Flip one dispute-hold row `→CANCELLED` (terminal — never re-claimed) in its
    /// own short txn. Shared by the LOST-cancel + the aged-out escalate. Mirrors
    /// [`Self::escalate_quarantine`]'s cancel.
    async fn cancel_dispute_hold_row(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        business_id: &str,
    ) -> Result<(), DomainError> {
        let scope_owned = scope.clone();
        let business_id_owned = business_id.to_owned();
        self.db
            .transaction(move |txn| {
                Box::pin(async move {
                    PendingQueueRepo::mark_cancelled(
                        txn,
                        &scope_owned,
                        tenant,
                        FLOW_REFUND_DISPUTE_HOLD,
                        &business_id_owned,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("dispute-hold cancel: {e}")))
    }

    /// The terminal `unknown_final` disposition (Group F, design §4.4 / Rev2 /
    /// K-1): the PSP can produce NO final state for a two-stage refund's stage-1,
    /// so its `REFUND_CLEARING` is stuck open. This is NOT a PSP event — it is a
    /// ledger-side, dual-control GOVERNED disposition (gated in `post_refund_inner`
    /// above) that:
    ///
    /// 1. Posts a balanced park-clearing entry that DRAINS the open clearing to
    ///    zero against the SUSPENSE holding account: `DR REFUND_CLEARING (open
    ///    amount) · CR SUSPENSE` ([`UNKNOWN_FINAL_PARK_CLASS`]). The DR cancels the
    ///    stage-1 `CR REFUND_CLEARING`, so the guarded `REFUND_CLEARING` balance
    ///    returns to zero; the amount holds on SUSPENSE (outcome unknown) until a
    ///    terminal disposition resolves it (Slice 7) — NOT a premature loss/gain.
    /// 2. In the SAME post txn writes one `secured_audit_record` via the
    ///    [`SecuredAuditSink`] ([`AuditEventType::ManualAdjustment`], reason
    ///    `REFUND_UNKNOWN_FINAL`, the acting subject, a PII-clean before/after
    ///    payload) — atomic with the park entry (a sink failure rolls the
    ///    disposition back; the no-op sink never fails).
    /// 3. Stamps the `refund` row `clearing_state = SETTLED` (the live
    ///    `REFUND_CLEARING` is drained — parked to SUSPENSE) on its own
    ///    `(tenant, psp_refund_id:unknown_final)` phase grain.
    ///
    /// Idempotent on the engine's `(tenant, REFUND, psp_refund_id:unknown_final)`
    /// claim (a replay returns before the sidecar — the audit append + park entry
    /// are at-most-once). The open clearing amount is the refund's `amount_minor`
    /// (the stuck stage-1 amount the disposition parks); a zero amount is
    /// rejected up-front by `post_refund_inner`.
    async fn post_unknown_final(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
    ) -> Result<PostingRef, DomainError> {
        // Z5-4: read the LIVE stage-1 state instead of assuming a hardcoded
        // PENDING / request amount. The disposition writes off the STAGE-1
        // (`initiated`) row's open `REFUND_CLEARING`, so resolve that row on its
        // `(tenant, psp_refund_id, initiated)` grain and use its REAL `clearing_state`
        // + `amount_minor`:
        //   - stage-1 `PENDING` (genuinely stuck — stage-2 never confirmed, no
        //     reversal landed) ⇒ write off the stage-1 row's `amount_minor` (the real
        //     open clearing), NOT the disposition request's amount;
        //   - stage-1 already `SETTLED` (stage-2 drained the cash out) / `REVERSED`
        //     (a reject/void already drained the clearing) ⇒ the clearing is NOT open;
        //     a write-off DR would drive `REFUND_CLEARING` NEGATIVE (an over-DR), so
        //     REJECT the disposition as a no-op (`InvalidRequest`) rather than corrupt
        //     the clearing;
        //   - no stage-1 row (e.g. a single-step refund never opened a clearing, or
        //     the stage-1 was never recorded) ⇒ nothing to dispose; REJECT.
        let stage1 = self
            .adjustment
            .read_refund_by_psp_phase(
                scope,
                req.tenant_id,
                &req.psp_refund_id,
                RefundPhase::Initiated.as_str(),
            )
            .await
            .map_err(|e| DomainError::Internal(format!("read stage-1 refund row: {e}")))?;
        let Some(stage1) = stage1 else {
            return Err(DomainError::InvalidRequest(format!(
                "unknown_final disposition for refund {} (psp_refund_id {}) has no recorded \
                 stage-1 initiated refund — nothing to write off",
                req.refund_id, req.psp_refund_id
            )));
        };
        // Only a genuinely-stuck PENDING stage-1 has open clearing to write off.
        if stage1.clearing_state != CLEARING_STATE_PENDING {
            return Err(DomainError::InvalidRequest(format!(
                "unknown_final disposition for refund {} (psp_refund_id {}) is a no-op: the \
                 stage-1 refund is already {} (not PENDING) — its REFUND_CLEARING is not open, so \
                 a write-off would over-debit it",
                req.refund_id, req.psp_refund_id, stage1.clearing_state
            )));
        }
        // The REAL open clearing amount is the stage-1 row's amount (what stage-1
        // CR'd into REFUND_CLEARING and never drained), not the disposition request's
        // amount_minor.
        let open_minor = stage1.amount_minor;

        // The park-clearing plan: DR REFUND_CLEARING (drain the open balance) · CR
        // SUSPENSE (park the amount pending reconciliation). Balanced (one DR == one
        // CR), and the DR on the GUARDED REFUND_CLEARING returns its balance toward
        // zero — the mirror of the stage-1 `CR REFUND_CLEARING`. Both legs are
        // stream-less (matches the never-stream refund classes). Sized at the REAL
        // open clearing amount (Z5-4). NOT a loss/gain — `unknown_final` means the
        // outcome is unknown, so the amount holds on SUSPENSE until a terminal
        // disposition resolves it (Slice 7).
        let plan = RefundLegPlan {
            legs: vec![
                PlannedLeg {
                    account_class: AccountClass::RefundClearing,
                    side: Side::Debit,
                    amount_minor: open_minor,
                    revenue_stream: None,
                },
                PlannedLeg {
                    account_class: UNKNOWN_FINAL_PARK_CLASS,
                    side: Side::Credit,
                    amount_minor: open_minor,
                    revenue_stream: None,
                },
            ],
            // The REFUND_CLEARING is drained off the live account (parked to
            // SUSPENSE) — SETTLED on the `refund` row, not a fresh PENDING. The
            // terminal loss/release attribution is a later governed step (Slice 7).
            clearing_state: CLEARING_STATE_SETTLED,
        };

        let business_id = refund_business_id(&req.psp_refund_id, req.phase.as_str());
        let (entry, lines) = self
            .assemble_post(ctx, scope, req, &plan, &business_id, None)
            .await?;

        // The sidecar persists the `refund` row (clearing_state = SETTLED) AND
        // appends the secured-audit record in the post txn — no cap movement (the
        // disposition does not touch the per-payment money-out counters: it neither
        // refunds nor reverses, it writes the stuck clearing off; the stage-1 that
        // opened the clearing already moved the cap, which stays as the net
        // money-out of record).
        let sidecar: Arc<dyn PostSidecar> = Arc::new(UnknownFinalSidecar {
            refund_row: Self::refund_row(req, CLEARING_STATE_SETTLED, None),
            audit: Arc::clone(&self.audit),
            // The acting subject (the approver/operator) — the audit `actor_ref`.
            // `subject_id` is always present on an authenticated context.
            actor_ref: Some(ctx.subject_id().to_string()),
            // The audit `before` image carries the REAL stage-1 state (Z5-4): its
            // live `clearing_state` + the live open clearing amount, read above — NOT
            // a hardcoded `PENDING` / the request's amount.
            before_after: unknown_final_audit_payload(req, &stage1.clearing_state, open_minor),
            tenant: req.tenant_id,
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
        });

        let posting = self
            .posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await?;

        // Count the disposition only on a FRESH post (a replay re-returns the same
        // handle but applied nothing — the sidecar never ran). `ledger_refund_
        // unknown_final_total` (design §9 / K-1).
        if !posting.replayed {
            self.metrics.refund_unknown_final();
        }
        Ok(posting)
    }

    /// Refund-of-refund CLAW-BACK stage-1 (`initiated`) path (Group E, design §4.4 /
    /// Rev3 / S3-F1). A claw-back DECREMENTS the origin payment's money-out counters
    /// (so the total money-out cap reflects the NET refunded). The decrement is
    /// guarded against UNDERFLOW under the rank-1 `payment_settlement` lock inside
    /// the post sidecar: if the PSP claw-back arrived BEFORE / without the matching
    /// outbound refund stage-1 (or claws back MORE than was refunded), the decrement
    /// would drive `refunded_minor` below zero. The design forbids both APPLYING
    /// that decrement AND hard-failing on the `refunded_minor >= 0` CHECK — instead
    /// it DEFERS: the sidecar returns [`DomainError::RefundClawbackDeferred`] which
    /// rolls the whole post back (nothing applied), and this method then durably
    /// ENQUEUES the claw-back on the deferred-apply queue (status QUEUED) for a later
    /// retry by [`Self::drain_clawbacks`] (when the matching outbound lands and the
    /// decrement stops underflowing). The deferred signal is surfaced as
    /// `RefundClawbackDeferred` (NOT a generic error — the future REST surface maps
    /// it to a 202-like accepted-but-queued; the kebab token is the queue handle).
    ///
    /// An IN-ORDER claw-back (the outbound already landed, the decrement fits) posts
    /// inline exactly like [`Self::post_forward`] and returns the posting handle.
    async fn post_clawback(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
    ) -> Result<PostingRef, DomainError> {
        // 0. Replay short-circuit (BEFORE the post), mirroring
        //    `ChargebackService::replay_short_circuit`. The engine dedup row for
        //    `(REFUND, psp_refund_id:initiated)` may already be `QUEUED` (a prior
        //    defer of THIS claw-back) or `POSTED` (already applied — inline or
        //    drained). A `Fresh` post on a `QUEUED` dedup row would surface an infra
        //    fault (the row carries no result id yet), so intercept it here: a
        //    `QUEUED` row re-signals `RefundClawbackDeferred` (still queued — the
        //    drain owns applying it); a `POSTED` row is an idempotent replay. Racy by
        //    nature (the authoritative dedup is the engine's in-txn claim); a
        //    `CLAIMED` / absent row falls through to the post.
        let business_id = refund_business_id(&req.psp_refund_id, RefundPhase::Initiated.as_str());
        if let Some(outcome) = self
            .clawback_replay_short_circuit(scope, req, &business_id)
            .await?
        {
            return outcome;
        }

        match self.post_forward(ctx, scope, req).await {
            Ok(posting) => Ok(posting),
            // The sidecar's locked underflow pre-check rolled the post back: DEFER —
            // durably enqueue for retry (never hard-fail). The enqueue is idempotent
            // on the `(tenant, REFUND, psp_refund_id:initiated)` engine dedup grain.
            Err(DomainError::RefundClawbackDeferred(_)) => {
                let token = self.enqueue_clawback(ctx, scope, req).await?;
                Err(DomainError::RefundClawbackDeferred(token))
            }
            Err(other) => Err(other),
        }
    }

    /// Replay short-circuit for a claw-back (the §4.4 counterpart to
    /// `ChargebackService::replay_short_circuit`): read the engine
    /// `(tenant, REFUND, psp_refund_id:initiated)` dedup status ONCE and, when the
    /// claw-back was already deferred (`QUEUED`) or applied (`POSTED`), return the
    /// matching outcome WITHOUT re-posting. `None` (absent / `CLAIMED` in-flight)
    /// falls through to the post. Out-of-txn (racy by nature).
    async fn clawback_replay_short_circuit(
        &self,
        scope: &AccessScope,
        req: &RefundRequest,
        business_id: &str,
    ) -> Result<Option<Result<PostingRef, DomainError>>, DomainError> {
        let dedup = self
            .payment
            .lookup_dedup_status(scope, req.tenant_id, SourceDocType::Refund, business_id)
            .await
            .map_err(|e| DomainError::Internal(format!("claw-back dedup lookup: {e}")))?;
        let Some((status, result_entry_id, _hash)) = dedup else {
            return Ok(None);
        };
        if status == STATUS_QUEUED {
            // Already deferred: re-signal `RefundClawbackDeferred` (the drain owns it).
            return Ok(Some(Err(DomainError::RefundClawbackDeferred(
                business_id.to_owned(),
            ))));
        }
        if status == STATUS_POSTED {
            let entry_id = result_entry_id.ok_or_else(|| {
                DomainError::Internal(format!(
                    "claw-back dedup POSTED but no result_entry_id for \
                     ({}, {FLOW_REFUND_ENGINE}, {business_id})",
                    req.tenant_id
                ))
            })?;
            return Ok(Some(Ok(PostingRef {
                entry_id,
                created_seq: 0,
                replayed: true,
            })));
        }
        // CLAIMED (in-flight) / other: fall through to the post.
        Ok(None)
    }

    /// Durably ENQUEUE a deferred claw-back on the deferred-apply queue (Group E,
    /// design §4.4): claim the `(tenant, REFUND_CLAWBACK, psp_refund_id:initiated)`
    /// dedup row as `QUEUED` and insert the work-state queue row carrying the
    /// PII-free [`QueuedClawbackPayload`], in ONE `db.transaction` (mirrors
    /// [`crate::infra::payment::chargeback::ChargebackService::enqueue_phase`]). The
    /// dedup hash is request-based (`content_hash` over the payload) — a deferred
    /// claw-back never inlines under the same key, so request-based is the stable
    /// choice and `claim_queued`'s `Replay` makes the intake idempotent. Returns the
    /// kebab queue token (the REST 202 handle later). A FLOW distinct from the
    /// engine `REFUND` source-doc, so the chargeback / allocation sweeps never pick
    /// it up.
    async fn enqueue_clawback(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
    ) -> Result<String, DomainError> {
        let now = OffsetDateTime::now_utc();
        let business_id = clawback_business_id(&req.psp_refund_id);
        let payload = QueuedClawbackPayload::from_request(req);
        let payload_json = serde_json::to_value(&payload)
            .map_err(|e| DomainError::Internal(format!("serialize claw-back payload: {e}")))?;
        let payload_hash = clawback_request_hash(&payload)?;

        let tenant = req.tenant_id;
        let gate = IdempotencyGate::new();
        let scope_owned = scope.clone();
        let business_id_owned = business_id.clone();
        // Keep an un-moved copy of the incoming hash for the AC #19 conflict capture
        // after the txn (Z14-1).
        let incoming_hash = payload_hash.clone();
        let outcome = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    // Claim the ENGINE dedup row (flow = REFUND, matching the entry's
                    // source-doc) as QUEUED — so `post_queued_apply` later reads +
                    // finalizes THIS row.
                    let claim = gate
                        .claim_queued(
                            txn,
                            tenant,
                            FLOW_REFUND_ENGINE,
                            &business_id_owned,
                            &payload_hash,
                        )
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                    match claim {
                        ClaimOutcome::Claimed => {
                            // The WORK-STATE queue row under the distinct
                            // REFUND_CLAWBACK flow (so the claw-back drain claims only
                            // these rows).
                            PendingQueueRepo::insert_queued(
                                txn,
                                &scope_owned,
                                &NewQueueRow {
                                    tenant_id: tenant,
                                    flow: FLOW_REFUND_CLAWBACK.to_owned(),
                                    business_id: business_id_owned.clone(),
                                    payload: payload_json,
                                    queued_at: now,
                                    // Immediately eligible — the drain re-tries the
                                    // decrement and only NOW-checks the aging horizon.
                                    apply_after: None,
                                },
                            )
                            .await
                            .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                            Ok::<ClawbackIntake, DbError>(ClawbackIntake::Enqueued)
                        }
                        // Same key already queued (idempotent re-defer) OR a
                        // different payload (conflict). A POSTED race (the matching
                        // outbound landed AND the drain applied this claw-back between
                        // the post rollback and this claim) is transient — surface it
                        // as an error so the caller retries cleanly.
                        ClaimOutcome::Replay(row) => {
                            if row.payload_hash != payload_hash {
                                Ok(ClawbackIntake::Conflict {
                                    stored_hash: row.payload_hash,
                                })
                            } else if row.status == STATUS_QUEUED {
                                Ok(ClawbackIntake::AlreadyQueued)
                            } else {
                                Err(DbError::Sea(DbErr::Custom(format!(
                                    "claw-back intake: unexpected dedup status {:?} for \
                                     ({tenant}, {FLOW_REFUND_ENGINE}, {business_id_owned})",
                                    row.status
                                ))))
                            }
                        }
                    }
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("claw-back intake: {e}")))?;

        match outcome {
            ClawbackIntake::Enqueued | ClawbackIntake::AlreadyQueued => Ok(business_id),
            ClawbackIntake::Conflict { stored_hash } => {
                // AC #19 (Z14-1): capture the conflicting reuse on the secured-audit
                // sink (best-effort, own txn) BEFORE returning the hard error.
                self.capture_idempotency_conflict(
                    ctx,
                    scope,
                    tenant,
                    FLOW_REFUND_CLAWBACK,
                    &business_id,
                    &stored_hash,
                    &incoming_hash,
                )
                .await;
                Err(DomainError::IdempotencyConflict(format!(
                    "claw-back {business_id} reused with a different payload"
                )))
            }
        }
    }

    /// Drain up to `limit` due queued claw-backs for one tenant (Group E): claim them
    /// under `SKIP LOCKED` in a short claim txn, then RE-TRY EACH in its own txn (the
    /// "apply is a second txn" shape). On retry the claw-back is re-driven through the
    /// SAME [`Self::post_forward`] path:
    /// - the decrement now FITS (the matching outbound refund stage-1 has landed) ⇒
    ///   the post commits + the queue row flips `→APPLIED`;
    /// - it STILL underflows AND the row is YOUNGER than the aging horizon ⇒ leave it
    ///   `QUEUED`, back off (a later drain retries);
    /// - it STILL underflows AND the row is OLDER than the aging horizon
    ///   ([`CLAWBACK_AGING_SECS`]) ⇒ it never reconciled: flip the row `→CANCELLED`,
    ///   ESCALATE to the exception stub, and raise the `ClawbackUnderflow` alarm +
    ///   Finance alert (design §4.4 — never hard-fail, escalate).
    ///
    /// Public so the periodic sweep ([`crate::infra::jobs::queue_applier`]) and a
    /// drain-on-outbound hook can drive it. Per-row faults are isolated.
    ///
    /// # Errors
    /// [`DomainError::Internal`] only if the initial claim txn fails; per-row faults
    /// are isolated inside the pass.
    pub async fn drain_clawbacks(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        limit: u64,
    ) -> Result<ClawbackDrainReport, DomainError> {
        let now = OffsetDateTime::now_utc();
        let pending_queue = self.pending_queue.clone();
        let scope_owned = scope.clone();
        let claimed: Vec<pending_event_queue::Model> = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    pending_queue
                        .claim_due(txn, &scope_owned, tenant, FLOW_REFUND_CLAWBACK, now, limit)
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("claw-back drain claim: {e}")))?;

        let mut report = ClawbackDrainReport::default();
        for row in claimed {
            match self.apply_queued_clawback(ctx, scope, &row).await {
                Ok(ClawbackApply::Applied) => report.applied += 1,
                Ok(ClawbackApply::StillDeferred) => {
                    report.still_deferred += 1;
                    if let Err(e) = self
                        .bump_clawback_attempts(
                            scope,
                            tenant,
                            &row.business_id,
                            i64::from(row.attempts),
                        )
                        .await
                    {
                        tracing::error!(
                            tenant_id = %tenant, business_id = %row.business_id, error = %e,
                            "bss-ledger: claw-back drain failed to bump attempts"
                        );
                    }
                }
                Ok(ClawbackApply::Escalated) => report.escalated += 1,
                Err(e) => tracing::error!(
                    tenant_id = %tenant, business_id = %row.business_id, error = %e,
                    "bss-ledger: queued claw-back apply failed (infra); continuing"
                ),
            }
        }
        Ok(report)
    }

    /// Apply ONE queued claw-back row (the drain): deserialize the payload,
    /// reconstruct the [`RefundRequest`], and RE-DRIVE it through the inline
    /// claw-back post path. Three terminal shapes:
    /// - the decrement FITS now ⇒ [`ClawbackApply::Applied`] (post committed; the
    ///   composite sidecar flipped the row `→APPLIED` in the same txn);
    /// - it STILL underflows but the row is within the aging horizon ⇒
    ///   [`ClawbackApply::StillDeferred`] (the caller backs off + leaves it QUEUED);
    /// - it STILL underflows AND the row aged out ⇒ [`ClawbackApply::Escalated`]
    ///   (flip `→CANCELLED` + exception stub + alarm).
    ///
    /// `pub(crate)` so the sweep can drive a single row.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on an infra fault (bad payload / engine Internal).
    pub(crate) async fn apply_queued_clawback(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        row: &pending_event_queue::Model,
    ) -> Result<ClawbackApply, DomainError> {
        let payload: QueuedClawbackPayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| DomainError::Internal(format!("deserialize queued claw-back: {e}")))?;
        let req = payload.into_request()?;

        // Re-drive the claw-back inline, but via the queued-apply engine path with a
        // COMPOSITE sidecar that also flips the queue row `→APPLIED`. The underflow
        // pre-check runs again under the rank-1 lock against THEN-CURRENT counters.
        match self
            .post_clawback_queued_apply(ctx, scope, &req, &row.business_id)
            .await
        {
            Ok(_) => Ok(ClawbackApply::Applied),
            // Still underflows: defer again UNLESS the row has aged out, in which
            // case it never reconciled → escalate (design §4.4).
            Err(DomainError::RefundClawbackDeferred(_)) => {
                let aged = (OffsetDateTime::now_utc() - row.queued_at)
                    >= Duration::seconds(CLAWBACK_AGING_SECS);
                if aged {
                    self.escalate_clawback(ctx, scope, &req, &row.business_id)
                        .await?;
                    Ok(ClawbackApply::Escalated)
                } else {
                    Ok(ClawbackApply::StillDeferred)
                }
            }
            Err(other) => Err(other),
        }
    }

    /// The deferred-apply twin of [`Self::post_clawback`]/[`Self::post_forward`]:
    /// re-drive the claw-back stage-1 via [`PostingService::post_queued_apply`] with
    /// a COMPOSITE sidecar that wraps the [`RefundPostSidecar`] AND flips the queue
    /// row `→APPLIED` in the same txn. The dedup row claimed `QUEUED` at the defer
    /// intake is read (not re-claimed) and finalized `QUEUED → POSTED`. Surfaces
    /// [`DomainError::RefundClawbackDeferred`] (rolled back) when the decrement still
    /// underflows.
    async fn post_clawback_queued_apply(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
        queue_business_id: &str,
    ) -> Result<PostingRef, DomainError> {
        let plan = build_refund_legs(req)?;
        let entry_business_id = refund_business_id(&req.psp_refund_id, req.phase.as_str());
        let (entry, lines) = self
            .assemble_post(ctx, scope, req, &plan, &entry_business_id, None)
            .await?;
        let sidecar: Arc<dyn PostSidecar> = Arc::new(QueuedClawbackApplySidecar {
            inner: RefundPostSidecar {
                cap_mode: CapMode::Clawback,
                cap: RefundCap::for_request(req),
                refund_row: Self::refund_row(req, plan.clearing_state, None),
                payment: self.payment.clone(),
                publisher: Arc::clone(&self.publisher),
                ctx: ctx.clone(),
            },
            flow: FLOW_REFUND_CLAWBACK.to_owned(),
            business_id: queue_business_id.to_owned(),
            tenant: req.tenant_id,
        });
        self.posting
            .post_queued_apply(ctx, scope, entry, lines, Some(sidecar))
            .await
    }

    /// A claw-back that never reconciled past the aging horizon: flip the queue row
    /// `→CANCELLED` (terminal) and ESCALATE (design §4.4 — never hard-fail). The
    /// exception-queue is Slice 7 (VHP-1859), so this is a minimal exception STUB: a
    /// logged escalation + the out-of-band `ClawbackUnderflow` alarm (which mirrors
    /// into `ledger_invariant_alarm_total` and pages Finance). The CANCELLED flip is
    /// its own short txn (the apply already rolled back).
    async fn escalate_clawback(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
        business_id: &str,
    ) -> Result<(), DomainError> {
        // exception stub (full exception_queue is Slice 7)
        tracing::error!(
            tenant_id = %req.tenant_id,
            payment_id = %req.payment_id,
            psp_refund_id = %req.psp_refund_id,
            relates_to_refund_id = ?req.relates_to_refund_id,
            amount_minor = req.amount_minor,
            "bss-ledger: claw-back never reconciled past the aging horizon — escalating \
             (CLAWBACK_UNDERFLOW; full exception_queue is Slice 7)"
        );

        // Flip the row `→CANCELLED` in its own txn (terminal — never re-claimed).
        let scope_owned = scope.clone();
        let tenant = req.tenant_id;
        let business_id_owned = business_id.to_owned();
        self.db
            .transaction(move |txn| {
                Box::pin(async move {
                    PendingQueueRepo::mark_cancelled(
                        txn,
                        &scope_owned,
                        tenant,
                        FLOW_REFUND_CLAWBACK,
                        &business_id_owned,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("claw-back escalate cancel: {e}")))?;

        // Raise the CLAWBACK_UNDERFLOW alarm out-of-band + Finance alert (Critical —
        // a books-affecting money-out that could not be netted; mirrors the
        // `CreditNoteSplitBlocked` explicit raise). Fire-and-forget.
        let alarm = LedgerInvariantAlarm {
            category: AlarmCategory::ClawbackUnderflow,
            severity: AlarmSeverity::Critical,
            tenant_id: req.tenant_id,
            scope: format!(
                "tenant:{}/flow:{FLOW_REFUND_CLAWBACK}/business:{business_id}",
                req.tenant_id
            ),
            code: "CLAWBACK_UNDERFLOW".to_owned(),
            detail: format!(
                "claw-back of {} minor on payment {} (psp_refund_id {}) never found a matching \
                 outbound refund to net against within the aging horizon",
                req.amount_minor, req.payment_id, req.psp_refund_id
            ),
            affected: vec![AffectedItem {
                id: format!(
                    "payment:{}/psp_refund:{}/relates_to:{}",
                    req.payment_id,
                    req.psp_refund_id,
                    req.relates_to_refund_id.as_deref().unwrap_or("")
                ),
                currency: req.currency.clone(),
                expected_minor: req.amount_minor,
                actual_minor: 0,
            }],
        };
        self.publisher.emit_invariant_alarm(ctx, alarm).await;

        // Slice 7 Phase 2: ADDITIVELY open a durable close-blocking exception row
        // beside the alarm above (the escalation already logged + alarmed; this only
        // makes the never-reconciled clawback block the next close until resolved).
        if let Some(ex) = &self.exceptions {
            ex.route(
                req.tenant_id,
                crate::domain::exception::ExceptionType::ReconMismatch,
                &req.psp_refund_id,
                Some(serde_json::json!({
                    "psp_refund_id": req.psp_refund_id,
                    "payment_id": req.payment_id,
                })),
            )
            .await;
        }
        Ok(())
    }

    /// Bump one claw-back queue row's `attempts` + defer its next eligibility by an
    /// exponential backoff, in its own short txn (mirrors the chargeback/allocate
    /// `bump_attempts_own_txn`). The `apply_after` defer keeps a still-underflowing
    /// row from hot-looping the drain before it either reconciles or ages out.
    async fn bump_clawback_attempts(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        business_id: &str,
        prior_attempts: i64,
    ) -> Result<(), DomainError> {
        let scope_owned = scope.clone();
        let business_id = business_id.to_owned();
        let defer_until = OffsetDateTime::now_utc() + clawback_backoff(prior_attempts + 1);
        self.db
            .transaction(move |txn| {
                Box::pin(async move {
                    PendingQueueRepo::bump_attempts_and_defer(
                        txn,
                        &scope_owned,
                        tenant,
                        FLOW_REFUND_CLAWBACK,
                        &business_id,
                        defer_until,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("claw-back drain bump attempts: {e}")))
    }

    /// Durably QUARANTINE a refund-before-payment (Group G, design §4.4 / PRD L668
    /// / Rev2 E-11): claim the `(tenant, REFUND_QUARANTINE, psp_refund_id:phase)`
    /// dedup row as `QUEUED` and insert the work-state queue row carrying the
    /// PII-free [`QuarantinedRefundPayload`] (+ the PSP correlation id), in ONE
    /// `db.transaction` (mirrors [`Self::enqueue_clawback`]). Raises the out-of-band
    /// `RefundQuarantined` alarm. Returns [`RefundOutcome::Quarantined`] (the REST
    /// 202 handle). NEVER posts — de-quarantine ([`Self::drain_quarantine`]) is the
    /// only path that can later post it, and only after re-validating everything.
    async fn quarantine_refund(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
    ) -> Result<RefundOutcome, DomainError> {
        let now = OffsetDateTime::now_utc();
        let business_id = refund_business_id(&req.psp_refund_id, req.phase.as_str());
        let payload = QuarantinedRefundPayload::from_request(req);
        let payload_json = serde_json::to_value(&payload)
            .map_err(|e| DomainError::Internal(format!("serialize quarantine payload: {e}")))?;
        let payload_hash = {
            let canonical = serde_json::to_string(&payload).map_err(|e| {
                DomainError::Internal(format!("canonicalize quarantine payload: {e}"))
            })?;
            IdempotencyGate::content_hash(&canonical)
        };

        let tenant = req.tenant_id;
        let gate = IdempotencyGate::new();
        let scope_owned = scope.clone();
        let business_id_owned = business_id.clone();
        // Keep an un-moved copy of the incoming hash for the AC #19 conflict capture
        // after the txn (Z14-1).
        let incoming_hash = payload_hash.clone();
        let outcome = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    // Claim the QUARANTINE-flow dedup row as QUEUED. A re-quarantine
                    // of the SAME key is idempotent (Replay); a different payload is
                    // a conflict.
                    let claim = gate
                        .claim_queued(
                            txn,
                            tenant,
                            FLOW_REFUND_QUARANTINE,
                            &business_id_owned,
                            &payload_hash,
                        )
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                    match claim {
                        ClaimOutcome::Claimed => {
                            PendingQueueRepo::insert_queued(
                                txn,
                                &scope_owned,
                                &NewQueueRow {
                                    tenant_id: tenant,
                                    flow: FLOW_REFUND_QUARANTINE.to_owned(),
                                    business_id: business_id_owned.clone(),
                                    payload: payload_json,
                                    queued_at: now,
                                    apply_after: None,
                                },
                            )
                            .await
                            .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                            Ok::<QuarantineIntake, DbError>(QuarantineIntake::Quarantined)
                        }
                        ClaimOutcome::Replay(row) => {
                            if row.payload_hash == payload_hash {
                                Ok(QuarantineIntake::AlreadyQuarantined)
                            } else {
                                Ok(QuarantineIntake::Conflict {
                                    stored_hash: row.payload_hash,
                                })
                            }
                        }
                    }
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("refund quarantine intake: {e}")))?;

        if let QuarantineIntake::Conflict { stored_hash } = &outcome {
            // AC #19 (Z14-1): capture the conflicting reuse on the secured-audit sink
            // (best-effort, own txn) BEFORE returning the hard error.
            self.capture_idempotency_conflict(
                ctx,
                scope,
                req.tenant_id,
                FLOW_REFUND_QUARANTINE,
                &business_id,
                stored_hash,
                &incoming_hash,
            )
            .await;
            return Err(DomainError::IdempotencyConflict(format!(
                "quarantined refund {business_id} reused with a different payload"
            )));
        }

        // Raise the RefundQuarantined alarm out-of-band (Warn — a PSP/ingestion
        // ordering signal, nothing posted). Fire-and-forget. Only on a FRESH
        // quarantine (an idempotent re-quarantine does not re-raise — mirrors how a
        // replay raises no alarm).
        if matches!(outcome, QuarantineIntake::Quarantined) {
            let alarm = LedgerInvariantAlarm {
                category: AlarmCategory::RefundQuarantined,
                severity: AlarmSeverity::Warn,
                tenant_id: req.tenant_id,
                scope: format!(
                    "tenant:{}/flow:{FLOW_REFUND_QUARANTINE}/business:{business_id}",
                    req.tenant_id
                ),
                code: "REFUND_QUARANTINED".to_owned(),
                detail: format!(
                    "refund {} (psp_refund_id {}) references payment {} with no resolvable \
                     settlement — quarantined, never posted",
                    req.refund_id, req.psp_refund_id, req.payment_id
                ),
                affected: vec![AffectedItem {
                    id: format!(
                        "payment:{}/psp_refund:{}",
                        req.payment_id, req.psp_refund_id
                    ),
                    currency: req.currency.clone(),
                    expected_minor: req.amount_minor,
                    actual_minor: 0,
                }],
            };
            self.publisher.emit_invariant_alarm(ctx, alarm).await;
        }

        Ok(RefundOutcome::Quarantined(QuarantineHandle {
            flow: FLOW_REFUND_QUARANTINE.to_owned(),
            business_id,
            quarantined_at: now,
        }))
    }

    /// Drain up to `limit` due QUARANTINED refunds for one tenant (Group G
    /// de-quarantine, design §4.4): claim them under `SKIP LOCKED`, then RE-VALIDATE
    /// each in its own txn. De-quarantine is NOT a blind apply — for each row it:
    /// 1. reconstructs the [`RefundRequest`] from the payload;
    /// 2. re-resolves the origin `payment_settlement` — STILL ABSENT ⇒ leave it
    ///    `QUEUED` (back off) UNLESS it has aged out past [`QUARANTINE_AGING_SECS`],
    ///    in which case CANCEL + escalate (exception stub + alarm);
    /// 3. origin NOW resolvable ⇒ re-drive through [`Self::record_refund`]'s posted
    ///    path, which RE-CHECKS all §4.7 caps (in-txn under the rank-1 lock) + the
    ///    THEN-CURRENT D2 threshold (the gated [`Self::post_refund`] path). An
    ///    over-threshold de-quarantine returns `DualControlRequired` ⇒ the row stays
    ///    `QUEUED` (an approval is now open; it NEVER auto-posts) and is marked
    ///    `APPLIED` once the approved replay posts. A posted refund flips the row
    ///    `→APPLIED`.
    ///
    /// Public so the periodic sweep can drive it. Per-row faults are isolated.
    ///
    /// # Errors
    /// [`DomainError::Internal`] only if the initial claim txn fails; per-row faults
    /// are isolated inside the pass.
    pub async fn drain_quarantine(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        limit: u64,
    ) -> Result<QuarantineDrainReport, DomainError> {
        let now = OffsetDateTime::now_utc();
        let pending_queue = self.pending_queue.clone();
        let scope_owned = scope.clone();
        let claimed: Vec<pending_event_queue::Model> = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    pending_queue
                        .claim_due(
                            txn,
                            &scope_owned,
                            tenant,
                            FLOW_REFUND_QUARANTINE,
                            now,
                            limit,
                        )
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("refund quarantine drain claim: {e}")))?;

        let mut report = QuarantineDrainReport::default();
        for row in claimed {
            match self.apply_quarantined(ctx, scope, &row).await {
                Ok(QuarantineApply::Released) => report.released += 1,
                Ok(QuarantineApply::AwaitingApproval) => report.awaiting_approval += 1,
                Ok(QuarantineApply::StillMissing) => report.still_missing += 1,
                Ok(QuarantineApply::Escalated) => report.escalated += 1,
                Err(e) => tracing::error!(
                    tenant_id = %tenant, business_id = %row.business_id, error = %e,
                    "bss-ledger: quarantined refund apply failed (infra); continuing"
                ),
            }
        }
        Ok(report)
    }

    /// Re-validate + (maybe) release ONE quarantined refund row (de-quarantine).
    /// See [`Self::drain_quarantine`] for the four terminal shapes. The flip
    /// `→APPLIED` is its OWN short txn once the refund posts (or an approval opens).
    async fn apply_quarantined(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        row: &pending_event_queue::Model,
    ) -> Result<QuarantineApply, DomainError> {
        let payload: QuarantinedRefundPayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| DomainError::Internal(format!("deserialize quarantined refund: {e}")))?;
        let req = payload.into_request()?;

        // Re-resolve the origin settlement. STILL ABSENT ⇒ leave queued (back off) or
        // escalate if aged out. (Re-uses the same scoped read the intake used.)
        let settlement = self
            .payment
            .read_settlement(scope, req.tenant_id, &req.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("re-read origin settlement: {e}")))?;
        if settlement.is_none() {
            let aged = (OffsetDateTime::now_utc() - row.queued_at)
                >= Duration::seconds(QUARANTINE_AGING_SECS);
            if aged {
                self.escalate_quarantine(ctx, scope, &req, &row.business_id)
                    .await?;
                return Ok(QuarantineApply::Escalated);
            }
            return Ok(QuarantineApply::StillMissing);
        }

        // Origin NOW resolvable ⇒ re-drive through the gated posted path. This
        // RE-CHECKS the §4.7 caps in-txn + the THEN-CURRENT D2 threshold AND the
        // DISPUTE-HOLD gate (Z5-2): `post_refund` → `post_refund_inner` reads the open
        // dispute on the origin payment BEFORE posting and HOLDS the cash leg if one
        // exists (the 4th dispute-hold checkpoint — a de-quarantined refund whose
        // payment is now disputed must NOT post). An over-threshold de-quarantine
        // opens an approval (DualControlRequired): the row stays QUEUED until the
        // approved replay posts — it NEVER auto-posts over threshold. (`record_refund`
        // would re-quarantine on a now-impossible absent origin; we call `post_refund`
        // directly since we just confirmed presence.)
        match self.post_refund(ctx, scope, req).await {
            Ok(_) => {
                // Posted (in-txn caps + threshold + no open dispute) ⇒ flip the
                // quarantine row →APPLIED (its own short txn — the post already
                // committed).
                self.mark_quarantine_applied(scope, row).await?;
                Ok(QuarantineApply::Released)
            }
            // Over the THEN-CURRENT D2 ⇒ an approval is now open. The row stays QUEUED
            // (it never auto-posts); the approved replay will post it and a later
            // drain marks it APPLIED once the origin+threshold clear. Counted, not
            // re-tried this pass.
            Err(DomainError::DualControlRequired(_)) => Ok(QuarantineApply::AwaitingApproval),
            // The origin landed BUT the payment now has an OPEN dispute ⇒ the refund
            // was durably HELD on the `REFUND_DISPUTE_HOLD` queue (Z5-2). The
            // QUARANTINE concern is resolved (origin present) and the refund is now
            // tracked on the dispute-hold queue, so flip the quarantine row →APPLIED
            // (it never re-quarantines). The dispute-hold drain owns it from here
            // (re-drives on WON / cancels on LOST). Counted as Released (the
            // quarantine terminated cleanly — it neither stays queued nor escalates).
            Err(DomainError::RefundDisputeHeld(_)) => {
                self.mark_quarantine_applied(scope, row).await?;
                Ok(QuarantineApply::Released)
            }
            Err(other) => Err(other),
        }
    }

    /// Flip one quarantine row `→APPLIED` (de-quarantine succeeded) in its own short
    /// txn (the post already committed). Mirrors [`Self::escalate_clawback`]'s cancel.
    async fn mark_quarantine_applied(
        &self,
        scope: &AccessScope,
        row: &pending_event_queue::Model,
    ) -> Result<(), DomainError> {
        let scope_owned = scope.clone();
        let tenant = row.tenant_id;
        let business_id_owned = row.business_id.clone();
        self.db
            .transaction(move |txn| {
                Box::pin(async move {
                    PendingQueueRepo::mark_applied(
                        txn,
                        &scope_owned,
                        tenant,
                        FLOW_REFUND_QUARANTINE,
                        &business_id_owned,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("mark quarantine applied: {e}")))
    }

    /// A quarantined refund whose origin payment never landed past the aging horizon
    /// ([`QUARANTINE_AGING_SECS`]): flip the row `→CANCELLED` + escalate (exception
    /// stub + `RefundQuarantined` Critical alarm — the origin never arrived). Mirrors
    /// [`Self::escalate_clawback`].
    async fn escalate_quarantine(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
        business_id: &str,
    ) -> Result<(), DomainError> {
        // exception stub (full exception_queue is Slice 7)
        tracing::error!(
            tenant_id = %req.tenant_id,
            payment_id = %req.payment_id,
            psp_refund_id = %req.psp_refund_id,
            amount_minor = req.amount_minor,
            "bss-ledger: quarantined refund's origin payment never landed past the aging horizon \
             — escalating (REFUND_QUARANTINED; full exception_queue is Slice 7)"
        );
        let scope_owned = scope.clone();
        let tenant = req.tenant_id;
        let business_id_owned = business_id.to_owned();
        self.db
            .transaction(move |txn| {
                Box::pin(async move {
                    PendingQueueRepo::mark_cancelled(
                        txn,
                        &scope_owned,
                        tenant,
                        FLOW_REFUND_QUARANTINE,
                        &business_id_owned,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("quarantine escalate cancel: {e}")))?;

        let alarm = LedgerInvariantAlarm {
            category: AlarmCategory::RefundQuarantined,
            severity: AlarmSeverity::Critical,
            tenant_id: req.tenant_id,
            scope: format!(
                "tenant:{}/flow:{FLOW_REFUND_QUARANTINE}/business:{business_id}",
                req.tenant_id
            ),
            code: "REFUND_QUARANTINED".to_owned(),
            detail: format!(
                "quarantined refund {} (psp_refund_id {}) on payment {} never found its origin \
                 settlement within the aging horizon",
                req.refund_id, req.psp_refund_id, req.payment_id
            ),
            affected: vec![AffectedItem {
                id: format!(
                    "payment:{}/psp_refund:{}",
                    req.payment_id, req.psp_refund_id
                ),
                currency: req.currency.clone(),
                expected_minor: req.amount_minor,
                actual_minor: 0,
            }],
        };
        self.publisher.emit_invariant_alarm(ctx, alarm).await;
        Ok(())
    }

    /// Forward post for the `initiated` / `confirmed` phases (design §4.4),
    /// OUTBOUND or refund-of-refund CLAW-BACK. Builds the routed two-leg plan
    /// (`build_refund_legs` flips the sides for a claw-back), assembles the engine
    /// entry, and posts it with the [`RefundPostSidecar`]. The cap mode follows the
    /// `(phase, direction)`:
    /// - OUTBOUND `initiated` (a plain stage-1 OR an additional-outbound
    ///   refund-of-refund) → [`CapMode::Initiate`] (INCREMENT the money-out cap);
    /// - CLAW-BACK `initiated` → [`CapMode::Clawback`] (DECREMENT, under the
    ///   sidecar's rank-1-lock underflow pre-check — Group E);
    /// - `confirmed` (either direction) → [`CapMode::None`] (the counters moved at
    ///   stage-1; stage-2 only drains `REFUND_CLEARING`).
    ///
    /// On a CLAW-BACK whose decrement would underflow (out-of-order PSP claw-back),
    /// the sidecar returns [`DomainError::RefundClawbackDeferred`] which rolls the
    /// whole post back; [`Self::post_clawback`] catches it and DEFERS the request to
    /// the queue (never hard-fail). An OUTBOUND post never defers here.
    async fn post_forward(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
    ) -> Result<PostingRef, DomainError> {
        let plan = build_refund_legs(req)?;
        let cap_mode = match (req.phase, req.is_clawback()) {
            // Outbound stage-1 (single-step `initiated` included): reserve the cap.
            // A single-step refund's one `initiated` entry already moves the cash,
            // so it must reserve the cap exactly like a two-stage stage-1. An
            // additional-outbound refund-of-refund also lands here (cash out again
            // under the SAME money-out cap).
            (RefundPhase::Initiated, false) => CapMode::Initiate,
            // Claw-back stage-1: DECREMENT the origin money-out counters (net to the
            // refunded amount) — the sidecar pre-checks the underflow under the
            // rank-1 lock and defers (not hard-fail) if it would go negative.
            (RefundPhase::Initiated, true) => CapMode::Clawback,
            // Stage-2 (either direction) drains REFUND_CLEARING; the cash/claw-back
            // was capped at stage-1.
            (RefundPhase::Confirmed, _) => CapMode::None,
            // `post_forward` is only called for Initiated/Confirmed (routed in
            // `post_refund`); defend the invariant.
            (other, _) => {
                return Err(DomainError::Internal(format!(
                    "post_forward reached with non-forward phase {}",
                    other.as_str()
                )));
            }
        };

        let business_id = refund_business_id(&req.psp_refund_id, req.phase.as_str());
        let (entry, lines) = self
            .assemble_post(ctx, scope, req, &plan, &business_id, None)
            .await?;

        let sidecar: Arc<dyn PostSidecar> = Arc::new(RefundPostSidecar {
            cap_mode,
            cap: RefundCap::for_request(req),
            refund_row: Self::refund_row(req, plan.clearing_state, None),
            payment: self.payment.clone(),
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
        });

        self.posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await
    }

    /// Stage-1 reversal for the `rejected` / `voided` phases (design §4.4 / §4.7,
    /// Group C): the PSP failed an initiated refund, so we (a) STRICTLY line-negate
    /// the stage-1 entry (`reverses_entry_id = <stage-1 entry id>`, legs = the
    /// stage-1 legs with sides inverted — `DR REFUND_CLEARING` against the restored
    /// `UNALLOCATED`(A) / `AR`(B)), and (b) in the SAME txn RELEASE the caps the
    /// stage-1 reserved + drain `REFUND_CLEARING` to zero. Idempotent on
    /// `(tenant, REFUND, psp_refund_id:rejected/voided)`; the `refund` row records
    /// the reversal with `clearing_state = REVERSED` + `reverses_entry_id`.
    async fn post_reversal(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
    ) -> Result<PostingRef, DomainError> {
        // Resolve the stage-1 `initiated` entry this reversal negates. The
        // line-negation requires the original entry id (the `reverses_entry_id`
        // foreign link); a reject/void with no prior stage-1 is an upstream contract
        // violation (nothing to reverse). The stage-1 entry's business id is
        // `(psp_refund_id:initiated)` — resolve it scoped (SQL-level BOLA).
        let stage1_business_id =
            refund_business_id(&req.psp_refund_id, RefundPhase::Initiated.as_str());
        let stage1_entry_id = self
            .resolve_stage1_entry(scope, req, &stage1_business_id)
            .await?;

        // Build the stage-1 plan, then invert each leg's side: the reversal posts
        // exactly the stage-1 legs with DR<->CR swapped. Stage-1 was
        // `DR pattern.debit · CR REFUND_CLEARING`, so the reversal is
        // `DR REFUND_CLEARING · CR pattern.debit` — restoring the drawn-down
        // UNALLOCATED(A) / AR(B) and draining REFUND_CLEARING back to zero.
        let stage1_plan = build_refund_legs(&RefundRequest {
            phase: RefundPhase::Initiated,
            ..req.clone()
        })?;
        let reversal_plan = invert_plan(&stage1_plan);

        let business_id = refund_business_id(&req.psp_refund_id, req.phase.as_str());
        let (entry, lines) = self
            .assemble_post(
                ctx,
                scope,
                req,
                &reversal_plan,
                &business_id,
                Some(stage1_entry_id),
            )
            .await?;

        let sidecar: Arc<dyn PostSidecar> = Arc::new(RefundPostSidecar {
            // Release the caps the stage-1 reserved (decrement by the stage-1 amount,
            // which equals this reversal's amount). The reversal `refund` row stamps
            // `clearing_state = REVERSED` + `reverses_entry_id`.
            cap_mode: CapMode::Release,
            cap: RefundCap::for_request(req),
            refund_row: Self::refund_row(req, CLEARING_STATE_REVERSED, Some(stage1_entry_id)),
            payment: self.payment.clone(),
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
        });

        self.posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await
    }

    /// Resolve the stage-1 `initiated` journal entry id a stage-1 reversal negates,
    /// by its `(tenant, psp_refund_id:initiated)` business id (scoped). Exactly one
    /// `initiated` entry exists per `(psp_refund_id)` (the idempotency claim
    /// guarantees at-most-once); a reject/void with no prior stage-1 is rejected as
    /// `InvalidRequest` (nothing to reverse — an upstream contract violation).
    async fn resolve_stage1_entry(
        &self,
        scope: &AccessScope,
        req: &RefundRequest,
        stage1_business_id: &str,
    ) -> Result<Uuid, DomainError> {
        let mut ids = self
            .journal
            .entry_ids_for_business_id(scope, req.tenant_id, stage1_business_id)
            .await
            .map_err(|e| DomainError::Internal(format!("resolve stage-1 entry: {e}")))?;
        match ids.len() {
            0 => Err(DomainError::InvalidRequest(format!(
                "refund {} phase {} has no stage-1 initiated entry to reverse (psp_refund_id {})",
                req.refund_id,
                req.phase.as_str(),
                req.psp_refund_id
            ))),
            1 => Ok(ids.remove(0)),
            // The at-most-once idempotency claim makes >1 a hard invariant breach
            // (two `initiated` posts for one psp_refund_id). Surface as Internal
            // rather than silently negating an arbitrary one.
            n => Err(DomainError::Internal(format!(
                "refund {} found {n} stage-1 initiated entries for psp_refund_id {} (expected 1)",
                req.refund_id, req.psp_refund_id
            ))),
        }
    }

    /// Resolve each planned leg's chart `account_id` + currency scale and assemble
    /// the engine [`NewEntry`] + [`NewLine`] vector. Every refund class is
    /// stream-less, so each resolves on `stream = None`. The header is
    /// `source_doc_type = Refund` + `source_business_id = "{psp_refund_id}:{phase}"`
    /// (the engine's idempotency key — one claim per PSP-refund phase).
    /// `reverses_entry_id` is `Some(<stage-1 entry id>)` for a stage-1 reversal (the
    /// strict line-negation link) and `None` for a forward post.
    async fn assemble_post(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &RefundRequest,
        plan: &RefundLegPlan,
        business_id: &str,
        reverses_entry_id: Option<Uuid>,
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

        // Slice 5 (F2): functional carry-forward on a cross-currency refund. A
        // refund unwinds a position through REFUND_CLEARING entirely in the
        // transaction currency (no in-ledger conversion), so the functional cost
        // basis carries forward and each stage's functional column nets to zero —
        // NO realized FX line (true EUR→USD realization is Slice-7 reconciliation /
        // ERP-export, owner decision 2026-06-28). The stamp keeps the relieved
        // grain's functional column in lockstep with balance_minor; a single-
        // currency refund is a no-op (functional NULL).
        self.stamp_fx_carry_forward(scope, req, &chart, &mut lines)
            .await?;

        // A line-negation reversal carries the reversed period alongside the
        // reversed entry (the journal's `(reverses_entry_id IS NULL) =
        // (reverses_period_id IS NULL)` CHECK). v1 posts the reversal into the
        // current period; the reversed period is the same period_id (the stage-1
        // entry is recent — a cross-period reverse is not a Group-C concern).
        let reverses_period_id = reverses_entry_id.map(|_| period_id.clone());

        let entry = NewEntry {
            entry_id: Uuid::now_v7(),
            tenant_id: req.tenant_id,
            // v1: one legal entity per tenant — derived server-side.
            legal_entity_id: req.tenant_id,
            period_id,
            entry_currency: req.currency.clone(),
            source_doc_type: SourceDocType::Refund,
            // The engine's `(tenant, REFUND, psp_refund_id:phase)` idempotency key —
            // one claim per PSP-refund phase (design §7).
            source_business_id: business_id.to_owned(),
            reverses_entry_id,
            reverses_period_id,
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: eff_date,
            origin: ORIGIN_SYSTEM.to_owned(),
            posted_by_actor_id: ctx.subject_id(),
            correlation_id: Uuid::now_v7(),
            rounding_evidence: serde_json::Value::Null,
            // Slice 5 (F2): a refund locks NO new rate — it unwinds at the carried
            // rate (functional carry-forward in `stamp_fx_carry_forward`), so the
            // entry never carries a rate_snapshot_ref.
            rate_snapshot_ref: None,
        };
        Ok((entry, lines))
    }

    /// Stamp functional **carry-forward** onto a cross-currency refund entry (Slice
    /// 5 F2, design §3.5 — refund close, owner decision 2026-06-28: a refund unwinds
    /// a position at its carried cost basis; the true EUR→USD realization is
    /// Slice-7 reconciliation, not the refund). A refund moves money through the
    /// two-stage `REFUND_CLEARING` entirely in the transaction currency, so the
    /// functional cost basis carries forward and each stage's two equal-amount legs
    /// are stamped at the SAME functional → the entry's functional column nets to
    /// zero (NO `FX_GAIN_LOSS` line) while the relieved grain's functional column
    /// decrements in lockstep with `balance_minor`.
    ///
    /// The carried-WAC **anchor** grain (the value each stage moves) by stage:
    /// - `Confirmed` (stage-2) / `Rejected` / `Voided` (stage-1 reversal) → the
    ///   `REFUND_CLEARING` leg (its functional was set at stage-1; the stage drains
    ///   it, restoring the cash / the position at exactly the carried basis);
    /// - `Initiated` Pattern A (stage-1 / single-step) → the `UNALLOCATED` pool it
    ///   relieves (`read_unallocated_carried`);
    /// - `Initiated` Pattern B (stage-1 / single-step) → `CASH_CLEARING` (the AR leg
    ///   re-OPENS the receivable rather than relieving it, so the cash being
    ///   returned is the carried value the re-opened AR inherits).
    ///
    /// No-op (leaves functional NULL — byte-green single-currency path) when the
    /// anchor grain carries no functional balance (design decision 8), when the
    /// relieved amount exceeds the grain's balance (an over-relief the projector
    /// rejects with `NegativeBalance` — let that cleaner rejection surface), or for a
    /// refund-of-refund claw-back (a rare Group-E edge whose restored position needs
    /// the prior refund's rate, not readily available — its cross-currency
    /// functional is reconciled in Slice 7; tracked, not silently wrong) and the
    /// `UnknownFinal` disposition (Group F, out of scope).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a carried-read fault or a [`carried_relief`]
    /// misuse (a malformed grain value — an internal invariant breach).
    async fn stamp_fx_carry_forward(
        &self,
        scope: &AccessScope,
        req: &RefundRequest,
        chart: &ChartIndex,
        lines: &mut [NewLine],
    ) -> Result<(), DomainError> {
        // Read the carried (transaction, functional) value of this stage's anchor
        // grain — the position whose cost basis the stage carries. For a claw-back
        // this read is used ONLY to detect cross-currency below (a claw-back restores
        // a position at the PRIOR refund's rate, which this WAC carry-forward cannot
        // source — see the cross-currency reject after the detect).
        let (balance_minor, functional_balance_minor, functional_currency) =
            match (req.phase, req.pattern) {
                // Stage-2 / reversal: relieve REFUND_CLEARING (set at stage-1). The
                // REFUND_CLEARING leg is present in every such entry; defensively no-op
                // if absent.
                (RefundPhase::Confirmed | RefundPhase::Rejected | RefundPhase::Voided, _) => {
                    let Some(account_id) = lines
                        .iter()
                        .find(|l| l.account_class == AccountClass::RefundClearing)
                        .map(|l| l.account_id)
                    else {
                        return Ok(());
                    };
                    let c = self
                        .payment
                        .read_account_carried(scope, req.tenant_id, account_id, &req.currency)
                        .await
                        .map_err(|e| {
                            DomainError::Internal(format!("read refund-clearing carried: {e}"))
                        })?;
                    (
                        c.balance_minor,
                        c.functional_balance_minor,
                        c.functional_currency,
                    )
                }
                // Stage-1 / single-step Pattern A: relieve the UNALLOCATED pool.
                (RefundPhase::Initiated, RefundPattern::AUnallocated) => {
                    let u = self
                        .payment
                        .read_unallocated_carried(
                            scope,
                            req.tenant_id,
                            req.payer_tenant_id,
                            &req.currency,
                        )
                        .await
                        .map_err(|e| {
                            DomainError::Internal(format!("read unallocated carried: {e}"))
                        })?;
                    (
                        u.balance_minor,
                        u.functional_balance_minor,
                        u.functional_currency,
                    )
                }
                // Stage-1 / single-step Pattern B: the AR leg re-OPENS the receivable, so
                // anchor on CASH_CLEARING (the cash being returned is the carried value).
                (RefundPhase::Initiated, RefundPattern::BRestoreAr) => {
                    let Some(account_id) =
                        chart.resolve(AccountClass::CashClearing, &req.currency, None)
                    else {
                        return Ok(());
                    };
                    let c = self
                        .payment
                        .read_account_carried(scope, req.tenant_id, account_id, &req.currency)
                        .await
                        .map_err(|e| {
                            DomainError::Internal(format!("read cash-clearing carried: {e}"))
                        })?;
                    (
                        c.balance_minor,
                        c.functional_balance_minor,
                        c.functional_currency,
                    )
                }
                // UnknownFinal (Group F) — out of scope.
                (RefundPhase::UnknownFinal, _) => return Ok(()),
            };

        // Cross-currency detect (design decision 8): the anchor grain carries a
        // functional balance. NULL ⇒ single-currency refund: leave functional NULL.
        let (Some(anchor_functional), Some(functional_ccy)) =
            (functional_balance_minor, functional_currency)
        else {
            return Ok(());
        };

        // A CROSS-CURRENCY claw-back (refund-of-refund) restores the drawn-down
        // position, which must be valued at the PRIOR refund's locked rate — NOT the
        // anchor grain's current WAC (the grain is restored, not relieved, so its WAC
        // would synthesize a spurious FX result). Sourcing the prior refund's snapshot
        // is Slice 7; until then REJECT a cross-currency claw-back with a precise,
        // NON-retryable error (distinct from the queue's `RefundClawbackDeferred`)
        // rather than silently posting functional-NULL — a silent transaction-vs-
        // functional drift. Single-currency claw-backs took the NULL branch above and
        // post unaffected.
        if req.is_clawback() {
            return Err(DomainError::FxOperationUnsupported(format!(
                "refund {} is a cross-currency claw-back; functional carry-forward at \
                 the prior refund's rate is not yet supported (Slice 7) — route it \
                 through a manual adjustment",
                req.refund_id
            )));
        }

        // Both refund legs share the amount, so the first line's amount is the
        // relieved amount. A non-positive carried balance or an over-relief ⇒ skip
        // carry-forward so the projector's NegativeBalance surfaces.
        let relieved = lines.first().map_or(0, |l| l.amount_minor);
        if balance_minor <= 0 || relieved > balance_minor {
            return Ok(());
        }

        // Stamp every leg's functional at the anchor grain's WAC pro-rata of its OWN
        // amount. Both legs share the amount, so both get the same value → the
        // functional column nets to zero (carry-forward; no FX line).
        for line in lines.iter_mut() {
            let func = carried_relief(anchor_functional, balance_minor, line.amount_minor)
                .map_err(|e| DomainError::Internal(format!("refund FX carry-forward: {e}")))?;
            line.functional_amount_minor = Some(func);
            line.functional_currency = Some(functional_ccy.clone());
        }
        Ok(())
    }

    /// Assemble the `refund` record row for a phase post. `clearing_state` is the
    /// plan's resulting state (`PENDING` / `SETTLED`) for a forward post or
    /// `REVERSED` for a stage-1 reversal; `reverses_entry_id` is `Some` only on a
    /// reversal.
    fn refund_row(
        req: &RefundRequest,
        clearing_state: &str,
        reverses_entry_id: Option<Uuid>,
    ) -> NewRefund {
        NewRefund {
            tenant_id: req.tenant_id,
            refund_id: req.refund_id.clone(),
            psp_refund_id: req.psp_refund_id.clone(),
            phase: req.phase.as_str().to_owned(),
            pattern: req.pattern.as_str().to_owned(),
            payment_id: req.payment_id.clone(),
            invoice_id: req.invoice_id.clone(),
            currency: req.currency.clone(),
            amount_minor: req.amount_minor,
            clearing_state: clearing_state.to_owned(),
            // The refund-of-refund forward link (Group E): a claw-back / additional-
            // outbound carries the prior refund it references; `None` for a
            // first-order refund. Rides the request verbatim.
            relates_to_refund_id: req.relates_to_refund_id.clone(),
            reverses_entry_id,
            created_at_utc: OffsetDateTime::now_utc(),
        }
    }

    /// Map one [`PlannedLeg`] + its resolved chart account/scale to the engine
    /// [`NewLine`]. The `AR` / `UNALLOCATED` legs carry `payer_tenant_id` (their
    /// cache grains key on it) + the Pattern-B `invoice_id` (so a restored-AR leg
    /// nets the right invoice's `ar_invoice_balance`); the clearing legs are
    /// stream-less, invoice-less system grains.
    fn mk_line(req: &RefundRequest, leg: &PlannedLeg, account_id: Uuid, scale: u8) -> NewLine {
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
            // Only the Pattern-B AR leg keys on an invoice (it re-opens that
            // invoice's receivable). Pattern A (UNALLOCATED) + the clearing legs
            // carry no invoice. `validate_shape` guaranteed `invoice_id` is `Some`
            // exactly for Pattern B, so this rides the request's `invoice_id`. The
            // stage-1 reversal carries the same invoice_id (the inverted AR leg nets
            // the receivable back down).
            invoice_id: req.invoice_id.clone(),
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
}

/// The engine idempotency `source_business_id` for a refund phase post:
/// `"{psp_refund_id}:{phase}"` — one claim per PSP-refund phase (design §7). A
/// single PSP refund advances through several phase rows (`initiated → confirmed`,
/// or `rejected`/`voided`), each its own idempotent post.
fn refund_business_id(psp_refund_id: &str, phase: &str) -> String {
    format!("{psp_refund_id}:{phase}")
}

/// The deferred-apply queue / dedup `business_id` for a deferred claw-back
/// (Group E): `"{psp_refund_id}:initiated"` — a claw-back only ever defers at its
/// stage-1 `initiated` (where the money-out decrement happens). Distinct grain from
/// the engine `REFUND` claim because it rides the separate `REFUND_CLAWBACK` flow.
fn clawback_business_id(psp_refund_id: &str) -> String {
    format!("{psp_refund_id}:{}", RefundPhase::Initiated.as_str())
}

/// What the claw-back defer intake committed (mirrors `ChargebackService`'s
/// `IntakeOutcome`): a fresh enqueue, an idempotent re-defer of the same key, or a
/// same-key / different-payload conflict (carrying the STORED hash for the AC #19
/// secured-audit capture, Z14-1).
enum ClawbackIntake {
    Enqueued,
    AlreadyQueued,
    /// A same-key reuse with a different payload — carries the STORED payload hash so
    /// the conflict-return site can capture stored-vs-incoming on the secured-audit
    /// sink (Z14-1) before returning the error.
    Conflict {
        stored_hash: String,
    },
}

/// The terminal shape of applying ONE queued claw-back row
/// ([`RefundHandler::apply_queued_clawback`]).
#[derive(Debug)]
pub enum ClawbackApply {
    /// The decrement fit (the matching outbound landed) — posted + flipped
    /// `→APPLIED`.
    Applied,
    /// The decrement STILL underflows but the row is within the aging horizon —
    /// leave `QUEUED`, back off.
    StillDeferred,
    /// The claw-back aged out without reconciling — flipped `→CANCELLED` + escalated
    /// (exception stub + `CLAWBACK_UNDERFLOW` alarm).
    Escalated,
}

/// Summary of one [`RefundHandler::drain_clawbacks`] pass (mirrors the
/// allocation/chargeback `DrainReport`).
#[derive(Debug, Default)]
pub struct ClawbackDrainReport {
    pub applied: u64,
    pub still_deferred: u64,
    pub escalated: u64,
}

/// The outcome of recording a refund via [`RefundHandler::record_refund`] (Group
/// G): either it POSTED inline (the origin settlement resolved) or it was
/// QUARANTINED (refund-before-payment — durably queued, NEVER posted; the REST
/// surface maps it to a 202 + `refund-quarantined` token).
#[derive(Debug)]
pub enum RefundOutcome {
    /// The refund posted inline (fresh ⇒ 201, replay ⇒ 200 via `PostingRef::replayed`).
    Posted(PostingRef),
    /// The refund references a payment with no resolvable origin settlement — it was
    /// QUARANTINED on the `REFUND_QUARANTINE` queue (202), never posted.
    Quarantined(QuarantineHandle),
    /// The refund's origin payment has an OPEN dispute (Z5-2, design §5) — the cash
    /// leg was durably HELD on the `REFUND_DISPUTE_HOLD` queue (202), never posted.
    /// The hold drain re-drives it once the dispute resolves WON (or cancels it on
    /// LOST). The REST surface maps it to a 202 + `refund-dispute-held` body token.
    DisputeHeld(DisputeHoldHandle),
}

/// The handle a quarantined refund returns (Group G): the queue key + the intake
/// instant — the REST 202 `refund-quarantined` body. No posting handle (nothing
/// posted).
#[derive(Debug, Clone)]
pub struct QuarantineHandle {
    /// The quarantine queue flow (`REFUND_QUARANTINE`).
    pub flow: String,
    /// The quarantine/dedup business id (`psp_refund_id:phase`).
    pub business_id: String,
    /// When the intake durably quarantined the request.
    pub quarantined_at: OffsetDateTime,
}

/// The handle a dispute-held refund returns (Z5-2, design §5): the queue key + the
/// intake instant — the REST 202 `refund-dispute-held` body. No posting handle
/// (nothing posted). Mirrors [`QuarantineHandle`].
#[derive(Debug, Clone)]
pub struct DisputeHoldHandle {
    /// The dispute-hold queue flow (`REFUND_DISPUTE_HOLD`).
    pub flow: String,
    /// The dispute-hold/dedup business id (`psp_refund_id:phase`).
    pub business_id: String,
    /// When the intake durably held the request.
    pub held_at: OffsetDateTime,
}

/// The outcome of the atomic `refund-with-credit-note` composite
/// ([`RefundHandler::post_refund_with_credit_note`]): both entry ids, committed
/// together. `replayed` ⇒ an idempotent re-drive of an already-posted composite.
#[derive(Debug, Clone)]
pub struct RefundWithCreditNoteOutcome {
    pub refund_entry_id: Uuid,
    pub credit_note_entry_id: Uuid,
    pub replayed: bool,
}

/// What the quarantine intake committed (mirrors `ClawbackIntake`).
enum QuarantineIntake {
    Quarantined,
    AlreadyQuarantined,
    /// A same-key reuse with a different payload — carries the STORED payload hash
    /// for the AC #19 secured-audit capture (Z14-1).
    Conflict {
        stored_hash: String,
    },
}

/// The terminal shape of de-quarantining ONE row
/// ([`RefundHandler::apply_quarantined`]).
#[derive(Debug)]
pub enum QuarantineApply {
    /// The origin landed + caps/threshold passed ⇒ posted + flipped `→APPLIED`.
    Released,
    /// The origin landed but the THEN-CURRENT D2 threshold was crossed ⇒ an approval
    /// is now open; the row stays `QUEUED` (it NEVER auto-posts over threshold).
    AwaitingApproval,
    /// The origin payment is STILL absent and the row is within the aging horizon ⇒
    /// leave `QUEUED`, back off.
    StillMissing,
    /// The origin never landed past the aging horizon ⇒ flipped `→CANCELLED` +
    /// escalated (exception stub + `RefundQuarantined` Critical alarm).
    Escalated,
}

/// Summary of one [`RefundHandler::drain_quarantine`] pass.
#[derive(Debug, Default)]
pub struct QuarantineDrainReport {
    pub released: u64,
    pub awaiting_approval: u64,
    pub still_missing: u64,
    pub escalated: u64,
}

/// What the dispute-hold intake committed (mirrors `QuarantineIntake`).
enum DisputeHoldIntake {
    Held,
    AlreadyHeld,
    /// A same-key reuse with a different payload — carries the STORED payload hash
    /// for the AC #19 secured-audit capture (Z14-1).
    Conflict {
        stored_hash: String,
    },
}

/// The terminal shape of draining ONE dispute-held refund row
/// ([`RefundHandler::apply_dispute_hold`], Z5-2 / design §5).
#[derive(Debug)]
pub enum DisputeHoldApply {
    /// The dispute resolved WON + caps/threshold passed ⇒ posted + flipped
    /// `→APPLIED`.
    Released,
    /// The dispute resolved WON but the THEN-CURRENT D2 threshold was crossed ⇒ an
    /// approval is now open; the row stays `QUEUED` (it NEVER auto-posts over
    /// threshold).
    AwaitingApproval,
    /// The dispute is STILL `OPENED` and the row is within the aging horizon ⇒ leave
    /// `QUEUED`, back off (the cash stays held).
    StillDisputed,
    /// The dispute resolved LOST (a chargeback returned the money) ⇒ flipped
    /// `→CANCELLED` + escalated. NEVER posted (double-pay guard).
    Cancelled,
    /// The dispute never resolved past the aging horizon ⇒ flipped `→CANCELLED` +
    /// escalated (exception stub + `RefundQuarantined` Critical alarm).
    Escalated,
}

/// Summary of one [`RefundHandler::drain_dispute_hold`] pass.
#[derive(Debug, Default)]
pub struct DisputeHoldDrainReport {
    pub released: u64,
    pub awaiting_approval: u64,
    pub still_disputed: u64,
    pub cancelled: u64,
    pub escalated: u64,
}

/// The PII-free snapshot of a dispute-held refund, persisted as the queue row's
/// `payload` jsonb at intake + re-read by the dispute-hold drain (Z5-2 / design §5).
/// Carries the held refund's financial keys PLUS the `(dispute_id, dispute_cycle)`
/// the drain re-reads to decide WON (re-drive) vs LOST (cancel) vs still-OPEN (back
/// off). PII-free by construction (ids + money + enum wire literals only). The enums
/// ride as their stable wire literals (the domain stays serde-free); the drain
/// parses them back into a [`RefundRequest`]. Mirrors [`QuarantinedRefundPayload`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DisputeHeldRefundPayload {
    tenant_id: Uuid,
    payer_tenant_id: Uuid,
    refund_id: String,
    psp_refund_id: String,
    /// The phase wire literal.
    phase: String,
    /// The pattern wire literal.
    pattern: String,
    payment_id: String,
    invoice_id: Option<String>,
    currency: String,
    amount_minor: i64,
    two_stage: bool,
    relates_to_refund_id: Option<String>,
    /// The direction wire literal.
    direction: String,
    /// The OPEN dispute this refund is held behind — re-read by the drain.
    dispute_id: String,
    /// The dispute cycle at hold time (diagnostic; the drain re-reads the row's
    /// current state, not this snapshot).
    dispute_cycle: i32,
}

impl DisputeHeldRefundPayload {
    /// Snapshot a dispute-held [`RefundRequest`] (+ the open dispute it is held
    /// behind) into the PII-free payload.
    fn from_request(req: &RefundRequest, dispute_id: &str, dispute_cycle: i32) -> Self {
        Self {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            refund_id: req.refund_id.clone(),
            psp_refund_id: req.psp_refund_id.clone(),
            phase: req.phase.as_str().to_owned(),
            pattern: req.pattern.as_str().to_owned(),
            payment_id: req.payment_id.clone(),
            invoice_id: req.invoice_id.clone(),
            currency: req.currency.clone(),
            amount_minor: req.amount_minor,
            two_stage: req.two_stage,
            relates_to_refund_id: req.relates_to_refund_id.clone(),
            direction: req.direction.as_str().to_owned(),
            dispute_id: dispute_id.to_owned(),
            dispute_cycle,
        }
    }

    /// Reconstruct the [`RefundRequest`] from a dispute-held payload at drain time,
    /// parsing the wire enum literals (the inverse of [`Self::from_request`]).
    ///
    /// # Errors
    /// [`DomainError::Internal`] when a stored enum literal is unknown (data
    /// corruption — written only from `as_str`).
    fn into_request(self) -> Result<RefundRequest, DomainError> {
        let phase = RefundPhase::parse(&self.phase).ok_or_else(|| {
            DomainError::Internal(format!(
                "dispute-held refund unknown phase {:?}",
                self.phase
            ))
        })?;
        let pattern = RefundPattern::parse(&self.pattern).ok_or_else(|| {
            DomainError::Internal(format!(
                "dispute-held refund unknown pattern {:?}",
                self.pattern
            ))
        })?;
        let direction = RefundDirection::parse(&self.direction).ok_or_else(|| {
            DomainError::Internal(format!(
                "dispute-held refund unknown direction {:?}",
                self.direction
            ))
        })?;
        Ok(RefundRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            refund_id: self.refund_id,
            psp_refund_id: self.psp_refund_id,
            phase,
            pattern,
            payment_id: self.payment_id,
            invoice_id: self.invoice_id,
            currency: self.currency,
            amount_minor: self.amount_minor,
            two_stage: self.two_stage,
            relates_to_refund_id: self.relates_to_refund_id,
            direction,
        })
    }
}

/// The PII-free snapshot of a quarantined refund, persisted as the queue row's
/// `payload` jsonb at intake + re-read by the de-quarantine drain. Carries the
/// PSP-correlation id (`psp_refund_id`) so a reconciler can match it to the missing
/// payment, but NO names / free text. The enums ride as their stable wire literals
/// (the domain stays serde-free); the drain parses them back into a
/// [`RefundRequest`]. Mirrors [`QueuedClawbackPayload`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct QuarantinedRefundPayload {
    tenant_id: Uuid,
    payer_tenant_id: Uuid,
    refund_id: String,
    psp_refund_id: String,
    /// The phase wire literal.
    phase: String,
    /// The pattern wire literal.
    pattern: String,
    payment_id: String,
    invoice_id: Option<String>,
    currency: String,
    amount_minor: i64,
    two_stage: bool,
    relates_to_refund_id: Option<String>,
    /// The direction wire literal.
    direction: String,
}

impl QuarantinedRefundPayload {
    /// Snapshot a refund-before-payment [`RefundRequest`] into the PII-free payload.
    fn from_request(req: &RefundRequest) -> Self {
        Self {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            refund_id: req.refund_id.clone(),
            psp_refund_id: req.psp_refund_id.clone(),
            phase: req.phase.as_str().to_owned(),
            pattern: req.pattern.as_str().to_owned(),
            payment_id: req.payment_id.clone(),
            invoice_id: req.invoice_id.clone(),
            currency: req.currency.clone(),
            amount_minor: req.amount_minor,
            two_stage: req.two_stage,
            relates_to_refund_id: req.relates_to_refund_id.clone(),
            direction: req.direction.as_str().to_owned(),
        }
    }

    /// Reconstruct the [`RefundRequest`] from a quarantined payload at drain time,
    /// parsing the wire enum literals (the inverse of [`Self::from_request`]).
    ///
    /// # Errors
    /// [`DomainError::Internal`] when a stored enum literal is unknown (data
    /// corruption — written only from `as_str`).
    fn into_request(self) -> Result<RefundRequest, DomainError> {
        let phase = RefundPhase::parse(&self.phase).ok_or_else(|| {
            DomainError::Internal(format!("quarantined refund unknown phase {:?}", self.phase))
        })?;
        let pattern = RefundPattern::parse(&self.pattern).ok_or_else(|| {
            DomainError::Internal(format!(
                "quarantined refund unknown pattern {:?}",
                self.pattern
            ))
        })?;
        let direction = RefundDirection::parse(&self.direction).ok_or_else(|| {
            DomainError::Internal(format!(
                "quarantined refund unknown direction {:?}",
                self.direction
            ))
        })?;
        Ok(RefundRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            refund_id: self.refund_id,
            psp_refund_id: self.psp_refund_id,
            phase,
            pattern,
            payment_id: self.payment_id,
            invoice_id: self.invoice_id,
            currency: self.currency,
            amount_minor: self.amount_minor,
            two_stage: self.two_stage,
            relates_to_refund_id: self.relates_to_refund_id,
            direction,
        })
    }
}

/// The in-transaction [`PostSidecar`] for the atomic `refund-with-credit-note`
/// composite (Group G / K-3): it runs the refund's normal in-txn work (delegating
/// to the wrapped [`RefundPostSidecar`] — caps + `refund` row + `refund.recorded`
/// event) AND posts the prepared credit note as the SECOND entry in the SAME txn
/// (via [`CreditNoteHandler::apply_in_txn`]). So both journal entries + both record
/// rows + both events commit atomically, or roll back together — AR is never
/// overstated between them. Mirrors the composite shape of
/// [`QueuedClawbackApplySidecar`] (delegate-then-extra-write).
struct RefundWithCreditNoteSidecar {
    /// The refund's own in-txn sidecar (caps + record + event).
    refund: RefundPostSidecar,
    /// The credit-note orchestrator (posts the second entry in-txn).
    credit_note: Arc<CreditNoteHandler>,
    /// The prepared credit note, shared by `Arc` so a serializable RETRY of the
    /// outer refund post re-runs this sidecar safely (the prepared note is borrowed,
    /// never consumed). `apply_in_txn` re-claims the credit note's dedup each attempt
    /// (idempotent: a committed prior attempt replays).
    prepared: Arc<PreparedCreditNote>,
    /// The security context for the credit-note's in-txn writes.
    ctx: SecurityContext,
    /// The slot the composite orchestrator reads the posted credit-note entry id
    /// back from after the txn commits (overwritten each attempt; the committed
    /// attempt's value is the one read back).
    outcome: Arc<std::sync::Mutex<Option<CompositeCreditNoteOutcome>>>,
}

#[async_trait::async_trait]
impl PostSidecar for RefundWithCreditNoteSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. The refund's in-txn work (caps + refund row + refund.recorded event).
        self.refund.run(txn, scope, posted).await?;
        // 2. The paired credit note as the SECOND entry in the SAME txn. A failure
        //    here (split-ambiguous slipped through, headroom CHECK, AR no-negative,
        //    credit-note idempotency conflict) rolls the WHOLE composite back — the
        //    refund entry included.
        let cn = self
            .credit_note
            .apply_in_txn(&self.ctx, txn, scope, &self.prepared)
            .await?;
        let mut slot = self
            .outcome
            .lock()
            .map_err(|_| DomainError::Internal("composite outcome mutex poisoned".to_owned()))?;
        *slot = Some(cn);
        Ok(())
    }
}

/// The financial-key snapshot of a deferred claw-back, persisted as the queue row's
/// `payload` jsonb at intake and re-read by the drain. PII-free by construction
/// (ids + money + enum wire literals only — no names / free text). The enums are
/// carried as their stable wire literals so the domain stays serde-free (pure); the
/// drain parses them back into a [`RefundRequest`]. Mirrors
/// [`crate::infra::payment::chargeback::QueuedDisputePayload`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct QueuedClawbackPayload {
    tenant_id: Uuid,
    payer_tenant_id: Uuid,
    refund_id: String,
    psp_refund_id: String,
    /// The pattern wire literal (`A_UNALLOCATED` / `B_RESTORE_AR`).
    pattern: String,
    payment_id: String,
    invoice_id: Option<String>,
    currency: String,
    amount_minor: i64,
    two_stage: bool,
    /// The prior refund this claws back (always `Some` — a claw-back requires it).
    relates_to_refund_id: Option<String>,
    /// The direction wire literal — always `CLAWBACK` (only a claw-back defers), but
    /// carried verbatim so the rebuilt request is byte-identical.
    direction: String,
}

impl QueuedClawbackPayload {
    /// Snapshot a deferred claw-back [`RefundRequest`] into the PII-free payload.
    fn from_request(req: &RefundRequest) -> Self {
        Self {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            refund_id: req.refund_id.clone(),
            psp_refund_id: req.psp_refund_id.clone(),
            pattern: req.pattern.as_str().to_owned(),
            payment_id: req.payment_id.clone(),
            invoice_id: req.invoice_id.clone(),
            currency: req.currency.clone(),
            amount_minor: req.amount_minor,
            two_stage: req.two_stage,
            relates_to_refund_id: req.relates_to_refund_id.clone(),
            direction: req.direction.as_str().to_owned(),
        }
    }

    /// Reconstruct the [`RefundRequest`] from a queued payload at drain time,
    /// parsing the wire enum literals (the inverse of [`Self::from_request`]). The
    /// phase is fixed to `initiated` (the only phase a claw-back defers at).
    ///
    /// # Errors
    /// [`DomainError::Internal`] when a stored enum literal is unknown (data
    /// corruption — written only from `as_str`).
    fn into_request(self) -> Result<RefundRequest, DomainError> {
        let pattern = RefundPattern::parse(&self.pattern).ok_or_else(|| {
            DomainError::Internal(format!(
                "queued claw-back unknown pattern {:?}",
                self.pattern
            ))
        })?;
        let direction = RefundDirection::parse(&self.direction).ok_or_else(|| {
            DomainError::Internal(format!(
                "queued claw-back unknown direction {:?}",
                self.direction
            ))
        })?;
        Ok(RefundRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            refund_id: self.refund_id,
            psp_refund_id: self.psp_refund_id,
            phase: RefundPhase::Initiated,
            pattern,
            payment_id: self.payment_id,
            invoice_id: self.invoice_id,
            currency: self.currency,
            amount_minor: self.amount_minor,
            two_stage: self.two_stage,
            relates_to_refund_id: self.relates_to_refund_id,
            direction,
        })
    }
}

/// The request-based idempotency hash for a deferred claw-back — the `content_hash`
/// of the canonical [`QueuedClawbackPayload`]. Stable across retries, so it (not an
/// entry hash) is what the dedup row stores; `claim_queued`'s `Replay` compares it
/// to reject a same-key / different-payload reuse. Mirrors `dispute_request_hash`.
fn clawback_request_hash(payload: &QueuedClawbackPayload) -> Result<String, DomainError> {
    let canonical = serde_json::to_string(payload)
        .map_err(|e| DomainError::Internal(format!("canonicalize claw-back payload: {e}")))?;
    Ok(IdempotencyGate::content_hash(&canonical))
}

/// Exponential backoff (wall-clock) before a still-underflowing claw-back may be
/// re-claimed (mirrors the allocate/chargeback `blocked_backoff`): ~`2^(attempts-1)`
/// seconds, capped at 5 minutes. Keeps a not-yet-reconciled claw-back from
/// hot-looping the drain before it either nets against its matching outbound or ages
/// out and escalates.
fn clawback_backoff(attempts: i64) -> Duration {
    const BASE_SECS: i64 = 2;
    const MAX_SECS: i64 = 300;
    let shift = attempts.clamp(1, 16) - 1;
    let secs = BASE_SECS.saturating_mul(1_i64 << shift).min(MAX_SECS);
    Duration::seconds(secs)
}

/// Composite in-transaction sidecar for the deferred apply of a queued claw-back:
/// it runs the SAME claw-back decrement + record write as the inline path (by
/// delegating to the wrapped [`RefundPostSidecar`] — whose underflow pre-check
/// re-runs under the rank-1 lock) and THEN flips the work-state queue row
/// `→APPLIED` — both inside the post txn opened by
/// [`PostingService::post_queued_apply`]. So the claw-back effect and the queue-row
/// transition commit atomically, or roll back together (an underflow-deferred
/// re-try rolls the `→APPLIED` flip back too, leaving the row claimable). Mirrors
/// [`crate::infra::payment::chargeback`]'s `QueuedChargebackApplySidecar`.
struct QueuedClawbackApplySidecar {
    inner: RefundPostSidecar,
    flow: String,
    business_id: String,
    tenant: Uuid,
}

#[async_trait::async_trait]
impl PostSidecar for QueuedClawbackApplySidecar {
    /// The queued-apply re-drive must, like the inline claw-back, run its cap /
    /// underflow CHECK BEFORE projection (delegates to the wrapped inner
    /// [`RefundPostSidecar`]). Otherwise an orphan claw-back (no matching
    /// outbound) draws `REFUND_CLEARING` negative and the projector's no-negative
    /// guard trips with a raw `NegativeBalance` before the underflow can surface
    /// as `RefundClawbackDeferred` — so the drain never recognizes the aged row as
    /// never-reconciled and never escalates it.
    fn run_before_projection(&self) -> bool {
        self.inner.run_before_projection()
    }

    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. The claw-back decrement (+ underflow pre-check) + the refund record
        //    write (delegated). A still-underflow returns `RefundClawbackDeferred`,
        //    rolling the whole apply back (including the `→APPLIED` flip below).
        self.inner.run(txn, scope, posted).await?;
        // 2. Flip the work-state queue row `→APPLIED` in the SAME txn.
        PendingQueueRepo::mark_applied(txn, scope, self.tenant, &self.flow, &self.business_id)
            .await
            .map_err(|e| {
                DomainError::Internal(format!("claw-back queue-apply mark_applied: {e}"))
            })?;
        Ok(())
    }
}

/// Build the strict line-negation of a refund plan: the SAME legs with DR<->CR
/// inverted (and the same amounts). Used for the stage-1 reversal — a `rejected` /
/// `voided` posts the stage-1 entry with its sides swapped, so every grain the
/// stage-1 moved is moved back exactly. The result is balanced iff the input was
/// (one DR ↔ one CR of equal amount), preserved by the inversion.
fn invert_plan(plan: &RefundLegPlan) -> RefundLegPlan {
    let legs = plan
        .legs
        .iter()
        .map(|l| PlannedLeg {
            account_class: l.account_class,
            side: match l.side {
                Side::Debit => Side::Credit,
                Side::Credit => Side::Debit,
            },
            amount_minor: l.amount_minor,
            revenue_stream: l.revenue_stream.clone(),
        })
        .collect();
    RefundLegPlan {
        legs,
        // The reversal leaves the refund REVERSED; the plan's own clearing_state is
        // not used for the row (the handler stamps CLEARING_STATE_REVERSED).
        clearing_state: CLEARING_STATE_REVERSED,
    }
}

/// The per-payment cap deltas a refund stage-1 post applies (design §4.4 / §4.7).
/// Resolved from the request's pattern: BOTH patterns move `refunded_minor`;
/// Pattern A additionally moves `refunded_unallocated_minor`; Pattern B additionally
/// moves the per-`(payment, invoice)` `payment_allocation_refund.refunded_minor`.
/// The [`CapMode`] decides the SIGN (initiate = +amount, release = −amount); stage-2
/// (`CapMode::None`) does not construct any movement.
struct RefundCap {
    tenant: Uuid,
    payment_id: String,
    amount_minor: i64,
    /// `Some(invoice_id)` for Pattern B (the per-invoice cap target); `None` for
    /// Pattern A.
    invoice_id: Option<String>,
    /// Whether to additionally move `refunded_unallocated_minor` (Pattern A only).
    is_unallocated_pattern: bool,
}

impl RefundCap {
    /// Resolve the cap movement from the request's pattern.
    fn for_request(req: &RefundRequest) -> Self {
        Self {
            tenant: req.tenant_id,
            payment_id: req.payment_id.clone(),
            amount_minor: req.amount_minor,
            invoice_id: match req.pattern {
                RefundPattern::BRestoreAr => req.invoice_id.clone(),
                RefundPattern::AUnallocated => None,
            },
            is_unallocated_pattern: matches!(req.pattern, RefundPattern::AUnallocated),
        }
    }

    /// Apply the cap movement for `mode` under the rank-1 `payment_settlement` lock
    /// (the post-delta CHECKs are the over-refund backstop). [`CapMode::None`] is a
    /// no-op (stage-2). For [`CapMode::Clawback`] this FIRST runs the underflow
    /// PRE-CHECK under the rank-1 lock (`payment.read_settlement_for_update`): if the
    /// decrement would drive `refunded_minor` below zero (an out-of-order / over PSP
    /// claw-back) it returns [`UnderflowDeferred`] WITHOUT applying any movement —
    /// the design defers such a claw-back rather than applying it or tripping the
    /// `refunded_minor >= 0` CHECK (the CHECK stays a backstop that must never fire).
    /// Other errors are repo-level; the sidecar refines them.
    async fn apply(
        &self,
        payment: &PaymentRepo,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        mode: CapMode,
    ) -> Result<(), CapApplyError> {
        let signed = match mode {
            CapMode::Initiate => self.amount_minor,
            // Both DECREMENT by the amount: `Release` backs out a matching stage-1
            // (cannot underflow); `Clawback` nets the origin money-out down to the
            // NET refunded AFTER the underflow pre-check below has cleared it.
            CapMode::Release | CapMode::Clawback => -self.amount_minor,
            CapMode::None => return Ok(()),
        };

        // Claw-back underflow PRE-CHECK (Group E, design §4.4). Read the current
        // counters UNDER THE RANK-1 LOCK the decrement is about to take, and decide
        // `current - amount < 0` BEFORE applying anything. If it would underflow,
        // DEFER (return `UnderflowDeferred`) — do NOT apply the decrement and do NOT
        // let the `refunded_minor >= 0` CHECK hard-fail. We check `refunded_minor`
        // (the total money-out counter both patterns decrement) and, for the
        // additional counters, their own current values, so a Pattern-A
        // `refunded_unallocated` / Pattern-B `payment_allocation_refund` claw-back
        // also defers rather than tripping their nonneg CHECKs. The settlement MUST
        // exist for a claw-back (nothing to claw back otherwise) — absent ⇒ a repo
        // Db error (an upstream contract violation), surfaced as infra.
        if mode == CapMode::Clawback {
            let settlement = payment
                .read_settlement_for_update(txn, scope, self.tenant, &self.payment_id)
                .await
                .map_err(CapApplyError::Repo)?
                .ok_or_else(|| {
                    CapApplyError::Repo(RepoError::Db(format!(
                        "claw-back references payment {} with no settlement row",
                        self.payment_id
                    )))
                })?;
            // Would the total money-out decrement underflow? (`refunded_minor` is the
            // counter the matching outbound refund stage-1 raised; the claw-back
            // arriving first / over-clawing leaves it too small.)
            if settlement.refunded_minor < self.amount_minor {
                return Err(CapApplyError::UnderflowDeferred);
            }
            // The additional per-pattern counters must also have room (defensive —
            // they move in lockstep with `refunded_minor` in the happy path, but an
            // out-of-order Pattern-A/B claw-back could underflow one of them first).
            if self.is_unallocated_pattern
                && settlement.refunded_unallocated_minor < self.amount_minor
            {
                return Err(CapApplyError::UnderflowDeferred);
            }
            if let Some(invoice_id) = &self.invoice_id {
                let par_refunded = payment
                    .read_allocation_refund_refunded_for_update(
                        txn,
                        scope,
                        self.tenant,
                        &self.payment_id,
                        invoice_id,
                    )
                    .await
                    .map_err(CapApplyError::Repo)?;
                if par_refunded < self.amount_minor {
                    return Err(CapApplyError::UnderflowDeferred);
                }
            }
        }

        // 1. Total money-out cap (both patterns): refunded + clawed_back <= settled.
        //    Rank-1 `payment_settlement` lock — taken first.
        PaymentRepo::add_refunded(txn, scope, self.tenant, &self.payment_id, signed)
            .await
            .map_err(CapApplyError::Repo)?;

        // 2a. Pattern A: spendable-headroom cap (allocated + refunded_unallocated <=
        //     settled) — refunded on-account cash can no longer be allocated. Same
        //     `payment_settlement` row (rank-1).
        if self.is_unallocated_pattern {
            PaymentRepo::add_refunded_unallocated(
                txn,
                scope,
                self.tenant,
                &self.payment_id,
                signed,
            )
            .await
            .map_err(CapApplyError::Repo)?;
        }

        // 2b. Pattern B: per-`(payment, invoice)` cap (refunded <= allocated) on the
        //     `payment_allocation_refund` row.
        if let Some(invoice_id) = &self.invoice_id {
            PaymentRepo::add_allocation_refund_refunded(
                txn,
                scope,
                self.tenant,
                &self.payment_id,
                invoice_id,
                signed,
            )
            .await
            .map_err(CapApplyError::Repo)?;
        }
        Ok(())
    }
}

/// The outcome of [`RefundCap::apply`] when it fails. A `Repo` error is a cap CHECK
/// violation or an infra fault (the sidecar refines it to the over-refund domain
/// errors); `UnderflowDeferred` is the Group-E signal that a CLAW-BACK's money-out
/// decrement would drive a counter below zero (out-of-order / over PSP claw-back) —
/// the sidecar turns it into [`DomainError::RefundClawbackDeferred`] so the post
/// rolls back and the handler defers the request (never a hard-fail on the nonneg
/// CHECK).
enum CapApplyError {
    /// A cap CHECK violation or an infra fault from a counter write / read.
    Repo(RepoError),
    /// A claw-back decrement would underflow a money-out counter — DEFER, do not
    /// apply (Group E).
    UnderflowDeferred,
}

/// Build the PII-clean before/after payload for an `unknown_final` disposition's
/// secured-audit record (design §2.3 — ids + amounts + enum codes only, NO names
/// / free text). The "before" is the stuck open clearing — its LIVE
/// `clearing_state` + open amount, read from the stage-1 `refund` row by the caller
/// (Z5-4), NOT a hardcoded `PENDING` / the request amount; the "after" is the
/// SUSPENSE park that drains it. The caller (`post_unknown_final`) asserts this is
/// PII-clean before it reaches the sink.
fn unknown_final_audit_payload(
    req: &RefundRequest,
    before_clearing_state: &str,
    open_minor: i64,
) -> serde_json::Value {
    serde_json::json!({
        "disposition": "REFUND_UNKNOWN_FINAL",
        "refund_id": req.refund_id,
        "psp_refund_id": req.psp_refund_id,
        "payment_id": req.payment_id,
        "pattern": req.pattern.as_str(),
        "currency": req.currency,
        "before": {
            // The REAL open clearing amount + the REAL stage-1 clearing_state (Z5-4),
            // read live from the stage-1 refund row — not assumed.
            "refund_clearing_open_minor": open_minor,
            "clearing_state": before_clearing_state,
        },
        "after": {
            "refund_clearing_open_minor": 0,
            "park_account_class": UNKNOWN_FINAL_PARK_CLASS.as_str(),
            "parked_minor": open_minor,
            "clearing_state": CLEARING_STATE_SETTLED,
        },
    })
}

/// The in-transaction [`PostSidecar`] for the `unknown_final` disposition (Group
/// F): in the post txn it **(1)** persists the `refund` row (`clearing_state` =
/// SETTLED) and **(2)** appends one `secured_audit_record` via the
/// [`SecuredAuditSink`] — both atomic with the loss-clearing journal entry (or
/// rolled back with it). Order is record-then-audit; either failure rolls the
/// whole disposition back. The sink is the [`NoopSecuredAuditSink`] until Slice 6
/// merges (it logs + never fails), so today the audit step is observably a no-op
/// but the call site is the real Slice-6 contract.
struct UnknownFinalSidecar {
    /// The `refund` row to persist (`clearing_state` = SETTLED, the terminal
    /// park-to-SUSPENSE resolution; its own `(tenant, psp_refund_id, unknown_final)`
    /// grain).
    refund_row: NewRefund,
    /// The secured-audit sink (Slice 6 port). No-op until merge.
    audit: Arc<dyn SecuredAuditSink>,
    /// The acting subject id (the approver/operator) for the audit `actor_ref`;
    /// `None` for a system-initiated disposition.
    actor_ref: Option<String>,
    /// The PII-clean before/after audit payload.
    before_after: serde_json::Value,
    /// Owning tenant.
    tenant: Uuid,
    /// The event publisher: `billing.ledger.refund.recorded` (phase =
    /// `unknown_final`, K-1) is published IN this post txn (Group G).
    publisher: Arc<LedgerEventPublisher>,
    /// The security context for the in-txn outbox publish.
    ctx: SecurityContext,
}

#[async_trait::async_trait]
impl PostSidecar for UnknownFinalSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. Persist the refund record row (surrogate PK + natural UNIQUE on
        //    (tenant, psp_refund_id, unknown_final)). A replay is short-circuited
        //    by the engine claim BEFORE the sidecar, so a collision rolls back.
        AdjustmentRepo::insert_refund(txn, scope, &self.refund_row)
            .await
            .map_err(|e| DomainError::Internal(format!("insert refund (unknown_final): {e}")))?;

        // 2. Append the secured-audit record IN THE SAME txn (atomic with the loss
        //    entry, design §4.4 / K-1). `ManualAdjustment` event type + the
        //    REFUND_UNKNOWN_FINAL reason; the posted ENTRY id is the correlation id
        //    (links the audit record to the loss-clearing entry — `PostedFacts`
        //    surfaces the entry id, not the header's correlation uuid). `retain_until
        //    = None` ⇒ the store's default retention. Until Slice 6 merges this is
        //    the no-op sink (never fails).
        self.audit
            .append(
                txn,
                scope,
                self.tenant,
                AuditEventType::ManualAdjustment,
                self.actor_ref.as_deref(),
                Some(REASON_REFUND_UNKNOWN_FINAL),
                &self.before_after,
                Some(posted.entry_id),
                None,
            )
            .await
            .map_err(|e| {
                DomainError::Internal(format!("secured-audit append (unknown_final): {e}"))
            })?;

        // 3. Publish `billing.ledger.refund.recorded` (phase = unknown_final, K-1
        //    "incl unknown_final") into the SAME post txn — atomic with the loss
        //    entry + the secured-audit record (Group G).
        self.publisher
            .publish_refund_recorded(
                &self.ctx,
                txn,
                refund_recorded_event(&self.refund_row, posted),
            )
            .await
            .map_err(|e| {
                DomainError::Internal(format!("publish refund_recorded (unknown_final): {e}"))
            })?;
        Ok(())
    }
}

/// The in-transaction [`PostSidecar`] for a refund stage: runs AFTER balance
/// projection and BEFORE the dedup finalize (fresh-claim path only — a replay
/// returns before the sidecar), so its writes commit atomically with the journal
/// entry or roll back with it (design §4.4 / §4.7).
///
/// Order (the §4.7 lock order): **(1)** the per-payment money-out CAPS under the
/// rank-1 `payment_settlement` lock (taken before the rank-N record write) — a
/// stage-1 initiation INCREMENTS them ([`CapMode::Initiate`]); a stage-1 reversal
/// DECREMENTS them ([`CapMode::Release`]); a stage-2 `confirmed` leaves them
/// untouched ([`CapMode::None`]). Then **(2)** the `refund` record row. A cap CHECK
/// violation surfaces as [`RepoError::MoneyOutCapExceeded`], refined here to
/// [`DomainError::RefundExceedsSettled`] / [`DomainError::RefundExceedsAllocated`].
pub struct RefundPostSidecar {
    /// Whether (and how) to move the caps for this phase.
    cap_mode: CapMode,
    /// The pattern-resolved cap deltas to move.
    cap: RefundCap,
    /// The `refund` record to persist (surrogate `(tenant, refund_id)` PK + natural
    /// `(tenant, psp_refund_id, phase)` UNIQUE).
    refund_row: NewRefund,
    /// The payment counter repo — used by [`RefundCap::apply`] for the CLAW-BACK
    /// underflow pre-read under the rank-1 lock (Group E). A cheap clone of the
    /// handler's repo (it wraps the provider Arc).
    payment: PaymentRepo,
    /// The event publisher: `billing.ledger.refund.recorded` is published IN this
    /// post txn (the transactional outbox, Group G) so it commits atomically with
    /// the refund entry + caps, or rolls back with them. Mirrors
    /// [`CreditNotePostSidecar`](super::credit_note_service::CreditNotePostSidecar).
    publisher: Arc<LedgerEventPublisher>,
    /// The security context for the in-txn outbox publish (cloned by the handler).
    ctx: SecurityContext,
}

#[async_trait::async_trait]
impl PostSidecar for RefundPostSidecar {
    /// Refund / claw-back runs its rank-1 cap / underflow CHECK BEFORE projection
    /// (see [`PostSidecar::run_before_projection`]). Without this a stage-1
    /// over-settled forward draws `UNALLOCATED` negative, and an out-of-order
    /// claw-back draws `REFUND_CLEARING` negative, tripping the projector's
    /// no-negative guard with a raw `NegativeBalance` BEFORE `cap.apply` can
    /// refine it to `RefundExceedsSettled` / `RefundClawbackDeferred`.
    fn run_before_projection(&self) -> bool {
        true
    }

    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. Move the per-payment money-out caps under the rank-1 payment_settlement
        //    lock, BEFORE the rank-N record write (design §4.7). Stage-1 initiation
        //    INCREMENTS the cap (outbound — CHECKs enforce it before the cash leaves
        //    on stage-2); a stage-1 reversal / claw-back DECREMENTS it; stage-2 is a
        //    no-op. For a CLAW-BACK the apply runs the underflow PRE-CHECK under the
        //    same lock and returns `UnderflowDeferred` if the decrement would go
        //    negative — mapped here to `RefundClawbackDeferred`, which rolls the
        //    whole post back (the handler then defers the request to the queue, never
        //    hard-failing on the nonneg CHECK). A cap CHECK violation (over-refund)
        //    is refined to RefundExceedsSettled / RefundExceedsAllocated.
        self.cap
            .apply(&self.payment, txn, scope, self.cap_mode)
            .await
            .map_err(map_cap_apply_err)?;

        // 2. Persist the refund record row (surrogate PK + natural UNIQUE). A
        //    duplicate (replay) is short-circuited by the
        //    (tenant, REFUND, psp_refund_id:phase) idempotency claim BEFORE the
        //    sidecar, so an unexpected collision rolls the post back.
        AdjustmentRepo::insert_refund(txn, scope, &self.refund_row)
            .await
            .map_err(|e| DomainError::Internal(format!("insert refund: {e}")))?;

        // 3. Publish `billing.ledger.refund.recorded` into the SAME post txn
        //    (transactional outbox, Group G): the event commits atomically with the
        //    entry + caps + record, or a publish failure rolls the whole post back.
        //    Never on replay (a replay returns before the sidecar). Ids + amount +
        //    enum codes only (no PII). The clearing_state stamped on the `refund`
        //    row is the event's clearing_state.
        self.publisher
            .publish_refund_recorded(
                &self.ctx,
                txn,
                refund_recorded_event(&self.refund_row, posted),
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish refund_recorded: {e}")))?;
        Ok(())
    }
}

/// Build the `billing.ledger.refund.recorded` event payload from the `refund` row
/// being persisted + the posted entry facts (the in-txn `PostedFacts` surfaces the
/// entry id). PII-free by construction (ids + enum codes + amount only). Shared by
/// the forward/reversal sidecar ([`RefundPostSidecar`]) and the `unknown_final`
/// disposition sidecar ([`UnknownFinalSidecar`]).
fn refund_recorded_event(row: &NewRefund, posted: &PostedFacts) -> RefundRecorded {
    RefundRecorded {
        tenant_id: row.tenant_id,
        refund_id: row.refund_id.clone(),
        psp_refund_id: row.psp_refund_id.clone(),
        entry_id: posted.entry_id,
        phase: row.phase.clone(),
        pattern: row.pattern.clone(),
        payment_id: row.payment_id.clone(),
        amount_minor: row.amount_minor,
        currency: row.currency.clone(),
        clearing_state: row.clearing_state.clone(),
    }
}

/// Map a [`CapApplyError`] into the sidecar's [`DomainError`]. An
/// `UnderflowDeferred` (a claw-back whose decrement would underflow) becomes
/// [`DomainError::RefundClawbackDeferred`] — rolling the post back so the handler
/// can DEFER the claw-back to the queue (Group E, never hard-fail). A `Repo` error
/// is refined by [`map_refund_cap_err`] (over-refund cap CHECK → the
/// `RefundExceeds*` domain errors; everything else → infra).
fn map_cap_apply_err(e: CapApplyError) -> DomainError {
    match e {
        CapApplyError::UnderflowDeferred => DomainError::RefundClawbackDeferred(
            "claw-back money-out decrement would underflow (out-of-order / over-claw); deferred"
                .to_owned(),
        ),
        CapApplyError::Repo(repo) => map_refund_cap_err(repo),
    }
}

/// Map a refund cap-counter [`RepoError`] into the sidecar's [`DomainError`]: a cap
/// CHECK violation becomes [`DomainError::RefundExceedsSettled`] (the
/// `payment_settlement` total-money-out / spendable-headroom caps) or, when the
/// violated constraint is the per-`(payment, invoice)` `chk_par_*` cap,
/// [`DomainError::RefundExceedsAllocated`]. Every other repo failure is an
/// infrastructure fault that rolls the post back.
///
/// Both cap families surface as the same [`RepoError::MoneyOutCapExceeded`] (the
/// repo's `is_check_violation` matches both the `chk_payment_settlement_*` and
/// `chk_par_*` prefixes), so the message carries the discriminating constraint
/// context the repo stamped; a `chk_par_` / "allocated" marker routes to
/// `RefundExceedsAllocated`, everything else to `RefundExceedsSettled` (the
/// settled-amount caps are the common case + the safe default — both are
/// over-refund rejects on the `InvalidArgument` category).
fn map_refund_cap_err(e: RepoError) -> DomainError {
    match e {
        RepoError::MoneyOutCapExceeded(m) => {
            // The per-invoice cap is the `payment_allocation_refund` row
            // (`chk_par_refunded_le_allocated`); its repo context stamps the
            // "allocation_refund" marker. Everything else is a settlement cap.
            if m.contains("allocation_refund") || m.contains("chk_par_") {
                DomainError::RefundExceedsAllocated(m)
            } else {
                DomainError::RefundExceedsSettled(m)
            }
        }
        other => DomainError::Internal(format!("refund cap sidecar: {other}")),
    }
}

#[cfg(test)]
#[path = "refund_service_tests.rs"]
mod tests;
