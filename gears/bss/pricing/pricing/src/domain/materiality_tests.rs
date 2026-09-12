//! Tests for `evaluate` — **and for the direction that makes them mean
//! something**.
//!
//! Every material-side assertion here passes against `fn evaluate(..) -> Material`,
//! which is the shape of a feature that was never built. They are evidence only
//! alongside [`a_below_threshold_amount_change_on_unchanged_geometry_auto_publishes`],
//! which is the **only** shape that auto-publishes and is §3's own sentence for the
//! residue: *"a pure amount change on unchanged geometry, below an explicitly
//! configured threshold, not a first publish."*
//!
//! Phase 3 proved a fail-safe was a rule and not an absent feature, by injecting a
//! policy at the domain level because no surface could produce one. This phase
//! proves the other direction, and each of the four halves the group landed —
//! the threshold comparison, the delta domain's geometry clause, the per-currency
//! fail-safe and the trigger registry — **flips it back independently**. Four
//! cases, not one, because one case cannot tell an implemented arm from a missing
//! one; that is the argument this module's own doc makes about the three reasons.
//!
//! Each material-side test puts the world in the state where **its own** rule is
//! the one that can answer: the auto-publishable world is the fixture, and each
//! test changes exactly the one fact its rule is named for. A test whose fixture is
//! material for two reasons at once proves neither.

use uuid::Uuid;

use super::triggers::Trigger;
use super::{
    ChangeSet, MaterialityReason, MaterialityVerdict, PublishedPriceBaseline, ThresholdBasis,
    ThresholdEntry, ThresholdPolicy, ThresholdRefusal, ThresholdVersion, evaluate,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow, TierBand};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use time::OffsetDateTime;

/// A published `flat` row on `currency`, at an amount the case may move.
///
/// The change set and the baseline both carry whole rows now, because D-115's
/// delta domain takes its operand off a row's content; the key alone answered
/// `inst-mat-newrow` and nothing else.
fn row(currency: &str, amount: i64) -> PriceRecord {
    let mut shape = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    shape.amount_minor = Some(MinorAmount::new(amount).expect("a non-negative amount"));
    PriceRecord {
        price_id: Uuid::from_u128(0xb0_01),
        scope_key: key(currency),
        row: shape,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Published,
        created_by: Uuid::from_u128(0xac_01),
        created_at_utc: utc_ymd_hms(2026, 8, 2, 10, 0, 0),
        row_version: RowVersion::new(1),
    }
}

/// A published `graduated` row on `currency` over `bands`.
fn graduated(currency: &str, bands: &[(u64, Option<u64>, i64)]) -> PriceRecord {
    let mut shape = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    shape.bands = bands
        .iter()
        .map(|&(from, to, price)| {
            // Stated in whole minor units and scaled to the stored rate scale
            // (D-311), so these thresholds mean what they always meant.
            let unit = RateMinor::from_minor_units(price).expect("a non-negative rate");
            match to {
                Some(top) => TierBand::closed(from, top, unit),
                None => TierBand::open(from, unit),
            }
        })
        .collect();
    let mut record = row(currency, 0);
    record.row = shape;
    record
}

fn key(currency: &str) -> ScopeKey {
    ScopeKey::new(
        PlanId::new(Uuid::from_u128(1)),
        CurrencyCode::new(currency).expect("a three-letter code"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(2)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the eight axes agree")
}

/// An absolute entry for `currency`, in that currency's minor units.
fn absolute(currency: &str, minor: i64) -> ThresholdEntry {
    ThresholdEntry {
        currency: CurrencyCode::new(currency).expect("a three-letter code"),
        basis: ThresholdBasis::Absolute { minor },
    }
}

/// A policy with a 500-minor absolute threshold on the two currencies these cases
/// use.
///
/// `500` is the bar every amount case below is placed either side of: a move of
/// `100` is under it and a move of `900` is over it, so no case sits on the
/// boundary by accident — the boundary itself is
/// `delta_tests::a_move_that_reaches_the_threshold_is_not_below_it`'s.
fn configured_policy() -> ThresholdPolicy {
    ThresholdPolicy::of_entries([absolute("USD", 500), absolute("EUR", 500)])
        .expect("a non-empty entry set is a configured policy")
}

/// `inst-mat-failsafe` — no configured policy ⇒ material, whatever else is
/// true.
#[test]
fn without_a_configured_policy_every_change_is_material() {
    let published = row("USD", 1000);
    let change = ChangeSet::of_records([published.clone(), row("EUR", 900)]);

    // Three worlds: a first publish, a change that adds a row, and the world
    // that is otherwise auto-publishable. The third is the one that makes this
    // a test of *this* rule rather than of one of its siblings; the first two
    // additionally fix the precedence — the missing policy answers before the
    // missing baselines do, so an operator is told the fact they can act on.
    let first_publish = evaluate(&change, None, None);
    let baseline_missing_a_row = evaluate(
        &change,
        None,
        Some(&PublishedPriceBaseline::of_records([published.clone()])),
    );
    let otherwise_auto_publishable = evaluate(
        &ChangeSet::of_records([published.clone()]),
        None,
        Some(&PublishedPriceBaseline::of_records([published])),
    );

    for verdict in [
        first_publish,
        baseline_missing_a_row,
        otherwise_auto_publishable,
    ] {
        assert_eq!(
            verdict,
            MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold),
            "an unset threshold policy is the fail-safe, and it answers first"
        );
    }
}

/// `inst-mat-first` — a first publish is material even under a configured
/// policy.
#[test]
fn a_first_publish_is_material_even_below_a_configured_threshold() {
    // The policy is configured, so `inst-mat-failsafe` has already declined:
    // the absent baseline is what answers here. There is no delta to threshold
    // against a plan that has never published.
    let policy = configured_policy();
    let change = ChangeSet::of_records([row("USD", 1000)]);

    let verdict = evaluate(&change, Some(&policy), None);

    assert_eq!(
        verdict,
        MaterialityVerdict::material(MaterialityReason::FirstPublish)
    );
}

/// `inst-mat-newrow` — a row with no baseline of its own is material.
#[test]
fn a_row_added_to_a_published_plan_is_material_having_no_baseline() {
    // This fixture is the auto-publishable one plus a single added row: same
    // policy, same baseline, same published row. If the added row did not
    // decide the verdict, the answer would be AutoPublishable.
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published.clone()]);
    let change = ChangeSet::of_records([published, row("EUR", 900)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict,
        MaterialityVerdict::material(MaterialityReason::RowWithoutBaseline),
        "a new currency key on a published plan has no predecessor to delta against"
    );
}

/// **The test that makes every other one here mean anything, and the only shape
/// that auto-publishes.**
///
/// §3's residue, verbatim: a pure amount change on unchanged geometry, below an
/// explicitly configured threshold, not a first publish. `1000 -> 1100` is a move
/// of `100` against a bar of `500`.
///
/// The row **must move**. It used to be the same row on both sides, which is now
/// D-115's pure-shape revision and material — a change set identical to its
/// baseline is a revision whose content is the plan's shape, and reading that as
/// "below threshold" is exactly the hole D-115 closed.
///
/// # This change set is one `create_draft` refuses to author, and that is stated
/// # rather than fixed
///
/// **A mounted surface reaches this comparison now, and it is not the publish route**
/// (corrected 2026-08-06, D-88's orchestrator and route landing). The change set is a
/// row that moved on a key the baseline already carries — a *reprice of an occupied
/// key* — and the **authoring** door still refuses one unconditionally:
/// `price_repo::insert_prepared` refuses when `find_key_occupant` finds a `draft` or
/// `published` row there, with the repository's own sentence: *"this is not the way to
/// reprice an occupied key. That is the D-88 supersession unit"*. That unit is whole:
/// `insert_successor_draft_on` (D-195) authors the pair, `plan_supersession` composes
/// it, `commit_supersession` writes it, and `POST …/plans/{planId}/supersessions`
/// carries it — so `evaluate` is now reached over an **authored** change set with a
/// real per-currency delta, and `tests/sqlite_supersession_unit.rs` holds both halves:
/// the `thresholdReached` case and the `AutoPublishable` one.
///
/// **D-183 expected that to arrive through a *publish*, and the first correction of that
/// expectation was itself wrong** (both 2026-08-06; the second found by review of the
/// first). It said `infra::publish::validated_draft_rows` forbids the pair permanently.
/// That function governs what a publish **writes**; the evaluator's **input** is built
/// at `api::rest::publish` from the assembled shape — published *plus* draft — so a plan
/// holding a staged supersession successor **does** present the pair. See **D-200**,
/// **decided 2026-08-06 on option (b)**: the two sets stay different by decision — the
/// evaluator ranges over the candidate set, the commit flips `validated_draft_rows` — so
/// a plan-revision unit's stored verdict can name a row the revision will not publish.
/// What is true without qualification is the output half: no publish ever *flips* a
/// moved row against a published predecessor on one key.
///
/// **And the third correction is that D-200's own ground was false.** *"Over-material
/// and never under-material"* held for `inst-mat-percurrency`, which is the step D-200
/// reasoned about; it does not hold one step over. The staged row is the **only** moved
/// row a plan revision can present, so it is also the only thing that can make
/// `triggers::triggered_by_content`'s `moves_no_row` answer `false` — and with that
/// answer, D-115's whole-revision trigger never fires and a revision whose entire
/// content is the plan's shape auto-publishes. Measured through the router: a €500
/// per-period bound (D-319) published on one principal, `approval: null`,
/// `published_price_ids: []`. `api::rest::publish::materiality_of` now ranges over
/// `infra::publish::unit_row_set` — the candidate set less another unit's staged rows,
/// which is neither of the two options D-200 weighed — and the entry that records the
/// reversal is **D-326**, decided and CONFIRMED 2026-08-16 (this line said it was owed
/// until 2026-08-20; the sibling claim in `infra::publish` said the same).
///
/// So this stays a legitimate unit test of `evaluate` over a **hand-built** input, and
/// it is kept for the reason it always was: it is the only executable statement of the
/// shape §3 says auto-publishes on a **plan revision**, which is the one subject no
/// production path presents. That clause used to read *"will ever present"* and was
/// wrong by one row for eleven days; what makes it true is the caller, and
/// `infra::publish_tests::the_evaluators_row_set_is_the_set_the_commit_publishes` and
/// `rest_publish::a_period_bound_published_beside_an_orphaned_successor_is_still_judged`
/// are the two ends of it. Deleting this would leave the shape unstated and the four
/// halves below with nothing to be halves of.
#[test]
fn a_below_threshold_amount_change_on_unchanged_geometry_auto_publishes() {
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published]);
    let change = ChangeSet::of_records([row("USD", 1100)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(verdict, MaterialityVerdict::AutoPublishable);
    assert!(!verdict.is_material());
    assert_eq!(verdict.reason(), None);
}

/// **Half one of four: the threshold comparison itself.**
///
/// The same world one digit over the bar. Before this group `evaluate` compared
/// nothing at all, so a move of any size came back auto-publishable the moment a
/// policy existed — the fail-open `materiality.rs`'s own absence section named.
///
/// **The token is `thresholdReached` and the assertion used to contradict itself.**
/// It read `noConfiguredThreshold` under the message *"a move of 900 reaches a bar of
/// 500"* — a bar that is configured, reported as a bar that is not. The enum had no
/// arm for the one outcome the comparison exists to produce, so the fail-safe token
/// was reused for it and the stored column could not tell this world from a currency
/// nobody configured. That is [`a_currency_with_no_entry_is_material`]'s world, and
/// the two now differ where an auditor reads them.
#[test]
fn an_above_threshold_amount_change_is_material() {
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published]);
    let change = ChangeSet::of_records([row("USD", 1900)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict.reason(),
        Some(MaterialityReason::ThresholdReached),
        "a move of 900 reaches a bar of 500, and USD has that bar"
    );
    assert_ne!(
        verdict.reason(),
        Some(MaterialityReason::NoConfiguredThreshold),
        "a configured bar that was reached is not an absent bar"
    );
}

/// **Half two of four: the delta domain's unchanged-geometry precondition.**
///
/// The band bounds move and every unit price stays where it was, so the amount
/// side of the comparison is zero in every band. D-115: `[0,1000)` to `[0,10)`
/// multiplies what a subscriber pays at zero price delta, and a threshold
/// comparison over the vector alone would wave it through.
#[test]
fn an_unchanged_geometry_precondition_violation_is_material() {
    let policy = configured_policy();
    let published = graduated("USD", &[(0, Some(1000), 5), (1000, None, 3)]);
    let baseline = PublishedPriceBaseline::of_records([published]);
    let change = ChangeSet::of_records([graduated("USD", &[(0, Some(10), 5), (10, None, 3)])]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict,
        MaterialityVerdict::triggered(Trigger::NoComputableRowDelta),
        "no delta is computable across a geometry change, so D-115 registers it"
    );
}

/// **Half three of four: `inst-mat-percurrency`'s fail-safe.**
///
/// *"A row whose currency has no threshold entry in the configured policy is
/// material — the G1 fail-safe applies per currency, not per policy object."* The
/// policy is configured, the plan has a baseline, the row has a predecessor and the
/// move is below every bar the policy does hold; the only thing wrong is that
/// nobody configured `GBP`.
#[test]
fn a_currency_with_no_entry_is_material() {
    let policy = configured_policy();
    let published = row("GBP", 1000);
    let baseline = PublishedPriceBaseline::of_records([published]);
    let change = ChangeSet::of_records([row("GBP", 1100)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict,
        MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold),
        "the fail-safe is per currency, and GBP has no entry"
    );
}

/// **A registered trigger names itself on a currency with no threshold entry.**
///
/// Both answers are material, so the verdict's force is the same and only the
/// *reason* differs — which is the whole of what an operator acts on. Answered as
/// the fail-safe, this tells them to configure a bar for GBP; the row is a
/// registered trigger and would be material at every bar, so the remedy is one they
/// can carry out and that changes nothing. It is the mirror of the fault the
/// `Reached` arm names: reporting the fail-safe where a threshold *is* configured
/// would tell an operator to configure one twice.
#[test]
fn a_registered_trigger_names_itself_on_a_currency_with_no_entry() {
    let policy = configured_policy();
    let baseline = PublishedPriceBaseline::of_records([row("GBP", 1000)]);

    let mut discounted = row("GBP", 1000);
    discounted.row.discount_ref = Some("promo/autumn".to_owned());

    assert_eq!(
        evaluate(
            &ChangeSet::of_records([discounted]),
            Some(&policy),
            Some(&baseline)
        ),
        MaterialityVerdict::triggered(Trigger::NoComputableRowDelta),
        "GBP has no entry and the row is a trigger; the trigger is the reason, \
         because configuring an entry would not change the verdict"
    );
}

/// **A trigger on one row outranks another row's fail-safe.**
///
/// The threshold walk returns on the first row it can answer for, so a change set
/// whose GBP row has no entry answered `NoConfiguredThreshold` and never reached the
/// EUR row's registered trigger at all — the reason belongs to a row that is not the
/// one an operator would be sent to look at, and the remedy it names would not change
/// the verdict. Ordered GBP first on purpose: reversed, the row loop reaches the
/// trigger before the fail-safe and the case cannot tell the two walks apart.
#[test]
fn a_trigger_on_a_later_row_outranks_an_earlier_rows_fail_safe() {
    let policy = configured_policy();
    let baseline = PublishedPriceBaseline::of_records([row("GBP", 1000), row("EUR", 1000)]);

    let mut triggering = row("EUR", 1000);
    triggering.row.discount_ref = Some("promo/autumn".to_owned());

    assert_eq!(
        evaluate(
            &ChangeSet::of_records([row("GBP", 1100), triggering]),
            Some(&policy),
            Some(&baseline)
        ),
        MaterialityVerdict::triggered(Trigger::NoComputableRowDelta),
        "the trigger is the reason, wherever in the change set it sits"
    );
}

/// **Half four of four: the registered act, in the world that would otherwise
/// auto-publish.**
///
/// The fixture is
/// [`a_below_threshold_amount_change_on_unchanged_geometry_auto_publishes`]
/// exactly — configured policy, real baseline, a move of 100 against a bar of
/// 500 — with one fact changed: the act is D-62's cancel. Every other rule
/// therefore declines and the trigger is what answers.
///
/// **It used to carry an unmoved row**, which was material for two reasons at once:
/// a change set identical to its baseline is also D-115's pure-shape revision, and
/// deleting the act arm left the test green. That is the shape this module's own
/// doc warns about — a test whose fixture is material for two reasons proves
/// neither — and the zero-delta half of the claim is
/// [`a_pure_shape_revision_is_material_at_zero_delta`]'s to make.
#[test]
fn a_registered_act_is_material_in_the_world_that_would_otherwise_auto_publish() {
    let policy = configured_policy();
    let baseline = PublishedPriceBaseline::of_records([row("USD", 1000)]);
    let change = ChangeSet::of_act(Trigger::WindowCancellation, [row("USD", 1100)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict,
        MaterialityVerdict::triggered(Trigger::WindowCancellation)
    );
}

/// A band vector is §3's any-row-trips rule one level down: **any** band over its
/// threshold trips the row.
///
/// The first band moves by 1 and the second by 900 against a bar of 500, on
/// unchanged geometry. An evaluator that compared only the first band — or an
/// aggregate over the vector — would answer auto-publishable while a usage row's
/// top tier tripled.
#[test]
fn one_band_over_the_bar_trips_a_row_whose_other_bands_did_not_move() {
    let policy = configured_policy();
    let baseline = PublishedPriceBaseline::of_records([graduated(
        "USD",
        &[(0, Some(1000), 5), (1000, None, 100)],
    )]);
    let change =
        ChangeSet::of_records([graduated("USD", &[(0, Some(1000), 6), (1000, None, 1000)])]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict.reason(),
        Some(MaterialityReason::ThresholdReached),
        "the top band moved by 900 against a bar of 500"
    );
    // And D-187's payload names **which** band, which is this case's whole subject: the
    // bottom band moved by 1 and an aggregate would have hidden the top one behind it.
    let tripped = verdict.tripped().expect("a tripped row is named");
    assert_eq!(
        (tripped.from_minor, tripped.to_minor),
        (100_000_000_000, 1_000_000_000_000),
        "the band that reached the bar, not the row's first band; a band price is \
         a rate and reads back in the rate scale (D-311), while the bar it was \
         judged against stays authored in minor units"
    );
}

/// And the same shape below the bar in **every** band auto-publishes, which is what
/// makes the case above evidence rather than a band-kind row always being material.
#[test]
fn a_band_vector_entirely_below_the_bar_auto_publishes() {
    let policy = configured_policy();
    let baseline = PublishedPriceBaseline::of_records([graduated(
        "USD",
        &[(0, Some(1000), 5), (1000, None, 100)],
    )]);
    let change =
        ChangeSet::of_records([graduated("USD", &[(0, Some(1000), 6), (1000, None, 200)])]);

    assert_eq!(
        evaluate(&change, Some(&policy), Some(&baseline)),
        MaterialityVerdict::AutoPublishable
    );
}

/// **A successor whose only change is a reference this gear persists is material**
/// (review H6) — the whole-pipeline half of `delta_tests`' two field cases.
///
/// Both fields sit inside `PriceRecord::content()`, so the pure-shape trigger stays
/// quiet: the row *did* move, which is exactly why `moves_no_row` cannot answer for
/// them. And `SupersessionUnitGuard` names neither, because neither changes what the
/// key meters — a re-pointed discount is a legal supersession. So this walk was the
/// only door left, and with no arm in the delta domain both changes came back
/// `Amount(1000 -> 1000)`: below the configured bar, `AutoPublishable`, committed on
/// one principal.
///
/// Asserted through `evaluate` and not only through `row_delta`, because the fix is
/// only worth anything if the verdict changes: `triggers::triggered_by_row` is what
/// turns an incomputable delta into `NoComputableRowDelta`, and this is the assertion
/// that the two are wired together.
#[test]
fn a_successor_moving_only_a_persisted_reference_is_material() {
    let policy = configured_policy();

    let mut discounted = row("USD", 1000);
    discounted.row.discount_ref = Some("promo/autumn".to_owned());
    let baseline = PublishedPriceBaseline::of_records([row("USD", 1000)]);
    assert_eq!(
        evaluate(
            &ChangeSet::of_records([discounted]),
            Some(&policy),
            Some(&baseline)
        ),
        MaterialityVerdict::triggered(Trigger::NoComputableRowDelta),
        "the amount is unmoved and the discount instrument is not: no effective-price \
         delta is computable catalog-side, so the second signature is the answer"
    );

    let mut rounded = row("USD", 1000);
    rounded.rounding_policy_ref = Some("down".to_owned());
    assert_eq!(
        evaluate(
            &ChangeSet::of_records([rounded]),
            Some(&policy),
            Some(&baseline)
        ),
        MaterialityVerdict::triggered(Trigger::NoComputableRowDelta),
        "and the same for the rounding mode every charge on the row is rounded by"
    );

    // The world that makes the two above evidence rather than this fixture always
    // being material: the identical row set, under the identical policy, with
    // neither reference moved.
    assert_eq!(
        evaluate(
            &ChangeSet::of_records([row("USD", 1100)]),
            Some(&policy),
            Some(&baseline)
        ),
        MaterialityVerdict::AutoPublishable,
        "a move of 100 under a bar of 500 still auto-publishes"
    );
}

/// **A percent policy over an allowance-compiled row**, which is the shape D-45
/// made the preferred authoring — and the one a percent bar could never
/// auto-publish.
///
/// `domain::allowance` compiles `includedAllowance` into a leading `[0, N) @ $0`
/// band followed by the authored ones, so every allowance-bearing row carries a
/// band whose baseline is zero and which never moves. `band_delta` emits it as
/// element zero of the vector, `compare` asks `reaches_percent` of it, and a
/// `from_minor == 0` answer of `None` made the whole row `NotComparable` —
/// material, reported as `noConfiguredThreshold` about a policy the tenant had
/// configured. However small the authored change.
///
/// Here the authored band moves `$0.30 → $0.31`, a 3.33% rise under a 10% bar,
/// with the compiled `$0` band identical on both sides.
#[test]
fn a_sub_threshold_move_on_an_allowance_compiled_row_auto_publishes_under_a_percent_bar() {
    let policy = ThresholdPolicy::of_entries([ThresholdEntry {
        currency: CurrencyCode::new("USD").expect("USD"),
        basis: ThresholdBasis::Percent { bp: 1_000 },
    }])
    .expect("configured");
    let baseline = PublishedPriceBaseline::of_records([graduated(
        "USD",
        &[(0, Some(1000), 0), (1000, None, 30)],
    )]);
    let change = ChangeSet::of_records([graduated("USD", &[(0, Some(1000), 0), (1000, None, 31)])]);

    assert_eq!(
        evaluate(&change, Some(&policy), Some(&baseline)),
        MaterialityVerdict::AutoPublishable,
        "1 of 30 is 3.33%, below the 10% bar, and the unmoved free band decides nothing"
    );

    // And the bar is still a bar on this shape: the same free opening band, with
    // the authored band moved 30 -> 34 (13.3%), is material. Without this half the
    // case above could pass on a `reaches_percent` that answered `Some(false)` to
    // everything.
    let over = ChangeSet::of_records([graduated("USD", &[(0, Some(1000), 0), (1000, None, 34)])]);
    assert_eq!(
        evaluate(&over, Some(&policy), Some(&baseline)).reason(),
        Some(MaterialityReason::ThresholdReached),
        "4 of 30 is 13.3%, over the 10% bar"
    );
}

/// A registered act answers **before** the fail-safe, and the reason is what the
/// operator can act on.
///
/// With no policy configured the cancellation is material either way, so the only
/// observable difference is the stored reason — and `noConfiguredThreshold` would
/// send an operator to configure a threshold that changes nothing about a D-62 act.
#[test]
fn a_registered_act_names_the_trigger_even_with_no_policy_at_all() {
    let change = ChangeSet::of_act(Trigger::WindowCancellation, [row("USD", 1000)]);

    let verdict = evaluate(&change, None, None);

    assert_eq!(
        verdict,
        MaterialityVerdict::triggered(Trigger::WindowCancellation),
        "a registered act is material whatever a threshold says, so the threshold is not the reason"
    );
}

/// A percent-only policy against a zero baseline: no percentage is computable, so
/// §3 step 3's clause applies.
///
/// The move is one minor unit — as far below any sane relative bar as a move can
/// be — and it is still material, because there is no percentage of nothing.
#[test]
fn a_percent_policy_against_a_zero_baseline_is_material() {
    let policy = ThresholdPolicy::of_entries([ThresholdEntry {
        currency: CurrencyCode::new("USD").expect("USD"),
        basis: ThresholdBasis::Percent { bp: 5_000 },
    }])
    .expect("configured");
    let baseline = PublishedPriceBaseline::of_records([row("USD", 0)]);
    let change = ChangeSet::of_records([row("USD", 1)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict,
        MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold),
        "50% of nothing is nothing, so the comparison has no answer and the fail-safe applies"
    );
    // And **not** `thresholdReached`, which is the other half of the same
    // distinction: nothing was reached here, the bar could not be evaluated. §3 puts
    // this clause under G1 in the same breath as the no-entry case, so it carries
    // G1's token.
    assert_ne!(
        verdict.reason(),
        Some(MaterialityReason::ThresholdReached),
        "an unevaluable bar was not a bar that was reached"
    );
}

/// **D-115's headline case**, at the evaluator rather than at the registry: a
/// revision that moves no price row.
///
/// A trial stretched 7 to 90 days, a GL code moved, an availability date pushed —
/// none of it touches a price row, so a delta-only evaluator sees a change set
/// identical to its baseline and auto-publishes a commercial giveaway.
#[test]
fn a_pure_shape_revision_is_material_at_zero_delta() {
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published.clone()]);
    let change = ChangeSet::of_records([published]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict,
        MaterialityVerdict::triggered(Trigger::PlanShapeRevisionContent)
    );
}

/// One row over its own bar trips the whole change (§3's G3, any-row-trips).
///
/// Two rows on two currencies: the `USD` one moves by 100 against a bar of 500 and
/// the `EUR` one by 900 against the same bar. An evaluator that stopped at the
/// first row, or that compared an aggregate, would answer auto-publishable.
#[test]
fn one_row_over_its_own_currencys_bar_trips_the_whole_change() {
    let policy = configured_policy();
    let baseline = PublishedPriceBaseline::of_records([row("USD", 1000), row("EUR", 1000)]);
    let change = ChangeSet::of_records([row("USD", 1100), row("EUR", 1900)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(verdict.reason(), Some(MaterialityReason::ThresholdReached));
    // The payload names the currency that tripped, and this case is where that earns
    // its place: both rows are in the change set, only one reached its bar, and a
    // reviewer told merely `thresholdReached` would have to guess which.
    assert_eq!(
        verdict
            .tripped()
            .expect("a tripped row is named")
            .currency
            .as_str(),
        "EUR",
        "the row that moved by 900, not the one that moved by 100"
    );
}

/// An entry set with nothing in it is not a configured policy.
///
/// Asserted at the constructor rather than through [`evaluate`]: routed through
/// the evaluator this would also fail when `inst-mat-failsafe` is removed, and
/// a guard whose test fires for two different deletions cannot say which one
/// happened. The consequence — that the resulting `None` is material — is
/// [`without_a_configured_policy_every_change_is_material`]'s.
#[test]
fn an_empty_entry_set_is_not_a_configured_policy() {
    assert!(
        ThresholdPolicy::of_entries([]).is_none(),
        "a policy object with a threshold for no currency has configured nothing"
    );
    assert!(ThresholdPolicy::of_entries([absolute("USD", 500)]).is_some());
}

/// Two currencies configured once each, however the caller spells them — **and
/// the first entry for a currency is the one that stands**.
#[test]
fn a_currency_listed_twice_is_one_configured_entry() {
    let policy = ThresholdPolicy::of_entries([
        absolute("USD", 500),
        absolute("usd", 900),
        absolute("EUR", 500),
    ])
    .expect("configured");

    let currencies: Vec<&str> = policy.currencies().map(CurrencyCode::as_str).collect();

    assert_eq!(currencies, ["USD", "EUR"]);
    // The duplicate carried a different bar, so which one won is observable
    // rather than a matter of taste: first-wins, and a policy with two answers
    // for one row would otherwise depend on read order.
    assert_eq!(
        policy
            .entry(&CurrencyCode::new("USD").expect("USD"))
            .map(|entry| entry.basis),
        Some(ThresholdBasis::Absolute { minor: 500 })
    );
}

/// **The property that makes the stored record able to name the act, and the only
/// thing holding it.**
///
/// [`MaterialityVerdict::Material`]'s `trigger` is `Some` exactly under
/// `alwaysMaterialTrigger`. The type cannot say so — [`MaterialityVerdict::material`]
/// would take the reason with no trigger, and folding the trigger into
/// [`MaterialityReason`] would cost `ALL` its `Copy` roster shape, which D-187
/// already priced and declined for the tripped row — so this walks every arm
/// [`evaluate`] can take and asserts the biconditional in both directions.
///
/// **Armed against the claim rather than against materiality.** A case asserting
/// only that these change sets are material passed before the member existed and
/// would pass again if every arm answered `None`, which is the state this whole
/// change is about: D-321 measured `Trigger::as_str` as having no production
/// consumer at all, so *"the publish is material"* was true and *"the record says
/// which act"* was false, and no case in this file could tell them apart.
///
/// The act half ranges over [`Trigger::ALL`] rather than a chosen few, so a
/// nineteenth trigger is covered the day it is declared. Both halves of the diff
/// side are here too — a trigger minted inside the registry reaches the verdict by
/// a different arm of `evaluate` and could have been left behind by either.
#[test]
fn a_registered_act_names_its_trigger_and_nothing_else_does() {
    // The act half: every registered trigger a surface can declare, through the
    // one door `evaluate` reads it back from.
    for declared in Trigger::ALL {
        let verdict = evaluate(&ChangeSet::of_act(*declared, []), None, None);
        assert_eq!(
            verdict.reason(),
            Some(MaterialityReason::AlwaysMaterialTrigger),
            "{declared:?} is a registered act and must be material for that reason"
        );
        assert_eq!(
            verdict.trigger(),
            Some(*declared),
            "{declared:?} declared the act, so the record must name {declared:?} and not \
             merely `alwaysMaterialTrigger` — that token is one word for eighteen acts"
        );
    }

    // The content half, minted inside the registry rather than declared by a
    // surface: D-115's pure-shape revision.
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published.clone()]);
    assert_eq!(
        evaluate(
            &ChangeSet::of_records([published]),
            Some(&policy),
            Some(&baseline)
        )
        .trigger(),
        Some(Trigger::PlanShapeRevisionContent),
        "the whole-change-set half reaches the verdict by its own arm of `evaluate`"
    );

    // The per-row half, likewise: a geometry change carries no computable delta.
    let geometry_baseline = PublishedPriceBaseline::of_records([graduated(
        "USD",
        &[(0, Some(1000), 5), (1000, None, 3)],
    )]);
    assert_eq!(
        evaluate(
            &ChangeSet::of_records([graduated("USD", &[(0, Some(10), 5), (10, None, 3)])]),
            Some(&policy),
            Some(&geometry_baseline)
        )
        .trigger(),
        Some(Trigger::NoComputableRowDelta),
        "the per-row half is a third arm and is not covered by either above"
    );

    // The other direction, and it is the half a one-sided case would miss: a
    // threshold, a fail-safe and a first publish are answers about a policy, a bar
    // and a baseline. None of them is an act, so none may name one — a verdict
    // that filled the member in would put a trigger in front of a reviewer that
    // nothing declared.
    let no_baseline = evaluate(
        &ChangeSet::of_records([row("USD", 1000)]),
        Some(&policy),
        None,
    );
    assert_eq!(no_baseline.reason(), Some(MaterialityReason::FirstPublish));
    assert_eq!(no_baseline.trigger(), None, "a first publish is no act");

    let unconfigured = evaluate(&ChangeSet::of_records([row("USD", 1000)]), None, None);
    assert_eq!(
        unconfigured.reason(),
        Some(MaterialityReason::NoConfiguredThreshold)
    );
    assert_eq!(unconfigured.trigger(), None, "a missing policy is no act");

    let tripped = evaluate(
        &ChangeSet::of_records([row("USD", 1900)]),
        Some(&policy),
        Some(&baseline),
    );
    assert_eq!(
        tripped.reason(),
        Some(MaterialityReason::ThresholdReached),
        "the fixture must actually reach its bar, or the assertion below proves nothing"
    );
    assert_eq!(tripped.trigger(), None, "a bar reached is no act");

    // A move of 100 against a bar of 500, which is §3's own residue: a pure amount
    // change on unchanged geometry, below a configured threshold, not a first
    // publish. It is the one arm that must carry neither operand.
    let below = evaluate(
        &ChangeSet::of_records([row("USD", 1100)]),
        Some(&policy),
        Some(&baseline),
    );
    assert_eq!(below, MaterialityVerdict::AutoPublishable);
    assert_eq!(
        below.trigger(),
        None,
        "and an auto-publishable change names neither a reason nor a trigger"
    );
}

/// Every reason has its own stored token.
#[test]
fn every_reason_carries_a_distinct_token() {
    // These spellings are what `pricing_approval.materiality` holds for the next
    // seven years; two reasons sharing one would make an audit unable to say which
    // rule required the reviewer. `ALL` is the roster and the list below is its
    // transcription, so a member added without a token fails here.
    let tokens: Vec<&str> = MaterialityReason::ALL
        .iter()
        .map(|reason| reason.as_str())
        .collect();

    assert_eq!(
        tokens,
        [
            "noConfiguredThreshold",
            "firstPublish",
            "rowWithoutBaseline",
            // `inst-mat-registered`. `evaluate` **does** answer with it — the act
            // half from `ChangeSet::of_act`, the content half from
            // `triggers::triggered_by_content`. The note that used to sit here said
            // the writer was `infra::window`, which stopped being true when the
            // trigger registry landed and is now false twice over: that module hands
            // the act to the evaluator rather than minting the verdict itself.
            "alwaysMaterialTrigger",
            // `inst-mat-percurrency` proper. Its absence is what made a row over its
            // bar report `noConfiguredThreshold`, a state in which no bar exists.
            "thresholdReached",
        ]
    );
}

// ---------------------------------------------------------------------------
// `ThresholdVersion` — the pinned subject of the D-10 unit.
// ---------------------------------------------------------------------------

/// An authored instant, quantized as D-144 requires.
fn at_utc(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, hour, 0, 0)
}

/// One entry, spelled compactly.
fn version_entry(code: &str, minor: i64) -> ThresholdEntry {
    ThresholdEntry {
        currency: CurrencyCode::new(code).expect("a valid code"),
        basis: ThresholdBasis::Absolute { minor },
    }
}

#[test]
fn a_version_with_no_entries_is_refused_and_the_store_is_why() {
    // Not a style rule. `pricing_approval_threshold` holds one row per configured
    // currency, so an empty version writes **zero rows** — `latest_version` cannot
    // see it, `read_version` cannot tell it from a version nobody proposed, and the
    // approval unit pinning it would cover nothing. It is refused where the
    // operator hears about it rather than written and lost.
    assert_eq!(
        ThresholdVersion::new(0, at_utc(9), Vec::new()),
        Err(ThresholdRefusal::NoEntries)
    );
}

#[test]
fn a_version_naming_one_currency_twice_is_refused_and_names_it() {
    // `ThresholdPolicy::of_entries` collapses duplicates first-wins because a
    // *read* of the store must be total; a **proposal** is authored, so the
    // operator is told rather than silently losing one of their two entries. The
    // two directions of the same rule, and this asserts they stay apart.
    assert_eq!(
        ThresholdVersion::new(
            0,
            at_utc(9),
            vec![version_entry("EUR", 1), version_entry("EUR", 2)]
        ),
        Err(ThresholdRefusal::DuplicateCurrency("EUR".to_owned()))
    );
    // The read half, unchanged: first wins, and nothing is refused.
    let policy =
        ThresholdPolicy::of_entries(vec![version_entry("EUR", 1), version_entry("EUR", 2)])
            .expect("a read is total");
    assert_eq!(
        policy.entry(&CurrencyCode::new("EUR").expect("a valid code")),
        Some(&version_entry("EUR", 1))
    );
}

#[test]
fn a_version_keeps_the_entry_order_it_was_given() {
    // Load-bearing rather than tidy: the pin is taken over this rendering, so a
    // constructor that re-sorted its argument would leave the surface and the
    // store's `ORDER BY` free to disagree about the order and never find out. The
    // surface sorts; this does not.
    let version = ThresholdVersion::new(
        7,
        at_utc(9),
        vec![version_entry("USD", 1), version_entry("EUR", 2)],
    )
    .expect("well formed");
    assert_eq!(version.entries()[0].currency.as_str(), "USD");
    assert_eq!(version.entries()[1].currency.as_str(), "EUR");
    assert_eq!(version.version(), 7);
    assert_eq!(version.effective_from(), at_utc(9));
}

#[test]
fn a_versions_policy_is_the_one_the_evaluator_compares_against() {
    // The bridge between the pinned subject and the evaluator, asserted so that a
    // version whose entries reached the pin but not the comparison is caught. It is
    // `Some` for every version the constructor admits.
    let version =
        ThresholdVersion::new(0, at_utc(9), vec![version_entry("EUR", 500)]).expect("well formed");
    let policy = version.policy().expect("a non-empty version is a policy");
    assert_eq!(
        policy.entry(&CurrencyCode::new("EUR").expect("a valid code")),
        Some(&version_entry("EUR", 500))
    );
    assert_eq!(
        policy.entry(&CurrencyCode::new("USD").expect("a valid code")),
        None,
        "and a currency with no entry stays without one - inst-mat-percurrency's fail-safe half"
    );
}

/// **M-3, settled by executing it: a plan with a published revision and no price
/// rows at all.**
///
/// `moves_no_row` used to require the change set to carry at least one row, on the
/// ground that *"`inst-mat-first` owns that world, and it runs before this"*. That
/// justification holds only while `baseline` is `None`. A plan that published an
/// empty revision has a baseline — the two-publish sequence
/// `rest_windows::a_plans_first_window_is_authorable_through_the_routes_after_an_empty_publish`
/// executes is exactly that world — so its **second** revision reached the row walk
/// with nothing to walk and came back `AutoPublishable` under a configured policy.
/// D-115's own example is that revision: a trial stretched 7 → 90 days on a plan
/// whose price rows are not authored yet.
#[test]
fn a_second_revision_of_a_plan_with_no_price_rows_is_material() {
    let policy = configured_policy();
    // A published revision, and not one price row on it. `PublishedPriceBaseline`
    // distinguishes this from a first publish by construction: `Some` with no rows.
    let baseline = PublishedPriceBaseline::of_records([]);
    let change = ChangeSet::of_records([]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    assert_eq!(
        verdict,
        MaterialityVerdict::triggered(Trigger::PlanShapeRevisionContent),
        "a revision that moves no row is D-115's pure-shape revision whether it carries none or \
         carries only rows that did not move"
    );
    // And the first publish of the same plan is still `inst-mat-first`'s, which is
    // the world the `any` flag was protecting and the reason it is not needed: the
    // baseline's absence answers before this rule is asked.
    assert_eq!(
        evaluate(&change, Some(&policy), None),
        MaterialityVerdict::material(MaterialityReason::FirstPublish),
        "and the rule that owns the never-published plan still answers first"
    );
}

/// **The pure-shape arm belongs to a plan revision, and a window mutation is not one.**
///
/// One change set, two subjects, two verdicts — which is the whole content of the gate.
/// The rows are exactly what is published, so `moves_no_row` is `true` either way; what
/// differs is what that *means*. For a plan revision it means D-115's pure-shape
/// revision (a trial stretched, a GL code moved) and it is material. For a window
/// mutation it means the mutation moved an interval and no row, which is what **every**
/// window mutation does by construction — so reading it as a shape revision made a
/// schedule and a lengthening material whatever the threshold said, taking D-62's
/// answer for the two acts D-62 deliberately does not govern.
///
/// The window arm's verdict is `AutoPublishable` here because the currency has an entry
/// and a zero delta reaches no bar. Drop the entry and it is material again — which is
/// [`a_currency_with_no_entry_is_material`]'s rule reaching the window plane through its
/// one owner, and it is asserted below rather than argued.
#[test]
fn a_window_mutation_is_not_a_pure_shape_revision() {
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published.clone()]);
    let window_id = Uuid::from_u128(0x_d1_d0);

    let as_revision = ChangeSet::of_records([published.clone()]);
    let as_window = ChangeSet::of_window_mutation(window_id, None, [published.clone()]);

    assert_eq!(
        evaluate(&as_revision, Some(&policy), Some(&baseline)),
        MaterialityVerdict::triggered(Trigger::PlanShapeRevisionContent),
        "a revision that moves no row is D-115's"
    );
    assert_eq!(
        evaluate(&as_window, Some(&policy), Some(&baseline)),
        MaterialityVerdict::AutoPublishable,
        "the same rows as a window mutation are not a shape revision, and a zero delta is below \
         the bar"
    );

    // And the per-currency fail-safe still reaches the window plane: a policy that
    // thresholds another currency leaves this row's currency without an entry.
    let elsewhere = ThresholdPolicy::of_entries([absolute("EUR", 500)]).expect("configured");
    assert_eq!(
        evaluate(&as_window, Some(&elsewhere), Some(&baseline)),
        MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold),
        "one owner of `inst-mat-percurrency`, and the window plane reads it through the evaluator"
    );

    // The act half is untouched by the gate: D-62's cancel is material on the same
    // rows and the same policy, because the trigger is examined before any of this.
    let cancelled =
        ChangeSet::of_window_mutation(window_id, Some(Trigger::WindowCancellation), [published]);
    assert_eq!(
        evaluate(&cancelled, Some(&policy), Some(&baseline)),
        MaterialityVerdict::triggered(Trigger::WindowCancellation)
    );
}

/// **§6's declared payload: which row, in which currency, by how much** (D-187).
///
/// The column is declared as *"evaluator output: per-currency deltas, tripped rows,
/// trigger source"*, and what was stored was the verdict token alone. The consequence
/// landed on the reviewer rather than on the gate: a `thresholdReached` unit said a bar
/// was reached and could not say *which row*, *in which currency*, or *by how much* —
/// precisely the content a `FinanceReviewer` needs in order to sign for it.
///
/// The evaluator holds all three at the moment it answers. It walks rows in their own
/// currency and `compare` already knows which move reached the bar; it simply threw the
/// move away. So this is an unwritten rather than an unknowable, which is why the
/// declaration was not narrowed to match the code.
#[test]
fn a_tripped_row_is_named_with_its_currency_and_its_move() {
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published]);
    let change = ChangeSet::of_records([row("USD", 1900)]);

    let verdict = evaluate(&change, Some(&policy), Some(&baseline));

    let tripped = verdict
        .tripped()
        .expect("a thresholdReached verdict names the row that reached the bar");
    assert_eq!(
        tripped.price_id,
        Uuid::from_u128(0xb0_01),
        "the row an operator has to look at"
    );
    assert_eq!(
        tripped.currency.as_str(),
        "USD",
        "and the currency the bar belongs to - the comparison is per currency"
    );
    assert_eq!(
        (tripped.from_minor, tripped.to_minor),
        (1000, 1900),
        "by how much: the move that reached the bar, not a re-derivation of it"
    );
    assert_eq!(
        verdict.reason(),
        Some(MaterialityReason::ThresholdReached),
        "the token stays the discriminator; the evidence rides beside it"
    );
}

/// The evidence is **absent** where there is none, and that is not an omission.
///
/// `alwaysMaterialTrigger` is an answer about the act, `noConfiguredThreshold` about
/// the policy: neither names a row that reached a bar, because in neither case did one.
/// Filling the field in anyway — with the first row of the change set, say — would put a
/// row in front of a reviewer as though it had tripped something.
#[test]
fn a_verdict_that_tripped_no_row_carries_no_tripped_row() {
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published]);
    let change = ChangeSet::of_records([row("USD", 1010)]);

    let unconfigured = evaluate(&change, /* policy */ None, Some(&baseline));
    assert_eq!(
        unconfigured.reason(),
        Some(MaterialityReason::NoConfiguredThreshold)
    );
    assert!(
        unconfigured.tripped().is_none(),
        "the fail-safe is about the policy, not about a row"
    );
}

/// **The gap D-319 recorded, held by a probe rather than by a paragraph** — and
/// the statement of what stops it reaching an operator.
///
/// `inst-mat-registered` registers *plan-shape revision content* — the descriptor
/// set, the phase graph, the add-on rules, the composites, the plan-change
/// contract and (D-319) the plan-level period floor/cap — as always material. The
/// detector for it is [`triggers::triggered_by_content`], and the detector is
/// **narrower than the rule**: it fires on a change set that moves **no** row, so
/// a change set that moves one is not a shape revision however much shape moved
/// beside it. That is what this case asserts, in the direction the gap runs.
///
/// `ChangeSet` cannot express the difference. Its members are a publish-unit kind,
/// a declared act and a row set; there is no revision, no shape and nothing to
/// diff a shape against, so no argument [`evaluate`] is handed could distinguish
/// *"this revision authored a €500-per-period minimum"* from *"this revision
/// authored nothing"*. Closing it needs a **shape diff against the published
/// revision** — a new operand, moving the five plan-shape facets that predate
/// D-319 as well as its own — and that is a decision, not a fix.
///
/// **What holds the line instead is the caller, and it is named here so a reader
/// of this probe knows what to check.** `api::rest::publish::materiality_of`
/// builds this change set from `infra::publish::unit_row_set`, and the only moved
/// row a plan revision could ever hold — a supersession successor another unit
/// staged on an occupied key — is exactly what that set omits;
/// `infra::publish_tests::the_evaluators_row_set_is_the_set_the_commit_publishes`
/// is the other end of it. With the row in, this route published a period floor
/// on one principal. So a later change that widens the change set again reddens
/// `rest_publish::a_period_bound_published_beside_an_orphaned_successor_is_still_judged`
/// and lands back here.
///
/// The **positive control** is the second half: the same shape, the same policy,
/// the same baseline, with the moved row taken out answers the trigger. Without it
/// this case would pass against an evaluator that never answered
/// `alwaysMaterialTrigger` at all.
#[test]
fn a_revision_that_moves_a_row_reaches_no_shape_trigger_however_its_shape_moved() {
    let policy = configured_policy();
    let published = row("USD", 1000);
    let baseline = PublishedPriceBaseline::of_records([published.clone()]);

    // A row moved by 50 against a bar of 500 — below it, so nothing about the row
    // is material either. The revision beside it may have authored any shape at
    // all; no argument here can say, which is the gap.
    let with_a_moved_row = ChangeSet::of_records([row("USD", 1050)]);
    assert_eq!(
        super::triggers::triggered_by_content(&with_a_moved_row, &baseline),
        None,
        "the detector is `moves_no_row`, and one moved row is enough to silence it"
    );
    assert_eq!(
        evaluate(&with_a_moved_row, Some(&policy), Some(&baseline)),
        MaterialityVerdict::AutoPublishable,
        "so the whole change publishes on one principal, shape and all"
    );

    // The control: take the moved row out and the same world is material.
    let shape_only = ChangeSet::of_records([published]);
    assert_eq!(
        evaluate(&shape_only, Some(&policy), Some(&baseline)),
        MaterialityVerdict::triggered(Trigger::PlanShapeRevisionContent),
        "a revision that moves no row is D-115's, and the bound rides that trigger"
    );
}
