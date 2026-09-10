//! Composite fallback + provenance tests with in-memory fake sources.

use std::sync::Arc;

use async_trait::async_trait;
use bss_ledger_sdk::{CurrencyPair, ProviderRate, RateProviderError, RateProviderV1};
use time::OffsetDateTime;
use toolkit_security::SecurityContext;

use super::CompositeRateProvider;

/// The composite's own configured identity under test.
const COMPOSITE_ID: &str = "bss-rate-provider";

/// A fake source whose `fetch_latest` and `health` outcomes are set
/// independently, so the two fallback loops can be exercised in isolation.
struct Fake {
    id: &'static str,
    /// `None` = fetch succeeds; `Some(e)` = fetch fails with `e`.
    fetch_err: Option<RateProviderError>,
    /// `None` = health succeeds; `Some(e)` = health fails with `e`.
    health_err: Option<RateProviderError>,
}

impl Fake {
    /// A source where both fetch and health succeed.
    fn ok(id: &'static str) -> Self {
        Self {
            id,
            fetch_err: None,
            health_err: None,
        }
    }

    /// A source where both fetch and health fail with `err`.
    fn failing(id: &'static str, err: RateProviderError) -> Self {
        Self {
            id,
            fetch_err: Some(clone_err(&err)),
            health_err: Some(err),
        }
    }
}

/// `RateProviderError` is not `Clone`, and a `Fake` needs the same failure for
/// both entry points; rebuild it by variant instead.
fn clone_err(e: &RateProviderError) -> RateProviderError {
    match e {
        RateProviderError::Unreachable(m) => RateProviderError::Unreachable(m.clone()),
        RateProviderError::UpstreamStatus(s) => RateProviderError::UpstreamStatus(*s),
        RateProviderError::Internal(m) => RateProviderError::Internal(m.clone()),
        RateProviderError::InvalidPair(m) => RateProviderError::InvalidPair(m.clone()),
        RateProviderError::PairUnavailable { base, quote } => RateProviderError::PairUnavailable {
            base: base.clone(),
            quote: quote.clone(),
        },
    }
}

#[async_trait]
impl RateProviderV1 for Fake {
    fn provider_id(&self) -> &str {
        self.id
    }

    async fn fetch_latest(
        &self,
        _ctx: &SecurityContext,
        _pairs: &[CurrencyPair],
        _request_id: &str,
    ) -> Result<Vec<ProviderRate>, RateProviderError> {
        match &self.fetch_err {
            Some(e) => Err(clone_err(e)),
            None => Ok(vec![ProviderRate {
                base: "EUR".to_owned(),
                quote: "USD".to_owned(),
                rate_micro: 1_000_000,
                as_of: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                // Each source stamps its own id — this is what the ledger
                // records, so the fake must behave like a real source.
                provider: self.id.to_owned(),
            }]),
        }
    }

    async fn health(
        &self,
        _ctx: &SecurityContext,
        _request_id: &str,
    ) -> Result<(), RateProviderError> {
        match &self.health_err {
            Some(e) => Err(clone_err(e)),
            None => Ok(()),
        }
    }
}

#[tokio::test]
async fn falls_back_and_the_served_document_names_the_serving_source() {
    let composite = CompositeRateProvider::new(
        COMPOSITE_ID.to_owned(),
        vec![
            Arc::new(Fake::failing(
                "ecb",
                RateProviderError::Unreachable("down".to_owned()),
            )),
            Arc::new(Fake::ok("bank-x")),
        ],
    );
    let ctx = SecurityContext::anonymous();
    let rates = composite.fetch_latest(&ctx, &[], "req").await.unwrap();
    assert_eq!(rates.len(), 1);
    // Provenance rides on the rate itself, so it cannot be raced by another
    // caller fetching between this fetch and a later `provider_id()` read.
    assert_eq!(rates[0].provider, "bank-x");
    // `provider_id()` is the composite's constant identity, NOT the last server.
    assert_eq!(composite.provider_id(), COMPOSITE_ID);
}

#[tokio::test]
async fn all_fail_returns_the_last_sources_error() {
    // Distinct errors per source, so "returns the FIRST error" cannot pass.
    let composite = CompositeRateProvider::new(
        COMPOSITE_ID.to_owned(),
        vec![
            Arc::new(Fake::failing(
                "ecb",
                RateProviderError::Unreachable("first".to_owned()),
            )),
            Arc::new(Fake::failing(
                "bank-x",
                RateProviderError::UpstreamStatus(503),
            )),
        ],
    );
    let ctx = SecurityContext::anonymous();
    let err = composite.fetch_latest(&ctx, &[], "req").await.unwrap_err();
    assert!(
        matches!(err, RateProviderError::UpstreamStatus(503)),
        "expected the LAST source's error, got {err:?}"
    );
    // Identity stays reportable after a total failure, and is never "none" (the
    // ledger's unconfigured sentinel).
    assert_eq!(composite.provider_id(), COMPOSITE_ID);
}

#[tokio::test]
async fn health_falls_back_to_the_next_source() {
    // A separate loop from `fetch_latest`'s: a source that cannot serve rates may
    // still answer a probe, and vice versa.
    let composite = CompositeRateProvider::new(
        COMPOSITE_ID.to_owned(),
        vec![
            Arc::new(Fake::failing(
                "ecb",
                RateProviderError::Unreachable("down".to_owned()),
            )),
            Arc::new(Fake::ok("bank-x")),
        ],
    );
    let ctx = SecurityContext::anonymous();
    assert!(composite.health(&ctx, "req").await.is_ok());
}

#[tokio::test]
async fn health_all_fail_returns_the_last_sources_error() {
    let composite = CompositeRateProvider::new(
        COMPOSITE_ID.to_owned(),
        vec![
            Arc::new(Fake::failing(
                "ecb",
                RateProviderError::Unreachable("first".to_owned()),
            )),
            Arc::new(Fake::failing(
                "bank-x",
                RateProviderError::UpstreamStatus(503),
            )),
        ],
    );
    let ctx = SecurityContext::anonymous();
    let err = composite.health(&ctx, "req").await.unwrap_err();
    assert!(
        matches!(err, RateProviderError::UpstreamStatus(503)),
        "expected the LAST source's error, got {err:?}"
    );
}

#[tokio::test]
async fn health_probes_in_order_and_stops_at_the_first_healthy_source() {
    // The primary is healthy, so the probe must not fall through to a source
    // that would fail — proving the loop short-circuits on success.
    let composite = CompositeRateProvider::new(
        COMPOSITE_ID.to_owned(),
        vec![
            Arc::new(Fake::ok("ecb")),
            Arc::new(Fake::failing(
                "bank-x",
                RateProviderError::Internal("must not be probed".to_owned()),
            )),
        ],
    );
    let ctx = SecurityContext::anonymous();
    assert!(composite.health(&ctx, "req").await.is_ok());
}

#[test]
fn empty_sources_still_reports_the_configured_identity() {
    // `CompositeRateProvider::new` debug_asserts non-emptiness, so build the
    // violating instance directly (this test module is a child of `composite`, so
    // the private fields are visible) to cover the release-mode case:
    // `provider_id()` reads no element of the list, so it cannot index out of
    // bounds.
    let composite = CompositeRateProvider {
        id: COMPOSITE_ID.to_owned(),
        sources: Vec::new(),
    };
    assert_eq!(composite.provider_id(), COMPOSITE_ID);
}

#[tokio::test]
async fn empty_composite_errors_rather_than_panicking() {
    let composite = CompositeRateProvider {
        id: COMPOSITE_ID.to_owned(),
        sources: Vec::new(),
    };
    let ctx = SecurityContext::anonymous();
    assert!(matches!(
        composite.fetch_latest(&ctx, &[], "req").await,
        Err(RateProviderError::Internal(_))
    ));
    assert!(matches!(
        composite.health(&ctx, "req").await,
        Err(RateProviderError::Internal(_))
    ));
}
