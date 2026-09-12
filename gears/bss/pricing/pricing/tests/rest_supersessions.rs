//! `POST /bss-pricing/v1/plans/{planId}/supersessions` over the real router
//! (`inst-su-api`, `inst-su-return`, D-88).
//!
//! What this suite owns that `sqlite_supersession_unit.rs` cannot: the **wire**. The
//! service-level suite drives `SupersessionService` directly and can say nothing about
//! the status code, the `outcome` token, which half of the view is null on which arm, or
//! whether the `{planId}` segment is checked against the row it names.
//!
//! The gate itself is not asserted here. `rest_authz.rs`'s census carries this route, so
//! `every_route_asks_the_catalogued_pair`, `a_pdp_outage_fails_closed_on_every_route` and
//! the deny properties all range over it — one set of gate properties for every route
//! rather than a per-suite copy free to be the weaker one.
//!
//! The commit arms here really do commit: `Harness` supplies a working
//! `RegistryDouble`. That is worth stating because the *deployed* gear has no
//! `CatalogVersionRegistryV1` implementation anywhere in this repository — it boots with
//! `UnconfiguredCatalogVersionRegistryV1`, so a commit arm exercised against a running
//! stand answers **503** at the version request. Nothing in this file asserts anything
//! about that stand; the sentence is here so a reader does not take these 202s as
//! evidence about one.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::supersessions::PLAN_SUPERSESSIONS;
use bss_pricing::domain::approval::{DecisionBy, WithdrawAuthority};
use bss_pricing::infra::approval::{DecideRequest, RegionGrant};

use bss_pricing::domain::instant::utc_ymd_hms;
use rest_support::{Harness, Publishable, body_json, request, seed_publishable_plan};
use time::OffsetDateTime;
use uuid::Uuid;

const SUBMITTER: Uuid = Uuid::from_u128(0x_5c_11);
const REVIEWER: Uuid = Uuid::from_u128(0x_5c_22);

/// Inside `common`'s `[2099-08-04, 2099-09-01)` coverage window, and clear of both of
/// `inst-su-instant`'s floors against a wall clock that is nowhere near 2099.
fn changeover() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

fn path(plan_id: Uuid) -> String {
    PLAN_SUPERSESSIONS.replace("{planId}", &plan_id.to_string())
}

fn supersede_body(predecessor: Uuid, amount: i64) -> serde_json::Value {
    serde_json::json!({
        "predecessor_price_id": predecessor.to_string(),
        "changeover": changeover()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339"),
        "successor": {
            "model_kind": "flat",
            "amount_minor": amount,
            "billing_timing": "advance",
            // The predecessor's proration contract, restated. Without it the
            // successor drops the whole contract, which Slice 6's
            // `contract_change` arm correctly reports as a material change --
            // so this body would be testing "amount and contract moved" while
            // its name claims only the amount did. The auto-publishable case
            // needs a successor that moved exactly one thing.
            "billing_anchor_policy": "calendar_month",
            "proration_basis": "calendar_days_actual",
            "credit_on_downgrade": false,
            "rounding_policy_ref": "half_up"
        },
        "reason_code": "repricing"
    })
}

/// A published plan with one published row on the `eu` key.
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
                    // `OffsetDateTime::now_utc()` here put a 2026 stamp inside the sequence
                    // `changeover()`'s own doc places entirely in 2099, and nothing
                    // could assert the decision instant.
                    recorded_at: changeover() - time::Duration::days(1),
                    correlation_id: Uuid::from_u128(0x_5c_c0),
                },
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the reviewer approves");
}

#[tokio::test]
async fn the_controlled_arm_answers_202_and_names_the_staged_successor() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 12_000)),
        ))
        .await;
    // 202 on the arm that wrote no publish, for `inst-su-return`'s reason and the
    // window surface's: a publish unit is not consumer-visible until warm, so a 200
    // would claim the price changed for readers.
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let view = body_json(response).await;
    assert_eq!(view["outcome"], "submitted_for_approval");
    assert_eq!(view["predecessor_price_id"], seeded.price_id.to_string());
    // The staged draft is named on this arm too — it is the reviewer's subject, so
    // there is nothing to review until it exists.
    assert!(view["successor_price_id"].is_string());
    assert!(
        view["approval"]["approval_id"].is_string(),
        "the unit the call opened: {view}"
    );
    assert!(view["materiality"]["reason"].is_string());
    // And the halves that describe a commit are absent rather than zeroed.
    assert!(view["successor_window_id"].is_null());
    assert!(view["pending_version_ref"].is_null());
    assert!(view["shortened_mutation_seq"].is_null());
    // The window whose end *would* move is named, so a reviewer is shown the act
    // rather than the world it leaves.
    assert_eq!(
        view["shortened_window_id"],
        common::coverage_window_id(seeded.price_id).to_string()
    );
}

#[tokio::test]
async fn the_same_act_twice_over_the_wire_is_one_unit() {
    // `inst-su-api`'s idempotency, and this suite is where it is visible as a wire
    // property: there is **no `Idempotency-Key` header** — S5's column for this surface
    // is the act's own identity — so two identical requests must answer the same unit
    // rather than 409.
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    let first = body_json(
        h.allowed_as(SUBMITTER)
            .send(request(
                "POST",
                &path(plan_id),
                Some(supersede_body(seeded.price_id, 12_000)),
            ))
            .await,
    )
    .await;
    let second_response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 12_000)),
        ))
        .await;
    assert_eq!(second_response.status(), StatusCode::ACCEPTED);
    let second = body_json(second_response).await;
    assert_eq!(
        second["approval"]["approval_id"], first["approval"]["approval_id"],
        "one act, one unit"
    );
    assert_eq!(second["successor_price_id"], first["successor_price_id"]);
}

#[tokio::test]
async fn the_same_act_with_different_content_is_409() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    h.allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 12_000)),
        ))
        .await;
    let refused = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 13_000)),
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    // **The code *and* the sentence.** `DUPLICATE_SCOPE_KEY` has two producers reachable
    // from here — this guard and the store's own `duplicate_key` — and the service-level
    // twin exists precisely because the store cannot write the sentence. A code-only
    // assertion here would stay green with the guard deleted and the staging re-attempted
    // (review, 2026-08-06).
    let problem = body_json(refused).await;
    assert_eq!(rest_support::code_in(&problem), "DUPLICATE_SCOPE_KEY");
    assert!(
        problem.to_string().contains("different content"),
        "the guard's own sentence, not the store's: {problem}"
    );
}

#[tokio::test]
async fn the_call_after_an_independent_approve_commits_and_answers_the_pending_handle() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    let opened = body_json(
        h.allowed_as(SUBMITTER)
            .send(request(
                "POST",
                &path(plan_id),
                Some(supersede_body(seeded.price_id, 12_000)),
            ))
            .await,
    )
    .await;
    let unit: Uuid = opened["approval"]["approval_id"]
        .as_str()
        .expect("the unit id")
        .parse()
        .expect("a uuid");
    approve(&h, unit).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 12_000)),
        ))
        .await;
    // Still **202**, and that is the point of the token rather than the status: a
    // committed publish unit is not consumer-visible either.
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let view = body_json(response).await;
    assert_eq!(view["outcome"], "superseded");
    assert_eq!(
        view["successor_price_id"], opened["successor_price_id"],
        "the row the first call staged, never one this request minted"
    );
    assert!(view["pending_version_ref"].is_string());
    assert!(view["successor_window_id"].is_string());
    assert!(view["shortened_mutation_seq"].is_number());
    // **`approval` names the unit this commit ran under**, which is the discriminator
    // between the two committing arms; `materiality` is null because this call evaluated
    // nothing — the unit's stored verdict is the authority and rides `approval`.
    assert_eq!(
        view["approval"]["approval_id"],
        serde_json::json!(unit.to_string())
    );
    assert!(view["materiality"].is_null());
    // And the record is where the verdict lives, which is the half `materiality`
    // being null leaves unsaid: a client that stops at the null field cannot tell
    // "nothing was evaluated" from "nothing was material".
    assert_eq!(
        view["approval"]["materiality"]["material"],
        serde_json::json!(true),
        "the record says what made the act material: {view}"
    );
}

#[tokio::test]
async fn a_row_of_another_plan_answers_404() {
    // The `{planId}` segment is checked against the row's own plan, which is
    // `patch_price`'s decision for the same shape: a 400 would confirm the row exists
    // somewhere else.
    let h = Harness::new().await;
    let (_, seeded) = published(&h).await;
    let (other_plan, _) = published(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(other_plan),
            Some(supersede_body(seeded.price_id, 12_000)),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_predecessor_answers_404() {
    let h = Harness::new().await;
    let (plan_id, _) = published(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(Uuid::now_v7(), 12_000)),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_dormant_changeover_is_400_carrying_its_code() {
    // An architectural refusal reaching the wire, which is the property this suite owns:
    // the design set's 422s have no canonical category, so they arrive as **400 carrying
    // the code** (Foundation section 3.3), and `module_test::no_operation_declares_a_422`
    // is that rule's other half.
    //
    // The refusal staged is **dormancy** and not the changeover floor, and that is a
    // limit of the wire rather than a preference. The route stamps `OffsetDateTime::now_utc()`, so an
    // instant inside `MAX_BATCHING_DELAY` of it is a 2026 instant, and the fixtures'
    // coverage window is in 2099 — `plan_supersession` answers the plane before the
    // commit floor is ever asked, so the floor is unreachable from here. It is pinned at
    // the service level, where the stamp is a constant
    // (`sqlite_supersession_unit::the_commit_floor_is_answered_before_any_catalog_version_is_requested`),
    // and the first version of this case asserted the floor and was answered by
    // dormancy — then, one fix earlier, by `TIMESTAMP_PRECISION_EXCEEDED`, because
    // `OffsetDateTime::now_utc()` carries microseconds and D-144's quantum is checked first.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published(&h).await;

    let mut body = supersede_body(seeded.price_id, 10_000);
    body["changeover"] = serde_json::json!(
        (common::coverage_to() + time::Duration::days(30))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339")
    );
    let response = h
        .allowed_as(SUBMITTER)
        .send(request("POST", &path(plan_id), Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // **The code identifies the guard, which is what D-146's narrowing buys.** Under
    // `LIFECYCLE_FORBIDDEN` it could not: that code has several producers reachable
    // from this route — `compose_windows`' arms and the `RepoError` variants sharing
    // one mapping arm — so a case had to grep the rendered message for "dormant" to
    // tell them apart, which is a consumer parsing prose for a discriminator.
    let problem = body_json(response).await;
    assert_eq!(
        rest_support::code_in(&problem),
        "SUPERSESSION_KEY_DORMANT",
        "{problem}"
    );
}

#[tokio::test]
async fn an_auto_publishable_supersession_commits_on_one_call_and_says_so() {
    // The wire's negative control, and the arm `SupersessionOutcomeView::materiality`'s
    // doc makes a specific claim about. Every other committing case here reaches the
    // commit through an approve, so without this one the surface could refuse every
    // single-call commit and the suite would not notice.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 10_000)),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let view = body_json(response).await;
    assert_eq!(view["outcome"], "superseded");
    // **`approval` is the discriminator**, which is `api::rest::publish`'s decision: an
    // auto-publishable commit and a commit under an approval are otherwise the same
    // document, and a client has to be able to record which approval authorized a price
    // change it just made.
    assert!(
        view["approval"].is_null(),
        "no second principal was required: {view}"
    );
    assert_eq!(view["materiality"]["material"], serde_json::json!(false));
    assert!(view["successor_mutation_seq"].is_number());
}

#[tokio::test]
async fn a_caller_with_no_grant_is_denied_before_the_body_is_read() {
    // The gate runs before the parse, which is `api::rest::plans`' house rule. Before
    // this order a caller holding no grant at all was told their body was malformed, and
    // a PDP outage answered 400 rather than the fail-closed 503 — so the outage property
    // `rest_authz.rs` asserts held only for callers who sent parseable bodies.
    let h = Harness::new().await;
    let (plan_id, _) = published(&h).await;

    let response = h
        .denied()
        .send(request(
            "POST",
            &path(plan_id),
            Some(serde_json::json!({ "not": "a supersession" })),
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
/// another tenant's successor.
#[tokio::test]
async fn a_foreign_tenant_cannot_supersede_this_tenants_plan() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published(&h).await;

    rest_support::foreign_is_indistinguishable(
        &h,
        request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 12_000)),
        ),
        request(
            "POST",
            &path(Uuid::now_v7()),
            Some(supersede_body(seeded.price_id, 12_000)),
        ),
    )
    .await;

    // The control, and it is what makes the two refusals mean anything. Last,
    // because it stages a successor and opens a unit.
    let owner = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &path(plan_id),
            Some(supersede_body(seeded.price_id, 12_000)),
        ))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::ACCEPTED,
        "the owner's identical supersession must be accepted, or the refusals above are about the \
         request rather than about the tenant"
    );
}
