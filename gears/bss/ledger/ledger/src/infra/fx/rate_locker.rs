//! `RateLocker` — translate + snapshot + stamp (design §4.2 / §4.5): for a
//! cross-currency entry, resolve the lock-ready rate
//! ([`RateSource`](super::rate_source::RateSource)), freeze it in a
//! `ledger_fx_rate_snapshot` ([`FxRepo::insert_snapshot`]), translate every line
//! to the functional currency at that single locked rate
//! ([`translate_entry`](crate::domain::fx::translate::translate_entry)), and
//! stamp the functional columns (`functional_amount_minor` /
//! `functional_currency` / `rate_snapshot_ref`) onto the lines in place.
//!
//! A single-currency entry (`transaction_ccy == functional_ccy`) needs no FX: the
//! functional columns stay NULL and no snapshot is written.
//!
//! **Wired into the live S1 (invoice/post) and S2 (settle) posting paths.** Each
//! caller drives this gated on `functional_ccy.is_some() && fc != entry_currency`,
//! so a single-currency tenant (no functional currency, or one equal to the entry
//! currency) is unaffected — no snapshot, functional columns stay NULL. The
//! cross-currency path is exercised by the controller's testcontainer test (the
//! snapshot insert needs a database); the single-currency short-circuit is
//! unit-tested here.

use bss_ledger_sdk::AccountClass;

use toolkit_db::secure::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::fx::translate::{FxLine, FxTranslateError, translate_entry};
use crate::domain::model::NewLine;
use crate::infra::fx::rate_source::RateSource;
use crate::infra::storage::repo::{FxRepo, NewRateSnapshot};
use time::OffsetDateTime;

/// Resolves + locks the FX rate for an entry and stamps the functional columns.
#[derive(Clone)]
pub struct RateLocker {
    source: RateSource,
    repo: FxRepo,
}

impl RateLocker {
    #[must_use]
    pub fn new(source: RateSource, repo: FxRepo) -> Self {
        Self { source, repo }
    }

    /// Lock an FX rate for the entry and stamp the functional translation onto
    /// `lines`, or do nothing for a single-currency entry.
    ///
    /// - `transaction_ccy == functional_ccy` (single-currency): returns
    ///   `Ok(None)` and leaves every line's functional columns NULL — there is no
    ///   translation to do and no snapshot to write.
    /// - else (cross-currency): resolves the rate for `transaction_ccy →
    ///   functional_ccy`, inserts a `ledger_fx_rate_snapshot` from it (`rate_id`),
    ///   translates every line at the locked `rate_micro` with the per-entry
    ///   rounding residual closed onto the AR anchor (the first `AccountClass::Ar`
    ///   line, else line 0), and sets each line's `functional_amount_minor` /
    ///   `functional_currency` / `rate_snapshot_ref`. Returns `Ok(Some(rate_id))`.
    ///
    /// # Errors
    /// - [`DomainError::FxRateUnavailable`] / [`DomainError::FxRateStaleNotAllowed`]
    ///   propagated from [`RateSource::resolve`].
    /// - [`DomainError::Internal`] on a snapshot-insert failure, or when the pure
    ///   translation rejects the input (a residual/anchor/overflow misuse — see
    ///   [`map_translate_err`]).
    pub async fn lock_and_stamp(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        lines: &mut [NewLine],
        transaction_ccy: &str,
        functional_ccy: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Uuid>, DomainError> {
        // Single-currency: nothing to translate, functional columns stay NULL.
        if transaction_ccy == functional_ccy {
            return Ok(None);
        }

        // Resolve the lock-ready rate (transaction → functional) and freeze it.
        let resolved = self
            .source
            .resolve(scope, tenant, transaction_ccy, functional_ccy, now)
            .await?;
        let rate_id = self
            .repo
            .insert_snapshot(
                scope,
                &NewRateSnapshot {
                    tenant_id: tenant,
                    base_currency: transaction_ccy.to_owned(),
                    quote_currency: functional_ccy.to_owned(),
                    rate_micro: resolved.rate_micro,
                    as_of: resolved.as_of,
                    provider: resolved.provider.clone(),
                    stale: resolved.stale,
                    fallback_order: resolved.fallback_order,
                    triangulated_via: resolved.triangulated_via.clone(),
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("fx snapshot insert: {e}")))?;

        // Translate every line at the single locked rate, closing the per-entry
        // functional rounding residual onto the AR anchor (the leg whose
        // functional dwarfs a ≤ lines−1 minor-unit residual). No AR line → anchor
        // line 0 (the residual plug still balances the functional column; the
        // anchor must merely be a substantial real line).
        let fxlines: Vec<FxLine> = lines
            .iter()
            .map(|l| FxLine {
                amount_minor: l.amount_minor,
                side: l.side,
            })
            .collect();
        let anchor = lines
            .iter()
            .position(|l| l.account_class == AccountClass::Ar)
            .unwrap_or(0);
        let func = translate_entry(&fxlines, resolved.rate_micro, anchor)
            .map_err(|e| map_translate_err(&e))?;

        // Stamp the functional columns in place (one functional amount per line,
        // input order). NOTE: the per-line `rate_snapshot_ref` FK (journal_line →
        // fx_rate_snapshot) is NOT carried on `NewLine` — one rate per entry (§4.3),
        // so it rides the entry header (`NewEntry.rate_snapshot_ref`): the live
        // S1/S2 hook sets it from the `rate_id` returned here, and the journal repo
        // stamps it onto every line on insert. The snapshot id is returned to the
        // caller for that purpose.
        for (line, func_amount) in lines.iter_mut().zip(func) {
            line.functional_amount_minor = Some(func_amount);
            line.functional_currency = Some(functional_ccy.to_owned());
        }

        Ok(Some(rate_id))
    }
}

/// Map a pure [`FxTranslateError`] to a [`DomainError`]. Every variant is a
/// translate **misuse** the caller should not reach with a balanced entry and a
/// positive locked rate (a non-positive rate, an out-of-bounds anchor, a residual
/// that would drive the anchor non-positive, or an out-of-range product) — the
/// rate itself was already accepted by `RateSource`, so these are internal
/// invariant breaches, not a "no acceptable rate" condition. They therefore map
/// to [`DomainError::Internal`] (a 500 whose diagnostic stays server-side), NOT
/// `FxRateUnavailable` (which means the store had no usable quote).
#[must_use]
fn map_translate_err(e: &FxTranslateError) -> DomainError {
    DomainError::Internal(format!("fx translation rejected the entry: {e}"))
}

#[cfg(test)]
#[path = "rate_locker_tests.rs"]
mod rate_locker_tests;
