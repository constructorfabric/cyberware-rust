//! What the plan views promise on the wire, and that the paths are spelled once.
//!
//! Everything that needs a database or a PDP is in `tests/rest_plans.rs`. What
//! is here is the shape of the body — in particular the one distinction a
//! collapsing serializer would silently destroy.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{PlanSummaryView, PlanView};
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{EntitlementGrants, PlanChangeContract};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::plan::PlanRevision;
use crate::domain::plan_shape::{BillingCycle, CustomIntervalUnit, DescriptorSet, Frequency};
use crate::domain::scope_key::PlanId;

fn revision(plan_id: PlanId) -> PlanRevision {
    PlanRevision {
        plan_id,
        revision: 3,
        sku_id: None,
        plan_tier: Some("gold".to_owned()),
        plan_name: None,
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: Some(Frequency::CustomEveryN {
            n: 45,
            unit: CustomIntervalUnit::Days,
        }),
        plan_tier_override: false,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: None,
        available_from: None,
        available_to: None,
        entitlement_grants: EntitlementGrants::default(),
        change_contract: PlanChangeContract::default(),
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac_12),
        created_at_utc: utc_ymd_hms(2026, 8, 3, 9, 0, 0),
        cloned_from: None,
        row_version: RowVersion::new(4),
    }
}

fn body(view: &PlanView) -> serde_json::Value {
    serde_json::to_value(view).expect("the view serializes")
}

#[test]
fn an_unattached_descriptor_set_is_null_and_an_empty_one_is_an_object() {
    // The store keeps the distinction — an unattached set has no row — and
    // `DESCRIPTOR_INCOMPLETE` is asked of an ATTACHED set, so collapsing the two
    // would make "nobody attached one" and "somebody attached an empty one" the
    // same publish input.
    let plan_id = PlanId::new(Uuid::now_v7());

    let unattached = PlanView::new(
        revision(plan_id),
        utc_ymd_hms(2026, 8, 1, 0, 0, 0),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
    );
    assert!(
        body(&unattached)["descriptor_set"].is_null(),
        "{}",
        body(&unattached)
    );

    let attached = PlanView::new(
        revision(plan_id),
        utc_ymd_hms(2026, 8, 1, 0, 0, 0),
        Vec::new(),
        Vec::new(),
        Some(DescriptorSet::default()),
        Vec::new(),
        Vec::new(),
    );
    let rendered = body(&attached);
    assert!(rendered["descriptor_set"].is_object(), "{rendered}");
    assert!(
        rendered["descriptor_set"]["gl_code"].is_null(),
        "{rendered}"
    );
    assert!(
        rendered["descriptor_set"]["additional"].is_object(),
        "{rendered}"
    );
}

#[test]
fn the_view_names_which_revision_it_answered() {
    // A caller must never have to infer whether it was given the draft or the
    // current revision - the next PATCH depends on it.
    let rendered = body(&PlanView::new(
        revision(PlanId::new(Uuid::now_v7())),
        utc_ymd_hms(2026, 8, 1, 0, 0, 0),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
    ));

    assert_eq!(rendered["revision"], serde_json::json!(3));
    assert_eq!(rendered["lifecycle_state"], serde_json::json!("draft"));
    assert_eq!(rendered["row_version"], serde_json::json!(4));
}

#[test]
fn a_custom_frequency_carries_its_interval_and_a_fixed_one_carries_none() {
    // Three flat members would let a caller send `monthly` with an interval —
    // the pairing the CHECK constraint and `Frequency` both exist to refuse.
    let mut fixed = revision(PlanId::new(Uuid::now_v7()));
    fixed.frequency = Some(Frequency::Monthly);
    let rendered = body(&PlanView::new(
        fixed,
        utc_ymd_hms(2026, 8, 1, 0, 0, 0),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
    ));
    assert_eq!(rendered["frequency"]["kind"], serde_json::json!("monthly"));
    assert!(rendered["frequency"]["custom_interval_n"].is_null());

    let custom = body(&PlanView::new(
        revision(PlanId::new(Uuid::now_v7())),
        utc_ymd_hms(2026, 8, 1, 0, 0, 0),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
    ));
    assert_eq!(
        custom["frequency"]["kind"],
        serde_json::json!("custom_every_n")
    );
    assert_eq!(
        custom["frequency"]["custom_interval_n"],
        serde_json::json!(45)
    );
    assert_eq!(
        custom["frequency"]["custom_interval_unit"],
        serde_json::json!("days")
    );
}

/// **A page says which revision it answered about, and hands back the
/// precondition for editing it.**
///
/// The summary is a different rendering of the same row, so the two members a
/// caller cannot recompute are the ones to hold it to: `revision`, because a
/// page over authoring revisions is answering about the draft rather than the
/// published revision and a caller that assumed otherwise would `PATCH` the
/// wrong number, and `row_version`, because a client that listed and then edited
/// would otherwise have to re-read every row to learn its `If-Match`.
#[test]
fn the_summary_view_carries_the_revision_it_answered_and_its_row_version() {
    let plan_id = PlanId::new(Uuid::from_u128(7));
    let mut row = revision(plan_id);
    // Not the fixture's own number: a view that hardcoded the field would pass
    // against a value the fixture already carries.
    row.revision = 5;
    row.lifecycle_state = LifecycleState::Draft;

    let view = PlanSummaryView::from(&crate::infra::storage::repo::plan_repo::PlanListEntry {
        revision: row.clone(),
        created_at: utc_ymd_hms(2026, 8, 1, 0, 0, 0),
    });

    assert_eq!(view.plan_id, plan_id.get());
    assert_eq!(view.revision, 5);
    assert_eq!(view.lifecycle_state, "draft");
    assert_eq!(
        view.row_version,
        row.row_version.get(),
        "a caller that lists then patches needs the precondition from the page"
    );
}
