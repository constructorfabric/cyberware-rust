//! `PaymentRepo` — the payment counter tables (`payment_settlement`,
//! `payment_allocation`, `payment_allocation_refund`) plus the allocation
//! candidate / view reads.
//!
//! The counter **writes** (`seed_settlement`, `add_allocated`,
//! `insert_allocation_rows`, `bump_allocation_refund`) run inside the
//! passed-in posting transaction (the in-txn sidecar, decision M): each
//! mirrors the projector's `upsert_ar_payer` shape — a scoped insert with a
//! `SecureOnConflict` that nets `col + delta` and bumps `version + 1`. The
//! per-payment cap CHECKs (`allocated_minor <= settled_minor`, the
//! refund-vs-allocated CHECK) are the concurrency backstop under
//! `SERIALIZABLE`; a violation surfaces as [`RepoError::MoneyOutCapExceeded`]
//! (the sidecar turns it into the `ALLOCATION_EXCEEDS_SETTLED` wire code).
//!
//! The **reads** (`list_open_ar_invoices`, `list_payment_allocations`,
//! `read_unallocated`, `read_settlement`, `read_effective_policy`) take the
//! PDP-compiled `AccessScope` and run through `.secure().scope_with(scope)`
//! (SQL-level BOLA — a foreign tenant yields no rows).

use bss_ledger_sdk::SourceDocType;

use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, DbErr, EntityTrait, Order};
use toolkit_db::secure::{
    AccessScope, DbTx, ScopeError, SecureEntityExt, SecureInsertExt, SecureOnConflict,
    SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::model::RepoError;
use crate::domain::payment::credit::CreditSubgrain;
use crate::domain::payment::precedence::PrecedenceStrategy;
use crate::infra::posting::idempotency::STATUS_POSTED;
use crate::infra::storage::entity::{
    account_balance, ar_invoice_balance, idempotency_dedup, payment_allocation,
    payment_allocation_refund, payment_settlement, reusable_credit_subbalance,
    tenant_precedence_policy, unallocated_balance,
};
use time::OffsetDateTime;

/// A `payment_allocation` row to insert (one per allocation split).
pub struct NewAllocationRow {
    pub tenant_id: Uuid,
    pub allocation_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub payment_id: String,
    pub invoice_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub precedence_policy_ref: String,
    pub allocated_at_utc: OffsetDateTime,
}

/// One open AR invoice in the allocation candidate set (oldest-first ordered by
/// the caller's read). `balance_minor` is the still-open amount (`> 0`).
pub struct OpenArInvoice {
    pub invoice_id: String,
    pub balance_minor: i64,
    pub original_posted_at: Option<OffsetDateTime>,
    pub currency: String,
    /// The grain's carried functional balance (Slice 5). `Some` only when the
    /// invoice was posted cross-currency (S1 stamped a functional translation);
    /// `None` for a single-currency invoice (functional ≡ transaction). The
    /// realized-FX poster reads it to value each AR leg's close at the grain's
    /// WAC carried rate.
    pub functional_balance_minor: Option<i64>,
}

/// The unallocated pool's carried balance for a `(payer, currency)` — the read
/// the realized-FX poster needs at allocation close (Slice 5). `balance_minor`
/// is the pool's transaction balance (the WAC denominator); the functional
/// fields are `Some` only on a cross-currency pool (S2 settle stamped them) and
/// drive the realized-FX cross-currency detect + the DR UNALLOCATED leg's
/// carried functional value.
pub struct UnallocatedCarried {
    pub balance_minor: i64,
    pub functional_balance_minor: Option<i64>,
    pub functional_currency: Option<String>,
}

/// A single balance-cache grain's carried `(transaction, functional)` value — the
/// read the chargeback functional carry-forward (Slice 5 F3) needs for the grain a
/// dispute phase CLOSES (`account_balance` for a `CASH_HOLD`'s `CASH_CLEARING` /
/// `DISPUTE_HOLD`; `ar_invoice_balance` for an `AR_RECLASS` invoice). `functional_*`
/// are `Some` only on a cross-currency grain (S1/S2 stamped them); `None` ⇒ a
/// single-currency close (no carry-forward). `balance_minor` is `0` (functional
/// `None`) when the grain row is absent.
pub struct CarriedBalance {
    pub balance_minor: i64,
    pub functional_balance_minor: Option<i64>,
    pub functional_currency: Option<String>,
}

/// One open, cross-currency **monetary** grain to remeasure at period end (Slice 5
/// Phase 3, the unrealized-revaluation scan). Only grains that carry a
/// `functional_currency` (cross-currency, decision 8) and an open
/// `balance_minor > 0` are listed, so `functional_balance_minor` /
/// `functional_currency` are NON-optional here (the scan filtered NULLs out). The
/// run translates `balance_minor` at the period-end rate and remeasures it against
/// `functional_balance_minor`. `invoice_id` is `Some` for an AR grain;
/// `credit_grant_event_type` is `Some` for a `REUSABLE_CREDIT` grain — both feed the
/// projector's grain key when the adjusting line is built.
pub struct RevaluationGrain {
    pub payer_tenant_id: Uuid,
    pub account_id: Uuid,
    pub currency: String,
    pub invoice_id: Option<String>,
    pub credit_grant_event_type: Option<String>,
    pub balance_minor: i64,
    pub functional_balance_minor: i64,
    pub functional_currency: String,
}

/// SeaORM-backed payment counter + read repository.
#[derive(Clone)]
pub struct PaymentRepo {
    db: DBProvider<DbError>,
}

impl PaymentRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    // --- In-txn counter writes (called by the payment post sidecars) ---

    /// Seed the `payment_settlement` row for a fresh settlement
    /// (`settled_minor = settled_minor` gross, `fee_minor = fee_minor` the PSP
    /// cut withheld so `net = settled_minor − fee_minor` is derivable; every
    /// other counter starts at 0). Idempotent under the posting txn's
    /// `SERIALIZABLE` + idempotency gate: a replay returns before the sidecar,
    /// so this is only reached on the first post for `(tenant, payment_id)`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn seed_settlement(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        currency: &str,
        settled_minor: i64,
        fee_minor: i64,
    ) -> Result<(), RepoError> {
        let am = payment_settlement::ActiveModel {
            tenant_id: Set(tenant),
            payment_id: Set(payment_id.to_owned()),
            currency: Set(currency.to_owned()),
            settled_minor: Set(settled_minor),
            fee_minor: Set(fee_minor),
            allocated_minor: Set(0),
            refunded_minor: Set(0),
            refunded_unallocated_minor: Set(0),
            clawed_back_minor: Set(0),
            version: Set(0),
        };
        payment_settlement::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("payment_settlement scope: {e}")))?
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("seed payment_settlement: {e}")))?;
        Ok(())
    }

    /// Increment `payment_settlement.allocated_minor` by `delta` for an
    /// allocation, bumping `version`. The `allocated_minor <= settled_minor`
    /// cap CHECK enforces that the running total never exceeds what was
    /// settled; a violation maps to [`RepoError::MoneyOutCapExceeded`]
    /// (`ALLOCATION_EXCEEDS_SETTLED`). This is a scoped UPDATE, not an upsert:
    /// the settlement row is always seeded first, and an `INSERT … ON CONFLICT`
    /// would trip the cap CHECK on the INSERT VALUES tuple during arbitration
    /// (see the body comment). Returns [`RepoError::Db`] if no row matched
    /// (payment not settled).
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when the cap CHECK rejects the
    /// increment; [`RepoError::Db`] on any other scope / storage failure.
    pub async fn add_allocated(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        // The settlement row always pre-exists (settle precedes allocate; an
        // allocate of an un-settled payment is rejected upstream), so this is a
        // scoped UPDATE — NOT an upsert. An `INSERT … ON CONFLICT` cannot be
        // used here: Postgres evaluates the CHECK against the INSERT VALUES
        // tuple during ON CONFLICT arbitration (see projector.rs), and a seed of
        // `(settled_minor = 0, allocated_minor = delta)` trips
        // `allocated_minor <= settled_minor` before the DO UPDATE can net
        // against the real settled amount. The UPDATE evaluates the CHECK
        // against the resulting row (`allocated_minor + delta <= settled_minor`),
        // and SSI + retry serialize concurrent allocates of the same payment —
        // an over-cap surfaces as the CHECK violation, mapped to
        // `MoneyOutCapExceeded`.
        let result = payment_settlement::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                payment_settlement::Column::AllocatedMinor,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::AllocatedMinor,
                ))
                .add(delta),
            )
            .col_expr(
                payment_settlement::Column::Version,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("add allocated_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "payment_settlement row absent for ({tenant}, {payment_id}) — not settled"
            )));
        }
        Ok(())
    }

    /// Adjust `payment_settlement.settled_minor` by `delta` — **negative** for a
    /// settlement return that claws a receipt back out — bumping `version`. A
    /// return that would drop `settled_minor` below the already
    /// allocated / refunded / clawed-back total (or below zero) trips a
    /// `chk_payment_settlement_*` cap CHECK and surfaces as
    /// [`RepoError::MoneyOutCapExceeded`] (the settlement-return sidecar maps it
    /// to `SETTLEMENT_RETURN_OVER_ALLOCATED`). A scoped UPDATE, not an upsert:
    /// the settlement row always pre-exists (a return of an un-settled payment
    /// is rejected upstream); `rows_affected == 0` ⇒ not settled. SSI + retry
    /// serialize concurrent writers of the same payment.
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when a cap CHECK rejects the change;
    /// [`RepoError::Db`] when no row matched or on any other scope / storage
    /// failure.
    pub async fn add_settled(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        // Scoped UPDATE (not an upsert): the settlement row always pre-exists, so
        // the CHECK evaluates against the resulting row
        // (`settled_minor + delta >= allocated / refunded / clawed-back`, and
        // `>= 0`). An over-claw surfaces as the CHECK violation, mapped to
        // `MoneyOutCapExceeded`; the return sidecar refines it to
        // `SettlementReturnOverAllocated`.
        let result = payment_settlement::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                payment_settlement::Column::SettledMinor,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::SettledMinor,
                ))
                .add(delta),
            )
            .col_expr(
                payment_settlement::Column::Version,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("adjust settled_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "payment_settlement row absent for ({tenant}, {payment_id}) — not settled"
            )));
        }
        Ok(())
    }

    /// Adjust `payment_settlement.fee_minor` by `delta` — **negative** for a
    /// settlement return that reverses the proportional fee slice (Model N, D1)
    /// — bumping `version`. Mirrors [`Self::add_settled`] exactly: a scoped
    /// UPDATE (not an upsert; the settlement row always pre-exists, so
    /// `rows_affected == 0` ⇒ not settled), evaluating the CHECK against the
    /// resulting row. The `fee_minor >= 0` (`chk_payment_settlement_nonneg`) and
    /// `fee_minor <= settled_minor` (`chk_payment_settlement_fee_le_settled`)
    /// CHECKs are the backstop — a violation surfaces as
    /// [`RepoError::MoneyOutCapExceeded`] (the same mapping `add_settled` uses
    /// for its cap CHECK; the return sidecar maps both to
    /// [`crate::domain::error::DomainError::SettlementReturnOverAllocated`]). SSI
    /// + retry serialize concurrent writers of the same payment.
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when a CHECK rejects the change;
    /// [`RepoError::Db`] when no row matched or on any other scope / storage
    /// failure.
    pub async fn add_fee(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        // Scoped UPDATE (not an upsert), exactly like `add_settled`: the CHECK
        // evaluates against the resulting row (`fee_minor + delta >= 0` and
        // `<= settled_minor`), so an over-reverse surfaces as the CHECK
        // violation, mapped to `MoneyOutCapExceeded`; the return sidecar refines
        // it to `SettlementReturnOverAllocated`.
        let result = payment_settlement::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                payment_settlement::Column::FeeMinor,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::FeeMinor,
                ))
                .add(delta),
            )
            .col_expr(
                payment_settlement::Column::Version,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("adjust fee_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "payment_settlement row absent for ({tenant}, {payment_id}) — not settled"
            )));
        }
        Ok(())
    }

    /// Increment `payment_settlement.clawed_back_minor` by `delta` for a
    /// chargeback `lost`/cash-out, bumping `version`. The total money-out cap
    /// CHECK (`refunded_minor + clawed_back_minor <= settled_minor`,
    /// `chk_payment_settlement_moneyout_le_settled`) enforces that a settlement is
    /// never paid out twice (refund + clawback); a violation maps to
    /// [`RepoError::MoneyOutCapExceeded`] (the chargeback sidecar refines it to
    /// [`crate::domain::error::DomainError::ChargebackExceedsSettled`]). A scoped
    /// UPDATE, not an upsert: the settlement row always pre-exists (a chargeback
    /// references a settled payment); `rows_affected == 0` ⇒ not settled. SSI +
    /// retry serialize concurrent writers of the same payment. Mirrors
    /// [`Self::add_settled`].
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when the cap CHECK rejects the
    /// increment; [`RepoError::Db`] when no row matched or on any other scope /
    /// storage failure.
    pub async fn add_clawed_back(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        // Scoped UPDATE (not an upsert), exactly like `add_allocated` /
        // `add_settled`: the CHECK evaluates against the resulting row
        // (`refunded_minor + clawed_back_minor + delta <= settled_minor`), so an
        // over-claw surfaces as the CHECK violation, mapped to
        // `MoneyOutCapExceeded`; the chargeback sidecar refines it to
        // `ChargebackExceedsSettled`.
        let result = payment_settlement::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                payment_settlement::Column::ClawedBackMinor,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::ClawedBackMinor,
                ))
                .add(delta),
            )
            .col_expr(
                payment_settlement::Column::Version,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("add clawed_back_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "payment_settlement row absent for ({tenant}, {payment_id}) — not settled"
            )));
        }
        Ok(())
    }

    /// Increment `payment_settlement.refunded_minor` by `delta` for a refund
    /// stage-1 initiation, bumping `version`. This is the TOTAL money-out counter
    /// (both refund patterns bump it): the
    /// `chk_payment_settlement_moneyout_le_settled` CHECK
    /// (`refunded_minor + clawed_back_minor <= settled_minor`) — plus the
    /// `chk_payment_settlement_refunded_le_settled` CHECK (`refunded_minor <=
    /// settled_minor`) — enforces that a settlement is never paid out twice
    /// (refund + clawback); a violation maps to [`RepoError::MoneyOutCapExceeded`]
    /// (the refund sidecar refines it to
    /// [`crate::domain::error::DomainError::RefundExceedsSettled`]). A scoped
    /// UPDATE, not an upsert: the settlement row always pre-exists (a refund
    /// unwinds a settled receipt, resolved out-of-txn before the post);
    /// `rows_affected == 0` ⇒ not settled. SSI + retry serialize concurrent
    /// writers of the same payment. Mirrors [`Self::add_clawed_back`].
    ///
    /// The Group-C stage-1 **reversal** (PSP `rejected`/`voided`) calls this with
    /// a NEGATIVE `delta` to release the cap reserved at initiation. A decrement
    /// never trips a cap CHECK (it lowers the LHS); it could only trip the nonneg
    /// CHECK (`refunded_minor >= 0`), which is impossible here — the reversal
    /// decrements EXACTLY the amount the matching stage-1 initiation incremented,
    /// so the counter cannot underflow (the full out-of-order refund-of-refund
    /// underflow handling is Group E, not here).
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when a cap / nonneg CHECK rejects the
    /// change; [`RepoError::Db`] when no row matched or on any other scope /
    /// storage failure.
    pub async fn add_refunded(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        // Scoped UPDATE (not an upsert), exactly like `add_clawed_back`: the CHECK
        // evaluates against the resulting row
        // (`refunded_minor + clawed_back_minor + delta <= settled_minor` and
        // `refunded_minor + delta <= settled_minor`), so an over-refund surfaces as
        // the CHECK violation, mapped to `MoneyOutCapExceeded`; the refund sidecar
        // refines it to `RefundExceedsSettled`.
        let result = payment_settlement::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                payment_settlement::Column::RefundedMinor,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::RefundedMinor,
                ))
                .add(delta),
            )
            .col_expr(
                payment_settlement::Column::Version,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("add refunded_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "payment_settlement row absent for ({tenant}, {payment_id}) — not settled"
            )));
        }
        Ok(())
    }

    /// Increment `payment_settlement.refunded_unallocated_minor` by `delta` for a
    /// **Pattern A** (`A_UNALLOCATED`) refund stage-1 initiation, bumping
    /// `version`. This is the *spendable-headroom* counter: the
    /// `chk_payment_settlement_alloc_refu_le_settled` CHECK
    /// (`allocated_minor + refunded_unallocated_minor <= settled_minor`) enforces
    /// that refunded on-account cash can no longer ALSO be allocated to an invoice
    /// (the receipt left the spendable pool). A violation maps to
    /// [`RepoError::MoneyOutCapExceeded`] (the refund sidecar refines it to
    /// [`crate::domain::error::DomainError::RefundExceedsSettled`]). Only Pattern A
    /// touches this counter — a Pattern B refund restores AR (it never drew the
    /// unallocated pool), so it bumps only `refunded_minor` + the per-`(payment,
    /// invoice)` `payment_allocation_refund.refunded_minor`. A scoped UPDATE, not
    /// an upsert (the row pre-exists); `rows_affected == 0` ⇒ not settled. SSI +
    /// retry serialize concurrent writers. Mirrors [`Self::add_refunded`].
    ///
    /// The stage-1 reversal calls this with a NEGATIVE `delta` to release the
    /// headroom reserved at initiation; a decrement never trips the cap CHECK and
    /// cannot underflow the nonneg CHECK (it decrements exactly what initiation
    /// added — Group E owns the out-of-order case).
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when a cap / nonneg CHECK rejects the
    /// change; [`RepoError::Db`] when no row matched or on any other scope /
    /// storage failure.
    pub async fn add_refunded_unallocated(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        // Scoped UPDATE (not an upsert), exactly like `add_allocated`: the CHECK
        // evaluates against the resulting row
        // (`allocated_minor + refunded_unallocated_minor + delta <= settled_minor`),
        // so refunding cash that would no longer fit the spendable pool surfaces as
        // the CHECK violation, mapped to `MoneyOutCapExceeded`; the refund sidecar
        // refines it to `RefundExceedsSettled`.
        let result = payment_settlement::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                payment_settlement::Column::RefundedUnallocatedMinor,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::RefundedUnallocatedMinor,
                ))
                .add(delta),
            )
            .col_expr(
                payment_settlement::Column::Version,
                Expr::col((
                    payment_settlement::Entity,
                    payment_settlement::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("add refunded_unallocated_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "payment_settlement row absent for ({tenant}, {payment_id}) — not settled"
            )));
        }
        Ok(())
    }

    /// Increment `payment_allocation_refund.refunded_minor` for a `(payment,
    /// invoice)` by `delta` — the **Pattern B** (`B_RESTORE_AR`) per-invoice refund
    /// cap, bumping `version`. The `chk_par_refunded_le_allocated` CHECK
    /// (`refunded_minor <= allocated_minor`) enforces that a `(payment, invoice)`
    /// pair is never refunded for more than was allocated to it; a violation maps
    /// to [`RepoError::MoneyOutCapExceeded`] (the refund sidecar refines it to
    /// [`crate::domain::error::DomainError::RefundExceedsAllocated`]).
    ///
    /// A scoped UPDATE, not an upsert (contrast [`Self::bump_allocation_refund`],
    /// which seeds `allocated_minor` at allocation time): the `payment_allocation_refund`
    /// row is seeded by the allocation that applied this payment to the invoice, so
    /// it always pre-exists when a Pattern-B refund of that same `(payment,
    /// invoice)` runs. An `INSERT … ON CONFLICT` would trip `refunded_minor <=
    /// allocated_minor` on the INSERT VALUES tuple (`allocated_minor = 0`) during
    /// arbitration before the DO UPDATE can net against the real allocated amount —
    /// the `add_allocated` rationale. `rows_affected == 0` ⇒ the `(payment,
    /// invoice)` was never allocated (a Pattern-B refund of an unallocated
    /// receipt — an upstream contract violation); surfaced as [`RepoError::Db`].
    /// SSI + retry serialize concurrent writers.
    ///
    /// The stage-1 reversal calls this with a NEGATIVE `delta` to release the
    /// per-invoice reservation; a decrement never trips the cap CHECK and cannot
    /// underflow the nonneg CHECK (it decrements exactly the initiation amount).
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when the per-invoice cap CHECK rejects
    /// the increment; [`RepoError::Db`] when no row matched or on any other scope /
    /// storage failure.
    pub async fn add_allocation_refund_refunded(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        invoice_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        // Scoped UPDATE (not an upsert): the row is seeded with the real
        // `allocated_minor` by `bump_allocation_refund` at allocation time, so the
        // CHECK evaluates against the resulting row
        // (`refunded_minor + delta <= allocated_minor`). An over-refund of the
        // pair surfaces as the CHECK violation, mapped to `MoneyOutCapExceeded`;
        // the refund sidecar refines it to `RefundExceedsAllocated`.
        let result = payment_allocation_refund::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                payment_allocation_refund::Column::RefundedMinor,
                Expr::col((
                    payment_allocation_refund::Entity,
                    payment_allocation_refund::Column::RefundedMinor,
                ))
                .add(delta),
            )
            .col_expr(
                payment_allocation_refund::Column::Version,
                Expr::col((
                    payment_allocation_refund::Entity,
                    payment_allocation_refund::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(payment_allocation_refund::Column::TenantId.eq(tenant))
                    .add(payment_allocation_refund::Column::PaymentId.eq(payment_id))
                    .add(payment_allocation_refund::Column::InvoiceId.eq(invoice_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("add allocation_refund refunded_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "payment_allocation_refund row absent for ({tenant}, {payment_id}, {invoice_id}) \
                 — payment was never allocated to this invoice"
            )));
        }
        Ok(())
    }

    /// Insert the N `payment_allocation` rows for one allocation. The PK
    /// `(tenant, allocation_id, invoice_id)` makes a replay of the same
    /// `allocation_id` collide — but a replay returns before the sidecar, so
    /// this is only reached on the first post; an unexpected duplicate
    /// surfaces as [`RepoError::Db`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn insert_allocation_rows(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        rows: &[NewAllocationRow],
    ) -> Result<(), RepoError> {
        for row in rows {
            let am = payment_allocation::ActiveModel {
                tenant_id: Set(row.tenant_id),
                allocation_id: Set(row.allocation_id),
                invoice_id: Set(row.invoice_id.clone()),
                payer_tenant_id: Set(row.payer_tenant_id),
                payment_id: Set(row.payment_id.clone()),
                amount_minor: Set(row.amount_minor),
                currency: Set(row.currency.clone()),
                precedence_policy_ref: Set(row.precedence_policy_ref.clone()),
                allocated_at_utc: Set(row.allocated_at_utc),
            };
            payment_allocation::Entity::insert(am.clone())
                .secure()
                .scope_with_model(scope, &am)
                .map_err(|e| RepoError::Db(format!("payment_allocation scope: {e}")))?
                .exec(txn)
                .await
                .map_err(|e| RepoError::Db(format!("insert payment_allocation: {e}")))?;
        }
        Ok(())
    }

    /// Increment `payment_allocation_refund.allocated_minor` for a
    /// `(payment, invoice)` by `delta` (the per-invoice amount this allocation
    /// applied), bumping `version`. Feeds Slice 3's refund cap (the
    /// `refunded_minor <= allocated_minor` CHECK); a CHECK violation maps to
    /// [`RepoError::MoneyOutCapExceeded`].
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when the cap CHECK rejects the
    /// increment; [`RepoError::Db`] on any other scope / storage failure.
    pub async fn bump_allocation_refund(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        invoice_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        let am = payment_allocation_refund::ActiveModel {
            tenant_id: Set(tenant),
            payment_id: Set(payment_id.to_owned()),
            invoice_id: Set(invoice_id.to_owned()),
            allocated_minor: Set(delta),
            refunded_minor: Set(0),
            version: Set(0),
        };
        let on_conflict = SecureOnConflict::<payment_allocation_refund::Entity>::columns([
            payment_allocation_refund::Column::TenantId,
            payment_allocation_refund::Column::PaymentId,
            payment_allocation_refund::Column::InvoiceId,
        ])
        .value(
            payment_allocation_refund::Column::AllocatedMinor,
            Expr::col((
                payment_allocation_refund::Entity,
                payment_allocation_refund::Column::AllocatedMinor,
            ))
            .add(delta),
        )
        .and_then(|oc| {
            oc.value(
                payment_allocation_refund::Column::Version,
                Expr::col((
                    payment_allocation_refund::Entity,
                    payment_allocation_refund::Column::Version,
                ))
                .add(1),
            )
        })
        .map_err(|e| RepoError::Db(format!("payment_allocation_refund on_conflict: {e}")))?;

        payment_allocation_refund::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("payment_allocation_refund scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| map_cap_violation("bump allocation_refund", &e))?;
        Ok(())
    }

    // --- Out-of-txn reads (PDP In-scoped; SQL-level BOLA) ---

    /// List the open AR invoices for `(payer, currency)` — the allocation
    /// candidate set — ordered oldest-first (`original_posted_at` then
    /// `invoice_id`). Filters `balance_minor > 0`. SQL-level BOLA: a foreign
    /// tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_open_ar_invoices(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer: Uuid,
        currency: &str,
    ) -> Result<Vec<OpenArInvoice>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = ar_invoice_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(ar_invoice_balance::Column::TenantId.eq(tenant))
                    .add(ar_invoice_balance::Column::PayerTenantId.eq(payer))
                    .add(ar_invoice_balance::Column::Currency.eq(currency))
                    .add(ar_invoice_balance::Column::BalanceMinor.gt(0)),
            )
            .order_by(ar_invoice_balance::Column::OriginalPostedAt, Order::Asc)
            .order_by(ar_invoice_balance::Column::InvoiceId, Order::Asc)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list open ar invoices: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|m| OpenArInvoice {
                invoice_id: m.invoice_id,
                balance_minor: m.balance_minor,
                original_posted_at: m.original_posted_at,
                currency: m.currency,
                functional_balance_minor: m.functional_balance_minor,
            })
            .collect())
    }

    /// List the payer's spendable reusable-credit sub-grains for `(payer, currency)`
    /// — the wallet draw-down candidate set — ordered oldest-grant-first
    /// (`first_granted_at` then `credit_grant_event_type`). Filters `balance_minor > 0`.
    /// SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_credit_subgrains(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer: Uuid,
        currency: &str,
    ) -> Result<Vec<CreditSubgrain>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = reusable_credit_subbalance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(reusable_credit_subbalance::Column::TenantId.eq(tenant))
                    .add(reusable_credit_subbalance::Column::PayerTenantId.eq(payer))
                    .add(reusable_credit_subbalance::Column::Currency.eq(currency))
                    .add(reusable_credit_subbalance::Column::BalanceMinor.gt(0)),
            )
            .order_by(
                reusable_credit_subbalance::Column::FirstGrantedAt,
                Order::Asc,
            )
            .order_by(
                reusable_credit_subbalance::Column::CreditGrantEventType,
                Order::Asc,
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list credit subgrains: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|m| CreditSubgrain {
                credit_grant_event_type: m.credit_grant_event_type,
                available_minor: m.balance_minor,
            })
            .collect())
    }

    /// List the `payment_allocation` rows for `(tenant, payment_id)` (the
    /// `GET …/allocations` view), ordered by `invoice_id`. SQL-level BOLA.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_payment_allocations(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
    ) -> Result<Vec<payment_allocation::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = payment_allocation::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(payment_allocation::Column::TenantId.eq(tenant))
                    .add(payment_allocation::Column::PaymentId.eq(payment_id)),
            )
            .order_by(payment_allocation::Column::InvoiceId, Order::Asc)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list payment_allocation: {e}")))?;
        Ok(rows)
    }

    /// Read the payer's unallocated pool balance for `(payer, currency)`. The
    /// grain is single-row per `(tenant, payer, account, currency)`; 2a posts
    /// one UNALLOCATED account per currency, so this sums the matching rows
    /// (zero or one in practice) and returns the total, or 0 when empty.
    /// SQL-level BOLA.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_unallocated(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer: Uuid,
        currency: &str,
    ) -> Result<i64, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = unallocated_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(unallocated_balance::Column::TenantId.eq(tenant))
                    .add(unallocated_balance::Column::PayerTenantId.eq(payer))
                    .add(unallocated_balance::Column::Currency.eq(currency)),
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read unallocated: {e}")))?;
        Ok(rows.iter().map(|r| r.balance_minor).sum())
    }

    /// Read the payer's unallocated pool with its carried functional balance for
    /// `(payer, currency)` — the realized-FX poster's read at allocation close
    /// (Slice 5, design §3.5). Like [`Self::read_unallocated`] this sums the
    /// matching grain rows (zero or one in practice — 2a posts one UNALLOCATED
    /// account per currency); the functional column is `Some` only when the pool
    /// is cross-currency (S2 settle stamped it), and `functional_currency` is the
    /// first non-NULL grain currency (one functional currency per seller). A
    /// `None` functional balance ⇒ a single-currency close: no realized FX.
    /// SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_unallocated_carried(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer: Uuid,
        currency: &str,
    ) -> Result<UnallocatedCarried, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = unallocated_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(unallocated_balance::Column::TenantId.eq(tenant))
                    .add(unallocated_balance::Column::PayerTenantId.eq(payer))
                    .add(unallocated_balance::Column::Currency.eq(currency)),
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read unallocated carried: {e}")))?;
        let balance_minor = rows.iter().map(|r| r.balance_minor).sum();
        // Functional is NULL on a single-currency pool: keep it NULL unless at
        // least one grain row carries it (then sum the populated values — one row
        // in practice). Mirrors the projector's no-COALESCE NULL discipline.
        let functional_balance_minor = if rows.iter().any(|r| r.functional_balance_minor.is_some())
        {
            Some(rows.iter().filter_map(|r| r.functional_balance_minor).sum())
        } else {
            None
        };
        let functional_currency = rows.iter().find_map(|r| r.functional_currency.clone());
        Ok(UnallocatedCarried {
            balance_minor,
            functional_balance_minor,
            functional_currency,
        })
    }

    /// Read a general `account_balance` grain's carried `(transaction, functional)`
    /// value for `(tenant, account_id, currency)` — the chargeback functional
    /// carry-forward's read for a `CASH_HOLD` dispute (the `CASH_CLEARING` grain it
    /// closes at `opened`, the `DISPUTE_HOLD` grain it closes at `won`/`lost`).
    /// Returns a zero / functional-`None` [`CarriedBalance`] when no row exists.
    /// SQL-level BOLA: a foreign tenant yields no row.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_account_carried(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        account_id: Uuid,
        currency: &str,
    ) -> Result<CarriedBalance, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = account_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(account_balance::Column::TenantId.eq(tenant))
                    .add(account_balance::Column::AccountId.eq(account_id))
                    .add(account_balance::Column::Currency.eq(currency)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read account balance carried: {e}")))?;
        Ok(row.map_or(
            CarriedBalance {
                balance_minor: 0,
                functional_balance_minor: None,
                functional_currency: None,
            },
            |m| CarriedBalance {
                balance_minor: m.balance_minor,
                functional_balance_minor: m.functional_balance_minor,
                functional_currency: m.functional_currency,
            },
        ))
    }

    /// Read one AR invoice grain's carried `(transaction, functional)` value for
    /// `(tenant, payer, invoice_id, currency)` — the chargeback functional
    /// carry-forward's read for an `AR_RECLASS` dispute (the disputed receivable it
    /// reclasses / writes off). Targeted single-invoice read (unlike
    /// [`Self::list_open_ar_invoices`], which lists all open candidates and filters
    /// `balance_minor > 0`). Returns a zero / functional-`None` [`CarriedBalance`]
    /// when no row exists. SQL-level BOLA.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_ar_invoice_carried(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer: Uuid,
        invoice_id: &str,
        currency: &str,
    ) -> Result<CarriedBalance, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = ar_invoice_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(ar_invoice_balance::Column::TenantId.eq(tenant))
                    .add(ar_invoice_balance::Column::PayerTenantId.eq(payer))
                    .add(ar_invoice_balance::Column::InvoiceId.eq(invoice_id))
                    .add(ar_invoice_balance::Column::Currency.eq(currency)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read ar invoice carried: {e}")))?;
        Ok(row.map_or(
            CarriedBalance {
                balance_minor: 0,
                functional_balance_minor: None,
                functional_currency: None,
            },
            |m| CarriedBalance {
                balance_minor: m.balance_minor,
                functional_balance_minor: m.functional_balance_minor,
                functional_currency: m.functional_currency,
            },
        ))
    }

    /// List the open, cross-currency AR-invoice grains for `tenant` to remeasure
    /// at period end (Slice 5 Phase 3): `ar_invoice_balance` rows with
    /// `balance_minor > 0` and a non-NULL `functional_currency` (cross-currency,
    /// decision 8). A row whose `functional_currency` is set but
    /// `functional_balance_minor` is NULL (an impossible projector state) is
    /// skipped. SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_ar_invoices_to_revalue(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Vec<RevaluationGrain>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = ar_invoice_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(ar_invoice_balance::Column::TenantId.eq(tenant))
                    .add(ar_invoice_balance::Column::FunctionalCurrency.is_not_null())
                    .add(ar_invoice_balance::Column::BalanceMinor.gt(0)),
            )
            .order_by(ar_invoice_balance::Column::InvoiceId, Order::Asc)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list ar invoices to revalue: {e}")))?;
        Ok(rows
            .into_iter()
            .filter_map(|m| {
                Some(RevaluationGrain {
                    payer_tenant_id: m.payer_tenant_id,
                    account_id: m.account_id,
                    currency: m.currency,
                    invoice_id: Some(m.invoice_id),
                    credit_grant_event_type: None,
                    balance_minor: m.balance_minor,
                    functional_balance_minor: m.functional_balance_minor?,
                    functional_currency: m.functional_currency?,
                })
            })
            .collect())
    }

    /// List the open, cross-currency unallocated-pool grains for `tenant` to
    /// remeasure at period end (Slice 5 Phase 3): `unallocated_balance` rows with
    /// `balance_minor > 0` and a non-NULL `functional_currency`. SQL-level BOLA.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_unallocated_to_revalue(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Vec<RevaluationGrain>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = unallocated_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(unallocated_balance::Column::TenantId.eq(tenant))
                    .add(unallocated_balance::Column::FunctionalCurrency.is_not_null())
                    .add(unallocated_balance::Column::BalanceMinor.gt(0)),
            )
            .order_by(unallocated_balance::Column::PayerTenantId, Order::Asc)
            .order_by(unallocated_balance::Column::Currency, Order::Asc)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list unallocated to revalue: {e}")))?;
        Ok(rows
            .into_iter()
            .filter_map(|m| {
                Some(RevaluationGrain {
                    payer_tenant_id: m.payer_tenant_id,
                    account_id: m.account_id,
                    currency: m.currency,
                    invoice_id: None,
                    credit_grant_event_type: None,
                    balance_minor: m.balance_minor,
                    functional_balance_minor: m.functional_balance_minor?,
                    functional_currency: m.functional_currency?,
                })
            })
            .collect())
    }

    /// List the open, cross-currency reusable-credit (wallet) grains for `tenant`
    /// to remeasure at period end (Slice 5 Phase 3): `reusable_credit_subbalance`
    /// rows with `balance_minor > 0` and a non-NULL `functional_currency`. Each
    /// `credit_grant_event_type` sub-bucket is its own grain. SQL-level BOLA.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_reusable_credit_to_revalue(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Vec<RevaluationGrain>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = reusable_credit_subbalance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(reusable_credit_subbalance::Column::TenantId.eq(tenant))
                    .add(reusable_credit_subbalance::Column::FunctionalCurrency.is_not_null())
                    .add(reusable_credit_subbalance::Column::BalanceMinor.gt(0)),
            )
            .order_by(
                reusable_credit_subbalance::Column::PayerTenantId,
                Order::Asc,
            )
            .order_by(reusable_credit_subbalance::Column::Currency, Order::Asc)
            .order_by(
                reusable_credit_subbalance::Column::CreditGrantEventType,
                Order::Asc,
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list reusable credit to revalue: {e}")))?;
        Ok(rows
            .into_iter()
            .filter_map(|m| {
                Some(RevaluationGrain {
                    payer_tenant_id: m.payer_tenant_id,
                    account_id: m.account_id,
                    currency: m.currency,
                    invoice_id: None,
                    credit_grant_event_type: Some(m.credit_grant_event_type),
                    balance_minor: m.balance_minor,
                    functional_balance_minor: m.functional_balance_minor?,
                    functional_currency: m.functional_currency?,
                })
            })
            .collect())
    }

    /// Read the `payment_settlement` row for `(tenant, payment_id)` (counters +
    /// currency), or `None` if the payment was never settled. SQL-level BOLA.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_settlement(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
    ) -> Result<Option<payment_settlement::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = payment_settlement::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read payment_settlement: {e}")))?;
        Ok(row)
    }

    /// Read the `payment_settlement` row for `(tenant, payment_id)` UNDER the
    /// rank-1 row lock, INSIDE the passed-in posting transaction — the claw-back
    /// underflow pre-check (Group E, design §4.4). A refund-of-refund claw-back
    /// DECREMENTS `refunded_minor`; if the decrement would drive the counter below
    /// zero (a PSP claw-back that arrived BEFORE / without the matching outbound
    /// refund stage-1, or claws back MORE than was refunded) the design DEFERS it —
    /// it must NOT be applied and must NOT hard-abort on the `refunded_minor >= 0`
    /// CHECK. So the handler reads the current counters HERE under the same rank-1
    /// `payment_settlement` lock the decrement takes, decides `current - amount < 0`,
    /// and either decrements (in-order / sufficient) or defers (would underflow) —
    /// the CHECK stays a defense-in-depth backstop that must never fire.
    ///
    /// Locked with `FOR UPDATE` on Postgres (serializes against a concurrent
    /// outbound stage-1 that is raising `refunded_minor`, so the read-then-decrement
    /// is atomic); `SQLite` has no `FOR UPDATE` and omits the clause (the unit/test
    /// path uses `SERIALIZABLE` semantics already). Takes `&self` to reach the
    /// provider for the backend probe (mirrors [`PendingQueueRepo::claim_due`]).
    /// Returns `None` when the payment was never settled — an upstream contract
    /// violation for a claw-back (nothing to claw back), surfaced by the caller.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_settlement_for_update(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
    ) -> Result<Option<payment_settlement::Model>, RepoError> {
        use sea_orm::QuerySelect as _;
        use sea_orm::sea_query::LockType;
        // Apply the row lock on the raw `find()` before SecureORM wraps it (the lock
        // rides the underlying SelectStatement, surviving `.secure().scope_with`) —
        // the same shape `PendingQueueRepo::claim_due` uses. Plain `FOR UPDATE` (NOT
        // SKIP LOCKED): the pre-check must BLOCK on a concurrent writer of this row,
        // not skip it, so it reads the post-write counter and serializes the
        // read-then-decrement against a concurrent outbound stage-1.
        let mut find = payment_settlement::Entity::find();
        if self.db.db().backend() == sea_orm::DatabaseBackend::Postgres {
            find = find.lock(LockType::Update);
        }
        let row = find
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(payment_settlement::Column::TenantId.eq(tenant))
                    .add(payment_settlement::Column::PaymentId.eq(payment_id)),
            )
            .one(txn)
            .await
            .map_err(|e| RepoError::Db(format!("read payment_settlement for update: {e}")))?;
        Ok(row)
    }

    /// Read the `payment_allocation_refund.refunded_minor` for a `(payment, invoice)`
    /// UNDER the row lock, INSIDE the posting transaction — the Pattern-B leg of the
    /// claw-back underflow pre-check (Group E). A Pattern-B claw-back DECREMENTS this
    /// per-invoice counter; the handler reads it here under `FOR UPDATE` to decide
    /// whether `current - amount < 0` (and defer instead of underflowing the
    /// `chk_par_refunded_le_allocated` / nonneg CHECK). Returns `0` when no
    /// `payment_allocation_refund` row exists — which a Pattern-B claw-back of an
    /// unallocated `(payment, invoice)` would be, so `0 < amount` defers it (the same
    /// out-of-order treatment). Locked with `FOR UPDATE` on Postgres (blocks on a
    /// concurrent outbound Pattern-B refund raising the counter); omitted on
    /// `SQLite`. Takes `&self` for the backend probe (mirrors
    /// [`Self::read_settlement_for_update`]).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_allocation_refund_refunded_for_update(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        payment_id: &str,
        invoice_id: &str,
    ) -> Result<i64, RepoError> {
        use sea_orm::QuerySelect as _;
        use sea_orm::sea_query::LockType;
        let mut find = payment_allocation_refund::Entity::find();
        if self.db.db().backend() == sea_orm::DatabaseBackend::Postgres {
            find = find.lock(LockType::Update);
        }
        let row = find
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(payment_allocation_refund::Column::TenantId.eq(tenant))
                    .add(payment_allocation_refund::Column::PaymentId.eq(payment_id))
                    .add(payment_allocation_refund::Column::InvoiceId.eq(invoice_id)),
            )
            .one(txn)
            .await
            .map_err(|e| {
                RepoError::Db(format!("read payment_allocation_refund for update: {e}"))
            })?;
        Ok(row.map_or(0, |r| r.refunded_minor))
    }

    /// Look up a FINALIZED idempotent post by its `(tenant, source_doc_type,
    /// business_id)` key, returning the prior entry id + stored request
    /// `payload_hash` when the key already posted (dedup status `POSTED`), else
    /// `None`. An orchestrator whose
    /// pre-post validation depends on mutable ledger state (the credit-application
    /// caps re-read open AR / the wallet) calls this FIRST, so a
    /// retry-after-success returns the prior posting as a replay instead of
    /// re-validating against the now-drained state and spuriously rejecting. The
    /// authoritative dedup is still the engine's in-txn claim inside `post` (this
    /// out-of-txn read is racy by nature — a `None` only means "proceed and let
    /// `post` claim", which still guards a concurrent first post). SQL-level BOLA
    /// via the scope.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn lookup_finalized_post(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        source_doc_type: SourceDocType,
        business_id: &str,
    ) -> Result<Option<(Uuid, String)>, RepoError> {
        // Only a finalized (POSTED) row carries an authoritative prior entry id; a
        // still-CLAIMED row is an in-flight concurrent post (and a QUEUED row is a
        // pending deferred apply) — fall through and let the engine's in-txn claim
        // serialize against it. The prior entry id is paired with the stored
        // request `payload_hash` so the caller's replay short-circuit can reject a
        // same-key / different-payload reuse instead of replaying it.
        Ok(self
            .lookup_dedup_status(scope, tenant, source_doc_type, business_id)
            .await?
            .and_then(|(status, entry_id, payload_hash)| {
                if status == STATUS_POSTED {
                    entry_id.map(|id| (id, payload_hash))
                } else {
                    None
                }
            }))
    }

    /// Read the dedup row for `(tenant, source_doc_type, business_id)`,
    /// returning its `(status, result_entry_id, payload_hash)` (or `None` when no
    /// row exists). The generalized form behind [`Self::lookup_finalized_post`]:
    /// callers that need to distinguish `CLAIMED` (in-flight inline) vs
    /// `QUEUED` (pending deferred apply) vs `POSTED` (finalized) on a known key
    /// read the raw status here, rather than collapsing everything-but-POSTED
    /// to "proceed". Like `lookup_finalized_post`, this out-of-txn read is racy
    /// by nature — the authoritative dedup is the engine's in-txn claim. SQL-level
    /// BOLA via the scope.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn lookup_dedup_status(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        source_doc_type: SourceDocType,
        business_id: &str,
    ) -> Result<Option<(String, Option<Uuid>, String)>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = idempotency_dedup::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(idempotency_dedup::Column::TenantId.eq(tenant))
                    .add(idempotency_dedup::Column::Flow.eq(source_doc_type.as_str()))
                    .add(idempotency_dedup::Column::BusinessId.eq(business_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("lookup dedup status: {e}")))?;
        Ok(row.map(|r| (r.status, r.result_entry_id, r.payload_hash)))
    }

    /// Read the precedence policy in effect for `tenant` at instant `at` — the
    /// row with the latest `effective_from <= at` (ties broken by the highest
    /// `version`), mapped to its [`PrecedenceStrategy`] and `version`. Returns
    /// `None` when the tenant has no policy effective at `at` (the caller falls
    /// back to oldest-first). SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure, or when a stored
    /// `strategy` is not a known policy id (an invariant breach — the column is
    /// only ever written from [`PrecedenceStrategy::policy_ref`]).
    pub async fn read_effective_policy(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        at: OffsetDateTime,
    ) -> Result<Option<(PrecedenceStrategy, i64)>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = tenant_precedence_policy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(tenant_precedence_policy::Column::TenantId.eq(tenant))
                    .add(tenant_precedence_policy::Column::EffectiveFrom.lte(at)),
            )
            .order_by(tenant_precedence_policy::Column::EffectiveFrom, Order::Desc)
            .order_by(tenant_precedence_policy::Column::Version, Order::Desc)
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read precedence policy: {e}")))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let strategy = PrecedenceStrategy::parse(&row.strategy).ok_or_else(|| {
            RepoError::Db(format!(
                "unknown stored precedence strategy {:?} for tenant {tenant} version {}",
                row.strategy, row.version
            ))
        })?;
        Ok(Some((strategy, row.version)))
    }
}

/// Map a counter-write [`ScopeError`] to [`RepoError`]: a CHECK-constraint
/// violation (the per-payment cap) becomes [`RepoError::MoneyOutCapExceeded`];
/// anything else stays a plain [`RepoError::Db`]. Only `ScopeError::Db` can
/// carry a driver CHECK error; the scope-validation variants never do.
fn map_cap_violation(context: &str, err: &ScopeError) -> RepoError {
    if let ScopeError::Db(db_err) = err
        && is_check_violation(db_err)
    {
        return RepoError::MoneyOutCapExceeded(format!("{context}: {err}"));
    }
    RepoError::Db(format!("{context}: {err}"))
}

/// Returns `true` iff `err` is a `CHECK`-constraint violation on either
/// supported backend. `sea_orm::SqlErr` has no `Check` discriminant, so a real
/// CHECK violation always surfaces unstructured (`sql_err() == None`); a
/// structured error (unique / FK) is therefore never a CHECK and is refused
/// here. The keyword / SQLSTATE-anchored fallbacks mirror the RBAC gear's
/// `is_check_violation` (Postgres `23514`, `SQLite` extended code `275`).
fn is_check_violation(err: &DbErr) -> bool {
    if err.sql_err().is_some() {
        return false;
    }
    let msg = err.to_string().to_lowercase();
    // The constraint NAME is the most stable signal — it survives the driver /
    // locale changes that can reshape the SQLSTATE text the keyword fallbacks
    // below anchor on. The cap CHECKs (`chk_payment_settlement_*` on the
    // settlement counters, `chk_par_*` on the allocation-refund counters) are
    // the only constraints whose violation must map to `MoneyOutCapExceeded`,
    // so match them by name first and keep the SQLSTATE anchors as a fallback.
    if msg.contains("chk_payment_settlement_") || msg.contains("chk_par_") {
        return true;
    }
    msg.contains("check constraint")
        || msg.contains("check_violation")
        || msg.contains("sqlite_constraint_check")
        || msg.contains("sqlstate 23514")
        || msg.contains("sqlstate: 23514")
        || msg.contains("sqlstate=23514")
        || msg.contains("code 23514")
        || msg.contains("code: 23514")
        || msg.contains("(23514)")
        || msg.contains("(23514:")
        || msg.starts_with("23514:")
        || msg.contains(" 23514:")
        || (msg.contains("sqlite")
            && (msg.contains("code 275")
                || msg.contains("code: 275")
                || msg.contains("(275)")
                || msg.contains("(275:")))
}
