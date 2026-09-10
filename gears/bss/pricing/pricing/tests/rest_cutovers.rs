//! `POST /bss-pricing/v1/plans/{planId}/cutovers` over the real router
//! (`inst-gc-api`, `inst-gc-return`).
//!
//! `rest_supersessions.rs`'s sibling, and what it asserts is what **that** suite
//! cannot: the shape of a cutover's `202`. The unit's own behaviour — two keys, two
//! staged rows, the fixed verdict, the idempotent replay, the announcement — is
//! pinned one layer down in `sqlite_cutover_unit.rs`, and repeating it here would be
//! two descriptions of one act, free to disagree.
//!
//! So the cases below are about **the wire**: the two arms and their tokens, the
//! fields that are absent rather than zeroed on the controlled one, the generation
//! key rendered at full arity, and the two refusals a caller can provoke from the
//! path alone.
//!
//! Every instant is fixed, inside `common`'s `[2099-08-04, 2099-09-01)` coverage
//! window and clear of both of `inst-gc-compose`'s floors against a wall clock that
//! is nowhere near 2099.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::cutovers::PLAN_CUTOVERS;
use bss_pricing::domain::approval::{DecisionBy, WithdrawAuthority};
use bss_pricing::infra::approval::{DecideRequest, RegionGrant};

use bss_pricing::domain::instant::utc_ymd_hms;
use rest_support::{
    Harness, Publishable, audit_rows, body_json, problem_code, request, seed_publishable_plan,
};
use time::OffsetDateTime;
use uuid::Uuid;

const SUBMITTER: Uuid = Uuid::from_u128(0x_5d_11);
const REVIEWER: Uuid = Uuid::from_u128(0x_5d_22);

fn cutover_at() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

fn path(plan_id: Uuid) -> String {
    PLAN_CUTOVERS.replace("{planId}", &plan_id.to_string())
}

fn cutover_body(predecessor: Uuid, amount: i64) -> serde_json::Value {
    serde_json::json!({
        "predecessor_price_id": predecessor.to_string(),
        "cutover_at": cutover_at()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339"),
        "successor": {
            "model_kind": "flat",
            "amount_minor": amount,
            "billing_timing": "advance",
            // `rest_supersessions`' body carries these three and this one did
            // not, which was invisible until D-344 gave the cutover an aggregate
            // pass: `inst-pi-required` obliges a recurring row to publish
            // `billingAnchorPolicy`, `prorationBasis` and `creditOnDowngrade`,
            // and this successor carried none of them. It published anyway, so
            // the plan ended up holding a row Subscriptions cannot prorate and
            // the catalog substitutes no defaults for. The fixture was authoring
            // an unpublishable successor, not the rule refusing a legal one.
            "billing_anchor_policy": "calendar_month",
            "proration_basis": "calendar_days_actual",
            "credit_on_downgrade": false,
            "rounding_policy_ref": "half_up"
        },
        "reason_code": "grandfatheringCutover"
    })
}

async fn published(h: &Harness) -> (Uuid, Publishable) {
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    (plan_id, seeded)
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
                    // A fixed instant, not the wall clock. This suite's whole
                    // premise is that every instant sits in 2099, and the approve is
                    // the step that makes the commit arm reachable - so a
                    // `OffsetDateTime::now_utc()` here put a 2026 stamp inside the sequence the
                    // module doc says is entirely in 2099, and nothing could assert
                    // the decision instant.
                    recorded_at: cutover_at() - time::Duration::days(1),
                    correlation_id: Uuid::from_u128(0x_5d_c0),
                },
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the reviewer approves");
}

#[tokio::test]
async fn the_controlled_arm_answers_202_and_names_both_staged_rows() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(cutover_body(seeded.price_id, 12_000)),
        ))
        .await;

    // 202 on the arm that published nothing, for `inst-gc-return`'s reason and the
    // supersession's: a publish unit is not consumer-visible until warm, so a 200
    // would claim the price changed for readers.
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let view = body_json(response).await;
    assert_eq!(view["outcome"], "submitted_for_approval");
    assert_eq!(view["predecessor_price_id"], seeded.price_id.to_string());

    // **Both** drafts are named here, which is the whole difference from a
    // supersession's answer: each is the reviewer's subject and neither exists to be
    // reviewed until it is staged.
    assert!(view["successor_price_id"].is_string(), "{view}");
    assert!(view["copy_price_id"].is_string(), "{view}");
    assert_ne!(view["successor_price_id"], view["copy_price_id"]);
    assert!(view["approval"]["approval_id"].is_string(), "{view}");

    // The halves that describe a commit are **absent rather than zeroed** — a `null`
    // says the act did not happen, where a zero or an empty string would name a row
    // that does not exist.
    assert!(view["shortened_window_id"].is_null());
    assert!(view["pending_version_ref"].is_null());
}

#[tokio::test]
async fn the_answer_renders_the_generation_key_at_full_arity() {
    // **The field a caller most needs from the controlled arm.** The generation is
    // minted by the act rather than named by the request, so this string is the only
    // thing that tells a caller which key their retained subscribers will resolve to
    // — and it has to carry the whole canonical key, because that rendering is what
    // the approval register's held-key rows and the duplicate-key refusal both use.
    //
    // Ten segments since D-196: a rendering whose arity depended on the row would be
    // a parsing hazard in the three places this string is embedded rather than read.
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(cutover_body(seeded.price_id, 12_000)),
        ))
        .await;
    let view = body_json(response).await;

    let copy_key = view["copy_key"].as_str().expect("the generation is named");
    assert_eq!(
        copy_key.split('|').count(),
        10,
        "the canonical key is ten axes: {copy_key}"
    );
    assert!(
        copy_key.contains("existing_grandfathered"),
        "the copy stands on the grandfathered class: {copy_key}"
    );
    assert!(
        copy_key.starts_with(&plan_id.to_string()),
        "on this plan: {copy_key}"
    );
}

#[tokio::test]
async fn the_call_after_an_independent_approve_commits_and_answers_the_pending_handle() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    let opened = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(cutover_body(seeded.price_id, 12_000)),
        ))
        .await;
    let opened = body_json(opened).await;
    let approval_id = opened["approval"]["approval_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the unit is named");
    approve(&h, approval_id).await;

    let committed = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(cutover_body(seeded.price_id, 12_000)),
        ))
        .await;

    let status = committed.status();
    let view = body_json(committed).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{view}");
    assert_eq!(view["outcome"], "cut_over");
    // **The ids are the staged rows', not the ones this call minted**, which is the
    // property `CutoverRequest`'s doc argues and the only one a caller can check from
    // out here: the answer to the second call names what the first call staged.
    assert_eq!(view["successor_price_id"], opened["successor_price_id"]);
    assert_eq!(view["copy_price_id"], opened["copy_price_id"]);
    assert_eq!(view["copy_key"], opened["copy_key"]);
    // And the commit's own halves are present now.
    assert_eq!(
        view["shortened_window_id"],
        common::coverage_window_id(seeded.price_id).to_string()
    );
    assert!(
        view["pending_version_ref"].is_string(),
        "the 202 in a value — the registry handle this commit requested: {view}"
    );
    assert!(
        view["approval"].is_null(),
        "the committed arm carries no unit to decide: {view}"
    );

    // **The trail of the act that moves money**, and armed against the claim rather
    // than against "a row exists": what makes an audit record worth anything here is
    // `approval_ref`, the only join between the approved unit and the act it
    // authorized. Until 2026-08-11 this path wrote it nowhere — `cutover.rs` held no
    // occurrence of `audit_repo` at all, while seven sibling services append — so an
    // auditor holding a cutover could not establish that a second principal had
    // approved it. Asserting merely that the chain grew would have passed against
    // any of the three staging records this flow already writes.
    //
    // Matched on the **action as well as** the ref, and that conjunction is
    // load-bearing rather than tidy: `approval_repo::open` writes a `submit` record
    // carrying this same `approval_ref` when the unit is opened, so a finder keyed
    // on the ref alone matches that one and passes against a commit that records
    // nothing. It did exactly that on the first run of this probe.
    let audited = audit_rows(&h).await;
    let cutover_record = audited
        .iter()
        .find(|row| row.approval_ref == Some(approval_id) && row.action == "publish")
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
    // Both published rows are named. A state naming only the successor would leave
    // the grandfathered copy's transition unrecorded on the act that created it —
    // and the copy is the row the grandfathered subscriber is actually billed from.
    let after = cutover_record
        .after_state
        .as_ref()
        .expect("the record names what the act produced");
    assert_eq!(after["predecessorState"], "superseded");
    assert_eq!(after["successorState"], "published");
    assert_eq!(
        after["copyPriceId"], view["copy_price_id"],
        "the copy this act minted is the copy the record names: {after}"
    );
}

#[tokio::test]
async fn a_row_of_another_plan_answers_404() {
    // `patch_price`'s decision for the same shape, and the supersession's: the path
    // names a resource that does not exist **under that plan**, and a 400 would
    // confirm the row exists somewhere else.
    let h = Harness::new().await;
    let (_plan_id, seeded) = published(&h).await;
    let other_plan = Uuid::now_v7();
    seed_publishable_plan(&h, other_plan).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(other_plan),
            Some(cutover_body(seeded.price_id, 12_000)),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_dormant_instant_is_refused_by_its_own_code() {
    // `inst-co-shorten` presupposes coverage to shorten, and the cutover has a code
    // of its own for it where the supersession's identical refusal has none — one
    // fact, two spellings on the wire, which D-204 records as the design set's
    // asymmetry rather than this gear's.
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    let mut body = cutover_body(seeded.price_id, 12_000);
    // Past the fixture's coverage window entirely.
    body["cutover_at"] = serde_json::json!(
        utc_ymd_hms(2099, 12, 1, 0, 0, 0)
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339")
    );

    let response = h
        .allowed_as(SUBMITTER)
        .send(request("POST", &path(plan_id), Some(body)))
        .await;

    // The status was bound here and never asserted — used only inside a panic
    // message, so the refusal's whole wire shape rested on a substring search over
    // the rendered body, which a 200 carrying the token anywhere would satisfy.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "CUTOVER_GAP",
        "the refusal carries its own code rather than a generic one"
    );
}

#[tokio::test]
async fn a_caller_with_no_grant_is_denied_before_the_body_is_read() {
    // The gate runs above the parse, which is this gear's house rule and the reason
    // the module doc gives for it: before that order a caller holding no grant at all
    // was told their body was malformed, and a PDP outage answered 400 rather than
    // the fail-closed 503. The body here is **deliberately malformed** so that a
    // parse-first handler would answer 400 and this case would catch it.
    let h = Harness::new().await;
    let (plan_id, _seeded) = published(&h).await;

    let response = h
        .denied()
        .send(request(
            "POST",
            &path(plan_id),
            Some(serde_json::json!({ "not": "a cutover" })),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// **The SQL tenant predicate on this door.**
///
/// `rest_authz.rs`'s census cannot reach it: that seed leaves the plan a draft and
/// this route answers its **owner** a `404 current plan revision … not found`
/// there, so the row is listed in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE`. Here
/// [`published`] commits the revision, so the owner's identical call is accepted
/// and a refusal of the foreign caller means tenancy rather than a missing
/// subject.
///
/// `Harness::denied` drives a PDP that refuses everything and
/// `Harness::scope_mismatch` exercises `access_scope`'s write-target membership
/// assertion; neither hands a caller-supplied id of another tenant's row to a
/// repository, which is the only way the predicate is exercised at all. A handler
/// that resolved `{planId}` before narrowing would satisfy both of those and stage
/// a grandfathering copy of another tenant's row.
#[tokio::test]
async fn a_foreign_tenant_cannot_cut_over_this_tenants_plan() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    rest_support::foreign_is_indistinguishable(
        &h,
        request(
            "POST",
            &path(plan_id),
            Some(cutover_body(seeded.price_id, 12_000)),
        ),
        request(
            "POST",
            &path(Uuid::now_v7()),
            Some(cutover_body(seeded.price_id, 12_000)),
        ),
    )
    .await;

    // The control, and it is what makes the two refusals mean anything. Last,
    // because it stages both drafts and opens a unit.
    let owner = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(cutover_body(seeded.price_id, 12_000)),
        ))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::ACCEPTED,
        "the owner's identical cutover must be accepted, or the refusals above are about the \
         request rather than about the tenant"
    );
}
