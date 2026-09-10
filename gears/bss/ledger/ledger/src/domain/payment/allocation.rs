//! Allocation-entry builder (architecture §5.2, **Pattern A apply**). Turns a
//! decided split of the unallocated pool into a balanced [`PostEntry`] that
//! drains the pool into the receivables it pays:
//!
//! - **DR `UNALLOCATED`** — one line for the sum of the splits (the amount leaving
//!   the pool).
//! - **CR `AR`** — one line per split, each carrying its `invoice_id` (the
//!   receivable that share pays down).
//!
//! `Σ DR (= Σ splits) == Σ CR (= Σ splits)` exactly — pure `i64`, summed via
//! `i128`. The split amounts come from
//! [`crate::domain::payment::precedence::oldest_first`]. Lines carry a placeholder
//! nil `account_id` (bound by the `crate::infra` orchestrator from
//! `(account_class, currency)` before posting) and placeholder header fields it
//! likewise overwrites (`period_id`, `posted_by_actor_id`, `correlation_id`, and
//! — when `effective_at` is `None` — `effective_at`).

use bss_ledger_sdk::{AccountClass, MappingStatus, PostEntry, PostLine, Side, SourceDocType};

use toolkit_macros::domain_model;
use uuid::Uuid;

use chrono::NaiveDate;

use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::payment::precedence::{Allocated, Candidate};
use time::OffsetDateTime;

/// A decided allocation to post (Pattern A apply input): which invoices the pool
/// pays and by how much.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationInput {
    /// The seller tenant whose ledger this posts into (`= entry.tenant_id`), and
    /// the `seller_tenant_id` stamped on each line.
    pub tenant_id: Uuid,
    /// The tenant whose receivables are being paid (the single payer of the
    /// entry).
    pub payer_tenant_id: Uuid,
    /// External payment identity (lineage — the payment whose pool this drains).
    pub payment_id: String,
    /// Allocation identity — the `PAYMENT_ALLOCATE` idempotency business id
    /// (`source_business_id = allocation_id.to_string()`).
    pub allocation_id: Uuid,
    /// ISO currency of the allocation (every line shares it).
    pub currency: String,
    /// The per-invoice shares to apply (from the precedence policy). Must be
    /// non-empty and every `amount_minor` must be `> 0`.
    pub splits: Vec<Allocated>,
    /// Allocation instant. `None` ⇒ a placeholder effective date the orchestrator
    /// overwrites before posting (see module docs).
    pub effective_at: Option<OffsetDateTime>,
}

/// Build the balanced Pattern-A allocation entry for `input`.
///
/// Lines: one DR `UNALLOCATED` for `Σ splits` FIRST, then one CR `AR` per split
/// (in `splits` order), each carrying `invoice_id = Some(split.invoice_id)`.
/// `source_doc_type = PAYMENT_ALLOCATE`, `source_business_id =
/// allocation_id.to_string()`, `reverses_* = None`. Every line carries the payer,
/// the currency, and `seller_tenant_id = Some(tenant_id)`; the UNALLOCATED line
/// has `invoice_id = None`.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `splits` is empty, or any split has
/// `amount_minor <= 0` (an empty / unrepresentable allocation).
pub fn build_allocation_entry(input: &AllocationInput) -> Result<PostEntry, DomainError> {
    if input.splits.is_empty() {
        return Err(DomainError::InvalidRequest(
            "allocation has no splits".to_owned(),
        ));
    }
    for split in &input.splits {
        if split.amount_minor <= 0 {
            return Err(DomainError::InvalidRequest(format!(
                "allocation split for invoice {} must be > 0, got {}",
                split.invoice_id, split.amount_minor
            )));
        }
    }

    // Σ splits = the amount leaving the pool (the single DR UNALLOCATED). Widened
    // to i128 while folding to dodge an intermediate overflow (mirrors the invoice
    // builder); the per-entry headroom guard keeps the total within i64.
    let sum_minor: i128 = input
        .splits
        .iter()
        .map(|s| i128::from(s.amount_minor))
        .sum();
    // Σ splits is bounded by `lump_minor` (an i64) on both the decided and the
    // caller-split paths, so this never overflows in practice; still, guard it as
    // an explicit `AmountOutOfRange` rather than silently clamping to `i64::MAX`
    // — a clamp would emit a `DR UNALLOCATED` that no longer equals `Σ CR AR`,
    // which the engine then rejects as "unbalanced", a misleading code for what
    // is really an over-range total.
    let sum_minor = i64::try_from(sum_minor).map_err(|_| {
        DomainError::AmountOutOfRange(format!(
            "allocation total {sum_minor} exceeds the representable i64 range"
        ))
    })?;

    // A nil account_id / Resolved status line carrying the entry-wide payer +
    // currency; only the class / side / amount / invoice_id differ per line.
    let line = |account_class: AccountClass,
                side: Side,
                amount_minor: i64,
                invoice_id: Option<String>| PostLine {
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
        invoice_id,
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

    // DR UNALLOCATED (Σ) first, then one CR AR per split (invoice_id carried). Σ
    // DR = Σ splits = Σ CR.
    let mut lines: Vec<PostLine> = Vec::with_capacity(1 + input.splits.len());
    lines.push(line(
        AccountClass::Unallocated,
        Side::Debit,
        sum_minor,
        None,
    ));
    for split in &input.splits {
        lines.push(line(
            AccountClass::Ar,
            Side::Credit,
            split.amount_minor,
            Some(split.invoice_id.clone()),
        ));
    }

    Ok(PostEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: input.tenant_id,
        // Placeholder header fields the infra orchestrator overwrites before
        // posting (period, actor/correlation, and a real effective date for the
        // `None` case) — mirrors the nil account_id.
        period_id: String::new(),
        entry_currency: input.currency.clone(),
        source_doc_type: SourceDocType::PaymentAllocate,
        source_business_id: input.allocation_id.to_string(),
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

/// Validate a **caller-computed** allocation split against the open candidate
/// set (architecture §4.4 F-5, Mode B). The escape hatch where the caller
/// supplies the per-invoice shares instead of letting a precedence policy decide
/// them; this subjects that split to the SAME invariants the decided path is
/// implicitly subject to, so the two paths post under identical guarantees.
///
/// Each caller split must name a present candidate with `open_minor > 0`, carry
/// `0 < amount_minor <= that candidate's open_minor`, and appear at most once;
/// the splits together must not exceed `lump_minor`. On success the validated
/// splits are returned in the caller's order (the order the resulting CR AR
/// lines are built in) — never reordered or coalesced.
///
/// # Errors
/// [`DomainError::AllocationSplitInvalid`] when any split names an unknown or
/// closed (`open_minor <= 0`) candidate, exceeds that candidate's open balance,
/// is non-positive, repeats an invoice, or the splits sum past `lump_minor`.
pub fn validate_caller_split(
    candidates: &[Candidate],
    caller: &[Allocated],
    lump_minor: i64,
) -> Result<Vec<Allocated>, DomainError> {
    let mut seen: Vec<&str> = Vec::with_capacity(caller.len());
    let mut sum: i128 = 0;
    for split in caller {
        // Reject a duplicate invoice_id: two splits for the same receivable are
        // ambiguous (which CR AR line wins?) and the precedence path never emits
        // one, so the caller path must not either.
        if seen.contains(&split.invoice_id.as_str()) {
            return Err(DomainError::AllocationSplitInvalid(format!(
                "duplicate invoice {} in caller split",
                split.invoice_id
            )));
        }
        seen.push(split.invoice_id.as_str());

        // Each share must be representable and positive — a zero/negative
        // allocation is meaningless (mirrors the decided path, which never emits
        // a non-positive share).
        if split.amount_minor <= 0 {
            return Err(DomainError::AllocationSplitInvalid(format!(
                "caller split for invoice {} must be > 0, got {}",
                split.invoice_id, split.amount_minor
            )));
        }

        // The invoice must be a present, still-open candidate, and the share may
        // not exceed its open balance (the per-invoice cap the decided fill is
        // bounded by via `min(remaining, open_minor)`).
        let candidate = candidates
            .iter()
            .find(|c| c.invoice_id == split.invoice_id)
            .ok_or_else(|| {
                DomainError::AllocationSplitInvalid(format!(
                    "caller split names invoice {} which is not an open candidate",
                    split.invoice_id
                ))
            })?;
        if candidate.open_minor <= 0 {
            return Err(DomainError::AllocationSplitInvalid(format!(
                "caller split names invoice {} which is closed (open {})",
                split.invoice_id, candidate.open_minor
            )));
        }
        if split.amount_minor > candidate.open_minor {
            return Err(DomainError::AllocationSplitInvalid(format!(
                "caller split for invoice {} ({}) exceeds its open balance ({})",
                split.invoice_id, split.amount_minor, candidate.open_minor
            )));
        }

        sum += i128::from(split.amount_minor);
    }

    // The splits together may not exceed the lump (the decided path can only
    // give out what `remaining` allows; the caller path is bounded the same).
    // Widened to i128 so the fold can't overflow before the comparison.
    if sum > i128::from(lump_minor) {
        return Err(DomainError::AllocationSplitInvalid(format!(
            "caller split total {sum} exceeds lump {lump_minor}"
        )));
    }

    Ok(caller.to_vec())
}

#[cfg(test)]
#[path = "allocation_tests.rs"]
mod tests;
