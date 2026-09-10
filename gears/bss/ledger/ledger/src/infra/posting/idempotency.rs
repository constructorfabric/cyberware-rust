//! `IdempotencyGate` — the at-most-once posting gate over
//! `bss.ledger_idempotency_dedup`. `claim` performs an `INSERT … ON CONFLICT
//! (tenant_id, flow, business_id) DO NOTHING` inside the caller's
//! transaction, then reads the row back: a fresh insert yields
//! [`ClaimOutcome::Claimed`], a conflict yields [`ClaimOutcome::Replay`]
//! carrying the stored row (incl. `payload_hash`, so the caller can map a
//! differing hash to `IDEMPOTENCY_PAYLOAD_CONFLICT`). `finalize` stamps the
//! result entry + sequence and flips the status to `POSTED` before COMMIT,
//! so a concurrent replay reads a complete reference.
//!
//! `payload_hash` uses the FIPS-validated `aws-lc-rs` SHA-256 — the same
//! crypto provider the platform installs at bootstrap — so no non-FIPS
//! hasher enters the graph (DE0708 clean).

use aws_lc_rs::digest::{SHA256, digest as sha256};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, DbErr, EntityTrait};
use toolkit_db::secure::{
    AccessScope, DbTx, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::model::{NewEntry, NewLine, RepoError};
use crate::infra::storage::entity::idempotency_dedup;
use time::OffsetDateTime;

/// Status literal written when a posting first claims a key.
const STATUS_CLAIMED: &str = "CLAIMED";
/// Status literal written once the posting is durably recorded.
pub(crate) const STATUS_POSTED: &str = "POSTED";
/// Idempotency-dedup status for a request whose effect was durably enqueued
/// onto `ledger_pending_event_queue` for a later (separate-transaction) apply,
/// rather than posted inline. The dedup column is `text` with no CHECK, so this
/// literal needs no migration. Consumed by `claim_queued` (this module) and the
/// queue repo's intake insert.
pub(crate) const STATUS_QUEUED: &str = "QUEUED";

/// The stored dedup row, returned on a replay so the caller can compare the
/// recorded `payload_hash` and read the prior posting reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostingRefRow {
    pub payload_hash: String,
    pub result_entry_id: Option<Uuid>,
    pub status: String,
}

/// Result of a [`IdempotencyGate::claim`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This call won the race and now owns the posting.
    Claimed,
    /// The key was already present; the caller must compare `payload_hash`
    /// and, if it matches, return the prior reference as a replay.
    Replay(PostingRefRow),
}

/// At-most-once posting gate over `idempotency_dedup`.
#[derive(Clone, Default)]
pub struct IdempotencyGate;

impl IdempotencyGate {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Claim `(tenant, flow, business_id)` inside `txn` for an **inline post**
    /// (seed status `CLAIMED`) via an `INSERT … ON CONFLICT DO NOTHING`, then
    /// read the row back. Delegates to [`Self::claim_with_status`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage failure, or [`RepoError::RowVanished`]
    /// if the dedup row cannot be read back inside the same transaction.
    pub async fn claim(
        &self,
        txn: &DbTx<'_>,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
        payload_hash: &str,
    ) -> Result<ClaimOutcome, RepoError> {
        self.claim_with_status(txn, tenant, flow, business_id, payload_hash, STATUS_CLAIMED)
            .await
    }

    /// Claim `(tenant, flow, business_id)` inside `txn` for a **deferred-apply
    /// enqueue** (seed status `QUEUED`) — the work is durably queued onto
    /// `ledger_pending_event_queue` for a later, separate-transaction apply
    /// rather than posted inline. Same `INSERT … ON CONFLICT DO NOTHING` +
    /// read-back as [`Self::claim`]; only the seeded status differs. On a
    /// replay the returned [`PostingRefRow::status`] lets the caller tell a
    /// still-`QUEUED` intake from a `CLAIMED` (in-flight inline) or `POSTED`
    /// (finalized) one. Delegates to [`Self::claim_with_status`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage failure, or [`RepoError::RowVanished`]
    /// if the dedup row cannot be read back inside the same transaction.
    pub async fn claim_queued(
        &self,
        txn: &DbTx<'_>,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
        payload_hash: &str,
    ) -> Result<ClaimOutcome, RepoError> {
        self.claim_with_status(txn, tenant, flow, business_id, payload_hash, STATUS_QUEUED)
            .await
    }

    /// Shared body for [`Self::claim`] / [`Self::claim_queued`]: an
    /// `INSERT … ON CONFLICT (tenant_id, flow, business_id) DO NOTHING` inside
    /// `txn` seeding `status = seed_status`, then a read-back. A fresh insert
    /// yields [`ClaimOutcome::Claimed`]; a conflict yields
    /// [`ClaimOutcome::Replay`] carrying the stored row (the caller compares
    /// `payload_hash` and reads `status` / `result_entry_id`).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage failure, or [`RepoError::RowVanished`]
    /// if the dedup row cannot be read back inside the same transaction.
    async fn claim_with_status(
        &self,
        txn: &DbTx<'_>,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
        payload_hash: &str,
        seed_status: &str,
    ) -> Result<ClaimOutcome, RepoError> {
        let scope = AccessScope::for_tenant(tenant);

        let am = idempotency_dedup::ActiveModel {
            tenant_id: Set(tenant),
            flow: Set(flow.to_owned()),
            business_id: Set(business_id.to_owned()),
            payload_hash: Set(payload_hash.to_owned()),
            result_entry_id: Set(None),
            posted_at_utc: Set(None),
            status: Set(seed_status.to_owned()),
            retain_until: Set(None),
        };
        let on_conflict = OnConflict::columns([
            idempotency_dedup::Column::TenantId,
            idempotency_dedup::Column::Flow,
            idempotency_dedup::Column::BusinessId,
        ])
        .do_nothing()
        .to_owned();

        let inserted = match idempotency_dedup::Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)
            .map_err(|e| RepoError::Db(format!("idempotency claim scope: {e}")))?
            .on_conflict_raw(on_conflict)
            .exec(txn)
            .await
        {
            Ok(_) => true,
            // The key already existed; the conflict swallowed the insert.
            Err(ScopeError::Db(DbErr::RecordNotInserted)) => false,
            Err(e) => return Err(RepoError::Db(format!("idempotency claim: {e}"))),
        };

        if inserted {
            return Ok(ClaimOutcome::Claimed);
        }

        let row = idempotency_dedup::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(idempotency_dedup::Column::TenantId.eq(tenant))
                    .add(idempotency_dedup::Column::Flow.eq(flow))
                    .add(idempotency_dedup::Column::BusinessId.eq(business_id)),
            )
            .one(txn)
            .await
            .map_err(|e| RepoError::Db(format!("idempotency read-back: {e}")))?
            .ok_or_else(|| {
                RepoError::RowVanished(format!("idempotency_dedup {tenant}/{flow}/{business_id}"))
            })?;

        Ok(ClaimOutcome::Replay(PostingRefRow {
            payload_hash: row.payload_hash,
            result_entry_id: row.result_entry_id,
            status: row.status,
        }))
    }

    /// In-transaction scoped read of the dedup row for `(tenant, flow,
    /// business_id)`, or `None` when absent — the read-back half of
    /// [`Self::claim_with_status`] WITHOUT the preceding insert. The
    /// queued-apply post path ([`crate::infra::posting::service::PostingService`]
    /// `ClaimMode::QueuedApply`) calls this instead of `claim`: the dedup row was
    /// already claimed `QUEUED` at intake, so a re-claim here would either be a
    /// no-op (the `INSERT … ON CONFLICT DO NOTHING` collides) and waste a round
    /// trip, or — worse — mis-read the row as a fresh `Claimed`. Reading lets the
    /// engine distinguish a still-`QUEUED` row (proceed to post + finalize) from
    /// an already-`POSTED` one (idempotent replay) inside the same serializable
    /// transaction that will finalize it. Scoped (`.secure().scope_with`) for
    /// SQL-level BOLA, exactly like the `claim` read-back.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read(
        &self,
        txn: &DbTx<'_>,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
    ) -> Result<Option<PostingRefRow>, RepoError> {
        let scope = AccessScope::for_tenant(tenant);
        let row = idempotency_dedup::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(idempotency_dedup::Column::TenantId.eq(tenant))
                    .add(idempotency_dedup::Column::Flow.eq(flow))
                    .add(idempotency_dedup::Column::BusinessId.eq(business_id)),
            )
            .one(txn)
            .await
            .map_err(|e| RepoError::Db(format!("idempotency read: {e}")))?;
        Ok(row.map(|row| PostingRefRow {
            payload_hash: row.payload_hash,
            result_entry_id: row.result_entry_id,
            status: row.status,
        }))
    }

    /// Stamp the result entry id + posted timestamp and flip the status to
    /// `POSTED` before COMMIT, so a concurrent replay reads a complete
    /// reference. `created_seq` is the journal header's monotonic sequence
    /// (owned by `journal_entry`); it is threaded through for the public
    /// contract and asserted positive — the dedup row keys the posting by
    /// `result_entry_id`, not by the sequence.
    ///
    /// # Panics
    /// In debug builds if `created_seq` is not positive (the header must be
    /// sequenced before finalize).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage failure, or [`RepoError::RowVanished`]
    /// if the claimed dedup row could not be updated inside the transaction.
    pub async fn finalize(
        &self,
        txn: &DbTx<'_>,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
        entry_id: Uuid,
        created_seq: i64,
    ) -> Result<(), RepoError> {
        debug_assert!(created_seq > 0, "finalize before the header was sequenced");
        let scope = AccessScope::for_tenant(tenant);

        let result = idempotency_dedup::Entity::update_many()
            .secure()
            .scope_with(&scope)
            .col_expr(
                idempotency_dedup::Column::ResultEntryId,
                Expr::value(Some(entry_id)),
            )
            .col_expr(
                idempotency_dedup::Column::PostedAtUtc,
                Expr::value(Some(OffsetDateTime::now_utc())),
            )
            .col_expr(
                idempotency_dedup::Column::Status,
                Expr::value(STATUS_POSTED.to_owned()),
            )
            .filter(
                Condition::all()
                    .add(idempotency_dedup::Column::TenantId.eq(tenant))
                    .add(idempotency_dedup::Column::Flow.eq(flow))
                    .add(idempotency_dedup::Column::BusinessId.eq(business_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("idempotency finalize: {e}")))?;

        if result.rows_affected == 0 {
            return Err(RepoError::RowVanished(format!(
                "idempotency_dedup {tenant}/{flow}/{business_id}"
            )));
        }
        Ok(())
    }

    /// FIPS-validated (`aws-lc-rs`) SHA-256 hex digest over the canonical
    /// financial content of an entry — the business key, effective date,
    /// period, and every per-line financial dimension (amount + scale,
    /// functional amount/currency, currency, side, payer/seller, invoice,
    /// due date, revenue stream, tax dims, credit-grant event type, AR dispute
    /// status), sorted for order-independence.
    /// Transport/envelope fields (correlation id, actor, posted-at, internal
    /// ids) are excluded so the same financial intent always hashes
    /// identically; a change in any financial dimension flips the hash so a
    /// conflicting reuse of the business key is caught, not swallowed.
    #[must_use]
    pub fn payload_hash(entry: &NewEntry, lines: &[NewLine]) -> String {
        let mut canon = String::new();
        canon.push_str(entry.source_doc_type.as_str());
        canon.push('\u{1f}');
        canon.push_str(&entry.source_business_id);
        canon.push('\u{1f}');
        canon.push_str(&entry.entry_currency);
        canon.push('\u{1f}');
        canon.push_str(&entry.effective_at.to_string());
        canon.push('\u{1f}');
        canon.push_str(&entry.period_id);
        canon.push('\u{1e}');

        let mut line_keys: Vec<String> = lines
            .iter()
            .map(|l| {
                [
                    l.account_id.to_string(),
                    l.account_class.as_str().to_owned(),
                    l.amount_minor.to_string(),
                    l.currency.clone(),
                    l.currency_scale.to_string(),
                    l.side.as_str().to_owned(),
                    l.payer_tenant_id.to_string(),
                    l.seller_tenant_id
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    l.invoice_id.clone().unwrap_or_default(),
                    l.due_date.map(|d| d.to_string()).unwrap_or_default(),
                    l.revenue_stream.clone().unwrap_or_default(),
                    l.functional_amount_minor
                        .map(|a| a.to_string())
                        .unwrap_or_default(),
                    l.functional_currency.clone().unwrap_or_default(),
                    l.tax_jurisdiction.clone().unwrap_or_default(),
                    l.tax_filing_period.clone().unwrap_or_default(),
                    // As-posted financial dimensions: the wallet sub-grain bucket
                    // (REUSABLE_CREDIT lines) and the AR dispute sub-class
                    // (chargeback reclass). A business-key reuse differing only in
                    // these is a conflict, not a replay — include them so the hash
                    // flips and `IDEMPOTENCY_PAYLOAD_CONFLICT` fires.
                    l.credit_grant_event_type.clone().unwrap_or_default(),
                    l.ar_status.clone().unwrap_or_default(),
                ]
                .join("\u{1f}")
            })
            .collect();
        line_keys.sort();
        canon.push_str(&line_keys.join("\u{1e}"));

        Self::content_hash(&canon)
    }

    /// FIPS-validated (`aws-lc-rs`) SHA-256 hex digest over an arbitrary
    /// already-canonical string — the same crypto provider [`Self::payload_hash`]
    /// runs, factored out so a caller that hashes a *request* payload (not an
    /// entry) can reuse it. A queued allocation hashes its request payload here:
    /// the per-invoice split is only decided at apply (Group D), so the entry —
    /// and thus `payload_hash`'s entry-based signature — does not yet exist at
    /// intake. The caller is responsible for canonicalizing `canonical` stably
    /// (e.g. a fixed field order) so the same intent always hashes identically.
    #[must_use]
    pub(crate) fn content_hash(canonical: &str) -> String {
        let digest = sha256(&SHA256, canonical.as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in digest.as_ref() {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

#[cfg(test)]
#[path = "idempotency_tests.rs"]
mod idempotency_tests;
