//! `GET /plans/{planId}/preview`, driven through the real router
//! (`design/04-currency-tax.md` §2, `inst-pv-api` / `inst-pv-resolve` /
//! `inst-pv-return`, `inst-mc-nofx`).
//!
//! # The fail-closed cases are the point of the flow
//!
//! `inst-mc-nofx` is the slice's sharpest sentence — *"No FX derivation ever … no
//! base-currency fallback"* — and the way an implementation gets it wrong is not
//! by writing an FX call, it is by answering *something* for a market it has no
//! row on: the nearest currency, the same currency in another region, or an empty
//! 200. Each of those is a case here, and each is asserted to be a **404**.
//!
//! The positive control comes first regardless, because a preview that answered
//! 404 to everything would satisfy every one of them.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use std::collections::BTreeMap;

use axum::http::StatusCode;
use bss_pricing::api::rest::preview::PLAN_PREVIEW;
use bss_pricing::domain::contracts::{EntitlementGrants, PlanChangeContract};

use bss_pricing::domain::instant::utc_ymd_hms;
use rest_support::{Harness, body_json, request};
use time::OffsetDateTime;
use uuid::Uuid;

const CURRENCY: &str = "EUR";
const REGION: &str = "EU";

fn at() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 1, 0, 0, 0)
}

fn preview_path(plan_id: Uuid, query: &str) -> String {
    format!(
        "{}?{query}",
        PLAN_PREVIEW.replace("{planId}", &plan_id.to_string())
    )
}

/// A published plan-subject delta selling one market.
fn delta_of(
    plan_id: Uuid,
    currency: &str,
    region: &str,
    tax_inclusive: bool,
    resolved_category: Option<&str>,
    not_sellable_ga: bool,
    grandfathered: bool,
) -> bss_pricing::domain::projection::PlanSubjectDelta {
    use bss_pricing::domain::concurrency::RowVersion;
    use bss_pricing::domain::lifecycle::LifecycleState;
    use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
    use bss_pricing::domain::price_record::PriceRecord;
    use bss_pricing::domain::price_row::{ModelKind, PriceRow};
    use bss_pricing::domain::projection::{PlanSubjectDelta, RowResolutionProjection};
    use bss_pricing::domain::scope_key::{
        ChargeKind, Cohort, PlanId, PriceEligibility, Region, ScopeKey,
    };

    let (eligibility, cohort) = if grandfathered {
        (
            PriceEligibility::ExistingGrandfathered,
            Cohort::Generation(at()),
        )
    } else {
        (PriceEligibility::AllSubscriptions, Cohort::None)
    };
    let key = ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new(currency).expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        rest_support::seeded_phase(),
        eligibility,
        ChargeKind::Recurring,
        cohort,
    )
    .expect("the class pairs with its cohort");

    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(1_200).expect("a non-negative amount"));
    let price_id = Uuid::from_u128(0xb_0001);

    let mut delta = PlanSubjectDelta {
        entitlement_grants: EntitlementGrants::default(),
        // Empty: no predicate this fixture drives reads a derived meter, and a
        // populated set here would be a seed no assertion reads back.
        composites: Vec::new(),
        change_contract: PlanChangeContract::default(),
        plan_id: PlanId::new(plan_id),
        revision: 0,
        lifecycle_state: LifecycleState::Published,
        sku_id: None,
        plan_tier: Some("gold".to_owned()),
        plan_tier_override: false,
        billing_cycle: None,
        frequency: None,
        available_from: Some(at()),
        available_to: None,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: None,
        phases: Vec::new(),
        addon_rules: Vec::new(),
        descriptor_set: None,
        period_floor_caps: Vec::new(),
        prices: Vec::new(),
        tax_projection: BTreeMap::new(),
        windows: Vec::new(),
    };
    delta.prices = vec![PriceRecord {
        price_id,
        scope_key: key,
        row,
        tax_inclusive,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Published,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(),
        row_version: RowVersion::new(0),
    }];
    delta.tax_projection = [(
        price_id,
        RowResolutionProjection {
            resolved_tax_category: resolved_category.map(ToOwned::to_owned),
            resolved_rounding_policy: Some("half_up".to_owned()),
            not_sellable_ga,
        },
    )]
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    delta
}

/// Freeze a delta at `version` and make that version pin-eligible.
async fn project_and_pin(
    h: &Harness,
    plan_id: Uuid,
    version: u64,
    delta: &bss_pricing::domain::projection::PlanSubjectDelta,
) {
    use bss_pricing::domain::read_model::SubjectRef;
    use bss_pricing::infra::storage::repo::{NewDelta, pin_frontier_repo, read_model_repo};
    use bss_pricing_sdk::CatalogVersion;

    let conn = h.db.conn().expect("conn");
    read_model_repo::project_subject(
        &conn,
        &h.scope(),
        NewDelta {
            tenant_id: h.tenant,
            catalog_version: CatalogVersion::new(version),
            subject: SubjectRef::Plan(plan_id),
            payload: delta.to_value(),
            projected_at: at(),
        },
    )
    .await
    .expect("project the plan subject");
    pin_frontier_repo::advance(
        &conn,
        &h.scope(),
        h.tenant,
        CatalogVersion::new(version),
        at(),
    )
    .await
    .expect("make the version pin-eligible");
}

/// A harness with one published plan selling `EUR/EU`.
async fn seeded(h: &Harness) -> Uuid {
    let plan_id = Uuid::now_v7();
    project_and_pin(
        h,
        plan_id,
        5,
        &delta_of(
            plan_id,
            CURRENCY,
            REGION,
            false,
            Some("standard"),
            false,
            false,
        ),
    )
    .await;
    plan_id
}

/// The wire code off a `404` problem document.
///
/// **Not `rest_support::problem_code`**, and the difference is a real property of
/// the platform rather than a helper preference: that helper reads the `reason` a
/// `failed_precondition`/`aborted` document carries, and the canonical
/// **not-found** family has no such slot — so `PRICE_ROW_ABSENT` rides
/// `context.resource_name` instead. §5 declares the code and the status together
/// and does not say where the code sits, so this is a divergence in *rendering*
/// rather than in the contract; it is recorded as `T-11`.
///
/// Asserted rather than skipped, because the status alone is not the claim: a
/// 404 from a mistyped path and a 404 from an unsold market are different facts
/// and only the code separates them.
async fn absent_code(response: axum::http::Response<axum::body::Body>) -> String {
    // The reading moved to `rest_support::not_found_code` when a second surface
    // needed it (D-278). This stays as the name this suite's cases read by.
    rest_support::not_found_code(response).await
}

// ---------------------------------------------------------------------------
// The positive control.
// ---------------------------------------------------------------------------

/// A market the plan sells is previewed, with the disclaimer §2 requires.
#[tokio::test]
async fn a_sold_market_previews_with_its_amount_and_the_disclaimer() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["amount_minor"], 1_200);
    assert_eq!(body["currency"], CURRENCY);
    assert_eq!(body["region"], REGION);
    assert_eq!(body["catalog_version"], 5);
    assert_eq!(
        body["resolved_tax_category"], "standard",
        "the version's frozen category, not a re-resolution"
    );
    assert!(
        body["disclaimer"]
            .as_str()
            .expect("a disclaimer")
            .contains("PriceOverlays"),
        "section 2 requires an explicit disclaimer that overlays apply at purchase"
    );
}

// ---------------------------------------------------------------------------
// `inst-mc-nofx` — the three shapes of "answering something anyway".
// ---------------------------------------------------------------------------

/// A currency the plan does not sell is `404`, never a converted price.
#[tokio::test]
async fn an_unsold_currency_fails_closed_rather_than_converting() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency=JPY&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent_code(response).await, "PRICE_ROW_ABSENT");
}

/// **The same currency in another region is not a hit.**
///
/// The market is the pair (D-95), so a preview that matched on currency alone
/// would quote a partner in `US` the price authored for `EU`. That is the
/// sharpest of the three fallbacks because it is the one an implementation
/// reaches for by accident.
#[tokio::test]
async fn the_same_currency_in_another_region_is_not_a_hit() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region=US")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent_code(response).await, "PRICE_ROW_ABSENT");
}

/// A plan with **no published version at all** is `404`, not an empty 200.
#[tokio::test]
async fn a_plan_with_no_published_version_fails_closed() {
    let h = Harness::new().await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(
                Uuid::now_v7(),
                &format!("currency={CURRENCY}&region={REGION}"),
            ),
            None,
        ))
        .await;

    // The code, as every other 404 in this file asserts it. All three of this
    // door's absences deliberately answer one code, so what this buys is not a
    // three-way discrimination but the fact that the 404 carries the family's code
    // at all - which a bare status cannot show and which a 404 minted somewhere
    // else on the path would not have.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent_code(response).await, "PRICE_ROW_ABSENT");
}

/// A market reachable **only** through a grandfathered generation is not
/// previewable.
///
/// Added after a probe removing the exclusion reddened nothing: no case seeded a
/// grandfathered row, so the branch was unheld. `inst-pv-resolve` says "base list
/// price rows only", and quoting a frozen generation would show a **prospective**
/// purchaser a price only an existing subscriber can have — the preview's whole
/// audience is people who do not have a subscription yet.
#[tokio::test]
async fn a_grandfathered_only_market_is_not_previewable() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    project_and_pin(
        &h,
        plan_id,
        5,
        &delta_of(
            plan_id,
            CURRENCY,
            REGION,
            false,
            Some("standard"),
            false,
            true,
        ),
    )
    .await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent_code(response).await, "PRICE_ROW_ABSENT");
}

/// **A hybrid market has more than one row, and the preview must not pick at
/// random.**
///
/// One `(currency, region)` legitimately carries a recurring row *and* usage rows
/// — different `chargeKind`, `meter` and `dimensionKey` are all scope-key axes.
/// §2 says the preview returns "the catalog **base list price**", and a usage row
/// has no single amount at all: its money lives in tier bands, so `amountMinor`
/// is NULL on it.
///
/// So a preview that took whichever row came first would answer `null` for a
/// plan that plainly has a monthly price, and *which* row came first would depend
/// on the projector's array order rather than on anything an operator authored.
#[tokio::test]
async fn a_hybrid_market_previews_its_recurring_row_and_not_a_usage_row() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    // The usage row is projected FIRST, so a first-match implementation picks it.
    let delta = hybrid_delta(plan_id);
    project_and_pin(&h, plan_id, 5, &delta).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["amount_minor"], 1_200,
        "the base list price is the recurring row's amount, not the usage row's absent one"
    );
}

/// One market, two rows: a usage row first, then the recurring base row.
fn hybrid_delta(plan_id: Uuid) -> bss_pricing::domain::projection::PlanSubjectDelta {
    use bss_pricing::domain::concurrency::RowVersion;
    use bss_pricing::domain::lifecycle::LifecycleState;
    use bss_pricing::domain::money::CurrencyCode;
    use bss_pricing::domain::price_record::PriceRecord;
    use bss_pricing::domain::price_row::{ModelKind, PriceRow};
    use bss_pricing::domain::scope_key::{
        ChargeKind, Cohort, DimensionKey, Meter, PlanId, PriceEligibility, Region, ScopeKey,
    };

    let mut delta = delta_of(
        plan_id,
        CURRENCY,
        REGION,
        false,
        Some("standard"),
        false,
        false,
    );

    let usage_key = ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new(CURRENCY).expect("three letters"),
        Region::new(REGION).expect("a non-blank region"),
        rest_support::seeded_phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
    .with_usage_line(
        Some(Meter::new("api_calls").expect("a non-blank meter")),
        DimensionKey::none(),
    )
    .expect("a usage line names its meter");

    let mut usage_row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    usage_row.meter = Some("api_calls".to_owned());
    // A usage row's money is in its bands; `amount_minor` is NULL by rule.
    usage_row.amount_minor = None;

    let usage = PriceRecord {
        price_id: Uuid::from_u128(0xb_0002),
        scope_key: usage_key,
        row: usage_row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Published,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(),
        row_version: RowVersion::new(0),
    };
    delta.prices.insert(0, usage);
    delta
}

/// A trial phase beside the steady state: the preview quotes **the steady state**
/// (D-244).
///
/// §2 used to say "the catalog base list price" as though a market had one, and
/// this handler picked the first non-usage row carrying an amount with ties broken
/// on `priceId` — deterministic and, as `T-12` recorded, arbitrary. **The fixture
/// is built so the arbitrary answer is the wrong one**: the trial row's id sorts
/// first, so the old selection quoted a prospective purchaser 1.00 for a plan that
/// charges 12.00 from the second month.
///
/// Terminality is read from `convertsToPhaseId` being null and never from `kind`,
/// because C-4 exists because those two were once conflated.
#[tokio::test]
async fn a_market_with_a_trial_phase_quotes_the_terminal_phases_row() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let delta = trial_and_steady_delta(plan_id);
    project_and_pin(&h, plan_id, 5, &delta).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["amount_minor"], 1_200,
        "the steady state is what a purchaser is charged first; the trial row's \
         id sorts ahead of it and used to win"
    );
    // **`displayTrialDays` reaches the wire**, and this fixture is the only one
    // that seeds it. A crate-wide grep on 2026-08-20 found no test reading this
    // member at all, so `preview.rs`'s `display_trial_days` mapping could be dropped,
    // mis-keyed or
    // made permanently `None` with the whole preview suite green — and the trial
    // length is the one number a purchaser reads off a trial offer.
    assert_eq!(
        body["display_trial_days"], 14,
        "the trial phase's own `displayTrialDays`, not the terminal phase's null: {body}"
    );
}

/// One market, two recurring rows on two phases: a trial that converts, and the
/// terminal phase it converts into.
fn trial_and_steady_delta(plan_id: Uuid) -> bss_pricing::domain::projection::PlanSubjectDelta {
    use bss_pricing::domain::concurrency::RowVersion;
    use bss_pricing::domain::lifecycle::LifecycleState;
    use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
    use bss_pricing::domain::plan_shape::{PhaseKind, PlanPhase};
    use bss_pricing::domain::price_record::PriceRecord;
    use bss_pricing::domain::price_row::{ModelKind, PriceRow};
    use bss_pricing::domain::scope_key::{
        ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
    };

    let mut delta = delta_of(
        plan_id,
        CURRENCY,
        REGION,
        false,
        Some("standard"),
        false,
        false,
    );

    let trial_phase = PhaseId::new(Uuid::from_u128(0x7_41a1));
    // The chain, and terminality is **structural**: the steady state converts to
    // nothing. `kind` is deliberately not what says so.
    delta.phases = vec![
        PlanPhase {
            phase_id: trial_phase,
            kind: PhaseKind::Trial,
            display_name: None,
            ordinal: 0,
            converts_to_phase_id: Some(rest_support::seeded_phase()),
            phase_duration_days: Some(14),
            display_trial_days: Some(14),
        },
        PlanPhase {
            phase_id: rest_support::seeded_phase(),
            kind: PhaseKind::Evergreen,
            display_name: None,
            ordinal: 1,
            converts_to_phase_id: None,
            phase_duration_days: None,
            display_trial_days: None,
        },
    ];

    let trial_key = ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new(CURRENCY).expect("three letters"),
        Region::new(REGION).expect("a non-blank region"),
        trial_phase,
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with cohort none");

    let mut trial_row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    trial_row.amount_minor = Some(MinorAmount::new(100).expect("a non-negative amount"));

    delta.prices.insert(
        0,
        PriceRecord {
            // Sorts **before** the steady-state row's `0xb_0001`, which is the
            // whole point of the fixture.
            price_id: Uuid::from_u128(0xa_0001),
            scope_key: trial_key,
            row: trial_row,
            tax_inclusive: false,
            tax_category_ref: None,
            billing_timing: None,
            proration_contract: None,
            rounding_policy_ref: None,
            grandfather_until: None,
            supersedes_price_id: None,
            lifecycle_state: LifecycleState::Published,
            created_by: Uuid::from_u128(0xac_10),
            created_at_utc: at(),
            row_version: RowVersion::new(0),
        },
    );
    delta
}

/// **A repriced market quotes the successor, never the superseded predecessor.**
///
/// Found by review and, at first, *fixed without a test* — the probe removing the
/// `lifecycleState` filter reddened nothing, which is how a remedy gets believed
/// rather than proven.
///
/// `PROJECTED_ROW_STATES` includes `superseded`, and a supersession stages the
/// successor on the **same** `ScopeKey` while flipping its predecessor — so a
/// market that has ever been repriced carries two byte-identical keys in the
/// frozen delta. The predecessor is seeded first here, so a filter-less
/// implementation picks it.
#[tokio::test]
async fn a_repriced_market_quotes_the_successor_and_not_the_superseded_row() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let mut delta = delta_of(
        plan_id,
        CURRENCY,
        REGION,
        false,
        Some("standard"),
        false,
        false,
    );
    // The delta's own row is the successor at 1_200. Put a superseded
    // predecessor at 9_900 ahead of it on the identical key.
    let mut predecessor = delta.prices[0].clone();
    predecessor.price_id = Uuid::from_u128(0x0000_0001);
    predecessor.lifecycle_state = bss_pricing::domain::lifecycle::LifecycleState::Superseded;
    predecessor.row.amount_minor =
        Some(bss_pricing::domain::money::MinorAmount::new(9_900).expect("non-negative"));
    delta.prices.insert(0, predecessor);
    project_and_pin(&h, plan_id, 5, &delta).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["amount_minor"],
        1_200,
        "a frozen version is never re-projected, so quoting the predecessor would quote a \
         price nobody sells for the life of that version"
    );
}

/// **A version frozen before `notSellableGa` existed reads as gated, not
/// sellable.**
///
/// Deltas are INSERT-only and resolvable forever, so any version projected before
/// the field was added carries no such key. Defaulting an absent gate to `false`
/// tells a partner a C3-gated market is sellable, in a handler whose own header
/// is titled "fail closed", and it cannot heal.
///
/// Driven by projecting a **hand-built payload** with the key removed, which is
/// the only way to reach the state — the current projector always writes it.
#[tokio::test]
async fn a_payload_predating_the_ga_flag_reads_as_gated() {
    use bss_pricing::domain::read_model::SubjectRef;
    use bss_pricing::infra::storage::repo::{NewDelta, pin_frontier_repo, read_model_repo};
    use bss_pricing_sdk::CatalogVersion;

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let delta = delta_of(
        plan_id,
        CURRENCY,
        REGION,
        true,
        Some("standard"),
        true,
        false,
    );
    let mut payload = delta.to_value();
    for row in payload["prices"]
        .as_array_mut()
        .expect("the payload carries rows")
    {
        row.as_object_mut()
            .expect("a row is an object")
            .remove("notSellableGa");
    }

    let conn = h.db.conn().expect("conn");
    read_model_repo::project_subject(
        &conn,
        &h.scope(),
        NewDelta {
            tenant_id: h.tenant,
            catalog_version: CatalogVersion::new(5),
            subject: SubjectRef::Plan(plan_id),
            payload,
            projected_at: at(),
        },
    )
    .await
    .expect("project the legacy-shaped subject");
    pin_frontier_repo::advance(&conn, &h.scope(), h.tenant, CatalogVersion::new(5), at())
        .await
        .expect("pin it");

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["not_sellable_ga"],
        true,
        "an absent gate is an unknown gate, and unknown fails closed"
    );
}

/// **A usage-only market is previewable**, with no amount and a tier summary.
///
/// This case exists because a probe stayed silent. Removing the non-usage filter
/// reddened nothing, and reasoning about why showed the filter was not the
/// belt-and-braces it looked like: it made a market priced **solely** by usage
/// answer `404`, as though the plan did not sell there. That contradicts §2
/// having a "tier summary" at all — a metered market has a price, it just does
/// not have an `amountMinor`.
///
/// So the non-usage rows are a **preference**, not a filter: the fallback is any
/// row of the market.
#[tokio::test]
async fn a_usage_only_market_previews_with_no_amount_and_a_tier_summary() {
    use bss_pricing::domain::money::RateMinor;
    use bss_pricing::domain::price_row::TierBand;

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let mut delta = hybrid_delta(plan_id);
    // Drop the recurring row, leaving only the metered one.
    delta.prices.retain(|row| {
        row.scope_key.charge_kind() == bss_pricing::domain::scope_key::ChargeKind::Usage
    });
    assert_eq!(
        delta.prices.len(),
        1,
        "the fixture leaves exactly the usage row"
    );
    // **And the graduated row is given its bands**, because "a tier summary" is
    // half of this test's name and the fixture carried none: `PriceRow::new`
    // leaves `bands` empty, `tier_band_count` filters `> 0`, and the member was
    // therefore `null` on the one case that claims to cover it. Two bands rather
    // than one so a summary that reported "has bands" as `1` would still fail.
    let usage = delta.prices.first_mut().expect("the usage row");
    usage.row.bands = vec![
        TierBand::closed(
            0,
            1_000,
            RateMinor::from_nano_minor(15_000_000).expect("a non-negative rate"),
        ),
        TierBand::open(
            1_000,
            RateMinor::from_nano_minor(11_000_000).expect("a non-negative rate"),
        ),
    ];
    project_and_pin(&h, plan_id, 5, &delta).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a metered market is sold, so it is previewable"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["amount_minor"],
        serde_json::Value::Null,
        "usage money lives in bands, so there is no single amount to quote"
    );
    // The other half of the name, unasserted until 2026-08-20: the §2 tier
    // summary. Nothing in the crate read `tier_band_count`, so
    // `preview.rs`'s `tier_band_count` sum could be dropped, mis-summed or left
    // permanently `None` and this case — the one whose name promises it — would
    // still pass on the amount alone.
    assert_eq!(
        body["tier_band_count"], 2,
        "the summary counts the row's bands: {body}"
    );
}

/// **A per-unit metered row carries an amount, and still must not win the
/// preview.**
///
/// The third case a silent probe forced into existence. Dropping the
/// `chargeKind != usage` preference reddened nothing, because every usage row in
/// this file was `graduated` — whose money lives in bands, so `amountMinor` is
/// NULL and the amount test already excluded it. A **`per_unit`** usage row is
/// the counter-example: it is metered *and* carries a unit price, so without the
/// charge-kind preference a hybrid plan would be quoted its per-call rate as
/// though that were the monthly subscription price.
#[tokio::test]
async fn a_per_unit_metered_row_does_not_win_over_the_recurring_row() {
    use bss_pricing::domain::money::MinorAmount;
    use bss_pricing::domain::price_row::ModelKind;

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let mut delta = hybrid_delta(plan_id);
    // The usage row is first in the array and now carries a unit price of 7.
    let usage = delta
        .prices
        .iter_mut()
        .find(|r| r.scope_key.charge_kind() == bss_pricing::domain::scope_key::ChargeKind::Usage)
        .expect("the fixture has a usage row");
    usage.row.model_kind = Some(ModelKind::PerUnit);
    usage.row.amount_minor = Some(MinorAmount::new(7).expect("non-negative"));
    // And it sorts first by priceId, so a preference-less implementation takes it.
    usage.price_id = Uuid::from_u128(0x0000_0001);
    project_and_pin(&h, plan_id, 5, &delta).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["amount_minor"],
        1_200,
        "the base list price is the recurring row's, not the metered unit rate"
    );
}

/// **A per-seat plan is quoted its rate.** D-311 gave `per_unit` the pair
/// `(wants_amount = false, wants_rate = true)`, so a lawful per-seat row keeps its
/// money in `unitRateNanoMinor` and **must** leave `amountMinor` NULL. A view whose
/// only money member is the amount therefore answers 200 carrying no price of any
/// kind for the modal commercial shape — recurring, `per_unit`, seat-counted.
///
/// The second assertion is the one that matters. A fix that copied the rate into
/// `amountMinor` would satisfy the first while reintroducing exactly the
/// amount/rate conflation D-311 removed, so the probe pins both halves.
#[tokio::test]
async fn a_per_seat_plan_is_quoted_its_unit_rate() {
    use bss_pricing::domain::money::RateMinor;
    use bss_pricing::domain::price_row::{ModelKind, QuantitySource};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let mut delta = delta_of(plan_id, CURRENCY, REGION, false, None, false, false);
    let record = delta.prices.first_mut().expect("the fixture seeds one row");
    record.row.model_kind = Some(ModelKind::PerUnit);
    record.row.unit_rate =
        Some(RateMinor::from_nano_minor(12_000_000_000).expect("a non-negative rate"));
    record.row.quantity_source = Some(QuantitySource::SubscriptionSeatCount);
    // Required NULL: `check_amount_placement` refuses a `per_unit` row carrying one,
    // so this is the shape publish actually admits and not a contrived one.
    record.row.amount_minor = None;
    project_and_pin(&h, plan_id, 5, &delta).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["unit_rate_nano_minor"], 12_000_000_000_i64,
        "the per-seat rate is this plan's price and the preview has to carry it"
    );
    assert!(
        body["amount_minor"].is_null(),
        "D-311 forbids the amount on a `per_unit` row -- the rate must not be copied into it"
    );
}

// ---------------------------------------------------------------------------
// The query contract.
// ---------------------------------------------------------------------------

/// Both parameters are required: a preview without a market names no row.
#[tokio::test]
async fn a_preview_without_a_market_is_refused() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;

    for query in [
        "",
        &format!("currency={CURRENCY}"),
        &format!("region={REGION}"),
    ] {
        let response = h
            .allowed()
            .send(request("GET", &preview_path(plan_id, query), None))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "`{query}` names no market"
        );
    }
}

// ---------------------------------------------------------------------------
// C3 — a gated row is previewable, which is the whole of `inst-td-gagate`.
// ---------------------------------------------------------------------------

/// A `not_sellable_ga` row **is** previewed, carrying its flag.
///
/// §2 and C3 both say a tax-inclusive row "MAY be authored and **previewed**"
/// while gated. A preview that hid it would make "not sold here" and "not
/// sellable yet" the same answer, and an operator checking their EU launch would
/// see a 404 for a row they had published.
#[tokio::test]
async fn a_ga_gated_row_is_previewable_and_says_so() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    project_and_pin(
        &h,
        plan_id,
        5,
        &delta_of(
            plan_id,
            CURRENCY,
            REGION,
            true,
            Some("standard"),
            true,
            false,
        ),
    )
    .await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["not_sellable_ga"], true);
    assert_eq!(body["tax_inclusive"], true);
}

// ---------------------------------------------------------------------------
// What the refusals report (§10, `pricing_preview_failclosed_total`).
// ---------------------------------------------------------------------------

const FAILCLOSED: &str = "pricing_preview_failclosed_total";

/// **The two 404s that share one wire code are two different series.**
///
/// `PRICE_ROW_ABSENT` is the code for both "nobody authored that market" and
/// "this tenant has published nothing", because §5 declares one code — and the
/// remediations are opposite: one is a catalog gap somebody fills, the other is a
/// tenant that has never published. An operator watching a single `failclosed`
/// count would learn the preview is refusing without learning whose job it is.
///
/// Asserted **through the router**, so what is proven is that a real refusal
/// reported a real series, not that the adapter can increment its own counter.
#[tokio::test]
async fn an_unsold_market_and_an_unpublished_tenant_report_different_reasons() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;

    // A market the plan does not sell, on a tenant that has published.
    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency=JPY&region={REGION}")),
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.metrics.force_flush();

    assert_eq!(
        h.metrics
            .counter_value(FAILCLOSED, &[("reason", "market_absent")]),
        1
    );
    assert_eq!(
        h.metrics
            .counter_value(FAILCLOSED, &[("reason", "no_published_version")]),
        0,
        "a tenant that has published is not an unpublished tenant"
    );
}

/// A tenant that has published nothing reports `no_published_version`, and does
/// **not** report the market as absent.
#[tokio::test]
async fn a_tenant_with_no_published_version_reports_that_and_not_an_absent_market() {
    let h = Harness::new().await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(
                Uuid::now_v7(),
                &format!("currency={CURRENCY}&region={REGION}"),
            ),
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.metrics.force_flush();

    assert_eq!(
        h.metrics
            .counter_value(FAILCLOSED, &[("reason", "no_published_version")]),
        1
    );
    assert_eq!(
        h.metrics
            .counter_value(FAILCLOSED, &[("reason", "market_absent")]),
        0,
        "nothing is published, so no market of it can be the absent one"
    );
}

/// **A preview that answered reports nothing.**
///
/// The negative control the other two rest on: a route that counted every
/// request would satisfy both of them and would report a healthy catalog as
/// permanently failing closed.
#[tokio::test]
async fn a_successful_preview_counts_no_refusal() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;

    let response = h
        .allowed()
        .send(request(
            "GET",
            &preview_path(plan_id, &format!("currency={CURRENCY}&region={REGION}")),
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    h.metrics.force_flush();

    for reason in ["market_absent", "no_published_version", "market_not_named"] {
        assert_eq!(
            h.metrics.counter_value(FAILCLOSED, &[("reason", reason)]),
            0,
            "a preview that answered must report no {reason} refusal"
        );
    }
}

/// A request naming **no market** is a client fault, and a series of its own.
///
/// It needs no catalog change at all, which is what separates it from the other
/// two — and it is counted after the authorization gate, so a caller without the
/// grant is told that rather than that their query is malformed.
#[tokio::test]
async fn a_request_naming_no_market_reports_a_client_fault() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;

    let response = h
        .allowed()
        .send(request("GET", &preview_path(plan_id, "region=EU"), None))
        .await;
    // The status, not merely "not 200": a 403 would also satisfy `!= OK` while
    // meaning the gate refused, and the emission sits **after** the gate.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    h.metrics.force_flush();

    assert_eq!(
        h.metrics
            .counter_value(FAILCLOSED, &[("reason", "market_not_named")]),
        1
    );
    assert_eq!(
        h.metrics
            .counter_value(FAILCLOSED, &[("reason", "market_absent")]),
        0,
        "a malformed query is not an unauthored market"
    );
}

/// **A caller of another tenant is answered exactly as for a plan that does not
/// exist**, which is the claim `authz::actions::PREVIEW`'s doc makes and nothing
/// measured.
///
/// That doc states it as a *finding* — the grant was called partner-facing, and
/// `preview.rs` binds `tenant = ctx.subject_tenant_id()`, so a partner
/// authenticated in tenant `P` holding a grant over vendor tenant `V` is answered
/// `404 PRICE_ROW_ABSENT` for `V`'s plan rather than a preview. Both halves matter
/// and only one is about isolation: the refusal must not distinguish `V`'s plan
/// from a plan nobody authored, or the status is a probe for which plan ids sell in
/// which market.
///
/// `rest_authz`'s census cannot make this claim — `drive` sends no query
/// parameters to this route, so its **owner's** read is the `400` §5 owes a request
/// naming no market, which is why the row sits in
/// `BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE`. Here the query is well-formed and the
/// owner's read is a priced answer.
#[tokio::test]
async fn a_foreign_tenants_plan_previews_like_an_absent_one() {
    let h = Harness::new().await;
    let plan_id = seeded(&h).await;
    let query = format!("currency={CURRENCY}&region={REGION}");

    let foreign = h
        .other_tenant()
        .send(request("GET", &preview_path(plan_id, &query), None))
        .await;
    let absent = h
        .other_tenant()
        .send(request("GET", &preview_path(Uuid::now_v7(), &query), None))
        .await;

    let foreign_status = foreign.status();
    let absent_status = absent.status();
    assert_eq!(
        foreign_status,
        StatusCode::NOT_FOUND,
        "the grant is tenant-scoped, so another tenant's plan is not previewable"
    );
    assert_eq!(
        absent_status, foreign_status,
        "and a plan nobody authored answers the same, or the status distinguishes them"
    );
    assert_eq!(
        absent_code(foreign).await,
        absent_code(absent).await,
        "in the code too, the doc's own `PRICE_ROW_ABSENT` for both"
    );

    // The control. Without it a preview refusing every caller — an unpinned
    // frontier, a query the surface rejects — reads as tenant isolation.
    let owner = h
        .allowed()
        .send(request("GET", &preview_path(plan_id, &query), None))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::OK,
        "the plan's own tenant previews it, or the refusals above are about the request"
    );
    assert_eq!(body_json(owner).await["amount_minor"], 1_200);
}
