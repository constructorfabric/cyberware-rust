//! `ChargebackService` — orchestrates the chargeback dispute domain
//! (`crate::domain::payment::chargeback`) over the foundation engine. Records
//! `opened` (Group B) and the `won`/`lost` outcomes (Group C) in both variants:
//! it seeds / advances the `ledger_dispute` current-state row and posts the
//! variant's legs in one serializable transaction (via [`ChargebackSidecar`]).
//!
//! Sequence for one phase (mirrors
//! [`crate::infra::payment::settlement_return::SettlementReturnService`] plus a
//! dispute pre-read + the net cash-leg read):
//! 1. **dedup short-circuit** — a replayed phase returns the prior entry BEFORE
//!    the state-dependent transition guard (which would spuriously reject a
//!    replayed `opened`).
//! 2. **read the dispute** — out-of-txn, scoped. Drives the variant selection,
//!    the transition guard, and the out-of-order decision.
//! 3. **out-of-order** (`won`/`lost` with NO prior dispute row, §4.7) — durably
//!    ENQUEUE the request on `ledger_pending_event_queue` (`flow = CHARGEBACK`,
//!    `business_id = dispute_id:cycle:phase`) and return [`ChargebackOutcome::Queued`]
//!    (HTTP 202), NEVER a partial outcome and NEVER a synthesised `opened`.
//! 4. **variant + transition guard** — `opened` derives the variant from
//!    `funds_at_open`; `won`/`lost` read the recorded variant back from the row.
//! 5. **net cash-leg read** (`CASH_HOLD` only, Model N) — read the settlement and
//!    size the cash legs at `net = settled_minor − fee_minor` (the cash that
//!    actually entered `CASH_CLEARING`; the PSP fee went to `PSP_FEE_EXPENSE`).
//!    An `AR_RECLASS` dispute has no PSP fee / no cash leg, so `net` is moot (`0`).
//! 6. **`CHARGEBACK_ON_REFUNDED` pre-check** — a `lost` whose clawback cannot fit
//!    under the total money-out cap because the payment was already refunded
//!    routes to a minimal exception stub (logged + a distinct `DomainError`;
//!    the full exception queue is Slice 7 / VHP-1859).
//! 7. **build → overwrite header → bind chart → post** with [`ChargebackSidecar`]
//!    (the dispute-row write + the clawback counter bump + the in-txn
//!    `dispute.recorded` outbox publish, all atomic with the entry).
//! 8. **drain-on-`opened`** — when a FRESH `opened` commits, [`Self::record_phase`]
//!    drains the tenant's CHARGEBACK queue inline (mirrors settle's drain-on-settle)
//!    so a queued `won`/`lost` whose `opened` just landed posts immediately; the
//!    periodic [`crate::infra::jobs::queue_applier`] sweep is the backstop.
//!
//! There is NO payer gate (a dispute records a card-network / bank event and must
//! land even for a closed payer). Idempotent on
//! `(tenant, CHARGEBACK, "dispute_id:cycle:phase")`. Lives in `infra` (needs repo
//! + posting access); the domain builder it calls stays pure (dylint DE0301).

use std::sync::Arc;
use std::time::Instant;

use bss_ledger_sdk::{AccountClass, PostEntry, PostLine, PostingRef, SourceDocType};
use chrono::Datelike;
use sea_orm::DbErr;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::fx::realized::carried_relief;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::payment::chargeback::{
    ChargebackInput, DisputePhase, DisputeVariant, FundsAtOpen, build_chargeback_entry,
    clawed_back_on_post,
};
use crate::domain::ports::metrics::{LedgerMetricsPort, PostFlow, PostResult};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::exception::ExceptionRouter;
use crate::infra::payment::sidecar::{ChargebackDisputeOp, ChargebackSidecar};
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::idempotency::{
    ClaimOutcome, IdempotencyGate, STATUS_POSTED, STATUS_QUEUED,
};
use crate::infra::posting::service::{PostSidecar, PostingService};
use crate::infra::storage::entity::pending_event_queue;
use crate::infra::storage::repo::{
    DisputeRepo, NewQueueRow, PaymentRepo, PendingQueueRepo, ReferenceRepo,
};
use time::Duration;
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// The deferred-apply queue flow + idempotency-dedup `flow` for a chargeback
/// phase — the `CHARGEBACK` source-doc literal (kept in lockstep with
/// [`SourceDocType::Chargeback`] by the round-trip test in `enums.rs`; `as_str`
/// is not `const`, so it can't be derived in a `const` initializer). The same
/// literal the inline post stamps on its entry header, so a queued phase and the
/// post it later becomes share one dedup key. Reuses the unconstrained
/// `flow varchar(64)` (G-B2) — no new flow literal, no DDL.
const FLOW_CHARGEBACK: &str = "CHARGEBACK";

/// Per-tenant cap on the drain-on-`opened` pass (mirrors settle's
/// `DRAIN_ON_SETTLE_CAP`): a sane batch ceiling so an `opened` that unblocks a
/// large backlog of out-of-order `won`/`lost` phases doesn't post an unbounded
/// number of outcomes inline on the record path. The periodic
/// [`crate::infra::jobs::queue_applier`] sweep drains the remainder.
const DRAIN_ON_OPENED_CAP: u64 = 100;

/// A chargeback phase to record (the infra request the local client / REST
/// surface lowers their DTOs into). The variant is NOT supplied — the service
/// selects it at `opened` from `funds_at_open` and reads it back from the dispute
/// row for the outcomes.
pub struct ChargebackRequest {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub payment_id: String,
    pub dispute_id: String,
    /// The disputed `(payer, invoice)` AR grain — required for an AR-reclass
    /// `opened` / `won` / `lost`; ignored for cash-hold.
    pub invoice_id: Option<String>,
    /// Re-entrancy counter (`>= 1`).
    pub cycle: i32,
    pub phase: DisputePhase,
    /// The funds-movement fact (card rails withheld vs invoice/ACH not moved) —
    /// the LEDGER reads it at `opened` to choose the variant.
    pub funds_at_open: FundsAtOpen,
    pub disputed_amount_minor: i64,
    pub currency: String,
    pub effective_at: Option<OffsetDateTime>,
}

impl ChargebackRequest {
    /// The `source_business_id` / idempotency composite for this phase:
    /// `dispute_id:cycle:phase` (matches the domain builder's `business_id`).
    fn business_id(&self) -> String {
        format!("{}:{}:{}", self.dispute_id, self.cycle, self.phase.as_str())
    }
}

/// The result of recording one phase: either it posted inline (the dispute had
/// its `opened`, or this IS the `opened`) or it was durably queued because its
/// `opened` has not landed yet (§4.7 out-of-order). The two arms drive the
/// SDK/REST 201/200-vs-202 split (mirrors
/// [`crate::infra::payment::allocate::AllocationOutcome`]).
#[derive(Debug)]
pub enum ChargebackOutcome {
    /// The phase posted inline: the posting handle.
    Recorded(PostingRef),
    /// The phase was an out-of-order `won`/`lost`: the request was enqueued
    /// (HTTP 202).
    Queued(QueuedDispute),
}

/// A dispute phase deferred because its `opened` has not landed: the request is
/// durably on `ledger_pending_event_queue` and the drain will apply it once the
/// `opened` lands. Carries the queue key (`flow` + `business_id`) and the
/// `queued_at` instant — the surface for the REST 202 `dispute-phase-queued`
/// body. No `PostingRef`: nothing has posted yet (mirrors
/// [`crate::infra::payment::allocate::QueuedAllocation`]).
#[derive(Debug)]
pub struct QueuedDispute {
    /// The deferred-apply queue flow (the `CHARGEBACK` literal).
    pub flow: String,
    /// The queue/dedup business id — `dispute_id:cycle:phase`.
    pub business_id: String,
    /// When the intake durably enqueued the request.
    pub queued_at: OffsetDateTime,
}

/// The financial-key snapshot of a chargeback phase request, persisted as the
/// queue row's `payload` jsonb at intake and re-read by the drain. PII-free by
/// construction (ids + money + enum literals only — no names / free-text). The
/// enums are carried as their stable wire literals so the domain stays serde-free
/// (pure); the drain parses them back. Lives here (not `domain`) next to the
/// service that writes it.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedDisputePayload {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub payment_id: String,
    pub dispute_id: String,
    pub invoice_id: Option<String>,
    pub cycle: i32,
    /// The phase wire literal (`WON` / `LOST` — an out-of-order intake only ever
    /// queues these; `OPENED` posts inline, never queues).
    pub phase: String,
    /// The funds-fact wire literal (`withheld` / `not_moved`). Recorded for
    /// completeness; on apply the variant is read from the now-present dispute row.
    pub funds_at_open: String,
    pub disputed_amount_minor: i64,
    pub currency: String,
}

impl QueuedDisputePayload {
    /// Snapshot a [`ChargebackRequest`] into the PII-free queue payload (by
    /// reference — the request is still needed to build the `Queued` handle).
    fn from_request(req: &ChargebackRequest) -> Self {
        Self {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id.clone(),
            dispute_id: req.dispute_id.clone(),
            invoice_id: req.invoice_id.clone(),
            cycle: req.cycle,
            phase: req.phase.as_str().to_owned(),
            funds_at_open: req.funds_at_open.as_str().to_owned(),
            disputed_amount_minor: req.disputed_amount_minor,
            currency: req.currency.clone(),
        }
    }

    /// Reconstruct the request from a queued payload at apply time, parsing the
    /// wire enum literals. The inverse of [`Self::from_request`].
    ///
    /// # Errors
    /// [`DomainError::Internal`] when a stored enum literal is unknown (data
    /// corruption — the column is only ever written from `as_str`).
    fn into_request(self) -> Result<ChargebackRequest, DomainError> {
        let phase = DisputePhase::parse(&self.phase).ok_or_else(|| {
            DomainError::Internal(format!(
                "queued dispute carries unknown phase {:?}",
                self.phase
            ))
        })?;
        let funds_at_open = FundsAtOpen::parse(&self.funds_at_open).ok_or_else(|| {
            DomainError::Internal(format!(
                "queued dispute carries unknown funds_at_open {:?}",
                self.funds_at_open
            ))
        })?;
        Ok(ChargebackRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            payment_id: self.payment_id,
            dispute_id: self.dispute_id,
            invoice_id: self.invoice_id,
            cycle: self.cycle,
            phase,
            funds_at_open,
            disputed_amount_minor: self.disputed_amount_minor,
            currency: self.currency,
            // A queued phase is applied at drain time; the period is stamped then
            // (no original instant is carried on the queue payload), mirroring how
            // a queued allocation re-derives at apply time.
            effective_at: None,
        })
    }
}

/// What the intake transaction committed — carried out of the `db.transaction`
/// closure so the post-txn code can build the `Queued` handle. Mirrors
/// [`crate::infra::payment::allocate`]'s `IntakeOutcome`.
enum IntakeOutcome {
    Enqueued {
        queued_at: OffsetDateTime,
    },
    AlreadyQueued,
    /// A concurrent/retried intake reused the same key with a DIFFERENT payload
    /// (the dedup row's `payload_hash` differs) — surfaced after the txn as
    /// [`DomainError::IdempotencyConflict`].
    Conflict,
}

/// The result of applying ONE queued dispute row ([`ChargebackService::apply_queued_row`]).
/// Distinct from [`ChargebackOutcome`] because the apply has a terminal shape the
/// intake cannot: the `opened` may still be absent.
#[derive(Debug)]
pub enum ApplyOutcome {
    /// The `opened` landed and the queued outcome posted — the queue row was
    /// flipped `→APPLIED` atomically in the post txn.
    Applied(PostingRef),
    /// The dispute's `opened` is STILL absent: leave the row `QUEUED`, do NOT
    /// bump attempts (an `opened` post will retry the drain).
    NotReady,
    /// A guard / cap rejected the apply at apply-time (re-evaluated against
    /// then-current state). The caller bumps `attempts` and leaves the row
    /// `QUEUED`. Carries the rejection for logging.
    Blocked(DomainError),
}

/// Summary of one [`ChargebackService::drain`] pass over a tenant's queued
/// dispute phases (mirrors [`crate::infra::payment::allocate::DrainReport`]).
#[derive(Debug, Default)]
pub struct DrainReport {
    pub applied: u64,
    pub not_ready: u64,
    pub blocked: u64,
}

/// Orchestrates the chargeback dispute domain over the foundation engine.
pub struct ChargebackService {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    dispute_repo: DisputeRepo,
    // The payment counter repo: the out-of-txn dedup short-circuit
    // (`lookup_finalized_post`) AND the net cash-leg read (`settled − fee`, Model
    // N) + the `CHARGEBACK_ON_REFUNDED` settlement pre-read.
    payment_repo: PaymentRepo,
    // The deferred-apply queue (work-state SoT): an out-of-order `won`/`lost`
    // (its `opened` not yet landed) is enqueued here at intake (§4.7) and drained
    // later (on an `opened` / by the periodic sweep).
    pending_queue: PendingQueueRepo,
    // One database provider, retained so the intake enqueue + the drain claim can
    // open their own `db.transaction` (mirrors `AllocationService`).
    db: DBProvider<DbError>,
    // The event publisher — threaded into the posting engine AND held so the
    // sidecar can publish `dispute.recorded` in-txn.
    publisher: Arc<LedgerEventPublisher>,
    metrics: Arc<dyn LedgerMetricsPort>,
    // Slice 7 Phase 2: routes the `CHARGEBACK_ON_REFUNDED` stub to a durable
    // close-blocking exception row (ADDITIVE beside the rejection). `None` until
    // `with_exceptions` wires it (so existing constructions are unchanged).
    exceptions: Option<Arc<ExceptionRouter>>,
}

impl ChargebackService {
    /// Build the service over one database provider, the event publisher
    /// (threaded into the posting engine + the sidecar's in-txn publish), and the
    /// metrics sink. Mirrors
    /// [`crate::infra::payment::settlement_return::SettlementReturnService::new`]
    /// plus the queue repo + the retained `db`/`publisher` the out-of-order path
    /// needs.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let dispute_repo = DisputeRepo::new(db.clone());
        let payment_repo = PaymentRepo::new(db.clone());
        let pending_queue = PendingQueueRepo::new(db.clone());
        Self {
            posting,
            reference,
            resolver,
            dispute_repo,
            payment_repo,
            pending_queue,
            db,
            publisher,
            metrics,
            exceptions: None,
        }
    }

    /// Attach the exception router (Slice 7 Phase 2) so a `CHARGEBACK_ON_REFUNDED`
    /// rejection also opens a durable close-blocking exception row. Additive — the
    /// rejection + the log are unchanged.
    #[must_use]
    pub fn with_exceptions(mut self, exceptions: Arc<ExceptionRouter>) -> Self {
        self.exceptions = Some(exceptions);
        self
    }

    /// Record one dispute phase. `opened` selects the variant from
    /// `funds_at_open`, guards the transition, builds the variant's `opened`
    /// legs, and posts them while seeding the `ledger_dispute` row. `won`/`lost`
    /// read the recorded variant back, read the settlement's `net` (`CASH_HOLD`,
    /// for the net-sized cash legs — Model N), build the outcome legs, and post
    /// them while advancing the row (and, on a `lost` cash-hold, bumping
    /// `clawed_back_minor` by `net`). A `won`/`lost` with NO prior `opened` is
    /// durably QUEUED (§4.7), never rejected. Idempotent on
    /// `(tenant, CHARGEBACK, "dispute_id:cycle:phase")`.
    ///
    /// On an inline post emits `chargeback(Posted | Replayed)` + the payment-post
    /// duration; every rejection emits `chargeback(Rejected)` + the duration. A
    /// queue (enqueue) records ONLY the duration (the post — and its outcome —
    /// happens later on the drain).
    ///
    /// # Errors
    /// [`DomainError::InvalidDisputeTransition`] when the phase is not a legal
    /// transition (`opened` on a still-open dispute; `partial`);
    /// [`DomainError::InvalidRequest`] for a non-positive amount or a missing
    /// AR-reclass `invoice_id`; [`DomainError::ChargebackExceedsSettled`] when a
    /// `lost` clawback exceeds the settled amount; [`DomainError::ChargebackOnRefunded`]
    /// when a `lost` lands on an already-refunded payment whose clawback can't
    /// fit; [`DomainError::AccountClosed`] when a required class is not
    /// provisioned; any foundation rejection or [`DomainError::Internal`].
    pub async fn record_phase(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: ChargebackRequest,
    ) -> Result<ChargebackOutcome, DomainError> {
        let started = Instant::now();
        // Captured before `req` moves into `record_inner` — the post-commit drain
        // hook below gates on the phase (only an `opened` unblocks queued outcomes)
        // and needs the tenant to scope the drain.
        let tenant = req.tenant_id;
        let phase = req.phase;
        let result = self.record_inner(ctx, scope, req).await;
        self.record(&result, started);

        // Drain-on-`opened` (mirrors settle's drain-on-settle, D3). An out-of-order
        // `won`/`lost` whose `opened` had not landed was durably QUEUED at intake
        // (§4.7); a freshly-recorded `opened` is exactly the event that unblocks it.
        // Once this `opened` COMMITS, drain the tenant's CHARGEBACK queue inline so a
        // previously-`NotReady` outcome posts immediately rather than waiting for the
        // periodic sweep. Gated on a FRESH inline `opened`:
        //   - `Opened` only — a `won`/`lost` resolves a dispute, it never unblocks a
        //     queued phase, so re-draining on those is pure waste.
        //   - `Recorded` and not `replayed` — a `Queued`/rejected outcome posted
        //     nothing (nothing to unblock), and an idempotent replay's ORIGINAL post
        //     already ran this drain (re-draining on every retried `opened` re-drives
        //     any apply-blocked rows on each retry).
        // A drain error MUST NOT fail the record — log + swallow; the periodic
        // `QueueApplierJob` is the backstop. The drain re-reads the dispute per row,
        // so it is safe even though the just-committed `opened` is now visible
        // out-of-txn.
        if matches!(phase, DisputePhase::Opened)
            && matches!(&result, Ok(ChargebackOutcome::Recorded(posting)) if !posting.replayed)
        {
            match self.drain(ctx, scope, tenant, DRAIN_ON_OPENED_CAP).await {
                Ok(report) => {
                    if report.applied > 0 || report.blocked > 0 {
                        tracing::info!(
                            tenant_id = %tenant,
                            applied = report.applied,
                            not_ready = report.not_ready,
                            blocked = report.blocked,
                            "bss-ledger: drain-on-opened applied queued dispute phases"
                        );
                    }
                }
                Err(e) => tracing::error!(
                    tenant_id = %tenant,
                    error = %e,
                    "bss-ledger: drain-on-opened failed (swallowed; sweep will retry)"
                ),
            }
        }

        result
    }

    /// Build + post the chargeback entry (no metrics — the public wrapper records
    /// them).
    async fn record_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: ChargebackRequest,
    ) -> Result<ChargebackOutcome, DomainError> {
        // 0. Early dedup short-circuit (BEFORE the dispute read + transition
        //    guard), mirroring the allocate/credit short-circuit. The transition
        //    guard below is state-dependent, so a replayed `opened` would
        //    otherwise be spuriously rejected (its row is already `OPENED`). A
        //    `POSTED` row returns the prior entry as a `Recorded` replay; a
        //    `QUEUED` row (an out-of-order phase already enqueued) returns the
        //    same `Queued` handle. Racy by nature (the authoritative dedup is the
        //    engine's in-txn claim / the intake's `claim_queued`).
        let business_id = req.business_id();
        if let Some(outcome) = self.replay_short_circuit(scope, &req, &business_id).await? {
            return Ok(outcome);
        }

        // 1. Read the dispute's current state (out-of-txn, scoped). Drives the
        //    variant selection + transition guard + the out-of-order decision.
        let existing = self
            .dispute_repo
            .read_dispute(scope, req.tenant_id, &req.dispute_id)
            .await?;

        // 2. Out-of-order (§4.7): a `won`/`lost` whose dispute has NO prior row
        //    (no `opened` landed yet) MUST NOT post a partial outcome — enqueue it
        //    and return 202. `opened` (and `partial`) never take this path.
        if existing.is_none() && matches!(req.phase, DisputePhase::Won | DisputePhase::Lost) {
            let queued = self.enqueue_phase(scope, &req, &business_id).await?;
            return Ok(ChargebackOutcome::Queued(queued));
        }

        // 3.–8. Decide the variant, read cash, build, and post inline.
        let posting = self
            .post_phase_inline(ctx, scope, &req, existing.as_ref())
            .await?;
        Ok(ChargebackOutcome::Recorded(posting))
    }

    /// The inline post path (steps 3–7): variant selection + transition guard,
    /// the net cash-leg read (`CASH_HOLD`, Model N), the `CHARGEBACK_ON_REFUNDED`
    /// pre-check, build, bind, and post with the sidecar. Shared by `record_inner`
    /// and the deferred-apply path ([`Self::apply_queued_row`]) — so the guard +
    /// caps are re-evaluated against THEN-CURRENT state every time this runs.
    async fn post_phase_inline(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &ChargebackRequest,
        existing: Option<&crate::infra::storage::entity::dispute::Model>,
    ) -> Result<PostingRef, DomainError> {
        // 3. Variant selection + transition guard.
        let variant = match req.phase {
            DisputePhase::Opened => {
                guard_open_transition(existing, &req.dispute_id)?;
                req.funds_at_open.variant()
            }
            DisputePhase::Won | DisputePhase::Lost => {
                let row = existing.ok_or_else(|| {
                    DomainError::InvalidDisputeTransition(format!(
                        "dispute {} has no opened cycle to resolve",
                        req.dispute_id
                    ))
                })?;
                guard_outcome_transition(row, &req.dispute_id)?;
                DisputeVariant::parse(&row.variant).ok_or_else(|| {
                    DomainError::Internal(format!(
                        "ledger_dispute {} carries an unknown variant {:?}",
                        req.dispute_id, row.variant
                    ))
                })?
            }
            DisputePhase::Partial => {
                return Err(DomainError::InvalidDisputeTransition(format!(
                    "dispute phase {} is behind a flag (split chargeback) and not implemented",
                    req.phase.as_str()
                )));
            }
        };

        let input = ChargebackInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id.clone(),
            dispute_id: req.dispute_id.clone(),
            cycle: req.cycle,
            phase: req.phase,
            variant,
            disputed_amount_minor: req.disputed_amount_minor,
            invoice_id: req.invoice_id.clone(),
            currency: req.currency.clone(),
            effective_at: req.effective_at,
        };

        // 4. Net cash-leg size (Model N). A `CASH_HOLD` dispute's cash legs are
        //    sized at `net = settled_minor − fee_minor` (`CASH_CLEARING` only ever
        //    held net — the PSP fee never entered it), but WHICH net depends on
        //    the phase:
        //    - `opened` reads the CURRENT net and parks `min(disputed, net)` in
        //      the hold (the sidecar records that amount on the dispute row);
        //    - `won`/`lost` size their release / forfeit off the amount STORED at
        //      open (`existing.cash_hold_minor`), NOT a re-read `settled − fee`.
        //      A settlement-return that lowers the payment's net between `opened`
        //      and the outcome must not change what the hold gives back — else the
        //      outcome would release less than was held and strand `DISPUTE_HOLD`
        //      non-zero (the held cash is a fact fixed at open, not re-derived).
        //    An `AR_RECLASS` dispute has no PSP fee and no cash leg, so `net` is
        //    irrelevant — pass `0`.
        let chart = load_chart(&self.reference, scope, input.tenant_id).await?;
        let net = match (input.variant, req.phase) {
            (DisputeVariant::CashHold, DisputePhase::Opened) => {
                self.read_net(scope, &input).await?
            }
            (DisputeVariant::CashHold, DisputePhase::Won | DisputePhase::Lost) => {
                // `existing` is `Some` on any outcome (the transition guard above
                // rejects a missing opened cycle); fall back to `0` defensively.
                existing.map_or(0, |row| row.cash_hold_minor)
            }
            // AR_RECLASS (any phase) has no cash leg; `partial` is rejected above.
            (DisputeVariant::ArReclass, _) | (_, DisputePhase::Partial) => 0,
        };
        // The cash parked in `DISPUTE_HOLD` at open (`min(disputed, net)`, Model
        // N) — persisted by the sidecar's `Open` write so the outcome branch can
        // size off it. On an outcome `net` already IS the stored hold, so the
        // `min` is a no-op there; only the `opened` write records a fresh value.
        let cash_hold_minor = net.min(input.disputed_amount_minor);

        // 5. `CHARGEBACK_ON_REFUNDED` pre-check: a `lost` cash-out whose clawback
        //    cannot fit under the total money-out cap because the payment was
        //    already refunded routes to a minimal exception stub (logged + a
        //    distinct error; the full exception queue is Slice 7 / VHP-1859). The
        //    cap CHECK is the authoritative backstop; this pre-check turns the
        //    specific already-refunded case into its own signal rather than a
        //    generic `ChargebackExceedsSettled`.
        let clawed_back = clawed_back_on_post(&input, net);
        if clawed_back > 0 {
            self.guard_not_on_refunded(scope, &input, clawed_back)
                .await?;
        }

        // 6. Build the balanced entry (validates amount > 0 + the AR-reclass
        //    invoice_id; rejects partial), then overwrite the placeholder header.
        let mut entry = build_chargeback_entry(&input, net)?;
        overwrite_header(&mut entry, ctx, req.effective_at);

        // 7. Bind each line's real chart account_id.
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

        // 7b. Slice 5 (F3): functional carry-forward on a cross-currency dispute
        //     close. A chargeback reclassifies a position at the CARRIED rate — it
        //     locks no new rate, so the functional cost basis carries forward and
        //     the entry's functional column nets to zero (NO realized FX; that is
        //     recognised at the cash in/out points — settle S2 / refund S3). The
        //     stamp keeps the closing grain's functional column in lockstep with
        //     balance_minor; a single-currency close is a no-op (functional NULL).
        self.apply_fx_carry_forward(scope, &input, &mut entry)
            .await?;

        // 8. Post, threading the chargeback sidecar (the dispute-row write — open
        //    vs advance — + the clawback bump + the in-txn `dispute.recorded`
        //    publish, all atomic with the entry).
        let op = match req.phase {
            DisputePhase::Won | DisputePhase::Lost => ChargebackDisputeOp::Advance {
                last_phase: req.phase,
                clawed_back_minor: clawed_back,
            },
            // `opened` seeds/re-opens the row; `partial` is rejected earlier in the
            // builder and is mapped defensively to Open so a future flag-flip can't
            // silently mis-advance.
            DisputePhase::Opened | DisputePhase::Partial => ChargebackDisputeOp::Open,
        };
        let sidecar: Arc<dyn PostSidecar> = Arc::new(ChargebackSidecar {
            tenant: req.tenant_id,
            dispute_id: req.dispute_id.clone(),
            payment_id: req.payment_id.clone(),
            currency: req.currency.clone(),
            variant,
            cycle: req.cycle,
            disputed_amount_minor: req.disputed_amount_minor,
            cash_hold_minor,
            op,
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
        });
        let posting = self.post_bound(ctx, scope, entry, sidecar).await?;
        Ok(posting)
    }

    /// Early dedup short-circuit for a dispute-phase replay, reading the
    /// `(tenant, CHARGEBACK, business_id)` dedup status ONCE:
    /// - `POSTED` ⇒ the phase already posted (inline OR queued-then-drained):
    ///   return a [`ChargebackOutcome::Recorded`] replay.
    /// - `QUEUED` ⇒ an out-of-order phase already enqueued: read the queue row's
    ///   `queued_at` and return the same [`ChargebackOutcome::Queued`] handle.
    /// - `CLAIMED` / absent ⇒ `None`: fall through to the dispute read.
    ///
    /// Runs out-of-txn (racy by nature); the authoritative dedup is the engine's
    /// in-txn claim (inline) or the intake's `claim_queued`.
    async fn replay_short_circuit(
        &self,
        scope: &AccessScope,
        req: &ChargebackRequest,
        business_id: &str,
    ) -> Result<Option<ChargebackOutcome>, DomainError> {
        let dedup = self
            .payment_repo
            .lookup_dedup_status(scope, req.tenant_id, SourceDocType::Chargeback, business_id)
            .await
            .map_err(|e| DomainError::Internal(format!("chargeback dedup lookup: {e}")))?;
        let Some((status, result_entry_id, _hash)) = dedup else {
            return Ok(None);
        };
        if status == STATUS_POSTED {
            let entry_id = result_entry_id.ok_or_else(|| {
                DomainError::Internal(format!(
                    "dedup POSTED but no result_entry_id for \
                     ({}, {FLOW_CHARGEBACK}, {business_id})",
                    req.tenant_id
                ))
            })?;
            return Ok(Some(ChargebackOutcome::Recorded(PostingRef {
                entry_id,
                created_seq: 0,
                replayed: true,
            })));
        }
        if status != STATUS_QUEUED {
            // CLAIMED (an in-flight inline post): not a replay — fall through.
            return Ok(None);
        }
        // QUEUED: surface `queued_at` from the work-state queue row.
        let row = self
            .pending_queue
            .get(scope, req.tenant_id, FLOW_CHARGEBACK, business_id)
            .await
            .map_err(|e| DomainError::Internal(format!("chargeback queue read: {e}")))?
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "dedup QUEUED but no queue row for ({}, {FLOW_CHARGEBACK}, {business_id})",
                    req.tenant_id
                ))
            })?;
        Ok(Some(ChargebackOutcome::Queued(QueuedDispute {
            flow: FLOW_CHARGEBACK.to_owned(),
            business_id: business_id.to_owned(),
            queued_at: row.queued_at,
        })))
    }

    /// Intake for an out-of-order `won`/`lost` (its `opened` not yet landed,
    /// §4.7): claim the dedup row as `QUEUED` and insert the work-state queue row,
    /// in ONE `db.transaction` (mirrors
    /// [`crate::infra::payment::allocate::AllocationService::enqueue_allocation`]).
    /// The dedup `payload_hash` is request-based (`content_hash` over the
    /// canonical [`QueuedDisputePayload`]) — the same hash the inline post would
    /// store is entry-derived, but a queued phase never inlines under the same
    /// key, so request-based is the stable choice and `claim_queued`'s `Replay`
    /// makes the intake idempotent.
    async fn enqueue_phase(
        &self,
        scope: &AccessScope,
        req: &ChargebackRequest,
        business_id: &str,
    ) -> Result<QueuedDispute, DomainError> {
        let now = OffsetDateTime::now_utc();
        let payload = QueuedDisputePayload::from_request(req);
        let payload_json = serde_json::to_value(&payload)
            .map_err(|e| DomainError::Internal(format!("serialize queue payload: {e}")))?;
        let payload_hash = dispute_request_hash(&payload)?;

        let tenant = req.tenant_id;
        let gate = IdempotencyGate::new();
        // Own everything the closure needs (it is `FnOnce`, so the captures move
        // straight into the async future — no inner re-clone). Mirrors
        // `AllocationService::enqueue_allocation`.
        let scope_owned = scope.clone();
        let business_id_owned = business_id.to_owned();
        let outcome = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    let claim = gate
                        .claim_queued(
                            txn,
                            tenant,
                            FLOW_CHARGEBACK,
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
                                    flow: FLOW_CHARGEBACK.to_owned(),
                                    business_id: business_id_owned.clone(),
                                    payload: payload_json,
                                    queued_at: now,
                                    // Immediately eligible once the `opened` lands.
                                    apply_after: None,
                                },
                            )
                            .await
                            .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                            Ok::<IntakeOutcome, DbError>(IntakeOutcome::Enqueued { queued_at: now })
                        }
                        ClaimOutcome::Replay(row) => {
                            if row.payload_hash != payload_hash {
                                Ok(IntakeOutcome::Conflict)
                            } else if row.status == STATUS_QUEUED {
                                Ok(IntakeOutcome::AlreadyQueued)
                            } else {
                                // Same payload but a POSTED race (the `opened`
                                // landed AND the drain applied it between the early
                                // check and this claim) — transient; the caller
                                // retries and the early check returns the POSTED
                                // replay cleanly.
                                Err(DbError::Sea(DbErr::Custom(format!(
                                    "chargeback intake: unexpected dedup status {:?} for \
                                     ({tenant}, {FLOW_CHARGEBACK}, {business_id_owned})",
                                    row.status
                                ))))
                            }
                        }
                    }
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("chargeback intake: {e}")))?;

        match outcome {
            IntakeOutcome::Enqueued { queued_at } => Ok(QueuedDispute {
                flow: FLOW_CHARGEBACK.to_owned(),
                business_id: business_id.to_owned(),
                queued_at,
            }),
            IntakeOutcome::Conflict => Err(DomainError::IdempotencyConflict(format!(
                "dispute phase {business_id} reused with a different payload"
            ))),
            IntakeOutcome::AlreadyQueued => {
                let row = self
                    .pending_queue
                    .get(scope, req.tenant_id, FLOW_CHARGEBACK, business_id)
                    .await
                    .map_err(|e| DomainError::Internal(format!("chargeback queue read: {e}")))?
                    .ok_or_else(|| {
                        DomainError::Internal(format!(
                            "intake replay but no queue row for \
                             ({}, {FLOW_CHARGEBACK}, {business_id})",
                            req.tenant_id
                        ))
                    })?;
                Ok(QueuedDispute {
                    flow: FLOW_CHARGEBACK.to_owned(),
                    business_id: business_id.to_owned(),
                    queued_at: row.queued_at,
                })
            }
        }
    }

    /// Apply ONE queued dispute row (the drain): deserialize the payload, re-read
    /// the dispute state, and — if its `opened` has now landed — RE-RUN the
    /// inline post path (guard + caps re-evaluated against then-current state)
    /// via [`PostingService::post_queued_apply`] with a COMPOSITE sidecar that
    /// does the dispute writes AND flips the queue row `→APPLIED`, both in the
    /// post txn. Returns [`ApplyOutcome::NotReady`] when the `opened` is STILL
    /// absent (leave `QUEUED`, no attempt bump), [`ApplyOutcome::Applied`] on a
    /// successful post, or [`ApplyOutcome::Blocked`] on an apply-time guard/cap
    /// rejection (caller bumps attempts + leaves `QUEUED`).
    ///
    /// `pub(crate)` so the [`crate::infra::jobs::queue_applier`] sweep can drive a
    /// single row; the public surface is [`Self::drain`].
    ///
    /// # Errors
    /// [`DomainError::Internal`] on an infra fault (bad payload, dispute read, or
    /// an engine `Internal`) — propagated so the caller can isolate the row.
    pub(crate) async fn apply_queued_row(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        row: &pending_event_queue::Model,
    ) -> Result<ApplyOutcome, DomainError> {
        // 1. Deserialize the PII-free snapshot + reconstruct the request.
        let payload: QueuedDisputePayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| DomainError::Internal(format!("deserialize queued dispute: {e}")))?;
        let req = payload.into_request()?;

        // 2. Re-read the dispute. ABSENT ⇒ NotReady (the `opened` still hasn't
        //    landed — leave QUEUED, no attempt bump; an `opened` post retries).
        let existing = self
            .dispute_repo
            .read_dispute(scope, req.tenant_id, &req.dispute_id)
            .await?;
        if existing.is_none() {
            return Ok(ApplyOutcome::NotReady);
        }

        // 3. Re-run the inline build/guard/post, but via the queued-apply engine
        //    path with a COMPOSITE sidecar (dispute writes + queue `→APPLIED`
        //    flip). A guard/cap rejection ⇒ Blocked; an `Internal` propagates.
        match self
            .post_phase_queued_apply(ctx, scope, &req, existing.as_ref(), &row.business_id)
            .await
        {
            Ok(posting) => Ok(ApplyOutcome::Applied(posting)),
            Err(e) if is_apply_blocked(&e) => Ok(ApplyOutcome::Blocked(e)),
            Err(e) => Err(e),
        }
    }

    /// The deferred-apply twin of [`Self::post_phase_inline`]: same variant
    /// guard + net cash-leg read + build, but posts via
    /// [`PostingService::post_queued_apply`] with a COMPOSITE sidecar that wraps
    /// the [`ChargebackSidecar`] AND flips the queue row `→APPLIED` in the same
    /// txn. The dedup row claimed `QUEUED` at intake is read (not re-claimed) and
    /// finalized `QUEUED → POSTED`.
    async fn post_phase_queued_apply(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &ChargebackRequest,
        existing: Option<&crate::infra::storage::entity::dispute::Model>,
        business_id: &str,
    ) -> Result<PostingRef, DomainError> {
        // Guard + variant (re-evaluated at apply time).
        let variant = match req.phase {
            DisputePhase::Won | DisputePhase::Lost => {
                let row = existing.ok_or_else(|| {
                    DomainError::InvalidDisputeTransition(format!(
                        "dispute {} has no opened cycle to resolve",
                        req.dispute_id
                    ))
                })?;
                guard_outcome_transition(row, &req.dispute_id)?;
                DisputeVariant::parse(&row.variant).ok_or_else(|| {
                    DomainError::Internal(format!(
                        "ledger_dispute {} carries an unknown variant {:?}",
                        req.dispute_id, row.variant
                    ))
                })?
            }
            // Only `won`/`lost` are ever queued (intake guards this); anything
            // else here is an invariant breach.
            other => {
                return Err(DomainError::Internal(format!(
                    "queued dispute carries non-outcome phase {}",
                    other.as_str()
                )));
            }
        };

        let input = ChargebackInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id.clone(),
            dispute_id: req.dispute_id.clone(),
            cycle: req.cycle,
            phase: req.phase,
            variant,
            disputed_amount_minor: req.disputed_amount_minor,
            invoice_id: req.invoice_id.clone(),
            currency: req.currency.clone(),
            effective_at: req.effective_at,
        };

        let chart = load_chart(&self.reference, scope, input.tenant_id).await?;
        // Net cash-leg size (Model N). This path is outcome-only (won/lost), so a
        // CASH_HOLD sizes its release / forfeit off the amount STORED in the hold
        // at open (`existing.cash_hold_minor`), NOT a re-read `settled − fee` — a
        // settlement-return between `opened` and this deferred apply must not
        // change what the hold gives back (mirrors `post_phase_inline`; the fix
        // for the stranded-hold bug applies to the queued path too). AR_RECLASS
        // has no cash leg ⇒ `0`. `existing` is `Some` here (guarded above).
        let net = match input.variant {
            DisputeVariant::CashHold => existing.map_or(0, |row| row.cash_hold_minor),
            DisputeVariant::ArReclass => 0,
        };
        // The hold size for the sidecar row (no-op `min` on an outcome, where
        // `net` already is the stored hold); the `Advance` op does not rewrite it.
        let cash_hold_minor = net.min(input.disputed_amount_minor);
        let clawed_back = clawed_back_on_post(&input, net);
        if clawed_back > 0 {
            self.guard_not_on_refunded(scope, &input, clawed_back)
                .await?;
        }
        let mut entry = build_chargeback_entry(&input, net)?;
        overwrite_header(&mut entry, ctx, req.effective_at);
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
        // Slice 5 (F3): functional carry-forward on the deferred-apply path too —
        // a queued won/lost drains after its opened lands; same carried-rate
        // reclassification as `post_phase_inline` (see `apply_fx_carry_forward`).
        self.apply_fx_carry_forward(scope, &input, &mut entry)
            .await?;
        let sidecar: Arc<dyn PostSidecar> = Arc::new(QueuedChargebackApplySidecar {
            inner: ChargebackSidecar {
                tenant: req.tenant_id,
                dispute_id: req.dispute_id.clone(),
                payment_id: req.payment_id.clone(),
                currency: req.currency.clone(),
                variant,
                cycle: req.cycle,
                disputed_amount_minor: req.disputed_amount_minor,
                cash_hold_minor,
                op: ChargebackDisputeOp::Advance {
                    last_phase: req.phase,
                    clawed_back_minor: clawed_back,
                },
                publisher: Arc::clone(&self.publisher),
                ctx: ctx.clone(),
            },
            flow: FLOW_CHARGEBACK.to_owned(),
            business_id: business_id.to_owned(),
            tenant: req.tenant_id,
        });
        let (new_entry, new_lines) = self.to_engine_inputs(scope, entry).await?;
        let posting = self
            .posting
            .post_queued_apply(ctx, scope, new_entry, new_lines, Some(sidecar))
            .await?;
        Ok(posting)
    }

    /// Drain up to `limit` due queued dispute phases for one tenant: claim them
    /// under `SKIP LOCKED` in a short claim txn, then apply EACH in its OWN txn
    /// (the "apply is a second txn" shape, §4.7). `Blocked` ⇒ bump attempts +
    /// back off via `apply_after`; `NotReady` ⇒ skip (no bump). Mirrors
    /// [`crate::infra::payment::allocate::AllocationService::drain`].
    ///
    /// # Errors
    /// [`DomainError::Internal`] only if the initial claim txn fails; per-row
    /// faults are isolated inside the pass.
    pub async fn drain(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        limit: u64,
    ) -> Result<DrainReport, DomainError> {
        let now = OffsetDateTime::now_utc();
        let pending_queue = self.pending_queue.clone();
        let scope_owned = scope.clone();
        let claimed: Vec<pending_event_queue::Model> = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    pending_queue
                        .claim_due(txn, &scope_owned, tenant, FLOW_CHARGEBACK, now, limit)
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("chargeback drain claim: {e}")))?;

        let mut report = DrainReport::default();
        for row in claimed {
            match self.apply_queued_row(ctx, scope, &row).await {
                Ok(ApplyOutcome::Applied(_)) => report.applied += 1,
                Ok(ApplyOutcome::NotReady) => report.not_ready += 1,
                Ok(ApplyOutcome::Blocked(err)) => {
                    report.blocked += 1;
                    if let Err(bump_err) = self
                        .bump_attempts_own_txn(
                            scope,
                            tenant,
                            &row.business_id,
                            i64::from(row.attempts),
                        )
                        .await
                    {
                        tracing::error!(
                            tenant_id = %tenant,
                            business_id = %row.business_id,
                            blocked_by = %err,
                            error = %bump_err,
                            "bss-ledger: chargeback drain failed to bump attempts for blocked phase"
                        );
                    } else {
                        tracing::warn!(
                            tenant_id = %tenant,
                            business_id = %row.business_id,
                            error = %err,
                            "bss-ledger: queued dispute phase blocked at apply (attempts bumped, left QUEUED)"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        tenant_id = %tenant,
                        business_id = %row.business_id,
                        error = %e,
                        "bss-ledger: queued dispute phase apply failed (infra); continuing"
                    );
                }
            }
        }
        Ok(report)
    }

    /// Bump one queue row's `attempts` + defer its next eligibility by an
    /// exponential backoff, in its own short transaction. Mirrors
    /// [`crate::infra::payment::allocate::AllocationService`]'s `bump_attempts_own_txn`.
    async fn bump_attempts_own_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        business_id: &str,
        prior_attempts: i64,
    ) -> Result<(), DomainError> {
        let scope_owned = scope.clone();
        let business_id = business_id.to_owned();
        let defer_until = OffsetDateTime::now_utc() + blocked_backoff(prior_attempts + 1);
        self.db
            .transaction(move |txn| {
                Box::pin(async move {
                    PendingQueueRepo::bump_attempts_and_defer(
                        txn,
                        &scope_owned,
                        tenant,
                        FLOW_CHARGEBACK,
                        &business_id,
                        defer_until,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("chargeback drain bump attempts: {e}")))
    }

    /// Read the net cash-leg size for a `CASH_HOLD` dispute (Model N):
    /// `net = settled_minor − fee_minor`, the cash that actually entered
    /// `CASH_CLEARING` at `settle` (the PSP fee went straight to `PSP_FEE_EXPENSE`,
    /// never into clearing). Read out-of-txn before the build so the pure builder
    /// can size every `CASH_HOLD` cash leg (opened/won/lost) at `net`.
    ///
    /// Only called for the `CASH_HOLD` variant — an `AR_RECLASS` dispute has no
    /// PSP fee and no cash leg, so the caller passes `0` without reading.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure, or when the settlement is
    /// ABSENT — a `CASH_HOLD` dispute on a payment that was never settled is an
    /// upstream contract violation (the cash it claims to hold never moved).
    async fn read_net(
        &self,
        scope: &AccessScope,
        input: &ChargebackInput,
    ) -> Result<i64, DomainError> {
        let settlement = self
            .payment_repo
            .read_settlement(scope, input.tenant_id, &input.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read settlement: {e}")))?
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "CASH_HOLD chargeback on payment {} has no settlement row \
                     (cannot size the net cash leg)",
                    input.payment_id
                ))
            })?;
        // The dispute must be denominated in the SETTLED currency: this read sizes
        // the net cash leg (`settled − fee`) and the sidecar updates that same
        // settlement row, while the journal legs post in `input.currency`. A
        // mismatch (mistyped or malicious — e.g. a USD payment disputed as EUR)
        // would post foreign legs against the original payment — reject before
        // sizing (mirrors `AllocateService`'s settlement-currency gate).
        if settlement.currency != input.currency {
            return Err(DomainError::CurrencyMismatch(format!(
                "chargeback currency {} != settled currency {} for payment {}",
                input.currency, settlement.currency, input.payment_id
            )));
        }
        Ok(settlement.settled_minor - settlement.fee_minor)
    }

    /// `CHARGEBACK_ON_REFUNDED` pre-check: a `lost` cash-out (`clawed_back > 0`)
    /// whose clawback cannot fit under the total money-out cap
    /// (`refunded_minor + clawed_back_minor + clawed_back > settled_minor`) AND
    /// where the payment was already refunded (`refunded_minor > 0`) routes to the
    /// minimal exception stub: a logged skip + [`DomainError::ChargebackOnRefunded`]
    /// (the full exception queue is Slice 7 / VHP-1859). When the payment was NOT
    /// refunded the over-cap is a plain `ChargebackExceedsSettled` (left to the
    /// sidecar's cap CHECK). A missing settlement is left to the sidecar too (the
    /// clawback bump's `rows_affected == 0` surfaces as Internal — a chargeback on
    /// an unsettled payment is an upstream contract violation).
    async fn guard_not_on_refunded(
        &self,
        scope: &AccessScope,
        input: &ChargebackInput,
        clawed_back: i64,
    ) -> Result<(), DomainError> {
        let Some(settlement) = self
            .payment_repo
            .read_settlement(scope, input.tenant_id, &input.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read settlement: {e}")))?
        else {
            return Ok(());
        };
        // Widen to i128 for the cap sum: three i64 minor-unit totals could
        // overflow at the extreme. The DB cap CHECK is the authoritative backstop,
        // but this pre-check must not panic (debug) or wrap (release) before the
        // post ever reaches it.
        let total_out = i128::from(settlement.refunded_minor)
            + i128::from(settlement.clawed_back_minor)
            + i128::from(clawed_back);
        let fits = total_out <= i128::from(settlement.settled_minor);
        if !fits && settlement.refunded_minor > 0 {
            tracing::warn!(
                tenant_id = %input.tenant_id,
                payment_id = %input.payment_id,
                dispute_id = %input.dispute_id,
                refunded_minor = settlement.refunded_minor,
                settled_minor = settlement.settled_minor,
                clawed_back,
                "bss-ledger: chargeback lost on an already-refunded payment — routed to \
                 exception stub (full exception_queue is Slice 7)"
            );
            // Slice 7 Phase 2: ADDITIVE close-blocking exception row beside the log +
            // the rejection below. Keyed `(dispute_id, payment_id)`; fire-and-forget.
            if let Some(ex) = &self.exceptions {
                let detail = serde_json::json!({
                    "dispute_id": input.dispute_id,
                    "payment_id": input.payment_id,
                    "clawed_back_minor": clawed_back,
                    "refunded_minor": settlement.refunded_minor,
                    "settled_minor": settlement.settled_minor,
                });
                ex.route(
                    input.tenant_id,
                    crate::domain::exception::ExceptionType::ChargebackOnRefunded,
                    &format!("{}:{}", input.dispute_id, input.payment_id),
                    Some(detail),
                )
                .await;
            }
            return Err(DomainError::ChargebackOnRefunded(format!(
                "dispute {} lost on payment {}: clawback {} cannot fit under the money-out cap \
                 (refunded={}, clawed={}, settled={})",
                input.dispute_id,
                input.payment_id,
                clawed_back,
                settlement.refunded_minor,
                settlement.clawed_back_minor,
                settlement.settled_minor
            )));
        }
        Ok(())
    }

    /// Stamp functional **carry-forward** onto a cross-currency chargeback entry
    /// (Slice 5 F3, design §3.5 — chargeback close). A dispute phase reclassifies a
    /// position WITHOUT locking a new rate: `CASH_HOLD` moves cash
    /// `CASH_CLEARING ↔ DISPUTE_HOLD` (and `DISPUTE_HOLD → DISPUTE_LOSS` on a lost
    /// forfeit); `AR_RECLASS` moves the receivable `ACTIVE ↔ DISPUTED` at one grain
    /// (and `DISPUTED → DISPUTE_LOSS` on a lost write-off). The functional cost
    /// basis of the grain it CLOSES therefore carries forward to the counter-leg
    /// unchanged — realized FX is recognised only at a cash in/out point (settle
    /// S2 / refund S3), never on an internal reclassification.
    ///
    /// Reads the carried `(functional, transaction)` value of the grain the phase
    /// closes and stamps EVERY line's functional at that grain's WAC pro-rata
    /// ([`carried_relief`]):
    /// - `CASH_HOLD` `opened` → `CASH_CLEARING`; `won`/`lost` → `DISPUTE_HOLD`
    ///   (`account_balance`, found by the closing leg's bound `account_id`);
    /// - `AR_RECLASS` (any) → the disputed AR invoice (`ar_invoice_balance`).
    ///
    /// Every chargeback entry is two legs of EQUAL transaction amount, so both legs
    /// get the SAME functional → the entry's functional column nets to zero (NO
    /// `FX_GAIN_LOSS` line) while the closing grain's functional decrements by
    /// exactly its pro-rata carried value (a full close → 0), keeping the
    /// functional column in lockstep with `balance_minor` under the dual-column
    /// commit trigger.
    ///
    /// No-op (leaves functional NULL — byte-green single-currency path) when the
    /// closing grain carries no functional balance (design decision 8) OR when the
    /// relieved amount exceeds the grain's balance (an over-relief the projector
    /// rejects with `NegativeBalance`; skipping lets that cleaner rejection surface,
    /// mirroring allocate F1's pool-underflow guard).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a carried-read fault, a missing closing leg, or
    /// a [`carried_relief`] misuse (a malformed grain value — an internal
    /// invariant breach).
    async fn apply_fx_carry_forward(
        &self,
        scope: &AccessScope,
        input: &ChargebackInput,
        entry: &mut PostEntry,
    ) -> Result<(), DomainError> {
        // Read the carried (transaction, functional) value of the grain this phase
        // CLOSES (the position whose functional cost basis carries forward).
        let carried = match input.variant {
            DisputeVariant::ArReclass => {
                // The disputed AR invoice grain. invoice_id is guaranteed present
                // (the builder rejects an AR_RECLASS without it before this runs).
                let invoice_id = input.invoice_id.as_deref().ok_or_else(|| {
                    DomainError::Internal(
                        "chargeback FX: AR_RECLASS entry has no invoice_id".to_owned(),
                    )
                })?;
                self.payment_repo
                    .read_ar_invoice_carried(
                        scope,
                        input.tenant_id,
                        input.payer_tenant_id,
                        invoice_id,
                        &input.currency,
                    )
                    .await
                    .map_err(|e| DomainError::Internal(format!("read ar invoice carried: {e}")))?
            }
            DisputeVariant::CashHold => {
                // The cash grain this phase relieves: CASH_CLEARING at `opened`
                // (cash leaves clearing into the hold), DISPUTE_HOLD at
                // `won`/`lost` (the hold is released / forfeited). `partial` is
                // rejected in the builder; map it to DISPUTE_HOLD defensively.
                let closing_class = match input.phase {
                    DisputePhase::Opened => AccountClass::CashClearing,
                    DisputePhase::Won | DisputePhase::Lost | DisputePhase::Partial => {
                        AccountClass::DisputeHold
                    }
                };
                let account_id = entry
                    .lines
                    .iter()
                    .find(|l| l.account_class == closing_class)
                    .map(|l| l.account_id)
                    .ok_or_else(|| {
                        DomainError::Internal(format!(
                            "chargeback FX: CASH_HOLD entry has no {} leg",
                            closing_class.as_str()
                        ))
                    })?;
                self.payment_repo
                    .read_account_carried(scope, input.tenant_id, account_id, &input.currency)
                    .await
                    .map_err(|e| DomainError::Internal(format!("read account carried: {e}")))?
            }
        };

        // Cross-currency detect (design decision 8): the closing grain carries a
        // functional balance. NULL ⇒ single-currency close: leave functional NULL.
        let (Some(carried_functional), Some(functional_ccy)) = (
            carried.functional_balance_minor,
            carried.functional_currency,
        ) else {
            return Ok(());
        };

        // The closing leg's relieved transaction amount (every chargeback leg
        // shares the amount, so the first line's amount is it). A non-positive
        // carried balance or an over-relief ⇒ skip carry-forward so the projector's
        // NegativeBalance surfaces (the close never posts cleanly either way).
        let relieved = entry.lines.first().map_or(0, |l| l.amount_minor);
        if carried.balance_minor <= 0 || relieved > carried.balance_minor {
            return Ok(());
        }

        // Stamp every line's functional at the grain's WAC pro-rata of its OWN
        // amount. All legs share the amount, so both get the same value → the
        // functional column nets to zero (carry-forward; no FX line) and the
        // closing grain decrements by exactly its pro-rata carried functional.
        for line in &mut entry.lines {
            let func = carried_relief(carried_functional, carried.balance_minor, line.amount_minor)
                .map_err(|e| DomainError::Internal(format!("chargeback FX carry-forward: {e}")))?;
            line.functional_amount_minor = Some(func);
            line.functional_currency = Some(functional_ccy.clone());
        }
        Ok(())
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's
    /// `NewEntry`/`NewLine`, resolving each line's scale, and post INLINE with the
    /// chargeback sidecar.
    async fn post_bound(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Arc<dyn PostSidecar>,
    ) -> Result<PostingRef, DomainError> {
        let (new_entry, new_lines) = self.to_engine_inputs(scope, entry).await?;
        self.posting
            .post(ctx, scope, new_entry, new_lines, Some(sidecar))
            .await
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's `NewEntry` +
    /// `Vec<NewLine>`, resolving each line's currency scale. Shared by the inline
    /// and deferred-apply post paths.
    async fn to_engine_inputs(
        &self,
        scope: &AccessScope,
        entry: PostEntry,
    ) -> Result<(NewEntry, Vec<NewLine>), DomainError> {
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
            // Slice 5 (F3): a chargeback locks NO new rate — it reclassifies at the
            // carried rate (functional carry-forward in `apply_fx_carry_forward`),
            // so the entry never carries a rate_snapshot_ref.
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
        Ok((new_entry, new_lines))
    }

    /// Emit `chargeback(outcome)` + the `Chargeback`-labelled payment-post
    /// duration for one attempt. A `Queued` outcome is an ENQUEUE, not a post: it
    /// records ONLY the duration (the post — and the Posted/Replayed/Rejected
    /// outcome — happens later on the drain), mirroring the allocate `record`.
    fn record(&self, result: &Result<ChargebackOutcome, DomainError>, started: Instant) {
        match result {
            Ok(ChargebackOutcome::Recorded(r)) => {
                let outcome = if r.replayed {
                    PostResult::Replayed
                } else {
                    PostResult::Posted
                };
                self.metrics.chargeback(outcome);
            }
            Err(_) => self.metrics.chargeback(PostResult::Rejected),
            // Enqueue: duration only, no outcome counter.
            Ok(ChargebackOutcome::Queued(_)) => {}
        }
        self.metrics
            .payment_post_duration(started.elapsed().as_secs_f64(), PostFlow::Chargeback);
    }
}

/// Guard the `∅ → opened` / `{won,lost} → opened` transition (design §2): an
/// `opened` is valid only when the dispute has no row yet, or its prior cycle
/// already ended (`last_phase` is a terminal `WON`/`LOST`). An `opened` on a
/// dispute whose `last_phase` is still `OPENED` (or `PARTIAL`) is an illegal
/// re-open and is rejected.
fn guard_open_transition(
    existing: Option<&crate::infra::storage::entity::dispute::Model>,
    dispute_id: &str,
) -> Result<(), DomainError> {
    let Some(row) = existing else {
        return Ok(());
    };
    match DisputePhase::parse(&row.last_phase) {
        Some(DisputePhase::Won | DisputePhase::Lost) => Ok(()),
        _ => Err(DomainError::InvalidDisputeTransition(format!(
            "dispute {dispute_id} is already {} — cannot open a new cycle until it is won/lost",
            row.last_phase
        ))),
    }
}

/// Guard the `opened → {won,lost}` transition (design §2): an outcome is valid
/// only when the dispute's current `last_phase` is `OPENED`. A `won`/`lost` on a
/// dispute that already resolved (`WON`/`LOST`) or was never opened is an illegal
/// transition.
fn guard_outcome_transition(
    row: &crate::infra::storage::entity::dispute::Model,
    dispute_id: &str,
) -> Result<(), DomainError> {
    match DisputePhase::parse(&row.last_phase) {
        Some(DisputePhase::Opened) => Ok(()),
        _ => Err(DomainError::InvalidDisputeTransition(format!(
            "dispute {dispute_id} is {} — only an OPENED dispute can be won/lost",
            row.last_phase
        ))),
    }
}

/// Overwrite the placeholder header fields the pure builder emits (mirrors
/// [`crate::infra::payment::settlement_return`]'s overwrite): derive the
/// `period_id` (YYYYMM) and a real `effective_at` from the phase instant
/// (`None` ⇒ now), stamp the actor, and mint a fresh correlation id.
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

/// Thin `PostLine` adapter over [`ChartIndex::resolve`]. The chargeback classes
/// (`DISPUTE_HOLD` / `CASH_CLEARING` / `AR` / `DISPUTE_LOSS_EXPENSE`) are all
/// stream-less, so this resolves on `stream = None`.
fn resolve_line(chart: &ChartIndex, line: &PostLine) -> Option<Uuid> {
    chart.resolve(
        line.account_class,
        &line.currency,
        line.revenue_stream.as_deref(),
    )
}

/// Map one SDK [`PostLine`] + its resolved scale to the engine's [`NewLine`]
/// (mirrors `settlement_return::new_line`).
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

/// Is this rejection an apply-time guard/cap failure (so the drain leaves the row
/// `QUEUED` + bumps attempts), as opposed to an infra fault (which propagates)?
/// The blocked set is the guard/cap family a chargeback apply can hit against
/// then-current state: the transition guard, the clawback cap, the
/// already-refunded route, a closed period, the projector's no-negative guard, an
/// unprovisioned chart, and a malformed-amount/invoice request. Everything else
/// (notably [`DomainError::Internal`]) is infra.
fn is_apply_blocked(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::InvalidDisputeTransition(_)
            | DomainError::ChargebackExceedsSettled(_)
            | DomainError::ChargebackOnRefunded(_)
            | DomainError::MoneyOutCapExceeded(_)
            | DomainError::NegativeBalance(_)
            | DomainError::PeriodClosed(_)
            | DomainError::AccountClosed(_)
            | DomainError::InvalidRequest(_)
    )
}

/// Exponential backoff (wall-clock) before a `Blocked` queued dispute phase may
/// be re-claimed (mirrors the allocate `blocked_backoff`): ~`2^(attempts-1)`
/// seconds, capped at 5 minutes — keeps a durably-`Blocked` ("poison") row from
/// hot-looping the drain before Phase 5 adds quarantine.
fn blocked_backoff(attempts: i64) -> Duration {
    const BASE_SECS: i64 = 2;
    const MAX_SECS: i64 = 300;
    let shift = attempts.clamp(1, 16) - 1;
    let secs = BASE_SECS.saturating_mul(1_i64 << shift).min(MAX_SECS);
    Duration::seconds(secs)
}

/// The request-based idempotency hash for a queued dispute phase — the
/// `content_hash` of the canonical [`QueuedDisputePayload`]. Stable across the
/// apply's state-dependent entry rebuild, so it (not an entry hash) is what the
/// queued-intake dedup row stores; the early `replay_short_circuit` /
/// `claim_queued` compare it to reject a same-key / different-payload reuse.
fn dispute_request_hash(payload: &QueuedDisputePayload) -> Result<String, DomainError> {
    let canonical = serde_json::to_string(payload)
        .map_err(|e| DomainError::Internal(format!("canonicalize dispute payload: {e}")))?;
    Ok(IdempotencyGate::content_hash(&canonical))
}

/// Composite in-transaction sidecar for the deferred apply of a queued dispute
/// phase: it runs the SAME dispute writes + clawback bump + `dispute.recorded`
/// publish as the inline path (by delegating to the wrapped [`ChargebackSidecar`])
/// and THEN flips the work-state queue row `→APPLIED` — both inside the post txn
/// opened by [`PostingService::post_queued_apply`]. So the dispute effect and the
/// queue-row transition commit atomically, or roll back together. Mirrors
/// [`crate::infra::payment::allocate`]'s `QueuedAllocationApplySidecar`.
struct QueuedChargebackApplySidecar {
    inner: ChargebackSidecar,
    flow: String,
    business_id: String,
    tenant: Uuid,
}

#[async_trait::async_trait]
impl PostSidecar for QueuedChargebackApplySidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &crate::infra::posting::service::PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. The dispute writes + clawback bump + the in-txn `dispute.recorded`
        //    publish (delegated — the cap CHECK is re-evaluated here).
        self.inner.run(txn, scope, posted).await?;
        // 2. Flip the work-state queue row `→APPLIED` in the SAME txn.
        PendingQueueRepo::mark_applied(txn, scope, self.tenant, &self.flow, &self.business_id)
            .await
            .map_err(|e| DomainError::Internal(format!("queue-apply mark_applied: {e}")))?;
        Ok(())
    }
}
