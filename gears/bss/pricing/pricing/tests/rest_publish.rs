//! `POST /bss-pricing/v1/plans/{planId}/publish` — the entrance, driven through
//! the real router.
//!
//! # What this suite is for, and what `rest_approvals.rs` is for
//!
//! This one owns the **publish** side: the two arms of the route, the
//! precondition, the approve→commit window, and what the commit does to the
//! units it did not consume. The approval surface's own routes — the queue, the
//! record and the three decisions — are `tests/rest_approvals.rs`.
//!
//! # Every case here puts the world in the state where the guard it names
//! # answers
//!
//! The route has four guards that fire before the interesting one: the authz
//! gate, the `If-Match`, "does the plan hold an open draft", and the validation
//! pipeline. A test that skipped any of them would be answered by that guard and
//! would pass against a handler that never reached the rule it claims to prove.
//! So the suite opens with
//! [`a_publish_of_a_publishable_plan_opens_a_pinned_unit`], which is the world
//! every other case moves exactly one thing away from.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

use bss_pricing::domain::approval::ApprovalState;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::scope_key::PhaseId;
use rest_support::{
    Harness, approval_row, approval_rows, audit_rows, body_json, outbox_correlations_of,
    plan_state, price_rows, problem_code, publishable_row, publishable_scope_key, refused_by,
    seed_draft_plan, seed_publishable_plan, seed_publishable_plan_with, with_headers,
};
use time::OffsetDateTime;
use uuid::Uuid;

/// The submitting principal, and the independent one that reviews.
const SUBMITTER: Uuid = Uuid::from_u128(0x5_b0);
const APPROVER: Uuid = Uuid::from_u128(0xa_b0);

fn publish_path(plan_id: Uuid) -> String {
    format!("/bss-pricing/v1/plans/{plan_id}/publish")
}

/// Drive the publish route as `principal`, under the seeded tag.
async fn publish_as(
    h: &Harness,
    principal: Uuid,
    plan_id: Uuid,
    tag: &str,
) -> axum::http::Response<axum::body::Body> {
    h.allowed_as(principal)
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", tag)],
        ))
        .await
}

/// Approve the plan's one pending unit as an independent principal, through the
/// real route.
async fn approve_through_the_route(h: &Harness, approval_id: Uuid) {
    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "the approve must succeed for the publish under test to mean anything"
    );
}

/// Reject the plan's one pending unit as an independent principal, through the
/// real route.
///
/// The reason is mandatory (`inst-as-reject`), so a reject staged without one
/// would be answered by `REASON_REQUIRED` and would never reach the state flip
/// the caller is here for.
async fn reject_through_the_route(h: &Harness, approval_id: Uuid) {
    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/reject"),
            Some(serde_json::json!({ "reason": "the EU uplift is not signed off" })),
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "the reject must succeed for the publish under test to mean anything"
    );
}

/// The id of the single unit the store holds.
async fn only_unit(h: &Harness) -> Uuid {
    let rows = approval_rows(h).await;
    assert_eq!(rows.len(), 1, "expected exactly one approval unit");
    rows[0].approval_id
}

// ---------------------------------------------------------------------------
// The submit arm.
// ---------------------------------------------------------------------------

/// **The positive control.** A publishable plan, published by a caller who may:
/// the change is material, a unit opens pinned to the exact revision, and the
/// answer is 202 rather than a freeze.
#[tokio::test]
async fn a_publish_of_a_publishable_plan_opens_a_pinned_unit() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(body["outcome"], "submitted_for_approval");
    assert_eq!(body["materiality"]["material"], true);
    // The fail-safe arm, named. This tenant has configured **no** policy, which is
    // the state every tenant starts in (D-10 makes configuring one itself material),
    // and `a_configured_threshold_policy_changes_the_stored_materiality_reason` is the
    // same publish under a policy - where this token stops being the answer.
    assert_eq!(body["materiality"]["reason"], "noConfiguredThreshold");
    assert_eq!(body["receipt"], serde_json::Value::Null);

    let rows = approval_rows(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "submitted");
    assert_eq!(rows[0].submitter_principal, SUBMITTER);
    assert_eq!(
        rows[0].subject_ref,
        format!("{plan_id}/{}", seeded.revision)
    );
    assert_eq!(
        rows[0].content_hash.len(),
        32,
        "the pin is a SHA-256 digest, and a record whose pin is not one cannot verify"
    );

    // And nothing was frozen: a submit is not a publish.
    assert_eq!(
        plan_state(&h, plan_id, seeded.revision).await.as_deref(),
        Some("draft")
    );
}

/// The plan has to be publishable before it is put in front of a reviewer.
///
/// §5's Purpose cell runs the fail-closed validation **and** the submit, in that
/// order, and the order is the point: a reviewer who approves a change set the
/// commit will refuse has spent their signature on nothing.
#[tokio::test]
async fn a_plan_that_cannot_publish_opens_no_unit_at_all() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    // The authoring suites' seed rather than the publishable one: a `recurring`
    // plan carrying no frequency, which is a legal draft (§4.2 puts the rules at
    // publish) and an illegal publish.
    seed_draft_plan(&h, plan_id).await;

    let response = publish_as(&h, SUBMITTER, plan_id, "\"0-0\"").await;

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "CYCLE_METADATA_MISSING");
    assert!(
        approval_rows(&h).await.is_empty(),
        "an unpublishable plan must not reach a reviewer"
    );
}

// ---------------------------------------------------------------------------
// Slice 10's two composite rules, driven through this route.
// ---------------------------------------------------------------------------

/// Attach a composite set to the plan's open draft through the real `PATCH`
/// route, and answer the tag the next verb needs.
///
/// **Through the route, and that is the whole design of these two cases** (D-257).
/// A test that hand-built a `PlanShape` with `shape.composites = vec![...]` proves
/// the *rule* and says nothing about whether anything can reach it - and until this
/// facet existed nothing could: `replace_composites` had no caller in `src/`, so
/// `CompositeArity` and `CompositeSelfReference` ran on every publish over a
/// permanently empty vector. Reachability is what these cases are for, so the
/// composite has to enter the store the way a client puts it there.
async fn attach_composites(
    h: &Harness,
    plan_id: Uuid,
    tag: &str,
    composites: serde_json::Value,
) -> String {
    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "PATCH",
            &format!("/bss-pricing/v1/plans/{plan_id}"),
            Some(serde_json::json!({ "composites": composites })),
            &[("if-match", tag)],
        ))
        .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "the composites facet must land for the publish under test to mean anything"
    );
    let body = body_json(response).await;
    format!(
        "\"{}-{}\"",
        body["revision"].as_u64().expect("the patched revision"),
        body["row_version"].as_u64().expect("its new version")
    )
}

/// Every precondition-violation code a refusal carries.
///
/// `rest_overlays.rs`' reading, and deliberately the same one: RFC 9457 puts the
/// code at `context.violations[].type`, and a case that read the prose instead
/// would be matching on a message rather than on the discriminator §3.3 makes the
/// contract. Compared by equality against the whole list rather than by
/// `contains` over the rendered document - a code with a character appended
/// satisfies a substring test.
async fn violation_codes(response: axum::http::Response<axum::body::Body>) -> Vec<String> {
    let body = body_json(response).await;
    body["context"]["violations"]
        .as_array()
        .unwrap_or_else(|| panic!("a 400 from the pre-check enumerates its violations: {body}"))
        .iter()
        .filter_map(|violation| violation["type"].as_str())
        .map(str::to_owned)
        .collect()
}

/// **`COMPOSITE_TOO_FEW_CONSTITUENTS` fires, in both of its readings.**
///
/// Two publishes over one seed, because the two readings are the same rule and
/// the second is the one that was silently passing:
///
/// 1. `["vcpu"]` - one constituent. A derived meter over a single meter adds a
///    level of indirection that changes no charge, which is what
///    `inst-cm-constituents` refuses.
/// 2. `["vcpu", "vcpu"]` - **two entries naming one meter.** This published
///    unrefused until the rule started counting *distinct* units: the guard read
///    `constituent_units.len() < 2`, and a duplicate satisfies a length test while
///    being exactly the composite in (1) wearing a disguise. No column catches it
///    either - the unique index is over `output_unit` and `constituent_units` is
///    opaque `jsonb`.
///
/// A refused publish writes nothing, so the second attempt runs against the same
/// draft with the tag the first patch left.
#[tokio::test]
async fn a_composite_that_prices_one_meter_cannot_publish_however_it_is_spelled() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let tag = attach_composites(
        &h,
        plan_id,
        &seeded.etag(),
        serde_json::json!([{
            "output_unit": "vm",
            "constituent_units": ["vcpu"],
            "formula": { "op": "identity" }
        }]),
    )
    .await;

    let refused = publish_as(&h, SUBMITTER, plan_id, &tag).await;
    assert_eq!(refused.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        violation_codes(refused).await,
        vec!["COMPOSITE_TOO_FEW_CONSTITUENTS".to_owned()]
    );
    assert!(
        approval_rows(&h).await.is_empty(),
        "an unpublishable plan must not reach a reviewer"
    );

    // The duplicate reading, on the same draft. The tag has not moved: the refusal
    // above ran the pre-check and wrote nothing.
    let tag = attach_composites(
        &h,
        plan_id,
        &tag,
        serde_json::json!([{
            "output_unit": "vm",
            "constituent_units": ["vcpu", "vcpu"],
            "formula": { "op": "sum" }
        }]),
    )
    .await;

    let refused = publish_as(&h, SUBMITTER, plan_id, &tag).await;
    assert_eq!(refused.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        violation_codes(refused).await,
        vec!["COMPOSITE_TOO_FEW_CONSTITUENTS".to_owned()],
        "two entries naming one meter is a one-constituent composite"
    );
    assert!(approval_rows(&h).await.is_empty());
}

/// **`COMPOSITE_SELF_REFERENCE` fires on the transitive cycle, through the route.**
///
/// `vm` is built from `pod` and `pod` from `vm`: neither definition is
/// self-referential on its own, and §9 asks for direct *and transitive* rejection
/// precisely because a row-local check only ever finds the half that matters less.
/// A formula defined in terms of its own output has no evaluation order, so what
/// this stops is a version freezing a definition Rating could not compute from -
/// and the freeze is what `inst-cm-frozen` makes irreversible.
///
/// **Two violations, not one.** Both definitions are in the cycle and an operator
/// breaking either one fixes it, so a report naming only the first would send them
/// to edit a definition that may not be the one they want to change.
#[tokio::test]
async fn a_composite_cycle_across_two_definitions_cannot_publish() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let tag = attach_composites(
        &h,
        plan_id,
        &seeded.etag(),
        serde_json::json!([
            {
                "output_unit": "vm",
                "constituent_units": ["vcpu", "pod"],
                "formula": { "op": "weighted_sum" }
            },
            {
                "output_unit": "pod",
                "constituent_units": ["ram", "vm"],
                "formula": { "op": "weighted_sum" }
            }
        ]),
    )
    .await;

    let refused = publish_as(&h, SUBMITTER, plan_id, &tag).await;

    assert_eq!(refused.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        violation_codes(refused).await,
        vec![
            "COMPOSITE_SELF_REFERENCE".to_owned(),
            "COMPOSITE_SELF_REFERENCE".to_owned()
        ],
        "both definitions are in the cycle, and either one is a place to break it"
    );
    assert!(
        approval_rows(&h).await.is_empty(),
        "an unpublishable plan must not reach a reviewer"
    );
}

/// **The positive control for the pair.** A well-formed composite publishes.
///
/// Without it the two refusals above are consistent with a facet that makes *every*
/// plan carrying a composite unpublishable - which would be a worse defect than the
/// one this increment closed, and both cases would still be green.
#[tokio::test]
async fn a_well_formed_composite_publishes_like_any_other_shape() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let tag = attach_composites(
        &h,
        plan_id,
        &seeded.etag(),
        serde_json::json!([{
            "output_unit": "vm",
            "constituent_units": ["vcpu", "ram"],
            "formula": { "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }
        }]),
    )
    .await;

    let response = publish_as(&h, SUBMITTER, plan_id, &tag).await;

    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(body["outcome"], "submitted_for_approval");
    assert_eq!(
        approval_rows(&h).await.len(),
        1,
        "a plan with a legal composite reaches a reviewer like any other"
    );
}

// ---------------------------------------------------------------------------
// `inst-ph-row-attached` (D-337), driven through this route.
// ---------------------------------------------------------------------------

/// **The state that started D-337, as a test.**
///
/// A plan publishable in every other respect whose one price row keys on a phase
/// the revision does not attach. That is the stand's plan
/// `019ff3de-7244-7d52-9496-bc823b1ba2f6`, all four of whose rows named a phase
/// id nobody had ever attached: it was refused with nine violations and **not one
/// of them said so**, so the operator was told to add a terminal phase — which
/// would have stranded every row they had authored, because a scope key is the
/// row's identity and cannot be re-pointed.
///
/// Driven through the route rather than over a hand-built `PlanShape` because the
/// unit cases already prove the rule; what this proves is that the aggregate the
/// publish route runs registers it, which is the one thing a rule's own tests
/// cannot see.
///
/// **The whole code list, and it is two codes long on purpose.** `PHASE_ROW_ORPHANED`
/// and its exact inverse `PHASE_UNCOVERED` — the attached terminal phase now has
/// no recurring row covering it — are adjacent in the report, which is the
/// arrangement D-337 registers the rule in. A `contains` check would be satisfied
/// by the rule firing anywhere in any order, and a count would be satisfied by the
/// neighbouring `PHASE_GRAPH_INVALID` standing in for it entirely.
#[tokio::test]
async fn a_row_keyed_on_a_phase_the_revision_never_attached_is_refused_naming_both() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    // Minted here and never attached anywhere: the seed hands its closure the
    // phase it *did* attach, and this one deliberately ignores it. That is how the
    // observed plan came to be — a client that invented an id instead of reading
    // one back.
    let ghost = PhaseId::new(Uuid::now_v7());
    let seeded = seed_publishable_plan_with(
        &h,
        plan_id,
        move |plan, _attached| publishable_scope_key(plan, ghost, "eu"),
        publishable_row(),
    )
    .await;

    let refused = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(refused.status(), axum::http::StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    let violations = body["context"]["violations"]
        .as_array()
        .unwrap_or_else(|| panic!("the refusal enumerates its violations: {body}"));
    assert_eq!(
        violations
            .iter()
            .filter_map(|violation| violation["type"].as_str())
            .collect::<Vec<_>>(),
        vec!["PHASE_ROW_ORPHANED", "PHASE_UNCOVERED"],
        "the row with no phase, then the phase with no rows: {body}"
    );
    // Both operands reach the wire, because both remediations need one of them:
    // attach *that* id, or delete *that* row.
    assert!(
        violations[0]["subject"]
            .as_str()
            .is_some_and(|subject| subject.contains(&ghost.to_string())),
        "the phase the rows already name is what an author would attach: {body}"
    );
    assert!(
        violations[0]["description"]
            .as_str()
            .is_some_and(|detail| detail.contains(&seeded.price_id.to_string())),
        "the row is named, because deleting it is the other remediation: {body}"
    );
    assert!(
        approval_rows(&h).await.is_empty(),
        "an unpublishable plan must not reach a reviewer"
    );
}

/// `inst-co-single-pending`: one pending unit per subject.
#[tokio::test]
async fn a_second_submit_while_a_unit_is_pending_is_refused() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    let first = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_eq!(first.status(), axum::http::StatusCode::ACCEPTED);

    let second = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(second.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(problem_code(second).await, "PENDING_CHANGE_UNIT_EXISTS");
    assert_eq!(
        approval_rows(&h).await.len(),
        1,
        "the refusal must leave one unit, not two"
    );
}

/// **G5.3 — the withdraw frees the subject the unit held.**
///
/// The two halves are one test on purpose. The first is the guard
/// ([`a_second_submit_while_a_unit_is_pending_is_refused`] proves it separately);
/// the second is what `inst-as-void` says the withdraw is *for*. Without the
/// first half the second proves nothing, because a subject nothing holds is free
/// whether or not anything freed it.
#[tokio::test]
async fn a_withdraw_frees_the_subject_and_a_fresh_submit_opens_a_new_record() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    let first = only_unit(&h).await;

    // Held.
    let blocked = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_eq!(problem_code(blocked).await, "PENDING_CHANGE_UNIT_EXISTS");

    // Withdrawn — by the submitter, which is exactly the actor `inst-as-void`
    // names and the one whose id used to collide with
    // `chk_pricing_approval_distinct_principals`.
    let withdrawn = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{first}/withdraw"),
            None,
            &[],
        ))
        .await;
    assert_eq!(withdrawn.status(), axum::http::StatusCode::OK);

    // Free, and the fresh submit opens a **new** record rather than re-opening
    // the old one — `inst-as-immutable`: there is no re-open.
    let again = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_eq!(again.status(), axum::http::StatusCode::ACCEPTED);

    let rows = approval_rows(&h).await;
    assert_eq!(rows.len(), 2, "a fresh submit opens a new record");
    let voided = approval_row(&h, first).await;
    assert_eq!(voided.state, ApprovalState::Voided);
    assert_eq!(
        voided.approver_principal, None,
        "a withdraw exercises no review authority, so it names no approver"
    );
    let fresh: Vec<_> = rows.iter().filter(|row| row.approval_id != first).collect();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].state, "submitted");
}

// ---------------------------------------------------------------------------
// The commit arm.
// ---------------------------------------------------------------------------

/// **The phase's goal, on the wire**: a plan published by two distinct
/// principals through HTTP.
#[tokio::test]
async fn an_approved_revision_publishes_on_the_second_call() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    let approval_id = only_unit(&h).await;
    approve_through_the_route(&h, approval_id).await;

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["outcome"], "published");
    assert_eq!(body["approval"]["approval_id"], approval_id.to_string());
    assert_eq!(body["receipt"]["revision"], seeded.revision);
    assert_eq!(
        body["receipt"]["published_price_ids"][0],
        seeded.price_id.to_string()
    );
    assert!(
        body["receipt"]["pending_version_ref"].is_string(),
        "the receipt names the registry handle, which is not a CatalogVersion"
    );

    assert_eq!(
        plan_state(&h, plan_id, seeded.revision).await.as_deref(),
        Some("published")
    );
    assert_eq!(
        price_rows(&h, plan_id).await[0].lifecycle_state,
        LifecycleState::Published
    );
}

/// A revision with no approved unit does not publish, however many times it is
/// asked.
///
/// The second call is the point: without it this would also pass against a
/// handler that publishes on the second attempt regardless of what was decided.
///
/// What the second call is asserted **not** to be is a publish, rather than any
/// particular refusal. Which refusal it earns is
/// [`a_second_submit_while_a_unit_is_pending_is_refused`]'s subject, and
/// asserting it here too would make one guard's removal redden two tests that
/// are about different things.
#[tokio::test]
async fn a_revision_nobody_approved_never_publishes() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let first = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_eq!(first.status(), axum::http::StatusCode::ACCEPTED);
    let second = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_ne!(
        second.status(),
        axum::http::StatusCode::OK,
        "nothing publishes without a second principal"
    );

    assert_eq!(
        plan_state(&h, plan_id, seeded.revision).await.as_deref(),
        Some("draft")
    );
    assert_eq!(
        price_rows(&h, plan_id).await[0].lifecycle_state,
        LifecycleState::Draft
    );
}

/// **A reviewer's explicit "no" authorizes nothing.**
///
/// The sibling above stages a unit nobody decided;
/// [`a_rejected_unit_never_publishes`] stages the case the design set is actually
/// about — a second principal read the change set and refused it — and the two
/// are not the same world. An **undecided** record carries no
/// `approver_principal`, so even a lookup that returned it would be turned into a
/// 500 by `publish::authorization_of` rather than into a publish; a **rejected**
/// one carries an approver, a distinct submitter and a 32-byte pin over exactly
/// this content, so it mints a perfectly well-formed [`PublishAuthorization`].
/// The **only** thing between it and a freeze is
/// `approval_repo::find_approved_for_content`'s `state = 'approved'` predicate,
/// and until this test nothing in the suite named that property: relaxing the
/// predicate turned this very call into a `200 published` while
/// `a_revision_nobody_approved_never_publishes` stayed green.
///
/// **The subject is deliberately left standing.** No row is touched between the
/// reject and the publish, so the pin still covers the current content and the
/// lookup's *content* half cannot be what refuses — if it were, this test would
/// pass against a store that treats a rejection as an approval.
///
/// What is asserted is the property and not the arm: today the route answers the
/// reject by opening a fresh unit (202), which is `inst-as-reject` returning the
/// revision to the authoring loop, but the invariant is that **nothing froze**.
#[tokio::test]
async fn a_rejected_unit_never_publishes() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    let approval_id = only_unit(&h).await;
    reject_through_the_route(&h, approval_id).await;

    // The record is a *decided* one, and everything a publish authorization is
    // minted from is on it. Without these three the test could pass because the
    // record was malformed rather than because it was refused.
    let rejected = approval_row(&h, approval_id).await;
    assert_eq!(rejected.state, ApprovalState::Rejected);
    assert_eq!(
        rejected.approver_principal,
        Some(APPROVER),
        "a reject names its reviewer, which is what makes the state the only guard left"
    );
    assert_eq!(rejected.content_hash.len(), 32);

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_ne!(
        response.status(),
        axum::http::StatusCode::OK,
        "a rejection must never authorize the freeze it rejected"
    );
    assert_ne!(
        body_json(response).await["outcome"],
        "published",
        "a rejection must never authorize the freeze it rejected"
    );
    assert_eq!(
        plan_state(&h, plan_id, seeded.revision).await.as_deref(),
        Some("draft")
    );
    assert_eq!(
        price_rows(&h, plan_id).await[0].lifecycle_state,
        LifecycleState::Draft
    );
    // And the reviewer's decision is left as they wrote it: a publish that ran
    // under it would also have voided or consumed it.
    let after = approval_row(&h, approval_id).await;
    assert_eq!(after.state, ApprovalState::Rejected);
    assert_eq!(
        after.reason.as_deref(),
        Some("the EU uplift is not signed off")
    );
}

/// **(a) The approve→commit window, at the surface.**
///
/// A price row is edited *after* the approve. `inst-ap-pin` scopes the TOCTOU
/// void to a `submitted` record, so the approved unit is **not** voided and
/// stands; the reviewer's signature is over content that has moved.
///
/// What the route does is refuse to publish under it and open a fresh unit —
/// see the module doc for why refusing outright would be a dead end. What
/// matters here is the property, and it is the same one: **the moved content
/// does not publish, and a second person has to look again.**
///
/// **The revision's row version does not move, and that is what makes this a
/// test of the pin.** A price-row edit moves the *row's* version and not the
/// revision's (D-141: the price plane carries its own), so the tag submitted is
/// still valid and the commit's compare-and-swap cannot be what answers. Staging
/// a plan-facet edit instead would move the revision's version and a plain
/// `STALE_VERSION` would answer whether or not a pin exists anywhere in this
/// gear.
///
/// The in-transaction half of the same guard — the one that closes the window
/// between this route's read and the commit — is
/// `sqlite_publish_commit::a_commit_whose_content_is_not_what_was_approved_is_refused`,
/// which is at the layer the check lives at.
#[tokio::test]
async fn a_row_edited_after_the_approve_does_not_publish_under_the_stale_decision() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    let approval_id = only_unit(&h).await;
    approve_through_the_route(&h, approval_id).await;

    // The subject moves, and nothing voids the approval: the unit is `approved`,
    // and every void in this gear closes only `submitted` ones.
    let before = price_rows(&h, plan_id).await;
    let mut content = rest_support::publishable_row();
    content.row.amount_minor =
        Some(bss_pricing::domain::money::MinorAmount::new(1).expect("a non-negative amount"));
    h.state
        .prices
        .update_draft(
            &h.scope(),
            h.tenant,
            seeded.price_id,
            before[0].row_version,
            content,
            rest_support::seed_stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect("re-price the row after the approve");
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Approved,
        "the approved unit must still stand, or this tests the void and not the pin"
    );
    assert_eq!(
        plan_row_version(&h, plan_id, seeded.revision).await,
        seeded.version.get(),
        "the revision's version must not have moved, or a compare-and-swap answers"
    );

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    // Not published — the whole property — and a fresh unit is open over what is
    // actually there.
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(body["outcome"], "submitted_for_approval");
    let fresh = body["approval"]["approval_id"]
        .as_str()
        .expect("the fresh unit")
        .to_owned();
    assert_ne!(
        fresh,
        approval_id.to_string(),
        "the stale decision must not be re-used for content it was not over"
    );
    assert_eq!(
        plan_state(&h, plan_id, seeded.revision).await.as_deref(),
        Some("draft")
    );
    // And the stale decision is left standing rather than rewritten:
    // `inst-as-immutable`, and it is the record of a decision that was made.
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Approved
    );
}

/// **(d) The publish accounts for the units it did not consume.**
///
/// A unit still `submitted` over the revision a commit freezes is an orphan: the
/// subject is immutable afterwards, so no approve of it can lead to a publish
/// and re-deriving it answers `APPROVAL_CONTENT_MISMATCH` for the rest of its
/// life. The commit voids it, in its own transaction, with a reason that says
/// what happened rather than the TOCTOU guard's "something moved".
///
/// **The orphan is staged through the service, because the mounted routes cannot
/// produce one, and that is reported rather than hidden.** The publish route
/// opens a unit only when no approved unit covers the content, and it commits
/// whenever one does — so between them there is no call sequence over the
/// mounted routes that leaves a `submitted` unit standing at a commit. The void
/// is therefore **defensive today**. It is kept because the set of surfaces that
/// can open a unit over a plan revision is going to grow — S12's bulk batch and
/// D-62's window units are both approval units over a plan — and because
/// "unreachable today" is the class of claim this program has already had to
/// withdraw once (`infra::approval`'s TOCTOU doc).
#[tokio::test]
async fn the_publish_voids_the_unit_it_did_not_consume() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    let consumed = only_unit(&h).await;
    approve_through_the_route(&h, consumed).await;

    // The orphan, opened directly: the route would have published instead.
    let orphan_id = Uuid::now_v7();
    h.governance
        .approvals
        .submit(
            &h.scope(),
            h.tenant,
            bss_pricing::domain::scope_key::PlanId::new(plan_id),
            orphan_id,
            serde_json::json!({ "material": true, "reason": "noConfiguredThreshold" }),
            bss_pricing::domain::audit::AuditStamp {
                actor_principal_id: SUBMITTER,
                recorded_at: OffsetDateTime::now_utc(),
                correlation_id: uuid::Uuid::from_u128(0x_c0_11_a7_10),
            },
        )
        .await
        .expect("stage a second pending unit over the same revision");
    assert_eq!(
        approval_row(&h, orphan_id).await.state,
        ApprovalState::Submitted
    );

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let closed = approval_row(&h, orphan_id).await;
    assert_eq!(closed.state, ApprovalState::Voided);
    assert_eq!(
        closed.reason.as_deref(),
        Some(bss_pricing::infra::approval::ORPHANED_BY_PUBLISH_REASON),
        "the reason has to say the revision was published, not that something moved"
    );
    // And the unit the publish ran under is untouched: it is the evidence of who
    // agreed, and voiding it would be the publish undoing its own authorization.
    assert_eq!(
        approval_row(&h, consumed).await.state,
        ApprovalState::Approved
    );
}

// ---------------------------------------------------------------------------
// The preconditions and the gate.
// ---------------------------------------------------------------------------

/// The composite tag, and the half of it a price route's tag does not have.
///
/// A bare version would satisfy the swap on a revision it was never read from,
/// which is D-145's lost update arriving through the revision.
#[tokio::test]
async fn a_tag_naming_another_revision_is_refused() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let response = publish_as(
        &h,
        SUBMITTER,
        plan_id,
        &format!("\"{}-{}\"", seeded.revision + 1, seeded.version.get()),
    )
    .await;

    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "STALE_VERSION");
    assert!(approval_rows(&h).await.is_empty());
}

#[tokio::test]
async fn an_absent_precondition_is_refused_before_anything_is_opened() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_publishable_plan(&h, plan_id).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers("POST", &publish_path(plan_id), None, &[]))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    // The guard, not the status. This door produces a code-less 400 for at least
    // five independent reasons - missing cycle metadata, a forbidden lifecycle
    // move, an abandoned plan with no successor, an aggregate violation report,
    // and this one - so the status alone did not say which answered.
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "If-Match is required on this verb: a mutation of a plan revision asserts both",
    );
    assert!(approval_rows(&h).await.is_empty());
}

/// A plan that does not exist reads as absent, not as a precondition failure.
#[tokio::test]
async fn a_plan_that_does_not_exist_answers_404() {
    let h = Harness::new().await;

    let absent = Uuid::now_v7();
    let response = publish_as(&h, SUBMITTER, absent, "\"0-0\"").await;

    // More than the status: this door answers 404 for an absent plan and for a plan
    // the caller may not read alike, and a bare status does not say which - the
    // argument the missing-precondition case one screen up makes about its own 400.
    // The family and the subject are what this refusal carries; it mints no wire
    // code, so `not_found_code` - which reads `context.resource_name` - answers the
    // plan id here rather than a code.
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let problem = body_json(response).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap_or_default()
            .contains("cf.core.err.not_found."),
        "the canonical family is `not_found`: {problem}"
    );
    assert_eq!(
        problem["context"]["resource_name"],
        serde_json::json!(absent.to_string()),
        "and the refusal names the plan it could not find: {problem}"
    );
}

/// A published plan holding no draft has nothing to publish, and the operator's
/// action is to edit it first — which is the refusal D-146 leaves
/// `LIFECYCLE_FORBIDDEN` holding.
#[tokio::test]
async fn a_plan_holding_no_open_draft_has_nothing_to_publish() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "LIFECYCLE_FORBIDDEN");
}

/// A spent plan — every revision abandoned — is told its id can carry no other,
/// which is a different next action and therefore a different code.
#[tokio::test]
async fn a_spent_plan_is_told_its_id_is_spent() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    h.abandon_draft(plan_id, seeded.revision).await;

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "PLAN_ABANDONED_NO_SUCCESSOR");
}

/// The gate is `plan × publish`, and it is asked **before** the repository.
#[tokio::test]
async fn the_publish_route_is_gated_and_writes_nothing_when_refused() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let denied = h
        .denied()
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);

    let anonymous = h
        .anonymous()
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(anonymous.status(), axum::http::StatusCode::UNAUTHORIZED);

    let unavailable = h
        .unavailable()
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(
        unavailable.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "a PDP outage fails closed"
    );

    let unconstrained = h
        .unconstrained()
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(
        unconstrained.status(),
        axum::http::StatusCode::FORBIDDEN,
        "an unconstrained allow compiles to a scope that filters nothing"
    );

    assert!(
        approval_rows(&h).await.is_empty(),
        "a 403 alone would also be produced by a handler that wrote first"
    );
    assert_eq!(
        plan_state(&h, plan_id, seeded.revision).await.as_deref(),
        Some("draft")
    );
}

/// **The SQL tenant predicate on the entrance.**
///
/// `rest_authz.rs`'s census cannot reach it: on that seed the plan does not satisfy
/// the publish rules, so the **owner** is refused there too and the route is listed
/// in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE`. Here `seed_publishable_plan` is the
/// world the rules pass on, so the owner's identical call is accepted and a refusal
/// of the foreign caller means tenancy rather than a rule.
///
/// `Harness::denied` drives a PDP that refuses everything and
/// `Harness::scope_mismatch` exercises `access_scope`'s write-target membership
/// assertion; neither hands a caller-supplied id of another tenant's row to a
/// repository, which is the only way the predicate is exercised at all. A handler
/// that resolved `{planId}` before narrowing would satisfy both of those and open a
/// unit over another tenant's plan.
#[tokio::test]
async fn a_foreign_tenants_plan_cannot_be_published() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let publish = |id: Uuid| {
        with_headers(
            "POST",
            &publish_path(id),
            None,
            &[("if-match", &seeded.etag())],
        )
    };
    rest_support::foreign_is_indistinguishable(&h, publish(plan_id), publish(Uuid::now_v7())).await;
    assert!(
        approval_rows(&h).await.is_empty(),
        "a refused caller of another tenant put this tenant's plan in front of a reviewer"
    );

    // The control, and it is what makes the two refusals mean anything. Last,
    // because it opens the unit.
    let owner = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_eq!(
        owner.status(),
        axum::http::StatusCode::ACCEPTED,
        "the owner's identical publish must be accepted, or the refusals above are about the \
         request rather than about the tenant"
    );
}

/// The revision's row version, read outside the caller's scope.
async fn plan_row_version(h: &Harness, plan_id: Uuid, revision: u64) -> u64 {
    rest_support::plan_row_version(h, plan_id, revision)
        .await
        .expect("the revision is there")
}

// ---------------------------------------------------------------------------
// D-178 — one correlation across the two stores one call writes to
// ---------------------------------------------------------------------------

/// A publish writes an audit record **and** a `pricing_outbox` row, and D-178
/// clause (2) is that they carry **one** value.
///
/// Two stores rather than two rows of one store, which is the half the
/// `PATCH`-side test cannot reach: an operator asking "which call emitted this
/// event" joins the outbox to the trail through this field and through nothing
/// else. The equality is the assertion; each being non-NULL separately would
/// pass against two independent mints.
#[tokio::test]
async fn the_publish_record_and_its_outbox_row_carry_one_correlation() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    let approval_id = only_unit(&h).await;
    approve_through_the_route(&h, approval_id).await;
    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let published: Vec<_> = audit_rows(&h)
        .await
        .into_iter()
        .filter(|row| row.action == "publish")
        .collect();
    assert_eq!(published.len(), 1, "one commit, one publish record");
    let recorded = published[0]
        .correlation_id
        .expect("the publish record carries a correlation");

    // The `PlanPublished` row by name, not "the only row in the outbox". The
    // count was a proxy for the event, and it stopped being a faithful one when
    // the plan plane got its `PlanCreated`/`PlanUpdated` producers and the
    // fixture began leaving three rows behind it — but it was never a
    // *sufficient* one either: a lone `PlanRetired` would have satisfied it.
    let emitted = outbox_correlations_of(&h, "PlanPublished").await;
    assert_eq!(emitted.len(), 1, "one commit, one `PlanPublished`");
    assert_eq!(
        emitted[0], recorded,
        "the trail and the outbox name the same operator call"
    );
}

/// **H-1(a): the tenant's configured policy reaches this route, and the stored
/// `materiality` stops saying `noConfiguredThreshold`.**
///
/// `infra::threshold::effective_policy` — the function whose own doc calls it *"the
/// fail-safe, executable"* — had **zero callers**: `materiality_of` passed `None`,
/// justified by a doc claiming the `GET/PUT` surface was not mounted. It is mounted, so
/// every publish of every tenant carried the fail-safe's token however that tenant had
/// configured itself, and a rule that cannot be switched off is an absent feature that
/// reads like one.
///
/// The fixture is [`a_publish_of_a_publishable_plan_opens_a_pinned_unit`]'s **exactly**,
/// plus a configured policy — so the one observable difference is the one fact that
/// changed, which is what makes it evidence rather than a second happy path. The reason
/// moves from `noConfiguredThreshold` to `firstPublish`: step 1 declined because the
/// tenant has a policy, and step 2 answered because the plan has never published.
///
/// **It does not become `AutoPublishable`, and that is not this test's failure.** Every
/// change set this unit may author is material for a rule above the threshold
/// comparison — `insert_prepared` refuses a draft row on an occupied key, so a revision
/// can add a price row and never edit one. The clause that used to close this sentence,
/// *"so repricing one needs the D-88 supersession unit that does not exist"*, was false:
/// that unit exists and is mounted, it stages a moved row on the plan's plane, and until
/// `materiality_of` narrowed to `unit_row_set` this route reached the `AutoPublishable`
/// arm through it — see
/// [`a_period_bound_published_beside_an_orphaned_successor_is_still_judged`]. What is
/// asserted *here* is that the policy is *read*, which nothing weaker than a change of
/// reason can distinguish from a policy being ignored.
#[tokio::test]
async fn a_configured_threshold_policy_changes_the_stored_materiality_reason() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 500)]).await;

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(body["materiality"]["material"], true);
    assert_eq!(
        body["materiality"]["reason"], "firstPublish",
        "the fail-safe declined because the tenant has a policy, and step 2 answered: {body}"
    );

    // The **stored** column, not only the response: it is what an auditor reads years
    // later, and it is the half a mislabelled body would not move. `only_unit` is not
    // usable here — the fixture's policy proposal is a unit of its own — so the record
    // is found by the kind this act opens.
    let opened_id = approval_rows(&h)
        .await
        .into_iter()
        .find(|row| row.subject_kind == "plan_revision")
        .map(|row| row.approval_id)
        .expect("the publish opened a plan-revision unit");
    let unit = approval_row(&h, opened_id).await;
    assert_eq!(
        unit.materiality["reason"], "firstPublish",
        "the stored verdict names the rule that fired"
    );
    assert_ne!(
        unit.materiality["reason"], "noConfiguredThreshold",
        "and no longer the fail-safe's, which is the whole of what wiring the policy changed"
    );
}

/// **D-188: a policy whose `effectiveFrom` has not arrived does not reach this route
/// at all.**
///
/// The fixture is the case above's *exactly*, one field moved: the same plan, the
/// same entries, the same two principals, and a start dated 2099 instead of one that
/// has passed. So the one observable difference is the one fact that changed, and the
/// reason goes back to `noConfiguredThreshold` — because between the approval and the
/// authored start the tenant's policy is the design set's **unset** state.
///
/// This is where the fail-open was worth closing. `effectiveFrom` was inside the pin
/// an approver signs and had no reader, so a reviewer who signed "these looser bars
/// start in 2099" was authorizing them for the next publish — the two-person rule
/// relaxed years before anybody agreed it should be. A comparison that only moved a
/// `GET`'s rendering would not have been worth the wave; this is the assertion that
/// says the enforcement moved with it.
#[tokio::test]
async fn a_threshold_policy_whose_start_is_ahead_does_not_govern_todays_publish() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    rest_support::approve_threshold_policy_from(&h, "2099-01-01T00:00:00Z", &[("EUR", 500)]).await;

    let response = publish_as(&h, SUBMITTER, plan_id, &seeded.etag()).await;

    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(
        body["materiality"]["reason"], "noConfiguredThreshold",
        "an approved version whose start has not arrived is not a configured policy: {body}"
    );

    let opened_id = approval_rows(&h)
        .await
        .into_iter()
        .find(|row| row.subject_kind == "plan_revision")
        .map(|row| row.approval_id)
        .expect("the publish opened a plan-revision unit");
    let unit = approval_row(&h, opened_id).await;
    assert_eq!(
        unit.materiality["reason"], "noConfiguredThreshold",
        "and the stored verdict says so too, which is what an auditor reads"
    );
}

// ---------------------------------------------------------------------------
// The `AutoPublishable` arm, and the one act that could reach it (D-200, D-319).
// ---------------------------------------------------------------------------

/// Publish `plan_id`, publish its one row, then submit a supersession that is
/// **material for the fail-safe** so it stages its successor as a `draft` on the
/// occupied key instead of committing it.
///
/// The supersession has to be submitted **before** any threshold policy is approved.
/// An auto-publishable supersession commits on one call and leaves nothing on the
/// plane — `rest_supersessions::an_auto_publishable_supersession_commits_on_one_call_and_says_so`
/// — and a staged row is the entire subject of both cases below.
///
/// Answers `(the staged successor's price id, the supersession unit's id)`.
async fn a_plan_with_a_staged_successor(h: &Harness, plan_id: Uuid) -> (String, Uuid) {
    let seeded = seed_publishable_plan(h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let staged = body_json(
        h.allowed_as(SUBMITTER)
            .send(rest_support::request(
                "POST",
                &format!("/bss-pricing/v1/plans/{plan_id}/supersessions"),
                Some(serde_json::json!({
                    "predecessor_price_id": seeded.price_id.to_string(),
                    "changeover": "2099-08-20T00:00:00Z",
                    "successor": {
                        "model_kind": "flat",
                        // 9 900 published, so a 200-minor move — under every bar
                        // these cases configure.
                        "amount_minor": 10_100,
                        "billing_timing": "advance",
                        "billing_anchor_policy": "calendar_month",
                        "proration_basis": "calendar_days_actual",
                        "credit_on_downgrade": false,
                        "rounding_policy_ref": "half_up"
                    },
                    "reason_code": "repricing"
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        staged["outcome"], "submitted_for_approval",
        "the successor has to be *staged* and not committed for either case to have a \
         subject: {staged}"
    );
    let successor_id = staged["successor_price_id"]
        .as_str()
        .expect("the staged successor's id")
        .to_owned();
    let unit: Uuid = staged["approval"]["approval_id"]
        .as_str()
        .expect("the supersession's unit id")
        .parse()
        .expect("a uuid");
    (successor_id, unit)
}

/// Author a €500-per-period minimum on the plan's own market and answer the tag the
/// publish needs. The `PATCH` opens the successor revision, so this **is** the
/// revision under test and its whole content is D-319's bound.
async fn a_revision_carrying_only_a_period_floor(h: &Harness, plan_id: Uuid) -> String {
    let etag = h.plan_etag(plan_id).await;
    let patched = body_json(
        h.allowed_as(SUBMITTER)
            .send(with_headers(
                "PATCH",
                &format!("/bss-pricing/v1/plans/{plan_id}"),
                Some(serde_json::json!({
                    "period_floor_caps": [
                        { "currency": "EUR", "region": "eu", "floor_minor": 50_000 }
                    ]
                })),
                &[("if-match", &etag)],
            ))
            .await,
    )
    .await;
    format!(
        "\"{}-{}\"",
        patched["revision"]
            .as_u64()
            .expect("the successor revision"),
        patched["row_version"].as_u64().expect("its version")
    )
}

/// **A plan revision whose whole content is a D-319 period floor is judged, and
/// before this it published on one principal.**
///
/// This is D-200's *wide-open* window, in its own words: the supersession unit is
/// rejected, *"the register releases the key and the staged draft survives"*. So no
/// pending unit holds anything, the plan is free to publish, and the only thing on
/// the plane that is not the published plan is a `draft` row belonging to nobody's
/// open act.
///
/// Measured through this router before `materiality_of` narrowed to
/// `infra::publish::unit_row_set`: `200 OK`, `outcome: "published"`, `approval:
/// null`, `materiality.material: false`, `published_price_ids: []`. A
/// €500-per-period minimum reached consumers on one principal, on a revision that
/// published no price row at all.
///
/// # Why the world has to be built exactly this way
///
/// Every step is load-bearing and each one is a guard that would otherwise answer
/// first:
///
/// 1. **The plan publishes first**, so `inst-mat-first` is spent and the revision
///    under test has a baseline. Without it the answer is `firstPublish`.
/// 2. **The supersession is submitted with no policy configured**, so it is material
///    for `noConfiguredThreshold` and stages rather than commits.
/// 3. **It is then rejected**, which frees the key `refuse_held_key` would otherwise
///    refuse the plan-revision submit on — the guard that made D-200 call the window
///    narrow, and the one the next case shows the auto-publish arm walked past.
/// 4. **The policy's bar is above the staged move** (200 minor against 1 000 000), so
///    nothing about that row is material on its own. A bar under it would answer
///    `thresholdReached` and this case would pass against the unfixed code for a
///    reason that has nothing to do with the plan's shape.
///
/// Only then is the plan's shape the sole thing that changed, which is what makes
/// `alwaysMaterialTrigger` here mean *D-115's whole-revision clause fired*.
///
/// # What it holds, and what it does not
///
/// It holds the **caller**: the evaluator's change set is this unit's rows. It does
/// not hold the **predicate** — `triggers::triggered_by_content` still answers `None`
/// for any plan-content change set carrying a moved row, and
/// `domain::materiality_tests::a_revision_that_moves_a_row_reaches_no_shape_trigger_however_its_shape_moved`
/// is where that limit is written down with the operand it would need.
#[tokio::test]
async fn a_period_bound_published_beside_an_orphaned_successor_is_still_judged() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (successor_id, unit) = a_plan_with_a_staged_successor(&h, plan_id).await;

    // (3) The unit goes away and its row does not — D-198's orphan, which is the
    // state D-200 calls wide open.
    reject_through_the_route(&h, unit).await;
    // (4) A bar the staged move is under.
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;

    let tag = a_revision_carrying_only_a_period_floor(&h, plan_id).await;
    let response = publish_as(&h, SUBMITTER, plan_id, &tag).await;

    assert_eq!(
        response.status(),
        axum::http::StatusCode::ACCEPTED,
        "a revision carrying a per-period minimum needs a second principal"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["outcome"], "submitted_for_approval",
        "and it was 200 `published` before the change set was narrowed: {body}"
    );
    assert_eq!(
        body["materiality"]["material"], true,
        "the pre-fix answer here was `false`: {body}"
    );
    assert_eq!(
        body["materiality"]["reason"], "alwaysMaterialTrigger",
        "D-115's whole-revision clause, which is what the bound is registered under: {body}"
    );
    assert_eq!(
        body["receipt"],
        serde_json::Value::Null,
        "nothing was frozen: {body}"
    );

    // **The control that the narrowing removed a judgement and not a row.** The
    // orphaned successor is still on the plane and still `draft`: excluding it from
    // the verdict must not exclude it from existing, or this case would be passing
    // because the fixture had quietly lost its subject.
    let rows = price_rows(&h, plan_id).await;
    let successor = rows
        .iter()
        .find(|record| record.price_id.to_string() == successor_id)
        .unwrap_or_else(|| panic!("the staged successor is still on the plane: {rows:?}"));
    assert_eq!(successor.lifecycle_state, LifecycleState::Draft);
}

/// **And while that supersession is still under review, the publish is refused —
/// which is the guard the auto-publishable arm used to walk straight past.**
///
/// D-200 called its own window narrow because *"`refuse_held_key` then refuses the
/// plan-revision submit outright"* while the supersession unit is `submitted`. It
/// does; the flaw is in the word **submit**. `publish_plan` calls
/// `ApprovalService::submit` only on the arm where the verdict is material, and the
/// arm the staged row created is the other one — it goes straight to `commit`, which
/// asks nothing about held keys. So the very row that made the verdict
/// `AutoPublishable` also carried the publish around the guard that was supposed to
/// make its window narrow, and the measured answer here was `200 OK`
/// `published` rather than `409`.
///
/// This is the pair to the case above and not a duplicate of it: that one asserts
/// what the verdict *is* when nothing blocks the publish, this one asserts that the
/// block is reached at all. A fix that only moved the verdict would leave this at
/// 200.
#[tokio::test]
async fn a_publish_beside_a_pending_supersession_reaches_the_held_key_guard() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (_successor_id, _unit) = a_plan_with_a_staged_successor(&h, plan_id).await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;

    let tag = a_revision_carrying_only_a_period_floor(&h, plan_id).await;
    let response = publish_as(&h, SUBMITTER, plan_id, &tag).await;

    assert_eq!(
        response.status(),
        axum::http::StatusCode::CONFLICT,
        "a key another unit is reviewing is not this revision's to publish over"
    );
    assert_eq!(
        problem_code(response).await,
        "PENDING_CHANGE_UNIT_EXISTS",
        "and by name, because the operator's next action is to decide or withdraw that unit"
    );

    // Nothing was frozen and nothing was opened: the refusal is ahead of both.
    assert_eq!(
        plan_state(&h, plan_id, 1).await.as_deref(),
        Some("draft"),
        "the revision carrying the bound is still a draft"
    );
    assert!(
        approval_rows(&h)
            .await
            .into_iter()
            .all(|row| row.subject_kind != "plan_revision"),
        "and no plan-revision unit was opened either"
    );
}
