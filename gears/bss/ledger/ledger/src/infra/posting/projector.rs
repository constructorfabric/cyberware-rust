//! `BalanceProjector` — derives per-grain signed deltas from the posted
//! lines and upserts the derived balance caches inside the posting
//! transaction, in a fixed lock-key order (deadlock-freedom), re-asserting
//! the no-negative invariant on the guarded account classes (the DB
//! conditional CHECK from P1 is the backstop).
//!
//! Grains:
//! - `account_balance` `(tenant, account, currency)` — every line;
//! - `ar_payer_balance` `(tenant, payer, account, currency)` — `AR` lines;
//! - `ar_invoice_balance` `(tenant, payer, account, invoice)` — `AR` lines
//!   carrying an `invoice_id`;
//! - `reusable_credit_subbalance`
//!   `(tenant, payer, account, currency, credit_grant_event_type)` —
//!   `REUSABLE_CREDIT` lines (the wallet sub-grain);
//! - `tax_subbalance` `(tenant, account, jurisdiction, filing)` —
//!   `TAX_PAYABLE` lines carrying both tax dims.
//!
//! A line's signed delta is `+amount` when its side equals the account's
//! normal side, else `-amount`.

use std::collections::HashMap;

use bss_ledger_sdk::{AccountClass, Side};
use chrono::NaiveDate;
use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use uuid::Uuid;

use crate::domain::model::{NewEntry, NewLine};
use crate::domain::status::AR_STATUS_DISPUTED;
use crate::infra::storage::entity::{
    account_balance, ar_invoice_balance, ar_payer_balance, reusable_credit_subbalance,
    tax_subbalance, unallocated_balance,
};
use time::OffsetDateTime;

/// Projection error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectError {
    /// A guarded balance would go negative after applying a delta.
    #[error("balance for account {account_id} would go negative ({balance_minor})")]
    NegativeBalance {
        account_id: Uuid,
        balance_minor: i64,
    },
    /// An account's `normal_side` was not supplied in the lookup map.
    #[error("missing normal_side for account {0}")]
    MissingNormalSide(Uuid),
    /// A `REUSABLE_CREDIT` line reached projection without its wallet sub-grain
    /// bucket (`credit_grant_event_type`). The domain builders always set it, so
    /// this is an invariant breach — projecting it would key a phantom "" sub-
    /// balance, which the DB NOT-NULL CHECK does not catch (it tests NULL, not "").
    #[error("REUSABLE_CREDIT line {0} missing credit_grant_event_type")]
    MissingCreditEventType(Uuid),
    /// Underlying storage failure.
    #[error("balance projector db error: {0}")]
    Db(String),
    /// A coalesced money delta exceeded `i64` while summing an entry's same-grain
    /// legs — the one money path that previously saturated. Surfaced as a clean
    /// amount-class rejection rather than a silently-wrong saturated balance.
    #[error("coalesced money delta overflowed i64 for account {account_id} ({currency}, {field})")]
    Overflow {
        account_id: Uuid,
        currency: String,
        field: &'static str,
    },
}

/// One derived cache mutation, keyed by its grain. `table_rank` orders the
/// four cache tables; the remaining key parts order rows within a table so
/// concurrent posts acquire row locks in a single global order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GrainDelta {
    table_rank: GrainTable,
    tenant_id: Uuid,
    account_id: Uuid,
    currency: String,
    payer_tenant_id: Uuid,
    invoice_id: String,
    tax_jurisdiction: String,
    tax_filing_period: String,
    account_class: AccountClass,
    normal_side: Side,
    delta: i64,
    /// FX functional-column delta (Slice 5): the signed `functional_amount_minor`
    /// summed in parallel with `delta`, projected onto `functional_balance_minor`.
    /// `0` with `functional_currency = None` on single-currency grains, where the
    /// functional cache column stays NULL (functional ≡ transaction by identity).
    functional_delta: i64,
    /// The grain's functional (legal-entity reporting) currency; `Some` only on
    /// cross-currency grains. Drives `functional_currency` on the cache row.
    functional_currency: Option<String>,
    /// AR-invoice grain only (chargeback `ar_status` seam): the signed disputed
    /// sub-delta routed to `ar_invoice_balance.disputed_minor`, parallel to
    /// `delta` (which nets `balance_minor`). It is the line's signed amount
    /// (`delta`) when the AR line carries `ar_status = DISPUTED`, else `0`. A
    /// balanced reclass (`DR AR DISPUTED` + `CR AR ACTIVE`, same grain) thus nets
    /// ZERO on `balance_minor` (AR-class-neutral) while moving `+amount` onto
    /// `disputed_minor`; a `won` reversal (`DR AR ACTIVE` + `CR AR DISPUTED`)
    /// nets `-amount`. Every non-AR-invoice grain carries `0`.
    disputed_delta: i64,
    /// AR-invoice grain only (decision P): the entry's posted-at and the line's
    /// due date, stamped first-write-wins onto `ar_invoice_balance` so the
    /// oldest-first allocation precedence has a stable post date. Other grains
    /// carry the defaults (`posted_at` is unused, `due_date` is `None`).
    posted_at: OffsetDateTime,
    due_date: Option<NaiveDate>,
    /// Reusable-credit grain only: the credit-grant event type that sub-divides
    /// the wallet balance (a PK dim), and the entry's posted-at stamped
    /// first-write-wins onto `reusable_credit_subbalance.first_granted_at` as a
    /// recency marker. Other grains carry the defaults (empty event type,
    /// `first_granted_at` is `None`).
    credit_grant_event_type: String,
    first_granted_at: Option<OffsetDateTime>,
}

/// The canonical lock-order sort key for a [`GrainDelta`]: `(table_rank, tenant,
/// account, currency, payer, invoice, tax_juris, tax_filing, credit_grant_event_type)`.
/// Borrows the row's string dims, so it carries the delta's lifetime.
type GrainSortKey<'a> = (
    GrainTable,
    Uuid,
    Uuid,
    &'a str,
    Uuid,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
);

impl GrainDelta {
    /// The canonical ordering key: `(table_rank, tenant, account, currency,
    /// payer, invoice)` — extended with the tax dims and the credit-grant event
    /// type for total order.
    fn sort_key(&self) -> GrainSortKey<'_> {
        (
            self.table_rank,
            self.tenant_id,
            self.account_id,
            &self.currency,
            self.payer_tenant_id,
            &self.invoice_id,
            &self.tax_jurisdiction,
            &self.tax_filing_period,
            &self.credit_grant_event_type,
        )
    }
}

/// The cache table a [`GrainDelta`] targets, in canonical **lock order**:
/// `derive(Ord)` ranks by declaration order, so concurrent posts acquire row
/// locks in one global order (design §4.3 / §7) and the `match` in `project` is
/// exhaustive by construction — a new grain kind cannot be added without also
/// adding its upsert arm (the compiler enforces it; there is no wildcard arm).
///
/// The recognition tables sit just below the balance caches: a recognition post
/// (Slice 4) locks the `CONTRACT_LIABILITY` + `REVENUE` `account_balance` rows
/// first, then the schedule, then the segment, by `(tenant_id, schedule_id,
/// segment_no)`. Those two variants (and their `match` arms) are added here,
/// after `Tax`, when the `RecognitionRunner` starts projecting recognition grains.
///
/// **Canonical procedural lock order (design §4.7) — the full chain a Slice-3
/// adjustment handler observes, of which only the balance-cache ranks below are
/// `GrainTable` variants:**
/// `payment_settlement → account_balance → ar_invoice_balance → ar_payer_balance
/// → unallocated_balance → reusable_credit_subbalance → tax_subbalance →
/// recognition_schedule → recognition_segment → invoice_exposure →
/// payment_allocation_refund`, then by `(tenant_id, …key…)`.
///
/// The tail four — `recognition_schedule`/`recognition_segment` (Slice 4) and
/// `invoice_exposure`/`payment_allocation_refund` (Slice 3) — are NOT
/// `BalanceProjector` balance grains: they are single-row counter/stamp grains
/// touched by an in-place delta in the respective handler (e.g.
/// `CreditNoteHandler` bumps `invoice_exposure.credit_note_total_minor` then
/// `payment_allocation_refund.refunded_minor`), so they carry no `GrainTable`
/// rank and the projector ranks stay balance-only (`grain_lock_order_ranks_are_pinned`
/// pins exactly the balance set). The cross-table order among these tail tables
/// is enforced PROCEDURALLY by each handler's acquisition order — the same
/// discipline recognition uses (m11 docstring), extended with the two Slice-3
/// ranks appended last so there is no inversion vs Slices 1/2/4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GrainTable {
    Account,
    ArPayer,
    ArInvoice,
    Unallocated,
    ReusableCredit,
    Tax,
}

/// Projects posted lines into the derived balance caches.
#[derive(Clone, Default)]
pub struct BalanceProjector;

impl BalanceProjector {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Derive per-grain signed deltas, sort them into the canonical lock
    /// order, and upsert each cache row, re-asserting no-negative on the
    /// guarded classes. `created_seq` stamps `last_entry_seq` on every
    /// touched row.
    ///
    /// # Errors
    /// [`ProjectError::MissingNormalSide`] if a line's account is absent from
    /// `normal_sides`; [`ProjectError::NegativeBalance`] if a guarded balance
    /// would go negative; [`ProjectError::Db`] on a storage failure.
    ///
    /// # Panics
    /// Never in practice — the internal `unreachable!` guards against a grain
    /// carrying an unknown table rank, which the derivation cannot produce.
    pub async fn project(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        entry: &NewEntry,
        lines: &[NewLine],
        normal_sides: &HashMap<Uuid, Side>,
        created_seq: i64,
    ) -> Result<(), ProjectError> {
        let grains = derive_grains(entry, lines, normal_sides)?;

        for g in grains {
            match g.table_rank {
                GrainTable::Account => {
                    self.upsert_account_balance(txn, scope, &g, created_seq)
                        .await?;
                }
                GrainTable::ArPayer => self.upsert_ar_payer(txn, scope, &g, created_seq).await?,
                GrainTable::ArInvoice => {
                    self.upsert_ar_invoice(txn, scope, &g, created_seq).await?;
                }
                GrainTable::Unallocated => {
                    self.upsert_unallocated(txn, scope, &g, created_seq).await?;
                }
                GrainTable::ReusableCredit => {
                    self.upsert_reusable_credit(txn, scope, &g, created_seq)
                        .await?;
                }
                GrainTable::Tax => self.upsert_tax(txn, scope, &g, created_seq).await?,
            }
        }
        Ok(())
    }

    async fn upsert_account_balance(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        g: &GrainDelta,
        seq: i64,
    ) -> Result<(), ProjectError> {
        // Pre-check the projected balance for guarded classes BEFORE the
        // upsert: the DB no-negative CHECK would otherwise abort the whole
        // transaction, so the app-level guard fires first for a clean error.
        // The read takes no row lock (none used anywhere in this codebase);
        // under a concurrent race the P1 conditional CHECK is the backstop —
        // it aborts the txn so a negative balance can never persist.
        let guarded = g.account_class.is_guarded();
        let seed = if guarded {
            let current = account_balance::Entity::find()
                .filter(
                    Condition::all()
                        .add(account_balance::Column::TenantId.eq(g.tenant_id))
                        .add(account_balance::Column::AccountId.eq(g.account_id))
                        .add(account_balance::Column::Currency.eq(g.currency.clone())),
                )
                .secure()
                .scope_with(scope)
                .one(txn)
                .await
                .map_err(|e| ProjectError::Db(format!("account_balance pre-read: {e}")))?
                .map_or(0_i64, |r| r.balance_minor);
            let projected = current.saturating_add(g.delta);
            if projected < 0 {
                return Err(ProjectError::NegativeBalance {
                    account_id: g.account_id,
                    balance_minor: projected,
                });
            }
            // Seed the INSERT tuple with the projected (post-state) balance, not
            // the bare delta: Postgres evaluates the no-negative CHECK against the
            // INSERT VALUES tuple during ON CONFLICT arbitration, so a negative
            // delta (a legitimate net-down of a guarded balance, e.g. a reversal)
            // would be rejected on the arbiter tuple even though the DO UPDATE
            // path nets a non-negative result. The fresh-insert seed equals the
            // delta (current == 0), preserving first-post semantics; the conflict
            // path discards this seed and applies the atomic `+ delta` below.
            projected
        } else {
            g.delta
        };

        let am = account_balance::ActiveModel {
            tenant_id: Set(g.tenant_id),
            account_id: Set(g.account_id),
            currency: Set(g.currency.clone()),
            account_class: Set(g.account_class.as_str().to_owned()),
            normal_side: Set(g.normal_side.as_str().to_owned()),
            balance_minor: Set(seed),
            // Slice 5: functional columns are populated ONLY on cross-currency
            // posts (functional_currency = Some); single-currency posts leave them
            // NULL (functional ≡ transaction by identity). A plain `.add` on the
            // conflict path keeps NULL = NULL (no COALESCE).
            functional_balance_minor: Set(g
                .functional_currency
                .as_ref()
                .map(|_| g.functional_delta)),
            functional_currency: Set(g.functional_currency.clone()),
            last_entry_seq: Set(Some(seq)),
            version: Set(0),
        };
        let mut on_conflict = SecureOnConflict::<account_balance::Entity>::columns([
            account_balance::Column::TenantId,
            account_balance::Column::AccountId,
            account_balance::Column::Currency,
        ]);
        // The conflict path nets atomically with `existing + delta` (NOT the
        // seed): two racing posts then serialize at the row and the no-negative
        // CHECK on the resulting row is the backstop against a concurrent
        // overdraw the lockless pre-read could not see.
        on_conflict = on_conflict
            .value(
                account_balance::Column::BalanceMinor,
                Expr::col((
                    account_balance::Entity,
                    account_balance::Column::BalanceMinor,
                ))
                .add(g.delta),
            )
            .and_then(|oc| {
                oc.value(
                    account_balance::Column::Version,
                    Expr::col((account_balance::Entity, account_balance::Column::Version)).add(1),
                )
            })
            .and_then(|oc| {
                oc.value(
                    account_balance::Column::LastEntrySeq,
                    Expr::value(Some(seq)),
                )
            })
            // Slice 5: net the functional column with a PLAIN `+ functional_delta`
            // (NOT COALESCE) so a single-currency row's NULL stays NULL.
            .and_then(|oc| {
                oc.value(
                    account_balance::Column::FunctionalBalanceMinor,
                    Expr::col((
                        account_balance::Entity,
                        account_balance::Column::FunctionalBalanceMinor,
                    ))
                    .add(g.functional_delta),
                )
            })
            .and_then(|oc| {
                oc.value(
                    account_balance::Column::FunctionalCurrency,
                    Expr::value(g.functional_currency.clone()),
                )
            })
            .map_err(|e| ProjectError::Db(format!("account_balance on_conflict: {e}")))?;

        account_balance::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| ProjectError::Db(format!("account_balance scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("account_balance upsert: {e}")))?;
        Ok(())
    }

    async fn upsert_ar_payer(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        g: &GrainDelta,
        seq: i64,
    ) -> Result<(), ProjectError> {
        // AR is a guarded class — pre-check the projected balance (see
        // `upsert_account_balance` for why this precedes the upsert). The seed
        // is the projected post-state, not the bare delta, so a net-down does
        // not trip the no-negative CHECK on the INSERT arbiter tuple.
        let current = ar_payer_balance::Entity::find()
            .filter(
                Condition::all()
                    .add(ar_payer_balance::Column::TenantId.eq(g.tenant_id))
                    .add(ar_payer_balance::Column::PayerTenantId.eq(g.payer_tenant_id))
                    .add(ar_payer_balance::Column::AccountId.eq(g.account_id))
                    .add(ar_payer_balance::Column::Currency.eq(g.currency.clone())),
            )
            .secure()
            .scope_with(scope)
            .one(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("ar_payer_balance pre-read: {e}")))?
            .map_or(0_i64, |r| r.balance_minor);
        let projected = current.saturating_add(g.delta);
        if projected < 0 {
            return Err(ProjectError::NegativeBalance {
                account_id: g.account_id,
                balance_minor: projected,
            });
        }

        let am = ar_payer_balance::ActiveModel {
            tenant_id: Set(g.tenant_id),
            payer_tenant_id: Set(g.payer_tenant_id),
            account_id: Set(g.account_id),
            currency: Set(g.currency.clone()),
            balance_minor: Set(projected),
            // Slice 5: functional columns populated only on cross-currency posts
            // (Some); single-currency leaves them NULL. Plain `.add` on conflict
            // keeps NULL = NULL.
            functional_balance_minor: Set(g
                .functional_currency
                .as_ref()
                .map(|_| g.functional_delta)),
            functional_currency: Set(g.functional_currency.clone()),
            last_entry_seq: Set(Some(seq)),
            version: Set(0),
        };
        let mut on_conflict = SecureOnConflict::<ar_payer_balance::Entity>::columns([
            ar_payer_balance::Column::TenantId,
            ar_payer_balance::Column::PayerTenantId,
            ar_payer_balance::Column::AccountId,
            ar_payer_balance::Column::Currency,
        ]);
        on_conflict = on_conflict
            .value(
                ar_payer_balance::Column::BalanceMinor,
                Expr::col((
                    ar_payer_balance::Entity,
                    ar_payer_balance::Column::BalanceMinor,
                ))
                .add(g.delta),
            )
            .and_then(|oc| {
                oc.value(
                    ar_payer_balance::Column::Version,
                    Expr::col((ar_payer_balance::Entity, ar_payer_balance::Column::Version)).add(1),
                )
            })
            .and_then(|oc| {
                oc.value(
                    ar_payer_balance::Column::LastEntrySeq,
                    Expr::value(Some(seq)),
                )
            })
            // Slice 5: plain `+ functional_delta` (NOT COALESCE) — single-currency
            // NULL stays NULL.
            .and_then(|oc| {
                oc.value(
                    ar_payer_balance::Column::FunctionalBalanceMinor,
                    Expr::col((
                        ar_payer_balance::Entity,
                        ar_payer_balance::Column::FunctionalBalanceMinor,
                    ))
                    .add(g.functional_delta),
                )
            })
            .and_then(|oc| {
                oc.value(
                    ar_payer_balance::Column::FunctionalCurrency,
                    Expr::value(g.functional_currency.clone()),
                )
            })
            .map_err(|e| ProjectError::Db(format!("ar_payer_balance on_conflict: {e}")))?;

        ar_payer_balance::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| ProjectError::Db(format!("ar_payer_balance scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("ar_payer_balance upsert: {e}")))?;
        Ok(())
    }

    async fn upsert_ar_invoice(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        g: &GrainDelta,
        seq: i64,
    ) -> Result<(), ProjectError> {
        // AR invoice rows are guarded — pre-check the projected balance. The
        // seed is the projected post-state, not the bare delta, so a net-down
        // (e.g. a reversal or payment) does not trip the no-negative CHECK on
        // the INSERT arbiter tuple. The same pre-read also yields the current
        // `disputed_minor` so the INSERT tuple seeds its projected post-state
        // (the chargeback `ar_status` sub-balance; see below).
        let existing = ar_invoice_balance::Entity::find()
            .filter(
                Condition::all()
                    .add(ar_invoice_balance::Column::TenantId.eq(g.tenant_id))
                    .add(ar_invoice_balance::Column::PayerTenantId.eq(g.payer_tenant_id))
                    .add(ar_invoice_balance::Column::AccountId.eq(g.account_id))
                    .add(ar_invoice_balance::Column::InvoiceId.eq(g.invoice_id.clone())),
            )
            .secure()
            .scope_with(scope)
            .one(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("ar_invoice_balance pre-read: {e}")))?;
        let current = existing.as_ref().map_or(0_i64, |r| r.balance_minor);
        let current_disputed = existing.as_ref().map_or(0_i64, |r| r.disputed_minor);
        let projected = current.saturating_add(g.delta);
        if projected < 0 {
            return Err(ProjectError::NegativeBalance {
                account_id: g.account_id,
                balance_minor: projected,
            });
        }
        // The disputed sub-balance moves by `disputed_delta` and stays the
        // disputed slice of the (unchanged-by-a-reclass) open AR. Its own DB
        // CHECKs are the guard — `disputed_minor >= 0` and
        // `disputed_minor <= balance_minor` — NOT the balance_minor no-negative
        // path; we seed the INSERT tuple with the projected post-state so a
        // net-down (a `won` reversal) does not trip those CHECKs on the arbiter
        // tuple, exactly as `balance_minor` does above.
        let projected_disputed = current_disputed.saturating_add(g.disputed_delta);

        let am = ar_invoice_balance::ActiveModel {
            tenant_id: Set(g.tenant_id),
            payer_tenant_id: Set(g.payer_tenant_id),
            account_id: Set(g.account_id),
            invoice_id: Set(g.invoice_id.clone()),
            currency: Set(g.currency.clone()),
            balance_minor: Set(projected),
            disputed_minor: Set(projected_disputed),
            // Slice 5: functional columns populated only on cross-currency posts
            // (Some); single-currency leaves them NULL. Plain `.add` on conflict
            // keeps NULL = NULL.
            functional_balance_minor: Set(g
                .functional_currency
                .as_ref()
                .map(|_| g.functional_delta)),
            functional_currency: Set(g.functional_currency.clone()),
            // Decision P (first-write-wins): the INSERT tuple stamps the
            // original post date + due date; the `on_conflict` builder below
            // deliberately omits both columns, so a later net-down (payment /
            // reversal) never overwrites them.
            original_posted_at: Set(Some(g.posted_at)),
            due_date: Set(g.due_date),
            last_entry_seq: Set(Some(seq)),
            version: Set(0),
        };
        let mut on_conflict = SecureOnConflict::<ar_invoice_balance::Entity>::columns([
            ar_invoice_balance::Column::TenantId,
            ar_invoice_balance::Column::PayerTenantId,
            ar_invoice_balance::Column::AccountId,
            ar_invoice_balance::Column::InvoiceId,
        ]);
        on_conflict = on_conflict
            .value(
                ar_invoice_balance::Column::BalanceMinor,
                Expr::col((
                    ar_invoice_balance::Entity,
                    ar_invoice_balance::Column::BalanceMinor,
                ))
                .add(g.delta),
            )
            // The conflict path nets `disputed_minor` atomically with
            // `existing + disputed_delta` (parallel to `balance_minor`): two
            // racing posts serialize at the row and the `disputed_minor >= 0` /
            // `<= balance_minor` CHECKs on the resulting row are the backstop.
            .and_then(|oc| {
                oc.value(
                    ar_invoice_balance::Column::DisputedMinor,
                    Expr::col((
                        ar_invoice_balance::Entity,
                        ar_invoice_balance::Column::DisputedMinor,
                    ))
                    .add(g.disputed_delta),
                )
            })
            .and_then(|oc| {
                oc.value(
                    ar_invoice_balance::Column::Version,
                    Expr::col((
                        ar_invoice_balance::Entity,
                        ar_invoice_balance::Column::Version,
                    ))
                    .add(1),
                )
            })
            .and_then(|oc| {
                oc.value(
                    ar_invoice_balance::Column::LastEntrySeq,
                    Expr::value(Some(seq)),
                )
            })
            // Slice 5: plain `+ functional_delta` (NOT COALESCE) — single-currency
            // NULL stays NULL.
            .and_then(|oc| {
                oc.value(
                    ar_invoice_balance::Column::FunctionalBalanceMinor,
                    Expr::col((
                        ar_invoice_balance::Entity,
                        ar_invoice_balance::Column::FunctionalBalanceMinor,
                    ))
                    .add(g.functional_delta),
                )
            })
            .and_then(|oc| {
                oc.value(
                    ar_invoice_balance::Column::FunctionalCurrency,
                    Expr::value(g.functional_currency.clone()),
                )
            })
            .map_err(|e| ProjectError::Db(format!("ar_invoice_balance on_conflict: {e}")))?;

        ar_invoice_balance::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| ProjectError::Db(format!("ar_invoice_balance scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("ar_invoice_balance upsert: {e}")))?;
        Ok(())
    }

    async fn upsert_unallocated(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        g: &GrainDelta,
        seq: i64,
    ) -> Result<(), ProjectError> {
        // UNALLOCATED is a guarded class — pre-check the projected balance (see
        // `upsert_account_balance` for why this precedes the upsert). The seed is
        // the projected post-state, not the bare delta, so a net-down (an
        // allocation DR against unapplied cash) does not trip the no-negative
        // CHECK on the INSERT arbiter tuple.
        let current = unallocated_balance::Entity::find()
            .filter(
                Condition::all()
                    .add(unallocated_balance::Column::TenantId.eq(g.tenant_id))
                    .add(unallocated_balance::Column::PayerTenantId.eq(g.payer_tenant_id))
                    .add(unallocated_balance::Column::AccountId.eq(g.account_id))
                    .add(unallocated_balance::Column::Currency.eq(g.currency.clone())),
            )
            .secure()
            .scope_with(scope)
            .one(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("unallocated_balance pre-read: {e}")))?
            .map_or(0_i64, |r| r.balance_minor);
        let projected = current.saturating_add(g.delta);
        if projected < 0 {
            return Err(ProjectError::NegativeBalance {
                account_id: g.account_id,
                balance_minor: projected,
            });
        }

        let am = unallocated_balance::ActiveModel {
            tenant_id: Set(g.tenant_id),
            payer_tenant_id: Set(g.payer_tenant_id),
            account_id: Set(g.account_id),
            currency: Set(g.currency.clone()),
            balance_minor: Set(projected),
            // Slice 5: cross-currency only (Some); single-currency stays NULL.
            // Plain `.add` on conflict keeps NULL = NULL.
            functional_balance_minor: Set(g
                .functional_currency
                .as_ref()
                .map(|_| g.functional_delta)),
            functional_currency: Set(g.functional_currency.clone()),
            last_entry_seq: Set(Some(seq)),
            version: Set(0),
        };
        let mut on_conflict = SecureOnConflict::<unallocated_balance::Entity>::columns([
            unallocated_balance::Column::TenantId,
            unallocated_balance::Column::PayerTenantId,
            unallocated_balance::Column::Currency,
        ]);
        on_conflict = on_conflict
            .value(
                unallocated_balance::Column::BalanceMinor,
                Expr::col((
                    unallocated_balance::Entity,
                    unallocated_balance::Column::BalanceMinor,
                ))
                .add(g.delta),
            )
            .and_then(|oc| {
                oc.value(
                    unallocated_balance::Column::Version,
                    Expr::col((
                        unallocated_balance::Entity,
                        unallocated_balance::Column::Version,
                    ))
                    .add(1),
                )
            })
            .and_then(|oc| {
                oc.value(
                    unallocated_balance::Column::LastEntrySeq,
                    Expr::value(Some(seq)),
                )
            })
            // Slice 5: plain `+ functional_delta` (NOT COALESCE) — single-currency
            // NULL stays NULL.
            .and_then(|oc| {
                oc.value(
                    unallocated_balance::Column::FunctionalBalanceMinor,
                    Expr::col((
                        unallocated_balance::Entity,
                        unallocated_balance::Column::FunctionalBalanceMinor,
                    ))
                    .add(g.functional_delta),
                )
            })
            .and_then(|oc| {
                oc.value(
                    unallocated_balance::Column::FunctionalCurrency,
                    Expr::value(g.functional_currency.clone()),
                )
            })
            .map_err(|e| ProjectError::Db(format!("unallocated_balance on_conflict: {e}")))?;

        unallocated_balance::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| ProjectError::Db(format!("unallocated_balance scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("unallocated_balance upsert: {e}")))?;
        Ok(())
    }

    async fn upsert_reusable_credit(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        g: &GrainDelta,
        seq: i64,
    ) -> Result<(), ProjectError> {
        // REUSABLE_CREDIT is NOT an `AccountClass::GUARDED` class, so
        // `account_balance` does not pre-check it — the wallet-overdraw invariant
        // (a balance may never be spent below zero) lives ONLY on this sub-grain:
        // here as the app-level pre-check, and the `chk_reusable_credit_subbalance_no_negative`
        // DB CHECK as the serializable backstop against a concurrent overdraw the
        // lockless pre-read cannot see. The seed is the projected post-state, not
        // the bare delta, so a net-down (a spend against an existing wallet
        // balance) does not trip the no-negative CHECK on the INSERT arbiter
        // tuple (see `upsert_account_balance` for the full arbitration rationale).
        let current = reusable_credit_subbalance::Entity::find()
            .filter(
                Condition::all()
                    .add(reusable_credit_subbalance::Column::TenantId.eq(g.tenant_id))
                    .add(reusable_credit_subbalance::Column::PayerTenantId.eq(g.payer_tenant_id))
                    .add(reusable_credit_subbalance::Column::AccountId.eq(g.account_id))
                    .add(reusable_credit_subbalance::Column::Currency.eq(g.currency.clone()))
                    .add(
                        reusable_credit_subbalance::Column::CreditGrantEventType
                            .eq(g.credit_grant_event_type.clone()),
                    ),
            )
            .secure()
            .scope_with(scope)
            .one(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("reusable_credit_subbalance pre-read: {e}")))?
            .map_or(0_i64, |r| r.balance_minor);
        let projected = current.saturating_add(g.delta);
        if projected < 0 {
            return Err(ProjectError::NegativeBalance {
                account_id: g.account_id,
                balance_minor: projected,
            });
        }

        let am = reusable_credit_subbalance::ActiveModel {
            tenant_id: Set(g.tenant_id),
            payer_tenant_id: Set(g.payer_tenant_id),
            account_id: Set(g.account_id),
            currency: Set(g.currency.clone()),
            credit_grant_event_type: Set(g.credit_grant_event_type.clone()),
            // First-write-wins recency stamp: the INSERT tuple records the first
            // grant's posted-at; the `on_conflict` builder below deliberately
            // omits this column, so later grants/spends never overwrite it
            // (exactly like `ar_invoice_balance.original_posted_at`).
            first_granted_at: Set(g.first_granted_at),
            balance_minor: Set(projected),
            // Slice 5: cross-currency only (Some); single-currency stays NULL.
            // Plain `.add` on conflict keeps NULL = NULL.
            functional_balance_minor: Set(g
                .functional_currency
                .as_ref()
                .map(|_| g.functional_delta)),
            functional_currency: Set(g.functional_currency.clone()),
            last_entry_seq: Set(Some(seq)),
            version: Set(0),
        };
        let mut on_conflict = SecureOnConflict::<reusable_credit_subbalance::Entity>::columns([
            reusable_credit_subbalance::Column::TenantId,
            reusable_credit_subbalance::Column::PayerTenantId,
            reusable_credit_subbalance::Column::Currency,
            reusable_credit_subbalance::Column::CreditGrantEventType,
        ]);
        on_conflict = on_conflict
            .value(
                reusable_credit_subbalance::Column::BalanceMinor,
                Expr::col((
                    reusable_credit_subbalance::Entity,
                    reusable_credit_subbalance::Column::BalanceMinor,
                ))
                .add(g.delta),
            )
            .and_then(|oc| {
                oc.value(
                    reusable_credit_subbalance::Column::Version,
                    Expr::col((
                        reusable_credit_subbalance::Entity,
                        reusable_credit_subbalance::Column::Version,
                    ))
                    .add(1),
                )
            })
            .and_then(|oc| {
                oc.value(
                    reusable_credit_subbalance::Column::LastEntrySeq,
                    Expr::value(Some(seq)),
                )
            })
            // Slice 5: plain `+ functional_delta` (NOT COALESCE) — single-currency
            // NULL stays NULL.
            .and_then(|oc| {
                oc.value(
                    reusable_credit_subbalance::Column::FunctionalBalanceMinor,
                    Expr::col((
                        reusable_credit_subbalance::Entity,
                        reusable_credit_subbalance::Column::FunctionalBalanceMinor,
                    ))
                    .add(g.functional_delta),
                )
            })
            .and_then(|oc| {
                oc.value(
                    reusable_credit_subbalance::Column::FunctionalCurrency,
                    Expr::value(g.functional_currency.clone()),
                )
            })
            .map_err(|e| {
                ProjectError::Db(format!("reusable_credit_subbalance on_conflict: {e}"))
            })?;

        reusable_credit_subbalance::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| ProjectError::Db(format!("reusable_credit_subbalance scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("reusable_credit_subbalance upsert: {e}")))?;
        Ok(())
    }

    async fn upsert_tax(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        g: &GrainDelta,
        seq: i64,
    ) -> Result<(), ProjectError> {
        let am = tax_subbalance::ActiveModel {
            tenant_id: Set(g.tenant_id),
            account_id: Set(g.account_id),
            tax_jurisdiction: Set(g.tax_jurisdiction.clone()),
            tax_filing_period: Set(g.tax_filing_period.clone()),
            balance_minor: Set(g.delta),
            last_entry_seq: Set(Some(seq)),
            version: Set(0),
        };
        let mut on_conflict = SecureOnConflict::<tax_subbalance::Entity>::columns([
            tax_subbalance::Column::TenantId,
            tax_subbalance::Column::AccountId,
            tax_subbalance::Column::TaxJurisdiction,
            tax_subbalance::Column::TaxFilingPeriod,
        ]);
        on_conflict = on_conflict
            .value(
                tax_subbalance::Column::BalanceMinor,
                Expr::col((tax_subbalance::Entity, tax_subbalance::Column::BalanceMinor))
                    .add(g.delta),
            )
            .and_then(|oc| {
                oc.value(
                    tax_subbalance::Column::Version,
                    Expr::col((tax_subbalance::Entity, tax_subbalance::Column::Version)).add(1),
                )
            })
            .and_then(|oc| oc.value(tax_subbalance::Column::LastEntrySeq, Expr::value(Some(seq))))
            .map_err(|e| ProjectError::Db(format!("tax_subbalance on_conflict: {e}")))?;

        // tax_subbalance has no no-negative CHECK and is not a guarded class.
        tax_subbalance::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| ProjectError::Db(format!("tax_subbalance scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| ProjectError::Db(format!("tax_subbalance upsert: {e}")))?;
        Ok(())
    }
}

/// Derive the sorted per-grain deltas for an entry's lines (pure;
/// unit-testable). Each line always touches `account_balance`; `AR` lines
/// additionally touch the payer (always) and invoice (when present) grains;
/// `TAX_PAYABLE` lines with both tax dims touch the tax grain.
fn derive_grains(
    entry: &NewEntry,
    lines: &[NewLine],
    normal_sides: &HashMap<Uuid, Side>,
) -> Result<Vec<GrainDelta>, ProjectError> {
    let tenant = entry.tenant_id;
    let posted_at = entry.posted_at_utc;
    let mut grains: Vec<GrainDelta> = Vec::new();

    for line in lines {
        let normal_side = *normal_sides
            .get(&line.account_id)
            .ok_or(ProjectError::MissingNormalSide(line.account_id))?;
        let delta = if line.side == normal_side {
            line.amount_minor
        } else {
            -line.amount_minor
        };
        // FX (Slice 5): mirror the transaction sign onto the functional column.
        // Present only on cross-currency lines; single-currency lines contribute
        // 0 / None so the functional cache column stays NULL.
        let functional_delta = line
            .functional_amount_minor
            .map_or(0, |f| if line.side == normal_side { f } else { -f });
        let functional_currency = line.functional_currency.clone();

        grains.push(GrainDelta {
            table_rank: GrainTable::Account,
            tenant_id: tenant,
            account_id: line.account_id,
            currency: line.currency.clone(),
            payer_tenant_id: line.payer_tenant_id,
            invoice_id: String::new(),
            tax_jurisdiction: String::new(),
            tax_filing_period: String::new(),
            account_class: line.account_class,
            normal_side,
            delta,
            functional_delta,
            functional_currency: functional_currency.clone(),
            disputed_delta: 0,
            posted_at,
            due_date: None,
            credit_grant_event_type: String::new(),
            first_granted_at: None,
        });

        if line.account_class == AccountClass::Ar {
            // Chargeback `ar_status` seam: a `DISPUTED` AR line routes its signed
            // amount (`delta`: DR +, CR −) onto `disputed_minor`; every other AR
            // line leaves it untouched. The disputed sub-balance is tracked at
            // the invoice grain only (`ar_invoice_balance.disputed_minor`), so
            // the payer grain carries `0`.
            let disputed_delta = if line.ar_status.as_deref() == Some(AR_STATUS_DISPUTED) {
                delta
            } else {
                0
            };
            grains.push(GrainDelta {
                table_rank: GrainTable::ArPayer,
                tenant_id: tenant,
                account_id: line.account_id,
                currency: line.currency.clone(),
                payer_tenant_id: line.payer_tenant_id,
                invoice_id: String::new(),
                tax_jurisdiction: String::new(),
                tax_filing_period: String::new(),
                account_class: line.account_class,
                normal_side,
                delta,
                functional_delta,
                functional_currency: functional_currency.clone(),
                disputed_delta: 0,
                posted_at,
                due_date: None,
                credit_grant_event_type: String::new(),
                first_granted_at: None,
            });
            if let Some(invoice_id) = &line.invoice_id {
                grains.push(GrainDelta {
                    table_rank: GrainTable::ArInvoice,
                    tenant_id: tenant,
                    account_id: line.account_id,
                    currency: line.currency.clone(),
                    payer_tenant_id: line.payer_tenant_id,
                    invoice_id: invoice_id.clone(),
                    tax_jurisdiction: String::new(),
                    tax_filing_period: String::new(),
                    account_class: line.account_class,
                    normal_side,
                    delta,
                    functional_delta,
                    functional_currency: functional_currency.clone(),
                    disputed_delta,
                    // Decision P: stamp the entry's posted-at + the line's due
                    // date; `upsert_ar_invoice` writes them first-write-wins.
                    posted_at,
                    due_date: line.due_date,
                    credit_grant_event_type: String::new(),
                    first_granted_at: None,
                });
            }
        }

        if line.account_class == AccountClass::Unallocated {
            grains.push(GrainDelta {
                table_rank: GrainTable::Unallocated,
                tenant_id: tenant,
                account_id: line.account_id,
                currency: line.currency.clone(),
                payer_tenant_id: line.payer_tenant_id,
                invoice_id: String::new(),
                tax_jurisdiction: String::new(),
                tax_filing_period: String::new(),
                account_class: line.account_class,
                normal_side,
                delta,
                functional_delta,
                functional_currency: functional_currency.clone(),
                disputed_delta: 0,
                posted_at,
                due_date: None,
                credit_grant_event_type: String::new(),
                first_granted_at: None,
            });
        }

        if line.account_class == AccountClass::ReusableCredit {
            // The credit-grant event type sub-divides the wallet (a PK dim). A
            // missing/empty value would key a phantom "" sub-balance, so reject
            // rather than default — the DB NOT-NULL CHECK tests NULL, not "", so
            // it would not catch the empty string. `first_granted_at` is stamped
            // first-write-wins by the upsert.
            let credit_grant_event_type = line
                .credit_grant_event_type
                .clone()
                .filter(|s| !s.is_empty())
                .ok_or(ProjectError::MissingCreditEventType(line.line_id))?;
            grains.push(GrainDelta {
                table_rank: GrainTable::ReusableCredit,
                tenant_id: tenant,
                account_id: line.account_id,
                currency: line.currency.clone(),
                payer_tenant_id: line.payer_tenant_id,
                invoice_id: String::new(),
                tax_jurisdiction: String::new(),
                tax_filing_period: String::new(),
                account_class: line.account_class,
                normal_side,
                delta,
                functional_delta,
                functional_currency: functional_currency.clone(),
                disputed_delta: 0,
                posted_at,
                due_date: None,
                credit_grant_event_type,
                first_granted_at: Some(posted_at),
            });
        }

        if line.account_class == AccountClass::TaxPayable
            && let (Some(juris), Some(filing)) = (&line.tax_jurisdiction, &line.tax_filing_period)
        {
            grains.push(GrainDelta {
                table_rank: GrainTable::Tax,
                tenant_id: tenant,
                account_id: line.account_id,
                currency: line.currency.clone(),
                payer_tenant_id: line.payer_tenant_id,
                invoice_id: String::new(),
                tax_jurisdiction: juris.clone(),
                tax_filing_period: filing.clone(),
                account_class: line.account_class,
                normal_side,
                delta,
                functional_delta,
                functional_currency: functional_currency.clone(),
                disputed_delta: 0,
                posted_at,
                due_date: None,
                credit_grant_event_type: String::new(),
                first_granted_at: None,
            });
        }
    }

    grains.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

    // Coalesce grains that map to the SAME cache row (equal sort key) by summing
    // their deltas, so the no-negative guard and the upsert see the entry's NET
    // effect per grain. Without this, a balanced multi-line entry touching one
    // guarded account (e.g. a -100 leg then a +150 leg, net +50) would be
    // rejected on the intermediate running balance, order-dependently.
    let mut coalesced: Vec<GrainDelta> = Vec::with_capacity(grains.len());
    for g in grains {
        if let Some(last) = coalesced.last_mut()
            && last.sort_key() == g.sort_key()
        {
            // Checked, not saturating: a money delta that overflows `i64` is a
            // hard rejection (surfaced as AmountOutOfRange / 422 at the call-site),
            // never a silently-clamped balance. Same grain (equal sort key) ⇒ same
            // account/currency, captured once for the error.
            let acc = last.account_id;
            let ccy = last.currency.clone();
            last.delta = last
                .delta
                .checked_add(g.delta)
                .ok_or_else(|| ProjectError::Overflow {
                    account_id: acc,
                    currency: ccy.clone(),
                    field: "delta",
                })?;
            // The disputed sub-delta coalesces in parallel: an AR reclass's two
            // same-grain legs net ZERO on `delta` (balance_minor) while summing
            // their `disputed_delta` to the net disputed move (`+D` opened, `-D`
            // won).
            last.disputed_delta = last
                .disputed_delta
                .checked_add(g.disputed_delta)
                .ok_or_else(|| ProjectError::Overflow {
                    account_id: acc,
                    currency: ccy.clone(),
                    field: "disputed_delta",
                })?;
            last.functional_delta = last
                .functional_delta
                .checked_add(g.functional_delta)
                .ok_or(ProjectError::Overflow {
                    account_id: acc,
                    currency: ccy,
                    field: "functional_delta",
                })?;
            if last.functional_currency.is_none() {
                last.functional_currency.clone_from(&g.functional_currency);
            }
            continue;
        }
        coalesced.push(g);
    }
    Ok(coalesced)
}

#[cfg(test)]
#[path = "projector_tests.rs"]
mod projector_tests;
