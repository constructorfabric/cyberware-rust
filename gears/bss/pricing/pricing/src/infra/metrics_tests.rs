//! The `OTel` adapter, asserted against a real in-memory exporter.
//!
//! Every case here reads the **exported** metric stream rather than a spy. The
//! difference is the whole reason the harness exists: a spy proves the adapter
//! called its own recording method, and this proves an instrument *by that
//! name, carrying those attributes* reached an exporter — which is the claim a
//! dashboard and an alerting rule actually depend on. A metric whose name
//! drifted, or whose label was spelled differently, passes a spy and fails here.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::test_harness::MetricsHarness;
use super::*;

use crate::domain::ports::metrics::{AlarmSeverity, NoopPricingMetrics};

// ---------------------------------------------------------------------------
// The names, transcribed from §10 rather than from the identifiers.
// ---------------------------------------------------------------------------

/// A constant whose value drifted from its name would still compile and would
/// still be wrong on the wire.
#[test]
fn the_metric_names_are_spelled_as_section_10_spells_them() {
    assert_eq!(
        PRICING_PREVIEW_FAILCLOSED,
        "pricing_preview_failclosed_total"
    );
    assert_eq!(
        PRICING_CURRENCY_BINDING_BLOCKS,
        "pricing_currency_binding_blocks_total"
    );
    assert_eq!(PRICING_TAX_NOT_SELLABLE_GA, "pricing_tax_not_sellable_ga");
    assert_eq!(PRICING_ALARM, "pricing_alarm_total");
}

/// Counters end `_total`, the gauge does not, and nothing carries a unit
/// suffix it has not earned.
///
/// The collector runs `add_metric_suffixes: false`, so the exporter renders
/// these verbatim — a counter that forgot `_total` would ship a name no
/// convention-following dashboard queries, and would do so silently.
#[test]
fn counter_names_end_in_total_and_the_gauge_is_bare() {
    for counter in [
        PRICING_PREVIEW_FAILCLOSED,
        PRICING_CURRENCY_BINDING_BLOCKS,
        PRICING_ALARM,
    ] {
        assert!(
            counter.ends_with("_total"),
            "{counter} is a counter and must say so in its name"
        );
    }
    assert!(
        !PRICING_TAX_NOT_SELLABLE_GA.ends_with("_total"),
        "a gauge that rises and falls must not be named like a monotonic counter"
    );
}

// ---------------------------------------------------------------------------
// The instruments actually reach an exporter.
// ---------------------------------------------------------------------------

#[test]
fn a_preview_refusal_increments_its_reason_and_nothing_else() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.preview_failclosed(PreviewFailClosed::MarketAbsent);
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "market_absent")]
        ),
        1
    );
    // The other reasons are untouched — a single bucket would have told an
    // operator the preview is refusing without telling them whose fault it is.
    assert_eq!(
        h.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "no_published_version")]
        ),
        0
    );
    assert_eq!(
        h.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "market_not_named")]
        ),
        0
    );
}

#[test]
fn every_preview_reason_is_a_series_of_its_own() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    for reason in PreviewFailClosed::ALL {
        m.preview_failclosed(*reason);
    }
    h.force_flush();

    for reason in PreviewFailClosed::ALL {
        assert_eq!(
            h.counter_value(
                "pricing_preview_failclosed_total",
                &[("reason", reason.as_str())]
            ),
            1,
            "{} must be its own series",
            reason.as_str()
        );
    }
}

#[test]
fn each_currency_binding_case_counts_separately() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.currency_binding_block(CurrencyBindingCase::RequiredAddon);
    m.currency_binding_block(CurrencyBindingCase::RequiredAddon);
    m.currency_binding_block(CurrencyBindingCase::BundleOwnPrice);
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "required_addon")]
        ),
        2
    );
    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "bundle_own_price")]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "bundle_sum_of_parts")]
        ),
        0
    );
}

/// A coverage report carrying one or more blocks, as the bundle walk leaves it.
fn blocked_report(blocks: usize) -> ValidationReport {
    let mut report = ValidationReport::default();
    for n in 0..blocks {
        report.violate(
            CURRENCY_NOT_COVERED,
            format!("component-{n}"),
            "no covering published row".to_owned(),
        );
    }
    report
}

/// The bundle cases are chosen by the composition's **basis**, never by the
/// violation's code (`T-17`, cases (ii) and (iii)).
///
/// The code cannot choose: `bundle_rules` and `currency_binding` raise the same
/// `CURRENCY_NOT_COVERED` string, so a derivation scanning it would label a
/// bundle's fault `required_addon` — invisibly, because the refusal an operator
/// sees would still be correct. **Both bases are exercised**, because one alone
/// cannot tell "reads the basis" from "hard-codes a case".
#[test]
fn the_bundle_binding_case_comes_from_the_basis_and_not_from_the_code() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    report_bundle_coverage_metrics(&m, PriceBasis::SumOfParts, &blocked_report(1));
    report_bundle_coverage_metrics(&m, PriceBasis::OwnPrice, &blocked_report(1));
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "bundle_sum_of_parts")]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "bundle_own_price")]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "required_addon")]
        ),
        0,
        "case (i) is the plan plane's and nothing here may wear its label"
    );
}

/// One rule run is one count, however many components missed a market.
///
/// §10 labels this `case`, and a bundle whose three components all miss one
/// market is one composition mistake. Paired with the control below, this is
/// what separates "counts runs" from "counts violations".
#[test]
fn a_bundle_blocked_on_three_components_counts_once() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    report_bundle_coverage_metrics(&m, PriceBasis::SumOfParts, &blocked_report(3));
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "bundle_sum_of_parts")]
        ),
        1
    );
}

/// A publishable composition counts nothing.
///
/// The control the two cases above need: without it both would pass against a
/// derivation that emitted on every rule run regardless of the verdict, which is
/// a counter measuring publishes rather than blocks.
#[test]
fn a_bundle_that_covers_every_market_counts_nothing() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    report_bundle_coverage_metrics(&m, PriceBasis::SumOfParts, &blocked_report(0));
    report_bundle_coverage_metrics(&m, PriceBasis::OwnPrice, &blocked_report(0));
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "bundle_sum_of_parts")]
        ),
        0
    );
    assert_eq!(
        h.counter_value(
            "pricing_currency_binding_blocks_total",
            &[("case", "bundle_own_price")]
        ),
        0
    );
}

/// The GA gauge reports the **latest** backlog, not a running total.
///
/// The question C3 poses is how many markets are gated *now*; the number falls
/// when the engine GAs and the affected plans re-publish. A counter would only
/// rise, and would answer "how many were ever gated" — which is not the backlog
/// anyone manages.
#[test]
fn the_ga_gauge_reports_the_latest_backlog_rather_than_a_running_total() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.tax_not_sellable_ga(7);
    m.tax_not_sellable_ga(3);
    h.force_flush();

    assert_eq!(h.gauge_value("pricing_tax_not_sellable_ga", &[]), 3);
}

/// The gauge can fall to zero, which is the state an operator is waiting for.
#[test]
fn the_ga_gauge_can_fall_to_zero() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.tax_not_sellable_ga(4);
    m.tax_not_sellable_ga(0);
    h.force_flush();

    // `gauge_point`, not `gauge_value`: the latter answers `0` for a series that
    // was never recorded, so it cannot tell a fall to zero from an instrument
    // that was renamed, silenced or built as a counter.
    assert_eq!(h.gauge_point("pricing_tax_not_sellable_ga", &[]), Some(0));
}

/// **The harness reads the newest cumulative snapshot, not the sum of them.**
///
/// `InMemoryMetricExporter` defaults to `Temporality::Cumulative`: every
/// collection appends a fresh running total, and a reader that added the batches
/// together would answer 3 here. Held because the first later slice to write
/// `emit; flush; assert; emit; flush; assert` gets a wrong number otherwise, and
/// would have no reason to suspect the harness.
#[test]
fn a_second_flush_does_not_double_count_the_first() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.preview_failclosed(PreviewFailClosed::MarketAbsent);
    h.force_flush();
    assert_eq!(
        h.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "market_absent")]
        ),
        1
    );

    m.preview_failclosed(PreviewFailClosed::MarketAbsent);
    h.force_flush();
    assert_eq!(
        h.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "market_absent")]
        ),
        2,
        "two increments and two flushes are two, not three"
    );
}

/// A gauge read after a second flush is the **latest** value, not the oldest.
#[test]
fn a_gauge_read_after_two_flushes_is_the_later_value() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.tax_not_sellable_ga(4);
    h.force_flush();
    m.tax_not_sellable_ga(1);
    h.force_flush();

    assert_eq!(h.gauge_point("pricing_tax_not_sellable_ga", &[]), Some(1));
}

// ---------------------------------------------------------------------------
// The alarm rollup.
// ---------------------------------------------------------------------------

/// One counter for every alarm, discriminated by label.
#[test]
fn an_alarm_counts_on_the_rollup_under_its_declared_name() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.alarm(PricingAlarm::TaxNotSellableGaActive);
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_alarm_total",
            &[
                ("alarm", "pricing.tax.not_sellable_ga_active"),
                ("severity", "info")
            ]
        ),
        1
    );
}

/// **The severity comes from the alarm, not from the caller.**
///
/// Two firings of one alarm must not be able to disagree about how urgent it
/// is — an alerting rule keyed on `severity` would then fire for some of an
/// alarm's occurrences and not others.
#[test]
fn an_alarms_severity_is_a_property_of_the_alarm() {
    let h = MetricsHarness::new();
    let m = h.metrics();

    m.alarm(PricingAlarm::TaxReadinessDivergent);
    m.alarm(PricingAlarm::TaxReadinessDivergent);
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_alarm_total",
            &[
                ("alarm", "pricing.tax.readiness_divergent"),
                ("severity", "warn")
            ]
        ),
        2,
        "both firings land on the one severity the design set declares"
    );
    assert_eq!(
        h.counter_value(
            "pricing_alarm_total",
            &[
                ("alarm", "pricing.tax.readiness_divergent"),
                ("severity", "info")
            ]
        ),
        0
    );
}

/// Every declared alarm's name is the **dotted** form the design set spells.
///
/// An operator greps the design document for `pricing.tax.readiness_divergent`
/// and has to find the series. A `snake_case` rewrite would make the document
/// and the dashboard two vocabularies.
#[test]
fn alarm_labels_are_the_dotted_names_the_design_set_declares() {
    // The literal spellings, transcribed from §7. A shape assertion —
    // "starts with `pricing.`, contains a dot" — is satisfied by
    // `pricing.tax.not_sellable_ga`, which is a name §7 does not declare and no
    // alerting rule matches.
    let declared: Vec<(&str, &str)> = PricingAlarm::ALL
        .iter()
        .map(|a| (a.as_str(), a.severity().as_str()))
        .collect();
    assert_eq!(
        declared,
        [
            // `pricing.window.activation_overdue` is the newest and the reason
            // this roster earns its keep: it was raised as a bare
            // `tracing::error!` for a whole slice under the argument that this
            // gear had no alarm plane — the third instance of the defect D-238
            // closed twice, living under a decision filed BUILT.
            ("pricing.tax.not_sellable_ga_active", "info"),
            ("pricing.tax.readiness_divergent", "warn"),
            // Foundation's two, added by D-238. **This census is what caught
            // them being added** — the roster grew and this list did not, and the
            // case went red rather than the two new names shipping untranscribed.
            // That is the whole argument for transcribing instead of deriving.
            ("pricing.catalogversion.commit_overdue", "critical"),
            ("pricing.readmodel.pin_eligibility_overdue", "critical"),
            ("pricing.window.activation_overdue", "warn"),
            // The one entry §7 does not declare, transcribed from
            // `PricingAlarm::FixtureGateUnreadable`'s own doc rather than from the
            // design set, because there is nothing there to transcribe it from —
            // it is a fault of the deployment's artifact and §7 enumerates faults
            // of the catalog.
            ("pricing.fixture_gate.unreadable", "critical"),
            // Also absent from §7, and for the adjacent reason: the set declares
            // the region rules and not an alarm for their having no substrate.
            // Transcribed from `PricingAlarm::RegionGrantUntransported`'s doc.
            ("pricing.authz.region_grant_untransported", "warn"),
        ]
    );
}

/// Every severity is a legal label, so a later slice adding a Critical alarm
/// does not discover the vocabulary is short.
#[test]
fn every_severity_renders_a_label() {
    let rendered: Vec<&str> = AlarmSeverity::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(rendered, ["info", "warn", "critical"]);
}

// ---------------------------------------------------------------------------
// The no-op.
// ---------------------------------------------------------------------------

/// The default port answers every method behind the `Arc<dyn …>` the surfaces
/// hold, without panicking.
///
/// **That is the whole claim, and it is not "does nothing"** — there is no
/// observer to prove a no-op against, and inventing one would be testing the
/// double. What this holds is that a service built before an exporter is wired
/// can be called on every method of the port, so a missing exporter can never be
/// the reason a publish fails. Covers dyn dispatch, which a concrete-type call
/// would not.
#[test]
fn the_noop_port_answers_every_method_behind_a_dyn_port() {
    let m: std::sync::Arc<dyn PricingMetricsPort> = std::sync::Arc::new(NoopPricingMetrics);
    m.preview_failclosed(PreviewFailClosed::MarketAbsent);
    m.currency_binding_block(CurrencyBindingCase::RequiredAddon);
    m.tax_not_sellable_ga(9);
    m.alarm(PricingAlarm::TaxNotSellableGaActive);
}

/// The adapter is usable behind the same `Arc<dyn …>`.
#[test]
fn the_otel_adapter_is_usable_as_a_dyn_port() {
    let h = MetricsHarness::new();
    let m: std::sync::Arc<dyn PricingMetricsPort> = std::sync::Arc::new(h.metrics());

    m.preview_failclosed(PreviewFailClosed::NoPublishedVersion);
    h.force_flush();

    assert_eq!(
        h.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "no_published_version")]
        ),
        1,
        "dyn dispatch reaches the same instrument"
    );
}

/// Two harnesses do not share a meter provider.
///
/// These cases run in one process; a shared global would let one test's counter
/// decide another's assertion — the same reason every repository suite in this
/// crate builds its own database.
#[test]
fn two_harnesses_are_isolated_from_each_other() {
    let a = MetricsHarness::new();
    let b = MetricsHarness::new();

    a.metrics()
        .preview_failclosed(PreviewFailClosed::MarketAbsent);
    a.force_flush();
    b.force_flush();

    assert_eq!(
        a.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "market_absent")]
        ),
        1
    );
    assert_eq!(
        b.counter_value(
            "pricing_preview_failclosed_total",
            &[("reason", "market_absent")]
        ),
        0,
        "one harness's emission must not be visible in another"
    );
}

// ---------------------------------------------------------------------------
// The publish path's derivation (`report_market_metrics`).
// ---------------------------------------------------------------------------

mod publish_path {
    use std::collections::{BTreeMap, BTreeSet};

    use uuid::Uuid;

    use super::MetricsHarness;
    use crate::domain::concurrency::RowVersion;
    use crate::domain::currency_binding::{AddonCoverage, Market};
    use crate::domain::instant::utc_ymd_hms;
    use crate::domain::lifecycle::LifecycleState;
    use crate::domain::money::{CurrencyCode, MinorAmount};
    use crate::domain::plan_rules::{CustomIntervalBounds, DescriptorSetComplete};
    use crate::domain::plan_shape::{AddonRule, PlanShape};
    use crate::domain::price_record::PriceRecord;
    use crate::domain::price_row::PriceRow;
    use crate::domain::publish::rules::{PublishRuleParams, SoftSizeCaps};
    use crate::domain::scope_key::{
        ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
    };
    use crate::infra::metrics::report_market_metrics;
    use time::OffsetDateTime;

    const ADDON: Uuid = Uuid::from_u128(0x0add_000a);
    const BLOCKS: &str = "pricing_currency_binding_blocks_total";
    const ALARM: &str = "pricing_alarm_total";
    const GA_ALARM: &str = "pricing.tax.not_sellable_ga_active";

    fn now() -> OffsetDateTime {
        utc_ymd_hms(2026, 8, 6, 12, 0, 0)
    }

    fn market(currency: &str, region: &str) -> Market {
        (
            CurrencyCode::new(currency).expect("three letters"),
            Region::new(region).expect("non-blank"),
        )
    }

    /// One candidate row on a market, at an eligibility and a tax basis.
    fn row(
        price_id: u128,
        currency: &str,
        region: &str,
        tax_inclusive: bool,
        eligibility: PriceEligibility,
    ) -> PriceRecord {
        let cohort = if eligibility == PriceEligibility::ExistingGrandfathered {
            Cohort::Generation(now())
        } else {
            Cohort::None
        };
        let scope_key = ScopeKey::new(
            PlanId::new(Uuid::from_u128(0x91a4)),
            CurrencyCode::new(currency).expect("three letters"),
            Region::new(region).expect("non-blank"),
            PhaseId::new(Uuid::from_u128(0xf1)),
            eligibility,
            ChargeKind::Recurring,
            cohort,
        )
        .expect("the eligibility and cohort pair");

        let mut shape = PriceRow::new(ChargeKind::Recurring, None);
        shape.amount_minor = Some(MinorAmount::new(1000).expect("non-negative"));

        PriceRecord {
            price_id: Uuid::from_u128(price_id),
            scope_key,
            row: shape,
            tax_inclusive,
            tax_category_ref: None,
            billing_timing: None,
            proration_contract: None,
            rounding_policy_ref: None,
            grandfather_until: None,
            supersedes_price_id: None,
            lifecycle_state: LifecycleState::Draft,
            created_by: Uuid::from_u128(0xac_10),
            created_at_utc: now(),
            row_version: RowVersion::new(0),
        }
    }

    /// A plan whose rows are exactly the given `(currency, region, taxInclusive)`
    /// triples, with one required add-on when `required_addon` is set.
    fn shape_of(rows: &[(&str, &str, bool)], required_addon: bool) -> PlanShape {
        let mut shape = PlanShape::new(PlanId::new(Uuid::from_u128(0x91a4)), 1, now());
        shape.rows = rows
            .iter()
            .enumerate()
            .map(|(n, (c, r, tax))| {
                row(
                    0xb000 + n as u128,
                    c,
                    r,
                    *tax,
                    PriceEligibility::AllSubscriptions,
                )
            })
            .collect();
        if required_addon {
            shape.addon_rules = vec![AddonRule {
                addon_sku_id: ADDON,
                required: true,
                min_qty: None,
                max_qty: None,
                step_qty: None,
                price_override_ref: None,
                depends_on: Vec::new(),
                conflicts_with: Vec::new(),
            }];
        }
        shape
    }

    /// Parameters whose add-on coverage is exactly the given markets.
    fn params_covering(markets: &[(&str, &str)]) -> PublishRuleParams {
        // The ratified launch caps rather than zeros: nothing here is about the
        // interval or size rules, and a zero cap would put the shape in a state
        // they reject instead.
        PublishRuleParams::new(
            CustomIntervalBounds::new(366, 24),
            DescriptorSetComplete::default(),
            None,
            SoftSizeCaps::new(100, 500),
        )
        .with_addon_coverage(AddonCoverage::new(
            [(
                ADDON,
                markets
                    .iter()
                    .map(|(c, r)| market(c, r))
                    .collect::<BTreeSet<Market>>(),
            )]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
        ))
    }

    /// An add-on missing a market the plan sells is counted, under case (i)'s
    /// label.
    #[test]
    fn an_uncovered_required_addon_counts_one_block() {
        let h = MetricsHarness::new();

        report_market_metrics(
            &h.metrics(),
            &shape_of(&[("EUR", "EU", false), ("USD", "US", false)], true),
            &params_covering(&[("EUR", "EU")]),
        );
        h.force_flush();

        assert_eq!(h.counter_value(BLOCKS, &[("case", "required_addon")]), 1);
    }

    /// **One block per publish, not one per offending add-on or market.**
    ///
    /// §10 labels this `case`, and a plan missing three markets is one authoring
    /// mistake. A counter that grew with the gap would make the same fault look
    /// worse the wider it was.
    #[test]
    fn a_gap_of_three_markets_is_still_one_block() {
        let h = MetricsHarness::new();

        report_market_metrics(
            &h.metrics(),
            &shape_of(
                &[
                    ("EUR", "EU", false),
                    ("USD", "US", false),
                    ("GBP", "UK", false),
                    ("JPY", "JP", false),
                ],
                true,
            ),
            &params_covering(&[("EUR", "EU")]),
        );
        h.force_flush();

        assert_eq!(h.counter_value(BLOCKS, &[("case", "required_addon")]), 1);
    }

    /// A plan whose add-on covers everything it sells counts nothing.
    ///
    /// The negative control: a derivation that counted every publish would pass
    /// every assertion above and would report a healthy catalog as permanently
    /// blocked.
    #[test]
    fn a_fully_covered_plan_counts_no_block() {
        let h = MetricsHarness::new();

        report_market_metrics(
            &h.metrics(),
            &shape_of(&[("EUR", "EU", false)], true),
            &params_covering(&[("EUR", "EU"), ("USD", "US")]),
        );
        h.force_flush();

        assert_eq!(h.counter_value(BLOCKS, &[("case", "required_addon")]), 0);
    }

    /// A plan composing no add-on is no case (i) block, and wears no other
    /// case's label either.
    ///
    /// # What this does **not** hold, stated because the name once claimed it did
    ///
    /// `bundle_rules` raises the *same* `CURRENCY_NOT_COVERED` string as case
    /// (i), and the hazard is a derivation that scans the report by code and
    /// labels a bundle's fault `required_addon`. **No case here can see that**:
    /// staging it needs a bundle publish, whose rules live in a file this strand
    /// is forbidden, and this one seeds no bundle at all — so a derivation
    /// rewritten to scan the report would pass it unchanged.
    ///
    /// What closes the hazard is structural rather than tested:
    /// `report_market_metrics` takes no `ValidationReport`, so it *cannot* scan
    /// one. That is a compile-time fact and a stronger guarantee than a case,
    /// which is why this one is left holding the narrower claim its assertions
    /// actually make. Cases (ii) and (iii) have no emitter at all — `T-17`.
    #[test]
    fn a_plan_composing_no_addon_is_no_block_of_any_case() {
        let h = MetricsHarness::new();

        report_market_metrics(
            &h.metrics(),
            &shape_of(&[("EUR", "EU", false), ("USD", "US", false)], false),
            &params_covering(&[]),
        );
        h.force_flush();

        assert_eq!(
            h.counter_value(BLOCKS, &[("case", "required_addon")]),
            0,
            "a plan composing no add-on cannot be a case (i) block"
        );
        assert_eq!(
            h.counter_value(BLOCKS, &[("case", "bundle_sum_of_parts")]),
            0
        );
        assert_eq!(h.counter_value(BLOCKS, &[("case", "bundle_own_price")]), 0);
    }

    /// A plan gating at least one market raises §7's Info alarm.
    #[test]
    fn a_plan_gating_a_market_raises_the_ga_alarm() {
        let h = MetricsHarness::new();

        report_market_metrics(
            &h.metrics(),
            &shape_of(&[("EUR", "EU", true), ("USD", "US", false)], false),
            &params_covering(&[]),
        );
        h.force_flush();

        assert_eq!(h.counter_value(ALARM, &[("alarm", GA_ALARM)]), 1);
    }

    /// **One alarm per publish, however many markets it gates.**
    ///
    /// The alarm says "this plan would gate markets"; it is not a count of them.
    /// A firing per market would make one authoring decision look like four
    /// incidents, and §7 declares an alarm rather than a measure. The *count* is
    /// the gauge's job, and the gauge has no honest writer on a per-plan path —
    /// see `PricingMetricsPort::tax_not_sellable_ga` and `T-17`.
    #[test]
    fn many_gated_markets_are_still_one_alarm() {
        let h = MetricsHarness::new();

        report_market_metrics(
            &h.metrics(),
            &shape_of(
                &[
                    ("EUR", "EU", true),
                    ("EUR", "EU", true),
                    ("GBP", "UK", true),
                    ("USD", "US", false),
                ],
                false,
            ),
            &params_covering(&[]),
        );
        h.force_flush();

        assert_eq!(h.counter_value(ALARM, &[("alarm", GA_ALARM)]), 1);
    }

    /// A plan gating nothing raises nothing.
    ///
    /// The negative control: an alarm raised on every publish would satisfy both
    /// cases above and would page somebody for every tax-exclusive plan in the
    /// catalog.
    #[test]
    fn a_plan_gating_nothing_raises_no_alarm() {
        let h = MetricsHarness::new();

        report_market_metrics(
            &h.metrics(),
            &shape_of(&[("USD", "US", false)], false),
            &params_covering(&[]),
        );
        h.force_flush();

        assert_eq!(h.counter_value(ALARM, &[("alarm", GA_ALARM)]), 0);
    }

    /// **A market reachable only through a frozen generation is not gated.**
    ///
    /// `sold_markets`' rule and its reason (ADR-0002): a grandfathered generation
    /// is not a market the plan sells, and no re-publish can ever un-gate it — so
    /// counting it would put a market into a backlog no action clears, and §7's
    /// alarm would stand for the life of the plan.
    #[test]
    fn a_grandfathered_tax_inclusive_market_does_not_raise_the_alarm() {
        let h = MetricsHarness::new();
        let mut shape = shape_of(&[("USD", "US", false)], false);
        shape.rows.push(row(
            0xb0ff,
            "EUR",
            "EU",
            true,
            PriceEligibility::ExistingGrandfathered,
        ));

        report_market_metrics(&h.metrics(), &shape, &params_covering(&[]));
        h.force_flush();

        assert_eq!(h.counter_value(ALARM, &[("alarm", GA_ALARM)]), 0);
    }
}
