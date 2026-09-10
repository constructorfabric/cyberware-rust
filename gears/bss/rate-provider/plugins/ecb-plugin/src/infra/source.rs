//! `EcbRateProvider` — HTTP fetch + XML parse + `rate_micro` conversion over the
//! ECB daily reference-rate feed. Direct pairs only (design O-3): a requested
//! pair the feed does not publish is omitted, never synthesized or inverted.

use std::collections::HashSet;

use async_trait::async_trait;
use bss_ledger_sdk::{CurrencyPair, ProviderRate, RateProviderError, RateProviderV1};
use bss_rate_provider_sdk::conversion::rate_to_micro;
use bss_rate_provider_sdk::currency::normalize_currency;
use bss_rate_provider_sdk::error::map_http_error;
use bss_rate_provider_sdk::fetch::fetch_and_parse;
use bss_rate_provider_sdk::metrics::SharedFetchMetrics;
use bss_rate_provider_sdk::publication_time::reject_future_publication_time;
use chrono::{Datelike, NaiveDate};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use toolkit_http::HttpClient;
use toolkit_security::SecurityContext;

/// ECB publishes EUR-based pairs (EUR -> currency).
const ECB_BASE: &str = "EUR";

/// A parsed ECB feed: the publication date plus its raw `(currency, rate)` pairs.
type EcbTable = (NaiveDate, Vec<(String, String)>);

/// Parse the ECB daily XML into a publication date and raw `(currency, rate)` pairs.
///
/// The daily feed is one day's rates, so a second *distinct* publication date
/// fails the whole document: it means this is some other file (most likely the
/// 90-day history feed behind a mistyped `base_url`), there is no non-arbitrary
/// way to pick a date, and failing lets the composite move on to the next source.
/// A repeat of the *same* date is fine. A duplicate currency entry is only logged
/// — the first occurrence wins.
///
/// Currency rows are kept only while nested inside the publication date's
/// `<Cube time="...">` block, so a row after that block's `</Cube>`, or one under
/// a `time` that failed to parse, is omitted rather than stamped with an `as_of`
/// that isn't its own.
///
/// # Errors
/// [`RateProviderError::Internal`] on malformed XML, a missing `time` date, more
/// than one distinct `time` date, or an empty rate table.
pub fn parse_ecb_xml(bytes: &[u8]) -> Result<EcbTable, RateProviderError> {
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut date: Option<NaiveDate> = None;
    let mut current_date: Option<NaiveDate> = None;
    let mut rates: Vec<(String, String)> = Vec::new();
    let mut seen_currencies: HashSet<String> = HashSet::new();
    // Nesting depth of open `<Cube>` elements, and the depth of the retained
    // date's Cube. Comparing dates alone cannot tell a row *inside* that block
    // from a sibling that follows its `</Cube>`, because `current_date` outlives
    // the element it came from.
    let mut depth: usize = 0;
    let mut retained_depth: Option<usize> = None;
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| RateProviderError::Internal(format!("ECB XML parse: {e}")))?;
        match event {
            // The date lives on the outer `<Cube time="...">` and each inner
            // `<Cube currency="X" rate="Y"/>` is one EUR-based pair — both are
            // `Cube` elements, so inspect every `Cube`'s attributes.
            Event::Start(e) if e.local_name().as_ref() == b"Cube" => {
                depth += 1;
                let (time, currency, rate) = read_cube_attributes(&e);
                if let Some(value) = time {
                    current_date = update_publication_date(&mut date, &value)?;
                    // Only a `Start` can open the block that owns recordable
                    // rows: a self-closing Cube encloses nothing.
                    if retained_depth.is_none() && is_retained_date(current_date, date) {
                        retained_depth = Some(depth);
                    }
                }
                record_cube_row(
                    &mut seen_currencies,
                    &mut rates,
                    keep_row(retained_depth, current_date, date),
                    currency,
                    rate,
                );
            }
            // Self-closing: encloses nothing, so it never changes depth.
            Event::Empty(e) if e.local_name().as_ref() == b"Cube" => {
                let (time, currency, rate) = read_cube_attributes(&e);
                if let Some(value) = time {
                    current_date = update_publication_date(&mut date, &value)?;
                }
                record_cube_row(
                    &mut seen_currencies,
                    &mut rates,
                    keep_row(retained_depth, current_date, date),
                    currency,
                    rate,
                );
            }
            Event::End(e) if e.local_name().as_ref() == b"Cube" => {
                if retained_depth == Some(depth) {
                    retained_depth = None;
                }
                // `saturating_sub`: a stray `</Cube>` in a malformed document
                // must not underflow the counter.
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let date = date.ok_or_else(|| {
        RateProviderError::Internal("ECB XML missing publication date".to_owned())
    })?;
    if rates.is_empty() {
        return Err(RateProviderError::Internal(
            "ECB XML contained no rate entries".to_owned(),
        ));
    }
    Ok((date, rates))
}

/// Whether `current` is a parsed date that is the retained one.
///
/// `is_some()` matters as much as the equality: before any date is seen both
/// sides are `None` and compare equal, which would let a row that arrived first
/// inherit a date a later block establishes.
fn is_retained_date(current: Option<NaiveDate>, retained: Option<NaiveDate>) -> bool {
    current.is_some() && current == retained
}

/// Whether a `(currency, rate)` row may be recorded. Both halves are needed:
///
/// * `retained_depth.is_some()` — the row sits **inside** the still-open date
///   block, so a sibling Cube after that block's `</Cube>` cannot inherit the
///   closed block's `as_of`.
/// * [`is_retained_date`] — the enclosing block carries the document's own date,
///   so a row under an unparseable `time` (or one seen before any date) is not
///   stamped with a date it never had.
fn keep_row(
    retained_depth: Option<usize>,
    current_date: Option<NaiveDate>,
    date: Option<NaiveDate>,
) -> bool {
    retained_depth.is_some() && is_retained_date(current_date, date)
}

/// Record one Cube's `(currency, rate)` pair, or log why it was dropped.
fn record_cube_row(
    seen: &mut HashSet<String>,
    rates: &mut Vec<(String, String)>,
    keep: bool,
    currency: Option<String>,
    rate: Option<String>,
) {
    match (currency, rate) {
        (Some(c), Some(r)) => {
            if keep {
                record_currency_rate(seen, rates, &c, r);
            } else {
                tracing::warn!(
                    currency = %c,
                    "bss-rate-provider-ecb: currency entry outside the retained publication \
                     date's block; omitting rather than mislabeling its as_of"
                );
            }
        }
        // A real ECB rate `Cube` always carries both. Half a pair (truncated or
        // malformed feed) is unusable, but still logged: every oddity in this
        // module leaves a trace.
        (c @ Some(_), None) | (c @ None, Some(_)) => {
            tracing::warn!(
                currency = ?c,
                "bss-rate-provider-ecb: Cube carries only one of currency/rate; \
                 ignoring the entry"
            );
        }
        // The outer `<Cube time=...>` and wrapper elements: no currency, no
        // rate, nothing to report.
        (None, None) => {}
    }
}

/// Pull the `time`, `currency`, and `rate` attribute values off one `<Cube>`
/// element. A real ECB `Cube` carries `time` XOR `currency`+`rate`, but all three
/// are read from whichever element shows up.
fn read_cube_attributes(e: &BytesStart<'_>) -> (Option<String>, Option<String>, Option<String>) {
    let mut time = None;
    let mut currency = None;
    let mut rate = None;
    for attr in e.attributes().flatten() {
        let value = String::from_utf8_lossy(attr.value.as_ref()).into_owned();
        match attr.key.as_ref() {
            b"time" => time = Some(value),
            b"currency" => currency = Some(value),
            b"rate" => rate = Some(value),
            _ => {}
        }
    }
    (time, currency, rate)
}

/// Record the document's publication date, or reject the document.
///
/// Returns the parsed date so the caller can tell which date-block the following
/// currency rows sit in. `Ok(None)` means this `Cube`'s `time` was unparseable
/// and the element carries no usable date.
///
/// # Errors
/// [`RateProviderError::Internal`] when a *second, distinct* date appears (see
/// [`parse_ecb_xml`]). A repeat of the same date is accepted.
fn update_publication_date(
    date: &mut Option<NaiveDate>,
    value: &str,
) -> Result<Option<NaiveDate>, RateProviderError> {
    let parsed = match NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        Ok(parsed) => parsed,
        Err(e) => {
            // Log rather than drop silently: if this is the feed's ONLY date,
            // parsing later fails with "missing publication date", which
            // misattributes the cause (unparseable, not absent). The raw value
            // comes from a public feed, so it is diagnostic data, not a secret.
            tracing::warn!(
                value = %value,
                error = %e,
                "bss-rate-provider-ecb: unparseable publication date in feed; ignoring this Cube"
            );
            return Ok(None);
        }
    };
    match *date {
        None => *date = Some(parsed),
        Some(existing) if existing != parsed => {
            tracing::warn!(
                previous = %existing,
                found = %parsed,
                "bss-rate-provider-ecb: feed contains more than one publication date; \
                 rejecting the document (is base_url pointing at the history feed?)"
            );
            return Err(RateProviderError::Internal(format!(
                "ECB XML contains more than one publication date ({existing} and {parsed}); \
                 the daily feed must carry exactly one"
            )));
        }
        Some(_) => {}
    }
    Ok(Some(parsed))
}

/// Keep the first entry for a given currency; a duplicate is logged, not fatal
/// (see [`parse_ecb_xml`]).
///
/// The `currency` attribute arrives verbatim from the remote XML and becomes a
/// tenant-store column value, so it is gated on the ISO-4217 shape here; a
/// malformed code is dropped and logged. Dedup keys off the normalized code, so a
/// feed publishing both `USD` and `usd` is caught as the duplicate it is.
fn record_currency_rate(
    seen: &mut HashSet<String>,
    rates: &mut Vec<(String, String)>,
    currency: &str,
    rate: String,
) {
    let Some(normalized) = normalize_currency(currency) else {
        tracing::warn!(
            currency = %currency,
            "bss-rate-provider-ecb: feed entry has a non-ISO-4217-shaped currency code; dropping it"
        );
        return;
    };
    if seen.insert(normalized.clone()) {
        rates.push((normalized, rate));
    } else {
        tracing::warn!(
            currency = %normalized,
            "bss-rate-provider-ecb: duplicate currency entry in feed; keeping the first"
        );
    }
}

/// The feed's civil date, anchored at midnight UTC — the same instant chrono's
/// `NaiveDate::and_time(MIN).and_utc()` produced.
///
/// # Errors
///
/// [`RateProviderError::Internal`] when `time` cannot hold the date. chrono's
/// `%Y-%m-%d` accepts a signed year past 9999 (`+12345-01-01`); on main such a
/// document read as far-future and `reject_future_publication_time` refused it.
/// An epoch fallback here would pass that guard and store the feed as
/// permanently stale, with `as_of_unix = 0` reported as the fetch instant.
fn publication_instant(date: NaiveDate) -> Result<OffsetDateTime, RateProviderError> {
    let unrepresentable = || {
        RateProviderError::Internal(format!(
            "ECB publication date {date} is outside the range `time` represents"
        ))
    };
    let month = u8::try_from(date.month())
        .ok()
        .and_then(|m| Month::try_from(m).ok())
        .ok_or_else(unrepresentable)?;
    let day = u8::try_from(date.day()).map_err(|_| unrepresentable())?;
    let date = Date::from_calendar_date(date.year(), month, day).map_err(|_| unrepresentable())?;
    Ok(PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc())
}

/// Convert parsed ECB rows into `ProviderRate`s (base = EUR), each stamped with
/// `provider` as its provenance. When `pairs` is non-empty, keep only the
/// requested EUR-based quotes (others omitted, not an error). `as_of` is the
/// publication date at 00:00:00 UTC.
///
/// A rate string that fails conversion is skipped (logged with its quote), not
/// fatal — matching the http-json source. The ledger's `RateSyncJob` treats any
/// failure from a configured provider as `FX_SNAPSHOT_MISSING` and refreshes
/// nothing, so one malformed currency would otherwise stall the FX fan-out for
/// every other currency in an otherwise-good feed.
///
/// # Errors
/// [`RateProviderError::Internal`] when entries were attempted and **all** of
/// them failed conversion, or when `date` is one `time` cannot hold (see
/// `publication_instant`).
///
/// [`RateProviderError::PairUnavailable`] when a non-empty `pairs` matched
/// nothing this feed publishes — the contract's shared answer for "not served
/// here", identical in the http-json source. It stays an `Err` so the composite
/// moves on to a source that may publish the pair; `Ok(vec![])` would read as a
/// complete document and stop the fallback chain here.
pub fn ecb_rates_to_provider_rates(
    date: NaiveDate,
    raw: &[(String, String)],
    pairs: &[CurrencyPair],
    provider: &str,
) -> Result<Vec<ProviderRate>, RateProviderError> {
    let as_of = publication_instant(date)?;
    let mut out = Vec::with_capacity(raw.len());
    let mut skipped: u64 = 0;
    for (quote, rate_str) in raw {
        // ISO 4217 codes are ASCII; compare case-insensitively so a caller that
        // didn't normalize casing (e.g. "usd") still matches a pair ECB
        // publishes, instead of looking like a genuinely unpublished one.
        if !pairs.is_empty()
            && !pairs.iter().any(|p| {
                p.base.eq_ignore_ascii_case(ECB_BASE) && p.quote.eq_ignore_ascii_case(quote)
            })
        {
            continue;
        }
        match rate_to_micro(rate_str) {
            Ok(rate_micro) => out.push(ProviderRate {
                base: ECB_BASE.to_owned(),
                quote: quote.clone(),
                rate_micro,
                as_of,
                provider: provider.to_owned(),
            }),
            Err(e) => {
                skipped += 1;
                tracing::warn!(
                    provider,
                    quote = %quote,
                    error = %e,
                    "bss-rate-provider-ecb: skipping entry whose rate failed conversion"
                );
            }
        }
    }
    if skipped > 0 {
        tracing::warn!(
            provider,
            skipped,
            "bss-rate-provider-ecb: skipped unconvertible rate entries"
        );
        // Every attempted entry failed: report it rather than pass an empty
        // table off as a success, which would suppress the composite's fallback.
        if out.is_empty() {
            return Err(RateProviderError::Internal(
                "every ECB rate entry failed conversion".to_owned(),
            ));
        }
    }
    // Nothing matched a caller-requested pair, so the composite should try the
    // next source. Named after the first requested pair: with one pair that is
    // exactly what was missing, with several it is the caller's own first choice.
    if out.is_empty()
        && let Some(first) = pairs.first()
    {
        return Err(RateProviderError::PairUnavailable {
            base: first.base.clone(),
            quote: first.quote.clone(),
        });
    }
    Ok(out)
}

/// The ECB source.
pub struct EcbRateProvider {
    id: String,
    base_url: String,
    client: HttpClient,
    metrics: SharedFetchMetrics,
}

impl EcbRateProvider {
    /// Build the source over a shared HTTP client.
    #[must_use]
    pub fn new(
        id: String,
        base_url: String,
        client: HttpClient,
        metrics: SharedFetchMetrics,
    ) -> Self {
        Self {
            id,
            base_url,
            client,
            metrics,
        }
    }
}

#[async_trait]
impl RateProviderV1 for EcbRateProvider {
    fn provider_id(&self) -> &str {
        &self.id
    }

    async fn fetch_latest(
        &self,
        _ctx: &SecurityContext,
        pairs: &[CurrencyPair],
        _request_id: &str,
    ) -> Result<Vec<ProviderRate>, RateProviderError> {
        let request = self.client.get(&self.base_url);
        fetch_and_parse(request, &self.id, self.metrics.as_ref(), |bytes| {
            let (date, raw) = parse_ecb_xml(bytes)?;
            let as_of = publication_instant(date)?;
            // A document dated far enough ahead reads as permanently fresh to
            // the ledger's age-based staleness rule.
            reject_future_publication_time(as_of, OffsetDateTime::now_utc(), &self.id)?;
            let rates = ecb_rates_to_provider_rates(date, &raw, pairs, &self.id)?;
            Ok((rates, as_of.unix_timestamp()))
        })
        .await
    }

    /// Cheap reachability probe: a HEAD request, never a full fetch+parse. ECB's
    /// static feed serves HEAD the same as GET, minus the body.
    ///
    /// **Any HTTP response is reachable, including a non-2xx one.** A `503` proves
    /// the request arrived, which is the only question this probe answers; only a
    /// transport failure (DNS, connect, TLS, timeout) is `Err`. See
    /// `RateProviderV1::health`. The status is still logged, because "reachable
    /// but answering 503" is worth seeing.
    ///
    /// So a green probe says nothing about whether the body still parses. Feed
    /// freshness lives on `fx_provider_last_success_timestamp`, which advances
    /// only on a fetch that actually parsed.
    async fn health(
        &self,
        _ctx: &SecurityContext,
        _request_id: &str,
    ) -> Result<(), RateProviderError> {
        let resp = self
            .client
            .head(&self.base_url)
            .send()
            .await
            .map_err(|e| map_http_error(&e))?;
        if !resp.status().is_success() {
            tracing::warn!(
                provider = %self.id,
                status = resp.status().as_u16(),
                "bss-rate-provider-ecb: reachability probe answered with a non-success status; \
                 the host answered, so the probe passes"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
