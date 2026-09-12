//! The approval surface's pure pieces: the digest rendering,
//! the pinned-content projection, and the one rule this surface **cannot**
//! enforce.

use uuid::Uuid;

use super::{
    MaterialityView, PinnedContentView, hex, region_grant_of_this_surface,
    report_region_grant_transport,
};
use crate::domain::approval::content_hash;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::materiality::delta::MoveScale;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{MaterialityReason, MaterialityVerdict, TrippedRow};
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::{BillingCycle, PlanShape};
use crate::domain::ports::metrics::{
    AlarmSeverity, CurrencyBindingCase, PreviewFailClosed, PricingAlarm, PricingMetricsPort,
};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use crate::infra::approval::RegionGrant;
use time::OffsetDateTime;

fn instant(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 8, day, 0, 0, 0)
}

/// The scope key the window group below is filed under.
fn scope_key() -> ScopeKey {
    ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x9_1a4)),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xf1)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

fn shape() -> PlanShape {
    let mut shape = PlanShape::new(
        PlanId::new(Uuid::from_u128(0x9_1a4)),
        3,
        utc_ymd_hms(2026, 8, 4, 10, 0, 0),
    );
    shape.sku_id = Some(Uuid::from_u128(0x5_c1));
    shape.plan_tier = Some("gold".to_owned());
    shape.billing_cycle = Some(BillingCycle::Recurring);
    shape.windows = vec![KeyWindows {
        scope_key: scope_key(),
        intervals: vec![WindowInterval::new(
            instant(4),
            Some(instant(11)),
            WindowState::Scheduled,
        )],
    }];
    shape
}

#[test]
fn the_digest_renders_as_lower_case_hex_of_the_whole_pin() {
    let digest = content_hash(&shape());

    let rendered = hex(&digest);

    assert_eq!(rendered.len(), 64, "32 bytes render as 64 hex characters");
    assert!(
        rendered
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    // And it is the digest, not a truncation of it: the last byte survives.
    assert!(rendered.ends_with(&format!("{:02x}", digest[31])));
}

#[test]
fn the_pinned_content_carries_the_plan_the_pin_was_taken_over() {
    let shape = shape();

    let view = PinnedContentView::from(&shape);

    assert_eq!(view.plan_id, shape.plan_id.get());
    assert_eq!(view.revision, shape.revision);
    // `sku_id` is the field the pin was blind to until 2026-08-04, so a reviewer
    // reading this view has to be shown it.
    assert_eq!(view.sku_id, shape.sku_id);
    assert_eq!(view.plan_tier, shape.plan_tier);
    assert_eq!(view.billing_cycle.as_deref(), Some("recurring"));
    // And `windows` is the same case with the operands swapped: the pin framed it
    // from the day it existed and this view did not, so two subjects differing
    // only in their intervals rendered identically here and hashed apart. D-61's
    // invariant is that the document is the content the hash covers.
    assert_eq!(view.windows.len(), 1, "one key, one entry");
    assert_eq!(view.windows[0].scope_key.region, "eu");
    assert_eq!(view.windows[0].scope_key.charge_kind, "recurring");
    assert_eq!(
        view.windows[0].intervals.len(),
        1,
        "one interval, from the shape's own group"
    );
    assert_eq!(view.windows[0].intervals[0].effective_from, instant(4));
    assert_eq!(view.windows[0].intervals[0].effective_to, Some(instant(11)));
    assert_eq!(view.windows[0].intervals[0].state, "scheduled");
}

/// The document a reviewer reads and the digest they sign move **together**.
///
/// The property the omission broke, stated as one assertion rather than as a list
/// of members: change the window plane and both the pin and the view change; change
/// nothing and neither does. A member missing from the view fails the first half
/// while every field-by-field assertion above still passes, because a field nobody
/// renders cannot disagree with anything.
#[test]
fn a_window_the_pin_moves_for_moves_the_document_too() {
    let pinned = shape();
    let mut moved = shape();
    moved.windows[0].intervals[0].effective_to = Some(instant(12));

    assert_ne!(
        content_hash(&pinned),
        content_hash(&moved),
        "the interval is content: the pin has to move"
    );
    assert_ne!(
        serde_json::to_value(PinnedContentView::from(&pinned)).expect("render the pinned document"),
        serde_json::to_value(PinnedContentView::from(&moved)).expect("render the moved document"),
        "and the document the reviewer reads has to move with it"
    );
}

/// The verdict round-trips through the column it is stored in.
///
/// One shape for the store and the wire, so the reviewer reads back what the
/// submit wrote. Asserted through `serde_json` because that is the path the
/// column takes.
#[test]
fn the_materiality_verdict_survives_the_column_it_is_stored_in() {
    let verdict = MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold);

    let stored = serde_json::to_value(MaterialityView::from(&verdict)).expect("render");
    let read: MaterialityView = serde_json::from_value(stored).expect("read back");

    assert!(read.material);
    assert_eq!(read.reason.as_deref(), Some("noConfiguredThreshold"));
    assert_eq!(
        read.trigger, None,
        "a fail-safe is an answer about the policy, so it names no act"
    );
}

/// **§6's third declared member reaches the column**: the act, not merely the rule.
///
/// The reason token is one word — `alwaysMaterialTrigger` — for eighteen registered
/// acts, so a stored verdict carrying it alone told an auditor a second principal
/// was required and could not tell them what for. D-104 registers *two* bundle
/// triggers precisely so a rev-share re-split and a component swap are
/// distinguishable in this document, and until this member existed they rendered
/// byte-identically.
///
/// Rendered **and read back**, because the column is parsed by `ApprovalView::from`
/// with a `.ok()` that turns an unparseable document into a `null` materiality — so
/// a member that serializes and will not deserialize would blank the whole verdict
/// on the reviewer's screen rather than fail loudly.
#[test]
fn a_registered_acts_verdict_carries_the_trigger_that_declared_it() {
    let verdict = MaterialityVerdict::triggered(Trigger::RevenueShareChange);

    let stored = serde_json::to_value(MaterialityView::from(&verdict)).expect("render");
    assert_eq!(
        stored["reason"], "alwaysMaterialTrigger",
        "the discriminator is unchanged; the act rides beside it"
    );
    assert_eq!(
        stored["trigger"], "revenueShareChange",
        "section 6's `trigger source`, in the registry's own spelling"
    );

    let read: MaterialityView = serde_json::from_value(stored.clone()).expect("read back");
    assert_eq!(read.trigger.as_deref(), Some("revenueShareChange"));

    // And the sibling act renders differently, which is the whole point: a
    // classifier that answered one trigger for both would satisfy every assertion
    // above.
    let sibling = serde_json::to_value(MaterialityView::from(&MaterialityVerdict::triggered(
        Trigger::BundleComposition,
    )))
    .expect("render");
    assert_ne!(
        sibling["trigger"], stored["trigger"],
        "D-104's two acts must not render alike, which is the state D-232 recorded"
    );
}

/// **A verdict stored before this member existed still parses**, which is what
/// keeps every already-open unit readable.
///
/// The bytes below are exactly what a pre-2026-08-16 submit wrote. `MaterialityView`
/// is `(request, response)` because the column is read *back*, and
/// `ApprovalView::from` swallows a parse failure into `materiality: null` — so a
/// required member added here would silently blank the verdict on the detail screen
/// of every unit opened before the deploy, on the one surface the two-person rule
/// exists to put in front of a second principal. `Option` is what prevents that
/// (serde reads a missing key as `None`), and this is the case that holds it.
///
/// It is deliberately a **literal document** rather than a round-trip: a round-trip
/// through today's writer can only ever produce today's shape, so it could not fail.
#[test]
fn a_stored_verdict_written_before_the_trigger_member_still_parses() {
    let legacy = serde_json::json!({
        "material": true,
        "reason": "alwaysMaterialTrigger",
        "tripped": null,
    });

    let read: MaterialityView = serde_json::from_value(legacy).expect(
        "a document written before the trigger member must still read, or every unit \
         opened before the deploy loses its verdict on the reviewer's screen",
    );

    assert!(read.material);
    assert_eq!(read.reason.as_deref(), Some("alwaysMaterialTrigger"));
    assert_eq!(
        read.trigger, None,
        "an absent key is an unknown act, never a wrong one"
    );
}

/// **A rate's move keeps the scale it was measured at, all the way to the
/// document the approver reads** (D-311).
///
/// The case above round-trips a verdict with **no** tripped row, so every
/// member of [`super::TrippedRowView`] was unobserved by it — which is how the
/// view came to drop `MoveScale` while documenting its two amounts as minor
/// units. A `per_unit` rate of `$0.230777165` is stored as `230_777_165`
/// nano-minor, and rendered under a `minor` label that is a factor of `10⁹`
/// out: `$2,307,771.65` presented to the second principal whose signature is
/// the whole of the two-person rule.
///
/// The numbers are deliberately not round — neither end is a whole count of
/// minor units, so a view that converted rather than labelled cannot pass this
/// by truncating both to the same value, and neither is a multiple of the
/// other.
#[test]
fn a_tripped_rates_move_carries_the_scale_it_was_measured_at() {
    let price_id = Uuid::from_u128(0x7_a7e);
    let verdict = MaterialityVerdict::tripped_row(TrippedRow {
        price_id,
        currency: CurrencyCode::new("USD").expect("three letters"),
        from_minor: 230_777_165,
        to_minor: 246_931_407,
        scale: MoveScale::NanoMinor,
    });

    let stored = serde_json::to_value(MaterialityView::from(&verdict)).expect("render");
    assert_eq!(
        stored["tripped"],
        serde_json::json!({
            "price_id": price_id,
            "currency": "USD",
            "from_minor": 230_777_165,
            "to_minor": 246_931_407,
            "scale": "nanoMinor",
        }),
        "the stored document has to say which units its two amounts are in, or an approver reads \
         a rate move as an amount move a billion times its size: {stored}"
    );

    let read: MaterialityView = serde_json::from_value(stored).expect("read back");
    let tripped = read
        .tripped
        .expect("a threshold-reached verdict names its row");
    assert_eq!(
        tripped.scale, "nanoMinor",
        "and it parses back out of the column, which is where the approvals surface reads it"
    );

    // The other scale, so the label is the move's own fact and not a constant.
    // `flat`'s money is whole minor units and its move must still say so.
    let flat = MaterialityVerdict::tripped_row(TrippedRow {
        price_id,
        currency: CurrencyCode::new("USD").expect("three letters"),
        from_minor: 9_900,
        to_minor: 12_000,
        scale: MoveScale::Minor,
    });
    assert_eq!(
        serde_json::to_value(MaterialityView::from(&flat)).expect("render")["tripped"]["scale"],
        serde_json::json!("minor"),
        "an amount move is still in the currency's own minor units"
    );
}

/// **`inst-ap-scope` is not enforced on this surface, and this is the test that
/// says so out loud.**
///
/// It is not a test of a feature; it is a pin on a gap, so that the day a
/// pricing-region grant gets a declared transport the change is visible instead
/// of silent. See [`region_grant_of_this_surface`]'s doc for the whole argument:
/// nothing in the design set says how the grant travels, `SecurityContext`
/// carries no claim that could hold it, and `authz::SUPPORTED_PROPERTIES` is two
/// uuid-typed properties.
///
/// What the surface hands over is therefore the **fact** that it has no grant to
/// hand over, and `infra::approval::judge` resolves that against the change set
/// it re-derived itself — so `change_set_regions.is_subset(approver_regions)`
/// holds by construction and `REGION_SCOPE_DENIED` is unreachable over HTTP. The
/// rule itself is built and both its directions are driven through the service,
/// in `tests/sqlite_approval_service.rs`, under [`RegionGrant::Explicit`]. What
/// is missing is one transport — and, with it, any pin on the **wiring**: what
/// `decide_record` hands `DecideRequest::approver_regions` is asserted
/// nowhere, and while the grant is `Untransported` a dropped assignment would
/// change no answer.
///
/// **The assertion is about the variant and not about a set**, because a set
/// computed at this surface is established before the judgement transaction
/// while `judge` re-derives its change set inside one. Two reads at two times
/// is what answers `OutOfScope` for a reviewer who did nothing wrong — a 403
/// and a `deny` record — the moment a mutation adds a row in a new region
/// between them.
#[test]
fn the_region_rule_is_not_enforced_at_this_surface() {
    assert_eq!(region_grant_of_this_surface(), RegionGrant::Untransported);
}

/// **The gap is on the alarm plane and not only in these comments.**
///
/// A pin on the boot report rather than on the sentence describing it: source
/// comments are what the region gap had for its whole life, and a reader of them
/// is whoever went looking. The alarm is what an alerting rule matches, so this
/// asserts the exact variant — an `is_empty()` check would pass on any alarm at
/// all, including one raised for another plane entirely.
#[test]
fn the_unenforced_region_rule_is_reported_at_boot() {
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<PricingAlarm>>);
    impl PricingMetricsPort for Recording {
        fn preview_failclosed(&self, _reason: PreviewFailClosed) {}
        fn currency_binding_block(&self, _case: CurrencyBindingCase) {}
        fn tax_not_sellable_ga(&self, _count: i64) {}
        fn alarm(&self, alarm: PricingAlarm) {
            self.0.lock().expect("a test-local lock").push(alarm);
        }
    }

    let recorder = Recording::default();
    report_region_grant_transport(&recorder);

    let raised = recorder.0.lock().expect("a test-local lock").clone();
    assert_eq!(raised, [PricingAlarm::RegionGrantUntransported]);
    assert_eq!(
        raised[0].severity(),
        AlarmSeverity::Warn,
        "a standing gap somebody has to close, not a page in the night"
    );
}
