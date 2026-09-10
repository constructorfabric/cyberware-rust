//! `HttpJsonRateProvider` — config-driven GET-JSON fetch + field mapping. Direct
//! pairs only — a requested pair whose base is not the feed base is omitted.

use std::borrow::Cow;

use async_trait::async_trait;
use bss_ledger_sdk::{CurrencyPair, ProviderRate, RateProviderError, RateProviderV1};
use bss_rate_provider_sdk::conversion::rate_to_micro;
use bss_rate_provider_sdk::currency::normalize_currency;
use bss_rate_provider_sdk::error::map_http_error;
use bss_rate_provider_sdk::fetch::fetch_and_parse;
use bss_rate_provider_sdk::metrics::SharedFetchMetrics;
use bss_rate_provider_sdk::publication_time::reject_future_publication_time;
use secrecy::ExposeSecret;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_http::{HttpClient, RequestBuilder};
use toolkit_security::SecurityContext;

use crate::config::{Auth, Mapping};

/// Custom header used for [`Auth::HeaderKey`].
const API_KEY_HEADER: &str = "X-API-Key";

/// Walk a dotted path (`a.b.c`) through nested JSON objects.
#[must_use]
pub fn json_lookup<'a>(value: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in dotted.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Apply the mapping to a parsed JSON document, stamping `provider` onto every
/// rate as its provenance.
///
/// An entry that fails mapping is skipped (logged individually with the reason,
/// plus an aggregate count), never fabricated. A syntactically valid document
/// from which ZERO entries map is a [`RateProviderError::Internal`] (returning
/// `Ok([])` would read as success and suppress the composite fallback).
///
/// The feed is a system boundary, so the configured `base` and every `quote` key
/// are gated on the ISO-4217 shape before they can reach a tenant's FX store: a
/// malformed quote is skipped like any other unmappable entry, while a malformed
/// configured `base` fails the whole document (it is operator config, not feed
/// data, and would taint every rate).
///
/// # Errors
/// [`RateProviderError::Internal`] if the `rates`/`as_of` paths are absent, the
/// configured `base` is not ISO-4217-shaped, or no entry maps.
pub fn map_json_document(
    body: &Value,
    mapping: &Mapping,
    provider: &str,
) -> Result<Vec<ProviderRate>, RateProviderError> {
    let base = normalize_currency(&mapping.base).ok_or_else(|| {
        RateProviderError::Internal(format!(
            "configured mapping.base '{}' is not an ISO-4217-shaped currency code",
            mapping.base
        ))
    })?;
    let as_of_raw = json_lookup(body, &mapping.as_of)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RateProviderError::Internal(format!("as_of path '{}' missing", mapping.as_of))
        })?;
    let as_of = OffsetDateTime::parse(as_of_raw, &Rfc3339).map_err(|e| {
        RateProviderError::Internal(format!("as_of '{as_of_raw}' not RFC3339: {e}"))
    })?;
    let rates_obj = json_lookup(body, &mapping.rates)
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RateProviderError::Internal(format!(
                "rates path '{}' missing or not an object",
                mapping.rates
            ))
        })?;

    let mut out = Vec::new();
    let mut skipped: u64 = 0;
    for (quote, entry) in rates_obj {
        match map_entry(quote, entry, mapping, &base, as_of, provider) {
            Some(rate) => out.push(rate),
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(
            provider,
            skipped,
            "bss-rate-provider-http-json: skipped unmappable entries"
        );
    }
    if out.is_empty() {
        return Err(RateProviderError::Internal(
            "http-json document produced zero mappable rates".to_owned(),
        ));
    }
    Ok(out)
}

/// Map one `(quote, entry)` pair from the feed's rates object.
///
/// `None` means "skip this entry". Every skip path logs the offending quote and
/// the reason: an aggregate "skipped N entries" alone would not tell an operator
/// which currency of which feed started failing, or why.
fn map_entry(
    quote: &str,
    entry: &Value,
    mapping: &Mapping,
    base: &str,
    as_of: OffsetDateTime,
    provider: &str,
) -> Option<ProviderRate> {
    let normalized_quote = normalize_currency(quote).or_else(|| {
        tracing::warn!(
            provider,
            quote = %quote,
            "bss-rate-provider-http-json: skipping entry with a non-ISO-4217-shaped quote code"
        );
        None
    })?;
    let rate_val = entry.get(&mapping.rate).or_else(|| {
        tracing::warn!(
            provider,
            quote = %normalized_quote,
            rate_field = %mapping.rate,
            "bss-rate-provider-http-json: skipping entry with no rate field"
        );
        None
    })?;
    let rate_str: Cow<'_, str> = match rate_val {
        Value::String(s) => Cow::Borrowed(s.as_str()),
        Value::Number(n) => Cow::Owned(n.to_string()),
        _ => {
            tracing::warn!(
                provider,
                quote = %normalized_quote,
                "bss-rate-provider-http-json: skipping entry whose rate is neither string nor number"
            );
            return None;
        }
    };
    match rate_to_micro(&rate_str) {
        Ok(rate_micro) => Some(ProviderRate {
            base: base.to_owned(),
            quote: normalized_quote,
            rate_micro,
            as_of,
            provider: provider.to_owned(),
        }),
        Err(e) => {
            tracing::warn!(
                provider,
                quote = %normalized_quote,
                error = %e,
                "bss-rate-provider-http-json: skipping entry whose rate failed conversion"
            );
            None
        }
    }
}

/// The config-derived settings [`HttpJsonRateProvider`] needs, as one named
/// struct rather than a positional argument list: `id` and `base_url` are
/// adjacent `String`s that a positional constructor would let callers transpose
/// without a compile error. The fields map 1:1 onto
/// [`crate::config::HttpJsonPluginConfig`].
#[derive(Debug, Clone)]
pub struct HttpJsonSourceSettings {
    /// Stable `provider_id`, stamped as provenance on every rate served.
    pub id: String,
    /// Feed endpoint (validated as https when the client is built).
    pub base_url: String,
    /// How the feed is authenticated, with its credential.
    pub auth: Auth,
    /// Field mapping — required for this source kind.
    pub mapping: Mapping,
}

/// The generic http-json source.
pub struct HttpJsonRateProvider {
    settings: HttpJsonSourceSettings,
    client: HttpClient,
    metrics: SharedFetchMetrics,
}

impl HttpJsonRateProvider {
    /// Build the source over its config-derived settings and a shared HTTP client.
    #[must_use]
    pub fn new(
        settings: HttpJsonSourceSettings,
        client: HttpClient,
        metrics: SharedFetchMetrics,
    ) -> Self {
        Self {
            settings,
            client,
            metrics,
        }
    }

    /// Attach the configured credential to an outbound request.
    ///
    /// Shared by the fetch and the reachability probe so both hit the endpoint as
    /// the same caller; a probe that skipped the credential would report on a
    /// request the real fetch never makes. The credential travels inside the
    /// `Auth` variant that needs it, so there is no "kind set but key missing"
    /// arm, and every variant uses a header, so nothing here can leak into a
    /// logged URL.
    fn with_auth(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.settings.auth {
            Auth::None {} => request,
            Auth::Bearer { api_key } => request.header(
                "Authorization",
                &format!("Bearer {}", api_key.expose_secret()),
            ),
            Auth::HeaderKey { api_key } => request.header(API_KEY_HEADER, api_key.expose_secret()),
        }
    }
}

#[async_trait]
impl RateProviderV1 for HttpJsonRateProvider {
    fn provider_id(&self) -> &str {
        &self.settings.id
    }

    async fn fetch_latest(
        &self,
        _ctx: &SecurityContext,
        pairs: &[CurrencyPair],
        _request_id: &str,
    ) -> Result<Vec<ProviderRate>, RateProviderError> {
        let request = self.with_auth(self.client.get(&self.settings.base_url));
        fetch_and_parse(request, &self.settings.id, self.metrics.as_ref(), |bytes| {
            let body: Value = serde_json::from_slice(bytes)
                .map_err(|e| RateProviderError::Internal(format!("invalid JSON: {e}")))?;
            let mut rates = map_json_document(&body, &self.settings.mapping, &self.settings.id)?;
            // One `as_of` is parsed per document and copied onto every rate, so
            // checking the first covers all of them. A document dated far enough
            // ahead reads as permanently fresh to the ledger's age-based
            // staleness rule; rejecting also lets the composite fall through to
            // the next source instead of storing the bad value.
            if let Some(first) = rates.first() {
                reject_future_publication_time(
                    first.as_of,
                    OffsetDateTime::now_utc(),
                    &self.settings.id,
                )?;
            }
            let as_of_unix = rates.first().map_or(0, |r| r.as_of.unix_timestamp());
            if !pairs.is_empty() {
                // Case-insensitive, matching the ECB source: both sit behind the
                // same `CompositeRateProvider`, so a caller passing "usd" must
                // not get rates from one source and nothing from the other.
                rates.retain(|r| {
                    pairs.iter().any(|p| {
                        p.base.eq_ignore_ascii_case(&r.base)
                            && p.quote.eq_ignore_ascii_case(&r.quote)
                    })
                });
                // The contract's shared answer for "not served here", identical
                // in the ECB source: a routine unsupported pair must not land on
                // the fetch-error metric under the "internal" label, which is
                // for real defects. Still an `Err`, so the composite moves on to
                // a source that may publish the pair.
                if rates.is_empty()
                    && let Some(first) = pairs.first()
                {
                    return Err(RateProviderError::PairUnavailable {
                        base: first.base.clone(),
                        quote: first.quote.clone(),
                    });
                }
            }
            Ok((rates, as_of_unix))
        })
        .await
    }

    /// Cheap reachability probe: a HEAD against the configured `base_url`, never a
    /// fetch+parse of the document.
    ///
    /// **Any HTTP response means reachable** — only a transport failure (DNS,
    /// connect, TLS, timeout) is `Err`. That rule is what makes a HEAD usable for
    /// an arbitrary configured feed: unlike ECB's static file, a JSON API commonly
    /// answers `405 Method Not Allowed` to HEAD while serving GET perfectly, and
    /// `401`/`403` if a credential is rotated. Each of those is the host
    /// answering, so treating them as failures would report a working feed as
    /// down. A non-success status is logged all the same.
    ///
    /// A deliberately weak signal, per the trait contract: it says nothing about
    /// whether the document parses, whether `mapping` still matches the feed's
    /// shape, or whether the rates are fresh. Freshness lives on
    /// `fx_provider_last_success_timestamp`.
    ///
    /// The probe bypasses `fetch_and_parse` on purpose: touching those series
    /// would advance the last-success gauge and the fetch-duration count,
    /// silencing the freshness alerts meant to catch a feed whose real syncs fail.
    async fn health(
        &self,
        _ctx: &SecurityContext,
        _request_id: &str,
    ) -> Result<(), RateProviderError> {
        let resp = self
            .with_auth(self.client.head(&self.settings.base_url))
            .send()
            .await
            .map_err(|e| map_http_error(&e))?;
        if !resp.status().is_success() {
            tracing::warn!(
                provider = %self.settings.id,
                status = resp.status().as_u16(),
                "bss-rate-provider-http-json: reachability probe answered with a non-success \
                 status; the host answered, so the probe passes"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
