//! `PATCH /bss-pricing/v1/prices/{priceId}/grandfather-until` over the real
//! router — S7 §4's `inst-gs-bound` and `inst-gs-tighten`.
//!
//! Every case here runs against a generation a **real cutover** created, approved
//! and committed. The fixture is expensive on purpose: a hand-inserted
//! `existing_grandfathered` row could be given any cohort, any horizon and any
//! window, and this door's whole subject is a state only the cutover produces —
//! a published generation, on a cohort that is the instant it was created, with an
//! open-ended window scheduled from that instant and a null horizon. Judging the
//! door against a row nobody could author would be judging it against the fixture.
//!
//! What is pinned here is what only the wire can show: the two arms and their
//! tokens, the transition the approval unit names, the before-image the trail
//! carries, the tag that deliberately does not move, and the four refusals a caller
//! can provoke. The span rule's own asymmetry — which direction of travel can newly
//! break D-04 — is pinned one layer down in `infra::grandfather`'s unit tests,
//! where the operand can be varied without a cutover per case.
//!
//! Instants are fixed at 2099 for `rest_cutovers.rs`' reason.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::cutovers::{PLAN_CUTOVERS, PRICE_GRANDFATHER_UNTIL};
use bss_pricing::domain::approval::{DecisionBy, WithdrawAuthority};
use bss_pricing::infra::approval::{DecideRequest, RegionGrant};

use bss_pricing::domain::instant::{format_rfc3339, utc_ymd_hms};
use rest_support::{
    Harness, audit_rows, body_json, code_in, problem_code, seed_publishable_plan, violation_for,
    with_headers,
};
use time::OffsetDateTime;
use uuid::Uuid;

const SUBMITTER: Uuid = Uuid::from_u128(0x_9f_11);
const REVIEWER: Uuid = Uuid::from_u128(0x_9f_22);

fn cutover_at() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

/// A horizon well inside the fixture's coverage.
fn horizon(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, day, 0, 0, 0)
}

fn path(price_id: Uuid) -> String {
    PRICE_GRANDFATHER_UNTIL.replace("{priceId}", &price_id.to_string())
}

/// The gear's own rendering, not a second one written here.
///
/// This used to call `time`'s well-known `Rfc3339` directly, which is a
/// re-implementation of the thing under test: the day the gear's renderer
/// changed, every expectation built through this helper asserted the form the
/// gear had stopped emitting, and the failure read as a wire-format regression
/// rather than as a stale fixture.
fn wire(at: OffsetDateTime) -> String {
    bss_pricing::domain::instant::format_rfc3339(at)
}

fn body(at: OffsetDateTime) -> serde_json::Value {
    serde_json::json!({ "grandfather_until": wire(at) })
}

async fn approve(h: &Harness, approval_id: Uuid) {
    h.governance
        .approvals
        .decide(
            &h.scope(),
            h.tenant,
            DecideRequest {
                approval_id,
                decision: DecisionBy::Approve(REVIEWER),
                reason: None,
                approver_regions: RegionGrant::Untransported,
                stamp: bss_pricing::domain::audit::AuditStamp {
                    actor_principal_id: REVIEWER,
                    recorded_at: OffsetDateTime::now_utc(),
                    correlation_id: Uuid::from_u128(0x_9f_c0),
                },
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the reviewer approves");
}

/// The id of the unit an act opened, from its `202`.
fn unit_of(view: &serde_json::Value) -> Uuid {
    view["approval"]["approval_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .unwrap_or_else(|| panic!("the controlled arm names its unit: {view}"))
}

/// A committed cutover, and the two published rows it leaves: the successor on the
/// original key, and the **grandfathered copy** on a generation of its own.
struct Generation {
    /// The `existing_grandfathered` copy — this door's subject.
    copy_price_id: Uuid,
    /// The `all_subscriptions` successor, which carries no horizon and may not.
    successor_price_id: Uuid,
}

async fn cut_over(h: &Harness) -> Generation {
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let cutover = serde_json::json!({
        "predecessor_price_id": seeded.price_id.to_string(),
        "cutover_at": cutover_at()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339"),
        "successor": {
            "model_kind": "flat",
            "amount_minor": 12_000,
            "billing_timing": "advance",
            // The same three `rest_cutovers`' own successor body gained when
            // D-344 gave the cutover door its aggregate pass, and the same
            // reason: `inst-pi-required` obliges a **recurring** row to publish
            // `billingAnchorPolicy`, `prorationBasis` and `creditOnDowngrade`,
            // and this body carried none of them. It is the identical fixture
            // fault one file over, and it stayed because this binary's whole
            // subject is the horizon rather than the cutover — so nothing here
            // reads the successor's contract and the omission was invisible
            // until the aggregate tier started asking.
            //
            // `seed_publishable_plan`'s predecessor already carries the trio
            // (`rest_support::publishable_row`); a successor that does not is a
            // row Subscriptions cannot prorate, and the catalog substitutes no
            // defaults. The fixture was authoring an unpublishable successor,
            // not the rule refusing a legal one.
            "billing_anchor_policy": "calendar_month",
            "proration_basis": "calendar_days_actual",
            "credit_on_downgrade": false,
            "rounding_policy_ref": "half_up"
        },
        "reason_code": "grandfatheringCutover"
    });
    let cutovers = PLAN_CUTOVERS.replace("{planId}", &plan_id.to_string());

    let opened = h
        .allowed_as(SUBMITTER)
        .send(with_headers("POST", &cutovers, Some(cutover.clone()), &[]))
        .await;
    let opened = body_json(opened).await;
    approve(h, unit_of(&opened)).await;

    let committed = h
        .allowed_as(SUBMITTER)
        .send(with_headers("POST", &cutovers, Some(cutover), &[]))
        .await;
    let committed = body_json(committed).await;
    assert_eq!(
        committed["outcome"], "cut_over",
        "the fixture needs a committed generation: {committed}"
    );
    Generation {
        copy_price_id: Uuid::parse_str(committed["copy_price_id"].as_str().unwrap()).unwrap(),
        successor_price_id: Uuid::parse_str(committed["successor_price_id"].as_str().unwrap())
            .unwrap(),
    }
}

/// The row's `ETag` as the store holds it. A published row's version is frozen
/// (D-141), so this is read once and stays correct for the whole case.
async fn tag_of(h: &Harness, price_id: Uuid) -> String {
    let record = h
        .governance
        .prices
        .find(&h.scope(), h.tenant, price_id)
        .await
        .expect("read the row")
        .expect("the row exists");
    format!("\"{}\"", record.row_version.get())
}

async fn stored_horizon(h: &Harness, price_id: Uuid) -> Option<OffsetDateTime> {
    h.governance
        .prices
        .find(&h.scope(), h.tenant, price_id)
        .await
        .expect("read the row")
        .expect("the row exists")
        .grandfather_until
}

// ---------------------------------------------------------------------------
// `inst-gs-bound` — the two arms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_controlled_arm_answers_202_names_the_transition_and_writes_no_column() {
    let h = Harness::new().await;
    let g = cut_over(&h).await;
    let tag = tag_of(&h, g.copy_price_id).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(1))),
            &[("if-match", &tag)],
        ))
        .await;

    // 202 on the arm that changed nothing, for the cutover's reason: a publish unit
    // is not consumer-visible until warm, so a 200 would claim the horizon moved.
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let view = body_json(response).await;
    assert_eq!(view["outcome"], "submitted_for_approval");
    // **Both ends of the transition**, and the prior one is `null` rather than
    // absent: a cutover's copy is born indefinite (D-147), and this is the
    // `active_indefinite -> active_bounded` edge.
    assert!(view["prior_grandfather_until"].is_null(), "{view}");
    assert_eq!(
        view["grandfather_until"],
        serde_json::json!(wire(horizon(1)))
    );
    assert!(view["pending_version_ref"].is_null(), "{view}");

    // Nothing was written. Asserted on the **column** rather than on the response,
    // because the response is what a door that wrote first and answered second
    // would produce too.
    assert!(
        stored_horizon(&h, g.copy_price_id).await.is_none(),
        "the controlled arm must leave the generation indefinite"
    );
}

#[tokio::test]
async fn the_call_after_an_independent_approve_sets_the_bound_and_records_the_before_image() {
    let h = Harness::new().await;
    let g = cut_over(&h).await;
    let tag = tag_of(&h, g.copy_price_id).await;

    let opened = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(1))),
            &[("if-match", &tag)],
        ))
        .await;
    let opened = body_json(opened).await;
    let approval_id = unit_of(&opened);
    approve(&h, approval_id).await;

    let committed = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(1))),
            &[("if-match", &tag)],
        ))
        .await;

    assert_eq!(committed.status(), StatusCode::ACCEPTED);
    // The tag is the **same** one on the way out, which is D-141's freeze made
    // observable rather than asserted in a comment: a published row's version does
    // not move under either sanctioned in-place mutation.
    assert_eq!(
        committed
            .headers()
            .get(axum::http::header::ETAG)
            .and_then(|v| v.to_str().ok()),
        Some(tag.as_str()),
        "a published row's entity tag is frozen with its content"
    );
    let view = body_json(committed).await;
    assert_eq!(view["outcome"], "tightened");
    assert!(
        view["pending_version_ref"].is_string(),
        "the committed act requested a version for the re-projection: {view}"
    );
    assert!(view["approval"].is_null(), "{view}");
    assert_eq!(
        stored_horizon(&h, g.copy_price_id).await,
        Some(horizon(1)),
        "the column moved"
    );

    // **The before-image**, which is this door's answer to D-327: the cutover's
    // unwind is unbuildable because a shortened `effectiveTo` is written in place
    // with no record of what it was, and a horizon tightening is an in-place
    // `UPDATE` of exactly that kind. Armed against the claim rather than against
    // "an audit row exists": what makes the record worth anything is that
    // `before_state` carries the **value**, so a reader can say what the horizon
    // was. A probe asserting only that the chain grew would pass against a record
    // whose `before_state` was null, and against the four staging records this
    // flow already writes.
    let audited = audit_rows(&h).await;
    let record = audited
        .iter()
        .find(|row| row.approval_ref == Some(approval_id) && row.action == "update")
        .unwrap_or_else(|| {
            panic!(
                "the commit must record the act under the unit that authorized it \
                 ({approval_id}); the chain holds {} row(s): {:?}",
                audited.len(),
                audited
                    .iter()
                    .map(|r| (&r.action, r.approval_ref))
                    .collect::<Vec<_>>()
            )
        });
    let before = record
        .before_state
        .as_ref()
        .expect("the record names what the horizon was");
    let after = record
        .after_state
        .as_ref()
        .expect("the record names what it became");
    assert!(
        before["grandfatherUntil"].is_null(),
        "the generation was indefinite before this act: {before}"
    );
    assert_eq!(
        after["grandfatherUntil"],
        serde_json::json!(format_rfc3339(horizon(1)))
    );
    // The pair is self-describing: one field moved and the tag did not, which is
    // what tells a reader this is the in-place mutation D-141 freezes the tag
    // through rather than a republish.
    assert_eq!(
        before["rowVersion"], after["rowVersion"],
        "the tag is frozen across the act: {before} -> {after}"
    );
    assert_eq!(before["priceId"], after["priceId"]);
}

/// `inst-gs-tighten`: the second edge, from the state the first one produced.
#[tokio::test]
async fn a_bounded_generation_may_be_tightened_further() {
    let h = Harness::new().await;
    let g = cut_over(&h).await;
    let tag = tag_of(&h, g.copy_price_id).await;
    let headers = [("if-match", tag.as_str())];

    for at in [horizon(20), horizon(5)] {
        let opened = h
            .allowed_as(SUBMITTER)
            .send(with_headers(
                "PATCH",
                &path(g.copy_price_id),
                Some(body(at)),
                &headers,
            ))
            .await;
        let opened = body_json(opened).await;
        approve(&h, unit_of(&opened)).await;
        let committed = h
            .allowed_as(SUBMITTER)
            .send(with_headers(
                "PATCH",
                &path(g.copy_price_id),
                Some(body(at)),
                &headers,
            ))
            .await;
        let view = body_json(committed).await;
        assert_eq!(view["outcome"], "tightened", "{view}");
    }

    assert_eq!(stored_horizon(&h, g.copy_price_id).await, Some(horizon(5)));
    // **The second act's unit is not the first's.** The subject names both ends of
    // the transition (D-184), so an approval taken for `null -> 09-20` cannot
    // authorize `09-20 -> 09-05`; if it could, one approve would license every
    // later tightening of the row.
    let units = audit_rows(&h)
        .await
        .into_iter()
        .filter(|row| row.action == "update")
        .filter_map(|row| row.approval_ref)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        units.len(),
        2,
        "each tightening commits under its own unit, got {units:?}"
    );
}

// ---------------------------------------------------------------------------
// The refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_horizon_that_does_not_move_earlier_is_refused_by_its_own_code() {
    let h = Harness::new().await;
    let g = cut_over(&h).await;
    let tag = tag_of(&h, g.copy_price_id).await;
    let headers = [("if-match", tag.as_str())];

    let opened = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(10))),
            &headers,
        ))
        .await;
    approve(&h, unit_of(&body_json(opened).await)).await;
    h.allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(10))),
            &headers,
        ))
        .await;

    // Later.
    let loosened = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(25))),
            &headers,
        ))
        .await;
    assert_eq!(
        problem_code(loosened).await,
        "GRANDFATHER_LOOSEN_FORBIDDEN",
        "pushing the horizon out re-grants an eligibility a reviewer approved the end of"
    );
    // And equal, which is the arm a no-op reading would have accepted — and which
    // would have put a transition that moves nothing in front of a second principal.
    let unchanged = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(10))),
            &headers,
        ))
        .await;
    assert_eq!(
        problem_code(unchanged).await,
        "GRANDFATHER_LOOSEN_FORBIDDEN"
    );
    assert_eq!(
        stored_horizon(&h, g.copy_price_id).await,
        Some(horizon(10)),
        "neither refusal moved the column"
    );
}

#[tokio::test]
async fn a_row_off_the_grandfathered_class_is_refused_by_d147s_code() {
    // The cutover's **successor** is `all_subscriptions` and published, so it is
    // the same shape as this door's subject in every way except the one axis that
    // admits a row to the eligibility machine at all (S7 §4's entry condition).
    // That is what makes it the right negative: a fixture differing in more than
    // the operand could be refused for another reason and still read green.
    let h = Harness::new().await;
    let g = cut_over(&h).await;
    let tag = tag_of(&h, g.successor_price_id).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.successor_price_id),
            Some(body(horizon(1))),
            &[("if-match", &tag)],
        ))
        .await;

    assert_eq!(problem_code(response).await, "GRANDFATHER_UNTIL_FORBIDDEN");
    assert!(stored_horizon(&h, g.successor_price_id).await.is_none());
}

#[tokio::test]
async fn a_draft_row_is_refused_and_told_which_door_authors_its_horizon() {
    let h = Harness::new().await;
    // A plan whose **revision** is published and whose one price row is not. The
    // revision has to be, and finding that out is what this probe's first run was
    // worth: `read_horizon_context` resolves the plan's current revision before any
    // refusal — `infra::window::read_plan_context`'s shape — so a plan that has
    // never published answers `404` and the state refusal below is unreachable
    // through it. That ordering is inherited rather than chosen here, and it is
    // reported as such: an operator pointed at a draft row of a never-published
    // plan is told the plan is absent, which is true of its *current revision* and
    // reads as though the plan were.
    let plan_id = Uuid::now_v7();
    let draft = seed_publishable_plan(&h, plan_id).await;
    h.publish(plan_id, draft.revision).await;
    let tag = tag_of(&h, draft.price_id).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(draft.price_id),
            Some(body(horizon(1))),
            &[("if-match", &tag)],
        ))
        .await;

    // **The status and the code, not a substring of the whole document.** This
    // case bound `status` and never asserted it, and rested the refusal on
    // `view.to_string().contains("LIFECYCLE_FORBIDDEN")` — the defect
    // `rest_cutovers.rs` records having fixed one file over. Measured on
    // 2026-08-20: sending `If-Match: LIFECYCLE_FORBIDDEN prices/{priceId}`
    // provoked the *tag* refusal ("a strong entity tag is wrapped in double
    // quotes", 400) whose detail echoes the header verbatim, and **both**
    // substring assertions passed on it. A caller could satisfy this test from
    // the outside; a `LIFECYCLE_FORBIDDEN` rendered under a 200 or a 500 would
    // have satisfied it too.
    let status = response.status();
    let view = body_json(response).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the lifecycle refusal reaches the wire as a 400 (Foundation section 3.3): {view}"
    );
    assert_eq!(
        code_in(&view),
        "LIFECYCLE_FORBIDDEN",
        "a draft row's horizon is another door's: {view}"
    );
    // The remediation hint, read off the violation this refusal files rather than
    // off the rendered document, so nothing a caller put in a header can stand in
    // for it. (`detail` on a precondition document is the platform's generic
    // "Operation precondition not met" — the sentence an operator acts on is the
    // violation's `description`.)
    assert!(
        violation_for(&view, "lifecycle_state")
            .is_some_and(|described| described.contains("prices/{priceId}")),
        "the refusal names the door that does author it: {view}"
    );
}

#[tokio::test]
async fn a_tag_the_row_never_carried_is_refused_before_the_unit_is_opened() {
    let h = Harness::new().await;
    let g = cut_over(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(1))),
            &[("if-match", "\"4095\"")],
        ))
        .await;

    assert_eq!(problem_code(response).await, "STALE_VERSION");
    // No unit was opened. The precondition is judged before the evaluator, so a
    // caller who cannot commit is never put in front of a reviewer — the ordering
    // `infra::window::mutate_in`'s step 3a states and this door inherits.
    let units = audit_rows(&h)
        .await
        .into_iter()
        .filter(|row| row.action == "submit")
        .count();
    assert_eq!(
        units, 1,
        "only the cutover's own unit; the refused call opened none"
    );
}

#[tokio::test]
async fn a_caller_with_no_grant_is_denied_before_the_body_is_read() {
    // The gate runs above the parse and above the precondition, which is this
    // gear's house rule. The body and the tag are **both deliberately malformed**
    // so that a handler doing either first would answer 400 and this case would
    // catch it.
    let h = Harness::new().await;
    let g = cut_over(&h).await;

    let response = h
        .denied()
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(serde_json::json!({ "not": "a horizon" })),
            &[("if-match", "not-a-tag")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// **A caller of another tenant cannot bound this tenant's generation**, and the
/// refusal does not tell them the row exists.
///
/// The one shape that exercises the SQL tenant predicate on this door:
/// `Harness::denied` drives a PDP that refuses everything and
/// `Harness::scope_mismatch` exercises `access_scope`'s write-target membership
/// assertion, and neither hands a caller-supplied id of another tenant's row to
/// the repository. `rest_authz`'s census cannot cover this route either — its seed
/// leaves a draft plan and this door answers a `404` to its *owner* there, so the
/// route is listed in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE`. Here the fixture
/// is a committed generation, so the owner's identical call is accepted and the
/// refusal below means tenancy rather than a missing subject.
#[tokio::test]
async fn a_foreign_tenant_is_refused_like_an_absent_row_and_the_owner_is_not() {
    let h = Harness::new().await;
    let g = cut_over(&h).await;
    let tag = tag_of(&h, g.copy_price_id).await;
    // The cutover's own unit, and the only one this world should ever hold if the
    // two refusals below open none.
    let units_before = submits(&h).await;

    let foreign = h
        .other_tenant()
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(1))),
            &[("if-match", tag.as_str())],
        ))
        .await;
    let absent = h
        .other_tenant()
        .send(with_headers(
            "PATCH",
            &path(Uuid::now_v7()),
            Some(body(horizon(1))),
            &[("if-match", tag.as_str())],
        ))
        .await;

    let foreign_status = foreign.status();
    let absent_status = absent.status();
    assert_eq!(
        foreign_status, absent_status,
        "another tenant's row and an id nothing points at must answer alike, or the status is a \
         probe for which price ids are real"
    );
    let foreign_body = body_json(foreign).await;
    let absent_body = body_json(absent).await;
    assert_eq!(
        foreign_body.get("type"),
        absent_body.get("type"),
        "and alike in the problem `type` too, or the body distinguishes what the status does not"
    );
    // The `detail` of a 404 here names the id the **caller** supplied, so it is not
    // an oracle; what would be one is a body that named the row's plan, its tenant
    // or its state. Asserted as the absent call's own shape rather than by
    // inspecting members, because a member added later must not become a leak
    // nobody looked at.
    assert_eq!(
        foreign_body
            .get("context")
            .and_then(|c| c.get("resource_name")),
        Some(&serde_json::json!(g.copy_price_id.to_string())),
        "the refusal must say nothing about the row beyond the id that was asked for"
    );
    assert_eq!(
        stored_horizon(&h, g.copy_price_id).await,
        None,
        "the horizon moved for a caller of another tenant"
    );
    assert_eq!(
        submits(&h).await,
        units_before,
        "a refused caller of another tenant put this tenant's row in front of a reviewer"
    );

    // The control, and it is what makes the two refusals above mean anything: the
    // same request from the row's own tenant is accepted. Last, because it opens a
    // unit.
    let owner = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &path(g.copy_price_id),
            Some(body(horizon(1))),
            &[("if-match", tag.as_str())],
        ))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::ACCEPTED,
        "the owner's identical call must be accepted, or the refusals above are about the \
         request rather than about the tenant"
    );
    assert_eq!(
        submits(&h).await,
        units_before + 1,
        "and it is the call that opens the unit"
    );
}

/// How many units this world has been asked to open, off the trail rather than
/// off a response — a handler that answered 202 and opened nothing is the failure
/// mode the count exists for.
async fn submits(h: &Harness) -> usize {
    audit_rows(h)
        .await
        .into_iter()
        .filter(|row| row.action == "submit")
        .count()
}
