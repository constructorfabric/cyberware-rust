//! The two assembly decisions, proven where they are pure.
//!
//! The storage-backed half — that a real draft revision and a real published
//! plane assemble into the shape these constants describe — lands in
//! `tests/sqlite_publish_commit.rs`, where there is a database to put rows in.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::CANDIDATE_ROW_STATES;
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::PlanShape;
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::publish::PlanPublishUnit;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use time::OffsetDateTime;

#[test]
fn the_candidate_row_set_is_the_shape_the_commit_will_leave_behind() {
    // Published plus draft, and nothing else. Draft alone would fail every
    // completeness rule on a revision that authors no price rows; published
    // alone would let a newly authored market go unjudged.
    assert_eq!(
        CANDIDATE_ROW_STATES,
        [LifecycleState::Published, LifecycleState::Draft]
    );
}

#[test]
fn superseded_and_abandoned_rows_are_out_of_the_subject() {
    // A superseded row has had its key taken by a successor and is history;
    // `abandoned` is a plan-revision state that a price row never holds
    // (§4.3, `inst-ps-nodelete`), so naming it would be a second reading of the
    // price row's state set.
    for excluded in [LifecycleState::Superseded, LifecycleState::Abandoned] {
        assert!(
            !CANDIDATE_ROW_STATES.contains(&excluded),
            "{excluded} must not be a candidate state"
        );
    }
}

#[test]
fn the_candidate_states_are_exactly_the_two_a_publish_can_reach() {
    // Every state the machine admits is accounted for: two in, three out. A
    // state added later fails this rather than silently joining or missing the
    // subject.
    assert_eq!(LifecycleState::ALL.len(), 5);
    assert_eq!(CANDIDATE_ROW_STATES.len(), 2);
    assert!(
        !CANDIDATE_ROW_STATES.contains(&LifecycleState::Retired),
        "retirement is a plan state and its own publish unit (D-128), never a price row's"
    );
}

// ---------------------------------------------------------------------------
// What the commit derives before it touches anything.
// ---------------------------------------------------------------------------

/// Both preimages, pinned as values.
///
/// The registry is idempotent on `request_id`, so a commit that rolled back and
/// is retried must present the id its first attempt did. A random id per attempt
/// orphans a pending ref at the registry on every rollback, and every orphan
/// trips `pricing.catalogversion.commit_overdue` for a publish that never
/// happened.
///
/// **A golden vector rather than a self-comparison**, `supersession_tests`'
/// shape one module over: `publish_request_id` is a pure `format!` over its
/// arguments, so `f(x) == f(x)` holds for every body it could possibly have,
/// `String::new()` included — it observes nothing about the token. Only the
/// spelling is the idempotency handle, so only the spelling is worth asserting.
#[test]
fn one_publish_unit_always_presents_the_same_request_id() {
    let tenant = Uuid::from_u128(0x7e_11);
    let plan = PlanId::new(Uuid::from_u128(0x9_1a4));
    let window = Uuid::from_u128(0xd0_01);

    assert_eq!(
        super::publish_request_id(tenant, PlanPublishUnit::plan_content(plan, 2)),
        format!("plan-publish/{tenant}/{plan}/2")
    );
    // The window's own id is inside the token rather than beside it, which is
    // what makes two mutations of one revision two units.
    assert_eq!(
        super::publish_request_id(tenant, PlanPublishUnit::window_mutation(plan, 2, window)),
        format!("window-mutation/{window}/{tenant}/{plan}/2")
    );
}

#[test]
fn two_publish_units_never_share_a_request_id() {
    let tenant = Uuid::from_u128(0x7e_11);
    let other_tenant = Uuid::from_u128(0x7e_22);
    let plan = PlanId::new(Uuid::from_u128(0x9_1a4));
    let other_plan = PlanId::new(Uuid::from_u128(0x9_1a5));
    let unit = PlanPublishUnit::plan_content(plan, 2);

    assert_ne!(
        super::publish_request_id(tenant, unit),
        super::publish_request_id(tenant, PlanPublishUnit::plan_content(plan, 3)),
        "a second revision of one plan is a second publish"
    );
    assert_ne!(
        super::publish_request_id(tenant, unit),
        super::publish_request_id(tenant, PlanPublishUnit::plan_content(other_plan, 2))
    );
    // The tenant is in it, unlike the outbox dedup key: that index is per
    // tenant and the registry is a cross-tenant service with no such scope.
    assert_ne!(
        super::publish_request_id(tenant, unit),
        super::publish_request_id(other_tenant, unit)
    );
}

/// Two units of one plan at one revision - a content publish and a window
/// mutation - are DIFFERENT units, so their version requests do not collide.
///
/// Without the discriminator inside `publish_request_id` the second request
/// dedups onto the first and the window mutation silently rides a version that
/// never re-projected it - which is exactly the "last-warmed delta advertises
/// coverage the truth side removed" exposure D-99 was raised to close.
///
/// **The world is what makes this the discriminator's test rather than the
/// revision's.** A window mutation is a unit over the plan's *current* revision,
/// so the two units genuinely share `(tenant, plan, revision)`: every other term
/// of the preimage is equal by construction here, and the only thing left that can
/// tell them apart is the kind.
#[test]
fn a_window_mutation_and_a_content_publish_of_one_revision_are_two_units() {
    use crate::domain::publish::PublishUnitKind;

    let tenant = Uuid::from_u128(0x7e_11);
    let plan = PlanId::new(Uuid::from_u128(0x9_1a4));
    let window = Uuid::from_u128(0xd0_01);
    let content = PlanPublishUnit::plan_content(plan, 2);
    let mutation = PlanPublishUnit::window_mutation(plan, 2, window);

    assert_eq!(
        (content.plan_id, content.revision),
        (mutation.plan_id, mutation.revision),
        "the two units must share the subject, or this test is about something else"
    );
    assert_ne!(
        super::publish_request_id(tenant, content),
        super::publish_request_id(tenant, mutation)
    );

    // And two window mutations of one revision are two units as well, which is
    // why the window's own id is in the token rather than a bare marker: a
    // schedule on one key and a cancellation on another are separate change sets
    // with separate materiality.
    let other_window = PlanPublishUnit::window_mutation(plan, 2, Uuid::from_u128(0xd0_02));
    assert_ne!(
        super::publish_request_id(tenant, mutation),
        super::publish_request_id(tenant, other_window)
    );

    // The plan-content token is the one it was before there was a kind at all.
    // `request_id` is the registry's idempotency handle, so re-spelling it would
    // make every outstanding pending ref unreachable by the retry meant to find
    // it - `PIN_DOMAIN_SEP`'s question, one plane over.
    assert_eq!(
        super::publish_request_id(tenant, content),
        format!("plan-publish/{tenant}/{plan}/2")
    );
    assert_eq!(PublishUnitKind::default(), PublishUnitKind::PlanContent);
}

#[test]
fn the_audit_before_state_names_the_flip_and_carries_no_version_ref() {
    // Before the commit there is no pending handle, and inventing one would put
    // an addressability claim in the record of the state that preceded it.
    assert_eq!(
        crate::domain::audit::subject_state(LifecycleState::Draft, 3, None),
        serde_json::json!({"lifecycleState": "draft", "rowVersion": 3})
    );
}

#[test]
fn the_audit_after_state_connects_the_flip_to_the_addressability_it_produced() {
    assert_eq!(
        crate::domain::audit::subject_state(LifecycleState::Published, 4, Some("pend-9")),
        serde_json::json!({
            "lifecycleState": "published",
            "rowVersion": 4,
            "pendingVersionRef": "pend-9",
        })
    );
}

/// A supersession successor is not the plan revision's row to publish.
///
/// Found by review, 2026-08-05. Price rows hang off `plan_id`, not off a revision, so
/// the validated set is every draft row of the plan — and since D-195 that can include
/// a successor staged beside the published row it will supersede. Sweeping it up flips
/// it while its predecessor still reads `published`, which the published-plane partial
/// `UNIQUE` refuses as a raw driver error: a **500** on an entirely unrelated revision
/// publish, for as long as a supersession is pending on any key of the plan.
///
/// Driven over a hand-built shape, because the property is about which rows the set
/// *omits* and reaching it through the commit would need a whole staged supersession to
/// demonstrate one row's absence.
#[test]
fn a_draft_on_a_key_a_published_row_already_holds_is_not_this_units_to_publish() {
    let occupied = key(PriceEligibility::AllSubscriptions);
    let free = key(PriceEligibility::NewSubscriptionsOnly);

    let predecessor = row(0x_b1, &occupied, LifecycleState::Published);
    let successor = row(0x_b2, &occupied, LifecycleState::Draft);
    let ordinary = row(0x_b3, &free, LifecycleState::Draft);

    let mut shape = PlanShape::new(PlanId::new(Uuid::from_u128(0x_9f)), 1, now());
    shape.rows = vec![predecessor, successor.clone(), ordinary.clone()];

    let validated = super::validated_draft_rows(&shape);

    assert!(
        validated.contains(&(ordinary.price_id, ordinary.row_version)),
        "the revision's own draft row publishes"
    );
    assert!(
        !validated.contains(&(successor.price_id, successor.row_version)),
        "the successor standing beside a published row does not: it publishes through \
         the unit that staged it, which also flips the predecessor"
    );
    assert_eq!(validated.len(), 1);
}

/// **And it is not this unit's row to be judged on, either** — the instrument that
/// keeps the evaluator's row set and the commit's from drifting apart.
///
/// The case above states the entitlement rule for the *commit*, and for eleven days
/// that was where the sentence stopped: `api::rest::publish::materiality_of` built
/// its change set from `shape.rows` whole. The consequence is the one thing this
/// crate could not otherwise produce — a **moved** row on a plan-revision change set.
/// `create_draft` refuses an occupied key, so a revision can add a price row and can
/// never edit one; the staged successor is the single exception, and with it in the
/// set `triggers::triggered_by_content`'s `moves_no_row` answers `false` and D-115's
/// whole-revision trigger never fires. A revision whose entire content was a D-319
/// period floor then published `AutoPublishable` through the real router.
///
/// So the assertion is **equality of the two sets' draft halves**, not two
/// independent lists: a later reader who narrows one must narrow the other or this
/// reddens.
///
/// The **positive control** is `ordinary`. Without it this case would pass against a
/// `unit_row_set` that returned nothing at all, which is the same fail-closed-looking
/// mistake in the opposite direction: an evaluator handed an empty change set answers
/// the pure-shape trigger for every publish and stops distinguishing anything.
#[test]
fn the_evaluators_row_set_is_the_set_the_commit_publishes() {
    let occupied = key(PriceEligibility::AllSubscriptions);
    let free = key(PriceEligibility::NewSubscriptionsOnly);

    let predecessor = row(0x_c1, &occupied, LifecycleState::Published);
    let successor = row(0x_c2, &occupied, LifecycleState::Draft);
    let ordinary = row(0x_c3, &free, LifecycleState::Draft);

    let mut shape = PlanShape::new(PlanId::new(Uuid::from_u128(0x_9f)), 1, now());
    shape.rows = vec![predecessor.clone(), successor.clone(), ordinary.clone()];

    let judged = super::unit_row_set(&shape);
    let judged_ids: Vec<Uuid> = judged.iter().map(|record| record.price_id).collect();

    assert!(
        judged_ids.contains(&predecessor.price_id),
        "the published plane is what the deltas are taken against and stays in the set"
    );
    assert!(
        judged_ids.contains(&ordinary.price_id),
        "the revision's own draft row is this act's to publish and this act's to be \
         judged on"
    );
    assert!(
        !judged_ids.contains(&successor.price_id),
        "the successor another unit staged is neither: judged here it is a moved row on \
         a plan-revision change set, which is the one shape `moves_no_row` cannot see \
         through"
    );

    // Equality of the draft halves, stated as one assertion rather than as two lists
    // that happen to agree today.
    let mut judged_drafts: Vec<Uuid> = judged
        .iter()
        .filter(|record| record.lifecycle_state == LifecycleState::Draft)
        .map(|record| record.price_id)
        .collect();
    let mut committed: Vec<Uuid> = super::validated_draft_rows(&shape)
        .into_iter()
        .map(|(price_id, _)| price_id)
        .collect();
    judged_drafts.sort_unstable();
    committed.sort_unstable();
    assert_eq!(
        judged_drafts, committed,
        "the rows this unit is judged on and the rows it publishes are one set"
    );
}

/// One key per eligibility class, so two rows can be made to share a key or not.
fn key(class: PriceEligibility) -> ScopeKey {
    ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x_9f)),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("non-blank"),
        PhaseId::new(Uuid::from_u128(0x_fa)),
        class,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("both classes pair with cohort none")
}

/// A minimal candidate row: identity, key and state are all this set reads.
fn row(id: u128, scope_key: &ScopeKey, state: LifecycleState) -> PriceRecord {
    PriceRecord {
        price_id: Uuid::from_u128(id),
        scope_key: scope_key.clone(),
        row: PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat)),
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        // Stated, because this is a **recurring** row and Slice 6's
        // `inst-pi-required` makes the three proration inputs mandatory on one.
        // A fixture that asserts a clean publish needs a row publishable in every
        // respect but the one under judgement, and a row with no proration
        // contract is not.
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
            proration_basis: ProrationBasis::CalendarDaysActual,
            credit_on_downgrade: false,
        }),
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: state,
        created_by: Uuid::from_u128(0x_ac),
        created_at_utc: now(),
        row_version: RowVersion::new(0),
    }
}

fn now() -> OffsetDateTime {
    utc_ymd_hms(2099, 1, 1, 0, 0, 0)
}
