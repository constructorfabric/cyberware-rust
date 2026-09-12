//! Integration: the core gear's `DiscoveringRateProvider` over a real
//! `ClientHub` + an in-memory types-registry mock.
//!
//! Covers the whole discovery path: list source-plugin instances → filter by
//! vendor → order by `priority` → resolve scoped `RateProviderV1` → compose
//! (ordered fallback + per-rate provenance), plus the empty-set error and the
//! per-call re-discovery that picks up a late-registered plugin without a restart.
#![allow(
    clippy::unwrap_used,
    reason = "integration-test setup helpers: an unwrap here just fails the test"
)]

use std::sync::Arc;

use async_trait::async_trait;
use bss_ledger_sdk::{CurrencyPair, ProviderRate, RateProviderError, RateProviderV1};
use bss_rate_provider::infra::discovery::DiscoveringRateProvider;
use bss_rate_provider_sdk::RateProviderSourcePluginSpecV1;
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_security::SecurityContext;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::models::GtsInstance;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};

/// The core gear's own configured id (what `provider_id()` reports).
const COMPOSITE_ID: &str = "bss-rate-provider";

/// A source whose `provider_id` is `id` and which succeeds or fails per `ok`.
struct FakeSource {
    id: String,
    ok: bool,
}

#[async_trait]
impl RateProviderV1 for FakeSource {
    fn provider_id(&self) -> &str {
        &self.id
    }
    async fn fetch_latest(
        &self,
        _ctx: &SecurityContext,
        _pairs: &[CurrencyPair],
        _request_id: &str,
    ) -> Result<Vec<ProviderRate>, RateProviderError> {
        if self.ok {
            Ok(vec![ProviderRate {
                base: "EUR".to_owned(),
                quote: "USD".to_owned(),
                rate_micro: 1_000_000,
                as_of: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                // Real sources stamp their own id; the fake must too, since
                // that is the provenance the ledger stores.
                provider: self.id.clone(),
            }])
        } else {
            Err(RateProviderError::Unreachable("down".to_owned()))
        }
    }

    /// Reachability tracks the same `ok` flag as the fetch, so one `SourceSpec` can
    /// express "this source is down" for both calls. A real source's probe is
    /// independent of its fetch.
    async fn health(
        &self,
        _ctx: &SecurityContext,
        _request_id: &str,
    ) -> Result<(), RateProviderError> {
        if self.ok {
            Ok(())
        } else {
            Err(RateProviderError::Unreachable("down".to_owned()))
        }
    }
}

/// One configured source for a test.
struct SourceSpec<'a> {
    /// Both the plugin's instance segment and the `provider_id` it reports.
    name: &'a str,
    vendor: &'a str,
    priority: i16,
    /// Whether its `fetch_latest` succeeds.
    ok: bool,
    /// When false, the `PluginV1` instance is published in the registry but NO
    /// scoped `ClientHub` client is registered — the partial-registration state a
    /// plugin is briefly in during startup.
    register_client: bool,
}

impl<'a> SourceSpec<'a> {
    /// A fully-registered, healthy source.
    fn ok(name: &'a str, vendor: &'a str, priority: i16) -> Self {
        Self {
            name,
            vendor,
            priority,
            ok: true,
            register_client: true,
        }
    }

    /// A fully-registered source whose fetches fail.
    fn failing(name: &'a str, vendor: &'a str, priority: i16) -> Self {
        Self {
            ok: false,
            ..Self::ok(name, vendor, priority)
        }
    }

    /// Instance published, scoped client absent.
    fn instance_only(name: &'a str, vendor: &'a str, priority: i16) -> Self {
        Self {
            register_client: false,
            ..Self::ok(name, vendor, priority)
        }
    }
}

/// The wiring under test plus the handles a test needs to poke it mid-run.
struct Harness {
    provider: DiscoveringRateProvider,
    mock: Arc<MockTypesRegistryClient>,
    hub: Arc<ClientHub>,
}

/// Build the `PluginV1` registration pair for a source, deterministically from
/// its name — so a test can register the scoped client later, out of band.
fn registration(spec: &SourceSpec<'_>) -> (String, serde_json::Value) {
    let segment = format!("cf.bss.rate_provider_{}.plugin.v1", spec.name);
    let (id, json) = PluginV1::<RateProviderSourcePluginSpecV1>::build_registration(
        &segment,
        spec.vendor,
        spec.priority,
    )
    .unwrap();
    (id.as_ref().to_owned(), json)
}

/// Register `spec`'s scoped `RateProviderV1` client on `hub` under its instance id.
fn register_scoped_client(hub: &ClientHub, spec: &SourceSpec<'_>) {
    let (id_str, _json) = registration(spec);
    let source: Arc<dyn RateProviderV1> = Arc::new(FakeSource {
        id: spec.name.to_owned(),
        ok: spec.ok,
    });
    hub.register_scoped::<dyn RateProviderV1>(ClientScope::gts_id(&id_str), source);
}

/// Build a `ClientHub` with the given sources registered scoped + a mock
/// types-registry listing their `PluginV1` instances.
fn setup(sources: &[SourceSpec<'_>], discover_vendor: &str) -> Harness {
    build(sources, Vec::new(), false, discover_vendor)
}

/// Full harness builder.
///
/// `extra_instances` are published verbatim alongside the `sources`, for registry
/// contents a `SourceSpec` cannot express (an instance whose object is not a
/// `PluginV1` envelope). `registry_unreachable` makes every `list_*` call fail,
/// standing in for a types-registry outage.
fn build(
    sources: &[SourceSpec<'_>],
    extra_instances: Vec<GtsInstance>,
    registry_unreachable: bool,
    discover_vendor: &str,
) -> Harness {
    let hub = Arc::new(ClientHub::default());
    let mut instances: Vec<GtsInstance> = Vec::new();
    for spec in sources {
        let (id_str, json) = registration(spec);
        if spec.register_client {
            register_scoped_client(&hub, spec);
        }
        instances.push(make_test_instance(&id_str, json));
    }
    instances.extend(extra_instances);
    let mut mock = MockTypesRegistryClient::new().with_instances(instances);
    if registry_unreachable {
        mock = mock.with_list_error(types_registry_sdk::testing::internal(
            "types-registry is down",
        ));
    }
    let mock = Arc::new(mock);
    hub.register::<dyn TypesRegistryClient>(mock.clone());
    let provider = DiscoveringRateProvider::new(
        Arc::clone(&hub),
        discover_vendor.to_owned(),
        COMPOSITE_ID.to_owned(),
    );
    Harness {
        provider,
        mock,
        hub,
    }
}

#[tokio::test]
async fn composes_in_priority_order_and_stamps_the_serving_source() {
    let h = setup(
        &[
            SourceSpec::ok("ecb", "cf.bss", 100),
            SourceSpec::ok("bank", "cf.bss", 200),
        ],
        "cf.bss",
    );
    let ctx = SecurityContext::anonymous();
    let rates = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    assert_eq!(rates.len(), 1);
    // Lowest priority (100 = ecb) is tried first and succeeds, and says so on the
    // rate itself.
    assert_eq!(rates[0].provider, "ecb");
    // The adapter's own identity is constant, not "whoever served last".
    assert_eq!(h.provider.provider_id(), COMPOSITE_ID);
}

#[tokio::test]
async fn falls_back_to_next_priority_on_failure() {
    let h = setup(
        &[
            SourceSpec::failing("ecb", "cf.bss", 100),
            SourceSpec::ok("bank", "cf.bss", 200),
        ],
        "cf.bss",
    );
    let ctx = SecurityContext::anonymous();
    let rates = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    assert_eq!(rates.len(), 1);
    // The primary failed, so the fallback served — and the stored provenance says
    // so.
    assert_eq!(rates[0].provider, "bank");
}

#[tokio::test]
async fn filters_out_other_vendor_sources() {
    // The rogue source has a LOWER priority (tried first if included) but a
    // different vendor, so it must be excluded and ecb must serve.
    let h = setup(
        &[
            SourceSpec::ok("ecb", "cf.bss", 100),
            SourceSpec::ok("rogue", "other", 10),
        ],
        "cf.bss",
    );
    let ctx = SecurityContext::anonymous();
    let rates = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    assert_eq!(rates[0].provider, "ecb");
}

#[tokio::test]
async fn equal_priorities_resolve_the_same_way_whatever_the_registry_order() {
    // Registry list order is not a stable contract, so if it decided ties the
    // provenance written onto financial rows could change between ticks with no
    // configuration change. The instance-id tiebreak pins it:
    // "…rate_provider_bank…" sorts before "…rate_provider_ecb…".
    let ctx = SecurityContext::anonymous();
    for order in [["ecb", "bank"], ["bank", "ecb"]] {
        let h = setup(
            &[
                SourceSpec::ok(order[0], "cf.bss", 100),
                SourceSpec::ok(order[1], "cf.bss", 100),
            ],
            "cf.bss",
        );
        let rates = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
        assert_eq!(
            rates[0].provider, "bank",
            "registry order {order:?} changed which tied source served"
        );
    }
}

#[tokio::test]
async fn errors_when_no_matching_vendor_source() {
    let h = setup(&[SourceSpec::ok("ecb", "other", 100)], "cf.bss");
    let ctx = SecurityContext::anonymous();
    let err = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap_err();
    // A `vendor` typo passes every startup check, so this first-tick error is the
    // only place it can be diagnosed: it must name the wanted vendor AND the ones
    // actually registered, or the operator is left comparing YAML against a
    // registry they cannot see.
    let msg = err.to_string();
    assert!(
        msg.contains("cf.bss"),
        "expected vendor missing from: {msg}"
    );
    assert!(
        msg.contains("other"),
        "vendor present in the registry missing from: {msg}"
    );
}

#[tokio::test]
async fn errors_when_the_registry_holds_no_source_plugins_at_all() {
    // The other arm of the same message: nothing to list means there is no
    // vendor to compare against, so the error must not imply a mismatch.
    let h = setup(&[], "cf.bss");
    let ctx = SecurityContext::anonymous();
    let err = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cf.bss"),
        "expected vendor missing from: {msg}"
    );
    assert!(
        !msg.contains("vendors present"),
        "no instances were registered, so none can be listed: {msg}"
    );
}

#[tokio::test]
async fn instance_without_a_scoped_client_is_excluded_not_fatal() {
    // Partial registration of one source must not take down a healthy other.
    let h = setup(
        &[
            SourceSpec::instance_only("ecb", "cf.bss", 100),
            SourceSpec::ok("bank", "cf.bss", 200),
        ],
        "cf.bss",
    );
    let ctx = SecurityContext::anonymous();
    let rates = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    assert_eq!(rates[0].provider, "bank");
}

#[tokio::test]
async fn discovery_reruns_every_tick_so_priority_changes_take_effect_without_restart() {
    let h = setup(&[SourceSpec::ok("ecb", "cf.bss", 100)], "cf.bss");
    let ctx = SecurityContext::anonymous();
    h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    // Never cached across calls: a second fetch re-lists instances, so a plugin
    // that registers, deregisters, or changes `priority` is picked up on the very
    // next tick, not only after a restart.
    assert_eq!(h.mock.list_instance_calls(), 2);
}

#[tokio::test]
async fn a_failed_tick_self_heals_once_the_source_finishes_registering() {
    // The late-registration race: the plugin published its PluginV1 instance but
    // not yet its scoped client, so the first tick finds nothing usable. Nothing
    // about that failure is cached, so the next tick succeeds with no restart.
    let spec = SourceSpec::instance_only("ecb", "cf.bss", 100);
    let h = setup(&[spec], "cf.bss");
    let ctx = SecurityContext::anonymous();

    assert!(
        h.provider.fetch_latest(&ctx, &[], "req").await.is_err(),
        "no usable source yet, so this tick must fail"
    );

    register_scoped_client(&h.hub, &SourceSpec::ok("ecb", "cf.bss", 100));

    let rates = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    assert_eq!(rates[0].provider, "ecb");
    assert_eq!(
        h.mock.list_instance_calls(),
        2,
        "the retry must re-query the registry, not reuse a cached failure"
    );
}

#[tokio::test]
async fn concurrent_first_fetches_each_discover_independently() {
    let h = setup(&[SourceSpec::ok("ecb", "cf.bss", 100)], "cf.bss");
    let ctx = SecurityContext::anonymous();

    // Discovery is not cached, so two concurrent callers each run their own pass.
    let (a, b) = tokio::join!(
        h.provider.fetch_latest(&ctx, &[], "req-a"),
        h.provider.fetch_latest(&ctx, &[], "req-b")
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(h.mock.list_instance_calls(), 2);
}

#[tokio::test]
async fn an_unreachable_registry_fails_the_tick_instead_of_reporting_no_sources() {
    // Discovery cannot run at all, which is a different fault from "the registry
    // answered and held nothing usable": the sources may be perfectly healthy. It
    // must surface as an error the ledger alarms on, never as an empty success that
    // would look like a served tick with no rates.
    let h = build(
        &[SourceSpec::ok("ecb", "cf.bss", 100)],
        Vec::new(),
        true,
        "cf.bss",
    );
    let ctx = SecurityContext::anonymous();
    let err = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, RateProviderError::Unreachable(_)),
        "a registry outage must be Unreachable, got {err:?}"
    );
    assert!(
        msg.contains("list rate-provider source plugins"),
        "the error must say the listing failed, not imply a vendor mismatch: {msg}"
    );
}

#[tokio::test]
async fn an_instance_that_is_not_a_plugin_envelope_is_excluded_not_fatal() {
    // A registry entry under this gear's own type prefix whose object does not
    // deserialize as `PluginV1`: a stale entry from an older format, or another
    // gear publishing a lookalike. It must be dropped, not taken down the whole
    // chain — and not silently demoted to a default priority either, which is why
    // the healthy source has the HIGHER priority number and must still serve.
    let broken = SourceSpec::ok("broken", "cf.bss", 10);
    let (broken_id, _json) = registration(&broken);
    let h = build(
        &[SourceSpec::ok("ecb", "cf.bss", 100)],
        vec![make_test_instance(
            &broken_id,
            serde_json::json!({ "unexpected": true }),
        )],
        false,
        "cf.bss",
    );
    let ctx = SecurityContext::anonymous();
    let rates = h.provider.fetch_latest(&ctx, &[], "req").await.unwrap();
    assert_eq!(
        rates[0].provider, "ecb",
        "the healthy source must serve despite the unparseable sibling"
    );
}

#[tokio::test]
async fn health_probes_the_discovered_sources() {
    let h = setup(
        &[
            SourceSpec::failing("ecb", "cf.bss", 100),
            SourceSpec::ok("bank", "cf.bss", 200),
        ],
        "cf.bss",
    );
    let ctx = SecurityContext::anonymous();
    // The composite probes in priority order and short-circuits on the first
    // reachable source: ecb is down, bank answers, so the composite is reachable.
    assert!(h.provider.health(&ctx, "req").await.is_ok());
}
