//! Event payloads published by the ledger.
//!
//! These are infra types: plain serde structs (the ledger event payloads).
//! They are **not** domain models (no
//! TODO(broker): these implemented `event_broker_sdk::TypedEvent` (TYPE_ID /
//! TOPIC / subject / partition-key); the impls are parked until the event
//! broker lands in gears-rust. Restore them alongside the publisher producers.
//!
//! //! `#[domain_model]`) and **not** REST DTOs (no `#[api_dto]`). Every field is
//! an internal identifier, enum code, or amount — there is **no PII**
//! (no names, emails, or free-text business content).

use crate::domain::instant::rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

/// One leg of a posted ledger entry, summarised for downstream consumers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerLineSummary {
    /// Account class the leg posts against (enum code, e.g. `"AR"`).
    pub account_class: String,
    /// Debit/credit side (enum code, `"DR"` or `"CR"`).
    pub side: String,
    /// Line amount in minor units; `side` carries the debit/credit direction.
    /// Wire-encoded as a JSON number (i64): consumers must use a 64-bit integer
    /// reader — an f64/JS parser loses precision above 2^53 (reachable only for
    /// very large amounts at high `currency_scale`).
    pub amount_minor: i64,
    /// ISO-4217 currency code.
    pub currency: String,
    /// Minor-unit scale for `amount_minor` (e.g. `2` ⇒ `1000` = 10.00). Without
    /// it `amount_minor` is ambiguous for non-default-scale currencies.
    pub currency_scale: u8,
}

/// Emitted when a balanced entry is freshly posted (never on replay).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerEntryPosted {
    /// Posted entry id.
    pub entry_id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning legal entity.
    pub legal_entity_id: Uuid,
    /// Accounting period the entry lands in.
    pub period_id: String,
    /// Source document type code.
    pub source_doc_type: String,
    /// Caller-supplied source business id (the idempotency key).
    pub source_business_id: String,
    /// Post timestamp (UTC).
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
    /// Monotonic creation sequence within the period.
    pub created_seq: i64,
    /// The entry's legs.
    pub lines: Vec<LedgerLineSummary>,
}

/// Emitted when a balanced ledger entry is reversed via the explicit reversal
/// path (`POST /journal-entries/{id}/reversals`), never on replay (architecture
/// §6 `billing.ledger.entry.reversed`). Internal ids + the audit `reason` only;
/// no PII, no amounts. Published in-txn via the transactional outbox alongside
/// the reversal's `entry.posted` event (the `ReversalEventSidecar`), so it
/// commits atomically with the reversing entry. A `MAPPING_CORRECTION` (which
/// also posts a reversal leg) does NOT emit this — it is a correction, not a
/// §6 reversal.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerEntryReversed {
    /// The reversing (compensating) entry's id.
    pub entry_id: Uuid,
    /// The original entry this reverses.
    pub reverses_entry_id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Operator-supplied audit reason for the reversal.
    pub reason: String,
}

/// Emitted on every successful dispute-phase post (architecture §6
/// `billing.ledger.dispute.recorded`) — opened / won / lost, never on replay.
/// Ids + enum codes only; no PII (no amounts, no invoice / payment business
/// content beyond the ids). Published in-txn via the transactional outbox
/// alongside the `entry.posted` event, so it commits atomically with the
/// dispute entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerDisputeRecorded {
    /// External dispute id (the first `source_business_id` token).
    pub dispute_id: String,
    /// The disputed payment id.
    pub payment_id: String,
    /// Owning (seller) tenant.
    pub tenant_id: Uuid,
    /// Re-entrancy cycle number (`>= 1`).
    pub cycle: i32,
    /// The phase recorded (`OPENED` | `WON` | `LOST` | `PARTIAL`).
    pub phase: String,
    /// The dispute variant (`CASH_HOLD` | `AR_RECLASS`).
    pub variant: String,
}

/// Emitted on every successful settlement-return post (architecture §4.2
/// `billing.ledger.settlement.returned`) — never on replay. Ids + enum codes +
/// the returned amount only; no PII (no names / free-text business content).
/// Published in-txn via the transactional outbox alongside the `entry.posted`
/// event, so it commits atomically with the settlement-return entry (or rolls
/// back with it). Mirrors [`LedgerDisputeRecorded`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerSettlementReturned {
    /// The original settled payment that was clawed back.
    pub payment_id: String,
    /// External return identity (the `SETTLEMENT_RETURN` `source_business_id`).
    pub psp_return_id: String,
    /// Owning (seller) tenant.
    pub tenant_id: Uuid,
    /// The returned gross amount in minor units. Wire-encoded as a JSON number
    /// (i64): consumers must use a 64-bit integer reader.
    pub amount_minor: i64,
    /// ISO-4217 currency of the return.
    pub currency: String,
}

/// Emitted on every recognition **release** post (design §3.7 / Group I1
/// `billing.ledger.revenue.recognized`) — when a recognition run releases one
/// segment (`DR CONTRACT_LIABILITY / CR REVENUE`), never on replay. Ids +
/// amount + stream + period only; no PII (no names / free-text business
/// content). Published in-txn via the transactional outbox alongside the
/// `entry.posted` event, so it commits atomically with the recognition entry
/// (or rolls back with it). Mirrors [`LedgerDisputeRecorded`] /
/// [`LedgerSettlementReturned`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerRevenueRecognized {
    /// The seller tenant whose ledger recognized the revenue — envelope routing +
    /// consumer-side tenant scoping (an id, not PII; matches the other ledger
    /// events, which all carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The owning recognition schedule's id.
    pub schedule_id: String,
    /// The released segment's number (immutable, 1:1 with `period_id`).
    pub segment_no: i32,
    /// The accounting period the recognized revenue lands in (`YYYYMM`). May
    /// differ from the segment's target period on an E-2 missed-close
    /// reassignment.
    pub period_id: String,
    /// The recognized amount in minor units (`= the entry's DR/CR amount`).
    /// Wire-encoded as a JSON number (i64): consumers must use a 64-bit integer
    /// reader.
    pub amount_minor: i64,
    /// The revenue stream both legs draw (per-stream disaggregation, §4.5).
    pub revenue_stream: String,
    /// ISO-4217 currency of the recognition entry.
    pub currency: String,
}

/// Emitted on every recognition **reversal / clawback** post (design §3.7 /
/// Group I1 `billing.ledger.revenue.recognition_reversed`) — when a reversal
/// compensates a prior release (`DR REVENUE / CR CONTRACT_LIABILITY`), never on
/// replay. Ids + amount + stream + period only; no PII. The mirror of
/// [`LedgerRevenueRecognized`]: same identity / amount, opposite legs.
/// Published in-txn via the transactional outbox alongside the `entry.posted`
/// event, so it commits atomically with the reversing entry (or rolls back
/// with it).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerRevenueRecognitionReversed {
    /// The seller tenant whose ledger the reversal lands in — envelope routing +
    /// consumer-side tenant scoping (an id, not PII; matches the other ledger
    /// events, which all carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The owning recognition schedule's id.
    pub schedule_id: String,
    /// The reversed segment's number (the segment stays `DONE`; design §4.3).
    pub segment_no: i32,
    /// The accounting period the reversal lands in (`YYYYMM`).
    pub period_id: String,
    /// Signed delta to cumulative recognized revenue, in minor units — NEGATIVE on a
    /// reversal, the mirror of [`LedgerRevenueRecognized::amount_minor`] (positive).
    /// A consumer nets recognition against reversals by summing `amount_minor` across
    /// both event types, with no special-casing of the type-id;
    /// the magnitude equals the reversing entry's DR/CR amount. Wire-encoded as a
    /// JSON number (i64): consumers must use a 64-bit integer reader.
    pub amount_minor: i64,
    /// The revenue stream both legs draw (per-stream disaggregation, §4.5).
    pub revenue_stream: String,
    /// ISO-4217 currency of the reversal entry.
    pub currency: String,
}

/// Emitted when a recognition schedule is changed or cancelled (design §3.7 /
/// Group I1 `billing.ledger.schedule.changed`) — e.g. a schedule superseded by
/// a new version (a fresh `schedule_id`) or cancelled outright (Phase 3 / Group
/// H). Ids + enum codes only; no PII. Group H owns the emit site (it CALLS
/// [`super::publisher::LedgerEventPublisher::publish_schedule_changed`]); the
/// payload + producer + schema + lockstep are defined here. Unlike the
/// recognition release / reversal events this carries no amount — a
/// schedule-change is a lifecycle transition, not a posting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerScheduleChanged {
    /// The seller tenant whose schedule changed — envelope routing + consumer-side
    /// tenant scoping (an id, not PII; matches the other ledger events, which all
    /// carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The schedule whose lifecycle changed (the one being superseded /
    /// cancelled).
    pub schedule_id: String,
    /// The id of the successor schedule version when the change supersedes this
    /// schedule with a fresh version (`REPLACED`); `None` on a cancellation (no
    /// successor) or any change that mints no new schedule.
    pub new_schedule_id: Option<String>,
    /// The modification-accounting treatment that let the change proceed —
    /// `prospective` or `separate_contract` (the wire values `gate_treatment`
    /// accepts; a `catch_up`/unknown treatment is refused before any emit). A stable
    /// enum code, not free text. (Do NOT name a recognition-timing code like
    /// `POINT_IN_TIME`/`OVER_TIME` here — the producer never emits those;
    /// `change_service` ships `cmd.treatment`.)
    pub treatment: String,
    /// The resulting schedule status (`ACTIVE` | `COMPLETED` | `REPLACED` |
    /// `CANCELLED`) — the durable lifecycle enum the change transitioned to.
    pub status: String,
}

/// Emitted on every successful credit-note post (Slice 3 §4.2 / Group F
/// `billing.ledger.credit_note.posted`) — never on replay. Ids + amount + the
/// recognized/deferred split parts + period only; no PII (no names / free-text
/// business content — `reason_code` is a closed enum code, NOT carried here to
/// keep the payload strictly ids/amounts). Published in-txn via the
/// transactional outbox alongside the `entry.posted` event, so it commits
/// atomically with the credit-note entry (or rolls back with it). Mirrors
/// [`LedgerSettlementReturned`] / [`LedgerRevenueRecognized`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreditNotePosted {
    /// The seller tenant whose ledger posted the credit note — envelope routing +
    /// consumer-side tenant scoping (an id, not PII; matches the other ledger
    /// events, which all carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The business id of the posted credit note (the `(tenant, CREDIT_NOTE,
    /// credit_note_id)` idempotency key + the `credit_note` row PK).
    pub credit_note_id: String,
    /// The originating posted invoice the note compensates.
    pub origin_invoice_id: String,
    /// The posted entry id of the credit-note journal entry (the `entry.posted`
    /// event's subject — lets a consumer correlate the two).
    pub entry_id: Uuid,
    /// ISO-4217 currency of the note.
    pub currency: String,
    /// The note amount **incl-tax**, in minor units. Wire-encoded as a JSON
    /// number (i64): consumers must use a 64-bit integer reader.
    pub amount_minor: i64,
    /// The ex-tax recognized part (debited `CONTRA_REVENUE` / `GOODWILL`).
    pub recognized_part_minor: i64,
    /// The ex-tax deferred part (debited `CONTRACT_LIABILITY`, reducing the
    /// schedule's deferred remainder).
    pub deferred_part_minor: i64,
    /// Post timestamp (UTC).
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
}

/// Emitted on every successful debit-note post (Slice 3 §4.3 / Group F
/// `billing.ledger.debit_note.posted`) — an additional charge against a posted
/// invoice; never on replay. Ids + amount + the recognized/deferred split parts
/// plus period only; no PII. Published in-txn via the transactional outbox
/// alongside the `entry.posted` event, so it commits atomically with the
/// debit-note entry (or rolls back with it). The mirror of [`CreditNotePosted`]
/// (an additional charge rather than a compensating reduction).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DebitNotePosted {
    /// The seller tenant whose ledger posted the debit note — envelope routing +
    /// consumer-side tenant scoping (an id, not PII; matches the other ledger
    /// events, which all carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The business id of the posted debit note (the `(tenant, DEBIT_NOTE,
    /// debit_note_id)` idempotency key + the `debit_note` row PK).
    pub debit_note_id: String,
    /// The originating posted invoice the note charges against.
    pub origin_invoice_id: String,
    /// The posted entry id of the debit-note journal entry (the `entry.posted`
    /// event's subject — lets a consumer correlate the two).
    pub entry_id: Uuid,
    /// ISO-4217 currency of the note.
    pub currency: String,
    /// The note amount **incl-tax**, in minor units. Wire-encoded as a JSON
    /// number (i64): consumers must use a 64-bit integer reader.
    pub amount_minor: i64,
    /// The ex-tax recognized part (credited `REVENUE` at post).
    pub recognized_part_minor: i64,
    /// The ex-tax deferred part (credited `CONTRACT_LIABILITY`, anchoring a fresh
    /// recognition schedule).
    pub deferred_part_minor: i64,
    /// Post timestamp (UTC).
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
}

/// Emitted on every successful refund-phase post (Slice 3 §4.4 / §6 / Group G
/// `billing.ledger.refund.recorded`) — never on replay. Ids + enum codes +
/// amount + clearing-state only; no PII. Published in-txn via the transactional
/// outbox alongside the `entry.posted` event on a refund's stage post (and on
/// the `unknown_final` disposition), so it commits atomically with the refund
/// entry (or rolls back with it). The `phase` discriminates the lifecycle stage
/// (`initiated` / `confirmed` / `rejected` / `voided` / `unknown_final`, K-1 incl.
/// `unknown_final`); the `pattern` the economic shape (`A_UNALLOCATED` /
/// `B_RESTORE_AR`). Mirrors [`CreditNotePosted`] / [`LedgerSettlementReturned`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RefundRecorded {
    /// The seller tenant whose ledger posted the refund — envelope routing +
    /// consumer-side tenant scoping (an id, not PII; matches the other ledger
    /// events, which all carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The business id of the refund (the `refund` row's surrogate PK
    /// `(tenant, refund_id)` + the `GET /refunds/{refundId}` handle).
    pub refund_id: String,
    /// The PSP's refund id (the `(tenant, psp_refund_id, phase)` idempotency
    /// grain + the PSP-correlation handle a reconciler keys on).
    pub psp_refund_id: String,
    /// The posted entry id of the refund-stage journal entry (the `entry.posted`
    /// event's subject — lets a consumer correlate the two).
    pub entry_id: Uuid,
    /// The lifecycle phase recorded (`initiated` / `confirmed` / `rejected` /
    /// `voided` / `unknown_final`).
    pub phase: String,
    /// The economic pattern (`A_UNALLOCATED` / `B_RESTORE_AR`).
    pub pattern: String,
    /// The origin settled payment the refund unwinds.
    pub payment_id: String,
    /// The refund amount in minor units. Wire-encoded as a JSON number (i64):
    /// consumers must use a 64-bit integer reader.
    pub amount_minor: i64,
    /// ISO-4217 currency of the refund.
    pub currency: String,
    /// The resulting clearing state stamped on the `refund` row (`PENDING` /
    /// `SETTLED` / `REVERSED`).
    pub clearing_state: String,
}

/// Emitted on every successful governed manual-adjustment post (Slice 3 §4.6 /
/// Phase 3 / Group 4 `billing.ledger.manual_adjustment.posted`) — never on
/// replay. Ids + enum codes + amount only; no PII. Published in-txn via the
/// transactional outbox alongside the `entry.posted` event on a governed manual
/// post, so it commits atomically with the manual-adjustment entry (or rolls back
/// with it). A manual adjustment is the ledger's governed escape hatch (rounding
/// residue / suspense clean-up); the `action` carries which governed capability
/// posted and the `reason_code` its mandatory justification. The `actor_ref` is
/// the preparer's internal subject id (a uuid string, PII-free), NOT a name.
/// Mirrors [`CreditNotePosted`] / [`RefundRecorded`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManualAdjustmentPosted {
    /// The seller tenant whose ledger posted the adjustment — envelope routing +
    /// consumer-side tenant scoping (an id, not PII; matches the other ledger
    /// events, which all carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The business id of the posted adjustment (the `(tenant, MANUAL_ADJUSTMENT,
    /// adjustment_id)` idempotency key + the `GET` handle).
    pub adjustment_id: String,
    /// The posted entry id of the manual-adjustment journal entry (the
    /// `entry.posted` event's subject — lets a consumer correlate the two).
    pub entry_id: Uuid,
    /// The governed action that posted (`ManualAdjustmentAction::as_str()`, e.g.
    /// `ROUNDING_CORRECTION` / `SUSPENSE_CLEAR`) — a stable enum code, not free
    /// text.
    pub action: String,
    /// The mandatory business reason code (a closed reason literal, not free
    /// text — AC #14).
    pub reason_code: String,
    /// The preparer's internal subject id (a uuid string, PII-free) — the actor
    /// who posted the adjustment. NOT a name / email.
    pub actor_ref: String,
    /// The gross adjustment amount in minor units (`Σ DR == Σ CR`; `govern`
    /// guarantees the legs net to zero). Wire-encoded as a JSON number (i64):
    /// consumers must use a 64-bit integer reader.
    pub amount_minor: i64,
    /// ISO-4217 currency of the adjustment (every leg shares it).
    pub currency: String,
}

/// Emitted on every successful unrealized-revaluation post (Slice 5 Phase 3 §3.6 /
/// §4.5 / design §6 `billing.ledger.fx.revaluation_completed`) — when a Mode-B
/// period-end run posts one functional-only `FX_UNREALIZED` entry for a moved
/// `(period, scope, payer)` grain set, never on replay. Ids + scope code + signed
/// functional amount + grain count only; no PII (the whole entry is
/// functional-only, `amount_minor = 0`, so there is no transaction amount to carry
/// — only the remeasurement movement). Published in-txn via the transactional
/// outbox alongside the `entry.posted` event (the `RevaluationCompletedSidecar`),
/// so it commits atomically with the revaluation entry (or rolls back with it).
/// Mirrors [`LedgerRevenueRecognized`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerFxRevaluationCompleted {
    /// The seller tenant whose ledger ran the revaluation — envelope routing +
    /// consumer-side tenant scoping (an id, not PII; matches the other ledger
    /// events, which all carry `tenant_id`).
    pub tenant_id: Uuid,
    /// The posted `FX_UNREALIZED` entry id (the `entry.posted` event's subject —
    /// lets a consumer correlate the two).
    pub entry_id: Uuid,
    /// The accounting period revalued (`YYYYMM`) — the period this entry posts INTO.
    pub period_id: String,
    /// The monetary scope revalued (`AR` / `UNALLOCATED` / `REUSABLE_CREDIT`) — a
    /// stable enum code (`RevaluationScope::as_token`), not free text.
    pub scope: String,
    /// The payer tenant the entry is scoped to (one entry spans one payer — the
    /// `MixedPayer` invariant; an id, not PII).
    pub payer_id: Uuid,
    /// ISO-4217 functional (reporting) currency the remeasurement is valued in.
    pub functional_currency: String,
    /// The net `FX_UNREALIZED` movement in functional minor units, SIGNED: positive
    /// = an unrealized GAIN (the net contra posted CREDIT), negative = an unrealized
    /// LOSS (posted DEBIT). The reversal next period carries its NEGATION. Wire-
    /// encoded as a JSON number (i64): consumers must use a 64-bit integer reader.
    pub fx_unrealized_minor: i64,
    /// The number of grains moved (remeasured with a non-zero delta) in this entry.
    pub grains_moved: i32,
    /// Post timestamp (UTC).
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
}

/// Emitted on every successful unrealized-revaluation REVERSAL post (Slice 5
/// Phase 3 §4.5 / decision 7 / design §6 `billing.ledger.fx.revaluation_reversed`)
/// — when the run negates a prior `FX_REVALUATION` entry as a fresh
/// `FX_REVAL_REVERSAL` JE in the next OPEN period, never on replay. Ids + scope
/// code + signed functional amount only; no PII. Published in-txn via the
/// transactional outbox alongside the `entry.posted` event (the
/// `RevaluationReversedSidecar`), so it commits atomically with the reversal entry
/// (or rolls back with it). The mirror of [`LedgerFxRevaluationCompleted`]: same
/// scope/payer, opposite legs, in a later period.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerFxRevaluationReversed {
    /// The seller tenant whose ledger posted the reversal — envelope routing +
    /// consumer-side tenant scoping (an id, not PII).
    pub tenant_id: Uuid,
    /// The posted `FX_REVAL_REVERSAL` entry id (the `entry.posted` event's subject).
    pub entry_id: Uuid,
    /// The original `FX_REVALUATION` entry this reversal negates (lets a consumer
    /// pair the reversal with the forward run it unwinds).
    pub reverses_entry_id: Uuid,
    /// The original revaluation period being reversed (`YYYYMM`).
    pub reval_period_id: String,
    /// The next OPEN period the reversal posts INTO (`YYYYMM`, strictly later than
    /// `reval_period_id`).
    pub reversal_period_id: String,
    /// The monetary scope reversed (`AR` / `UNALLOCATED` / `REUSABLE_CREDIT`) — a
    /// stable enum code (`RevaluationScope::as_token`), not free text.
    pub scope: String,
    /// The payer tenant the entry is scoped to (one entry spans one payer).
    pub payer_id: Uuid,
    /// ISO-4217 functional (reporting) currency.
    pub functional_currency: String,
    /// The net `FX_UNREALIZED` movement UNWOUND, in functional minor units, SIGNED —
    /// the NEGATION of the original revaluation's `fx_unrealized_minor` (the mirror
    /// of [`LedgerFxRevaluationCompleted::fx_unrealized_minor`]). A consumer nets a
    /// revaluation against its reversal by summing `fx_unrealized_minor` across both
    /// to zero. Wire-encoded as a JSON number (i64).
    pub fx_unrealized_minor: i64,
    /// Post timestamp (UTC).
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
}

/// Emitted on every successful fiscal-period close (Slice 7 Group C / design §6
/// `billing.ledger.period.closed`) — when the two-phase gate passes and the close
/// flips `fiscal_period OPEN→CLOSED` + `period_close → CLOSED` in one commit, never
/// on an idempotent re-close (an already-`CLOSED` period emits nothing). Published
/// **in-txn** via the transactional outbox in the SAME `SERIALIZABLE` transaction
/// as the flip, so the event commits atomically with the close (or rolls back with
/// it). Ids + period code + actor + timestamp only; no PII (a close posts no
/// financial lines, so there is no amount to carry). Mirrors
/// [`LedgerFxRevaluationCompleted`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerPeriodClosed {
    /// The seller tenant whose ledger owns the closed period — envelope routing +
    /// consumer-side tenant scoping (an id, not PII).
    pub tenant_id: Uuid,
    /// The selling legal-entity the period belongs to (the close unit; design §8).
    /// In v1 one legal entity per tenant, so this equals `tenant_id`.
    pub legal_entity_id: Uuid,
    /// The accounting period closed (`YYYYMM`).
    pub period_id: String,
    /// The Finance actor who initiated the close (an id, not PII).
    pub closed_by: Uuid,
    /// Close commit timestamp (UTC).
    #[serde(with = "rfc3339")]
    pub closed_at_utc: OffsetDateTime,
}

/// Emitted on every completed reconciliation check (Slice 7 Phase 3 / design §6
/// `billing.ledger.reconciliation.completed`) — when the `ReconciliationFramework`
/// finalizes a `reconciliation_run` (AR↔derived, Payments↔PSP, or invoice-completeness),
/// carrying the check type + the variance result. Emitted on a DONE run, never on a
/// still-RUNNING / FAILED one. Published **in-txn** via the transactional outbox alongside
/// the run's `finalize` write, so the event commits atomically with the run row (or rolls
/// back with it). Ids + check-type code + signed variance + tolerance flag only; no PII (a
/// reconciliation posts no financial lines). Mirrors [`LedgerPeriodClosed`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerReconciliationCompleted {
    /// The seller tenant whose ledger the run reconciled — envelope routing +
    /// consumer-side tenant scoping (an id, not PII).
    pub tenant_id: Uuid,
    /// The reconciliation-run id (the `reconciliation_run` PK + the event subject).
    pub run_id: Uuid,
    /// The accounting period reconciled (`YYYYMM`).
    pub period_id: String,
    /// The check type (`AR_DERIVED` | `PAYMENTS_PSP` | `INVOICE_COMPLETENESS`) — a stable
    /// enum code, not free text.
    pub check_type: String,
    /// The variance the run found, in minor units, SIGNED (`computed − cached`; `0` on a
    /// clean run). Wire-encoded as a JSON number (i64): consumers must use a 64-bit integer
    /// reader.
    pub variance_minor: i64,
    /// Whether the variance is within tolerance (X4) — `false` opens a close-blocking
    /// exception (`RECON_MISMATCH` / `PSP_VARIANCE` / `MISSED_POSTING`).
    pub within_tolerance: bool,
    /// Run completion timestamp (UTC).
    #[serde(with = "rfc3339")]
    pub at_utc: OffsetDateTime,
}

/// Category of an invariant-violation alarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AlarmCategory {
    /// Two posts share an idempotency key but disagree on payload (architecture I-9).
    IdempotencyPayloadConflict,
    /// A balance would go negative where the schema forbids it.
    NegativeBalanceViolation,
    /// A recomputed `account_balance` grain disagrees with the cached value
    /// (tie-out variance; architecture §4.10, zero tolerance).
    TieOutVariance,
    /// A posted entry fails the entry-balance backstop (net debits ≠ net
    /// credits per currency, no lines, or multiple payers).
    EntryImbalance,
    /// The daily chain Verifier re-walked a tenant's tamper-evidence hash chain
    /// and found a `row_hash` recompute mismatch or a broken back-link
    /// (architecture §4.2; the tenant is frozen tenant-wide on detection).
    TamperVerifyFailed,

    // ── §4.7 catalog rows not represented by the 5 above ─────────────────────
    // Each new variant is doc-commented with its owning slice + the design's
    // kebab token (the §4.7 row id) so the catalog stays traceable. The wire
    // token (`as_str`) is the SCREAMING_SNAKE form of the same kebab id.
    /// §4.7 `chargeback-cash-negative` — owned by Slice 4.
    ChargebackCashNegative,
    /// §4.7 `recognition-double-credit` — owned by Slice 3.
    RecognitionDoubleCredit,
    /// §4.7 `over-recognition` — owned by Slice 3.
    OverRecognition,
    /// §4.7 `fx-snapshot-missing` — owned by Slice 3.
    FxSnapshotMissing,
    /// §4.7 `fx-snapshot-stale-allowed` — owned by Slice 3.
    FxSnapshotStaleAllowed,
    /// §4.7 `fx-snapshot-stale-blocked` — owned by Slice 3.
    FxSnapshotStaleBlocked,
    /// §4.7 `negative-tax-subbalance` — owned by Slice 5.
    NegativeTaxSubbalance,
    /// §4.7 `credit-note-split-blocked` — owned by Slice 4.
    CreditNoteSplitBlocked,
    /// §4.7 `refund-quarantined` — owned by Slice 4.
    RefundQuarantined,
    /// §4.7 `recognition-period-queued` — owned by Slice 3.
    RecognitionPeriodQueued,
    /// §4.7 `reconciliation-variance` — owned by Slice 7.
    ReconciliationVariance,
    /// §4.7 `export-failed-aged` — owned by Slice 7.
    ExportFailedAged,
    /// §4.7 `aged-allocation-queue` — owned by Slice 4.
    AgedAllocationQueue,
    /// §4.7 `aged-unallocated` — owned by Slice 4.
    AgedUnallocated,
    /// §4.7 `refund-clearing-aged` — owned by Slice 4.
    RefundClearingAged,
    /// §4.7 `dispute-phase-queued` — owned by Slice 4.
    DisputePhaseQueued,
    /// §4.7 `stage1-refund-orphan` — owned by Slice 4.
    Stage1RefundOrphan,
    /// A refund-of-refund CLAW-BACK never reconciled (Slice 3 §4.4 / Rev3 / Group
    /// E). A claw-back whose money-out decrement would underflow (the PSP claw-back
    /// arrived before / without the matching outbound refund stage-1, or claws back
    /// more than was refunded) is DEFERRED on the `REFUND_CLAWBACK` queue, never
    /// hard-failed; when it has sat `QUEUED` past the aging horizon without its
    /// matching outbound landing it is CANCELLED and ESCALATED. `Critical`: a
    /// books-affecting money-out that could not be netted against any outbound refund
    /// — Finance must reconcile it (the `refunded_minor >= 0` CHECK never fired
    /// because the underflow was deferred, not applied). Raised EXPLICITLY by the
    /// `RefundHandler` on the never-reconcile escalation, out-of-band on the
    /// CANCELLED queue row (an exception stub — full `exception_queue` is Slice 7 /
    /// VHP-1859), mirroring the `CreditNoteSplitBlocked` explicit raise.
    ClawbackUnderflow,
    /// A `REFUND_CLEARING` balance aged past the PAGE threshold (14 days, design
    /// §4.4 / §13) — the close-blocking `STUCK_REFUND_CLEARING` exception. The
    /// 7-day `RefundClearingAged` Warn escalates to this `Critical` page at 14
    /// days: the clearing has been open long enough that it blocks the period
    /// close (the Slice-7 close gate, §4.5) and joins the Payments↔PSP
    /// reconciliation. Raised by the `AgedAlarmJob` alongside an
    /// `// exception stub (full exception_queue is Slice 7)` marker (the
    /// additive `exception_queue.type = STUCK_REFUND_CLEARING` lands in Slice 7 /
    /// VHP-1859). `Critical`. Slice 6 catalog seam (the additive type name,
    /// verbatim).
    StuckRefundClearing,
    /// §4.7 `billrun-partial-failure` — owned by Slice 2.
    BillrunPartialFailure,
    /// §4.7 `partition-detach-blocked` — owned by Slice 7.
    PartitionDetachBlocked,
    /// §4.7 `relay-lag` — owned by Slice 6.
    RelayLag,
    /// §4.7 `clock-skew` — owned by Slice 1.
    ClockSkew,
    /// §4.7 `attempted-write-off` — owned by Slice 4.
    AttemptedWriteOff,
    /// §4.7 `payer-attribution-drift` — owned by Slice 1.
    PayerAttributionDrift,
    /// §4.7 `fx-revaluation-incomplete` — owned by Slice 3.
    FxRevaluationIncomplete,
    /// §4.7 `dormant-open-credit` — owned by Slice 4.
    DormantOpenCredit,
    /// §4.7 `chain-lag` — owned by Slice 6 (post-MVP; defined-but-dormant).
    ChainLag,
    /// §4.7 `subtree-too-large` — owned by Slice 1 (post-MVP; defined-but-dormant).
    SubtreeTooLarge,
    /// §4.7 `subtree-resolution-degraded` — owned by Slice 1
    /// (post-MVP; defined-but-dormant).
    SubtreeResolutionDegraded,
    /// An issued invoice in the upstream issued-invoice manifest with no committed
    /// `INVOICE_POST` (Rev3 / N-recon-1) — owned by Slice 7. Raised by the Phase 3
    /// invoice-completeness check when a `MISSED_POSTING` exception ages past the
    /// configurable threshold (Warn → Page); the durable close-blocking row is the
    /// `exception_queue.type = MISSED_POSTING` (design §4.3 / §4.6).
    MissedPosting,
}

impl AlarmCategory {
    /// Every category, in declaration order — the canonical enumeration the
    /// lockstep test folds over to assert the Rust enum and the vendored schema
    /// `category` enum agree exactly. A new variant must be added here too (the
    /// schema-lockstep test enumerates this slice), keeping enum ↔ schema in sync.
    pub const ALL: &'static [Self] = &[
        Self::IdempotencyPayloadConflict,
        Self::NegativeBalanceViolation,
        Self::TieOutVariance,
        Self::EntryImbalance,
        // Slice 6 tamper-verification: the chain verifier emits this on a hash-chain
        // mismatch (a Critical integrity alarm). It must be in `ALL` + the vendored
        // schema or the eager validator drops the durable event when events are on.
        Self::TamperVerifyFailed,
        Self::ChargebackCashNegative,
        Self::AgedAllocationQueue,
        Self::DisputePhaseQueued,
        Self::AgedUnallocated,
        Self::RecognitionDoubleCredit,
        Self::OverRecognition,
        Self::RecognitionPeriodQueued,
        Self::CreditNoteSplitBlocked,
        Self::ClawbackUnderflow,
        Self::RefundQuarantined,
        Self::RefundClearingAged,
        Self::Stage1RefundOrphan,
        Self::StuckRefundClearing,
        Self::AttemptedWriteOff,
        Self::NegativeTaxSubbalance,
        // Slice 5 (FX) — the rate missing/staleness family. `FxSnapshotMissing`
        // is emitted by the `RateSyncJob` on a configured-provider sync failure;
        // the two staleness rows are emitted at the lock-time `RateSource` path
        // (live S1/S2 hook deferred). All three carry their severity/routing/
        // owning-slice in `alarm_catalog`, so they join the canonical catalog +
        // vendored schema here.
        Self::FxSnapshotMissing,
        Self::FxSnapshotStaleAllowed,
        Self::FxSnapshotStaleBlocked,
        // Slice 7 (reconciliation) — the recon framework is the first emitter of
        // both: an out-of-tolerance reconciliation run raises `ReconciliationVariance`
        // (alongside the close-blocking `RECON_MISMATCH`/`PSP_VARIANCE` exception),
        // and an aged invoice-completeness gap raises `MissedPosting`. Both join the
        // canonical catalog + the vendored schema here so they validate when events
        // are enabled.
        Self::ReconciliationVariance,
        Self::MissedPosting,
    ];

    /// Stable `SCREAMING_SNAKE_CASE` code for this category (the durable event
    /// `category` field + the schema enum value). Matches the existing categories'
    /// `as_str` convention — the kebab-case `chargeback-cash-negative` from the
    /// architecture is the human/wire `alarmCategory` label, while the persisted
    /// event enum stays `SCREAMING_SNAKE_CASE` like its siblings.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdempotencyPayloadConflict => "IDEMPOTENCY_PAYLOAD_CONFLICT",
            Self::NegativeBalanceViolation => "NEGATIVE_BALANCE_VIOLATION",
            Self::TieOutVariance => "TIE_OUT_VARIANCE",
            Self::EntryImbalance => "ENTRY_IMBALANCE",
            Self::TamperVerifyFailed => "TAMPER_VERIFY_FAILED",
            Self::ChargebackCashNegative => "CHARGEBACK_CASH_NEGATIVE",
            Self::RecognitionDoubleCredit => "RECOGNITION_DOUBLE_CREDIT",
            Self::OverRecognition => "OVER_RECOGNITION",
            Self::FxSnapshotMissing => "FX_SNAPSHOT_MISSING",
            Self::FxSnapshotStaleAllowed => "FX_SNAPSHOT_STALE_ALLOWED",
            Self::FxSnapshotStaleBlocked => "FX_SNAPSHOT_STALE_BLOCKED",
            Self::NegativeTaxSubbalance => "NEGATIVE_TAX_SUBBALANCE",
            Self::CreditNoteSplitBlocked => "CREDIT_NOTE_SPLIT_BLOCKED",
            Self::RefundQuarantined => "REFUND_QUARANTINED",
            Self::RecognitionPeriodQueued => "RECOGNITION_PERIOD_QUEUED",
            Self::ReconciliationVariance => "RECONCILIATION_VARIANCE",
            Self::ExportFailedAged => "EXPORT_FAILED_AGED",
            Self::AgedAllocationQueue => "AGED_ALLOCATION_QUEUE",
            Self::AgedUnallocated => "AGED_UNALLOCATED",
            Self::RefundClearingAged => "REFUND_CLEARING_AGED",
            Self::DisputePhaseQueued => "DISPUTE_PHASE_QUEUED",
            Self::Stage1RefundOrphan => "STAGE1_REFUND_ORPHAN",
            Self::ClawbackUnderflow => "CLAWBACK_UNDERFLOW",
            Self::StuckRefundClearing => "STUCK_REFUND_CLEARING",
            Self::BillrunPartialFailure => "BILLRUN_PARTIAL_FAILURE",
            Self::PartitionDetachBlocked => "PARTITION_DETACH_BLOCKED",
            Self::RelayLag => "RELAY_LAG",
            Self::ClockSkew => "CLOCK_SKEW",
            Self::AttemptedWriteOff => "ATTEMPTED_WRITE_OFF",
            Self::PayerAttributionDrift => "PAYER_ATTRIBUTION_DRIFT",
            Self::FxRevaluationIncomplete => "FX_REVALUATION_INCOMPLETE",
            Self::DormantOpenCredit => "DORMANT_OPEN_CREDIT",
            Self::ChainLag => "CHAIN_LAG",
            Self::SubtreeTooLarge => "SUBTREE_TOO_LARGE",
            Self::SubtreeResolutionDegraded => "SUBTREE_RESOLUTION_DEGRADED",
            Self::MissedPosting => "MISSED_POSTING",
        }
    }
}

/// Severity of an invariant-violation alarm.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AlarmSeverity {
    /// Recoverable / deterministically re-detectable on retry.
    Warn,
    /// Integrity-threatening; requires operator attention.
    Critical,
}

impl AlarmSeverity {
    /// Stable `SCREAMING_SNAKE_CASE` code for this severity.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warn => "WARN",
            Self::Critical => "CRITICAL",
        }
    }
}

/// One grain/entry that tripped an invariant — identifies WHICH balance
/// diverged and by how much, so an operator paged on an alarm needn't re-run
/// tie-out by hand. Ids only; no PII.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AffectedItem {
    /// Account id, entry id (uuid string), or a sub-grain key.
    pub id: String,
    /// Grain currency (empty where the defect has no single currency).
    pub currency: String,
    /// Recomputed-from-journal ("truth") minor units; `0` where not applicable.
    pub expected_minor: i64,
    /// Cached / observed minor units (the cached balance, or an entry's net).
    pub actual_minor: i64,
}

/// Emitted out-of-band when a posting attempt trips a ledger invariant.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerInvariantAlarm {
    /// Invariant category.
    pub category: AlarmCategory,
    /// Severity.
    pub severity: AlarmSeverity,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Diagnostic scope. Hot posting-path alarms use
    /// `"tenant:{uuid}/flow:{flow}/business:{id}"`; tie-out alarms carry just
    /// `"tenant:{uuid}"` (the divergent grains are named in `affected`).
    pub scope: String,
    /// Ledger error code that triggered the alarm.
    pub code: String,
    /// Internal diagnostic detail — no PII.
    pub detail: String,
    /// The specific grains/entries that diverged (capped). Empty for alarms
    /// raised on the hot posting path, where `detail` already names the defect.
    pub affected: Vec<AffectedItem>,
}

// ─── §6 dormant event payloads (Slice 6 Phase 4 Group 4C) ────────────────────
//
// These five payloads are the DURABLE SHAPES a future producer will emit for the
// audit / tamper / privacy lifecycle. They are defined now (so the wire contract
// is fixed and round-trip-tested) but DORMANT: the gear ships with
// `events_enabled = false` (`module.rs::build_event_publisher`) because the
// platform event-type GTS model is incomplete, so NO broker producer is wired
// for them and they carry no vendored JSON-Schema yet. The authoritative record
// of each of these facts ALREADY exists in the secured-audit / tamper-evidence
// chain; these structs are the future relay shape, not the source of truth.
// Every field is an id, token, count, or timestamp — there is NO PII.

/// Emitted when the daily chain Verifier re-walks a tenant's chain and it
/// verifies clean (dormant; see the §6 note above).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerTamperVerified {
    /// Tenant whose chain was verified.
    pub tenant_id: Uuid,
    /// Number of entries the walk visited (the observed chain length).
    pub chain_length: i64,
    /// When the verification completed (UTC).
    #[serde(with = "rfc3339")]
    pub verified_at_utc: OffsetDateTime,
}

/// Emitted when the daily chain Verifier detects a break (dormant; see the §6
/// note above). The tenant is frozen tenant-wide on detection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerTamperFailed {
    /// Tenant whose chain failed verification.
    pub tenant_id: Uuid,
    /// The entry at which the break was detected.
    pub break_entry_id: Uuid,
    /// Period of that entry.
    pub period_id: String,
    /// When the break was detected (UTC).
    #[serde(with = "rfc3339")]
    pub detected_at_utc: OffsetDateTime,
}

/// Emitted when a GDPR right-to-erasure is applied to a payer's PII map
/// (dormant; see the §6 note above). Carries a stable reference token, never the
/// erased PII itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerErasureApplied {
    /// Tenant that owns the PII map.
    pub tenant_id: Uuid,
    /// Opaque reference / token for the erased payer record (NOT the PII).
    pub payer_pii_ref: String,
    /// When the erasure was applied (UTC).
    #[serde(with = "rfc3339")]
    pub applied_at_utc: OffsetDateTime,
}

/// Emitted when a forensic re-identification of a payer is recorded (dormant;
/// see the §6 note above). Carries reference tokens, never the resolved PII.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerReidentificationRecorded {
    /// Tenant that owns the PII map.
    pub tenant_id: Uuid,
    /// Opaque reference / token for the re-identified payer record (NOT the PII).
    pub payer_pii_ref: String,
    /// Opaque reference / token for the investigator who performed the read.
    pub investigator_ref: String,
    /// When the re-identification was recorded (UTC).
    #[serde(with = "rfc3339")]
    pub recorded_at_utc: OffsetDateTime,
}

/// Emitted when an audit-pack CSV export completes (dormant; see the §6 note
/// above).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerAuditPackExported {
    /// Tenant under whose HOME scope the export ran.
    pub tenant_id: Uuid,
    /// Number of data rows in the pack (excludes the header).
    pub row_count: i64,
    /// For a cross-tenant (forensic) export, the TARGET tenant whose rows were
    /// read; `None` for a routine own-tenant export.
    pub target_scope_tenant_id: Option<Uuid>,
    /// When the export completed (UTC).
    #[serde(with = "rfc3339")]
    pub exported_at_utc: OffsetDateTime,
}

// Tests parked with the event broker: the `TypedEvent` impls + JSON-Schema
// lockstep checks return with `event-broker-sdk` (see
// `crate::infra::events::publisher`).
