//! The per-key pending register — `inst-co-single-pending` as the rule the design
//! set states, rather than as the plan-revision lock that stood in for it.
//!
//! # What this suite owns that no other can
//!
//! The rule is *"at most one pending approval unit **of any kind** may hold a
//! canonical scope key"*, and §5 glosses the code the same way: *"a pending unit
//! already holds one of the touched keys"*. Both sentences are about a **set of
//! keys**. What the crate enforced before this suite was `subject_ref LIKE
//! '<plan_id>/%'` over `subject_kind = 'plan_revision'` — a plan-revision lock,
//! which is a *consequence* of the rule on a plan whose whole key set one unit
//! holds and is neither necessary nor sufficient for it.
//!
//! So two of the three contention cases below **would have passed against the old
//! code**, and one of them is why the third is here at all:
//!
//! * [`a_window_unit_cannot_open_over_a_key_a_plan_unit_holds`] and
//!   [`a_plan_unit_cannot_open_over_a_key_a_window_unit_holds`] fail against the
//!   prefix match, and they fail for the right reason — it carries a
//!   `subject_kind` filter, so a window unit and a plan unit over one key were
//!   invisible to each other.
//! * [`two_units_on_disjoint_keys_of_one_plan_both_open`] is the **negative
//!   control**, and it is the one that makes the other two mean anything. A
//!   plan-level lock refuses both of the first two cases as well — for the wrong
//!   reason — so without a case that a plan-level lock **breaks**, the widening
//!   cannot be told from the code it replaced. This is that case: it passes under
//!   the register and reddens under any lock coarser than a key.
//!
//! # And the register is read back, not merely refused off
//!
//! A refusal test alone cannot tell a unit that held the right keys from one that
//! held every key of the tenant — a register that over-held would refuse
//! *everything* and pass every contention case in this file.
//! [`a_pending_unit_holds_exactly_the_keys_of_its_change_set`] and
//! [`a_decided_unit_frees_every_key_it_held`] are the observability twins, and the
//! second is load-bearing in its own right: the register follows its unit out of
//! `submitted` through a **trigger**, and a sync that silently stopped happening
//! would hold every key forever with nothing about the parent row looking wrong.
//!
//! # And the three properties D-99 states, which are this file's other half
//!
//! `inst-ws-publishunit` makes every window mutation a publish unit, and §9 states
//! what that has to *mean* rather than only what it is called: the mutation records a
//! pending version ref carrying the plan subject, the change is invisible until that
//! version is pin-eligible, a frozen version never moves, and the coverage check runs
//! inside the mutation rather than only at publish. The three tests §9 names —
//! [`a_schedule_is_a_publish_unit_and_is_invisible_until_the_version_is_pin_eligible`],
//! [`a_cancel_re_projects_and_the_old_pin_still_answers_the_old_coverage`] and
//! [`a_schedule_that_opens_a_gap_is_refused_inside_the_mutation`] — are here, and each
//! reads the **`pricing_catalog_version_ref` row** rather than the response body's
//! handle. That distinction is the whole reason they exist: the registry supplies the
//! handle *before* the row is written, so every assertion that read it off the response
//! was satisfied by a mutation that never recorded the ref at all — and wrapping the
//! `record_pending` call in `if false` left the suite green.
//!
//! # Every plan here is **published**, and that is a premise rather than staging
//!
//! A window unit's subject is a window of a published plan:
//! `ApprovalService::submit_window_mutation` resolves the plan's *current*
//! revision, exactly as `infra::window` does, so a plan that never published has
//! no window unit to open. The plan is therefore published for real before any of
//! it, and the successor draft the plan-revision cases need is opened explicitly.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

use std::sync::Arc;

use bss_pricing::config::JobsConfig;
use bss_pricing::domain::approval::{DecisionBy, WithdrawAuthority};
use bss_pricing::domain::audit::AuditSubjectKind;
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::scope_key::{Cohort, PlanId, PriceEligibility};
use bss_pricing::domain::window::{CoverageEnd, KeyWindows, WindowInterval, WindowState};
use bss_pricing::infra::approval::{DecideRequest, RegionGrant};
use bss_pricing::infra::jobs::readmodel_warm::ReadModelWarmJob;
use bss_pricing::infra::storage::repo::{approval_repo, window_repo};
use bss_pricing::infra::window::WindowMutationOutcome;

use bss_pricing::domain::instant::format_rfc3339;
use bss_pricing::domain::instant::utc_ymd_hms;
use rest_support::{Harness, Publishable, seed_price, seed_publishable_plan, seed_window};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

/// The materiality verdict every unit in this suite carries.
///
/// `NoConfiguredThreshold` because that is what G6's threshold policy answers
/// until it exists, and it is the same value `api::rest::publish` sends. Nothing
/// here asserts on it — it rides through to the store untouched.
fn materiality() -> serde_json::Value {
    json!({ "reason": "noConfiguredThreshold" })
}

/// A published plan with **two** billable keys, and the windows on both.
///
/// Two keys because the negative control needs a pair a coarser lock could not
/// tell apart, and published because every window path resolves the plan's current
/// revision. `publish_price` moves the rows so the activation sweep and the
/// projector can see them too, which is what the pin-seam case one file over
/// depends on.
struct TwoKeys {
    /// The `eu` key the seed's row and window sit on.
    eu_key: String,
    /// The `us` key of the second row.
    us_key: String,
    /// The window on the `eu` key — the seed's own coverage window.
    eu_window: Uuid,
    /// The window on the `us` key.
    us_window: Uuid,
    seeded: Publishable,
}

async fn two_keys(h: &Harness, plan_id: Uuid) -> TwoKeys {
    let seeded = seed_publishable_plan(h, plan_id).await;
    let second = seed_price(h, plan_id, "US").await;
    let us_window = seed_window(h, second.price_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    h.publish_price(plan_id, second.price_id).await;
    TwoKeys {
        eu_key: rest_support::publishable_scope_key(PlanId::new(plan_id), seeded.phase, "eu")
            .to_string(),
        us_key: second.scope_key.to_string(),
        eu_window: common::coverage_window_id(seeded.price_id),
        us_window,
        seeded,
    }
}

/// Every key `approval_id` holds, read off the register.
async fn held(h: &Harness, approval_id: Uuid) -> Vec<String> {
    let conn = h.db.conn().expect("conn");
    approval_repo::held_keys_of(&conn, &h.scope(), h.tenant, approval_id)
        .await
        .expect("read the register")
}

/// The keys `approval_id` still holds — the ones whose register row is `submitted`.
async fn still_held(h: &Harness, approval_id: Uuid) -> Vec<String> {
    let conn = h.db.conn().expect("conn");
    approval_repo::held_keys_still_pending(&conn, &h.scope(), h.tenant, approval_id)
        .await
        .expect("read the register")
}

/// Open a window unit over `window_id`.
///
/// The price row is read out of the window here because every window in this suite is
/// already stored. Production cannot do that on the schedule path — the window does not
/// exist when its unit opens — which is why `submit_window_mutation` takes the row and
/// not the window as its subject resolver.
/// One subject ref, in the shape `Planned::unit_subject_ref` builds for a cancel:
/// `<plan_id>/<window_id>/<op>/<prior end>/<new end>`. The plan and window ids are
/// this suite's fixtures; what matters to its callers is only that the ref parses.
fn a_subject_ref(plan_id: Uuid, window_id: Uuid) -> String {
    format!("{plan_id}/{window_id}/cancel/0/open/open")
}

async fn submit_window(h: &Harness, window_id: Uuid, approval_id: Uuid) -> Result<(), DomainError> {
    let conn = h.db.conn().expect("conn");
    let price_id = window_repo::find(&conn, &h.scope(), h.tenant, window_id)
        .await
        .expect("read the window")
        .expect("the suite's windows are stored")
        .price_id;
    let plan_id = bss_pricing::infra::storage::repo::price_repo::load_scope_key(
        &conn,
        &h.scope(),
        h.tenant,
        price_id,
    )
    .await
    .expect("read the price row's key")
    .expect("the suite's rows are stored")
    .plan_id()
    .get();
    // Runner-taking since D-191, so that a route's gate can own the transaction. A
    // suite driving it directly supplies the runner itself.
    let conn =
        h.db.conn()
            .map_err(|e| DomainError::Internal(format!("scoped connection: {e}")))?;
    bss_pricing::infra::approval::ApprovalService::submit_window_mutation_on(
        &conn,
        &h.scope(),
        h.tenant,
        window_id,
        price_id,
        approval_id,
        materiality(),
        rest_support::seed_stamp(),
        // A fixed subject in the shape the service builds for a cancel. This helper's
        // callers assert on the record it opens — its state, its held key, its subject —
        // and never re-issue a mutation that would have to resolve against it, so only
        // the shape matters.
        &a_subject_ref(plan_id, window_id),
    )
    .await
    .map(|_| ())
}

/// Open a plan-revision unit over the plan's open draft.
async fn submit_plan(h: &Harness, plan_id: Uuid, approval_id: Uuid) -> Result<(), DomainError> {
    h.governance
        .approvals
        .submit(
            &h.scope(),
            h.tenant,
            PlanId::new(plan_id),
            approval_id,
            materiality(),
            rest_support::seed_stamp(),
        )
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// The register, read back
// ---------------------------------------------------------------------------

/// A plan-revision unit holds **exactly** the keys of its change set — both of
/// them, and nothing else.
///
/// The world in which every refusal below is a fact about the rule it names rather
/// than about a register that holds everything. Asserted as the **whole list**
/// rather than as a length or a `contains`: a register that held the tenant's every
/// key would satisfy any count of two on a two-key plan, and would refuse every
/// submit in the tenant while passing all three contention cases in this file.
#[tokio::test]
async fn a_pending_unit_holds_exactly_the_keys_of_its_change_set() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;
    h.open_successor(plan_id).await;

    let unit = Uuid::from_u128(0x_e1);
    submit_plan(&h, plan_id, unit)
        .await
        .expect("the plan's draft revision opens a unit");

    let mut expected = vec![keys.eu_key.clone(), keys.us_key.clone()];
    expected.sort();
    assert_eq!(
        held(&h, unit).await,
        expected,
        "a plan unit holds the keys its publish would freeze, and only those"
    );

    // And a window unit holds **one** key, which is the asymmetry the rule rests
    // on: a window mutation can move only its own key's interval set.
    let other = Uuid::now_v7();
    let other_keys = two_keys(&h, other).await;
    let window_unit = Uuid::from_u128(0x_e2);
    submit_window(&h, other_keys.us_window, window_unit)
        .await
        .expect("a window of a published plan opens a unit");
    assert_eq!(
        held(&h, window_unit).await,
        vec![other_keys.us_key],
        "a window unit holds its own key and not the plan's set"
    );
}

/// A decided unit **frees** every key it held, and the trigger is what does it.
///
/// `inst-as-void`'s withdraw is the escape from the pin, and it is an escape only
/// if the register follows the unit. Nothing in this crate writes the register after
/// the insert — `trg_pricing_approval_key_follow_state` carries `state` across — so
/// a sync that stopped happening would leave every key held by a unit nobody can
/// decide again, and the parent row would look perfect.
///
/// The register rows are asserted to **survive** the decision as well: they are the
/// record of what the unit held, and `DELETE` is refused on that table for the same
/// reason it is refused on the unit.
#[tokio::test]
async fn a_decided_unit_frees_every_key_it_held() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;

    let first = Uuid::from_u128(0x_e3);
    submit_window(&h, keys.eu_window, first)
        .await
        .expect("the first unit opens");
    assert_eq!(still_held(&h, first).await, vec![keys.eu_key.clone()]);

    // The submitter withdraws it — `inst-as-void`, the one escape from the pin.
    //
    // **The withdrawer is taken from the submitting stamp, not named separately.**
    // This call passed `SEED_ACTOR` (`0xac70`) until 2026-08-11 while `submit_window`
    // opens the unit under `seed_stamp()` (`0xac10`), so the case was a *third
    // party's* withdraw wearing a comment that said "the submitter". It passed
    // because nothing enforced `inst-as-void`'s identity half, and it is exactly the
    // hole `a_stranger_cannot_withdraw_a_unit_they_did_not_submit` now covers. Taking
    // both from one value makes the two agree by construction rather than by a
    // constant somebody has to keep aligned.
    let submitting_stamp = rest_support::seed_stamp();
    h.governance
        .approvals
        .decide(
            &h.scope(),
            h.tenant,
            bss_pricing::infra::approval::DecideRequest {
                approval_id: first,
                decision: bss_pricing::domain::approval::DecisionBy::Void(Some(
                    submitting_stamp.actor_principal_id,
                )),
                reason: None,
                approver_regions: bss_pricing::infra::approval::RegionGrant::Explicit(
                    std::collections::BTreeSet::new(),
                ),
                stamp: submitting_stamp,
                // The withdrawer **is** the submitter, so no catalog authority is
                // needed — which is the case `inst-as-void` is centrally about.
                withdraw_authority: bss_pricing::domain::approval::WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the submitter withdraws the unit they opened");

    assert!(
        still_held(&h, first).await.is_empty(),
        "a withdrawn unit holds nothing: {:?}",
        still_held(&h, first).await
    );
    assert_eq!(
        held(&h, first).await,
        vec![keys.eu_key.clone()],
        "and the register still records what it held - the row is evidence, not a lock"
    );

    // The freed key is really free: a second unit over it opens.
    let second = Uuid::from_u128(0x_e4);
    submit_window(&h, keys.eu_window, second)
        .await
        .expect("the withdrawn unit freed the key");
}

/// **A principal who did not submit the unit cannot close it, and the key it holds
/// stays held** (`inst-as-void`).
///
/// The identity half of that instruction was enforced at **neither** layer until
/// 2026-08-11. `authorize_decision`'s two identity rules live inside
/// `if let Some(approver)`, and `approver()` is `None` on every void, so a withdraw
/// skipped both and nothing else looked at who was asking: what the code
/// implemented was *any principal the gate admitted may close any `submitted` unit
/// of the tenant*.
///
/// The second assertion is the one that says why it matters. A withdraw is not
/// cosmetic — the case above proves it releases the canonical scope keys the unit
/// held — so an unauthorized withdraw is an unauthorized **unlock**: a reviewer who
/// could not approve a change could close somebody else's review of it and re-open
/// the key to whoever wanted it. Asserting only the refusal would leave that
/// consequence untested.
#[tokio::test]
async fn a_stranger_cannot_withdraw_a_unit_they_did_not_submit() {
    const STRANGER: Uuid = Uuid::from_u128(0x_e5_51);

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;

    let unit = Uuid::from_u128(0x_e5);
    submit_window(&h, keys.eu_window, unit)
        .await
        .expect("the unit opens");
    assert_eq!(still_held(&h, unit).await, vec![keys.eu_key.clone()]);

    let refused = h
        .governance
        .approvals
        .decide(
            &h.scope(),
            h.tenant,
            DecideRequest {
                approval_id: unit,
                decision: DecisionBy::Void(Some(STRANGER)),
                reason: None,
                approver_regions: RegionGrant::Explicit(std::collections::BTreeSet::new()),
                stamp: rest_support::stamp_of(STRANGER, OffsetDateTime::now_utc()),
                // No catalog authority: this is the `FinanceReviewer`-shaped caller
                // the gate admits and `inst-as-void` does not name.
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await;

    match refused {
        Err(DomainError::WithdrawForbidden(_)) => {}
        other => panic!("a stranger's withdraw must be refused, got: {other:?}"),
    }

    assert_eq!(
        still_held(&h, unit).await,
        vec![keys.eu_key.clone()],
        "the refused withdraw released nothing, which is the consequence rather than the status code"
    );
}

// ---------------------------------------------------------------------------
// inst-co-single-pending: of ANY kind
// ---------------------------------------------------------------------------

/// A window unit cannot open over a key a **plan** unit holds.
///
/// The statement only the register can refuse: the plan unit's subject is
/// `<plan_id>/<revision>`, the window unit's is a window id, so nothing about the
/// two subjects contends — what contends is the key, and the key is the only place
/// it is written down. The prefix match this replaced filtered on
/// `subject_kind = 'plan_revision'` and could not see this submit at all.
#[tokio::test]
async fn a_window_unit_cannot_open_over_a_key_a_plan_unit_holds() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;
    h.open_successor(plan_id).await;

    let plan_unit = Uuid::from_u128(0x_f1);
    submit_plan(&h, plan_id, plan_unit)
        .await
        .expect("the plan unit opens first");

    let err = submit_window(&h, keys.eu_window, Uuid::from_u128(0x_f2))
        .await
        .expect_err("the plan unit holds this window's key");
    match err {
        DomainError::PendingChangeUnitExists(detail) => {
            assert!(
                detail.contains(&keys.eu_key),
                "the refusal names the contended key: {detail}"
            );
            assert!(
                detail.contains(&plan_unit.to_string()),
                "and the unit holding it, so the operator knows what to decide: {detail}"
            );
        }
        other => panic!("expected PENDING_CHANGE_UNIT_EXISTS, got {other:?}"),
    }
}

/// A plan unit cannot open over a key a **window** unit holds — the same rule from
/// the other side.
///
/// Not symmetric with the case above and worth its own test for that reason: the
/// window unit is the one the old prefix match was blind to, so this is the
/// direction in which the register is the *only* thing standing. A plan-revision
/// submit under an outstanding window unit succeeded before it existed.
#[tokio::test]
async fn a_plan_unit_cannot_open_over_a_key_a_window_unit_holds() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;

    let window_unit = Uuid::from_u128(0x_f3);
    submit_window(&h, keys.us_window, window_unit)
        .await
        .expect("the window unit opens first");

    h.open_successor(plan_id).await;
    let err = submit_plan(&h, plan_id, Uuid::from_u128(0x_f4))
        .await
        .expect_err("the window unit holds one of the revision's keys");
    match err {
        DomainError::PendingChangeUnitExists(detail) => {
            assert!(
                detail.contains(&keys.us_key),
                "the refusal names the contended key: {detail}"
            );
            assert!(
                detail.contains(&window_unit.to_string()),
                "and the window unit holding it: {detail}"
            );
        }
        other => panic!("expected PENDING_CHANGE_UNIT_EXISTS, got {other:?}"),
    }
    // The plan-revision subject itself was free — nothing was reviewing a revision
    // — so the refusal is the key's and not the prefix match's.
    assert_eq!(
        rest_support::approval_rows(&h)
            .await
            .iter()
            .filter(|row| row.subject_kind == AuditSubjectKind::PlanRevision.as_str())
            .count(),
        0,
        "no plan-revision unit exists, so the prefix match cannot be what refused"
    );
}

/// **The negative control, and the test that makes the two above mean anything**:
/// two units on **disjoint** keys of one plan both open.
///
/// The pre-existing `<plan_id>/` prefix match refused a second unit over one plan
/// whatever it touched, so a plan-level lock passes both contention cases above for
/// the wrong reason. This is the case a plan-level lock **breaks**: two window units
/// on two keys of one plan are two reviews of two independent interval sets, and
/// `inst-co-single-pending` scopes the pin to the key precisely so they can run at
/// once.
///
/// Both units are read back off the register, so a pass here is not a pass by two
/// submits that held nothing.
#[tokio::test]
async fn two_units_on_disjoint_keys_of_one_plan_both_open() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;

    let on_eu = Uuid::from_u128(0x_f5);
    let on_us = Uuid::from_u128(0x_f6);
    submit_window(&h, keys.eu_window, on_eu)
        .await
        .expect("the eu key is free");
    submit_window(&h, keys.us_window, on_us)
        .await
        .expect("and so is the us key: the pin is per key, not per plan");

    assert_eq!(held(&h, on_eu).await, vec![keys.eu_key.clone()]);
    assert_eq!(held(&h, on_us).await, vec![keys.us_key.clone()]);
    assert_ne!(
        keys.eu_key, keys.us_key,
        "the two keys really are disjoint, which is the whole premise"
    );

    // And the pin is real on both: a third unit over either key is refused. Without
    // this the test is satisfied by a register that holds nothing at all.
    let err = submit_window(&h, keys.eu_window, Uuid::from_u128(0x_f7))
        .await
        .expect_err("the eu key is held now");
    assert!(matches!(err, DomainError::PendingChangeUnitExists(_)));
}

/// A **second unit over one window** is refused even though the key is the same
/// one that unit already holds.
///
/// The per-subject half of the rule, which the register cannot answer: the key is
/// held by the very unit the second submit would duplicate, so a check that looked
/// only at the register would have to decide whether a unit conflicts with itself.
/// It does not — it conflicts with the *subject*, and two reviewers deciding two
/// records over one window mutation is what `inst-co-single-pending` forbids
/// whatever the key says.
#[tokio::test]
async fn a_second_unit_over_one_window_is_refused_on_the_subject() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;

    let first = Uuid::from_u128(0x_f8);
    submit_window(&h, keys.eu_window, first)
        .await
        .expect("the first unit opens");
    let err = submit_window(&h, keys.eu_window, Uuid::from_u128(0x_f9))
        .await
        .expect_err("one window, one unit");
    match err {
        DomainError::PendingChangeUnitExists(detail) => assert!(
            detail.contains(&keys.eu_window.to_string()),
            "the refusal names the window, not merely a key: {detail}"
        ),
        other => panic!("expected PENDING_CHANGE_UNIT_EXISTS, got {other:?}"),
    }
    let _ = keys.seeded;
    let _ = first;
}

// ---------------------------------------------------------------------------
// The register is read by the mutation too, before it touches anything
// ---------------------------------------------------------------------------

/// A window mutation on a held key is refused **before anything is touched**.
///
/// The half of the rule that is about the mutation rather than about a second
/// submit: committing a window mutation under an outstanding review would leave a
/// reviewer approving intervals that had already moved, and `inst-ap-pin`'s void
/// rail cannot catch it — the rail hangs off the *authoring* audit writers
/// (`record_revision_mutation`, `record_price_mutation`) and a window mutation is
/// neither.
///
/// "Before it touches anything" is asserted rather than argued, and on all three of
/// the things a mutation writes: the window plane, the outbox and the audit log. A
/// refusal raised after the registry request would leave a pending version ref
/// stranded, and a refusal raised after the write would have to roll back an
/// `UPDATE` the append-only trigger refuses.
#[tokio::test]
async fn a_window_mutation_on_a_held_key_is_refused_before_it_touches_anything() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let keys = two_keys(&h, plan_id).await;

    let before_windows = window_plane(&h, plan_id).await;
    let before_outbox = rest_support::outbox_correlations(&h).await.len();
    let before_audit = rest_support::audit_rows(&h).await.len();

    let unit = Uuid::from_u128(0x_fa);
    submit_window(&h, keys.eu_window, unit)
        .await
        .expect("a unit holds the eu key");
    let after_submit_outbox = rest_support::outbox_correlations(&h).await.len();
    let after_submit_audit = rest_support::audit_rows(&h).await.len();

    // The cancel of the very window under review.
    let err = h
        .governance
        .windows
        .cancel(
            &rest_support::security_context(rest_support::SEED_ACTOR, h.tenant),
            &h.scope(),
            h.tenant,
            keys.eu_window,
            bss_pricing::api::rest::windows::verdict_json,
            rest_support::seed_stamp(),
        )
        .await
        .expect_err("a review of this key is in progress");
    match err {
        DomainError::PendingChangeUnitExists(detail) => {
            assert!(
                detail.contains(&keys.eu_key),
                "the refusal names the key: {detail}"
            );
            assert!(
                detail.contains(&unit.to_string()),
                "and the unit to decide or withdraw: {detail}"
            );
        }
        other => panic!("expected PENDING_CHANGE_UNIT_EXISTS, got {other:?}"),
    }

    assert_eq!(
        window_plane(&h, plan_id).await,
        before_windows,
        "the window plane is exactly as the refusal found it"
    );
    assert_eq!(
        rest_support::outbox_correlations(&h).await.len(),
        after_submit_outbox,
        "no event was enqueued"
    );
    assert_eq!(
        rest_support::audit_rows(&h).await.len(),
        after_submit_audit,
        "and no audit record was written for a mutation that did not happen"
    );
    // **The non-emptiness control, on the plane the fixture actually moved.** It
    // used to read `after_submit_outbox >= before_outbox && ...`, and
    // `outbox_correlations` returns an append-only row list, so the outbox conjunct
    // was true by construction: the "no event was enqueued" assertion above could
    // have been comparing two counts of zero and this control would not have said
    // so. It is zero — measured 2026-08-20, `>` on the outbox half reddens, because
    // a window submit enqueues nothing — so the control is taken from the audit
    // count, which the submit does move. That measurement is pinned rather than left
    // in prose: if a submit ever does enqueue, the conjunct becomes available again
    // and this line is what says so.
    assert_eq!(
        after_submit_outbox, before_outbox,
        "a window submit enqueues no outbox event, which is why the control below cannot be \
         taken from this plane"
    );
    assert!(
        after_submit_audit > before_audit,
        "the submit itself did write its own audit trail, so the count above is not comparing \
         two empty stores"
    );

    // And a mutation on a **sibling** key is not refused: the pin is per key here
    // too, so a review of one key does not freeze the plan's whole time axis.
    //
    // The sibling mutation is a **schedule on an uncovered third key**, and the
    // choice is the point rather than convenience. A cancel of the `us` window is
    // refused — by `WINDOW_TRAILING_VOID`, because that window is the key's only
    // coverage and it is open-ended — so using one here would have passed a test
    // named for the register while the register was doing nothing. What is needed is
    // a mutation **only** the register could refuse: a schedule adds coverage, so
    // `Op::may_remove_coverage` is false and the trailing floor is never evaluated;
    // a first window on a key opens no interior gap; and an uncovered key has no
    // sibling interval to overlap. Every neighbouring guard is silent, so a refusal
    // here could only be the pin.
    let third = seed_price(&h, plan_id, "CA").await;
    h.governance
        .windows
        .schedule(
            &rest_support::security_context(rest_support::SEED_ACTOR, h.tenant),
            &h.scope(),
            h.tenant,
            third.price_id,
            Uuid::now_v7(),
            far_future(),
            None,
            "priceIncrease".to_owned(),
            bss_pricing::api::rest::windows::verdict_json,
            rest_support::seed_stamp(),
        )
        .await
        .expect("a free key's first window is nobody's business but the schedule's");
}

/// An instant no wall clock reaches and no fixture interval contains.
///
/// A **fact**, not `OffsetDateTime::now_utc() + something`: `inst-ws-future-start` requires a
/// strictly future start, and a fixture that computes one off the clock asserts
/// something different every day it runs. 2099 is where this crate's window scale
/// already lives (`common::COVERAGE_FROM_UTC`), and this sits after it so a schedule
/// dated here cannot collide with a seeded interval.
fn far_future() -> OffsetDateTime {
    utc_ymd_hms(2099, 12, 1, 0, 0, 0)
}

/// Every window of the plan, as `(window_id, state, from, to)` — the plane a
/// refusal must leave alone.
async fn window_plane(h: &Harness, plan_id: Uuid) -> Vec<(Uuid, String, String)> {
    let conn = h.db.conn().expect("conn");
    let mut rows: Vec<(Uuid, String, String)> =
        window_repo::list_for_plan(&conn, &h.scope(), h.tenant, PlanId::new(plan_id))
            .await
            .expect("read the window plane")
            .into_iter()
            .map(|w| {
                (
                    w.window_id,
                    w.state.as_str().to_owned(),
                    format!("{}..{:?}", w.effective_from, w.effective_to),
                )
            })
            .collect();
    rows.sort();
    rows
}

// ---------------------------------------------------------------------------
// D-99, as §9 states it: the ref row, the invisibility, and the frozen pin
// ---------------------------------------------------------------------------

/// The window scale, fixed after `common::COVERAGE_TO_UTC` so a scheduled interval
/// is adjacent to the fixture's and overlaps nothing.
fn window_at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, 1, 0, 0, 0) + time::Duration::days(day)
}

/// The warm sweep, over the harness's own provider and its scripted registry.
///
/// The **real** projector rather than a hand-written delta: what D-99 owes is that a
/// window mutation's pending ref carries the plan subject *to the projector*, and a
/// test that wrote the delta itself would assert about its own arithmetic.
fn warm_job(h: &Harness) -> ReadModelWarmJob {
    ReadModelWarmJob::new(
        h.db.clone(),
        Arc::clone(&h.registry) as Arc<dyn bss_pricing::domain::ports::CatalogVersionRegistryV1>,
        JobsConfig::default(),
    )
}

/// Every delta the tenant holds, oldest version first.
async fn deltas(h: &Harness) -> Vec<bss_pricing::infra::storage::entity::read_model::Model> {
    use bss_pricing::infra::storage::entity::read_model;
    use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
    use toolkit_db::secure::{AccessScope, SecureEntityExt};
    let conn = h.db.conn().expect("conn");
    read_model::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(read_model::Column::TenantId.eq(h.tenant)))
        .order_by(read_model::Column::CatalogVersion, Order::Asc)
        .all(&conn)
        .await
        .expect("read the read model")
}

/// The key plane one frozen delta reports, as the consumer pinned to it resolves.
///
/// Read out of the **stored payload** and never re-derived from the truth side, since
/// the whole property is that the two disagree after a mutation. D-99's shape is what
/// makes that possible at all: the delta carries intervals and states rather than a
/// point-in-time answer, so a test can ask a frozen version both what it covers *at an
/// instant* and where its coverage *ends*.
fn frozen_key(payload: &serde_json::Value) -> KeyWindows {
    let windows = payload
        .get("windows")
        .and_then(|w| w.as_array())
        .unwrap_or_else(|| panic!("a plan delta carries its window plane: {payload}"));
    let key = windows
        .first()
        .unwrap_or_else(|| panic!("one key, one entry: {payload}"));
    let intervals: Vec<WindowInterval> = key
        .get("intervals")
        .and_then(|i| i.as_array())
        .unwrap_or_else(|| panic!("a key entry carries its intervals: {key}"))
        .iter()
        .map(|interval| {
            let from = interval
                .get("effectiveFrom")
                .and_then(|v| v.as_str())
                .map(|s| bss_pricing::domain::instant::parse_rfc3339(s).expect("an instant"))
                .expect("every interval has a start");
            let to = interval
                .get("effectiveTo")
                .and_then(|v| v.as_str())
                .map(|s| bss_pricing::domain::instant::parse_rfc3339(s).expect("an instant"));
            let state = interval
                .get("state")
                .and_then(|v| v.as_str())
                .and_then(WindowState::from_token)
                .expect("every interval has a state");
            WindowInterval::new(from, to, state)
        })
        .collect();
    KeyWindows {
        // The key's identity is not what any case here asserts — the delta carries one
        // key and the assertions are about its intervals — so it is filled in rather
        // than reconstructed from ten axes the payload spells its own way.
        scope_key: rest_support::publishable_scope_key(
            PlanId::new(Uuid::now_v7()),
            rest_support::seeded_phase(),
            "eu",
        ),
        intervals,
    }
}

/// Assert the pending-ref row a window mutation on `plan_id` recorded.
///
/// **The row, not the response's handle.** `record_pending` is what moves the plan
/// subject onto the projector's input, and the registry answers the handle *before*
/// that write — so an assertion on the body is satisfied by a mutation that recorded
/// nothing. Every column D-157 and D-165 put on the row is checked, because the
/// projector reads all four: the subject it re-freezes, the revision it freezes, the
/// lifecycle state that revision was in, and the two commit columns that must still be
/// NULL while the handle is pending.
async fn assert_pending_ref(h: &Harness, plan_id: Uuid, revision: u64, handle: &str) {
    let rows = rest_support::pending_version_refs(h).await;
    let row = rows
        .iter()
        .find(|row| row.pending_ref == handle)
        .unwrap_or_else(|| {
            panic!("the mutation recorded no ref for {handle}: {rows:?}");
        });
    assert_eq!(
        row.subject_kind, "plan",
        "windows are plan facts (D-99), so the subject is the plan and not a window"
    );
    assert_eq!(row.subject_ref, plan_id.to_string());
    assert_eq!(
        row.subject_revision,
        Some(i64::try_from(revision).expect("a small revision")),
        "the projector freezes the revision this mutation validated (D-165), not \
         whichever is current when the sweep arrives"
    );
    assert_eq!(
        row.subject_lifecycle_state.as_deref(),
        Some(LifecycleState::Published.as_str()),
        "and the state that revision was in, off the same record"
    );
    assert_eq!(
        row.catalog_version, None,
        "the handle is pending: a version here would be one the registry never assigned"
    );
    assert_eq!(row.committed_at, None, "and nothing has committed it");
}

/// A published plan with one billable key, its fixture window, and nothing else —
/// **plus an approved threshold policy on the key's currency**.
///
/// The policy is part of the fixture because a schedule and a lengthening consult the
/// evaluator now: without an entry for `EUR`, `inst-mat-failsafe` answers, the mutation
/// writes nothing and every case below would be asserting about a refusal. The bar is
/// far above anything these cases move, and what actually matters is that the currency
/// *has* an entry — a window mutation moves no money, so its delta is zero on every
/// row.
async fn published(h: &Harness, plan_id: Uuid) -> Publishable {
    let seeded = seed_publishable_plan(h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    rest_support::approve_threshold_policy(h, &[("EUR", 100_000)]).await;
    seeded
}

/// Schedule a window through the service, as the route does.
async fn schedule(
    h: &Harness,
    price_id: Uuid,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> Result<bss_pricing::infra::window::WindowMutationReceipt, DomainError> {
    let outcome = h
        .governance
        .windows
        .schedule(
            &rest_support::security_context(rest_support::SEED_ACTOR, h.tenant),
            &h.scope(),
            h.tenant,
            price_id,
            Uuid::now_v7(),
            from,
            to,
            "priceIncrease".to_owned(),
            bss_pricing::api::rest::windows::verdict_json,
            rest_support::seed_stamp(),
        )
        .await?;
    match outcome {
        WindowMutationOutcome::Committed(receipt) => Ok(*receipt),
        // Every caller of this helper stages an approved threshold policy first, so a
        // schedule of a zero-delta change set is below every bar and commits. The arm
        // is a fixture assertion rather than a service claim: a schedule *can*
        // legitimately answer here (`inst-mat-failsafe`), and a suite that silently
        // treated the two arms alike would read a refusal as a mutation.
        WindowMutationOutcome::SubmittedForApproval(pending) => panic!(
            "the fixture's threshold policy should make a zero-delta schedule commit; window {} \
             opened a unit instead, with {:?}",
            pending.window_id,
            pending.verdict.reason()
        ),
    }
}

/// D-99: scheduling a window on a published plan answers 202 with a PENDING
/// version ref and the plan-subject delta re-projects. The sellability surface
/// reports the new coverage end only at the next pin-eligible version, never
/// before.
#[tokio::test]
async fn a_schedule_is_a_publish_unit_and_is_invisible_until_the_version_is_pin_eligible() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    // A first pin-eligible version, so "invisible" is a statement about a world a
    // consumer can already read rather than about an empty store.
    let first = schedule(&h, seeded.price_id, window_at(0), Some(window_at(10)))
        .await
        .expect("the adjacent window schedules");
    assert_pending_ref(&h, plan_id, seeded.revision, &first.pending_version_ref).await;
    h.registry.commit(&first.pending_version_ref, 1);
    warm_job(&h).run(rest_support::at(12)).await.expect("sweep");
    let before = deltas(&h).await;
    assert_eq!(before.len(), 1, "one frozen version so far");
    assert_eq!(before[0].catalog_version, 1);
    assert_eq!(
        frozen_key(&before[0].payload).coverage_end(),
        CoverageEnd::Ends(window_at(10)),
        "the pin a consumer holds reports coverage to the first window's end"
    );

    // The mutation under test: a second, adjacent window extending the coverage.
    let second = schedule(&h, seeded.price_id, window_at(10), None)
        .await
        .expect("the successor schedules");
    assert_pending_ref(&h, plan_id, seeded.revision, &second.pending_version_ref).await;

    // **Invisible.** The truth side moved and no pin-eligible version says so.
    let during = deltas(&h).await;
    assert_eq!(
        during.len(),
        1,
        "no new delta before the version commits and warms: {during:?}"
    );
    assert_eq!(
        frozen_key(&during[0].payload).coverage_end(),
        CoverageEnd::Ends(window_at(10)),
        "and the version a consumer may pin still answers the old coverage end"
    );

    // Then the registry batches, the sweep runs, and only now is it visible.
    h.registry.commit(&second.pending_version_ref, 2);
    warm_job(&h).run(rest_support::at(13)).await.expect("sweep");
    let after = deltas(&h).await;
    assert_eq!(after.len(), 2, "the second version froze its own delta");
    assert_eq!(after[1].catalog_version, 2);
    assert_eq!(
        frozen_key(&after[1].payload).coverage_end(),
        CoverageEnd::OpenEnded,
        "the newest pin-eligible version reports the extended coverage"
    );
}

/// A consumer pinned to the pre-cancel version still reports the old coverage -
/// frozen versions never mutate - while the newest pin-eligible version reports
/// it gone.
///
/// **The staging is the only cancel the rule set admits, and that is the finding worth
/// keeping.** Cancelling the *open-ended successor* is refused
/// `WINDOW_TRAILING_VOID` — D-182's floor, fail-closed with no exemption — so what is
/// cancellable is the **earlier** window of a key a later open-ended one keeps covered.
/// The key's coverage *end* therefore does not move, and the property is asserted where
/// it does move: what the two versions disagree about is whether the key is covered **at
/// an instant inside the cancelled window**. That is D-99's own shape doing the work —
/// intervals and states rather than a boolean — and it is why the assertion can be
/// written against a frozen payload at all.
#[tokio::test]
async fn a_cancel_re_projects_and_the_old_pin_still_answers_the_old_coverage() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let fixture = common::coverage_window_id(seeded.price_id);

    // The world before: the fixture's bounded window, plus an adjacent open-ended
    // successor, frozen at V1. An instant inside the fixture's interval is covered.
    let successor = schedule(&h, seeded.price_id, window_at(0), None)
        .await
        .expect("the adjacent successor schedules");
    h.registry.commit(&successor.pending_version_ref, 1);
    warm_job(&h).run(rest_support::at(12)).await.expect("sweep");
    let before = deltas(&h).await;
    assert_eq!(before.len(), 1, "one frozen version so far");
    let inside_the_fixture = common::coverage_from() + time::Duration::days(1);
    assert!(
        frozen_key(&before[0].payload).covers_at(inside_the_fixture),
        "V1 froze a key covered at that instant: {:?}",
        before[0].payload
    );

    // The cancel of the **earlier** window. D-62 controls it, so it takes a unit and
    // an independent approve before it commits.
    let cancel = cancel_under_approval(&h, fixture).await;
    assert_pending_ref(&h, plan_id, seeded.revision, &cancel.pending_version_ref).await;
    h.registry.commit(&cancel.pending_version_ref, 2);
    warm_job(&h).run(rest_support::at(13)).await.expect("sweep");

    let after = deltas(&h).await;
    assert_eq!(after.len(), 2, "the cancel re-projected the plan subject");

    // **The frozen version never moved.**
    assert_eq!(after[0].catalog_version, 1);
    assert!(
        frozen_key(&after[0].payload).covers_at(inside_the_fixture),
        "a consumer pinned to V1 still resolves the coverage V1 froze: {:?}",
        after[0].payload
    );

    // And the newest one says it is gone.
    assert_eq!(after[1].catalog_version, 2);
    assert!(
        !frozen_key(&after[1].payload).covers_at(inside_the_fixture),
        "while the newest pin-eligible version reports the cancelled window's coverage \
         gone: {:?}",
        after[1].payload
    );
    // What did **not** change is asserted too, because it is what makes the cancel
    // legal at all: the successor still runs forever, so the floor was satisfied.
    assert_eq!(
        frozen_key(&after[1].payload).coverage_end(),
        CoverageEnd::OpenEnded,
        "the key is still covered forward - the cancel removed history, not the horizon"
    );
}

/// `inst-fg-when`: the coverage check runs inside the mutation, not only at
/// publish. A schedule that opens an interior gap is refused.
#[tokio::test]
async fn a_schedule_that_opens_a_gap_is_refused_inside_the_mutation() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let before_refs = rest_support::pending_version_refs(&h).await.len();

    // Ten days after the fixture window's end, so the interval set the mutation
    // produces holds a hole nothing covers. The publish rules would catch this too —
    // the point of `inst-fg-when` is that they are not the only ones that do.
    let refused = schedule(&h, seeded.price_id, window_at(10), None)
        .await
        .expect_err("the gap opens inside the mutation and is refused there");

    match refused {
        DomainError::ValidationFailed(report) => {
            let codes: Vec<&str> = report
                .violations
                .iter()
                .map(|violation| violation.code.as_str())
                .collect();
            assert_eq!(
                codes,
                vec!["WINDOW_GAP"],
                "one violation, and it is the interior check's"
            );
        }
        other => panic!("expected a validation refusal naming WINDOW_GAP, got {other:?}"),
    }

    // And the refusal wrote nothing: no ref, so no version the projector would carry.
    assert_eq!(
        rest_support::pending_version_refs(&h).await.len(),
        before_refs,
        "a refusal is not a mutation that rolled back visibly"
    );
    assert_eq!(
        window_repo::list_for_plan(
            &h.db.conn().expect("conn"),
            &h.scope(),
            h.tenant,
            PlanId::new(plan_id)
        )
        .await
        .expect("read the plane")
        .len(),
        1,
        "the plane still holds the fixture's window and nothing else"
    );
}

/// Cancel `window_id` through the whole D-62 sequence, and hand back the receipt.
///
/// The control is the subject of `tests/rest_windows.rs`; here it is staging, so this
/// asserts only what the cases above depend on — that the first call changed nothing
/// and the second one committed.
async fn cancel_under_approval(
    h: &Harness,
    window_id: Uuid,
) -> bss_pricing::infra::window::WindowMutationReceipt {
    let ctx = rest_support::security_context(rest_support::SEED_ACTOR, h.tenant);
    let opened = h
        .governance
        .windows
        .cancel(
            &ctx,
            &h.scope(),
            h.tenant,
            window_id,
            bss_pricing::api::rest::windows::verdict_json,
            rest_support::stamp_of(SUBMITTER, rest_support::at(12)),
        )
        .await
        .expect("the controlled arm answers rather than refusing");
    // **The unit is opened by the service, inside the transaction that refused the
    // act** (D-191). It used to be a second transaction driven by the route, which is
    // why this helper used to open one itself; the record now travels on the refused
    // arm, so a service-level caller reads the unit rather than minting it. What is
    // still true is the sequence: this call changed nothing, and the act commits only
    // on the call that follows an independent approve.
    let unit = match opened {
        WindowMutationOutcome::SubmittedForApproval(pending) => pending.approval.approval_id,
        WindowMutationOutcome::Committed(_) => {
            panic!("a cancel is always-material (D-62) and must not commit on one principal")
        }
    };
    h.governance
        .approvals
        .decide(
            &h.scope(),
            h.tenant,
            DecideRequest {
                approval_id: unit,
                decision: DecisionBy::Approve(APPROVER),
                reason: None,
                approver_regions: RegionGrant::Untransported,
                stamp: rest_support::stamp_of(APPROVER, rest_support::at(12)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("an independent principal approves the unit");
    match h
        .governance
        .windows
        .cancel(
            &ctx,
            &h.scope(),
            h.tenant,
            window_id,
            bss_pricing::api::rest::windows::verdict_json,
            rest_support::stamp_of(SUBMITTER, rest_support::at(12)),
        )
        .await
        .expect("the call after the approve commits")
    {
        WindowMutationOutcome::Committed(receipt) => *receipt,
        WindowMutationOutcome::SubmittedForApproval(_) => {
            panic!("the approved unit must authorize the cancel rather than opening a second")
        }
    }
}

/// The two principals D-62's control needs. `inst-tp-distinct` is identity and not
/// role, so a sequence driven by one of them could never reach the commit.
const SUBMITTER: Uuid = Uuid::from_u128(0x5_c9);
const APPROVER: Uuid = Uuid::from_u128(0xa_c9);

// ---------------------------------------------------------------------------
// A DIVERGENCE, reproduced: a cancel can move a published plan's dates outside
// its own coverage, and nothing re-checks
// ---------------------------------------------------------------------------

/// **`inst-wc-availability` is a publish-time rule, and a cancel can break it after
/// the publish.** Reproduced here rather than argued, and reported rather than fixed.
///
/// The world: a published plan whose `available_from` sits inside its first window,
/// plus an adjacent open-ended successor. Cancelling the **first** window is legal —
/// the interval set stays contiguous, the coverage end stays open-ended, so neither
/// `inst-fg-detect` nor `inst-fg-trailing` has anything to say — and it moves the key's
/// coverage *start* forward past `available_from`. The plan is then purchasable, by its
/// own dates, over an interval no window covers: exactly what `inst-wc-availability`
/// refuses at publish. `AvailabilityInsideCoverage` is registered in the **publish**
/// pipeline only (`domain::publish::rules`), and a window mutation runs the two
/// Future-Gap rules `inst-fg-when` names, so nothing re-checks it.
///
/// **Why it is reported and not fixed.** The exclusion is textual and defensible:
/// `inst-fg-when` is step 2 of §3's *Future-Gap Detection* and is written about that
/// section's two rules, so adding a third inside the mutation would be implementing a
/// rule the instruction does not put there — and `inst-wc-required` shows why the
/// publish-time set cannot simply be run here wholesale (it would refuse the very
/// mutation that fixes it).
///
/// **And the argument the code carried was unfinished, which is the half this test
/// closes.** `api::rest::windows` says "the publish report is its surface", and D-128
/// makes that false for a plan that never publishes again — a retired plan has no later
/// publish, and a published plan nobody revises gets no report. So the finding would
/// have no reader. What *does* answer, and is asserted below, is the **sellability
/// gate**: predicate (1) evaluates each bound key's coverage at the caller's instant, so
/// over the uncovered interval the plan-market is `not_sellable` whatever its dates say.
/// The residue is therefore an operator-visible incoherence and **not** a sales hazard:
/// nothing can be sold into the hole, and no surface names the hole to the operator
/// unless the plan publishes again.
#[tokio::test]
async fn a_cancel_can_move_a_published_plan_outside_its_own_coverage() {
    use bss_pricing::domain::coverage::window_coverage_rules;
    use bss_pricing::domain::sellability::{PlanMarketVerdict, SellabilitySurface};
    use bss_pricing::infra::storage::entity::plan;
    use bss_pricing::infra::storage::repo::read_model_repo;
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, Condition, EntityTrait};
    use toolkit_db::secure::SecureUpdateExt;

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    let inside = common::coverage_from() + time::Duration::days(1);

    // `available_from` inside the fixture window. Written through the store because no
    // route sets it on a published plan, and the dates are the premise rather than the
    // subject.
    let conn = h.db.conn().expect("conn");
    plan::Entity::update_many()
        .secure()
        .scope_with(&h.scope())
        .col_expr(plan::Column::AvailableFrom, Expr::value(inside))
        .filter(Condition::all().add(plan::Column::PlanId.eq(plan_id)))
        .exec(&conn)
        .await
        .expect("set available_from inside the coverage span");
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    // The schedule below is not a registered trigger, so what decides it is the
    // per-currency threshold; without an entry it would open a unit and write nothing.
    rest_support::approve_threshold_policy(&h, &[("EUR", 100_000)]).await;

    // The adjacent open-ended successor, so cancelling the first window is legal.
    let successor = schedule(&h, seeded.price_id, window_at(0), None)
        .await
        .expect("the successor schedules");

    // The cancel, through D-62's own sequence.
    let fixture = common::coverage_window_id(seeded.price_id);
    let cancel = cancel_under_approval(&h, fixture).await;

    // **The reproduction.** The subject the approval plane re-derives *is* the shape a
    // publish would assemble — `ApprovalService::find` runs the same `assemble_from`
    // the publish path and the mutation both use — so running the publish-time coverage
    // rules over it is running them over the world the cancel left behind.
    let unit = rest_support::approval_rows(&h)
        .await
        .into_iter()
        .find(|row| {
            // Prefix, because the subject names the **act** after the window:
            // `<plan>/<window>/<op>/<from>/<to>`. This case wants the one unit
            // that is about this window, whichever act opened it.
            row.subject_ref
                .starts_with(&bss_pricing::infra::storage::repo::audit_repo::window_ref(
                    PlanId::new(plan_id),
                    fixture,
                ))
        })
        .expect("the cancel's unit")
        .approval_id;
    let detail = h
        .governance
        .approvals
        .find(&h.scope(), h.tenant, unit, rest_support::at(13))
        .await
        .expect("read the unit")
        .expect("it is there");
    let subject = detail.subject.expect("a window unit re-derives its plan");
    let shape = subject
        .plan()
        .expect("a window unit's pinned subject is a plan, never a threshold policy");
    // `false`: this suite is about the rule, not about the publish's D-332
    // exemption, which has its own case in `domain::coverage_tests`.
    let codes: Vec<String> = window_coverage_rules(false)
        .run(shape)
        .violations
        .iter()
        .map(|violation| violation.code.clone())
        .collect();
    assert!(
        codes.contains(&"AVAILABILITY_OUTSIDE_COVERAGE".to_owned()),
        "the cancel left the plan purchasable outside its coverage, and the publish-time \
         rule is the only thing that says so: {codes:?}"
    );

    // **The half that finishes the argument**: nothing can be sold into the hole. The
    // cancel is projected and pinned, and the gate is asked at the uncovered instant.
    h.registry.commit(&successor.pending_version_ref, 1);
    h.registry.commit(&cancel.pending_version_ref, 2);
    warm_job(&h).run(rest_support::at(14)).await.expect("sweep");
    let newest = deltas(&h).await.pop().expect("the cancel's own delta");
    // Resolved the way a consumer resolves it — at a pinned version, through the
    // repository — rather than by handing the raw payload to the parser.
    let stored = read_model_repo::delta_at(
        &h.db.conn().expect("conn"),
        &h.scope(),
        h.tenant,
        &bss_pricing::domain::read_model::SubjectRef::Plan(plan_id),
        bss_pricing_sdk::CatalogVersion::new(
            u64::try_from(newest.catalog_version).expect("a small version"),
        ),
    )
    .await
    .expect("read the delta")
    .expect("the version carries this plan");
    let facts = read_model_repo::sellability_facts(&stored).expect("a payload this gear wrote");
    let surface = SellabilitySurface::of_delta(
        &facts,
        inside,
        &bss_pricing::domain::money::CurrencyCode::new("EUR").expect("three letters"),
        &bss_pricing::domain::scope_key::Region::new("eu").expect("a region"),
    );
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable,
        "the gate refuses at the uncovered instant, whatever the plan's dates say"
    );
}

// ---------------------------------------------------------------------------
// D-188 at the window path: the policy a mutation reads is the one in force at
// **the act's own instant**, and not the one in force when the read happens
// ---------------------------------------------------------------------------

/// The instant the policy the two cases below stage starts being in force.
///
/// **Fixed and in the past, which is the opposite of this suite's other date
/// discipline, and deliberately so.** [`window_at`] is dated 2099 because those cases
/// need "this interval is in the future" to stay a fact as the wall clock advances.
/// These two need the *reverse* fact — "the wall clock is already past the policy's
/// start, while the act's stamp is not" — and that is a fact for exactly the same
/// reason: `2026-08-04T00:00:00Z` and `rest_support::at(12)`'s `2026-08-03T12:00Z` are
/// both in the past, the clock only moves further from them, and it moves away from
/// both in the same direction. So the asymmetry cannot age out either.
///
/// A reader who applies the 2099 rule here mechanically deletes the case's whole
/// premise: move this to 2099 and the policy is in force at neither reading of "now",
/// both arms answer the fail-safe, and the test passes against the defect it exists to
/// catch.
const POLICY_START_AFTER_THE_ACT: &str = "2026-08-04T00:00:00Z";

/// An instant **after** [`POLICY_START_AFTER_THE_ACT`], for the control.
///
/// `rest_support::at` only ranges over hours of 2026-08-03, so the one stamp that has
/// to sit on the far side of the policy's start is spelled here.
fn after_the_policy_starts() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 4, 12, 0, 0)
}

/// A published plan with one billable key and its fixture window, plus an approved
/// threshold policy on `EUR` whose start is `from`.
///
/// [`published`] with the start opened up: its policy begins in 2020, which is before
/// every reading of "now" any suite can produce, so it cannot separate the two
/// readings.
async fn published_with_policy_from(h: &Harness, plan_id: Uuid, from: &str) -> Publishable {
    let seeded = seed_publishable_plan(h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    rest_support::approve_threshold_policy_from(h, from, &[("EUR", 100_000)]).await;
    seeded
}

/// Schedule through the service under a **caller-supplied stamp**, and hand back the
/// raw outcome.
///
/// [`schedule`] cannot serve the two cases below for two reasons, and both are the
/// point: it stamps with `rest_support::seed_stamp()`, whose `recorded_at` *is*
/// `OffsetDateTime::now_utc()` — the very reading these cases have to hold apart from the act's
/// instant — and it collapses the two outcome arms with a `panic!`, where here which
/// arm answers is the assertion.
async fn schedule_stamped(
    h: &Harness,
    price_id: Uuid,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
    stamp: bss_pricing::domain::audit::AuditStamp,
) -> Result<WindowMutationOutcome, DomainError> {
    h.governance
        .windows
        .schedule(
            &rest_support::security_context(rest_support::SEED_ACTOR, h.tenant),
            &h.scope(),
            h.tenant,
            price_id,
            Uuid::now_v7(),
            from,
            to,
            "priceIncrease".to_owned(),
            bss_pricing::api::rest::windows::verdict_json,
            stamp,
        )
        .await
}

/// **A window act must not straddle two readings of "now".**
///
/// `infra::threshold`'s own module doc states the rule: *"A caller that already holds
/// the instant its act is about … should call `effective_policy_at`, so that one act
/// does not straddle two readings of 'now'."* `infra::window::mutate_in` holds that
/// instant — `let now = stamp.recorded_at;`, used for the plan-context read and for the
/// trailing-void floor — and called the `OffsetDateTime::now_utc()` wrapper anyway. So a schedule's
/// **verdict** was decided against the policy in force at the wall clock while its
/// **stamp** said the act happened somewhere else, and under D-188 those two instants
/// can sit on opposite sides of a policy's `effective_from`.
///
/// The staging puts them there: the policy starts
/// [`POLICY_START_AFTER_THE_ACT`] and the act is stamped `at(12)`, twelve hours
/// earlier. At the act's own instant the tenant has **no** policy in force, so
/// `inst-mat-failsafe` answers — material, a unit, and nothing written. Against the
/// unfixed code the wall clock finds the policy in force, `EUR` has an entry, a
/// zero-delta change set is below its bar, and the schedule commits on one principal.
///
/// # Scope, stated so nobody widens it
///
/// This is **not reachable through the routes**: `api::rest::windows` builds its stamp
/// from `OffsetDateTime::now_utc()`, so there the stamp *is* the wall clock and the two readings agree
/// to within the call. It is reachable for an in-process caller holding a fixed stamp —
/// which every service-level suite is, and which a scheduled or replayed act would be.
/// That is why the case is here and not in `tests/rest_windows.rs`.
///
/// The bar is deliberately far above anything the act moves (a window mutation moves an
/// interval and no money, so every row's delta is zero), which leaves *whether the
/// policy is in force at all* as the only thing that can decide the act — and
/// [`a_schedule_stamped_after_the_policy_start_commits`] is the control that turns that
/// from an assumption into evidence.
#[tokio::test]
async fn a_schedule_reads_the_policy_in_force_at_its_own_stamp_not_at_the_wall_clock() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published_with_policy_from(&h, plan_id, POLICY_START_AFTER_THE_ACT).await;
    let before_refs = rest_support::pending_version_refs(&h).await.len();

    let outcome = schedule_stamped(
        &h,
        seeded.price_id,
        window_at(0),
        None,
        rest_support::stamp_of(SUBMITTER, rest_support::at(12)),
    )
    .await
    .expect("the act answers rather than refusing - the question is which arm");

    match outcome {
        WindowMutationOutcome::SubmittedForApproval(pending) => {
            assert_eq!(
                pending.verdict.reason(),
                Some(bss_pricing::domain::materiality::MaterialityReason::NoConfiguredThreshold),
                "at 2026-08-03T12:00Z the tenant has no policy in force, so the fail-safe is \
                 what decides the act"
            );
        }
        WindowMutationOutcome::Committed(receipt) => panic!(
            "the schedule committed on one principal against a policy that had not started at \
             its own stamp: window {} froze {} - the verdict was read on the wall clock while \
             the record says 2026-08-03T12:00Z",
            receipt.window_id, receipt.pending_version_ref
        ),
    }

    // And the refused arm wrote nothing, on both planes a commit would have touched.
    assert_eq!(
        rest_support::pending_version_refs(&h).await.len(),
        before_refs,
        "a materially-refused schedule records no pending ref"
    );
    assert_eq!(
        window_repo::list_for_plan(
            &h.db.conn().expect("conn"),
            &h.scope(),
            h.tenant,
            PlanId::new(plan_id)
        )
        .await
        .expect("read the plane")
        .len(),
        1,
        "and no window: the plane still holds the fixture's and nothing else"
    );
}

/// **The control that makes the case above about the clock rather than about the
/// bar.**
///
/// Same plan, same policy, same `EUR` entry, same zero-delta schedule — the only thing
/// that moves is the act's stamp, from twelve hours *before*
/// [`POLICY_START_AFTER_THE_ACT`] to twelve hours *after* it. Now the policy is in
/// force at the act's own instant, the currency has an entry, and the schedule commits
/// on one principal.
///
/// Without this, the first case is satisfied by any code that refuses every schedule —
/// a bar the act could never clear, an entry on the wrong currency, a policy the
/// evaluator never sees — and none of those has anything to do with the clock.
#[tokio::test]
async fn a_schedule_stamped_after_the_policy_start_commits() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published_with_policy_from(&h, plan_id, POLICY_START_AFTER_THE_ACT).await;

    let outcome = schedule_stamped(
        &h,
        seeded.price_id,
        window_at(0),
        None,
        rest_support::stamp_of(SUBMITTER, after_the_policy_starts()),
    )
    .await
    .expect("the act answers rather than refusing");

    match outcome {
        WindowMutationOutcome::Committed(receipt) => {
            assert_pending_ref(&h, plan_id, seeded.revision, &receipt.pending_version_ref).await;
        }
        WindowMutationOutcome::SubmittedForApproval(pending) => panic!(
            "the policy is in force at 2026-08-04T12:00Z and `EUR` has an entry, so a zero-delta \
             schedule is below its bar; a unit here would mean the case above is about the bar \
             and not about the clock: {:?}",
            pending.verdict.reason()
        ),
    }
}

// ---------------------------------------------------------------------------
// D-314: the subject is a revision the catalog has already frozen
// ---------------------------------------------------------------------------

/// **A window cannot be scheduled on a plan that has never published, and the two
/// enforcements that say so are asserted together.**
///
/// The file's own premise banner says every plan here is published because the window
/// paths resolve the plan's *current* revision. This is the one case about the other
/// side of that premise, and it exists because the refusal reads like an accident:
/// `plan_repo::load_current` is `published | retired`, so a reader can conclude the 404
/// is a side effect of which repository function this path happened to reuse, and
/// delete it.
///
/// D-314 records that it is not, and the two assertions here guard the two **separate**
/// enforcements — which is the point, because either can be relaxed without the other:
///
/// 1. the surface refuses, naming the current plan revision as what is absent. This arm
///    is what reddens if the domain check is lifted: the act then runs to step 6 and
///    aborts at the write, so `schedule` answers `Internal` and the `match` below takes
///    its panicking arm. A domain relaxation cannot pass this case quietly, and it
///    converts a truthful 404 into a 500 rather than authoring anything.
/// 2. the ref row such a mutation would record at step 6 is refused **by the store** —
///    `chk_pricing_catalog_version_ref_subject_lifecycle` admits no `draft`. This arm
///    guards the *other* direction, and nothing else in the crate's fast tier does: a
///    later migration that widens or drops that `CHECK` — reasoning that `superseded`
///    is the token it was written for, which it is and which its Postgres twin now
///    spells out is only half of what it keeps out — leaves assertion (1) perfectly
///    green while removing the ground D-314's second half rests on.
///
/// The second assertion is written against `record_pending`, which is the call
/// `mutate_in` makes, with the two arguments the relaxed path would supply: the draft's
/// own revision number and its own lifecycle state, both taken off the record the way
/// `read_plan_context` takes them. It asserts the **constraint by name**, its Postgres
/// twin's discipline: `is_err()` alone is satisfied by any storage failure, so a chain
/// that dropped this `CHECK` and refused the insert on some other column would keep the
/// case green while the guarantee was gone.
#[tokio::test]
async fn a_window_needs_a_frozen_revision_and_the_store_says_so_too() {
    use bss_pricing::domain::read_model::SubjectRef;
    use bss_pricing::infra::storage::repo::{PendingVersionRow, catalog_version_ref_repo};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    // Deliberately **not** `published()`: the plan is publishable and unpublished,
    // which is the state under judgement.
    let seeded = seed_publishable_plan(&h, plan_id).await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 100_000)]).await;

    // (1) The surface.
    let refused = schedule(&h, seeded.price_id, window_at(0), None)
        .await
        .expect_err("a plan with no current revision has no window surface");
    match refused {
        DomainError::NotFound { subject, .. } => assert_eq!(
            subject, "current plan revision",
            "the absent thing is named, so the operator is told which act is missing"
        ),
        other => panic!("expected the current-revision refusal, got {other:?}"),
    }
    assert!(
        rest_support::pending_version_refs(&h).await.is_empty(),
        "and the refusal wrote nothing"
    );

    // (2) The store, against the row the relaxed path would have written.
    let draft = {
        let conn = h.db.conn().expect("conn");
        bss_pricing::infra::storage::repo::plan_repo::load_open_draft(
            &conn,
            &h.scope(),
            h.tenant,
            PlanId::new(plan_id),
        )
        .await
        .expect("read the open draft")
        .expect("the seed left one")
    };
    assert_eq!(
        draft.lifecycle_state,
        LifecycleState::Draft,
        "the subject state a relaxed path would carry off the same record"
    );
    let conn = h.db.conn().expect("conn");
    let stored = catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope(),
        PendingVersionRow::for_subject(
            h.tenant,
            "window-mutation/d314".to_owned(),
            &SubjectRef::Plan(plan_id),
            Some(draft.revision),
            Some(draft.lifecycle_state),
            rest_support::at(12),
        ),
    )
    .await;
    let refusal = format!("{stored:?}");
    assert!(
        refusal.contains("chk_pricing_catalog_version_ref_subject_lifecycle"),
        "the schema admits no draft subject state, and it is THIS constraint that says \
         so — an `is_err` alone would be satisfied by any other refusal: {refusal}"
    );

    // The negative control, and it is what makes the assertion above about the
    // **state** rather than about anything else on the row. It reuses the **same
    // handle**, so the two inserts differ in exactly one column: the rejected one wrote
    // nothing, so `(tenant, handle, kind, subject)` is still free, and a control under a
    // second handle could not rule out a refusal keyed on the handle instead.
    let accepted = catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope(),
        PendingVersionRow::for_subject(
            h.tenant,
            "window-mutation/d314".to_owned(),
            &SubjectRef::Plan(plan_id),
            Some(draft.revision),
            Some(LifecycleState::Published),
            rest_support::at(12),
        ),
    )
    .await;
    assert!(
        accepted.is_ok(),
        "one column apart, the same row stores: {accepted:?}"
    );
}
// D-04's `inst-co-bounds` — the grandfathered generation's two clocks.
// ---------------------------------------------------------------------------

/// The horizon the generation below is grandfathered until.
fn horizon() -> OffsetDateTime {
    utc_ymd_hms(2099, 10, 1, 0, 0, 0)
}

/// `horizon() + the longest billing cycle sold on the key`.
///
/// The seeded plan is `monthly`, and W6's margin rounds a month **up** to its
/// calendar maximum — 31 days — because a margin rounded down leaves the tail of
/// a bound period uncovered, which is the hole D-04 exists to close. Spelled as
/// the arithmetic rather than as a date so the case says what the floor *is*.
fn horizon_floor() -> OffsetDateTime {
    horizon() + time::Duration::days(31)
}

/// The instant the generation below was cut over at — its `cohort` axis, and the
/// first instant it can strand anybody.
///
/// **It is `window_at(0)` and not an arbitrary earlier date, because a cutover
/// schedules the copy's window *at* the cutover instant** (`algo-cutover` step 3,
/// D-204 clause 4): a generation's first covered instant is its cohort, by
/// construction. This fixture used to say `window_at(-30)`, which models a
/// generation that existed for thirty days carrying subscribers and holding no
/// window at all — the exact stranding `inst-co-bounds` forbids. The bound as
/// first built could not see it, because it only asked where coverage *ends*.
fn cohort_at() -> OffsetDateTime {
    window_at(0)
}

/// A published plan carrying a **grandfathered generation** with a horizon, on
/// its own cohort key, beside the ordinary published row.
///
/// The generation is a key of its own (ADR-0002's cohort axis), so it starts with
/// no window at all — which is what makes the first schedule on it the reachable
/// shape: nothing has been removed, no interior gap opens, and every neighbouring
/// coverage guard is silent.
async fn published_with_a_generation(h: &Harness, plan_id: Uuid) -> Uuid {
    published_with_a_generation_until(h, plan_id, Some(horizon())).await
}

/// [`published_with_a_generation`] over an explicit horizon, so the **null**
/// horizon — D-147's indefinite generation — gets the same fixture rather than a
/// second one written to look like it.
async fn published_with_a_generation_until(
    h: &Harness,
    plan_id: Uuid,
    grandfather_until: Option<OffsetDateTime>,
) -> Uuid {
    let seeded = rest_support::seed_publishable_plan(h, plan_id).await;
    let generation = rest_support::seed_price_keyed_with_horizon(
        h,
        plan_id,
        "us",
        PriceEligibility::ExistingGrandfathered,
        Cohort::Generation(cohort_at()),
        grandfather_until,
    )
    .await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    h.publish_price(plan_id, generation.price_id).await;
    rest_support::approve_threshold_policy(h, &[("EUR", 100_000), ("USD", 100_000)]).await;
    generation.price_id
}

/// Schedule through the service and hand back the outcome unjudged.
///
/// [`schedule`] panics on the approval arm, which is right for the cases that
/// stage a policy covering their own currency and wrong here: what these cases
/// assert is **refused or not refused**, and a unit being opened is not a
/// refusal.
async fn schedule_outcome(
    h: &Harness,
    price_id: Uuid,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> Result<WindowMutationOutcome, DomainError> {
    h.governance
        .windows
        .schedule(
            &rest_support::security_context(rest_support::SEED_ACTOR, h.tenant),
            &h.scope(),
            h.tenant,
            price_id,
            Uuid::now_v7(),
            from,
            to,
            "priceIncrease".to_owned(),
            bss_pricing::api::rest::windows::verdict_json,
            rest_support::seed_stamp(),
        )
        .await
}

/// **A window that stops inside the generation's bound is refused.**
///
/// D-04 / `inst-co-bounds`: a grandfathered generation carries two clocks — its
/// window and `grandfatherUntil` — and the window MUST cover through
/// `grandfatherUntil` **plus the longest billing cycle sold on the key**, because
/// re-bind happens only at the next renewal after the horizon. Until this case
/// nothing in the crate enforced it: the cutover half holds by construction (the
/// copy's window is scheduled open-ended, D-204 clause 4) and the adjustment half
/// was recorded as an open gap in `window_repo`'s own doc, on the ground that W6's
/// margin "has no producer anywhere in this crate" — which stopped being true when
/// `coverage::longest_cycle_sold` landed for D-80.
///
/// The end asked for here is **past the horizon** and short of the floor, which is
/// the case a bound-less reading would wave through: it looks like full coverage
/// of the grandfathered period and strands every subscriber whose period began
/// before the horizon for up to one full cycle.
#[tokio::test]
async fn a_generations_window_may_not_stop_inside_its_grandfathering_bound() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let price_id = published_with_a_generation(&h, plan_id).await;

    let refusal = schedule_outcome(
        &h,
        price_id,
        window_at(0),
        Some(horizon() + time::Duration::days(14)),
    )
    .await
    .expect_err("a window ending inside the D-04 bound is refused");

    let DomainError::WindowTrailingVoid(detail) = refusal else {
        panic!("expected the coverage floor's refusal, got {refusal:?}");
    };
    assert!(
        detail.contains(&format_rfc3339(horizon()))
            && detail.contains(&format_rfc3339(horizon_floor())),
        "the refusal names the horizon and the floor it has to reach: {detail}"
    );
}

/// **The positive control: a window that reaches the floor is accepted.**
///
/// Without it the refusal above is satisfied by a rule that refused every window
/// on a grandfathered key — which would make the class unusable and would pass
/// the case just as well. The end is the floor **exactly**, so the comparison is
/// pinned at its boundary rather than somewhere comfortably past it: the bound is
/// `>=`, and a rule written with `>` reddens here and nowhere else.
#[tokio::test]
async fn a_generations_window_reaching_the_floor_is_accepted() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let price_id = published_with_a_generation(&h, plan_id).await;

    schedule_outcome(&h, price_id, window_at(0), Some(horizon_floor()))
        .await
        .expect("coverage through the horizon plus one full cycle satisfies D-04");
}

/// **And an open-ended window is accepted**, which is the shape the cutover
/// itself composes.
///
/// D-204 clause (4)'s "holds by construction" is only true if this passes: an
/// open interval covers every instant the margin could name, whatever the horizon
/// and whatever the cycle roster says. A rule that refused it would refuse every
/// cutover the day the copy's window reached this path.
#[tokio::test]
async fn a_generations_open_ended_window_is_accepted() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let price_id = published_with_a_generation(&h, plan_id).await;

    schedule_outcome(&h, price_id, window_at(0), None)
        .await
        .expect("an open interval covers every instant the bound could name");
}

/// **An ordinary key is untouched by the bound**, which is the other control.
///
/// The rule keys on `price_eligibility = existing_grandfathered`, and a bound
/// applied to every key would refuse the ordinary bounded windows this suite is
/// built out of. This case is the same instants on the plan's `all_subscriptions`
/// row, and it commits.
#[tokio::test]
async fn an_ordinary_keys_window_is_not_judged_against_any_horizon() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = rest_support::seed_publishable_plan(&h, plan_id).await;
    let ordinary = rest_support::seed_price(&h, plan_id, "us").await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    h.publish_price(plan_id, ordinary.price_id).await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 100_000), ("USD", 100_000)]).await;

    schedule_outcome(
        &h,
        ordinary.price_id,
        window_at(0),
        Some(horizon() + time::Duration::days(14)),
    )
    .await
    .expect("a non-grandfathered key has no horizon to be bound by");
}

/// **A window that opens *after* the generation's cohort is refused, however far
/// past the floor it runs.**
///
/// The bound as first built asked one question — where does coverage *end* — and
/// `at >= floor` is satisfied by an interval that begins at any instant at all.
/// The three neighbouring guards are each silent on the shape for a stated
/// reason: `interior_gaps` walks each window's exclusive end to its successor's
/// start, so a **leading** void has no predecessor to open it from;
/// `refuse_trailing_void` runs only on the two coverage-removing acts and a
/// schedule is not one; and this rule read the far end.
///
/// So the interval below — opening two months past the horizon and running a year
/// past the floor — was accepted, and every subscriber the generation carries is
/// unrateable from the cutover instant until it opens. That is D-04's stranding
/// verbatim, arrived at from the other side. The bound is therefore asked of the
/// **whole** span the generation is answerable for, `[cohort, grandfatherUntil +
/// the longest billing cycle sold on the key)`, and not of its last instant.
#[tokio::test]
async fn a_generations_window_may_not_open_after_its_cohort() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let price_id = published_with_a_generation(&h, plan_id).await;

    let refusal = schedule_outcome(
        &h,
        price_id,
        horizon() + time::Duration::days(60),
        Some(horizon_floor() + time::Duration::days(365)),
    )
    .await
    .expect_err("a window opening after the cohort strands the generation it belongs to");

    let DomainError::WindowTrailingVoid(detail) = refusal else {
        panic!("expected the coverage floor's refusal, got {refusal:?}");
    };
    assert!(
        detail.contains(&format_rfc3339(cohort_at()))
            && detail.contains(&format_rfc3339(horizon_floor())),
        "the refusal names the first uncovered instant and the floor: {detail}"
    );
}

/// **An indefinite generation's window may not open after its cohort either** —
/// the null-horizon branch had the identical hole.
///
/// D-147 makes a grandfathered row with a null `grandfatherUntil` indefinite, and
/// the branch answering for it accepted anything [`CoverageEnd::OpenEnded`] — a
/// property of one interval's *end*. An open interval starting a month after the
/// cutover is open-ended and covers nothing in between, so the branch waved
/// through the same stranding the bounded one did, over a period that never ends.
#[tokio::test]
async fn an_indefinite_generations_window_may_not_open_after_its_cohort() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let price_id = published_with_a_generation_until(&h, plan_id, None).await;

    let refusal = schedule_outcome(&h, price_id, window_at(30), None)
        .await
        .expect_err("an open-ended window opening a month late still leaves that month uncovered");

    let DomainError::WindowTrailingVoid(detail) = refusal else {
        panic!("expected the indefinite generation's refusal, got {refusal:?}");
    };
    assert!(
        detail.contains(&format_rfc3339(cohort_at())),
        "the refusal names the first uncovered instant: {detail}"
    );
}

/// **The indefinite generation's positive control**: open-ended *from the cohort*
/// commits.
///
/// Without it the case above is satisfied by a rule that refused every window on a
/// null-horizon generation — which is the whole D-147 class, and which no cutover
/// could then ever adjust.
#[tokio::test]
async fn an_indefinite_generations_window_from_its_cohort_is_accepted() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let price_id = published_with_a_generation_until(&h, plan_id, None).await;

    schedule_outcome(&h, price_id, cohort_at(), None)
        .await
        .expect("coverage from the cohort onward, never ending, is what an indefinite row needs");
}

/// **And an indefinite generation may not be bounded at all**, which is the
/// branch's own sentence and had no case on it.
///
/// A horizon that never arrives cannot be reached by any finite end, so "no
/// horizon" must not be read as "a horizon of now". This pins the arm the
/// leading-gap walk now shares with the bounded one, so a later simplification
/// that collapsed the two would have to answer here.
#[tokio::test]
async fn an_indefinite_generations_window_may_not_be_bounded() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let price_id = published_with_a_generation_until(&h, plan_id, None).await;

    let refusal = schedule_outcome(&h, price_id, cohort_at(), Some(window_at(3650)))
        .await
        .expect_err("an indefinite generation's coverage may not be given an end");

    let DomainError::WindowTrailingVoid(detail) = refusal else {
        panic!("expected the indefinite generation's refusal, got {refusal:?}");
    };
    assert!(
        detail.contains(&format_rfc3339(window_at(3650))),
        "the refusal names the instant coverage would stop at: {detail}"
    );
}
