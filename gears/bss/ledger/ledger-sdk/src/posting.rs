//! Posting request/response DTOs for the in-process data-access API.
//! Handlers build balanced lines from these; the foundation persists them.
//! All amounts are `i64` minor units.

use chrono::NaiveDate;
use uuid::Uuid;

use crate::enums::{AccountClass, MappingStatus, Side, SourceDocType};
use time::OffsetDateTime;

/// One balanced journal line to post. Mirrors the `journal_line` columns a
/// handler supplies; the foundation fills DB-generated/derived fields.
#[derive(Clone, Debug)]
pub struct PostLine {
    pub line_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub seller_tenant_id: Option<Uuid>,
    pub resource_tenant_id: Option<Uuid>,
    pub account_id: Uuid,
    pub account_class: AccountClass,
    pub gl_code: Option<String>,
    pub side: Side,
    pub amount_minor: i64,
    pub currency: String,
    pub invoice_id: Option<String>,
    pub due_date: Option<NaiveDate>,
    pub revenue_stream: Option<String>,
    pub mapping_status: MappingStatus,
    pub functional_amount_minor: Option<i64>,
    pub functional_currency: Option<String>,
    pub tax_jurisdiction: Option<String>,
    pub tax_filing_period: Option<String>,
    pub tax_rate_ref: Option<String>,
    pub invoice_item_ref: Option<String>,
    pub sku_or_plan_ref: Option<String>,
    pub price_id: Option<String>,
    pub pricing_snapshot_ref: Option<String>,
    pub po_allocation_group: Option<String>,
    pub credit_grant_event_type: Option<String>,
    /// AR dispute sub-class (`ACTIVE`/`DISPUTED`), set on the two AR legs of a
    /// chargeback reclass; `None` on every other line. The projector routes a
    /// `DISPUTED` AR line's signed amount into `ar_invoice_balance.disputed_minor`
    /// while the reclass nets ZERO on `balance_minor` (AR-class-neutral).
    pub ar_status: Option<String>,
}

/// A balanced entry to post: header fields + its lines. The legal entity is
/// NOT supplied by the caller — in v1 there is exactly one legal entity per
/// tenant and the server derives it (= `tenant_id`); the DB column is retained
/// for future multi-LE.
#[derive(Clone, Debug)]
pub struct PostEntry {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub period_id: String,
    pub entry_currency: String,
    pub source_doc_type: SourceDocType,
    pub source_business_id: String,
    pub effective_at: NaiveDate,
    pub posted_by_actor_id: Uuid,
    pub correlation_id: Uuid,
    /// The original entry this post reverses, set for a `REVERSAL` /
    /// `MAPPING_CORRECTION` post; `None` for an `INVOICE_POST`. Persisted on
    /// the `journal_entry` header (`reverses_entry_id`).
    pub reverses_entry_id: Option<Uuid>,
    /// The original entry's `period_id`, paired with [`Self::reverses_entry_id`]
    /// (a reversal may post into a later period than the original).
    pub reverses_period_id: Option<String>,
    pub lines: Vec<PostLine>,
}

/// Reference to a posted (or replayed) entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostingRef {
    pub entry_id: Uuid,
    pub created_seq: i64,
    /// True when the call was an idempotent replay of a prior post.
    pub replayed: bool,
}

/// A read-back journal entry header plus its lines. Returned by
/// `LedgerClientV1::get_entry`; the gear maps the `journal_entry` row + its
/// `journal_line` rows into this. Infra-free: a plain struct mirroring
/// [`PostEntry`] on the read side, carrying the DB-derived fields a caller
/// needs to audit (`posted_at_utc`, `posted_by_actor_id`, `origin`,
/// `created_seq`) and to build a reversal (`reverses_entry_id`).
#[derive(Clone, Debug)]
pub struct EntryView {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub period_id: String,
    pub entry_currency: String,
    pub source_doc_type: SourceDocType,
    pub source_business_id: String,
    pub reverses_entry_id: Option<Uuid>,
    pub reverses_period_id: Option<String>,
    pub posted_at_utc: OffsetDateTime,
    pub effective_at: NaiveDate,
    pub posted_by_actor_id: Uuid,
    pub origin: String,
    pub correlation_id: Uuid,
    pub created_seq: i64,
    pub lines: Vec<LineView>,
}

/// A read-back journal line. Returned inside [`EntryView`] and by
/// `LedgerClientV1::list_lines`. Carries the posted dims a caller filters /
/// reconciles on (`payer_tenant_id`, `account_class`, `revenue_stream`, the
/// tax dims, `mapping_status`).
#[derive(Clone, Debug)]
pub struct LineView {
    pub line_id: Uuid,
    pub entry_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub account_id: Uuid,
    pub account_class: AccountClass,
    pub gl_code: Option<String>,
    pub side: Side,
    pub amount_minor: i64,
    pub currency: String,
    pub currency_scale: u8,
    pub invoice_id: Option<String>,
    pub due_date: Option<NaiveDate>,
    pub revenue_stream: Option<String>,
    pub mapping_status: MappingStatus,
    /// Functional-currency translation stamped on a cross-currency line (Slice 5):
    /// `Some` when a functional rate was locked at post time, `None` on a
    /// single-currency line (equals `amount_minor` by identity). Exposed so a
    /// reversal can reconstruct the original functional and net it to zero.
    pub functional_amount_minor: Option<i64>,
    /// Functional currency of `functional_amount_minor`; `None` single-currency.
    pub functional_currency: Option<String>,
    pub tax_jurisdiction: Option<String>,
    pub tax_filing_period: Option<String>,
    /// AR dispute sub-class (`ACTIVE`/`DISPUTED`) snapshot on the line; `None`
    /// on non-dispute lines. Lets a reversal land on the same AR sub-grain.
    pub ar_status: Option<String>,
}

/// A read-back account-balance cache row. Returned by
/// `LedgerClientV1::list_balances`. The signed `balance_minor` is the cached
/// normal-side-positive balance at the `(tenant, account, currency)` grain.
#[derive(Clone, Debug)]
pub struct BalanceView {
    pub account_id: Uuid,
    pub account_class: AccountClass,
    pub currency: String,
    pub balance_minor: i64,
    /// Functional-currency carried balance (Slice 5). `Some` only on a
    /// cross-currency grain (a functional translation was stamped); `None` on a
    /// single-currency grain, where the functional value equals `balance_minor`
    /// by identity (P1 decision 8 — the `?valuation=functional` read falls back to
    /// `balance_minor`).
    pub functional_balance_minor: Option<i64>,
    /// The functional currency of `functional_balance_minor`; `None` on a
    /// single-currency grain (equals `currency` by identity).
    pub functional_currency: Option<String>,
}

/// A read-back per-invoice AR-balance cache row. Returned by
/// `LedgerClientV1::list_ar_invoice_balances`; `due_date` drives AR-aging.
#[derive(Clone, Debug)]
pub struct ArInvoiceBalanceView {
    pub payer_tenant_id: Uuid,
    pub account_id: Uuid,
    pub invoice_id: String,
    pub currency: String,
    pub balance_minor: i64,
    pub due_date: Option<NaiveDate>,
}

/// A settled payment to record (the **money-in** side). The gross is what the
/// payer was charged; `fee_minor` is the processor's withheld cut (`<= gross`).
/// `scale` is the payment's currency scale as known to the caller; the ledger
/// resolves the authoritative per-line scale from the provisioned currency
/// config, so this is advisory. `effective_at` `None` ⇒ the receipt is stamped
/// at post time. Consumed by `LedgerClientV1::settle_payment`.
#[derive(Clone, Debug)]
pub struct SettlePayment {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub payment_id: String,
    pub gross_minor: i64,
    pub fee_minor: i64,
    pub currency: String,
    pub scale: u8,
    pub effective_at: Option<OffsetDateTime>,
}

/// A settlement to claw back (the **reversal of a money-in**). Records that the
/// PSP returned a previously-settled receipt: it removes `amount_minor` from the
/// payer's unallocated pool (`DR UNALLOCATED` / `CR CASH_CLEARING`) and
/// decrements the original payment's `settled_minor`. `psp_return_id` is the
/// idempotency key (a re-post replays). `scale` is advisory (as [`SettlePayment`]);
/// `effective_at` `None` ⇒ the return is stamped at post time. Consumed by
/// `LedgerClientV1::return_payment`.
#[derive(Clone, Debug)]
pub struct ReturnPayment {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    /// The original settled payment being clawed back.
    pub payment_id: String,
    /// External return identity — the idempotency key.
    pub psp_return_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub scale: u8,
    pub effective_at: Option<OffsetDateTime>,
}

/// A chargeback dispute phase to record (the dispute state machine, §4.5). One
/// endpoint (`POST /disputes/{dispute_id}/phases`) records one phase of a
/// dispute; the LEDGER chooses the **variant** at `opened` from `funds_at_open`
/// (`"withheld"` ⇒ cash-hold, `"not_moved"` ⇒ AR-reclass) and the `won`/`lost`
/// outcomes branch on the recorded variant. `phase` is one of `"opened"`,
/// `"won"`, `"lost"`, `"partial"` (Group B implements `opened`). `cycle`
/// defaults to 1 and increments on a re-open. `invoice_id` is the disputed
/// `(payer, invoice)` AR grain — required for an AR-reclass `opened`, ignored
/// for cash-hold. `scale` is advisory (as [`SettlePayment`]); `effective_at`
/// `None` ⇒ the phase is stamped at post time. Consumed by
/// `LedgerClientV1::record_dispute_phase`.
#[derive(Clone, Debug)]
pub struct RecordDisputePhase {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    /// The disputed payment.
    pub payment_id: String,
    /// External dispute identity (the idempotency key's first token).
    pub dispute_id: String,
    /// The disputed `(payer, invoice)` AR grain — required for an AR-reclass
    /// `opened`, ignored for cash-hold.
    pub invoice_id: Option<String>,
    /// Re-entrancy counter (`>= 1`); defaults to 1 at the wire boundary.
    pub cycle: i32,
    /// The phase literal: `"opened" | "won" | "lost" | "partial"`.
    pub phase: String,
    /// The funds-movement fact the ledger reads at `opened` to choose the
    /// variant: `"withheld"` (card rails) ⇒ cash-hold, `"not_moved"`
    /// (invoice/ACH) ⇒ AR-reclass.
    pub funds_at_open: String,
    /// The disputed amount in minor units (`> 0`): the **gross** claim — the full
    /// amount the buyer paid / the card network reverses, NOT net of the PSP fee.
    /// A `CASH_HOLD` dispute's cash hold is sized at `net = settled − fee` by the
    /// ledger itself; this gross value drives the AR-reclass legs + the dispute row.
    pub disputed_amount_minor: i64,
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    pub effective_at: Option<OffsetDateTime>,
}

/// The outcome of a dispute-phase record that POSTED inline (the dispute had its
/// `opened` cycle, or this IS the `opened`): the posting handle. The `Recorded`
/// arm of [`DisputeOutcome`]. (A struct rather than a bare `PostingRef` so a
/// later phase can carry extra fields without a breaking change — mirrors
/// [`AllocationApplied`].)
#[derive(Clone, Debug)]
pub struct DisputeRecorded {
    pub posting: PostingRef,
}

/// The outcome of a dispute-phase record that was DEFERRED because the `won`/
/// `lost` arrived before its `opened` (§4.7 out-of-order): the request was
/// durably queued onto `ledger_pending_event_queue` and will be applied by the
/// drain once the `opened` lands. Carries the queue key (`flow` + `business_id`)
/// and the `queued_at` instant — the surface for the REST 202
/// `dispute-phase-queued` body. No `PostingRef`: nothing has posted yet. The
/// `Queued` arm of [`DisputeOutcome`] (mirrors [`AllocationQueued`]).
#[derive(Clone, Debug)]
pub struct DisputeQueued {
    /// The deferred-apply queue flow (the `CHARGEBACK` source-doc literal).
    pub flow: String,
    /// The queue/dedup business id — `dispute_id:cycle:phase`.
    pub business_id: String,
    /// When the intake durably enqueued the request.
    pub queued_at: OffsetDateTime,
}

/// The result of `LedgerClientV1::record_dispute_phase`: either the phase posted
/// inline (`Recorded`) or it was durably queued because its `opened` has not
/// landed yet (`Queued`, surfaced as HTTP 202 `dispute-phase-queued`). The two
/// arms drive the handler's 201/200-vs-202 split (mirrors [`AllocateOutcome`]).
#[derive(Clone, Debug)]
pub enum DisputeOutcome {
    Recorded(DisputeRecorded),
    Queued(DisputeQueued),
}

/// One caller-computed allocation share (Mode B, §4.4 F-5): apply `amount_minor`
/// of the lump to `invoice_id`. Carried in [`AllocatePayment::splits`] when the
/// caller supplies the split instead of letting a precedence policy decide it.
#[derive(Clone, Debug)]
pub struct AllocationSplit {
    pub invoice_id: String,
    pub amount_minor: i64,
}

/// An allocation of a settled payment's unallocated pool to the payer's open
/// receivables (the **money-out** side). `allocation_id` is the idempotency key.
/// As with [`SettlePayment`], `scale` is advisory — the ledger resolves the
/// authoritative per-line scale. Consumed by `LedgerClientV1::allocate_payment`.
///
/// Two modes: when `splits` is `None` (Mode A/B precedence), `lump_minor` is
/// distributed by the tenant's precedence policy and `hint_invoice_id` jumps one
/// invoice to the front of that order. When `splits` is `Some` (Mode B escape
/// hatch), the precedence decision is skipped and the caller's explicit shares
/// are validated against the open receivables instead — they must name open
/// invoices, not over-allocate any invoice, and sum to at most `lump_minor`;
/// `hint_invoice_id` is then moot.
#[derive(Clone, Debug)]
pub struct AllocatePayment {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub payment_id: String,
    pub allocation_id: Uuid,
    pub lump_minor: i64,
    pub currency: String,
    pub scale: u8,
    pub hint_invoice_id: Option<String>,
    /// Mode B caller-computed split; `None` ⇒ the precedence policy decides.
    pub splits: Option<Vec<AllocationSplit>>,
}

/// One recorded `payment_allocation` row: how much of a payment was applied to
/// `invoice_id`, when, under which precedence policy. Returned inside
/// [`AllocationApplied`] and by `LedgerClientV1::list_payment_allocations`.
#[derive(Clone, Debug)]
pub struct AllocationView {
    pub invoice_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub allocated_at_utc: OffsetDateTime,
    pub precedence_policy_ref: String,
}

/// A read-back of the payer's unallocated pool for a currency: the still-undrained
/// portion of settled receipts. Returned by `LedgerClientV1::read_unallocated`.
#[derive(Clone, Debug)]
pub struct UnallocatedView {
    pub payer_tenant_id: Uuid,
    pub currency: String,
    pub balance_minor: i64,
}

/// The outcome of an allocate that posted inline (the payment was already
/// settled): the posting handle plus the per-invoice splits the allocation
/// applied. The `Applied` arm of [`AllocateOutcome`].
#[derive(Clone, Debug)]
pub struct AllocationApplied {
    pub posting: PostingRef,
    pub allocations: Vec<AllocationView>,
}

/// The outcome of an allocate that was DEFERRED because the payment was not yet
/// settled (§4.7 allocation-before-settlement): the request was durably queued
/// onto `ledger_pending_event_queue` and will be applied by the drain once the
/// settlement lands. Carries the queue key (`flow` + `business_id`) and the
/// `queued_at` instant the intake stamped — the surface for the REST 202
/// `allocation-queued` body. No `PostingRef`: nothing has posted yet (that
/// arrives on the drain, Group D). The `Queued` arm of [`AllocateOutcome`].
#[derive(Clone, Debug)]
pub struct AllocationQueued {
    /// The deferred-apply queue flow (the `PAYMENT_ALLOCATE` source-doc literal).
    pub flow: String,
    /// The queue/dedup business id — the allocation's `allocation_id`.
    pub business_id: String,
    /// When the intake durably enqueued the request.
    pub queued_at: OffsetDateTime,
}

/// The result of `LedgerClientV1::allocate_payment`: either the allocation posted
/// inline (`Applied`, the payment was settled) or it was durably queued for a
/// later drain (`Queued`, the payment was not yet settled — surfaced as HTTP 202
/// `allocation-queued`). The two arms drive the handler's 201-vs-202 split.
#[derive(Clone, Debug)]
pub enum AllocateOutcome {
    Applied(AllocationApplied),
    Queued(AllocationQueued),
}

/// A request to trigger an ASC 606 recognition run for one fiscal period (the
/// S6 release, design §5). `run_id` is the run-trigger idempotency key: the
/// orchestration layer dedups on `(tenant, period_id, run_id)`, so a replay
/// returns the prior run reference instead of starting a second run. `None` ⇒
/// the ledger mints a fresh `run_id` (a first, un-keyed trigger); a caller that
/// wants idempotent retries supplies a stable one. Consumed by
/// `LedgerClientV1::trigger_recognition_run`.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_field_names,
    reason = "the *_id fields mirror the run-trigger identity tuple (tenant / period / run)"
)]
pub struct TriggerRecognitionRun {
    /// The seller tenant whose ledger this releases revenue in (the PEP target).
    pub tenant_id: Uuid,
    /// The fiscal period to release due segments for (`YYYYMM`).
    pub period_id: String,
    /// The run-trigger idempotency key. `None` ⇒ the ledger mints a fresh one.
    pub run_id: Option<Uuid>,
}

/// The reference to a recognition run that EXECUTED (fresh or an idempotent
/// replay of a prior trigger): the run identity + a tally of how many due
/// segments it released this pass (`released` fresh + `replayed` already-done).
/// The `Ran` arm of [`RecognitionRunOutcome`]. (A struct rather than a bare
/// `run_id` so a later slice can carry more run facts without a breaking
/// change — mirrors [`AllocationApplied`].)
#[derive(Clone, Debug)]
pub struct RecognitionRunRef {
    /// The run that executed (minted by the trigger, or replayed).
    pub run_id: Uuid,
    /// The period the run released for (`YYYYMM`).
    pub period_id: String,
    /// `true` when this trigger replayed a prior run with the same
    /// `(tenant, period_id, run_id)` (no new run row was inserted).
    pub replayed: bool,
    /// Segments released on THIS pass (a fresh `DR CL / CR Revenue` post).
    pub released: usize,
    /// Segments that were already released (an idempotent `RECOGNITION` replay).
    pub already_recognized: usize,
}

/// The outcome of a recognition run that found work it could not release in
/// period order: at least one due segment's lower-`period_id` predecessor was
/// not yet `DONE`, so the segment was parked `QUEUED` (design §4.6 ordering)
/// rather than released early. The `Queued` arm of [`RecognitionRunOutcome`],
/// surfaced as HTTP 202 `recognition-period-queued` (a success/queued token,
/// NOT a rejection — uniform across Slices 2/3/4). A later run drains the
/// `QUEUED` segments once their predecessors commit. Mirrors [`AllocationQueued`].
#[derive(Clone, Debug)]
pub struct RecognitionRunQueued {
    /// The run that executed (it may still have released in-order segments; the
    /// queued ones are the out-of-order tail).
    pub run_id: Uuid,
    /// The period the run was triggered for (`YYYYMM`).
    pub period_id: String,
    /// Segments released in order on this pass before/around the queued ones.
    pub released: usize,
    /// Segments parked `QUEUED` this pass (a predecessor period was not `DONE`).
    pub queued: usize,
}

/// The result of `LedgerClientV1::trigger_recognition_run`: either the run
/// executed and released its due segments in order (`Ran`), or it had to park
/// one or more out-of-order segments `QUEUED` for a later drain (`Queued`,
/// surfaced as HTTP 202 `recognition-period-queued`). The two arms drive the
/// handler's 200-vs-202 split (mirrors [`AllocateOutcome`] / [`DisputeOutcome`]).
#[derive(Clone, Debug)]
pub enum RecognitionRunOutcome {
    Ran(RecognitionRunRef),
    Queued(RecognitionRunQueued),
}

/// The query for `LedgerClientV1::list_revenue_disaggregation` (the ASC 606
/// revenue-recognition-by-stream report, design §3.5 / §4.5). Recognized revenue
/// is the **DONE** recognition segments (each is a posted `DR CONTRACT_LIABILITY
/// / CR REVENUE` release), grouped by `(period_id, revenue_stream)`. `tenant_id`
/// is the seller whose recognized revenue is reported (the PEP target); a `None`
/// `period_id` reports every period, a `Some(_)` narrows to that one.
#[derive(Clone, Debug)]
pub struct RevenueDisaggregationQuery {
    /// The seller tenant whose recognized revenue is disaggregated.
    pub tenant_id: Uuid,
    /// Narrow to one fiscal period (`YYYYMM`); `None` ⇒ all periods.
    pub period_id: Option<String>,
}

/// One disaggregated recognized-revenue grain: the revenue RECOGNIZED into
/// `revenue_stream` during `period_id`, in minor units of `currency`. The sum of
/// the DONE segments' `amount_minor` at the `(period_id, revenue_stream)` grain
/// (each DONE segment posted a `DR CONTRACT_LIABILITY / CR REVENUE` release, so
/// this is the recognized-Revenue credit that period for that stream). A row of
/// [`RevenueDisaggregation::entries`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevenueDisaggregationEntry {
    /// The fiscal period the revenue recognized in (`YYYYMM`).
    pub period_id: String,
    /// The revenue stream the recognized revenue books to.
    pub revenue_stream: String,
    /// Revenue recognized into this `(period, stream)` grain, in minor units
    /// (`Σ amount_minor` of the DONE segments).
    pub recognized_minor: i64,
    /// ISO currency of the recognized amount (one account/schedule per currency).
    pub currency: String,
}

/// The result of `LedgerClientV1::list_revenue_disaggregation`: the recognized
/// revenue disaggregated by `(period_id, revenue_stream)`, ordered by
/// `(period_id, revenue_stream)`. Tenant-scoped (SQL-level BOLA): a foreign
/// tenant yields no entries.
#[derive(Clone, Debug)]
pub struct RevenueDisaggregation {
    pub entries: Vec<RevenueDisaggregationEntry>,
}

/// A read-back of one ASC 606 recognition schedule's lifecycle view (design
/// §3.7 / §4, the `GET /recognition-schedules/{schedule_id}` response). The
/// schedule header (the deferred Contract-liability obligation + its
/// recognized-to-date counter + the immutable policy/PO/subscription refs +
/// the lineage `version` and durable `status`) plus its ordered
/// [`segments`](Self::segments). Returned as `Some` by
/// [`LedgerClientV1::get_recognition_schedule`]; `None` ⇒ absent or
/// foreign-owned (SQL-level BOLA, the handler renders a 404 either way).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecognitionScheduleView {
    /// The schedule's business id (`(tenant, schedule_id)` PK tail).
    pub schedule_id: String,
    /// The durable lifecycle status (`ACTIVE` | `REPLACED` | `CANCELLED` | …).
    pub status: String,
    /// The lineage version (`0` for a freshly built schedule; `old + 1` for a
    /// `replace` successor).
    pub version: i64,
    /// The revenue stream the obligation books to (one schedule per stream).
    pub revenue_stream: String,
    /// ISO-4217 currency (one schedule/account per currency).
    pub currency: String,
    /// The total deferred Contract-liability the schedule plans to release.
    pub total_deferred_minor: i64,
    /// The cumulative recognized-to-date (`<= total_deferred_minor`, the
    /// per-obligation over-recognition cap).
    pub recognized_minor: i64,
    /// The originating posted invoice (`source_invoice_id`).
    pub source_invoice_id: String,
    /// The Contract-liability invoice line the schedule draws down
    /// (`source_invoice_item_ref`, the §4.7 invoice-link anchor).
    pub source_invoice_item_ref: String,
    /// The PO / allocation group this obligation books under (audit); `None`
    /// when the line carries no group.
    pub po_allocation_group: Option<String>,
    /// The subscription / entitlement this obligation belongs to (audit).
    pub subscription_ref: Option<String>,
    /// The immutable deferral/timing policy version stamped at build.
    pub policy_ref: String,
    /// The schedule's segments, ordered by `segment_no` (1:1 with `period_id`,
    /// so also period order).
    pub segments: Vec<RecognitionScheduleSegmentView>,
}

/// A read-back of one recognition segment (a time- or milestone-slice of a
/// [`RecognitionScheduleView`]): the `segment_no` (immutable, 1:1 with
/// `period_id`), the period it recognizes into, its minor-unit amount, and its
/// release `status` (`PENDING` | `QUEUED` | `DONE`). A row of
/// [`RecognitionScheduleView::segments`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecognitionScheduleSegmentView {
    /// The immutable segment number (1:1 with `period_id`).
    pub segment_no: i32,
    /// The fiscal period this segment recognizes into (`YYYYMM`).
    pub period_id: String,
    /// The segment's minor-unit amount.
    pub amount_minor: i64,
    /// The release status (`PENDING` | `QUEUED` | `DONE`).
    pub status: String,
}

/// The header view of a recognition schedule WITHOUT its segments — the row
/// shape of the `GET /recognition-schedules` list/discovery surface (and the
/// per-schedule reference echoed in the invoice-post response). The full
/// per-schedule view (incl. segments) is the by-id
/// `GET /recognition-schedules/{schedule_id}`. Fields mirror
/// [`RecognitionScheduleView`] minus [`RecognitionScheduleView::segments`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecognitionScheduleSummaryView {
    /// The schedule's business id (`(tenant, schedule_id)` PK tail).
    pub schedule_id: String,
    /// The durable lifecycle status (`ACTIVE` | `REPLACED` | `CANCELLED` | …).
    pub status: String,
    /// The lineage version (`0` fresh; `old + 1` for a `replace` successor).
    pub version: i64,
    /// The revenue stream the obligation books to (one schedule per stream).
    pub revenue_stream: String,
    /// ISO-4217 currency (one schedule/account per currency).
    pub currency: String,
    /// The total deferred Contract-liability the schedule plans to release.
    pub total_deferred_minor: i64,
    /// The cumulative recognized-to-date (`<= total_deferred_minor`).
    pub recognized_minor: i64,
    /// The originating posted invoice (`source_invoice_id`).
    pub source_invoice_id: String,
    /// The Contract-liability invoice line the schedule draws down.
    pub source_invoice_item_ref: String,
    /// The PO / allocation group this obligation books under; `None` when absent.
    pub po_allocation_group: Option<String>,
    /// The subscription / entitlement this obligation belongs to (audit).
    pub subscription_ref: Option<String>,
    /// The immutable deferral/timing policy version stamped at build.
    pub policy_ref: String,
}

/// The result of [`LedgerClientV1::list_recognition_schedules`]: the matching
/// schedule headers plus `truncated` — `true` when the scan hit the server cap
/// and the tail was dropped, so a client can tell a complete list from a capped
/// one (the list surface is not paginated). `Default` is the empty, untruncated
/// list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecognitionScheduleList {
    /// The matching schedule headers (`0..=cap`).
    pub schedules: Vec<RecognitionScheduleSummaryView>,
    /// `true` when the result was capped (more schedules exist than returned).
    pub truncated: bool,
}

/// One replacement recognition segment supplied on a `replace` change (design
/// §3.6 / Group H): the `(period_id, amount_minor)` slice the NEW schedule
/// version re-plans the remaining deferred over. `Σ amount_minor` of the supplied
/// segments is the new schedule's `total_deferred_minor` (= the OLD schedule's
/// remaining deferred, `total_deferred − recognized`). Carried in
/// [`ChangeRecognitionSchedule::new_segments`]; ignored on a `cancel`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSegment {
    /// Fiscal `period_id` (`YYYYMM`) this replacement segment recognizes into.
    pub period_id: String,
    /// Minor-unit amount of this segment (`>= 0`).
    pub amount_minor: i64,
}

/// A schedule change / cancel request (design §3.6 / §4.6, Group H). Targets one
/// ACTIVE `(tenant, schedule_id)` recognition schedule and either CANCELS it
/// (the unreleased deferred remainder stays as `CONTRACT_LIABILITY`; no
/// auto-reversal) or REPLACES it with a fresh schedule version that re-plans the
/// REMAINING deferred over `new_segments` (prospective — already-recognized
/// revenue is not unwound). `change_id` is the idempotency key (a replay returns
/// the prior result, minting no second schedule). `treatment` is the upstream
/// modification-accounting decision: `"prospective"` / `"separate_contract"`
/// apply directly; `"catch_up"` or any unknown value is surfaced as a
/// `MODIFICATION_TREATMENT_REVIEW` rejection (never silently prospective, §3.6) —
/// the ledger does not own the catch-up decision. `action` is `"cancel"` or
/// `"replace"`. Consumed by `LedgerClientV1::change_recognition_schedule`; the
/// target `schedule_id` is bound from the request PATH.
#[derive(Clone, Debug)]
pub struct ChangeRecognitionSchedule {
    /// The seller tenant whose schedule this changes (the PEP gate target).
    pub tenant_id: Uuid,
    /// The ACTIVE schedule being cancelled / replaced (bound from the PATH).
    pub schedule_id: String,
    /// The change idempotency key — a replay returns the prior result.
    pub change_id: String,
    /// The change action literal: `"cancel"` | `"replace"`.
    pub action: String,
    /// The upstream modification-accounting treatment: `"prospective"` |
    /// `"separate_contract"` (proceed) | `"catch_up"` / unknown (review).
    pub treatment: String,
    /// The replacement segments for a `replace` (the NEW schedule version's plan
    /// of the remaining deferred); `None` on a `cancel`.
    pub new_segments: Option<Vec<ChangeSegment>>,
}

/// The result of `LedgerClientV1::change_recognition_schedule`: a small reference
/// to the change's outcome. `schedule_id` is the original (now terminal)
/// schedule; `new_schedule_id` is the successor version's id on a `replace`
/// (`None` on a `cancel`); `status` is the original schedule's resulting durable
/// lifecycle status (`"REPLACED"` or `"CANCELLED"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleChangeRef {
    /// The original schedule that was cancelled / replaced.
    pub schedule_id: String,
    /// The successor schedule version's id on a `replace`; `None` on a `cancel`.
    pub new_schedule_id: Option<String>,
    /// The original schedule's resulting status (`"REPLACED"` | `"CANCELLED"`).
    pub status: String,
}

/// A reusable-credit operation (the wallet surface, architecture §5.2). ONE
/// endpoint (`POST /credit-applications`), two kinds: `Grant` parks unallocated
/// pool cash into the wallet; `Apply` spends the wallet against open AR. Consumed
/// by `LedgerClientV1::post_credit_application`.
#[derive(Clone, Debug)]
pub enum CreditApplication {
    Grant(CreditGrant),
    Apply(CreditApply),
}

impl CreditApplication {
    /// The seller tenant whose ledger the operation posts into — the authz gate's
    /// target, read from whichever arm the enum carries (both grant and apply
    /// name the same `tenant_id` field).
    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::Grant(g) => g.tenant_id,
            Self::Apply(a) => a.tenant_id,
        }
    }
}

/// Grant: park `amount_minor` of the payer's unallocated pool into the wallet
/// sub-grain `credit_grant_event_type` (`DR UNALLOCATED` / `CR REUSABLE_CREDIT`).
/// As with [`SettlePayment`], `scale` is advisory — the ledger resolves the
/// authoritative per-line scale from the provisioned currency config.
#[derive(Clone, Debug)]
// `credit_grant_event_type` is the canonical domain/DB term (matches the line
// field + the sub-grain column), not field-name noise — keep it as-is.
#[allow(clippy::struct_field_names)]
pub struct CreditGrant {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub credit_application_id: String,
    pub currency: String,
    pub scale: u8,
    pub amount_minor: i64,
    pub credit_grant_event_type: String,
}

/// Apply: spend the payer's reusable-credit wallet against the named open
/// receivables (oldest-grant-first draw-down). The per-invoice receivable shares
/// reuse [`AllocationSplit`] (`invoice_id` + `amount_minor`). `scale` is advisory.
#[derive(Clone, Debug)]
pub struct CreditApply {
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub credit_application_id: String,
    pub currency: String,
    pub scale: u8,
    pub targets: Vec<AllocationSplit>,
}

/// One per-sub-grain wallet draw-down an apply posted: `amount_minor` drawn from
/// the `credit_grant_event_type` bucket. A row of [`CreditApplicationApplied::debits`].
#[derive(Clone, Debug)]
pub struct CreditDebitView {
    pub credit_grant_event_type: String,
    pub amount_minor: i64,
}

/// The outcome of a grant or apply: the posting handle plus — for an apply — the
/// per-sub-grain wallet draw-downs (`debits`) and the per-invoice receivable
/// shares (`applications`, reusing [`AllocationSplit`]). A grant moves no
/// wallet/AR splits, so both vecs are empty. Returned by
/// `LedgerClientV1::post_credit_application`.
#[derive(Clone, Debug)]
pub struct CreditApplicationApplied {
    pub posting: PostingRef,
    pub debits: Vec<CreditDebitView>,
    pub applications: Vec<AllocationSplit>,
}

// The journal-line / balance / account list endpoints take a canonical
// `toolkit_odata::ODataQuery` (parsed `$filter` / `$orderby` / `$select` +
// `limit` / `cursor`) instead of bespoke filter structs, and return the
// canonical `toolkit_odata::Page<T>` envelope (`items` + `page_info` cursor
// metadata). The wire `$filter` field names are declared by the gear's
// `FilterField` enums (the gear binds them to columns); the SDK stays
// infra-free and only carries the parsed query + the page result. This is the
// platform list pattern (RBAC/AM/RG). The legacy `LineFilter` / `BalanceFilter`
// structs and the bespoke `Page { items, next_cursor }` are gone.
pub use toolkit_odata::{ODataQuery, Page};
