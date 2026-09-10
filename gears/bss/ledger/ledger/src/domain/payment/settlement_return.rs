//! Settlement-return entry builder (architecture §4.2). Reverses a settled
//! receipt SYMMETRICALLY (Model N, D1) — the mirror image of the settle entry,
//! which under Model N split the gross into NET cash + the PSP fee:
//!
//! - **DR `UNALLOCATED`** (`amount`) — remove the returned gross from the pool.
//! - **CR `CASH_CLEARING`** (`amount − fee_share`) — the NET cash leaves clearing
//!   (clearing only ever held net; the fee never entered it).
//! - **CR `PSP_FEE_EXPENSE`** (`fee_share`) — reverse the proportional slice of the
//!   fee that was expensed at settle. **Omitted entirely when `fee_share == 0`**
//!   (never a zero line — matches how settle omits its fee leg).
//!
//! `Σ DR == Σ CR == amount_minor` exactly (`amount = (amount − fee_share) +
//! fee_share`). A full return (`amount = gross`, `fee_share = fee`) is the exact
//! mirror of settle. Mirrors the settlement builder's shape: each line carries a
//! placeholder nil `account_id` (the `crate::infra` orchestrator binds the real
//! chart row before posting) and placeholder header fields it likewise overwrites
//! (`period_id`, `posted_by_actor_id`, `correlation_id`, and — when `effective_at`
//! is `None` — `effective_at`).
//!
//! The builder stays PURE: it sizes the legs on the `fee_share` the orchestrator
//! hands it (computed proportionally against the CURRENT remaining balances so
//! repeated partial returns stay proportional) — it does NO IO and reads no
//! settlement row.
//!
//! Scope (Phase 4, Group D): the happy-path clawback-from-pool. One edge is
//! tracked as a follow-up (not built here):
//! - **over-allocated** — a return that would push `allocated_minor >
//!   settled_minor` must route to the exception queue rather than auto-post.
//!
//! The §4.5 documented-loss-on-negative-cash substitution is GONE (Model N
//! removed it): the symmetric reverse no longer over-credits `CASH_CLEARING` by
//! the fee. A genuine pool/cash underflow stays on the engine's guarded
//! no-negative CHECK (reject), and the `settled_minor` cap CHECK rejects an
//! over-allocated return (surfaced as
//! [`DomainError::SettlementReturnOverAllocated`]) — neither auto-posts a wrong
//! entry.

use bss_ledger_sdk::{AccountClass, MappingStatus, PostEntry, PostLine, Side, SourceDocType};

use toolkit_macros::domain_model;
use uuid::Uuid;

use chrono::NaiveDate;

use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use time::OffsetDateTime;

/// A settlement to claw back (architecture §4.2 input). `amount_minor` is the
/// gross amount the PSP returned; it decrements the original payment's
/// `settled_minor` and leaves the pool.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementReturnInput {
    /// The seller tenant whose ledger this posts into (`= entry.tenant_id`), and
    /// the `seller_tenant_id` stamped on each line.
    pub tenant_id: Uuid,
    /// The tenant that paid (the single payer of the entry).
    pub payer_tenant_id: Uuid,
    /// The original settled payment being clawed back — the `payment_settlement`
    /// row whose `settled_minor` this return decrements.
    pub payment_id: String,
    /// External return identity — the `SETTLEMENT_RETURN` idempotency business id
    /// (`source_business_id`).
    pub psp_return_id: String,
    /// Amount returned in minor units. Must be `> 0`.
    pub amount_minor: i64,
    /// ISO currency of the return (every line shares it).
    pub currency: String,
    /// Return instant. `None` ⇒ a placeholder effective date the orchestrator
    /// overwrites before posting (see module docs).
    pub effective_at: Option<OffsetDateTime>,
}

/// Build the balanced settlement-return entry for `input`, sized for a return of
/// `amount_minor` given the proportional `fee_share_minor` the orchestrator
/// computed against the current remaining balances (Model N, symmetric reverse).
///
/// Lines: DR `UNALLOCATED` (`amount`), CR `CASH_CLEARING` (`amount − fee_share`),
/// and — only when `fee_share > 0` — CR `PSP_FEE_EXPENSE` (`fee_share`).
/// `Σ DR == Σ CR == amount`. A full return (`amount = gross`, `fee_share = fee`)
/// is the exact mirror of settle; `fee_share = 0` omits the fee leg (a 2-leg
/// entry identical to the pre-Model-N shape).
/// `source_doc_type = SETTLEMENT_RETURN`, `source_business_id = psp_return_id`,
/// `reverses_* = None` (this is a fresh compensating post, not an entry
/// reversal). Every line carries the payer, the currency, and
/// `seller_tenant_id = Some(tenant_id)`; `invoice_id` is `None`.
///
/// The builder is PURE — it only sizes the legs on the numbers handed in; the
/// orchestrator reads the settlement and computes `fee_share` before calling.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `amount_minor <= 0` (a meaningless
/// return — there is nothing to claw back), or when `fee_share_minor` is out of
/// the `0 ..= amount_minor` range (a fee slice larger than the return, or
/// negative, can't be reversed — a defensive guard on the orchestrator's
/// arithmetic).
pub fn build_settlement_return_entry(
    input: &SettlementReturnInput,
    fee_share_minor: i64,
) -> Result<PostEntry, DomainError> {
    // Reject a non-positive return at the boundary with a precise
    // `InvalidRequest`: zero-amount lines would otherwise surface deep down as
    // the misleading `AMOUNT_OUT_OF_RANGE`.
    if input.amount_minor <= 0 {
        return Err(DomainError::InvalidRequest(format!(
            "settlement return amount_minor must be > 0, got {}",
            input.amount_minor
        )));
    }
    // Defensive: the fee slice being reversed must fit within the return
    // (`0 <= fee_share <= amount`). The orchestrator computes
    // `fee_share = fee × amount / settled` with `fee <= settled` and
    // `amount <= settled`, so this always holds; a breach is a programming
    // error, surfaced as `InvalidRequest` rather than an unbalanced entry.
    if fee_share_minor < 0 || fee_share_minor > input.amount_minor {
        return Err(DomainError::InvalidRequest(format!(
            "settlement return fee_share_minor must be in 0..={}, got {fee_share_minor}",
            input.amount_minor
        )));
    }

    // A nil account_id / Resolved status line carrying the entry-wide payer +
    // currency; only the class / side / amount differ per line.
    let line = |account_class: AccountClass, side: Side, amount_minor: i64| PostLine {
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
    };

    // Symmetric reverse of settle (Model N): DR UNALLOCATED (claw the gross back
    // from the pool) + CR CASH_CLEARING (the NET cash leaves) + CR
    // PSP_FEE_EXPENSE (reverse the proportional fee slice). Omit the fee leg
    // entirely when `fee_share == 0` — never a zero line. Σ DR = amount =
    // (amount − fee_share) + fee_share = Σ CR.
    let mut lines: Vec<PostLine> = vec![
        line(AccountClass::Unallocated, Side::Debit, input.amount_minor),
        line(
            AccountClass::CashClearing,
            Side::Credit,
            input.amount_minor - fee_share_minor,
        ),
    ];
    if fee_share_minor > 0 {
        lines.push(line(
            AccountClass::PspFeeExpense,
            Side::Credit,
            fee_share_minor,
        ));
    }

    Ok(PostEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: input.tenant_id,
        // Placeholder header fields the infra orchestrator overwrites before
        // posting (period, actor/correlation, real effective date for `None`).
        period_id: String::new(),
        entry_currency: input.currency.clone(),
        source_doc_type: SourceDocType::SettlementReturn,
        source_business_id: input.psp_return_id.clone(),
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

#[cfg(test)]
#[path = "settlement_return_tests.rs"]
mod tests;
