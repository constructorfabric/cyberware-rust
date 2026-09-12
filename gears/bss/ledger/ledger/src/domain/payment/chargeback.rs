//! Chargeback (dispute) entry builder (architecture §4.5, design §2 / §3). A
//! dispute moves through `opened → {won, lost, partial}` phases; the LEDGER
//! chooses the **variant** at `opened` from the request's `funds_at_open` fact
//! (card rails withheld the cash ⇒ `CASH_HOLD`; invoice/ACH did not move it ⇒
//! `AR_RECLASS`) and the `won`/`lost` outcomes branch on the recorded variant.
//!
//! Scope (Groups B + C): this builder handles `opened`, `won`, and `lost`, in
//! both variants (`partial` is behind a flag — still rejected). The legs, pinned
//! from architecture §4.5 / design §3.
//!
//! **Model N (cash legs are net-of-fee).** A `CASH_HOLD` dispute's cash legs are
//! sized at `net = settled_minor − fee_minor`, NOT the gross `disputed`. `settle`
//! posts `DR CASH_CLEARING (net) · DR PSP_FEE_EXPENSE (fee) · CR UNALLOCATED
//! (gross)`, so `CASH_CLEARING` only ever holds **net** — sizing a cash leg at
//! gross would underflow it by exactly the fee. The PSP fee is finalised at
//! `settle` (in `PSP_FEE_EXPENSE`) and is never disputed; the seller eats it on a
//! loss. `disputed_amount_minor` is the **gross** claim (what the buyer paid / the
//! bank reverses) and sizes the AR-reclass legs + the dispute row, never the cash
//! legs. The orchestrator reads the settlement pre-build and threads `net` in
//! (the builder stays pure — it does no IO; it only sizes legs on the number it
//! is handed). Worked example (gross 100, fee 3, net 97):
//! ```text
//! settle: DR CASH_CLEARING 97 · DR PSP_FEE_EXPENSE 3 · CR UNALLOCATED 100
//! opened: DR DISPUTE_HOLD 97  · CR CASH_CLEARING 97
//! won:    DR CASH_CLEARING 97 · CR DISPUTE_HOLD 97
//! lost:   DR DISPUTE_LOSS 97  · CR DISPUTE_HOLD 97   (+ fee 3 already expensed = 100)
//! ```
//!
//! **`opened`** (Group B):
//! - **`CASH_HOLD`** — move the settled cash into a hold:
//!   `DR DISPUTE_HOLD (net) / CR CASH_CLEARING (net)`. Balanced; touches no AR.
//! - **`AR_RECLASS`** — reclass the disputed receivable `ACTIVE → DISPUTED` at
//!   the `(payer, invoice)` grain, AR-class-neutral: two AR lines for the SAME
//!   `(payer, invoice)` — `DR AR (ar_status = DISPUTED)` + `CR AR (ar_status =
//!   ACTIVE)`, each `disputed` — that net ZERO on `ar_invoice_balance.balance_minor`
//!   while the Group-A projector routes the signed `DISPUTED` delta into
//!   `disputed_minor` (+D). No PSP fee on invoice/ACH ⇒ gross = net = receivable.
//!
//! **`won`** (Group C — the seller's favour, no clawback):
//! - **`AR_RECLASS`** — reverse the opened reclass `DISPUTED → ACTIVE`:
//!   `DR AR (ar_status = ACTIVE)` + `CR AR (ar_status = DISPUTED)`, each
//!   `disputed`. Nets ZERO on `balance_minor`, `−D` on `disputed_minor`. No cash.
//! - **`CASH_HOLD`** — release the hold back to clearing:
//!   `DR CASH_CLEARING (net) / CR DISPUTE_HOLD (net)` (the reverse of the opened
//!   cash leg). The withheld net cash is the seller's again (the fee stays lost).
//!
//! **`lost`** (Group C — against the seller, the clawback):
//! - **`CASH_HOLD`** — the cash was already withheld at `opened` (it left
//!   `CASH_CLEARING` into `DISPUTE_HOLD` then), so the loss is recognised out of
//!   the hold: `DR DISPUTE_LOSS_EXPENSE (net) / CR DISPUTE_HOLD (net)` (release
//!   the hold, book the forfeiture). `clawed_back_minor += net`. `CASH_CLEARING`
//!   is NOT touched — the funds left clearing at open. The total loss is `net`
//!   (dispute-loss) + the fee already expensed at settle = gross = the buyer
//!   refund.
//! - **`AR_RECLASS`** — a write-off. The receivable was never collected (funds
//!   `not_moved`), so a lost dispute writes it off to loss:
//!   `DR DISPUTE_LOSS_EXPENSE (disputed) / CR AR (ar_status = DISPUTED)
//!   (disputed)`. The lone `CR AR DISPUTED` nets `−D` on BOTH `balance_minor` and
//!   `disputed_minor` (the projector routes the signed DISPUTED delta onto both),
//!   so no extra balance leg is needed. No cash leg, `clawed_back_minor`
//!   unchanged (nothing was ever collected to claw back).
//!
//! `partial` is NOT built here (behind a flag); the builder returns
//! [`DomainError::InvalidDisputeTransition`] for it.
//!
//! `source_doc_type = CHARGEBACK`; `source_business_id = "dispute_id:cycle:phase"`
//! (the `snake_case` composite idempotency key). Like the settlement / settlement-
//! return builders this stays pure (dylint DE0301): each line carries a nil
//! placeholder `account_id` + placeholder header fields the `crate::infra`
//! orchestrator overwrites before posting.

use bss_ledger_sdk::{AccountClass, MappingStatus, PostEntry, PostLine, Side, SourceDocType};

use toolkit_macros::domain_model;
use uuid::Uuid;

use chrono::NaiveDate;

use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::status::{AR_STATUS_ACTIVE, AR_STATUS_DISPUTED};
use time::OffsetDateTime;

/// A dispute phase (the `phase` of one chargeback event). `opened` ships in
/// Group B; `won`/`lost` arrive in Group C; `partial` is behind a flag
/// (design §2). The literal is the third token of `source_business_id`
/// (`dispute_id:cycle:phase`) and is persisted via the journal, not stored
/// on the dispute row except as `last_phase`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputePhase {
    /// The dispute was raised — cash moved to a hold (`CASH_HOLD`) or the
    /// receivable reclassed to `DISPUTED` (`AR_RECLASS`).
    Opened,
    /// The dispute resolved in the seller's favour (Group C).
    Won,
    /// The dispute resolved against the seller — a clawback (Group C).
    Lost,
    /// A split outcome (behind a flag; Group C).
    Partial,
}

impl DisputePhase {
    /// Stable uppercase wire literal (the `last_phase` column value + the third
    /// `source_business_id` token).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "OPENED",
            Self::Won => "WON",
            Self::Lost => "LOST",
            Self::Partial => "PARTIAL",
        }
    }

    /// Parse a stored / wire phase literal (case-insensitive), or `None` for an
    /// unknown value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "OPENED" => Some(Self::Opened),
            "WON" => Some(Self::Won),
            "LOST" => Some(Self::Lost),
            "PARTIAL" => Some(Self::Partial),
            _ => None,
        }
    }
}

/// The dispute variant the LEDGER records at `opened` (design §2): it pins how
/// the `opened` move posts AND how `won`/`lost` branch. Chosen from
/// `funds_at_open`, NOT tenant/plugin policy.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputeVariant {
    /// Card rails withheld the cash (`funds_at_open = withheld`): the settled
    /// cash is moved into a `DISPUTE_HOLD`.
    CashHold,
    /// Invoice / ACH did not move the cash (`funds_at_open = not_moved`): the
    /// receivable is reclassed `ACTIVE → DISPUTED` (AR-class-neutral).
    ArReclass,
}

impl DisputeVariant {
    /// Stable uppercase wire literal (the `variant` column value).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CashHold => "CASH_HOLD",
            Self::ArReclass => "AR_RECLASS",
        }
    }

    /// Parse a stored variant literal (case-insensitive), or `None` for an
    /// unknown value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "CASH_HOLD" => Some(Self::CashHold),
            "AR_RECLASS" => Some(Self::ArReclass),
            _ => None,
        }
    }
}

/// The funds-movement fact the LEDGER reads at `opened` to choose the variant
/// (architecture N-pay-8 / design §2). The PSP / payments gear populates it;
/// the ledger never re-derives it.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundsAtOpen {
    /// Card rails withheld the cash ⇒ [`DisputeVariant::CashHold`].
    Withheld,
    /// Invoice / ACH did not move the cash ⇒ [`DisputeVariant::ArReclass`].
    NotMoved,
}

impl FundsAtOpen {
    /// The variant this funds-fact selects at `opened`.
    #[must_use]
    pub const fn variant(self) -> DisputeVariant {
        match self {
            Self::Withheld => DisputeVariant::CashHold,
            Self::NotMoved => DisputeVariant::ArReclass,
        }
    }

    /// Stable lower-case wire literal.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Withheld => "withheld",
            Self::NotMoved => "not_moved",
        }
    }

    /// Parse a wire funds-fact literal (case-insensitive), or `None` for an
    /// unknown value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "withheld" => Some(Self::Withheld),
            "not_moved" => Some(Self::NotMoved),
            _ => None,
        }
    }
}

/// One chargeback phase event to post (architecture §4.5 input). The dispute is
/// per payment; an `AR_RECLASS` reclass additionally needs the `invoice_id`
/// being disputed (the AR grain is `(payer, invoice)`).
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargebackInput {
    /// The seller tenant whose ledger this posts into (`= entry.tenant_id`), and
    /// the `seller_tenant_id` stamped on each line.
    pub tenant_id: Uuid,
    /// The tenant that paid / owes (the single payer of the entry).
    pub payer_tenant_id: Uuid,
    /// The disputed payment — the `ledger_payment_settlement` row this dispute
    /// references.
    pub payment_id: String,
    /// External dispute identity — the first `source_business_id` token.
    pub dispute_id: String,
    /// Re-entrancy counter (`>= 1`); the second `source_business_id` token.
    pub cycle: i32,
    /// The phase being recorded (Group B handles only `Opened`).
    pub phase: DisputePhase,
    /// The variant — selected at `opened` from `funds_at_open`, then carried on
    /// every subsequent phase of the same dispute cycle.
    pub variant: DisputeVariant,
    /// The disputed amount in minor units. Must be `> 0`.
    pub disputed_amount_minor: i64,
    /// The disputed `(payer, invoice)` AR grain — REQUIRED for `AR_RECLASS`
    /// (the two AR legs carry it); `None` for `CASH_HOLD` (no AR leg).
    pub invoice_id: Option<String>,
    /// ISO currency of the dispute (every line shares it).
    pub currency: String,
    /// Phase instant. `None` ⇒ a placeholder effective date the orchestrator
    /// overwrites before posting.
    pub effective_at: Option<OffsetDateTime>,
}

impl ChargebackInput {
    /// The `source_business_id` composite for this event: `dispute_id:cycle:phase`
    /// (the `snake_case` idempotency key, design §5).
    #[must_use]
    pub fn business_id(&self) -> String {
        format!("{}:{}:{}", self.dispute_id, self.cycle, self.phase.as_str())
    }
}

/// Build the balanced chargeback entry for `input`, given the `CASH_HOLD` cash-leg
/// size `net_minor` (`= settled_minor − fee_minor`, read out-of-txn by the
/// orchestrator before the post — the builder stays pure; `0` / ignored for the
/// AR-reclass variants, which carry no PSP fee and size on `disputed`). The
/// `(variant, phase)` legs are pinned from architecture §4.5 / design §3, Model N
/// (see the module docs for the full table):
///
/// - `opened` `CASH_HOLD` ⇒ `DR DISPUTE_HOLD (net) / CR CASH_CLEARING (net)`.
/// - `opened` `AR_RECLASS` ⇒ `DR AR DISPUTED + CR AR ACTIVE` (nets ZERO on
///   `balance_minor`, `+D` on `disputed_minor`).
/// - `won` `CASH_HOLD` ⇒ `DR CASH_CLEARING (net) / CR DISPUTE_HOLD (net)`
///   (release the hold).
/// - `won` `AR_RECLASS` ⇒ `DR AR ACTIVE + CR AR DISPUTED` (nets ZERO on
///   `balance_minor`, `−D` on `disputed_minor`); no cash.
/// - `lost` `CASH_HOLD` ⇒ `DR DISPUTE_LOSS_EXPENSE (net) / CR DISPUTE_HOLD (net)`
///   (forfeit the already-withheld cash out of the hold; `CASH_CLEARING`
///   untouched — the fee is already expensed at settle).
/// - `lost` `AR_RECLASS` ⇒ a write-off: `DR DISPUTE_LOSS_EXPENSE (disputed) /
///   CR AR DISPUTED (disputed)` (the lone `CR AR DISPUTED` nets `−D` on both
///   `balance_minor` and `disputed_minor`); no cash, no clawback.
///
/// `source_doc_type = CHARGEBACK`, `source_business_id =
/// "dispute_id:cycle:phase"`, `reverses_* = None`. Every line carries the
/// payer, the currency, and `seller_tenant_id = Some(tenant_id)`.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `disputed_amount_minor <= 0`, when a
/// `CASH_HOLD` variant's net cash is non-positive, or when an `AR_RECLASS` phase
/// is missing its `invoice_id`;
/// [`DomainError::InvalidDisputeTransition`] for `Partial` (behind a flag —
/// not built).
pub fn build_chargeback_entry(
    input: &ChargebackInput,
    net_minor: i64,
) -> Result<PostEntry, DomainError> {
    // Reject a non-positive disputed amount at the boundary (zero-amount lines
    // would otherwise surface deep down as the misleading `AMOUNT_OUT_OF_RANGE`).
    if input.disputed_amount_minor <= 0 {
        return Err(DomainError::InvalidRequest(format!(
            "chargeback disputed_amount_minor must be > 0, got {}",
            input.disputed_amount_minor
        )));
    }

    // CASH_HOLD legs hold the disputed claim, but never more than the payment's
    // net (`settled − fee`, threaded in as `net_minor`) — the cash that actually
    // reached `CASH_CLEARING`. A full-payment claim caps at net (the fee was
    // already expensed at settle, never in clearing); a partial claim (disputed <
    // net) holds the claim itself. AR-reclass variants ignore it (no PSP fee, no
    // cash leg).
    let cash_hold_minor = net_minor.min(input.disputed_amount_minor);
    // CASH_HOLD legs are sized on the held cash; a non-positive hold would emit
    // zero-amount lines the engine rejects deep down as the misleading
    // `AMOUNT_OUT_OF_RANGE`. Catch it at the boundary with a clear code. In
    // normal flow this is defense-in-depth: settle enforces net > 0 and the
    // open-cycle guard enforces a stored hold on `won`/`lost`.
    if input.variant == DisputeVariant::CashHold && cash_hold_minor <= 0 {
        return Err(DomainError::InvalidRequest(format!(
            "chargeback CASH_HOLD net cash must be > 0, got {cash_hold_minor}"
        )));
    }
    let lines = match (input.phase, input.variant) {
        (DisputePhase::Opened, DisputeVariant::CashHold) => {
            opened_cash_hold_lines(input, cash_hold_minor)
        }
        (DisputePhase::Opened, DisputeVariant::ArReclass) => opened_ar_reclass_lines(input)?,
        (DisputePhase::Won, DisputeVariant::CashHold) => {
            won_cash_hold_lines(input, cash_hold_minor)
        }
        (DisputePhase::Won, DisputeVariant::ArReclass) => won_ar_reclass_lines(input)?,
        (DisputePhase::Lost, DisputeVariant::CashHold) => {
            lost_cash_hold_lines(input, cash_hold_minor)
        }
        (DisputePhase::Lost, DisputeVariant::ArReclass) => lost_ar_reclass_lines(input)?,
        // `partial` is behind a flag (design §2 / §7) — not built in this phase.
        (DisputePhase::Partial, _) => {
            return Err(DomainError::InvalidDisputeTransition(format!(
                "dispute phase {} is behind a flag (split chargeback) and not implemented",
                input.phase.as_str()
            )));
        }
    };

    Ok(PostEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: input.tenant_id,
        // Placeholder header fields the infra orchestrator overwrites before
        // posting (period, actor/correlation, real effective date for `None`).
        period_id: String::new(),
        entry_currency: input.currency.clone(),
        source_doc_type: SourceDocType::Chargeback,
        source_business_id: input.business_id(),
        effective_at: input.effective_at.map_or(
            NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or(NaiveDate::MIN),
            to_naive_date,
        ),
        posted_by_actor_id: Uuid::nil(),
        correlation_id: Uuid::nil(),
        reverses_entry_id: None,
        reverses_period_id: None,
        lines,
    })
}

/// A non-AR cash/expense line for `amount_minor` carrying the entry-wide payer +
/// currency + seller; only `account_class` + `side` + `amount_minor` differ.
/// Shared by the cash-hold legs (sized at `net`) + the lost cash/loss legs (the
/// settlement-return builder uses the same shape). `invoice_id`/`ar_status` are
/// `None` (these are not AR).
fn cash_line(
    input: &ChargebackInput,
    account_class: AccountClass,
    side: Side,
    amount_minor: i64,
) -> PostLine {
    PostLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: input.payer_tenant_id,
        seller_tenant_id: Some(input.tenant_id),
        resource_tenant_id: None,
        account_id: Uuid::nil(),
        account_class,
        gl_code: None,
        side,
        amount_minor,
        currency: input.currency.clone(),
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
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

/// The two balanced AR reclass legs for `input.invoice_id`, moving
/// `disputed_amount_minor` between the AR `ACTIVE` / `DISPUTED` sub-balances at
/// the SAME `(payer, invoice)` grain. `dr_status` is the `ar_status` on the
/// debit leg, `cr_status` on the credit leg — both AR, both the disputed amount,
/// so they net ZERO on `balance_minor` (AR-class-neutral) while the projector
/// routes the `DISPUTED` leg's signed delta into `disputed_minor`:
/// - opened ⇒ `(DR DISPUTED, CR ACTIVE)` ⇒ `+D` on `disputed_minor`;
/// - won / lost re-open ⇒ `(DR ACTIVE, CR DISPUTED)` ⇒ `−D` on `disputed_minor`.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `invoice_id` is absent (an AR reclass
/// has no receivable to move).
fn ar_reclass_lines(
    input: &ChargebackInput,
    dr_status: &str,
    cr_status: &str,
) -> Result<Vec<PostLine>, DomainError> {
    let invoice_id = require_invoice_id(input)?;
    Ok(vec![
        ar_line(input, &invoice_id, Side::Debit, dr_status),
        ar_line(input, &invoice_id, Side::Credit, cr_status),
    ])
}

/// The `invoice_id` of the disputed `(payer, invoice)` AR grain — required by
/// every AR-reclass / write-off leg.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `invoice_id` is absent (an AR reclass /
/// write-off has no receivable to move).
fn require_invoice_id(input: &ChargebackInput) -> Result<String, DomainError> {
    input.invoice_id.clone().ok_or_else(|| {
        DomainError::InvalidRequest(
            "AR_RECLASS chargeback requires an invoice_id (the disputed receivable)".to_owned(),
        )
    })
}

/// One AR line for the disputed `(payer, invoice)` grain, sized at
/// `disputed_amount_minor`, carrying the entry-wide payer + currency + seller and
/// the given `ar_status`; only `side` + `ar_status` differ between legs. The
/// projector routes a `DISPUTED`-tagged line's signed amount (`+D` on a debit,
/// `−D` on a credit) onto `disputed_minor` in lockstep with `balance_minor`.
fn ar_line(input: &ChargebackInput, invoice_id: &str, side: Side, ar_status: &str) -> PostLine {
    PostLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: input.payer_tenant_id,
        seller_tenant_id: Some(input.tenant_id),
        resource_tenant_id: None,
        account_id: Uuid::nil(),
        account_class: AccountClass::Ar,
        gl_code: None,
        side,
        amount_minor: input.disputed_amount_minor,
        currency: input.currency.clone(),
        invoice_id: Some(invoice_id.to_owned()),
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: Some(ar_status.to_owned()),
    }
}

/// The `CASH_HOLD` `opened` legs: move the settled cash into the hold.
/// `DR DISPUTE_HOLD (net) / CR CASH_CLEARING (net)`. Sized at `net_minor`
/// (`CASH_CLEARING` only holds net — the PSP fee never entered it; Model N). No
/// invoice / AR.
fn opened_cash_hold_lines(input: &ChargebackInput, net_minor: i64) -> Vec<PostLine> {
    // DR DISPUTE_HOLD (cash parked in the hold) + CR CASH_CLEARING (cash leaves
    // clearing). Σ DR = net = Σ CR.
    vec![
        cash_line(input, AccountClass::DisputeHold, Side::Debit, net_minor),
        cash_line(input, AccountClass::CashClearing, Side::Credit, net_minor),
    ]
}

/// The `CASH_HOLD` `won` legs: release the hold back to clearing — the reverse
/// of the opened cash leg. `DR CASH_CLEARING (net) / CR DISPUTE_HOLD (net)`.
/// Sized at `net_minor` (the reverse of the net-sized opened leg; Model N). The
/// withheld net cash is the seller's again. No clawback.
fn won_cash_hold_lines(input: &ChargebackInput, net_minor: i64) -> Vec<PostLine> {
    vec![
        cash_line(input, AccountClass::CashClearing, Side::Debit, net_minor),
        cash_line(input, AccountClass::DisputeHold, Side::Credit, net_minor),
    ]
}

/// The `AR_RECLASS` `won` legs: reverse the opened reclass `DISPUTED → ACTIVE`
/// (`DR AR ACTIVE + CR AR DISPUTED`, each disputed). Nets ZERO on `balance_minor`
/// and `−D` on `disputed_minor` (the projector books the CR-side DISPUTED leg as
/// a net-down). No cash leg.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `invoice_id` is absent.
fn won_ar_reclass_lines(input: &ChargebackInput) -> Result<Vec<PostLine>, DomainError> {
    // DR AR ACTIVE (restore the active portion) + CR AR DISPUTED (clear the
    // disputed portion). The DISPUTED leg is now the CREDIT, so the projector
    // routes `−D` onto `disputed_minor`.
    ar_reclass_lines(input, AR_STATUS_ACTIVE, AR_STATUS_DISPUTED)
}

/// The `CASH_HOLD` `lost` legs: the disputed cash was already withheld at
/// `opened` (moved `CASH_CLEARING → DISPUTE_HOLD` then, sized at net), so the
/// loss is recognised out of the hold — `DR DISPUTE_LOSS_EXPENSE (net) /
/// CR DISPUTE_HOLD (net)` (release the hold, book the forfeiture). Sized at
/// `net_minor` (the hold only ever held net; Model N). No `CASH_CLEARING`
/// movement: the cash left clearing at open, so there is no further cash leg and
/// no negative-cash path. The total loss is `net` (this leg) + the PSP fee
/// already expensed at settle = gross. The orchestrator bumps
/// `clawed_back_minor` by `net` (the held funds clawed back).
fn lost_cash_hold_lines(input: &ChargebackInput, net_minor: i64) -> Vec<PostLine> {
    vec![
        cash_line(
            input,
            AccountClass::DisputeLossExpense,
            Side::Debit,
            net_minor,
        ),
        cash_line(input, AccountClass::DisputeHold, Side::Credit, net_minor),
    ]
}

/// The `AR_RECLASS` `lost` legs: a write-off. The receivable was never collected
/// (funds `not_moved`, NO PSP fee ⇒ gross = net = the receivable), so a lost
/// dispute writes it off to loss at `disputed_amount_minor`:
/// `DR DISPUTE_LOSS_EXPENSE (disputed) / CR AR (ar_status = DISPUTED) (disputed)`.
///
/// The lone `CR AR DISPUTED` nets `−D` on BOTH `balance_minor` and
/// `disputed_minor` (the projector routes a DISPUTED-tagged line's signed amount
/// onto `disputed_minor` in lockstep with `balance_minor`), so the receivable AND
/// its disputed sub-balance are both cleared by this single AR leg — no extra
/// balance leg is needed. No cash moves (nothing was ever collected), so
/// `clawed_back_minor` is unchanged.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `invoice_id` is absent.
fn lost_ar_reclass_lines(input: &ChargebackInput) -> Result<Vec<PostLine>, DomainError> {
    let invoice_id = require_invoice_id(input)?;
    // DR DISPUTE_LOSS_EXPENSE (book the loss) + CR AR DISPUTED (write the disputed
    // receivable off — clears `−D` on both balance_minor and disputed_minor).
    // Σ DR = disputed = Σ CR.
    Ok(vec![
        cash_line(
            input,
            AccountClass::DisputeLossExpense,
            Side::Debit,
            input.disputed_amount_minor,
        ),
        ar_line(input, &invoice_id, Side::Credit, AR_STATUS_DISPUTED),
    ])
}

/// The `AR_RECLASS` `opened` legs: reclass the disputed receivable
/// `ACTIVE → DISPUTED` at the `(payer, invoice)` grain. Two AR lines for the
/// SAME invoice — `DR AR (ar_status = DISPUTED)` + `CR AR (ar_status =
/// ACTIVE)`, each `disputed` — net ZERO on `balance_minor` (AR-class-neutral)
/// while the Group-A projector routes the signed `DISPUTED` delta into
/// `disputed_minor` (+D).
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `invoice_id` is absent (an AR reclass
/// has no receivable to move).
fn opened_ar_reclass_lines(input: &ChargebackInput) -> Result<Vec<PostLine>, DomainError> {
    // DR AR DISPUTED (the disputed portion) + CR AR ACTIVE (removed from the
    // active portion). AR is debit-normal, so the DR raises and the CR lowers —
    // together they net ZERO on the full open AR; the projector books the
    // DISPUTED delta (the debit leg) into `disputed_minor` (+D).
    ar_reclass_lines(input, AR_STATUS_DISPUTED, AR_STATUS_ACTIVE)
}

/// Does a `lost` outcome on this input claw cash back out (so the orchestrator
/// must bump `clawed_back_minor`)? `true` (returning the clawed amount) only when
/// cash actually leaves on the post:
/// - `CASH_HOLD` `lost` ⇒ the held amount `min(disputed, net_minor)` (the
///   withheld net hold funds are forfeited; the PSP fee was already expensed at
///   settle, never held — so the claw caps at the payment's net, mirroring the
///   builder's `cash_hold_minor`);
/// - `AR_RECLASS` `lost` ⇒ `0` (a write-off — nothing was ever collected, so
///   there is no cash to claw back; see [`lost_ar_reclass_lines`]).
///
/// Any non-`lost` phase claws nothing back. Mirrors the builder's branch so the
/// counter the sidecar bumps matches the cash the entry actually moved.
#[must_use]
pub fn clawed_back_on_post(input: &ChargebackInput, net_minor: i64) -> i64 {
    match (input.phase, input.variant) {
        (DisputePhase::Lost, DisputeVariant::CashHold) => {
            net_minor.min(input.disputed_amount_minor)
        }
        _ => 0,
    }
}

#[cfg(test)]
#[path = "chargeback_tests.rs"]
mod tests;
