//! The FX rate-provider plugin contract (`RateProviderV1`).
//!
//! A cross-gear, GTS-versioned SDK trait (`gts.cf.bss.ledger.rate-provider.v1`,
//! mirroring [`crate::LedgerClientV1`]): an external adapter-gear (ECB primary /
//! PSP-bank fallback) implements it and registers an `Arc<dyn RateProviderV1>` in
//! the `ClientHub`; a ledger-side `RateSyncJob` pulls `fetch_latest` into the
//! local rate store. The adapter ONLY fetches — translation, triangulation, and
//! staleness all stay in the ledger. The default [`UnconfiguredRateProviderV1`]
//! is a fail-safe no-op (the store stays empty → FX-needing posts block).

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_security::SecurityContext;

/// A currency pair to fetch a rate for (ISO 4217 codes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrencyPair {
    pub base: String,
    pub quote: String,
}

/// A rate as published by a provider at a point in time. `rate_micro` is the
/// fixed-precision multiplier (functional per unit transaction × 1e6). `as_of`
/// drives the ledger's staleness rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRate {
    pub base: String,
    pub quote: String,
    pub rate_micro: i64,
    pub as_of: OffsetDateTime,
    /// The concrete upstream that published THIS rate (`"ecb"`, `"bank-x"`, …),
    /// stamped by the serving source itself.
    ///
    /// Provenance is per-rate, not per-call. A composite adapter serves one
    /// whole document from whichever source answered first — it never merges
    /// rates from several sources — so which upstream supplied a given batch
    /// varies between calls. An adapter shared by more than one caller therefore
    /// cannot answer "who served the batch you just fetched?" through a separate
    /// [`RateProviderV1::provider_id`] call without racing. Recording it on the
    /// rate keeps `ledger_fx_rate.provider` / `rate_snapshot.provider` truthful
    /// for audit regardless of call interleaving.
    pub provider: String,
}

/// A rate-provider failure. Semantic (the ledger maps it to a sync-job alarm, or
/// at lock time to `FX_RATE_UNAVAILABLE`); never an HTTP status here.
#[derive(Debug, thiserror::Error)]
pub enum RateProviderError {
    #[error("pair {base}->{quote} not published")]
    PairUnavailable { base: String, quote: String },
    #[error("provider unreachable: {0}")]
    Unreachable(String),
    #[error("upstream status {0}")]
    UpstreamStatus(u16),
    #[error("invalid pair: {0}")]
    InvalidPair(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// The FX rate-provider plugin contract — implemented out-of-gear, resolved from
/// `ClientHub` by GTS instance. See the module docs.
#[async_trait]
pub trait RateProviderV1: Send + Sync {
    /// Stable identity of the *adapter* — e.g. `"ecb"` for a single-source
    /// plugin, or the composite's configured id. E.g. "ecb", "bank-x",
    /// "psp-stripe"; `"none"` is reserved for
    /// [`UnconfiguredRateProviderV1`] and means "no adapter wired".
    ///
    /// MUST be a constant identity, not "whoever served last": per-rate
    /// provenance belongs on [`ProviderRate::provider`], because this call
    /// carries no information about which `fetch_latest` it relates to. Used for
    /// alarm/log attribution and the unconfigured-sentinel check, never to stamp
    /// a stored rate.
    fn provider_id(&self) -> &str;

    /// Fetch the latest published rates for the requested pairs — one round-trip.
    /// An adapter that publishes a whole table returns everything it has and OMITS
    /// pairs it cannot serve (the caller treats a missing pair as no acceptable
    /// rate). MUST NOT be called on the posting path — only the background
    /// `RateSyncJob` calls it (a provider outage fails the job, never a post).
    ///
    /// # Errors
    /// [`RateProviderError`] on an upstream failure.
    async fn fetch_latest(
        &self,
        ctx: &SecurityContext,
        pairs: &[CurrencyPair],
        request_id: &str,
    ) -> Result<Vec<ProviderRate>, RateProviderError>;

    /// **Reachability probe only** — "would a request to this provider get
    /// through right now?", never "would the next fetch produce usable rates?".
    ///
    /// ## What counts as reachable
    ///
    /// `Ok(())` means **the endpoint answered**. Any HTTP response is an answer,
    /// including `4xx` and `5xx`: a `503` proves the request arrived, and a `405`
    /// is a normal reply from a feed that simply does not accept the probe's
    /// method. Only a *transport* failure — DNS, connect, TLS, timeout — is
    /// `Err`, because only then did nothing get through.
    ///
    /// That line is deliberate. "Reachable" and "serving correctly" are different
    /// questions, and folding the second into this one would make `Ok(())` mean
    /// something no cheap probe can actually establish. Reading a non-2xx as
    /// unhealthy also breaks the probe on feeds that answer a `HEAD` with `405`
    /// while serving `GET` perfectly.
    ///
    /// ## What it does NOT tell you
    ///
    /// An adapter can be reachable while every real fetch fails on a malformed,
    /// empty, or wrongly-shaped body. Feed freshness is a separate signal and
    /// MUST NOT be inferred from this one: `fx_provider_last_success_timestamp`
    /// advances only on a fetch that actually parsed, and that gauge — not this
    /// probe — is what a stalled-feed alert reads.
    ///
    /// ## No default implementation, on purpose
    ///
    /// There is deliberately no default. A default delegating to `fetch_latest`
    /// gave adapters that did not override it a *parsing* health check by
    /// accident, so one method carried two different guarantees depending on the
    /// vendor, and `Ok(())` could only ever be read as the weakest of them.
    /// Every adapter now states its own probe, so the contract above is what
    /// each one implements rather than what it happens to inherit.
    ///
    /// # Errors
    /// [`RateProviderError`] when nothing got through to the provider at all.
    async fn health(
        &self,
        ctx: &SecurityContext,
        request_id: &str,
    ) -> Result<(), RateProviderError>;
}

/// Fail-safe default until a real adapter is wired: every fetch fails, so the
/// local rate store stays empty and FX-needing posts block with
/// `FX_RATE_UNAVAILABLE` (never a silent wrong rate). Mirrors the gear's
/// `AlwaysSatisfiedObligationState` / `NoopLedgerMetrics` no-op ports.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredRateProviderV1;

#[async_trait]
impl RateProviderV1 for UnconfiguredRateProviderV1 {
    // The trait returns `&str` (tied to `&self`) so a real adapter can return a
    // borrowed `&self.id` field; this no-op default happens to return a `'static`
    // literal, which clippy would prefer typed `&'static str` — but that would
    // not match the trait method signature. Allow the literal bound here.
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait signature is `-> &str` for adapters with a borrowed id; this default returns a literal"
    )]
    fn provider_id(&self) -> &str {
        "none"
    }

    async fn fetch_latest(
        &self,
        _ctx: &SecurityContext,
        _pairs: &[CurrencyPair],
        _request_id: &str,
    ) -> Result<Vec<ProviderRate>, RateProviderError> {
        Err(RateProviderError::Unreachable(
            "no FX rate adapter configured".to_owned(),
        ))
    }

    /// Always unreachable: there is no endpoint to probe. This sentinel exists
    /// precisely because nothing is wired, so reporting reachable would be a
    /// false green — the one thing the probe must never be.
    async fn health(
        &self,
        _ctx: &SecurityContext,
        _request_id: &str,
    ) -> Result<(), RateProviderError> {
        Err(RateProviderError::Unreachable(
            "no FX rate adapter configured".to_owned(),
        ))
    }
}
