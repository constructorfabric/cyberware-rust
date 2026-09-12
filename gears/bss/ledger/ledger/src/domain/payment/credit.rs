//! Reusable-credit (wallet) money-shapes (architecture §5.2). Two balanced
//! [`PostEntry`] builders move money in and out of a tenant's reusable-credit
//! wallet, plus the pure planner + validator that decide the two sides of a
//! spend before it is built:
//!
//! - **Grant** ([`build_grant_entry`]) — parks unallocated pool cash into the
//!   wallet. **DR `UNALLOCATED`** (amount) / **CR `REUSABLE_CREDIT`** (amount). The
//!   credit line carries `credit_grant_event_type` (the wallet sub-grain bucket
//!   the balance accrues to); the pool line does not.
//! - **Apply** ([`build_apply_entry`]) — spends the wallet against receivables.
//!   **N×DR `REUSABLE_CREDIT`** (one per drawn-down sub-grain, each carrying its
//!   `credit_grant_event_type`) / **M×CR `AR`** (one per receivable, each carrying
//!   its `invoice_id`). `Σ DR == Σ CR` exactly.
//!
//! Both postings use `source_doc_type = CREDIT_APPLY` (there is no separate
//! `CREDIT_GRANT` doc type) and `source_business_id = credit_application_id`.
//!
//! **The two-sided event-type rule** (DB CHECK `chk_journal_line_credit_grant`): a
//! line carries `credit_grant_event_type = Some(..)` **iff** its class is
//! `REUSABLE_CREDIT`. So in the grant the CR `REUSABLE_CREDIT` line sets it and the
//! DR `UNALLOCATED` line leaves it `None`; in the apply every DR `REUSABLE_CREDIT`
//! line sets it and every CR `AR` line leaves it `None`.
//!
//! **The three caps and where each is enforced.** Two are *available*-balance caps
//! the orchestrator (Group C) checks against live ledger state — these builders
//! never see the balances, they validate only shape:
//! - `GrantExceedsUnallocated` — the grant amount must not exceed the payer's
//!   available unallocated pool (orchestrator-side).
//! - `CreditExceedsWallet` — the spend must not exceed the available wallet. The
//!   *shape* of this is enforced here by [`plan_wallet_debit`], which refuses to
//!   fill more than the sub-grain availabilities it is handed (`Σ available <
//!   amount` ⇒ `CreditExceedsWallet`); the orchestrator is what supplies the live
//!   availabilities.
//! - `CreditExceedsOpenAr` — each spent target must name an open receivable and not
//!   exceed its open balance; enforced here by [`validate_credit_targets`] against
//!   the open candidate set the orchestrator supplies.
//!
//! Pure throughout (no infra / DB imports — dylint DE0301): sums fold through
//! `i128` to dodge an intermediate overflow. The emitted lines carry a placeholder
//! nil `account_id` (the `crate::infra` orchestrator binds the real chart row from
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

/// A credit grant to post: how much pool cash to park into which wallet
/// sub-grain.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantInput {
    /// The seller tenant whose ledger this posts into (`= entry.tenant_id`), and
    /// the `seller_tenant_id` stamped on each line.
    pub tenant_id: Uuid,
    /// The tenant whose wallet is credited (the single payer of the entry).
    pub payer_tenant_id: Uuid,
    /// The `CREDIT_APPLY` idempotency business id (`source_business_id`).
    pub credit_application_id: String,
    /// ISO currency of the grant (every line shares it).
    pub currency: String,
    /// Amount to park into the wallet in minor units. Must be `> 0`.
    pub amount_minor: i64,
    /// The wallet sub-grain bucket the credit accrues to (carried on the
    /// `REUSABLE_CREDIT` line). Must be non-empty.
    pub credit_grant_event_type: String,
    /// Grant instant. `None` ⇒ a placeholder effective date the orchestrator
    /// overwrites before posting (see module docs).
    pub effective_at: Option<OffsetDateTime>,
}

/// A wallet sub-grain available to spend. The caller supplies these in
/// oldest-grant-first order — [`plan_wallet_debit`] fills them verbatim in the
/// given order (it does NOT sort).
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreditSubgrain {
    /// The sub-grain bucket (matches a grant's `credit_grant_event_type`).
    pub credit_grant_event_type: String,
    /// Remaining available credit in this sub-grain, minor units. Non-positive
    /// availabilities are skipped during the fill.
    pub available_minor: i64,
}

/// One per-sub-grain debit the apply will post: a `REUSABLE_CREDIT` draw-down of
/// `amount_minor` from the named sub-grain. Always `> 0` (the planner never emits
/// a zero debit). Consumed by [`build_apply_entry`].
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreditDebit {
    /// The sub-grain this draw-down comes from (carried onto the DR
    /// `REUSABLE_CREDIT` line's `credit_grant_event_type`).
    pub credit_grant_event_type: String,
    /// Amount drawn from this sub-grain in minor units (always `> 0`).
    pub amount_minor: i64,
}

/// Build the balanced credit-grant entry for `input`.
///
/// Lines: DR `UNALLOCATED` (`amount`, `credit_grant_event_type = None`), CR
/// `REUSABLE_CREDIT` (`amount`, `credit_grant_event_type =
/// Some(input.credit_grant_event_type)`). `source_doc_type = CREDIT_APPLY`,
/// `source_business_id = credit_application_id`, `reverses_* = None`. Both lines
/// carry the payer, the currency, and `seller_tenant_id = Some(tenant_id)`;
/// `invoice_id` is `None` (a grant pays no receivable). `Σ DR (= amount) == Σ CR
/// (= amount)`.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `amount_minor <= 0` or
/// `credit_grant_event_type` is empty (an unrepresentable / unbucketed grant).
pub fn build_grant_entry(input: &GrantInput) -> Result<PostEntry, DomainError> {
    if input.amount_minor <= 0 {
        return Err(DomainError::InvalidRequest(format!(
            "credit grant amount_minor must be > 0, got {}",
            input.amount_minor
        )));
    }
    if input.credit_grant_event_type.is_empty() {
        return Err(DomainError::InvalidRequest(
            "credit grant credit_grant_event_type must not be empty".to_owned(),
        ));
    }

    // A nil account_id / Resolved status line carrying the entry-wide payer +
    // currency; only the class / side / amount / event-type differ per line.
    let line = |account_class: AccountClass,
                side: Side,
                amount_minor: i64,
                credit_grant_event_type: Option<String>| PostLine {
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
        credit_grant_event_type,
        ar_status: None,
    };

    // DR UNALLOCATED (amount, no event-type) + CR REUSABLE_CREDIT (amount,
    // carrying the sub-grain bucket — the two-sided event-type rule). Σ DR =
    // amount = Σ CR.
    let lines: Vec<PostLine> = vec![
        line(
            AccountClass::Unallocated,
            Side::Debit,
            input.amount_minor,
            None,
        ),
        line(
            AccountClass::ReusableCredit,
            Side::Credit,
            input.amount_minor,
            Some(input.credit_grant_event_type.clone()),
        ),
    ];

    Ok(PostEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: input.tenant_id,
        // Placeholder header fields the infra orchestrator overwrites before
        // posting (period, actor/correlation, and a real effective date for the
        // `None` case) — mirrors the nil account_id.
        period_id: String::new(),
        entry_currency: input.currency.clone(),
        source_doc_type: SourceDocType::CreditApply,
        source_business_id: input.credit_application_id.clone(),
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

/// Plan a wallet spend of `amount_minor` across `subgrains` IN THE GIVEN ORDER
/// (the caller sorts oldest-grant-first; this never reorders).
///
/// Walks `subgrains` in order, taking `give = min(remaining, available)` from
/// each, skipping any with `available_minor <= 0`, and stopping once `remaining`
/// hits zero. Returns the per-sub-grain debits in fill order, positive amounts
/// only (never a zero debit). The wallet-side cap is enforced here: the sub-grain
/// availabilities must cover the full amount.
///
/// # Errors
/// [`DomainError::CreditExceedsWallet`] when `Σ available_minor < amount_minor`
/// (the wallet cannot cover the spend); [`DomainError::InvalidRequest`] when
/// `amount_minor <= 0` (an unrepresentable spend).
pub fn plan_wallet_debit(
    subgrains: &[CreditSubgrain],
    amount_minor: i64,
) -> Result<Vec<CreditDebit>, DomainError> {
    if amount_minor <= 0 {
        return Err(DomainError::InvalidRequest(format!(
            "wallet debit amount_minor must be > 0, got {amount_minor}"
        )));
    }

    // Total available across the sub-grains (the wallet cap). Widened to i128
    // while folding to dodge an intermediate overflow; non-positive availabilities
    // contribute nothing to spendable capacity, so clamp them at 0 here exactly as
    // the fill below skips them.
    let available_total: i128 = subgrains
        .iter()
        .map(|s| i128::from(s.available_minor.max(0)))
        .sum();
    if available_total < i128::from(amount_minor) {
        return Err(DomainError::CreditExceedsWallet(format!(
            "wallet debit {amount_minor} exceeds available {available_total}"
        )));
    }

    // Fill in the given order: each sub-grain gives min(remaining, available),
    // skipping non-positive availabilities, stopping at 0. The cap check above
    // guarantees `remaining` reaches 0 before the list is exhausted.
    let mut remaining = amount_minor;
    let mut out: Vec<CreditDebit> = Vec::new();
    for sg in subgrains {
        if remaining == 0 {
            break;
        }
        // Nothing available ⇒ nothing to draw (and never a negative debit).
        if sg.available_minor <= 0 {
            continue;
        }
        let give = remaining.min(sg.available_minor);
        if give > 0 {
            out.push(CreditDebit {
                credit_grant_event_type: sg.credit_grant_event_type.clone(),
                amount_minor: give,
            });
            remaining -= give;
        }
    }
    Ok(out)
}

/// Validate caller-named AR targets against the open candidate set — the
/// reusable-credit apply's receivable side. Mirrors
/// [`crate::domain::payment::allocation::validate_caller_split`] but with no lump
/// check (the wallet-side cap is [`plan_wallet_debit`], not a single lump) and a
/// `CreditExceedsOpenAr` error.
///
/// Each target must name a present candidate with `open_minor > 0`, carry `0 <
/// amount_minor <= that candidate's open_minor`, and appear at most once. On
/// success the validated targets are returned in the caller's order (the order
/// the resulting CR AR lines are built in) — never reordered or coalesced.
///
/// # Errors
/// [`DomainError::CreditExceedsOpenAr`] when any target names an unknown or closed
/// (`open_minor <= 0`) candidate, exceeds that candidate's open balance, is
/// non-positive, or repeats an invoice.
pub fn validate_credit_targets(
    candidates: &[Candidate],
    targets: &[Allocated],
) -> Result<Vec<Allocated>, DomainError> {
    let mut seen: Vec<&str> = Vec::with_capacity(targets.len());
    for target in targets {
        // Reject a duplicate invoice_id: two targets for the same receivable are
        // ambiguous (which CR AR line wins?), so the apply path must not emit one.
        if seen.contains(&target.invoice_id.as_str()) {
            return Err(DomainError::CreditExceedsOpenAr(format!(
                "duplicate invoice {} in credit targets",
                target.invoice_id
            )));
        }
        seen.push(target.invoice_id.as_str());

        // Each target must be representable and positive — a zero/negative
        // application is meaningless.
        if target.amount_minor <= 0 {
            return Err(DomainError::CreditExceedsOpenAr(format!(
                "credit target for invoice {} must be > 0, got {}",
                target.invoice_id, target.amount_minor
            )));
        }

        // The invoice must be a present, still-open candidate, and the target may
        // not exceed its open balance (the per-invoice cap).
        let candidate = candidates
            .iter()
            .find(|c| c.invoice_id == target.invoice_id)
            .ok_or_else(|| {
                DomainError::CreditExceedsOpenAr(format!(
                    "credit target names invoice {} which is not an open candidate",
                    target.invoice_id
                ))
            })?;
        if candidate.open_minor <= 0 {
            return Err(DomainError::CreditExceedsOpenAr(format!(
                "credit target names invoice {} which is closed (open {})",
                target.invoice_id, candidate.open_minor
            )));
        }
        if target.amount_minor > candidate.open_minor {
            return Err(DomainError::CreditExceedsOpenAr(format!(
                "credit target for invoice {} ({}) exceeds its open balance ({})",
                target.invoice_id, target.amount_minor, candidate.open_minor
            )));
        }
    }

    Ok(targets.to_vec())
}

/// A reusable-credit application to post: the DR wallet side (from
/// [`plan_wallet_debit`]) and the CR receivable side (from
/// [`validate_credit_targets`]).
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyInput {
    /// The seller tenant whose ledger this posts into (`= entry.tenant_id`), and
    /// the `seller_tenant_id` stamped on each line.
    pub tenant_id: Uuid,
    /// The tenant whose wallet is spent and whose receivables are paid (the single
    /// payer of the entry).
    pub payer_tenant_id: Uuid,
    /// The `CREDIT_APPLY` idempotency business id (`source_business_id`).
    pub credit_application_id: String,
    /// ISO currency of the application (every line shares it).
    pub currency: String,
    /// The DR side: per-sub-grain wallet draw-downs (from `plan_wallet_debit`).
    /// Must be non-empty and every `amount_minor` must be `> 0`.
    pub debits: Vec<CreditDebit>,
    /// The CR side: per-invoice receivable shares (from `validate_credit_targets`).
    /// Must be non-empty and every `amount_minor` must be `> 0`.
    pub targets: Vec<Allocated>,
    /// Application instant. `None` ⇒ a placeholder effective date the orchestrator
    /// overwrites before posting (see module docs).
    pub effective_at: Option<OffsetDateTime>,
}

/// Build the balanced reusable-credit apply entry for `input`.
///
/// Lines: one DR `REUSABLE_CREDIT` per debit FIRST (in `debits` order, each
/// carrying `credit_grant_event_type = Some(debit.credit_grant_event_type)`,
/// `invoice_id = None`), then one CR `AR` per target (in `targets` order, each
/// carrying `invoice_id = Some(target.invoice_id)`, `credit_grant_event_type =
/// None` — the two-sided event-type rule). `source_doc_type = CREDIT_APPLY`,
/// `source_business_id = credit_application_id`, `reverses_* = None`. Every line
/// carries the payer, the currency, and `seller_tenant_id = Some(tenant_id)`.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `debits` or `targets` is empty, any
/// amount is `<= 0`, or `Σ debits != Σ targets`. The orchestrator guarantees the
/// two sides are equal (it sizes the debit plan to the targets); this is the
/// balance backstop that refuses to post an unbalanced entry.
pub fn build_apply_entry(input: &ApplyInput) -> Result<PostEntry, DomainError> {
    if input.debits.is_empty() {
        return Err(DomainError::InvalidRequest(
            "credit apply has no debits".to_owned(),
        ));
    }
    if input.targets.is_empty() {
        return Err(DomainError::InvalidRequest(
            "credit apply has no targets".to_owned(),
        ));
    }
    for debit in &input.debits {
        if debit.amount_minor <= 0 {
            return Err(DomainError::InvalidRequest(format!(
                "credit apply debit for sub-grain {} must be > 0, got {}",
                debit.credit_grant_event_type, debit.amount_minor
            )));
        }
    }
    for target in &input.targets {
        if target.amount_minor <= 0 {
            return Err(DomainError::InvalidRequest(format!(
                "credit apply target for invoice {} must be > 0, got {}",
                target.invoice_id, target.amount_minor
            )));
        }
    }

    // Σ DR (wallet draw-downs) must equal Σ CR (receivable shares). Both folds are
    // widened to i128 to dodge an intermediate overflow before the comparison.
    let debit_total: i128 = input
        .debits
        .iter()
        .map(|d| i128::from(d.amount_minor))
        .sum();
    let credit_total: i128 = input
        .targets
        .iter()
        .map(|t| i128::from(t.amount_minor))
        .sum();
    if debit_total != credit_total {
        return Err(DomainError::InvalidRequest(format!(
            "credit apply does not balance: Σ debits {debit_total} != Σ targets {credit_total}"
        )));
    }

    // A nil account_id / Resolved status line carrying the entry-wide payer +
    // currency; only the class / side / amount / invoice_id / event-type differ.
    let line = |account_class: AccountClass,
                side: Side,
                amount_minor: i64,
                invoice_id: Option<String>,
                credit_grant_event_type: Option<String>| PostLine {
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
        credit_grant_event_type,
        ar_status: None,
    };

    // N×DR REUSABLE_CREDIT (each carrying its sub-grain event-type, no invoice)
    // first, then M×CR AR (each carrying its invoice_id, no event-type). Σ DR = Σ
    // debits = Σ targets = Σ CR.
    let mut lines: Vec<PostLine> = Vec::with_capacity(input.debits.len() + input.targets.len());
    for debit in &input.debits {
        lines.push(line(
            AccountClass::ReusableCredit,
            Side::Debit,
            debit.amount_minor,
            None,
            Some(debit.credit_grant_event_type.clone()),
        ));
    }
    for target in &input.targets {
        lines.push(line(
            AccountClass::Ar,
            Side::Credit,
            target.amount_minor,
            Some(target.invoice_id.clone()),
            None,
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
        source_doc_type: SourceDocType::CreditApply,
        source_business_id: input.credit_application_id.clone(),
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
#[path = "credit_tests.rs"]
mod tests;
