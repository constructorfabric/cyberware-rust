//! `AdjustmentRepo` — the Slice-3 adjustment counter/record tables
//! (`invoice_exposure`, `credit_note`, `debit_note`), plus the two ledger reads a
//! credit note needs: the invoice's **posted AR incl. tax** (the headroom seed
//! basis) and its **current open AR** (the AR-vs-wallet credit-leg cap).
//!
//! A debit note (Group D) **raises** the headroom: `add_debit_note_total` bumps
//! `debit_note_total_minor` (the RHS of the headroom CHECK), so it can never trip
//! the cap — it only widens the room available to later credit notes; the
//! `insert_debit_note` record write mirrors `insert_credit_note`.
//!
//! The counter **writes** (`seed_exposure_first_touch`, `add_credit_note_total`)
//! and the record write (`insert_credit_note`) run inside the passed-in posting
//! transaction (the in-txn `CreditNoteHandler` sidecar), mirroring
//! [`PaymentRepo`](super::PaymentRepo) / [`RecognitionRepo`](super::RecognitionRepo):
//! `seed_exposure_first_touch` is the Slice-1 first-touch `INSERT … ON CONFLICT DO
//! UPDATE` (so concurrent creators serialize, no duplicate-key), and
//! `add_credit_note_total` is the counter-delta-under-lock bumping
//! `credit_note_total_minor`, with the `chk_ledger_invoice_exposure_headroom` CHECK
//! (`credit_note_total_minor <= original_total_minor + debit_note_total_minor`,
//! AC #24) as the authoritative over-cap guard — a violation surfaces as
//! [`RepoError::MoneyOutCapExceeded`], which the handler refines to the
//! `CREDIT_NOTE_EXCEEDS_HEADROOM` wire code (mirroring how `PaymentRepo::add_*`
//! maps its per-payment cap CHECKs).
//!
//! The **reads** (`read_posted_ar_incl_tax_out_of_txn`,
//! `read_open_ar_for_invoice_out_of_txn`) take the PDP-compiled `AccessScope` and
//! run out-of-txn on a fresh scoped connection — the handler's PRE-txn cap basis,
//! exactly as [`CreditApplicationService`](crate::infra::payment::credit) reads
//! its open-AR candidates out-of-txn; the authoritative in-txn backstops (the
//! headroom CHECK, the AR no-negative CHECK) cover a concurrent race the lockless
//! reads cannot see. Both are `.secure().scope_with` (SQL-level BOLA — a foreign
//! tenant yields no rows).

use bss_ledger_sdk::{AccountClass, Side, SourceDocType};

use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, DbErr, EntityTrait};
use toolkit_db::secure::{
    AccessScope, DbTx, ScopeError, SecureEntityExt, SecureInsertExt, SecureOnConflict,
    SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use toolkit_db::odata::sea_orm_filter::{LimitCfg, paginate_odata};
use toolkit_odata::{ODataQuery, Page, SortDir};

use crate::domain::model::RepoError;
use crate::infra::storage::entity::{
    credit_note, debit_note, invoice_exposure, journal_entry, journal_line, refund,
};
use crate::infra::storage::odata_mapping::{
    CreditNoteODataMapper, DebitNoteODataMapper, RefundODataMapper,
};
use crate::infra::storage::repo::journal_repo::{
    OdataPageError, map_odata_err, query_with_default_order,
};
use crate::odata::{CreditNoteFilterField, DebitNoteFilterField, RefundFilterField};
use time::OffsetDateTime;

/// A `credit_note` row to persist (the record linking a posted credit note to its
/// originating invoice item + the recognized/deferred split basis). `amount_minor`
/// is incl-tax; the split parts are ex-tax and do NOT sum to it (no CHECK).
pub struct NewCreditNote {
    pub tenant_id: Uuid,
    pub credit_note_id: String,
    pub origin_invoice_id: String,
    pub origin_invoice_item_ref: Option<String>,
    pub revenue_stream: String,
    pub currency: String,
    pub amount_minor: i64,
    pub recognized_part_minor: i64,
    pub deferred_part_minor: i64,
    pub split_basis_ref: Option<String>,
    pub reason_code: String,
    pub created_at_utc: OffsetDateTime,
}

/// A `debit_note` row to persist (the record linking a posted debit note — an
/// additional charge — to its originating invoice + its recognized/deferred split,
/// design §4.3). `amount_minor` is incl-tax; the split parts are ex-tax and do NOT
/// sum to it (no CHECK, mirroring `credit_note`).
pub struct NewDebitNote {
    pub tenant_id: Uuid,
    pub debit_note_id: String,
    pub origin_invoice_id: String,
    pub currency: String,
    pub amount_minor: i64,
    pub recognized_part_minor: i64,
    pub deferred_part_minor: i64,
    pub created_at_utc: OffsetDateTime,
}

/// A `refund` row to persist (the record of a PSP refund's two-stage lifecycle,
/// design §4.4). The surrogate PK is `(tenant_id, refund_id)`; the idempotency
/// grain is the natural `(tenant_id, psp_refund_id, phase)` (a separate UNIQUE
/// index — one PSP refund advances through several `phase` rows). `amount_minor`
/// is the cash returned. `invoice_id` is `None` for Pattern A (`A_UNALLOCATED`)
/// and `Some` for Pattern B (`B_RESTORE_AR`). `clearing_state` tracks the
/// two-stage `REFUND_CLEARING` drain (`PENDING → SETTLED`, or `REVERSED` on a PSP
/// reject/void in Group E). `reverses_entry_id` / `relates_to_refund_id` stay
/// `None` in Group B (the stage-1 line-negation is Group E, the refund-of-refund
/// link is Group E too); they are carried for the forward-compatible row shape.
pub struct NewRefund {
    pub tenant_id: Uuid,
    pub refund_id: String,
    pub psp_refund_id: String,
    /// The phase wire literal (`RefundPhase::as_str`) — the `chk_ledger_refund_phase`
    /// CHECK set + the natural-UNIQUE grain.
    pub phase: String,
    /// The pattern wire literal (`RefundPattern::as_str`) —
    /// `chk_ledger_refund_pattern`.
    pub pattern: String,
    pub payment_id: String,
    pub invoice_id: Option<String>,
    pub currency: String,
    pub amount_minor: i64,
    /// The clearing-state wire literal (`PENDING` / `SETTLED` / `REVERSED`) —
    /// `chk_ledger_refund_clearing_state`.
    pub clearing_state: String,
    /// The refund-of-refund forward link (Group E); `None` in Group B.
    pub relates_to_refund_id: Option<String>,
    /// The negated stage-1 entry id on a PSP reject/void (Group E); `None` in
    /// Group B.
    pub reverses_entry_id: Option<Uuid>,
    pub created_at_utc: OffsetDateTime,
}

/// SeaORM-backed Slice-3 adjustment counter + record + read repository.
#[derive(Clone)]
pub struct AdjustmentRepo {
    db: DBProvider<DbError>,
}

impl AdjustmentRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    // --- Out-of-txn reads (the handler's pre-txn cap basis; PDP In-scoped,
    //     SQL-level BOLA). The headroom CHECK + the AR no-negative CHECK are the
    //     authoritative in-txn backstops, exactly as `CreditApplicationService`
    //     reads open-AR candidates out-of-txn and relies on the AR CHECK. ---

    /// Sum the invoice's **posted AR incl. tax** — the headroom seed basis
    /// (`invoice_exposure.original_total_minor`, design §4.7): the net of the
    /// `AR`-class `journal_line`s of the invoice's `INVOICE_POST` entry (DR adds,
    /// any CR nets down), for `(tenant, origin_invoice_id)`. This is the ORIGINAL
    /// posted receivable (incl. tax), independent of later payments — exactly the
    /// cap basis the design seeds at first touch, read from the journal (NOT the
    /// payment-reduced `ar_invoice_balance` cache). Scoped (SQL-level BOLA).
    ///
    /// Implemented as a scoped two-step read (entry ids for the invoice's
    /// `INVOICE_POST` business id, then their AR lines), the gear's single-entity
    /// idiom; the per-invoice line set is tiny. Returns `0` when the invoice has no
    /// posted AR line (a fully-deferred or non-AR invoice — the headroom then
    /// floors at the debit-note total).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_posted_ar_incl_tax_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        origin_invoice_id: &str,
    ) -> Result<i64, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        // The original posted AR lives on the invoice's INVOICE_POST entry — keyed
        // by `source_business_id = invoice_id` on the header (the line carries no
        // business id). Resolve those entry ids first (scoped).
        let entry_ids: Vec<Uuid> = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::TenantId.eq(tenant))
                    .add(
                        journal_entry::Column::SourceDocType
                            .eq(SourceDocType::InvoicePost.as_str()),
                    )
                    .add(journal_entry::Column::SourceBusinessId.eq(origin_invoice_id)),
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read invoice-post entry ids: {e}")))?
            .into_iter()
            .map(|e| e.entry_id)
            .collect();
        if entry_ids.is_empty() {
            return Ok(0);
        }

        // Net the AR lines of those entries (DR +, CR −). `journal_line` also
        // carries `invoice_id`, so filter on it defensively (a single INVOICE_POST
        // entry is one invoice, but the AND keeps the read robust).
        let lines = journal_line::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_line::Column::TenantId.eq(tenant))
                    .add(journal_line::Column::AccountClass.eq(AccountClass::Ar.as_str()))
                    .add(journal_line::Column::InvoiceId.eq(origin_invoice_id))
                    .add(journal_line::Column::EntryId.is_in(entry_ids)),
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read posted AR lines: {e}")))?;
        let mut total: i64 = 0;
        for line in lines {
            let signed = if line.side == Side::Debit.as_str() {
                line.amount_minor
            } else {
                -line.amount_minor
            };
            total = total.saturating_add(signed);
        }
        // `Ord::max` spelled explicitly: `ExprTrait` is in scope in this module
        // for the query builders and also provides a `max`.
        Ok(Ord::max(total, 0))
    }

    /// `true` iff a **posted invoice** exists for `(tenant, origin_invoice_id)` —
    /// i.e. there is at least one `INVOICE_POST` `journal_entry` whose
    /// `source_business_id` is the invoice id (design §4.2 / §5: a credit/debit
    /// note MUST link an originating posted invoice, else `NOTE_INVOICE_NOT_FOUND`
    /// / 404). Scoped (SQL-level BOLA — a foreign tenant yields `false`, the same
    /// 404 as absent, no existence leak). Out-of-txn on a fresh scoped connection
    /// (the handler's pre-txn link gate); mirrors the entry-id resolution
    /// [`Self::read_posted_ar_incl_tax_out_of_txn`] does, but only needs existence,
    /// so it `LIMIT 1`s rather than netting the AR lines.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn posted_invoice_exists_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        origin_invoice_id: &str,
    ) -> Result<bool, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let found = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::TenantId.eq(tenant))
                    .add(
                        journal_entry::Column::SourceDocType
                            .eq(SourceDocType::InvoicePost.as_str()),
                    )
                    .add(journal_entry::Column::SourceBusinessId.eq(origin_invoice_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read invoice-post entry existence: {e}")))?;
        Ok(found.is_some())
    }

    /// Sum the invoice's **current open AR** (incl. tax) — the AR-vs-wallet
    /// credit-leg cap (design §4.2 / K-2): the `balance_minor` of the
    /// `ar_invoice_balance` cache rows for `(tenant, origin_invoice_id)` (summed
    /// across the payer/account grain, though v1 is one row per invoice). This is
    /// the payment-reduced open receivable the `CR AR` leg is capped at; the
    /// remainder credits `REUSABLE_CREDIT`. Scoped (SQL-level BOLA).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_open_ar_for_invoice_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        origin_invoice_id: &str,
    ) -> Result<i64, RepoError> {
        use crate::infra::storage::entity::ar_invoice_balance;
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
                    .add(ar_invoice_balance::Column::InvoiceId.eq(origin_invoice_id)),
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read open AR for invoice: {e}")))?;
        let mut total: i64 = 0;
        for r in rows {
            total = total.saturating_add(Ord::max(r.balance_minor, 0));
        }
        Ok(total)
    }

    /// Read the `invoice_exposure` headroom row for `(tenant, invoice_id)` — the
    /// `GET /invoices/{invoice_id}/exposure` source (Group E). Returns the row (so
    /// the handler can compute remaining headroom = `original_total_minor +
    /// debit_note_total_minor − credit_note_total_minor`), or `None` when no note
    /// has ever touched this invoice (the row is seeded at the first credit/debit
    /// note's first touch — an invoice with neither yet has no exposure row). Scoped
    /// (SQL-level BOLA — a foreign tenant yields `None`, the same 404 as absent, no
    /// existence leak). Out-of-txn on a fresh scoped connection (a pure read).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_exposure_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        invoice_id: &str,
    ) -> Result<Option<invoice_exposure::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        invoice_exposure::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(invoice_exposure::Column::TenantId.eq(tenant))
                    .add(invoice_exposure::Column::InvoiceId.eq(invoice_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read invoice_exposure: {e}")))
    }

    // --- In-txn counter / record writes (called by the CreditNoteHandler sidecar) ---

    /// **First-touch** seed of the `invoice_exposure` row for `(tenant,
    /// invoice_id)` with `original_total_minor = original_total`, via an
    /// `INSERT … ON CONFLICT DO UPDATE` that is a no-op on conflict (it re-stamps
    /// `original_total_minor` to its own existing value, so a concurrent second
    /// creator serializes at the row without changing it — the Slice-1 first-touch
    /// upsert, design §4.7). `debit_note_total_minor` / `credit_note_total_minor`
    /// default to 0 on the fresh insert and are LEFT UNTOUCHED on conflict (the
    /// running counters are owned by `add_credit_note_total` / the debit-note
    /// handler). Idempotent + concurrency-safe: many credit notes on one invoice
    /// all call this, only the first seeds, the rest are no-ops.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn seed_exposure_first_touch(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        invoice_id: &str,
        currency: &str,
        original_total: i64,
    ) -> Result<(), RepoError> {
        let am = invoice_exposure::ActiveModel {
            tenant_id: Set(tenant),
            invoice_id: Set(invoice_id.to_owned()),
            currency: Set(currency.to_owned()),
            original_total_minor: Set(original_total),
            debit_note_total_minor: Set(0),
            credit_note_total_minor: Set(0),
            version: Set(0),
        };
        // DO UPDATE that re-stamps `original_total_minor` to ITS OWN existing value
        // (a self-assignment): a fresh insert seeds the real total; a conflict
        // serializes at the row but changes nothing — the running counters are
        // never reset. (`do_nothing` would also work, but an explicit self-update
        // forces the row lock the headroom serialization relies on, mirroring the
        // projector's first-write-wins upserts that always net a column.)
        let on_conflict = SecureOnConflict::<invoice_exposure::Entity>::columns([
            invoice_exposure::Column::TenantId,
            invoice_exposure::Column::InvoiceId,
        ])
        .value(
            invoice_exposure::Column::OriginalTotalMinor,
            Expr::col((
                invoice_exposure::Entity,
                invoice_exposure::Column::OriginalTotalMinor,
            )),
        )
        .map_err(|e| RepoError::Db(format!("invoice_exposure on_conflict: {e}")))?;

        invoice_exposure::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("invoice_exposure scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| RepoError::Db(format!("seed invoice_exposure: {e}")))?;
        Ok(())
    }

    /// Increment `invoice_exposure.credit_note_total_minor` by `delta` (the new
    /// credit note's incl-tax amount) for `(tenant, invoice_id)`, bumping
    /// `version`. The `chk_ledger_invoice_exposure_headroom` CHECK
    /// (`credit_note_total_minor <= original_total_minor + debit_note_total_minor`,
    /// AC #24) is the authoritative headroom guard, evaluated against the resulting
    /// row: an over-cap bump surfaces as [`RepoError::MoneyOutCapExceeded`] (the
    /// handler refines it to `CreditNoteExceedsHeadroom` → `CREDIT_NOTE_EXCEEDS_HEADROOM`).
    /// A scoped UPDATE, not an upsert: the row is always seeded first by
    /// [`Self::seed_exposure_first_touch`], and an `INSERT … ON CONFLICT` would
    /// trip the headroom CHECK on the INSERT VALUES tuple during arbitration (the
    /// `PaymentRepo::add_allocated` rationale). SSI + retry serialize concurrent
    /// credit notes on the same invoice; `rows_affected == 0` ⇒ no exposure row
    /// (the seed must precede this — an invariant breach).
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when the headroom CHECK rejects the bump;
    /// [`RepoError::Db`] when no row matched or on any other scope / storage
    /// failure.
    pub async fn add_credit_note_total(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        invoice_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        let result = invoice_exposure::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                invoice_exposure::Column::CreditNoteTotalMinor,
                Expr::col((
                    invoice_exposure::Entity,
                    invoice_exposure::Column::CreditNoteTotalMinor,
                ))
                .add(delta),
            )
            .col_expr(
                invoice_exposure::Column::Version,
                Expr::col((invoice_exposure::Entity, invoice_exposure::Column::Version)).add(1),
            )
            .filter(
                Condition::all()
                    .add(invoice_exposure::Column::TenantId.eq(tenant))
                    .add(invoice_exposure::Column::InvoiceId.eq(invoice_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_headroom_violation("bump credit_note_total_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "invoice_exposure row absent for ({tenant}, {invoice_id}) — not seeded"
            )));
        }
        Ok(())
    }

    /// Increment `invoice_exposure.debit_note_total_minor` by `delta` (the new
    /// debit note's incl-tax amount) for `(tenant, invoice_id)`, bumping `version`.
    /// A debit note **raises** the invoice's headroom (design §4.3 / AC #24): the
    /// headroom CHECK is `credit_note_total_minor <= original_total_minor +
    /// debit_note_total_minor`, so `debit_note_total_minor` is on the RHS — raising
    /// it can NEVER violate the headroom CHECK (it only widens the cap available to
    /// later credit notes). The only guard on this column is the nonneg CHECK
    /// (`debit_note_total_minor >= 0`), which a positive `delta` never trips; the
    /// caller never passes a negative `delta` (a debit note only adds charge). A
    /// scoped UPDATE, not an upsert: the row is always seeded first by
    /// [`Self::seed_exposure_first_touch`] (the same first-touch the debit-note
    /// handler runs against the invoice's posted AR); `rows_affected == 0` ⇒ no
    /// exposure row (the seed must precede this — an invariant breach). SSI + retry
    /// serialize concurrent notes on the same invoice.
    ///
    /// # Errors
    /// [`RepoError::Db`] when no row matched or on any scope / storage failure
    /// (incl. the unexpected nonneg-CHECK trip a negative `delta` would cause).
    pub async fn add_debit_note_total(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        invoice_id: &str,
        delta: i64,
    ) -> Result<(), RepoError> {
        let result = invoice_exposure::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                invoice_exposure::Column::DebitNoteTotalMinor,
                Expr::col((
                    invoice_exposure::Entity,
                    invoice_exposure::Column::DebitNoteTotalMinor,
                ))
                .add(delta),
            )
            .col_expr(
                invoice_exposure::Column::Version,
                Expr::col((invoice_exposure::Entity, invoice_exposure::Column::Version)).add(1),
            )
            .filter(
                Condition::all()
                    .add(invoice_exposure::Column::TenantId.eq(tenant))
                    .add(invoice_exposure::Column::InvoiceId.eq(invoice_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("bump debit_note_total_minor: {e}")))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "invoice_exposure row absent for ({tenant}, {invoice_id}) — not seeded"
            )));
        }
        Ok(())
    }

    /// Insert the `credit_note` record row (`(tenant, credit_note_id)` PK). Runs in
    /// the post txn after the entry is posted; a duplicate `credit_note_id` is
    /// short-circuited by the `(tenant, CREDIT_NOTE, credit_note_id)` idempotency
    /// claim BEFORE the sidecar (a replay returns before this), so an unexpected
    /// PK collision surfaces as [`RepoError::Db`] and rolls the post back.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn insert_credit_note(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        note: &NewCreditNote,
    ) -> Result<(), RepoError> {
        let am = credit_note::ActiveModel {
            tenant_id: Set(note.tenant_id),
            credit_note_id: Set(note.credit_note_id.clone()),
            origin_invoice_id: Set(note.origin_invoice_id.clone()),
            origin_invoice_item_ref: Set(note.origin_invoice_item_ref.clone()),
            revenue_stream: Set(note.revenue_stream.clone()),
            currency: Set(note.currency.clone()),
            amount_minor: Set(note.amount_minor),
            recognized_part_minor: Set(note.recognized_part_minor),
            deferred_part_minor: Set(note.deferred_part_minor),
            split_basis_ref: Set(note.split_basis_ref.clone()),
            reason_code: Set(note.reason_code.clone()),
            created_at_utc: Set(note.created_at_utc),
        };
        credit_note::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("credit_note scope: {e}")))?
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert credit_note: {e}")))?;
        Ok(())
    }

    /// Insert the `debit_note` record row (`(tenant, debit_note_id)` PK). Runs in
    /// the post txn after the entry is posted; a duplicate `debit_note_id` is
    /// short-circuited by the `(tenant, DEBIT_NOTE, debit_note_id)` idempotency
    /// claim BEFORE the sidecar (a replay returns before this), so an unexpected PK
    /// collision surfaces as [`RepoError::Db`] and rolls the post back. Mirrors
    /// [`Self::insert_credit_note`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn insert_debit_note(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        note: &NewDebitNote,
    ) -> Result<(), RepoError> {
        let am = debit_note::ActiveModel {
            tenant_id: Set(note.tenant_id),
            debit_note_id: Set(note.debit_note_id.clone()),
            origin_invoice_id: Set(note.origin_invoice_id.clone()),
            currency: Set(note.currency.clone()),
            amount_minor: Set(note.amount_minor),
            recognized_part_minor: Set(note.recognized_part_minor),
            deferred_part_minor: Set(note.deferred_part_minor),
            created_at_utc: Set(note.created_at_utc),
        };
        debit_note::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("debit_note scope: {e}")))?
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert debit_note: {e}")))?;
        Ok(())
    }

    /// Insert the `refund` record row (surrogate `(tenant, refund_id)` PK; design
    /// §4.4). Runs in the post txn after the refund's stage entry is posted. The
    /// engine's `(tenant, REFUND, psp_refund_id:phase)` idempotency claim
    /// short-circuits a replay BEFORE the sidecar (a replay returns before this),
    /// so an unexpected collision — on either the surrogate PK or the natural
    /// `UNIQUE (tenant, psp_refund_id, phase)` index — surfaces as
    /// [`RepoError::Db`] and rolls the post back. Mirrors
    /// [`Self::insert_credit_note`]; the `version` column defaults to 0.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope, UNIQUE/PK collision, or storage failure.
    pub async fn insert_refund(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        rf: &NewRefund,
    ) -> Result<(), RepoError> {
        let am = refund::ActiveModel {
            tenant_id: Set(rf.tenant_id),
            refund_id: Set(rf.refund_id.clone()),
            psp_refund_id: Set(rf.psp_refund_id.clone()),
            phase: Set(rf.phase.clone()),
            pattern: Set(rf.pattern.clone()),
            payment_id: Set(rf.payment_id.clone()),
            invoice_id: Set(rf.invoice_id.clone()),
            currency: Set(rf.currency.clone()),
            amount_minor: Set(rf.amount_minor),
            clearing_state: Set(rf.clearing_state.clone()),
            relates_to_refund_id: Set(rf.relates_to_refund_id.clone()),
            reverses_entry_id: Set(rf.reverses_entry_id),
            created_at_utc: Set(rf.created_at_utc),
            version: Set(0),
        };
        refund::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("refund scope: {e}")))?
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert refund: {e}")))?;
        Ok(())
    }

    /// Read the `refund` record row for `(tenant, refund_id)` — the
    /// `GET /refunds/{refundId}` source (Group G). The surrogate PK is
    /// `(tenant_id, refund_id)`, so this yields exactly one row (the latest phase
    /// stamped on that surrogate id — the handler writes one `refund` row per
    /// `refund_id`, advancing its `phase` / `clearing_state` as the PSP lifecycle
    /// progresses). Returns `None` when no refund with that id exists for the
    /// tenant. Scoped (SQL-level BOLA — a foreign tenant yields `None`, the same
    /// 404 as absent, no existence leak). Out-of-txn on a fresh scoped connection
    /// (a pure read). Mirrors [`Self::read_exposure_out_of_txn`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_refund_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        refund_id: &str,
    ) -> Result<Option<refund::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        refund::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(refund::Column::TenantId.eq(tenant))
                    .add(refund::Column::RefundId.eq(refund_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read refund: {e}")))
    }

    /// List the `refund` record rows for `tenant` under `scope`, cursor-paginated
    /// via the canonical `query` (`$filter` over `payment_id` / `psp_refund_id` /
    /// `phase` / `pattern` / `clearing_state` / `invoice_id`, `$orderby` / `limit` /
    /// `cursor`). The tenant predicate is pre-applied to the secured select; the
    /// user `$filter` is additive over it (SQL-level BOLA — a foreign value still
    /// ANDs the scope, so a cross-tenant refund never leaks). A bare list defaults
    /// to `refund_id ASC`. The `GET /refunds` read-surface source; out-of-txn on a
    /// fresh scoped connection. Mirrors `JournalRepo::list_lines`.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on a storage / connection failure;
    /// [`OdataPageError::Odata`] on a malformed `$filter` / `$orderby` / cursor
    /// (the caller projects it to a canonical 400).
    pub async fn list_refunds(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<refund::Model>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("conn: {e}")))?;
        // Pre-apply the tenant predicate to the secured select; the user `$filter`
        // is applied additively by `paginate_odata` (it never replaces this scope —
        // BOLA preserved).
        let base_select = refund::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(refund::Column::TenantId.eq(tenant)));
        let query = query_with_default_order(query, "refund_id");
        paginate_odata::<RefundFilterField, RefundODataMapper, refund::Entity, refund::Model, _, _>(
            base_select,
            &conn,
            &query,
            ("refund_id", SortDir::Asc),
            LimitCfg {
                default: 25,
                max: 200,
            },
            |m| m,
        )
        .await
        .map_err(map_odata_err)
    }

    /// Read the `credit_note` record row for `(tenant, credit_note_id)` — the
    /// `GET /credit-notes/{creditNoteId}` source (read surface R2). The PK is
    /// `(tenant_id, credit_note_id)`, so this yields at most one row. Returns
    /// `None` when no credit note with that id exists for the tenant. Scoped
    /// (SQL-level BOLA — a foreign tenant yields `None`, the same 404 as absent, no
    /// existence leak). Out-of-txn on a fresh scoped connection (a pure read).
    /// Mirrors [`Self::read_refund_out_of_txn`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_credit_note_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        credit_note_id: &str,
    ) -> Result<Option<credit_note::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        credit_note::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(credit_note::Column::TenantId.eq(tenant))
                    .add(credit_note::Column::CreditNoteId.eq(credit_note_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read credit_note: {e}")))
    }

    /// List the `credit_note` record rows for `tenant` under `scope`,
    /// cursor-paginated via the canonical `query` (`$filter` over
    /// `origin_invoice_id` / `revenue_stream` / `reason_code`, `$orderby` / `limit`
    /// / `cursor`). The tenant predicate is pre-applied to the secured select; the
    /// user `$filter` is additive over it (SQL-level BOLA — a foreign value still
    /// ANDs the scope, so a cross-tenant credit note never leaks). A bare list
    /// defaults to `credit_note_id ASC`. The `GET /credit-notes` read-surface
    /// source; out-of-txn on a fresh scoped connection. Mirrors
    /// [`Self::list_refunds`].
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on a storage / connection failure;
    /// [`OdataPageError::Odata`] on a malformed `$filter` / `$orderby` / cursor
    /// (the caller projects it to a canonical 400).
    pub async fn list_credit_notes(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<credit_note::Model>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("conn: {e}")))?;
        // Pre-apply the tenant predicate to the secured select; the user `$filter`
        // is applied additively by `paginate_odata` (it never replaces this scope —
        // BOLA preserved).
        let base_select = credit_note::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(credit_note::Column::TenantId.eq(tenant)));
        let query = query_with_default_order(query, "credit_note_id");
        paginate_odata::<
            CreditNoteFilterField,
            CreditNoteODataMapper,
            credit_note::Entity,
            credit_note::Model,
            _,
            _,
        >(
            base_select,
            &conn,
            &query,
            ("credit_note_id", SortDir::Asc),
            LimitCfg {
                default: 25,
                max: 200,
            },
            |m| m,
        )
        .await
        .map_err(map_odata_err)
    }

    /// Read the `debit_note` record row for `(tenant, debit_note_id)` — the
    /// `GET /debit-notes/{debitNoteId}` source (read surface R2). The PK is
    /// `(tenant_id, debit_note_id)`, so this yields at most one row. Returns `None`
    /// when no debit note with that id exists for the tenant. Scoped (SQL-level
    /// BOLA — a foreign tenant yields `None`, the same 404 as absent, no existence
    /// leak). Out-of-txn on a fresh scoped connection (a pure read). Mirrors
    /// [`Self::read_credit_note_out_of_txn`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_debit_note_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        debit_note_id: &str,
    ) -> Result<Option<debit_note::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        debit_note::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(debit_note::Column::TenantId.eq(tenant))
                    .add(debit_note::Column::DebitNoteId.eq(debit_note_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read debit_note: {e}")))
    }

    /// List the `debit_note` record rows for `tenant` under `scope`,
    /// cursor-paginated via the canonical `query` (`$filter` over
    /// `origin_invoice_id`, `$orderby` / `limit` / `cursor`). The tenant predicate
    /// is pre-applied to the secured select; the user `$filter` is additive over it
    /// (SQL-level BOLA — a foreign value still ANDs the scope, so a cross-tenant
    /// debit note never leaks). A bare list defaults to `debit_note_id ASC`. The
    /// `GET /debit-notes` read-surface source; out-of-txn on a fresh scoped
    /// connection. Mirrors [`Self::list_credit_notes`].
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on a storage / connection failure;
    /// [`OdataPageError::Odata`] on a malformed `$filter` / `$orderby` / cursor
    /// (the caller projects it to a canonical 400).
    pub async fn list_debit_notes(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<debit_note::Model>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("conn: {e}")))?;
        // Pre-apply the tenant predicate to the secured select; the user `$filter`
        // is applied additively by `paginate_odata` (it never replaces this scope —
        // BOLA preserved).
        let base_select = debit_note::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(debit_note::Column::TenantId.eq(tenant)));
        let query = query_with_default_order(query, "debit_note_id");
        paginate_odata::<
            DebitNoteFilterField,
            DebitNoteODataMapper,
            debit_note::Entity,
            debit_note::Model,
            _,
            _,
        >(
            base_select,
            &conn,
            &query,
            ("debit_note_id", SortDir::Asc),
            LimitCfg {
                default: 25,
                max: 200,
            },
            |m| m,
        )
        .await
        .map_err(map_odata_err)
    }

    /// Read the `refund` row on the natural `(tenant, psp_refund_id, phase)` grain —
    /// the live state of ONE PSP-refund phase (Z5-4). A single PSP refund advances
    /// through several phase rows (`initiated`, `confirmed`, `rejected`/`voided`),
    /// each its own row under the natural `UNIQUE (tenant, psp_refund_id, phase)`
    /// index, so this yields at most one row. The `unknown_final` disposition uses it
    /// to read the STAGE-1 (`initiated`) row's REAL `clearing_state` + `amount_minor`
    /// (the open clearing it writes off) rather than assuming a hardcoded
    /// `PENDING` / request amount: if the stage-1 already `SETTLED` (stage-2 drained)
    /// or `REVERSED` (a reject/void landed) the clearing is NOT open, so the
    /// disposition must be a no-op rather than an over-DR. Returns `None` when no row
    /// exists for that `(psp_refund_id, phase)`. Scoped (SQL-level BOLA). Out-of-txn
    /// on a fresh scoped connection.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_refund_by_psp_phase(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        psp_refund_id: &str,
        phase: &str,
    ) -> Result<Option<refund::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        refund::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(refund::Column::TenantId.eq(tenant))
                    .add(refund::Column::PspRefundId.eq(psp_refund_id))
                    .add(refund::Column::Phase.eq(phase)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read refund by psp_refund_id/phase: {e}")))
    }
}

/// Map a counter-write [`ScopeError`] to [`RepoError`]: a CHECK-constraint
/// violation (the `invoice_exposure` headroom cap) becomes
/// [`RepoError::MoneyOutCapExceeded`] (the handler refines it to
/// `CreditNoteExceedsHeadroom`); anything else stays [`RepoError::Db`]. Mirrors
/// `PaymentRepo::map_cap_violation` / `RecognitionRepo::map_cap_violation` with the
/// `invoice_exposure` constraint-name prefix.
fn map_headroom_violation(context: &str, err: &ScopeError) -> RepoError {
    if let ScopeError::Db(db_err) = err
        && is_headroom_check_violation(db_err)
    {
        return RepoError::MoneyOutCapExceeded(format!("{context}: {err}"));
    }
    RepoError::Db(format!("{context}: {err}"))
}

/// `true` iff `err` is a CHECK-constraint violation, matched first by the
/// `invoice_exposure` constraint-name prefix and then by the SQLSTATE-anchored
/// fallbacks (Postgres `23514`, `SQLite` extended code `275`). Mirrors
/// `RecognitionRepo::is_check_violation` (a structured `sql_err()` is never a
/// CHECK, so an unstructured error is required).
fn is_headroom_check_violation(err: &DbErr) -> bool {
    if err.sql_err().is_some() {
        return false;
    }
    let msg = err.to_string().to_lowercase();
    if msg.contains("chk_ledger_invoice_exposure_") {
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
