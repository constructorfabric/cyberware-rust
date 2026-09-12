//! `RateSource` — FX rate **resolution** (design §4.5 / F3): over the local
//! `ledger_fx_rate` candidate rows for a `(base, quote)` pair, pick the rate to
//! lock by the configured provider precedence, screening for staleness.
//!
//! The resolution is two pure decisions ([`order_index`] + [`is_stale`]) wrapped
//! around one repo read, so the money-relevant logic (ordering, staleness) is
//! unit-tested without a database. The selected provider's rank in
//! `provider_order` becomes the result's `fallback_order` (0 = the primary
//! provider).
//!
//! **v1 scope.**
//! - Direct pairs only — triangulation (design §4.6, e.g. via EUR) is deferred.
//! - The stale fallback is FORBIDDEN: if every candidate is stale the resolve
//!   fails ([`DomainError::FxRateStaleNotAllowed`]) rather than locking a stale
//!   rate. The design's per-tenant "tenant policy explicitly allows last-good"
//!   is a follow-up (see the TODO at the all-stale branch).

use std::sync::Arc;

use toolkit_db::secure::AccessScope;
use uuid::Uuid;

use crate::config::FxConfig;
use crate::domain::error::DomainError;
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::storage::repo::FxRepo;
use time::OffsetDateTime;

/// G10 currencies — the major, deeply-liquid pairs held to the tighter
/// `stale_g10_hours` freshness window (design F3). A pair is "G10" if EITHER side
/// is in this set; everything else falls back to the (day-scale)
/// `stale_default_max_days` window.
const G10: &[&str] = &[
    "USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "NZD", "SEK", "NOK",
];

/// A resolved, lock-ready FX rate — the rate
/// [`RateLocker`](crate::infra::fx::rate_locker::RateLocker) freezes into a
/// snapshot and translates the entry at. `fallback_order` is the chosen
/// provider's rank in the configured precedence (0 = primary). `stale` is always
/// `false` in v1 (a stale-only candidate set is rejected, not returned);
/// `triangulated_via` is always `None` in v1 (direct pairs only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRate {
    pub rate_micro: i64,
    pub provider: String,
    pub as_of: OffsetDateTime,
    pub stale: bool,
    pub fallback_order: i32,
    pub triangulated_via: Option<String>,
}

/// Rank of `provider` in the configured precedence: its index in
/// `provider_order` when listed, else `provider_order.len()` (so EVERY unlisted
/// provider sorts AFTER every listed one, and ties among unlisted providers are
/// broken by the caller's stable secondary key). Pure — unit-tested without a DB.
#[must_use]
fn order_index(provider: &str, provider_order: &[String]) -> usize {
    provider_order
        .iter()
        .position(|p| p == provider)
        .unwrap_or(provider_order.len())
}

/// Whether a rate is stale: `age` (now − `as_of`) exceeds the freshness window
/// for the pair. A G10 pair (either side in [`G10`]) uses `stale_g10_hours`; any
/// other pair uses `stale_default_max_days`. A negative `age` (a future `as_of`,
/// i.e. clock skew) is never stale (`age <= window` holds). Pure — unit-tested
/// without a DB.
#[must_use]
fn is_stale(base: &str, quote: &str, age: time::Duration, cfg: &FxConfig) -> bool {
    let window = if is_g10_pair(base, quote) {
        time::Duration::hours(i64::try_from(cfg.stale_g10_hours).unwrap_or(i64::MAX))
    } else {
        time::Duration::days(i64::try_from(cfg.stale_default_max_days).unwrap_or(i64::MAX))
    };
    age > window
}

/// True when either leg of the pair is a G10 currency.
#[must_use]
fn is_g10_pair(base: &str, quote: &str) -> bool {
    G10.contains(&base) || G10.contains(&quote)
}

/// Resolves the lock-ready rate for a currency pair over the local rate store.
#[derive(Clone)]
pub struct RateSource {
    repo: FxRepo,
    cfg: FxConfig,
    /// Optional metrics sink for the provider-fallback counter
    /// (`ledger_fx_provider_fallback_total{provider}`). `None` on the no-metrics
    /// constructions (unit tests); wired via [`Self::with_metrics`] on the live
    /// lock paths (S1 invoice-post, S2 settle) and the revaluation runner.
    metrics: Option<Arc<dyn LedgerMetricsPort>>,
}

impl RateSource {
    #[must_use]
    pub fn new(repo: FxRepo, cfg: FxConfig) -> Self {
        Self {
            repo,
            cfg,
            metrics: None,
        }
    }

    /// Attach a metrics sink so a fallback resolve (a non-primary provider chosen)
    /// increments `ledger_fx_provider_fallback_total{provider}`. Builder-style to
    /// match the gear's `with_metrics` convention; a `RateSource` without it simply
    /// records no fallback metric (the `fallback_order` is still stamped on the
    /// snapshot either way).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn LedgerMetricsPort>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Resolve the rate to lock for `(base → quote)` as of `now`.
    ///
    /// Reads every provider's `ledger_fx_rate` row for the pair, orders them by
    /// the configured provider precedence (a provider's index in
    /// `provider_order`; an unlisted provider sorts AFTER every listed one, ties
    /// broken stably by `fallback_order` then `provider`), and returns the FIRST
    /// non-stale candidate — its precedence rank stamped as `fallback_order`
    /// (0 = primary).
    ///
    /// **Triangulation via EUR (design §4.6) is deferred — only direct pairs in
    /// v1.** A missing direct pair is unavailable, not synthesized from two legs.
    ///
    /// # Errors
    /// - [`DomainError::FxRateUnavailable`] when NO candidate row exists for the
    ///   pair (no provider has quoted it).
    /// - [`DomainError::FxRateStaleNotAllowed`] when candidates exist but EVERY
    ///   one is stale (v1 forbids the stale fallback).
    /// - [`DomainError::Internal`] on a repo / storage failure.
    pub async fn resolve(
        &self,
        // Intentionally unused: FX rates are tenant-axis reference data, not
        // object-scoped, so there is no BOLA surface to gate here — the repo read
        // (`FxRepo::latest_rates`) re-derives `AccessScope::for_tenant(tenant)` from
        // the server-trusted `tenant` and enforces it SQL-side. Kept on the
        // signature for call-site uniformity with the other secured resolves.
        _scope: &AccessScope,
        tenant: Uuid,
        base: &str,
        quote: &str,
        now: OffsetDateTime,
    ) -> Result<ResolvedRate, DomainError> {
        // Direct pairs only — see the §4.6 triangulation deferral above.
        let mut candidates = self
            .repo
            .latest_rates(tenant, base, quote)
            .await
            .map_err(|e| DomainError::Internal(format!("fx latest_rates: {e}")))?;

        if candidates.is_empty() {
            return Err(DomainError::FxRateUnavailable(format!(
                "no FX rate in the local store for {base}->{quote} (tenant {tenant})"
            )));
        }

        // Order by configured provider precedence, then stably by the row's own
        // `fallback_order`, then `provider` (a deterministic total order so the
        // pick is reproducible even when two providers share a precedence rank —
        // e.g. both unlisted).
        candidates.sort_by(|a, b| {
            order_index(&a.provider, &self.cfg.provider_order)
                .cmp(&order_index(&b.provider, &self.cfg.provider_order))
                .then(a.fallback_order.cmp(&b.fallback_order))
                .then_with(|| a.provider.cmp(&b.provider))
        });

        // First non-stale candidate wins; its precedence rank is the result's
        // `fallback_order` (0 = primary).
        for row in &candidates {
            // Defence in depth: a rate `<= 0` is never a valid quote. The REST
            // ingest DTO rejects it, but the provider-sync upsert and the raw
            // store have no such gate, so a corrupt/zero feed row could otherwise
            // be picked here and flip the sign of (or zero out) every downstream
            // translation. Skip it like a stale row so a valid fallback can win.
            if row.rate_micro <= 0 {
                continue;
            }
            let age = now - row.as_of;
            if !is_stale(base, quote, age, &self.cfg) {
                let rank = order_index(&row.provider, &self.cfg.provider_order);
                // A non-primary pick (rank > 0) is a provider fallback: the primary
                // provider had no fresh quote for this pair. Count it by the resolved
                // provider so an FX-feed degradation is observable
                // (`ledger_fx_provider_fallback_total{provider}`, §9).
                if rank > 0
                    && let Some(metrics) = &self.metrics
                {
                    metrics.fx_provider_fallback(&row.provider);
                }
                return Ok(ResolvedRate {
                    rate_micro: row.rate_micro,
                    provider: row.provider.clone(),
                    as_of: row.as_of,
                    stale: false,
                    fallback_order: i32::try_from(rank).unwrap_or(i32::MAX),
                    triangulated_via: None,
                });
            }
        }

        // Candidates exist but all are stale. v1 forbids the stale fallback.
        // TODO(VHP-1853): per-tenant stale-allowed policy → on the configured
        // tenants, return the freshest stale candidate as
        // ResolvedRate { stale: true, .. } instead of erroring here.
        Err(DomainError::FxRateStaleNotAllowed(format!(
            "every FX rate for {base}->{quote} is stale and the stale fallback is forbidden \
             (tenant {tenant})"
        )))
    }
}

#[cfg(test)]
#[path = "rate_source_tests.rs"]
mod rate_source_tests;
