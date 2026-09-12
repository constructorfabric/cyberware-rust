//! What the record model has to keep true independently of storage.
//!
//! Every field of `PriceRecord` is exercised end to end in
//! `tests/sqlite_price_repo.rs`, where a mapping that dropped or transposed a
//! column actually fails; its lifecycle predicate belongs to `LifecycleState`,
//! which `lifecycle_tests.rs` already covers for all four states. What is left is
//! the split between the record and the content, which is a decision rather than
//! a derive — and `authored_content`'s rewrites, which are what every caller that
//! **judges or compares** a row without a store in front of it relies on
//! (`domain::import`'s Phase 1 has no store at all).

use uuid::Uuid;

use super::{PriceContent, PriceRecord, authored_content, canonical_usage_line};
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};

#[test]
fn the_content_carries_every_editable_column_and_no_identity() {
    let key = ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x9_1a4)),
        CurrencyCode::new("usd").expect("USD is three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");

    // The exhaustive struct literal is the assertion, and it is a compile-time
    // one: when a slice widens what a draft edit may touch, this stops
    // compiling, so the widening is a decision somebody made rather than a
    // field that appeared.
    let record = PriceRecord {
        price_id: Uuid::from_u128(0xb_10),
        scope_key: key,
        row: PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat)),
        tax_inclusive: true,
        // **Both of these were `None` on the fixture and `None` again in the
        // expected value below**, so two of the eight members were asserted
        // against themselves: a `content()` hard-coding either to `None` passed.
        // `proration_contract` carries `inst-pi-required`'s three mandatory
        // recurring-row inputs, and their silent loss on a read-modify-write is
        // exactly the bug this test's doc claims to cover.
        tax_category_ref: Some("standard-vat".to_owned()),
        billing_timing: Some("advance".to_owned()),
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::SubscriptionStart,
            proration_basis: ProrationBasis::CalendarDaysActual,
            credit_on_downgrade: true,
        }),
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: Some(utc_ymd_hms(2027, 1, 1, 0, 0, 0)),
        supersedes_price_id: Some(Uuid::from_u128(0xb_0f)),
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: utc_ymd_hms(2026, 8, 2, 10, 0, 0),
        row_version: RowVersion::new(3),
    };

    // Round-tripping through the content must change nothing an author wrote:
    // a `content()` that dropped a column would silently blank it on the next
    // read-modify-write, which is the one bug this accessor exists to prevent.
    assert_eq!(
        record.content(),
        PriceContent {
            row: record.row.clone(),
            tax_inclusive: record.tax_inclusive,
            tax_category_ref: record.tax_category_ref.clone(),
            billing_timing: record.billing_timing.clone(),
            proration_contract: record.proration_contract,
            rounding_policy_ref: record.rounding_policy_ref.clone(),
            grandfather_until: record.grandfather_until,
            supersedes_price_id: record.supersedes_price_id,
        }
    );
}

/// **The usage line is spelled the way its axes are**, whichever door renders it.
///
/// `Meter::new` and `DimensionKey::new` trim, so the ninth and tenth axes of the
/// key a row is filed under carry the trimmed value. `authored_content` used to
/// leave the row's own copy of that same pair as the caller sent it, and the two
/// are one column each — so a row authored `"api_calls "` was filed under
/// `api_calls` and stored as `api_calls `, and every gate over the key compares
/// the axis against the column. Pure, because the callers that matter most here
/// have no store to compare against: `domain::import` judges its batch's rows
/// through this function.
#[test]
fn authored_content_spells_the_usage_line_the_way_its_axes_do() {
    let key = ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x9_1a4)),
        CurrencyCode::new("usd").expect("USD is three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
    .with_usage_line(
        Some(Meter::new("api_calls").expect("a non-blank meter")),
        DimensionKey::new("region=eu"),
    )
    .expect("a usage key carries its line");

    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    row.meter = Some("  api_calls ".to_owned());
    row.dimension_key = " region=eu\t".to_owned();
    let content = PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
    };

    let authored = authored_content(&key, content);

    assert_eq!(
        authored.row.meter.as_deref(),
        key.meter().map(Meter::as_str),
        "the row's copy of the ninth axis is the axis"
    );
    assert_eq!(
        authored.row.dimension_key,
        key.dimension_key().as_str(),
        "and so is its copy of the tenth"
    );
}

/// A **different** meter is still a different meter, which is the line the
/// normalization must not cross.
///
/// `price_repo::resolve_authored_usage_line` refuses a row whose line disagrees
/// with its key rather than rewriting it, because a rewrite would make the
/// D-82/D-98/D-127 unit guard's `meter` and `dimensionKey` clauses unreachable.
/// Trimming has to leave that refusal something to find.
#[test]
fn the_normalization_fixes_the_spelling_and_not_the_value() {
    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    row.meter = Some(" egress_gb ".to_owned());
    row.dimension_key = String::new();

    assert_eq!(
        canonical_usage_line(&row),
        (Some("egress_gb".to_owned()), String::new()),
        "the value is the author's; only its whitespace is the store's"
    );

    // And the meterless line stays the meterless line: `None` is the store's "no
    // meter" sentinel and a normalization that minted `Some("")` for it would land
    // a metered row on the meterless key.
    row.meter = None;
    assert_eq!(canonical_usage_line(&row), (None, String::new()));
}
