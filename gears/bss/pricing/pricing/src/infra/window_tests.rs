//! Unit tests for `WindowService` — the pure arithmetic the transaction wraps.
//!
//! The service is exercised end to end by `tests/sqlite_window_service.rs` and
//! `tests/rest_windows.rs`, which need a real store. What belongs here is what needs
//! none: the classification D-62's control keys on, and the answer the controlled arm
//! hands back. Both are decided from a `Planned` and nothing else, so a test that
//! stood up a database to reach them would be asserting about the database.
//!
//! **`Op::registered_trigger`'s `Adjust` arm has no other test, and it is why this
//! file exists.** Over HTTP the arm is shadowed: the coverage rules refuse every
//! shorten a caller can compose — the interior check where the shortened window has a
//! successor, D-182's fail-closed floor where it does not — so
//! `tests/rest_windows.rs` can only pin *that*
//! ordering (`a_shortening_patch_is_refused_by_the_coverage_rules_before_it_can_open_a_unit`)
//! and never the classification itself. Here the classification is the whole subject,
//! which is what keeps a shadowed guard from being an unasserted one.

use super::{Op, PlanContext, Planned, WindowState, pending_approval, window_state_value};
use crate::domain::instant::{format_rfc3339, utc_ymd_hms};
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{MaterialityReason, MaterialityVerdict};
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::PlanShape;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::window::KeyWindows;
use crate::infra::storage::repo::approval_repo::ApprovalRecord;
use crate::infra::storage::repo::window_repo::WindowRecord;
use time::OffsetDateTime;

/// The window scale, fixed at 2099 for `common::COVERAGE_FROM_UTC`'s reason: an
/// instant no clock reaches cannot make a case mean something different tomorrow.
fn at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 4, 0, 0, 0) + time::Duration::days(day)
}

fn plan() -> PlanId {
    PlanId::new(uuid::Uuid::from_u128(0x_71a2))
}

fn key() -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a region"),
        PhaseId::new(uuid::Uuid::from_u128(0x_fa5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

/// One stored window, `scheduled`, over `[from, to)`.
fn stored(from: i64, to: Option<i64>) -> WindowRecord {
    WindowRecord {
        window_id: uuid::Uuid::from_u128(0x_d1),
        tenant_id: uuid::Uuid::from_u128(0x_7e),
        price_id: uuid::Uuid::from_u128(0x_c1),
        scope_key: key(),
        effective_from: at(from),
        effective_to: to.map(at),
        state: WindowState::Scheduled,
        reason_code: "fixture".to_owned(),
        created_by: uuid::Uuid::from_u128(0x_ac70),
        created_at: at(-1),
        activated_at: None,
        expired_at: None,
        cancelled_at: None,
        // Freshly scheduled: no operator act beyond the schedule itself.
        mutation_seq: 0,
    }
}

/// A `submitted` window unit, as the refusing transaction opens one.
///
/// The record travels on [`PendingApproval`] since the unit moved inside the mutation
/// (D-191): the gate records the body it is handed, and on the refused arm that body
/// names the approval. Only the fields these cases read are meaningful; the rest are the
/// store's shape.
fn submitted_unit(verdict: &MaterialityVerdict) -> ApprovalRecord {
    ApprovalRecord {
        approval_id: uuid::Uuid::from_u128(0x_a1),
        tenant_id: uuid::Uuid::from_u128(0x_7e),
        subject_ref: "fixture".to_owned(),
        subject_kind: crate::domain::audit::AuditSubjectKind::Window,
        content_hash: Vec::new(),
        state: crate::domain::approval::ApprovalState::Submitted,
        submitter_principal: uuid::Uuid::from_u128(0x_ac70),
        approver_principal: None,
        reason: None,
        materiality: serde_json::json!({ "material": verdict.is_material() }),
        submitted_at: at(-1),
        decided_at: None,
    }
}

/// A `Planned` over `current`, asking for `effective_to`.
fn planned(op: Op, current: Option<WindowRecord>, effective_to: Option<i64>) -> Planned {
    let from = current.as_ref().map_or_else(|| at(0), |c| c.effective_from);
    Planned {
        price_id: uuid::Uuid::from_u128(0x_c1),
        effective_from: from,
        effective_to: effective_to.map(at),
        reason_code: "fixture".to_owned(),
        op,
        plan: PlanContext {
            plan_id: plan(),
            revision: 3,
            lifecycle_state: LifecycleState::Published,
            current,
            key: key(),
            before: KeyWindows {
                scope_key: key(),
                intervals: Vec::new(),
            },
            margin: Some(time::Duration::days(31)),
            plane: Vec::new(),
            published: Vec::new(),
            shape: PlanShape::new(plan(), 3, at(0)),
        },
    }
}

// ---------------------------------------------------------------------------
// D-62's classification
// ---------------------------------------------------------------------------

/// A **cancel** is always-material whatever the interval looks like.
///
/// Unconditional in `inst-mat-registered` and unconditional here: the three shapes
/// below differ in every way an interval can (bounded, open-ended, far future) and the
/// answer does not move, which is what "whatever a threshold says" means for the one
/// act that never consults one.
#[test]
fn a_cancel_is_always_material() {
    for stored_end in [None, Some(30), Some(3650)] {
        let planned = planned(Op::Cancel, Some(stored(0, stored_end)), stored_end);
        assert_eq!(
            planned.op.registered_trigger(&planned),
            Some(Trigger::WindowCancellation),
            "a cancel of [{}, {stored_end:?}) is a registered trigger, and names which",
            0
        );
    }
}

/// A **schedule** names no trigger, and that is D-62's split rather than an omission.
///
/// It removes no coverage, so it is not on the trigger list. **`None` is not "not
/// material"**, and the doc here used to read as though it were — it said the answer
/// is `false` by rule and that a threshold policy was G6's obligation. The policy
/// landed: what `None` now means is *"no registered trigger, so ask the threshold"*,
/// and `WindowService::mutate` asks. A schedule under an unset policy opens a unit,
/// which is `inst-mat-failsafe` and not this predicate.
#[test]
fn a_schedule_is_not_a_registered_trigger() {
    let planned = planned(Op::Schedule, None, Some(30));
    assert_eq!(planned.op.registered_trigger(&planned), None);
}

/// A **shortening** `PATCH` is always-material; a **lengthening** one is not.
///
/// The pair `window::shortens` decides, and the arm no route-level test can reach:
/// over HTTP D-182's floor refuses every shorten before the gate is consulted, so
/// without this the `Adjust` arm would be classified by an unasserted function.
///
/// Every case is one comparison of the *stored* end against the *asked-for* one, and
/// the four rows are the whole space: bounded→earlier, bounded→later, bounded→removed,
/// open→bounded.
#[test]
fn a_shorten_is_a_registered_trigger_and_a_lengthen_is_not() {
    // Bounded, pulled earlier: coverage is removed.
    let shorten = planned(
        Op::Adjust { expected_seq: 0 },
        Some(stored(0, Some(30))),
        Some(20),
    );
    assert_eq!(
        shorten.op.registered_trigger(&shorten),
        Some(Trigger::WindowShortening),
        "a bounded end pulled earlier is a shorten"
    );

    // Bounded, pushed later: coverage is added.
    let lengthen = planned(
        Op::Adjust { expected_seq: 0 },
        Some(stored(0, Some(30))),
        Some(40),
    );
    assert_eq!(
        lengthen.op.registered_trigger(&lengthen),
        None,
        "an extension removes nothing and is not on the trigger list"
    );

    // Bounded, bound removed: the largest extension there is.
    let unbounded = planned(
        Op::Adjust { expected_seq: 0 },
        Some(stored(0, Some(30))),
        None,
    );
    assert_eq!(
        unbounded.op.registered_trigger(&unbounded),
        None,
        "dropping the bound extends to forever"
    );

    // Open-ended, bounded: every instant past the new end is removed.
    let bounding = planned(
        Op::Adjust { expected_seq: 0 },
        Some(stored(0, None)),
        Some(40),
    );
    assert_eq!(
        bounding.op.registered_trigger(&bounding),
        Some(Trigger::WindowShortening),
        "bounding an open-ended window removes the whole tail"
    );

    // The same end again removes nothing, so it needs nobody's signature.
    let no_op = planned(
        Op::Adjust { expected_seq: 0 },
        Some(stored(0, Some(30))),
        Some(30),
    );
    assert_eq!(
        no_op.op.registered_trigger(&no_op),
        None,
        "a move to the end the window already has is not a shorten"
    );
}

// ---------------------------------------------------------------------------
// What the controlled arm hands back
// ---------------------------------------------------------------------------

/// The controlled arm answers with the **stored** interval, never the requested one.
///
/// The property a caller's client depends on: the 202 of a mutation that did not run
/// describes the world as the caller will still find it. Echoing
/// `planned.effective_to` — what was *asked* for — would read as though the end had
/// moved, which is exactly the misreading a two-person control must not invite.
///
/// **And it carries the verdict it was given rather than one of its own.** The field
/// used to be filled in here with `material(AlwaysMaterialTrigger)`, which was true of
/// the two acts D-62 governs and false the moment a schedule or a lengthening could
/// reach this arm — as one does now, under `inst-mat-failsafe`. Both are asserted
/// below, because a constant would satisfy only the first.
#[test]
fn the_controlled_arm_echoes_the_stored_interval_and_not_the_request() {
    let window_id = uuid::Uuid::from_u128(0x_d1);
    let planned = planned(
        Op::Adjust { expected_seq: 0 },
        Some(stored(0, Some(30))),
        Some(20),
    );

    let verdict = MaterialityVerdict::triggered(Trigger::WindowCancellation);
    let pending = pending_approval(
        &planned,
        window_id,
        verdict.clone(),
        submitted_unit(&verdict),
    );

    assert_eq!(pending.window_id, window_id);
    assert_eq!(pending.plan_id, plan());
    assert_eq!(pending.revision, 3);
    assert_eq!(pending.effective_from, at(0));
    assert_eq!(
        pending.effective_to,
        Some(at(30)),
        "the stored end, not the 20 the caller asked for"
    );
    assert_eq!(pending.state, WindowState::Scheduled);
    assert_eq!(
        pending.verdict,
        MaterialityVerdict::triggered(Trigger::WindowCancellation),
        "the verdict the evaluator produced for D-62's act"
    );
}

/// The same arm, reached by a **threshold** rule rather than by D-62.
///
/// A schedule is on nobody's trigger list, so what puts it here is
/// `inst-mat-failsafe` — no configured entry for the row's currency. The reason has to
/// survive to the answer: told `alwaysMaterialTrigger`, an operator would look for an
/// act on §3 step 4's list and find a schedule, while the fact they can act on is that
/// their policy has no entry for this currency.
#[test]
fn the_controlled_arm_carries_a_threshold_reason_when_that_is_what_answered() {
    let window_id = uuid::Uuid::from_u128(0x_d1);
    let planned = planned(Op::Schedule, None, Some(30));

    let verdict = MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold);
    let pending = pending_approval(
        &planned,
        window_id,
        verdict.clone(),
        submitted_unit(&verdict),
    );

    assert_eq!(
        pending.verdict,
        MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold),
        "a schedule refused by the fail-safe says so, and does not borrow D-62's reason"
    );
    assert_eq!(
        pending.state,
        WindowState::Scheduled,
        "and the window it describes is the one the caller asked for, since none is stored"
    );
}

/// The audited state pair carries the **content**, not a digest of it.
///
/// The standing heuristic from the writer's side: a record has to carry something
/// an independent expectation can be written against, so the four fields are asserted
/// by equality against a literal rather than against a re-render of the same record.
#[test]
fn the_audited_state_is_the_interval_and_where_it_stands() {
    let record = stored(0, Some(30));

    assert_eq!(
        window_state_value(&record),
        serde_json::json!({
            "state": "scheduled",
            "effectiveFrom": format_rfc3339(at(0)),
            "effectiveTo": format_rfc3339(at(30)),
            "priceId": record.price_id,
        })
    );
}
