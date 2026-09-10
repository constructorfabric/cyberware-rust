//! `AllocationService` — the orchestrator that drives the pure allocation
//! domain (`crate::domain::payment::{precedence, allocation}`) through the
//! foundation engine (Pattern A apply). It records the **money-out** side of a
//! payment: a lump from the payer's unallocated pool is applied to their open
//! receivables oldest-first, draining the pool (`DR UNALLOCATED`) into AR
//! (`CR AR` per invoice).
//!
//! Sequence for one allocate:
//! 1. **gate on settlement** — the payment must be settled (the
//!    `payment_settlement` row exists) and its currency must match the request.
//! 2. **read candidates** — the payer's open AR invoices for the currency
//!    (oldest-first, read-only and uncapped; the size bound is enforced below on
//!    the invoices the split actually touches, default [`MAX_INVOICES_PER_ALLOCATION`]).
//! 3. **decide** — resolve the tenant's effective-dated precedence policy
//!    (`PaymentRepo::read_effective_policy`; oldest-first when none) and
//!    `precedence::select_split` splits the lump across them under it.
//! 4. **build** the balanced Pattern-A-apply entry
//!    (`allocation::build_allocation_entry`), **overwrite** its placeholder
//!    header, **bind** chart `account_id`s, and **post** with the
//!    [`AllocationSidecar`] (bumps `allocated_minor` under the per-payment cap
//!    CHECK, inserts the `payment_allocation` rows, bumps the refund counters) —
//!    all in one serializable transaction.
//! 5. **emit metrics** — `allocation` (outcome) + the payment-post duration.
//!
//! The per-payment money-out cap (`allocated_minor <= settled_minor`) is the
//! sidecar's serializable backstop — an over-cap surfaces as
//! [`DomainError::MoneyOutCapExceeded`]. Idempotent on
//! `(tenant, PAYMENT_ALLOCATE, allocation_id)`: a replay of the same
//! `allocation_id` returns the prior entry with no duplicate rows. Lives in
//! `infra` (not `domain`) because it needs repo + posting access; the domain
//! modules it calls stay pure (dylint DE0301).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bss_ledger_sdk::{
    AccountClass, MappingStatus, PostEntry, PostLine, PostingRef, Side, SourceDocType,
};
use chrono::Datelike;
use sea_orm::DbErr;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::fx::realized::{ClosingLeg, realize};
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::payment::allocation::{
    AllocationInput, build_allocation_entry, validate_caller_split,
};
use crate::domain::payment::precedence::{
    Allocated, Candidate, DEFAULT_PRECEDENCE_POLICY, PrecedenceStrategy, select_split,
};
use crate::domain::ports::metrics::{LedgerMetricsPort, PostFlow, PostResult};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::payment::sidecar::AllocationSidecar;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::idempotency::{
    ClaimOutcome, IdempotencyGate, STATUS_POSTED, STATUS_QUEUED,
};
use crate::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use crate::infra::storage::entity::pending_event_queue;
use crate::infra::storage::repo::{NewQueueRow, PaymentRepo, PendingQueueRepo, ReferenceRepo};
use time::Duration;
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// The deferred-apply queue flow + idempotency-dedup `flow` for a payment
/// allocation — the `PAYMENT_ALLOCATE` source-doc literal. The same literal the
/// inline post stamps on its entry header (so a queued allocation and the post
/// it later becomes share one dedup key), reused here for the `claim_queued`
/// seed and the queue row's `flow`. The early dedup lookup passes the
/// [`SourceDocType::PaymentAllocate`] enum directly (the repo maps it to this
/// same literal); a `const_eq` debug assert in `claim_queued`'s caller would be
/// redundant since `SourceDocType::as_str` is the single source of truth.
/// `as_str` is not `const`, so this can't be derived from the enum in a `const`
/// initializer — it is the literal, kept in lockstep with the enum by the
/// round-trip test in `enums.rs`.
const FLOW_PAYMENT_ALLOCATE: &str = "PAYMENT_ALLOCATE";

/// Upper bound on the number of invoices a single allocation may **touch** (the
/// AR legs it posts). The bound guards the WRITE, not the read: each touched
/// invoice is one CR `AR` line, so an allocation posting more than this would
/// risk the engine's per-entry line ceiling and an unbounded transaction. It is
/// therefore enforced on the computed split (the invoices that receive a
/// positive amount), NOT on the candidate set: a payer with a large open-invoice
/// backlog whose payment only reaches a handful of them allocates fine. A split
/// exceeding this — a lump large enough to pay > 500 invoices at once — is
/// rejected with [`DomainError::AllocationTooLarge`] (§Bounds — chunked
/// continuations for the genuinely-> `MAX` case are a tracked follow-up).
pub const MAX_INVOICES_PER_ALLOCATION: usize = 500;

/// Audit `precedence_policy_ref` stamped on a Mode B (caller-computed) split
/// (§4.4 F-5). A distinct sentinel — NOT one of the
/// [`PrecedenceStrategy::policy_ref`](crate::domain::payment::precedence::PrecedenceStrategy::policy_ref)
/// ids — so the allocation's audit trail records that the split was supplied by
/// the caller and validated, not decided by a precedence policy.
const CALLER_SPLIT_POLICY_REF: &str = "caller-split.v1";

/// One allocate request: apply `lump_minor` of the settled payment's
/// unallocated pool to the payer's open receivables.
pub struct AllocateRequest {
    /// The seller tenant whose ledger this posts into.
    pub tenant_id: Uuid,
    /// The tenant whose receivables are paid (the pool owner / single payer).
    pub payer_tenant_id: Uuid,
    /// External payment identity — the settled payment whose pool this drains.
    pub payment_id: String,
    /// Allocation identity — the `PAYMENT_ALLOCATE` idempotency business id.
    pub allocation_id: Uuid,
    /// Amount to apply from the pool, in minor units.
    pub lump_minor: i64,
    /// ISO currency of the allocation (must match the settlement currency).
    pub currency: String,
    /// Optional invoice to pay FIRST (jumps to the front of the oldest-first
    /// order); ignored when it names no open candidate. Only consulted on the
    /// precedence path — a caller-computed split (`caller_splits`) bypasses the
    /// decision entirely, so the hint is moot there.
    pub hint_invoice_id: Option<String>,
    /// Mode B escape hatch (§4.4 F-5): an explicit caller-computed per-invoice
    /// split. `Some` ⇒ the precedence decision is SKIPPED and these shares are
    /// validated against the open candidates instead (same caps / no-negative /
    /// presence the decided path is subject to); `None` ⇒ the unchanged
    /// precedence path decides the split.
    pub caller_splits: Option<Vec<Allocated>>,
}

/// The result of an allocate: either it posted inline (the payment was settled)
/// or it was durably queued for a later drain (the payment was not yet settled —
/// §4.7 allocation-before-settlement). The two arms drive the SDK/REST 201-vs-202
/// split (the local client maps them onto `bss_ledger_sdk::AllocateOutcome`).
#[derive(Debug)]
pub enum AllocationOutcome {
    /// The payment was settled: the allocation posted inline.
    Applied(AppliedAllocation),
    /// The payment was NOT yet settled: the request was enqueued (HTTP 202).
    Queued(QueuedAllocation),
}

/// An allocation that posted inline: the posting handle, the per-invoice splits
/// applied (so the caller can surface what was applied where), and the
/// `precedence_policy_ref` stamped on the persisted rows — a precedence policy
/// id for the decided path, or `caller-split.v1` for a Mode B caller-computed
/// split. (The fields of the pre-Group-C `AllocationOutcome` struct.)
#[derive(Debug)]
pub struct AppliedAllocation {
    pub posting: PostingRef,
    pub splits: Vec<Allocated>,
    pub policy_ref: String,
}

/// An allocation that was deferred because the payment was not yet settled: the
/// request is durably on `ledger_pending_event_queue` and the drain (Group D)
/// will apply it once the settlement lands. Carries the queue key (`flow` +
/// `business_id`) and the `queued_at` instant — the surface for the REST 202
/// `allocation-queued` body. No `PostingRef`: nothing has posted yet.
#[derive(Debug)]
pub struct QueuedAllocation {
    /// The deferred-apply queue flow (the `PAYMENT_ALLOCATE` literal).
    pub flow: String,
    /// The queue/dedup business id — the allocation's `allocation_id` (string).
    pub business_id: String,
    /// When the intake durably enqueued the request.
    pub queued_at: OffsetDateTime,
}

/// The financial-key snapshot of an allocate request, persisted as the queue
/// row's `payload` jsonb at intake and re-read by the drain (Group D) to decide
/// the split + post. PII-free by construction: it carries only the ledger
/// identities and money fields the apply needs — tenant + payer ids, the payment
/// / allocation ids, the lump + currency, the optional precedence hint, and the
/// optional Mode B caller split — never names, addresses, or free-text.
///
/// It is ALSO the basis for the queued-allocation dedup `payload_hash`: the
/// per-invoice split is only decided at apply, so the inline post's entry-based
/// [`IdempotencyGate::payload_hash`] cannot be computed at intake (the entry
/// doesn't exist yet). Instead the request is hashed via
/// [`IdempotencyGate::content_hash`] over this struct's canonical JSON
/// (`serde_json::to_string`), so a conflicting reuse of the same `allocation_id`
/// with a *different* request still flips the hash. Lives here (not `domain`)
/// next to the service that writes it; Group D's apply reads the same type.
///
/// The Mode B caller split is carried as [`QueuedSplit`] (a local serde mirror of
/// the domain [`Allocated`]) rather than `Allocated` itself, so the domain stays
/// serde-free (pure); Group D converts it back to `Allocated` for
/// `validate_caller_split` at apply.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedAllocationPayload {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub payment_id: String,
    pub allocation_id: Uuid,
    pub lump_minor: i64,
    pub currency: String,
    pub hint_invoice_id: Option<String>,
    pub caller_splits: Option<Vec<QueuedSplit>>,
}

/// A serde mirror of the domain [`Allocated`] (`invoice_id` + `amount_minor`),
/// carried in [`QueuedAllocationPayload::caller_splits`]. Kept local to `infra`
/// so the domain `Allocated` need not derive serde — Group D maps it back to
/// `Allocated` at apply.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedSplit {
    pub invoice_id: String,
    pub amount_minor: i64,
}

impl AllocateRequest {
    /// Reconstruct the request from a queued payload at apply time (Group D),
    /// mapping each Mode B [`QueuedSplit`] back to the domain [`Allocated`]. The
    /// inverse of [`QueuedAllocationPayload::from_request`]; the round-trip
    /// preserves every field the decide/build path consults.
    fn from_payload(payload: QueuedAllocationPayload) -> Self {
        Self {
            tenant_id: payload.tenant_id,
            payer_tenant_id: payload.payer_tenant_id,
            payment_id: payload.payment_id,
            allocation_id: payload.allocation_id,
            lump_minor: payload.lump_minor,
            currency: payload.currency,
            hint_invoice_id: payload.hint_invoice_id,
            caller_splits: payload.caller_splits.map(|splits| {
                splits
                    .into_iter()
                    .map(|s| Allocated {
                        invoice_id: s.invoice_id,
                        amount_minor: s.amount_minor,
                    })
                    .collect()
            }),
        }
    }
}

impl QueuedAllocationPayload {
    /// Snapshot an [`AllocateRequest`] into the PII-free queue payload (by
    /// reference — the request is still needed to build the `Queued` handle).
    fn from_request(req: &AllocateRequest) -> Self {
        Self {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id.clone(),
            allocation_id: req.allocation_id,
            lump_minor: req.lump_minor,
            currency: req.currency.clone(),
            hint_invoice_id: req.hint_invoice_id.clone(),
            caller_splits: req.caller_splits.as_ref().map(|splits| {
                splits
                    .iter()
                    .map(|s| QueuedSplit {
                        invoice_id: s.invoice_id.clone(),
                        amount_minor: s.amount_minor,
                    })
                    .collect()
            }),
        }
    }
}

/// What the intake transaction committed — carried out of the `db.transaction`
/// closure (which must return `Send + 'static`) so the post-txn code can build
/// the `Queued` handle. `Enqueued` is the first intake (it owns the `queued_at`
/// it just wrote); `AlreadyQueued` is the race-lost replay (the prior intake's
/// queue row holds the authoritative `queued_at`, read out-of-txn afterwards).
enum IntakeOutcome {
    Enqueued {
        queued_at: OffsetDateTime,
    },
    AlreadyQueued,
    /// A concurrent/retried intake reused the same `allocation_id` with a
    /// DIFFERENT request payload (the dedup row's `payload_hash` differs) — an
    /// idempotency-key conflict, surfaced after the txn as
    /// [`DomainError::IdempotencyConflict`].
    Conflict,
}

/// The decided + chart-bound allocation entry produced by
/// [`AllocationService::decide_and_build_entry`], shared by the inline post path
/// and the deferred-apply path. Carries the bound [`PostEntry`], the per-invoice
/// `splits`, the audit `policy_ref`, and the `total` (the sum the sidecar bumps
/// `allocated_minor` by under the per-payment cap CHECK).
struct BuiltAllocation {
    entry: PostEntry,
    splits: Vec<Allocated>,
    policy_ref: String,
    total: i64,
}

/// The result of applying ONE queued allocation row ([`AllocationService::apply_queued_row`]).
/// Distinct from [`AllocationOutcome`] because the apply has a third terminal
/// shape the intake-time allocate cannot: the payment may still be unsettled.
#[derive(Debug)]
pub enum ApplyOutcome {
    /// The settlement landed and the queued allocation posted — the queue row was
    /// flipped `→APPLIED` atomically in the post txn.
    Applied(PostingRef),
    /// The payment is STILL not settled: leave the row `QUEUED`, do NOT bump
    /// attempts (this is not a failure — a settle/sweep will retry once the
    /// settlement exists).
    NotReady,
    /// A cap / precondition rejected the apply at apply-time (a re-evaluated cap:
    /// `MoneyOutCapExceeded` / `NegativeBalance` / `PeriodClosed` /
    /// `AllocationSplitInvalid`). The caller bumps `attempts` and leaves the row
    /// `QUEUED` for a later retry (alarm/quarantine is Phase 5). Carries the
    /// rejection for logging.
    Blocked(DomainError),
}

/// Summary of one [`AllocationService::drain`] pass over a tenant's queued
/// allocations.
#[derive(Debug, Default)]
pub struct DrainReport {
    /// Rows that posted + flipped `→APPLIED` this pass.
    pub applied: u64,
    /// Rows left `QUEUED` because the payment is not yet settled (no attempt bump).
    pub not_ready: u64,
    /// Rows left `QUEUED` after an apply-time cap/precondition rejection
    /// (attempts bumped).
    pub blocked: u64,
}

/// Orchestrates the allocation domain (Pattern A apply) over the foundation
/// engine.
pub struct AllocationService {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    repo: PaymentRepo,
    // The deferred-apply queue (work-state SoT): an allocate of a not-yet-settled
    // payment is enqueued here at intake (§4.7) and drained later (Group D).
    pending_queue: PendingQueueRepo,
    // One database provider, retained so the intake enqueue can open its own
    // `db.transaction` (the dedup claim + queue insert in one txn). The other
    // repos are out-of-txn readers; this is the only writer that needs a txn.
    db: DBProvider<DbError>,
    metrics: Arc<dyn LedgerMetricsPort>,
    /// Max invoices one allocation may touch (the posted AR legs). Defaults to
    /// [`MAX_INVOICES_PER_ALLOCATION`]; overridden from `payments` config via
    /// [`Self::with_max_invoices_per_allocation`].
    max_invoices: usize,
}

impl AllocationService {
    /// Build the service over one database provider, the event publisher
    /// (threaded into the posting engine), and the metrics sink. Same deps as
    /// [`crate::infra::payment::settle::SettlementService`]. The touched-invoice
    /// cap defaults to [`MAX_INVOICES_PER_ALLOCATION`]; override it from config
    /// with [`Self::with_max_invoices_per_allocation`].
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), publisher);
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let repo = PaymentRepo::new(db.clone());
        let pending_queue = PendingQueueRepo::new(db.clone());
        Self {
            posting,
            reference,
            resolver,
            repo,
            pending_queue,
            db,
            metrics,
            max_invoices: MAX_INVOICES_PER_ALLOCATION,
        }
    }

    /// Override the per-allocation touched-invoice cap (from `payments` config).
    /// Builder form; defaults to [`MAX_INVOICES_PER_ALLOCATION`]. The value is
    /// bounded at config-validation time (`1..=MAX_INVOICES_PER_ALLOCATION_CEILING`).
    #[must_use]
    pub fn with_max_invoices_per_allocation(mut self, max_invoices: usize) -> Self {
        self.max_invoices = max_invoices;
        self
    }

    /// Allocate `lump_minor` of a payment's pool to the payer's open AR. When the
    /// payment is already settled this posts inline and returns
    /// [`AllocationOutcome::Applied`] (the posting handle + the decided splits);
    /// when it is NOT yet settled the request is durably queued (§4.7
    /// allocation-before-settlement) and returns [`AllocationOutcome::Queued`]
    /// (the queue key + `queued_at`).
    ///
    /// On an inline post emits `allocation(Posted | Replayed)` + the payment-post
    /// duration; every rejection emits `allocation(Rejected)` + the duration. A
    /// queue (enqueue) is NOT a post, so it records only the duration — no
    /// `allocation()` outcome (see [`Self::record`]).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when there is no open AR to allocate (only
    /// reachable on the settled path — an unsettled payment queues, it does not
    /// reject); [`DomainError::AllocationCurrencyMismatch`] when the request
    /// currency differs from the settlement currency;
    /// [`DomainError::AllocationTooLarge`] when the computed split touches more
    /// invoices than the configured cap (default [`MAX_INVOICES_PER_ALLOCATION`]);
    /// [`DomainError::AllocationSplitInvalid`]
    /// when a caller-computed split (Mode B) names an unknown/closed invoice,
    /// over-allocates an invoice or the lump, repeats an invoice, or is
    /// non-positive; [`DomainError::MoneyOutCapExceeded`] when the allocation
    /// would push `allocated_minor` past `settled_minor`; any foundation
    /// rejection or [`DomainError::Internal`] on an infra fault.
    pub async fn allocate(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        input: AllocateRequest,
    ) -> Result<AllocationOutcome, DomainError> {
        let started = Instant::now();
        let result = self.allocate_inner(ctx, scope, input).await;
        self.record(&result, started);
        result
    }

    /// Run the allocate sequence (no metrics — the public wrapper records them).
    async fn allocate_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        input: AllocateRequest,
    ) -> Result<AllocationOutcome, DomainError> {
        // 0. Early dedup short-circuit (BEFORE the settlement gate), mirroring the
        //    credit short-circuit `CreditApplicationService::replay_if_posted`. A
        //    still-`QUEUED` dedup row means a prior intake already enqueued THIS
        //    allocation: return the same `Queued` handle (idempotent replay during
        //    the queued window) — regardless of whether the payment has since
        //    settled, because the drain (Group D) owns applying it. A `POSTED` row
        //    means the allocation already applied (inline, OR queued-then-drained
        //    by Group D): return an `Applied` replay with EMPTY splits (the splits
        //    were returned on the first call; this mirrors the credit
        //    short-circuit's empty-vec replay) so a queued-then-applied allocation
        //    replays cleanly here instead of conflicting in `post()`. A `CLAIMED`
        //    (in-flight inline) / absent row falls through to the settlement gate.
        if let Some(outcome) = self.replay_short_circuit(scope, &input).await? {
            return Ok(outcome);
        }

        // 1. Read the settlement. ABSENT ⇒ the payment is not yet settled: the
        //    allocate is durably queued for a later drain (§4.7), not rejected.
        //    PRESENT ⇒ the inline post path (its currency must match the request).
        let Some(settlement) = self
            .repo
            .read_settlement(scope, input.tenant_id, &input.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read settlement: {e}")))?
        else {
            let queued = self.enqueue_allocation(scope, &input).await?;
            return Ok(AllocationOutcome::Queued(queued));
        };
        if settlement.currency != input.currency {
            return Err(DomainError::AllocationCurrencyMismatch(format!(
                "allocation currency {} != settlement currency {} for payment {}",
                input.currency, settlement.currency, input.payment_id
            )));
        }

        // 2.–4. Decide the split + build the bound entry (caps re-read against
        //        current state). Shared with the deferred-apply path.
        let built = self.decide_and_build_entry(ctx, scope, &input).await?;
        let BuiltAllocation {
            entry,
            splits,
            policy_ref,
            total,
        } = built;

        // 5. Post inline with the allocation sidecar (bumps allocated_minor under
        //    the cap CHECK + inserts rows).
        let sidecar: Arc<dyn PostSidecar> = Arc::new(AllocationSidecar {
            tenant: input.tenant_id,
            payer: input.payer_tenant_id,
            payment_id: input.payment_id.clone(),
            allocation_id: input.allocation_id,
            currency: input.currency.clone(),
            splits: splits.clone(),
            total_minor: total,
            policy_ref: policy_ref.clone(),
        });
        let request_hash = allocation_request_hash(&input)?;
        let posting = self
            .post_bound(ctx, scope, entry, sidecar, request_hash)
            .await?;
        Ok(AllocationOutcome::Applied(AppliedAllocation {
            posting,
            splits,
            policy_ref,
        }))
    }

    /// Decide the per-invoice split and build the chart-bound, header-overwritten
    /// allocation [`PostEntry`] — steps 2–4 of [`Self::allocate_inner`], factored
    /// out so the deferred-apply path ([`Self::apply_queued_row`]) re-runs the
    /// EXACT same decide/validate/build sequence. Reading the open AR + cap,
    /// resolving precedence (or validating a caller split), building the entry,
    /// overwriting its placeholder header, and binding chart accounts all happen
    /// here — so the caps are re-evaluated against THEN-CURRENT state every time
    /// this runs (intake-time state is never trusted; §4.7).
    ///
    /// # Errors
    /// [`DomainError::AllocationTooLarge`] / [`DomainError::AllocationSplitInvalid`]
    /// / [`DomainError::InvalidRequest`] (empty split) / [`DomainError::AccountClosed`]
    /// (unprovisioned chart) — the same rejections the inline path raises.
    async fn decide_and_build_entry(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        input: &AllocateRequest,
    ) -> Result<BuiltAllocation, DomainError> {
        // 2. Read the open AR candidate set (oldest-first). Read-only and
        //    uncapped — the size bound is enforced below on the touched split.
        let rows = self
            .repo
            .list_open_ar_invoices(
                scope,
                input.tenant_id,
                input.payer_tenant_id,
                &input.currency,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("list open ar invoices: {e}")))?;
        // NOTE: the candidate read is intentionally NOT capped — it is a read-only
        // SELECT, cheap even for a large backlog. The size bound is enforced below
        // on the computed split (the invoices actually touched), which is what
        // drives the posted entry's AR-leg count.
        // Slice 5 (F1): snapshot each candidate's carried `(transaction, functional)`
        // balance by invoice_id BEFORE `rows` is consumed into precedence
        // `Candidate`s. The realized-FX poster (`apply_realized_fx`) reads it to
        // value each AR leg's close at the grain's WAC carried rate. Empty / all-
        // `None`-functional ⇒ a single-currency close (the poster no-ops).
        let ar_carried: HashMap<String, (i64, Option<i64>)> = rows
            .iter()
            .map(|r| {
                (
                    r.invoice_id.clone(),
                    (r.balance_minor, r.functional_balance_minor),
                )
            })
            .collect();

        // 3. Resolve the precedence policy in effect now (latest
        //    effective_from <= now) — used by the precedence path to decide the
        //    per-invoice splits (the hint still jumps the front). No
        //    effective-dated row ⇒ the oldest-first default, and the audit ref
        //    stays `DEFAULT_PRECEDENCE_POLICY` (byte-stable with the 2a
        //    behaviour). Mode B (a caller-computed split) ignores this and
        //    validates the caller's shares instead (see the branch below).
        let effective = self
            .repo
            .read_effective_policy(scope, input.tenant_id, OffsetDateTime::now_utc())
            .await
            .map_err(|e| DomainError::Internal(format!("read precedence policy: {e}")))?;
        let (strategy, policy_ref) = match effective {
            Some((strategy, version)) => (strategy, format!("{}#{version}", strategy.policy_ref())),
            None => (
                PrecedenceStrategy::OldestFirst,
                DEFAULT_PRECEDENCE_POLICY.to_owned(),
            ),
        };
        let candidates: Vec<Candidate> = rows
            .into_iter()
            .map(|r| Candidate {
                invoice_id: r.invoice_id,
                open_minor: r.balance_minor,
                original_posted_at: r.original_posted_at,
            })
            .collect();
        // Mode B (§4.4 F-5): a caller-computed split SKIPS the precedence
        // decision and is validated against the open candidates instead — same
        // caps / no-negative / presence the decided path is bound by. The audit
        // ref then records `caller-split.v1` (not the resolved policy) so the
        // trail shows the split was caller-provided, not policy-decided. The
        // resolved `policy_ref` only governs the unchanged precedence path.
        let (splits, policy_ref) = match &input.caller_splits {
            Some(caller) => (
                validate_caller_split(&candidates, caller, input.lump_minor)?,
                CALLER_SPLIT_POLICY_REF.to_owned(),
            ),
            None => (
                select_split(
                    &candidates,
                    input.lump_minor,
                    input.hint_invoice_id.as_deref(),
                    strategy,
                ),
                policy_ref,
            ),
        };
        if splits.is_empty() {
            return Err(DomainError::InvalidRequest(
                "no open AR to allocate".to_owned(),
            ));
        }
        // Size bound on the WRITE: reject only when the split itself touches more
        // than `self.max_invoices` invoices (each is a posted AR leg), so a payer
        // with a large open backlog whose payment reaches only a few of them
        // allocates fine. A lump large enough to pay > cap invoices at once still
        // rejects here (chunked continuations are a tracked follow-up). The cap is
        // `payments.max_invoices_per_allocation` (default MAX_INVOICES_PER_ALLOCATION).
        if splits.len() > self.max_invoices {
            return Err(DomainError::AllocationTooLarge(format!(
                "allocation touches {} invoices (max {})",
                splits.len(),
                self.max_invoices
            )));
        }
        let total: i64 = splits.iter().map(|s| s.amount_minor).sum();

        // 4. Build the balanced Pattern-A-apply entry, overwrite the placeholder
        //    header, and bind chart account_ids.
        let mut entry = build_allocation_entry(&AllocationInput {
            tenant_id: input.tenant_id,
            payer_tenant_id: input.payer_tenant_id,
            payment_id: input.payment_id.clone(),
            allocation_id: input.allocation_id,
            currency: input.currency.clone(),
            splits: splits.clone(),
            // 2a: allocation posts effective-now; thread a request field here if
            // a back-dated allocation is ever needed.
            effective_at: None,
        })?;
        overwrite_header(&mut entry, ctx);

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

        // 5. Slice 5 (F1): realized FX on a cross-currency close. When the relieved
        //    grains carry a functional balance, stamp each line's functional relief
        //    at its grain's carried value (WAC pro-rata for a partial close) and
        //    append the net FX_GAIN_LOSS line so the functional column balances.
        //    A single-currency close (functional NULL on the pool grain) is a no-op
        //    — functional stays NULL, byte-green. Runs on BOTH the inline and the
        //    deferred-apply paths (this method is shared), so a queued allocation
        //    drained after a rate move also posts the correct realized FX.
        self.apply_realized_fx(scope, input, &mut entry, &ar_carried, total, &chart)
            .await?;

        Ok(BuiltAllocation {
            entry,
            splits,
            policy_ref,
            total,
        })
    }

    /// Post realized FX onto a cross-currency allocation close (Slice 5 F1, design
    /// §3.5 / §4.4). The allocation relieves the payer's unallocated pool
    /// (DR UNALLOCATED) into their open receivables (CR AR per split); when those
    /// grains were posted at a rate ≠ today's, the functional value relieved on the
    /// DR leg differs from the sum relieved on the CR legs, and that net imbalance
    /// is the realized gain/loss.
    ///
    /// Reads each relieved grain's carried functional value (the pool via
    /// [`PaymentRepo::read_unallocated_carried`], each AR invoice via the
    /// `ar_carried` snapshot) and feeds the closing legs — IN entry-line order, so
    /// the per-leg relief maps 1:1 back onto the lines — to the pure
    /// [`realize`](crate::domain::fx::realized::realize). It then stamps each
    /// existing line's `functional_amount_minor` / `functional_currency` and
    /// appends the single net `FX_GAIN_LOSS` functional-only line (`amount_minor =
    /// 0`) bound via `ChartIndex::resolve(FxGainLoss, functional_ccy, None)`. The
    /// projector closes each grain's functional column from these stamped lines and
    /// the dual-column commit trigger validates the functional balance (fail-loud).
    ///
    /// **No realized FX** (returns leaving functional NULL) when:
    /// - the pool grain carries no functional balance — a single-currency close
    ///   (the cross-currency detect, design decision 8: functional is stamped ⟺
    ///   the position is cross-currency); or
    /// - the allocation would drain more than the pool holds — an invalid allocate
    ///   the projector rejects with `NegativeBalance`; skipping realized FX lets
    ///   that cleaner rejection surface rather than a `realize` range misuse.
    ///
    /// No new base rate is locked — an allocation close relieves at the **carried**
    /// rate (design §4.3); `rate_snapshot_ref` stays `None` on the allocate entry
    /// (provenance is the carried grains). `MAX_INVOICES_PER_ALLOCATION` bounds the
    /// AR leg count, so the FX entry never overruns the engine's per-entry ceiling.
    ///
    /// # Errors
    /// [`DomainError::AccountClosed`] when no `FX_GAIN_LOSS` account is provisioned
    /// for the functional currency; [`DomainError::Internal`] on a carried-read
    /// fault or a `realize` misuse (a malformed closing leg — an internal
    /// invariant breach, not a business condition).
    async fn apply_realized_fx(
        &self,
        scope: &AccessScope,
        input: &AllocateRequest,
        entry: &mut PostEntry,
        ar_carried: &HashMap<String, (i64, Option<i64>)>,
        total: i64,
        chart: &ChartIndex,
    ) -> Result<(), DomainError> {
        // Read the pool's carried functional value (the DR UNALLOCATED leg's grain).
        let pool = self
            .repo
            .read_unallocated_carried(
                scope,
                input.tenant_id,
                input.payer_tenant_id,
                &input.currency,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("read unallocated carried: {e}")))?;

        // Cross-currency detect (design decision 8): the relieved pool grain carries
        // a functional balance (S2 settle stamped it). NULL ⇒ a single-currency
        // close: leave every functional column NULL (byte-green) and post no FX line.
        let (Some(pool_functional), Some(functional_ccy)) = (
            pool.functional_balance_minor,
            pool.functional_currency.clone(),
        ) else {
            return Ok(());
        };

        // Pool-underflow guard: an allocate draining more than the pool holds is
        // invalid — the projector rejects it with `NegativeBalance`. Skip realized
        // FX so that cleaner rejection surfaces rather than a `realize`
        // `RelievedOutOfRange` mapped to a 500 (the close never posts either way).
        if total > pool.balance_minor {
            return Ok(());
        }

        // Build the closing legs IN entry-line order (DR UNALLOCATED first, then one
        // CR AR per split — `build_allocation_entry`'s order) so `realize`'s per-leg
        // relief maps 1:1 back onto `entry.lines`. Each AR leg is valued at its
        // invoice's carried functional, with an identity fallback (`functional ≡
        // transaction`) for a single-currency AR grain so an all-cross entry never
        // leaves a line functional-NULL (the trigger's all-or-nothing rule).
        let mut legs: Vec<ClosingLeg> = Vec::with_capacity(entry.lines.len());
        for line in &entry.lines {
            let (carried_transaction, carried_functional) = match line.account_class {
                AccountClass::Unallocated => (pool.balance_minor, pool_functional),
                AccountClass::Ar => {
                    let invoice_id = line.invoice_id.as_deref().ok_or_else(|| {
                        DomainError::Internal(
                            "realized FX: AR allocation line carries no invoice_id".to_owned(),
                        )
                    })?;
                    let (bal, func) = ar_carried.get(invoice_id).copied().ok_or_else(|| {
                        DomainError::Internal(format!(
                            "realized FX: no carried AR grain for invoice {invoice_id}"
                        ))
                    })?;
                    (bal, func.unwrap_or(bal))
                }
                other => {
                    return Err(DomainError::Internal(format!(
                        "realized FX: unexpected allocation line class {}",
                        other.as_str()
                    )));
                }
            };
            legs.push(ClosingLeg {
                side: line.side,
                carried_functional_minor: carried_functional,
                carried_transaction_minor: carried_transaction,
                relieved_transaction_minor: line.amount_minor,
            });
        }

        // Compute the per-leg functional relief + the net FX_GAIN_LOSS line.
        let realized =
            realize(&legs).map_err(|e| DomainError::Internal(format!("realized FX: {e}")))?;

        // Stamp the functional relief onto each existing line (same order as legs).
        for (line, func) in entry.lines.iter_mut().zip(&realized.leg_functional_minor) {
            line.functional_amount_minor = Some(*func);
            line.functional_currency = Some(functional_ccy.clone());
        }

        // Append the net FX_GAIN_LOSS functional-only line so the functional column
        // balances. `None` ⇒ closed at the carried rate: the relief legs above
        // already net to zero in the functional column, so no FX line is needed
        // (but the legs were still stamped — all-or-nothing for a cross-currency
        // entry).
        if let Some(fx_line) = realized.fx_line {
            let account_id = chart
                .resolve(AccountClass::FxGainLoss, &functional_ccy, None)
                .ok_or_else(|| {
                    DomainError::AccountClosed(format!(
                        "no provisioned FX_GAIN_LOSS account for functional currency {functional_ccy}"
                    ))
                })?;
            entry.lines.push(fx_gain_loss_line(
                input,
                account_id,
                &functional_ccy,
                fx_line.side,
                fx_line.functional_minor,
            ));
            // Realized-FX amount metric (§9): the sign-by-role direction is the
            // FX_GAIN_LOSS side — a CREDIT is a gain, a DEBIT a loss (the same
            // convention `RealizedFxLine` documents); the magnitude is the
            // non-negative functional amount. Emitted only on a real FX line (a
            // carried-rate close posts none). This is the fresh-build path (the
            // replay short-circuit returned earlier), so it counts once per
            // cross-currency close; a subsequent post rollback is rare and would
            // over-count this volume signal by one (acceptable for an observability
            // counter — the money truth is the posted FX_GAIN_LOSS line, not this).
            let direction = match fx_line.side {
                Side::Credit => "gain",
                Side::Debit => "loss",
            };
            self.metrics
                .fx_realized_minor(fx_line.functional_minor, &functional_ccy, direction);
        }

        Ok(())
    }

    /// Early dedup short-circuit for an allocation replay (the §4.7 counterpart
    /// to `CreditApplicationService::replay_if_posted`), reading the
    /// `(tenant, PAYMENT_ALLOCATE, allocation_id)` dedup status ONCE:
    ///
    /// - `QUEUED` ⇒ a prior intake already enqueued this allocation: read the
    ///   queue row's `queued_at` (the dedup row carries none) and return the
    ///   [`AllocationOutcome::Queued`] handle (idempotent replay during the
    ///   queued window).
    /// - `POSTED` ⇒ the allocation already applied — inline, OR queued-then-
    ///   drained by the apply path. Return an [`AllocationOutcome::Applied`]
    ///   *replay* with EMPTY splits and an empty `policy_ref`: the splits/ref were
    ///   returned on the first (posting) call, and a replay only needs to confirm
    ///   the prior posting handle. This mirrors the credit short-circuit's
    ///   empty-vec replay, and crucially lets a queued-then-applied allocation
    ///   replay cleanly here instead of falling through and tripping the engine's
    ///   replay path inside `post()`.
    /// - `CLAIMED` (an in-flight inline post) / absent ⇒ `None`: fall through to
    ///   the settlement gate.
    ///
    /// Runs out-of-txn (racy by nature, like `lookup_dedup_status`); the
    /// authoritative dedup is the intake txn's `claim_queued` (or the apply txn's
    /// `read`), so a `None` that races a concurrent first intake still serializes
    /// there.
    async fn replay_short_circuit(
        &self,
        scope: &AccessScope,
        input: &AllocateRequest,
    ) -> Result<Option<AllocationOutcome>, DomainError> {
        let business_id = input.allocation_id.to_string();
        let dedup = self
            .repo
            .lookup_dedup_status(
                scope,
                input.tenant_id,
                SourceDocType::PaymentAllocate,
                &business_id,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("allocate dedup lookup: {e}")))?;
        let Some((status, result_entry_id, stored_hash)) = dedup else {
            return Ok(None);
        };
        // A replay must carry the SAME request payload. A reuse of `allocation_id`
        // with a different lump / currency / payer / hint / splits is an
        // idempotency-key conflict, not a replay — reject it rather than silently
        // returning the prior result. (A `CLAIMED` in-flight inline post falls
        // through below; the engine's in-txn claim makes the same comparison.)
        if (status == STATUS_POSTED || status == STATUS_QUEUED)
            && stored_hash != allocation_request_hash(input)?
        {
            return Err(DomainError::IdempotencyConflict(format!(
                "allocation_id {} reused with a different payload",
                input.allocation_id
            )));
        }
        if status == STATUS_POSTED {
            // Applied already (inline or queued-then-drained). A POSTED row always
            // carries a finalized `result_entry_id` (the finalize stamps it in the
            // same txn that flips the status); guard the invariant rather than
            // fabricate a nil id.
            let entry_id = result_entry_id.ok_or_else(|| {
                DomainError::Internal(format!(
                    "dedup POSTED but no result_entry_id for \
                     ({}, {FLOW_PAYMENT_ALLOCATE}, {business_id})",
                    input.tenant_id
                ))
            })?;
            return Ok(Some(AllocationOutcome::Applied(AppliedAllocation {
                posting: PostingRef {
                    entry_id,
                    // Replay: the sequence is not re-read (callers key on the id —
                    // mirrors the engine's own replay `PostingRef`).
                    created_seq: 0,
                    replayed: true,
                },
                // Empty on replay: the splits + policy ref were returned on the
                // first (posting) call (mirrors the credit short-circuit).
                splits: vec![],
                policy_ref: String::new(),
            })));
        }
        if status != STATUS_QUEUED {
            // CLAIMED (an in-flight inline post): not a replay — fall through.
            return Ok(None);
        }
        // The dedup says QUEUED; surface `queued_at` from the work-state queue
        // row. A missing row here would be an invariant breach (both rows are
        // written in the same intake txn) — surface it rather than fabricate.
        let row = self
            .pending_queue
            .get(scope, input.tenant_id, FLOW_PAYMENT_ALLOCATE, &business_id)
            .await
            .map_err(|e| DomainError::Internal(format!("allocate queue read: {e}")))?
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "dedup QUEUED but no queue row for ({}, {FLOW_PAYMENT_ALLOCATE}, {business_id})",
                    input.tenant_id
                ))
            })?;
        Ok(Some(AllocationOutcome::Queued(QueuedAllocation {
            flow: FLOW_PAYMENT_ALLOCATE.to_owned(),
            business_id,
            queued_at: row.queued_at,
        })))
    }

    /// Intake for an allocate of a not-yet-settled payment (§4.7): claim the
    /// dedup row as `QUEUED` and insert the work-state queue row, in ONE
    /// `db.transaction` (mirrors `infra/jobs/period_open.rs`'s txn shape + its
    /// `DbError::Sea(DbErr::Custom(...))` error encoding — the closure error type
    /// is fixed to `DbError`, so a `RepoError` is encoded as `DbErr::Custom` and
    /// surfaced after the transaction).
    ///
    /// The dedup `payload_hash` is **request-based** (`content_hash` over the
    /// canonical JSON of the [`QueuedAllocationPayload`]) — NOT the inline post's
    /// entry-based `payload_hash` — because the per-invoice split is only decided
    /// at apply, so the entry doesn't exist at intake. `claim_queued`'s `Replay`
    /// makes the intake idempotent: a concurrent / retried intake that loses the
    /// claim race returns the existing `Queued` handle instead of double-enqueuing
    /// (the early `replay_short_circuit` already caught the common retry; this
    /// guards the race that slips past it).
    async fn enqueue_allocation(
        &self,
        scope: &AccessScope,
        input: &AllocateRequest,
    ) -> Result<QueuedAllocation, DomainError> {
        let now = OffsetDateTime::now_utc();
        let business_id = input.allocation_id.to_string();
        let payload = QueuedAllocationPayload::from_request(input);
        // Canonical JSON of the PII-free payload: the queue row's `payload` jsonb
        // AND (hashed) the request-based dedup key. `to_value` cannot fail for a
        // plain derive-Serialize struct of scalars/strings; map defensively.
        let payload_json = serde_json::to_value(&payload)
            .map_err(|e| DomainError::Internal(format!("serialize queue payload: {e}")))?;
        let payload_hash = allocation_request_hash(input)?;

        let tenant = input.tenant_id;
        let gate = IdempotencyGate::new();
        // Own everything the closure needs (the closure is `FnOnce`, so the
        // captures move straight into the async future — no inner re-clone). The
        // intake-txn shape + the `DbError::Sea(DbErr::Custom(...))` error encoding
        // mirror `infra/jobs/period_open.rs`.
        let scope_owned = scope.clone();
        let outcome = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    // Claim the dedup row as QUEUED. A first claim ⇒ insert the
                    // queue row; a Replay ⇒ the row already exists (idempotent).
                    let claim = gate
                        .claim_queued(
                            txn,
                            tenant,
                            FLOW_PAYMENT_ALLOCATE,
                            &business_id,
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
                                    flow: FLOW_PAYMENT_ALLOCATE.to_owned(),
                                    business_id: business_id.clone(),
                                    payload: payload_json,
                                    queued_at: now,
                                    // Immediately eligible for the drain once the
                                    // settlement lands (no apply delay in v1).
                                    apply_after: None,
                                },
                            )
                            .await
                            .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                            // First intake: `now` is the authoritative queued_at.
                            Ok::<IntakeOutcome, DbError>(IntakeOutcome::Enqueued { queued_at: now })
                        }
                        ClaimOutcome::Replay(row) => {
                            // A concurrent/retried intake won the claim first.
                            // FIRST guard the payload: a reuse of `allocation_id`
                            // with a DIFFERENT request is an idempotency conflict,
                            // not a replay. The early `replay_short_circuit` makes
                            // the same comparison; this closes the race that slips
                            // past it (a row that appeared between the early check
                            // and this in-txn claim).
                            if row.payload_hash != payload_hash {
                                Ok(IntakeOutcome::Conflict)
                            } else if row.status == STATUS_QUEUED {
                                // Same payload, already queued ⇒ idempotent.
                                Ok(IntakeOutcome::AlreadyQueued)
                            } else {
                                // Same payload but a POSTED race (the settlement
                                // landed AND Group D's apply drained it between the
                                // early check and this claim) — transient: the
                                // caller retries and the early check then returns
                                // the POSTED replay cleanly.
                                Err(DbError::Sea(DbErr::Custom(format!(
                                    "allocate intake: unexpected dedup status {:?} for \
                                     ({tenant}, {FLOW_PAYMENT_ALLOCATE}, {business_id})",
                                    row.status
                                ))))
                            }
                        }
                    }
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("allocate intake: {e}")))?;

        let business_id = input.allocation_id.to_string();
        match outcome {
            IntakeOutcome::Enqueued { queued_at } => Ok(QueuedAllocation {
                flow: FLOW_PAYMENT_ALLOCATE.to_owned(),
                business_id,
                queued_at,
            }),
            // A racing intake reused this `allocation_id` with a different payload.
            IntakeOutcome::Conflict => Err(DomainError::IdempotencyConflict(format!(
                "allocation_id {} reused with a different payload",
                input.allocation_id
            ))),
            // The claim raced and lost: the prior intake's queue row holds the
            // authoritative `queued_at` — read it out-of-txn for the handle.
            IntakeOutcome::AlreadyQueued => {
                let row = self
                    .pending_queue
                    .get(scope, input.tenant_id, FLOW_PAYMENT_ALLOCATE, &business_id)
                    .await
                    .map_err(|e| DomainError::Internal(format!("allocate queue read: {e}")))?
                    .ok_or_else(|| {
                        DomainError::Internal(format!(
                            "intake replay but no queue row for \
                             ({}, {FLOW_PAYMENT_ALLOCATE}, {business_id})",
                            input.tenant_id
                        ))
                    })?;
                Ok(QueuedAllocation {
                    flow: FLOW_PAYMENT_ALLOCATE.to_owned(),
                    business_id,
                    queued_at: row.queued_at,
                })
            }
        }
    }

    /// Apply ONE queued allocation row (Group D): deserialize the queued payload,
    /// re-gate on the settlement, RE-RUN the decide/validate/build path (so caps
    /// are re-evaluated against then-current state, §4.7), and post via
    /// [`PostingService::post_queued_apply`] with a COMPOSITE sidecar that does
    /// the allocation counter writes AND flips the queue row `→APPLIED` — both in
    /// the post txn. Returns:
    /// - [`ApplyOutcome::NotReady`] when the settlement is STILL absent (leave the
    ///   row `QUEUED`, no attempt bump — a settle/sweep retries),
    /// - [`ApplyOutcome::Applied`] on a successful post (row flipped `→APPLIED`),
    /// - [`ApplyOutcome::Blocked`] on an apply-time cap/precondition rejection
    ///   (`MoneyOutCapExceeded` / `NegativeBalance` / `PeriodClosed` /
    ///   `AllocationSplitInvalid`) — the caller bumps attempts + leaves `QUEUED`.
    ///
    /// `pub(crate)` so the [`crate::infra::payment::queue_apply::QueueApplier`]
    /// (and the sweep job, via it) can drive a single row; the public surface is
    /// [`Self::drain`].
    ///
    /// # Errors
    /// [`DomainError::Internal`] on an infra fault (bad payload, settlement read,
    /// or an engine `Internal`) — propagated so the caller can isolate the row and
    /// continue the pass.
    pub(crate) async fn apply_queued_row(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        row: &pending_event_queue::Model,
    ) -> Result<ApplyOutcome, DomainError> {
        // 1. Deserialize the PII-free financial-key snapshot + reconstruct the
        //    request (mapping the Mode B `QueuedSplit` back to the domain
        //    `Allocated`).
        let payload: QueuedAllocationPayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| DomainError::Internal(format!("deserialize queued payload: {e}")))?;
        let input = AllocateRequest::from_payload(payload);

        // 2. Re-gate on the settlement. ABSENT ⇒ NotReady (the payment still isn't
        //    settled — leave the row QUEUED, don't bump attempts; a settle/sweep
        //    retries). PRESENT ⇒ proceed (its currency must match the request).
        let Some(settlement) = self
            .repo
            .read_settlement(scope, input.tenant_id, &input.payment_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read settlement: {e}")))?
        else {
            return Ok(ApplyOutcome::NotReady);
        };
        if settlement.currency != input.currency {
            // A currency mismatch is a permanent precondition failure, not infra —
            // treat as Blocked (the caller bumps attempts; Phase 5 alarms it).
            return Ok(ApplyOutcome::Blocked(
                DomainError::AllocationCurrencyMismatch(format!(
                    "allocation currency {} != settlement currency {} for payment {}",
                    input.currency, settlement.currency, input.payment_id
                )),
            ));
        }

        // 3. Re-run the EXACT decide/validate/build path — caps thus re-evaluated
        //    against THEN-CURRENT open AR + wallet state. A decide/build rejection
        //    is an apply-time precondition failure ⇒ Blocked (not infra).
        let built = match self.decide_and_build_entry(ctx, scope, &input).await {
            Ok(built) => built,
            Err(e) if is_apply_blocked(&e) => return Ok(ApplyOutcome::Blocked(e)),
            Err(e) => return Err(e),
        };
        let BuiltAllocation {
            entry,
            splits,
            policy_ref,
            total,
        } = built;

        // 4. Composite sidecar: the allocation counter writes (reused via the
        //    wrapped `AllocationSidecar`) THEN the queue `→APPLIED` flip — both in
        //    the post txn, so the apply effect and the work-state transition
        //    commit atomically (or roll back together).
        let sidecar: Arc<dyn PostSidecar> = Arc::new(QueuedAllocationApplySidecar {
            alloc: AllocationSidecar {
                tenant: input.tenant_id,
                payer: input.payer_tenant_id,
                payment_id: input.payment_id.clone(),
                allocation_id: input.allocation_id,
                currency: input.currency.clone(),
                // Moved (not cloned): an apply does not return the splits to the
                // caller (the queue row already recorded them), so the sidecar is
                // their sole consumer here.
                splits,
                total_minor: total,
                policy_ref,
            },
            flow: row.flow.clone(),
            business_id: row.business_id.clone(),
            tenant: input.tenant_id,
        });

        // 5. Post via the queued-apply engine path. A cap/precondition surfaces as
        //    a `DomainError` ⇒ Blocked; an `Internal` propagates.
        match self
            .post_bound_queued_apply(ctx, scope, entry, sidecar)
            .await
        {
            Ok(posting) => Ok(ApplyOutcome::Applied(posting)),
            Err(e) if is_apply_blocked(&e) => Ok(ApplyOutcome::Blocked(e)),
            Err(e) => Err(e),
        }
    }

    /// Drain up to `limit` due queued allocations for one tenant: claim them under
    /// `SKIP LOCKED` in a short claim txn, then apply EACH in its OWN txn (the
    /// "apply is a second txn" shape, §4.7). `Blocked` ⇒ bump attempts + back off
    /// via `apply_after` (own txn);
    /// `NotReady` ⇒ skip (no bump). The claim and each apply are SEPARATE
    /// transactions: claiming only reserves the rows for this pass under the row
    /// lock; the authoritative work-state flip (`→APPLIED`) rides each apply's post
    /// txn. (Chosen over claim+apply-in-one-txn so a long apply doesn't hold the
    /// claim lock across the whole batch, and so a per-row failure is isolated.)
    ///
    /// Per-row infra errors are isolated (logged, counted as neither applied nor
    /// blocked) so one bad row doesn't abort the batch — mirrors the tie-out job's
    /// per-tenant isolation.
    ///
    /// # Errors
    /// [`DomainError::Internal`] only if the initial claim txn itself fails (the
    /// batch cannot start); per-row faults are swallowed within the pass.
    pub async fn drain(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        limit: u64,
    ) -> Result<DrainReport, DomainError> {
        let now = OffsetDateTime::now_utc();
        // Claim txn: reserve up to `limit` due rows under SKIP LOCKED. Returned
        // still `QUEUED` — the apply flips each. Its own short txn so the lock is
        // released before the (potentially slow) per-row applies run.
        let pending_queue = self.pending_queue.clone();
        let scope_owned = scope.clone();
        let claimed: Vec<pending_event_queue::Model> = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    pending_queue
                        .claim_due(txn, &scope_owned, tenant, FLOW_PAYMENT_ALLOCATE, now, limit)
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("drain claim: {e}")))?;

        let mut report = DrainReport::default();
        for row in claimed {
            match self.apply_queued_row(ctx, scope, &row).await {
                Ok(ApplyOutcome::Applied(_)) => report.applied += 1,
                Ok(ApplyOutcome::NotReady) => report.not_ready += 1,
                Ok(ApplyOutcome::Blocked(err)) => {
                    report.blocked += 1;
                    // Bump attempts in its OWN txn (the apply already rolled back).
                    // A bump failure is logged but does not abort the batch.
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
                            "bss-ledger: drain failed to bump attempts for blocked allocation"
                        );
                    } else {
                        tracing::warn!(
                            tenant_id = %tenant,
                            business_id = %row.business_id,
                            error = %err,
                            "bss-ledger: queued allocation blocked at apply (attempts bumped, left QUEUED)"
                        );
                    }
                }
                Err(e) => {
                    // Isolate per-row infra faults: log and continue (the row stays
                    // QUEUED, a later sweep retries) — one bad row must not abort
                    // the whole pass.
                    tracing::error!(
                        tenant_id = %tenant,
                        business_id = %row.business_id,
                        error = %e,
                        "bss-ledger: queued allocation apply failed (infra); continuing"
                    );
                }
            }
        }
        Ok(report)
    }

    /// Bump one queue row's `attempts` AND defer its next eligibility by an
    /// exponential backoff, in its own short transaction (the apply that produced
    /// the `Blocked` already rolled back, so this is a standalone write).
    /// `prior_attempts` is the claimed row's attempt count BEFORE this pass; the
    /// backoff is sized off the new count (`prior_attempts + 1`). Deferring is what
    /// stops a durably-`Blocked` ("poison") row — a currency mismatch, a chronic
    /// over-cap, a closed period — from being re-claimed on every drain pass and
    /// hot-looping CPU + DB until Phase 5 adds quarantine. Used by [`Self::drain`].
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
                        FLOW_PAYMENT_ALLOCATE,
                        &business_id,
                        defer_until,
                    )
                    .await
                    .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("drain bump attempts: {e}")))
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's
    /// `NewEntry`/`NewLine`, resolving each line's scale, and post INLINE
    /// (`PostingService::post_with_request_hash`, `ClaimMode::Fresh`) with the
    /// allocation sidecar, binding the request-based idempotency hash so a replay
    /// short-circuit can reject a same-key / different-payload reuse (the inline
    /// path thus stores the SAME request hash the queued-intake path does).
    async fn post_bound(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Arc<dyn PostSidecar>,
        request_hash: String,
    ) -> Result<PostingRef, DomainError> {
        let (new_entry, new_lines) = self.to_engine_inputs(scope, entry).await?;
        self.posting
            .post_with_request_hash(
                ctx,
                scope,
                new_entry,
                new_lines,
                Some(sidecar),
                request_hash,
            )
            .await
    }

    /// The deferred-apply twin of [`Self::post_bound`]: same mapping, but posts
    /// via [`PostingService::post_queued_apply`] (`ClaimMode::QueuedApply`) so the
    /// dedup row already claimed `QUEUED` at intake is read (not re-claimed) and
    /// finalized `QUEUED → POSTED`. The `sidecar` here is the COMPOSITE one that
    /// also flips the queue row `→APPLIED` in the same txn.
    async fn post_bound_queued_apply(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Arc<dyn PostSidecar>,
    ) -> Result<PostingRef, DomainError> {
        let (new_entry, new_lines) = self.to_engine_inputs(scope, entry).await?;
        self.posting
            .post_queued_apply(ctx, scope, new_entry, new_lines, Some(sidecar))
            .await
    }

    /// Map an already-account-bound [`PostEntry`] to the engine's
    /// `NewEntry` + `Vec<NewLine>`, resolving each line's currency scale. Shared
    /// by the inline ([`Self::post_bound`]) and deferred-apply
    /// ([`Self::post_bound_queued_apply`]) post paths.
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
            // Slice 5: the S2 allocate FX lock lands next; None until then.
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

    /// Emit the `Allocate`-labelled payment-post duration for one attempt, and —
    /// for an inline post or a rejection — the `allocation(outcome)` counter
    /// (mirrors `invoice_post::record`). A `Queued` outcome is an ENQUEUE, not a
    /// post: it records ONLY the duration and emits no `allocation()` outcome
    /// (the post — and thus the Posted/Replayed/Rejected outcome — happens later
    /// on the drain, Group D).
    fn record(&self, result: &Result<AllocationOutcome, DomainError>, started: Instant) {
        match result {
            Ok(AllocationOutcome::Applied(applied)) => {
                let outcome = if applied.posting.replayed {
                    PostResult::Replayed
                } else {
                    PostResult::Posted
                };
                self.metrics.allocation(outcome);
            }
            Err(_) => self.metrics.allocation(PostResult::Rejected),
            // Enqueue: duration only, no outcome counter (see the doc above).
            Ok(AllocationOutcome::Queued(_)) => {}
        }
        self.metrics
            .payment_post_duration(started.elapsed().as_secs_f64(), PostFlow::Allocate);
    }
}

/// Overwrite the placeholder header fields the pure builder emits: an allocation
/// posts effective-now, so derive the `period_id` (YYYYMM) and `effective_at`
/// from the wall clock, stamp the actor from the security context, and mint a
/// fresh correlation id. If `period_id` stayed `""` the post would fail the
/// fiscal-period gate, so this overwrite is mandatory.
fn overwrite_header(entry: &mut PostEntry, ctx: &SecurityContext) {
    let eff_date = to_naive_date(OffsetDateTime::now_utc());
    entry.effective_at = eff_date;
    entry.period_id = format!("{:04}{:02}", eff_date.year(), eff_date.month());
    entry.posted_by_actor_id = ctx.subject_id();
    entry.correlation_id = Uuid::now_v7();
}

/// Thin `PostLine` adapter over [`ChartIndex::resolve`]: the allocation classes
/// (UNALLOCATED / AR) are stream-less, so this resolves on `stream = None`.
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

/// Build the single net `FX_GAIN_LOSS` line that balances a realized-FX
/// allocation close (Slice 5 F1, design §3.5). It is **functional-only**:
/// `amount_minor = 0` (so it sits outside the transaction-column zero-sum and the
/// dual-column trigger exempts an `amount_minor = 0` functional-NOT-NULL line
/// from the entry-currency match), while `functional_amount_minor = fx_minor > 0`
/// carries the realized gain/loss — passing the tightened `amount_minor > 0 OR
/// (amount_minor = 0 AND functional_amount_minor > 0)` CHECK on the functional
/// arm. `currency = functional_ccy` matches the FX account's currency (bound via
/// `ChartIndex::resolve(FxGainLoss, functional_ccy, None)`); `side` is the
/// sign-by-role from [`realize`](crate::domain::fx::realized::realize) — a
/// realized **loss** is a debit, a **gain** a credit. Mirrors the per-line shape
/// the allocation [`build_allocation_entry`](crate::domain::payment::allocation::build_allocation_entry)
/// emits (payer + seller stamped, the rest `None`).
fn fx_gain_loss_line(
    input: &AllocateRequest,
    account_id: Uuid,
    functional_ccy: &str,
    side: Side,
    fx_minor: i64,
) -> PostLine {
    PostLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: input.payer_tenant_id,
        seller_tenant_id: Some(input.tenant_id),
        resource_tenant_id: None,
        account_id,
        account_class: AccountClass::FxGainLoss,
        gl_code: None,
        side,
        amount_minor: 0,
        currency: functional_ccy.to_owned(),
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: Some(fx_minor),
        functional_currency: Some(functional_ccy.to_owned()),
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

/// Is this rejection an apply-time cap / precondition failure (so the drain
/// leaves the row `QUEUED` + bumps attempts), as opposed to an infra fault (which
/// propagates)? The blocked set is the cap/precondition family the apply can hit
/// at apply-time against then-current state (§4.7): the per-payment money-out cap,
/// the projector's no-negative guard, a closed fiscal period, and an invalid
/// caller split. `AllocationTooLarge` / `AccountClosed` are also precondition
/// failures the decide/build path can raise — included so a transiently
/// unprovisioned chart or an oversized candidate set is retried, not surfaced as
/// infra. Everything else (notably [`DomainError::Internal`]) is infra.
fn is_apply_blocked(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::MoneyOutCapExceeded(_)
            | DomainError::NegativeBalance(_)
            | DomainError::PeriodClosed(_)
            | DomainError::AllocationSplitInvalid(_)
            | DomainError::AllocationCurrencyMismatch(_)
            | DomainError::AllocationTooLarge(_)
            | DomainError::AccountClosed(_)
            | DomainError::InvalidRequest(_)
    )
}

/// Exponential backoff (wall-clock) before a `Blocked` queued allocation may be
/// re-claimed, sized off the post-bump attempt count: ~`2^(attempts-1)` seconds,
/// capped at 5 minutes. This keeps a durably-`Blocked` ("poison") row — one whose
/// block is NOT transient (a currency mismatch, a chronic over-cap) — from being
/// re-applied on every drain / sweep pass (a CPU + DB hot-loop) before Phase 5
/// adds attempt-based quarantine. A transiently-blocked row (a period that later
/// reopens, an AR that reopens) simply waits out the backoff before its next try.
fn blocked_backoff(attempts: i64) -> Duration {
    const BASE_SECS: i64 = 2;
    const MAX_SECS: i64 = 300;
    // Clamp the shift so `1 << shift` cannot overflow; the cap dominates long
    // before then (2^9 · 2s already exceeds the 300s ceiling).
    let shift = attempts.clamp(1, 16) - 1;
    let secs = BASE_SECS.saturating_mul(1_i64 << shift).min(MAX_SECS);
    Duration::seconds(secs)
}

/// The request-based idempotency hash for an allocate — the `content_hash` of the
/// canonical [`QueuedAllocationPayload`]. It is STABLE across the apply's
/// state-dependent entry rebuild, so it (not the entry-based hash) is what the
/// dedup row stores on BOTH the queued-intake and the inline-post paths, and what
/// [`AllocationService::replay_short_circuit`] compares to reject a same
/// `allocation_id` reused with a different payload.
fn allocation_request_hash(input: &AllocateRequest) -> Result<String, DomainError> {
    let mut payload = QueuedAllocationPayload::from_request(input);
    // Order-independent: a Mode-B caller resending the SAME splits in a different
    // order must hash identically (mirrors credit's sorted `apply_request_hash`
    // targets), else an order-varying retry would spuriously trip
    // `IdempotencyConflict`. Sorting here affects ONLY the dedup fingerprint — the
    // queue row stores the caller's original order separately for apply.
    if let Some(splits) = payload.caller_splits.as_mut() {
        splits
            .sort_by(|a, b| (&a.invoice_id, a.amount_minor).cmp(&(&b.invoice_id, b.amount_minor)));
    }
    let canonical = serde_json::to_string(&payload)
        .map_err(|e| DomainError::Internal(format!("canonicalize allocate payload: {e}")))?;
    Ok(IdempotencyGate::content_hash(&canonical))
}

/// Composite in-transaction sidecar for the deferred apply of a queued
/// allocation (Group D): it runs the SAME allocation counter writes as the inline
/// path (by delegating to the wrapped [`AllocationSidecar`]) and THEN flips the
/// work-state queue row `→APPLIED` — both inside the post txn opened by
/// [`PostingService::post_queued_apply`]. So the allocation effect and the
/// queue-row transition commit atomically, or roll back together: a cap rejection
/// in `add_allocated` rolls back the `→APPLIED` flip too, leaving the row
/// claimable again on a later pass.
struct QueuedAllocationApplySidecar {
    /// The inline allocation sidecar — reused verbatim so the counter-write logic
    /// (bump `allocated_minor` under the cap CHECK, insert rows, bump refund
    /// counters) is not duplicated.
    alloc: AllocationSidecar,
    /// The queue row's `flow` (the `PAYMENT_ALLOCATE` literal) — its PK part.
    flow: String,
    /// The queue row's `business_id` (the allocation id string) — its PK part.
    business_id: String,
    /// The queue row's tenant — its PK part (and the scope tenant).
    tenant: Uuid,
}

#[async_trait::async_trait]
impl PostSidecar for QueuedAllocationApplySidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. The allocation counter writes (delegated — the per-payment cap CHECK
        //    is re-evaluated here against then-current `allocated_minor`).
        self.alloc.run(txn, scope, posted).await?;
        // 2. Flip the work-state queue row `→APPLIED` in the SAME txn. A zero-row
        //    update (the row vanished / was already terminal) surfaces as a repo
        //    error → DomainError::Internal, rolling the whole apply back.
        PendingQueueRepo::mark_applied(txn, scope, self.tenant, &self.flow, &self.business_id)
            .await
            .map_err(|e| DomainError::Internal(format!("queue-apply mark_applied: {e}")))?;
        Ok(())
    }
}
