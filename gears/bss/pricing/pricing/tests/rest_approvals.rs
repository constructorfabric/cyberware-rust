//! The approval surface, driven through the real router: the reviewer's queue,
//! the record D-61 binds, and the three decisions.
//!
//! # The positive control comes first, deliberately
//!
//! Five rules share one decision path and four of them can fire on a request
//! built carelessly, so every refusal below is staged as **one move away from**
//! [`an_independent_principal_approves_and_the_record_says_who`]. Without that
//! world, a service that refused everything would satisfy the whole file.
//!
//! # The publish route is what opens a unit, and this suite uses it
//!
//! There is no submit endpoint on the approval surface — §5's four rows are the
//! queue, the record and the three decisions — so a unit is opened the way an
//! operator opens one, by publishing a publishable plan. Reaching for
//! `ApprovalService::submit` instead would test the decisions against a record
//! no surface can produce.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

#[path = "rest_approvals/counts.rs"]
mod counts;

use bss_pricing::authz::{actions, labels};
use bss_pricing::config::JobsConfig;
use bss_pricing::domain::approval::ApprovalState;
use bss_pricing::domain::instant::format_rfc3339;
use bss_pricing::domain::window::WindowState;
use bss_pricing::infra::jobs::window_activation::WindowActivationJob;
use bss_pricing::infra::storage::repo::window_repo;

use account_management_sdk::{IdpUser, IdpUserFilterField, ListUsersQuery};
use async_trait::async_trait;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::infra::approval_participants::{ApprovalParticipants, ParticipantDirectory};
use parking_lot::Mutex;
use rest_support::{
    Harness, approval_row, approval_rows, audit_rows, body_json, problem_code, refused_by,
    seed_publishable_plan, with_headers,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_odata::filter::{FilterNode, ODataValue};
use toolkit_odata::{Page, PageInfo};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const SUBMITTER: Uuid = Uuid::from_u128(0x5_c0);
const APPROVER: Uuid = Uuid::from_u128(0xa_c0);

/// Caller, tenant and exact requested profile IDs at the AM boundary.
type ProfileCall = (Uuid, Uuid, Vec<Uuid>);

/// Simulates only AM's authorized profile response, not approval authorization.
#[derive(Default)]
struct Profiles {
    answers: Mutex<BTreeMap<Uuid, Result<IdpUser, CanonicalError>>>,
    calls: Mutex<Vec<ProfileCall>>,
}

#[async_trait]
impl ParticipantDirectory for Profiles {
    async fn list_users(
        &self,
        ctx: &SecurityContext,
        query: ListUsersQuery,
    ) -> Result<Page<IdpUser>, CanonicalError> {
        let Some(FilterNode::InList {
            field: IdpUserFilterField::Id,
            values,
        }) = query.filter
        else {
            panic!("an approval read must request exact IDs, never an unfiltered tenant");
        };
        let ids: Vec<_> = values
            .into_iter()
            .map(|value| match value {
                ODataValue::Uuid(id) => id,
                _ => panic!("typed UUID filter"),
            })
            .collect();
        self.calls
            .lock()
            .push((ctx.subject_id(), ctx.subject_tenant_id(), ids.clone()));
        let answers = self.answers.lock();
        let items = ids
            .iter()
            .filter_map(|id| answers.get(id).cloned())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: u64::from(query.pagination.top()),
            },
        ))
    }
}

/// The real routes with an observable profile-source boundary.
async fn named_harness() -> (Harness, Arc<Profiles>) {
    let mut h = Harness::new().await;
    let source = Arc::new(Profiles::default());
    source.answers.lock().insert(
        SUBMITTER,
        Ok(IdpUser::new(SUBMITTER, "submitter-login")
            .with_display_name("Alice Author")
            .with_email("hidden@example.test")),
    );
    source
        .answers
        .lock()
        .insert(APPROVER, Ok(IdpUser::new(APPROVER, "bob-reviewer")));
    Arc::make_mut(&mut h.governance).participants =
        ApprovalParticipants::with_directory(source.clone());
    (h, source)
}

/// A resource-scoped canonical AM failure without disclosing its detail in Pricing.
#[resource_error(gts_id!("cf.core.am.user.v1~"))]
struct ProfileResource;

#[tokio::test]
async fn participant_names_agree_on_list_and_detail_and_refresh_without_persisting() {
    let (h, source) = named_harness().await;
    let id = a_pending_unit(&h).await;
    let client = h.allowed_as(APPROVER);
    let decision = client
        .send(with_headers(
            "POST",
            &decision_path(id, "approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(decision.status(), axum::http::StatusCode::OK);
    let decision = body_json(decision).await;
    assert!(
        decision.get("submitter").is_none(),
        "mutation remains stored metadata only"
    );
    assert!(
        source.calls.lock().is_empty(),
        "submit and decide do not read profiles"
    );
    let path = format!("/bss-pricing/v1/approvals/{id}");
    let response = client.send(with_headers("GET", &path, None, &[])).await;
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let detail = body_json(response).await;
    assert_eq!(
        detail["approval"]["submitter"],
        serde_json::json!({
            "principal_id": SUBMITTER, "display_name": "Alice Author", "name_status": "resolved"
        })
    );
    assert_eq!(
        detail["approval"]["approver"]["display_name"],
        "bob-reviewer"
    );
    let response = client
        .send(with_headers("GET", "/bss-pricing/v1/approvals", None, &[]))
        .await;
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let list = body_json(response).await;
    assert_eq!(list["items"][0], detail["approval"]);
    assert_eq!(
        source.calls.lock().len(),
        2,
        "one batch of two principals per read, no cross-request cache"
    );
    assert!(
        source
            .calls
            .lock()
            .iter()
            .all(|(actor, tenant, _)| *actor == APPROVER && *tenant == h.tenant)
    );
    assert!(!detail.to_string().contains("hidden@example.test"));
    let audit_before = audit_rows(&h).await;
    source.answers.lock().insert(
        SUBMITTER,
        Ok(IdpUser::new(SUBMITTER, "submitter-login").with_display_name("Alice Renamed")),
    );
    let renamed = body_json(client.send(with_headers("GET", &path, None, &[])).await).await;
    assert_eq!(
        renamed["approval"]["submitter"]["display_name"],
        "Alice Renamed"
    );
    assert_eq!(
        renamed["approval"]["content_hash"],
        detail["approval"]["content_hash"]
    );
    assert_eq!(
        audit_rows(&h).await.len(),
        audit_before.len(),
        "read enrichment never writes audit"
    );
    let audit = format!("{audit_before:?}");
    assert!(!audit.contains("Alice Author") && !audit.contains("bob-reviewer"));
}

#[tokio::test]
async fn multiple_approval_rows_lookup_the_same_submitter_once() {
    let (h, source) = named_harness().await;
    a_pending_unit(&h).await;
    let second_plan = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, second_plan).await;
    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{second_plan}/publish"),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let list = body_json(
        h.allowed_as(APPROVER)
            .send(with_headers("GET", "/bss-pricing/v1/approvals", None, &[]))
            .await,
    )
    .await;
    let rows = list["items"].as_array().expect("approval page");
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row["submitter"]["display_name"] == "Alice Author"
                && row["approver"].is_null())
    );
    assert_eq!(
        *source.calls.lock(),
        [(APPROVER, h.tenant, vec![SUBMITTER])]
    );
}

#[tokio::test]
async fn unreadable_approval_and_empty_page_never_trigger_profile_lookup() {
    let (h, source) = named_harness().await;
    let id = a_pending_unit(&h).await;
    let path = format!("/bss-pricing/v1/approvals/{id}");
    for (client, expected) in [
        (h.denied(), axum::http::StatusCode::FORBIDDEN),
        (h.anonymous(), axum::http::StatusCode::UNAUTHORIZED),
        (h.scope_mismatch(), axum::http::StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            client
                .send(with_headers("GET", &path, None, &[]))
                .await
                .status(),
            expected
        );
    }
    let missing = format!("/bss-pricing/v1/approvals/{}", Uuid::now_v7());
    assert_eq!(
        h.allowed_as(APPROVER)
            .send(with_headers("GET", &missing, None, &[]))
            .await
            .status(),
        axum::http::StatusCode::NOT_FOUND
    );
    let response = h
        .scope_mismatch()
        .send(with_headers("GET", "/bss-pricing/v1/approvals", None, &[]))
        .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(body_json(response).await["items"], serde_json::json!([]));
    assert!(source.calls.lock().is_empty());
}

#[tokio::test]
async fn profile_failures_are_explicit_and_do_not_hide_an_authorized_approval() {
    let (h, source) = named_harness().await;
    let id = a_pending_unit(&h).await;
    let decision = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(id, "approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(decision.status(), axum::http::StatusCode::OK);
    let path = format!("/bss-pricing/v1/approvals/{id}");
    let cases = [
        (
            ProfileResource::permission_denied()
                .with_reason("PRIVATE_AM_DETAIL")
                .create(),
            "restricted",
        ),
        (
            ProfileResource::not_found("PRIVATE_AM_DETAIL")
                .with_resource("private-user")
                .create(),
            "not_found",
        ),
        (
            CanonicalError::service_unavailable()
                .with_detail("PRIVATE_AM_DETAIL")
                .create(),
            "unavailable",
        ),
        (
            CanonicalError::unauthenticated()
                .with_reason("PRIVATE_AM_DETAIL")
                .create(),
            "restricted",
        ),
        (
            ProfileResource::unimplemented("PRIVATE_AM_DETAIL").create(),
            "unavailable",
        ),
    ];
    for (error, status) in cases {
        source.answers.lock().insert(SUBMITTER, Err(error));
        let response = h
            .allowed_as(APPROVER)
            .send(with_headers("GET", &path, None, &[]))
            .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let detail = body_json(response).await;
        assert_eq!(detail["approval"]["submitter"]["name_status"], status);
        assert_eq!(detail["approval"]["approver"]["name_status"], status);
        assert!(detail["approval"]["approver"]["display_name"].is_null());
        assert!(detail["approval"]["submitter"]["display_name"].is_null());
        assert_eq!(
            detail["approval"]["submitter"]["principal_id"],
            SUBMITTER.to_string()
        );
        assert!(!detail.to_string().contains("PRIVATE_AM_DETAIL"));
        let list = body_json(
            h.allowed_as(APPROVER)
                .send(with_headers("GET", "/bss-pricing/v1/approvals", None, &[]))
                .await,
        )
        .await;
        assert_eq!(list["items"][0], detail["approval"]);
    }
}

/// A plan with one pending unit over it, opened through the publish route.
async fn a_pending_unit(h: &Harness) -> Uuid {
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(h, plan_id).await;
    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::ACCEPTED,
        "the submit must succeed for anything below to be about a decision"
    );
    let rows = approval_rows(h).await;
    assert_eq!(rows.len(), 1);
    rows[0].approval_id
}

fn decision_path(approval_id: Uuid, action: &str) -> String {
    format!("/bss-pricing/v1/approvals/{approval_id}/{action}")
}

// ---------------------------------------------------------------------------
// The decisions.
// ---------------------------------------------------------------------------

/// **The positive control**, and `inst-tp-record`: both identities and both
/// timestamps land on the record.
#[tokio::test]
async fn an_independent_principal_approves_and_the_record_says_who() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "approve"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["state"], "approved");
    assert_eq!(body["submitter_principal"], SUBMITTER.to_string());
    assert_eq!(body["approver_principal"], APPROVER.to_string());
    assert!(body["decided_at"].is_string());

    let record = approval_row(&h, approval_id).await;
    assert_eq!(record.state, ApprovalState::Approved);
    assert_eq!(record.approver_principal, Some(APPROVER));
}

/// `inst-tp-distinct` and `inst-tp-selfaudit`: identity, not role — and the
/// attempt is **written to the audit log**, which is the half a 403 alone does
/// not prove.
#[tokio::test]
async fn the_submitter_cannot_approve_their_own_unit_and_the_attempt_is_recorded() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;
    let before = audit_rows(&h).await.len();

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "approve"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(problem_code(response).await, "SELF_APPROVAL_FORBIDDEN");

    let after = audit_rows(&h).await;
    assert_eq!(after.len(), before + 1, "the attempt lands one record");
    let denial = after.last().expect("the record");
    assert_eq!(denial.action, "deny");
    assert_eq!(denial.actor_principal_id, SUBMITTER);
    assert_eq!(denial.approval_ref, Some(approval_id));
    assert_eq!(
        denial.after_state.as_ref().expect("the attempt")["refusedWith"],
        "SELF_APPROVAL_FORBIDDEN"
    );

    // And the unit is untouched: a refused decision leaves the store as it found
    // it.
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Submitted
    );
}

/// `inst-as-reject`: the reason is mandatory, and blank is absent.
#[tokio::test]
async fn a_reject_carries_its_reason_and_a_blank_one_is_refused() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let blank = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "reject"),
            Some(serde_json::json!({ "reason": "   " })),
            &[],
        ))
        .await;
    assert_eq!(blank.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(blank).await, "REASON_REQUIRED");
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Submitted,
        "a refused decision writes nothing"
    );

    let given = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "reject"),
            Some(serde_json::json!({ "reason": "the EU uplift is not signed off" })),
            &[],
        ))
        .await;
    assert_eq!(given.status(), axum::http::StatusCode::OK);
    let record = approval_row(&h, approval_id).await;
    assert_eq!(record.state, ApprovalState::Rejected);
    assert_eq!(
        record.reason.as_deref(),
        Some("the EU uplift is not signed off")
    );
}

/// `inst-as-immutable`: a second decision on a decided record is a conflict, and
/// the store's compare-and-swap is what answers it — which is why these routes
/// declare no `If-Match`.
#[tokio::test]
async fn a_decided_record_cannot_be_decided_again() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;
    let first = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(first.status(), axum::http::StatusCode::OK);

    for action in ["approve", "reject", "withdraw"] {
        let body = (action == "reject").then(|| serde_json::json!({ "reason": "changed my mind" }));
        let again = h
            .allowed_as(APPROVER)
            .send(with_headers(
                "POST",
                &decision_path(approval_id, action),
                body,
                &[],
            ))
            .await;
        assert_eq!(
            again.status(),
            axum::http::StatusCode::CONFLICT,
            "{action} on a decided record"
        );
        assert_eq!(problem_code(again).await, "APPROVAL_NOT_PENDING");
    }
}

/// The withdraw is exempt from the two-person rule, because its actor **is** the
/// submitter (`inst-as-void`).
///
/// This is the case that used to answer 500: the decider was written to
/// `approver_principal`, and `chk_pricing_approval_distinct_principals` refuses a
/// submitter's own id there.
#[tokio::test]
async fn the_submitter_may_withdraw_their_own_unit() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "withdraw"),
            Some(serde_json::json!({ "reason": "superseded by next quarter's book" })),
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let record = approval_row(&h, approval_id).await;
    assert_eq!(record.state, ApprovalState::Voided);
    assert_eq!(
        record.approver_principal, None,
        "a withdraw exercises no review authority"
    );
    assert_eq!(
        record.reason.as_deref(),
        Some("superseded by next quarter's book")
    );
}

/// **`WITHDRAW_FORBIDDEN`, over the wire, for the first time.**
///
/// A second principal who holds the withdraw route's gate (`approval x approve`)
/// but no catalog authority (`plan x publish`) may not close a unit that is not
/// theirs.
///
/// The code had unit coverage and **appeared in zero file under `tests/`** until
/// 2026-08-18 (review Z3-10), which is the one gap that matters most on this
/// surface: a rule whose whole point is *who* may act, tested only at the
/// pure-function layer, cannot see whether the surface transports the right
/// authority. `authorize_decision` takes `WithdrawAuthority` as a parameter and
/// the route is what establishes it — from a second, tenant-wide PDP question
/// that a unit test can neither ask nor get wrong.
///
/// **`selectively_allowed_as` rather than `allowed_as`, and that is the whole
/// fixture.** `allowed_as` grants every pair uniformly, so the authority question
/// answers `CatalogAuthority` and this refusal is unreachable — the suite would be
/// green and would be testing nothing. The pair granted here is exactly the gate,
/// so a 403 cannot be the gate answering.
///
/// A withdraw is not cosmetic: it moves the unit out of `submitted`, which
/// releases the canonical scope keys it held and re-opens them to whoever wants
/// them.
#[tokio::test]
async fn a_foreign_principal_without_catalog_authority_may_not_withdraw_the_unit() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let response = h
        .selectively_allowed_as(APPROVER, &[(labels::APPROVAL, actions::APPROVE)])
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "withdraw"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(problem_code(response).await, "WITHDRAW_FORBIDDEN");
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Submitted,
        "a refused withdraw leaves the unit open and its scope keys held"
    );
}

/// **The positive control for the case above**, and it is what makes that case
/// about *authority* rather than about the selective fixture refusing everything.
///
/// The same foreign principal, the same route, one pair more: `plan x publish` is
/// the expressible proxy for `inst-as-void`'s `CatalogAdmin`, and with it the
/// withdraw succeeds.
///
/// It also pins the residue `WithdrawAuthority`'s doc reports — the proxy is
/// coarser than the role, so a `FinanceManager` can close a `CatalogAdmin`'s unit
/// — as a fact of this build rather than as a sentence. If the design set ever
/// reconciles `inst-as-void`'s identity rule with the `approval x approve` gate
/// the endpoint map assigns this route, this is the test that has to move.
#[tokio::test]
async fn a_foreign_principal_with_catalog_authority_may_withdraw_the_unit() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let response = h
        .selectively_allowed_as(
            APPROVER,
            &[
                (labels::APPROVAL, actions::APPROVE),
                (labels::PLAN, actions::PUBLISH),
            ],
        )
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "withdraw"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let record = approval_row(&h, approval_id).await;
    assert_eq!(record.state, ApprovalState::Voided);
    assert_eq!(
        record.approver_principal, None,
        "a withdraw exercises no review authority, whoever performs it"
    );
}

/// A withdraw with no body at all is the ordinary case and is accepted.
#[tokio::test]
async fn a_withdraw_needs_no_body() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let response = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "withdraw"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Voided
    );
}

/// A body that is present must still be well-formed — and the refusal is a 400,
/// never axum's 422.
#[tokio::test]
async fn a_malformed_decision_body_is_four_hundred_and_never_four_twenty_two() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "reject"),
            Some(serde_json::json!({ "reason": 7 })),
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    // The status alone tells a 422 from a 400 and nothing else. The refusal that
    // shares this door's 400 is `REASON_REQUIRED`, which is
    // `failed_precondition` — a sibling in this file asserts it at 400 — so the
    // family plus the deserializer's own sentence is what says the body never
    // reached a rule, which is the half of the name that is not about `422`.
    // (The state-machine and stale-pin refusals share neither: both go through
    // `error_mapping`'s `aborted` and answer 409.)
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "the request body is not readable",
    );
}

/// A record outside the caller's scope is absent, and a decision on it decides
/// nothing.
///
/// **The SQL tenant predicate on the approve.** `rest_authz.rs`'s census cannot
/// reach it: the unit its seed leaves carries a pin that no longer matches its
/// subject, so the **owner** is answered `APPROVAL_CONTENT_MISMATCH` there and the
/// route is listed in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE` — which is why
/// `reject` and `withdraw`, exempt from the pin, are under that property and this
/// verb is not. Here [`a_pending_unit`] opens the unit through the publish route,
/// so the pin matches, the owner's identical call is accepted, and a refusal of the
/// foreign caller means tenancy rather than the pin.
///
/// `Harness::denied` drives a PDP that refuses everything and
/// `Harness::scope_mismatch` exercises `access_scope`'s write-target membership
/// assertion; neither hands a caller-supplied id of another tenant's row to a
/// repository. A handler that resolved `{approvalId}` before narrowing would
/// satisfy both of those and decide another tenant's unit — the two-person rule
/// spent by a principal the tenant never granted anything.
#[tokio::test]
async fn a_foreign_tenants_unit_cannot_be_decided_or_read() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let read = h
        .other_tenant()
        .send(with_headers(
            "GET",
            &format!("/bss-pricing/v1/approvals/{approval_id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(read.status(), axum::http::StatusCode::NOT_FOUND);

    let approve = |id: Uuid| with_headers("POST", &decision_path(id, "approve"), None, &[]);
    rest_support::foreign_is_indistinguishable(&h, approve(approval_id), approve(Uuid::now_v7()))
        .await;
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Submitted
    );

    // The control, and it is what makes the refusals mean anything. Last, because
    // it decides the unit.
    let owner = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        owner.status(),
        axum::http::StatusCode::OK,
        "an independent principal of the unit's own tenant must be able to approve it, or the \
         refusals above are about the request rather than about the tenant"
    );
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Approved
    );
}

#[tokio::test]
async fn a_decision_on_an_unknown_id_is_absent() {
    let h = Harness::new().await;

    let absent = Uuid::now_v7();
    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(absent, "approve"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    // This door has exactly **one** 404, and that is the thing worth pinning:
    // `re_derive` folds an absent plan to `Ok(None)` rather than reporting it,
    // for the reason its own doc gives — a 404 there would tell a reviewer their
    // approval record is missing while it sits in front of them — so the absent
    // plan answers `APPROVAL_CONTENT_MISMATCH` at 409 instead. A 404 naming
    // anything but the approval would therefore be the fold coming undone.
    //
    // No code to read: the canonical not-found family carries no `reason` slot
    // and `context.resource_name` holds the id, so the sentence is what says it.
    let body = body_json(response).await;
    assert!(
        body["type"]
            .as_str()
            .unwrap_or_default()
            .contains("cf.core.err.not_found."),
        "{body}"
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert_eq!(
        detail,
        format!("approval {absent} not found"),
        "the absent subject is the approval, and it is the one the caller named: {body}"
    );
}

// ---------------------------------------------------------------------------
// The reads, and D-61.
// ---------------------------------------------------------------------------

/// **D-61's reviewability invariant.** The record's `GET` returns the pinned
/// content, not the hash alone — without it the two-person rule is a hash-blind
/// signature.
#[tokio::test]
async fn the_record_carries_the_content_its_pin_covers() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    let submitted = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(submitted.status(), axum::http::StatusCode::ACCEPTED);
    let approval_id = approval_rows(&h).await[0].approval_id;

    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "GET",
            &format!("/bss-pricing/v1/approvals/{approval_id}"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;

    // The hash is still there — an auditor recomputes it — and it is 64 hex
    // characters rather than an array of numbers.
    let hash = body["approval"]["content_hash"]
        .as_str()
        .expect("the pin renders as hex");
    assert_eq!(hash.len(), 64);

    // And the content. This is the invariant: a reviewer who cannot read the
    // plan resource can still read what they are approving.
    let pinned = &body["pinned_content"];
    assert!(!pinned.is_null(), "the pinned content must be returned");
    assert_eq!(pinned["plan_id"], plan_id.to_string());
    assert_eq!(pinned["revision"], seeded.revision);
    assert_eq!(pinned["plan_tier"], "gold");
    assert_eq!(pinned["billing_cycle"], "recurring");
    assert_eq!(pinned["rows"][0]["price_id"], seeded.price_id.to_string());
    assert_eq!(pinned["rows"][0]["content"]["amount_minor"], 9_900);
    assert_eq!(pinned["rows"][0]["scope_key"]["region"], "eu");
    assert_eq!(pinned["phases"][0]["kind"], "evergreen");
    assert_eq!(pinned["descriptor_set"]["gl_code"], "4000");
    // The window plane, over the wire. The pin frames it, so D-61 says the
    // document has to carry it — and the interval rendering is `GET …/coverage`'s
    // own `WindowIntervalView`, so an operator and a reviewer read one spelling.
    let (from_y, from_m, from_d) = common::COVERAGE_FROM_UTC;
    let (to_y, to_m, to_d) = common::COVERAGE_TO_UTC;
    assert_eq!(
        pinned["windows"],
        serde_json::json!([{
            "scope_key": pinned["rows"][0]["scope_key"],
            "intervals": [{
                // The gear's renderer, not `time`'s: an expectation that spells
                // the instant its own way asserts a form the gear may not emit.
                "effective_from": format_rfc3339(utc_ymd_hms(from_y, from_m, from_d, 0, 0, 0)),
                "effective_to": format_rfc3339(utc_ymd_hms(to_y, to_m, to_d, 0, 0, 0)),
                "state": "scheduled",
            }],
        }]),
        "the seed's coverage window, filed under the row's own key: {pinned}"
    );
    assert_eq!(body["content_matches_pin"], true);
}

/// **The clock crosses a window boundary under a pending unit, and the decision
/// still lands.** D-99's paired clarification, executed at the approval seam.
///
/// The world nothing in this crate stood in before: `inst-ws-future-start` (D-63)
/// forces every scheduled window's start strictly into the future and
/// `inst-wc-required` refuses a billable row with no window, so **every** pending
/// unit whose pin covers the window plane has an activation boundary ahead of it.
/// The boundary arrives; `WindowActivationJob` flips the row on its sixty-second
/// tick, which by D-99 is deliberately *not* a publish unit and re-projects nothing;
/// then the reviewer decides, and `judge` re-derives the shape through the assembler
/// — which reads the window state **live from the store**.
///
/// What the design set obliges is therefore that the decision is unaffected: the
/// interval is what a version freezes (D-99, D-121), the transition is the clock's,
/// and *"the time-driven transitions change nothing projected"*. Framing the state
/// verbatim in the content pin broke exactly that, and broke it without remedy —
/// `APPROVAL_CONTENT_MISMATCH`'s message asks for a re-submit and a second review,
/// and no author can put the clock back. `content_pin.rs`'s
/// `the_clock_may_flip_a_window_but_not_the_pin` states the property over the
/// digest; this is the only test that walks the whole path that consumes it.
///
/// # The premise this test used to record, and why it no longer holds
///
/// It used to assert `report.activated == 0`, and to record the reason: the sweep's
/// `window_repo::list_due` restricts to `PROJECTED_ROW_STATES`
/// (`price_id IN (published, superseded)`), a pending unit was necessarily a
/// **plan-revision** unit, `infra::publish::assemble` needs an open draft revision,
/// and `plan_repo::create_draft_on` mints only revision 0 — so a plan holding a
/// pending unit held no published row and the sweep could not reach its window. The
/// defect was **latent**, and the test said so, naming what would end that: *"the
/// moment a window mutation becomes a publish unit over an already-published plan
/// (D-99, G4)"*.
///
/// That moment has arrived, so the premise is replaced rather than re-argued. A
/// **window-mutation unit** (`inst-co-single-pending`'s D-62/D-99 unit, opened by
/// `ApprovalService::submit_window_mutation`) pins the plan shape of a plan whose
/// revision *and* price row are **published** — so `list_due` reaches the window, the
/// sweep is what flips it, and the flip happens inside the pin. The `activated`
/// assertion is therefore **`1`**, and it is the load-bearing one: were it still 0,
/// this test would pass without the interleaving it is named for ever occurring.
///
/// The flip is asserted on the **truth row** as well as on the counter, because a
/// counter alone cannot say *which* window moved.
#[tokio::test]
async fn an_activation_under_a_pending_unit_does_not_void_the_approval() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    // Published for real on both planes: the revision so the window unit has a
    // current revision to pin, and the price row so the sweep's projected-state
    // filter can see its window.
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let window_id = common::coverage_window_id(seeded.price_id);
    let approval_id = Uuid::from_u128(0x_a1_d0);
    // Runner-taking since D-191: the unit is opened inside the transaction that
    // refuses the act, so that the `POST`'s gate records a body naming it. A suite
    // driving it directly supplies the runner the route's gate would have supplied.
    let conn = h.governance.db.conn().expect("scoped connection");
    bss_pricing::infra::approval::ApprovalService::submit_window_mutation_on(
        &conn,
        &h.scope(),
        h.tenant,
        window_id,
        seeded.price_id,
        approval_id,
        serde_json::json!({ "reason": "noConfiguredThreshold" }),
        rest_support::stamp_of(SUBMITTER, rest_support::at(12)),
        // A fixed subject in the shape the service builds for a cancel, act sequence
        // and all (D-190); this case asserts on the record that opens, not on a later
        // mutation resolving against it.
        &format!("{plan_id}/{window_id}/cancel/0/open/open"),
    )
    .await
    .expect("a window of a published plan opens a pending unit");
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Submitted,
        "the pin is taken here, over a window that has not started yet"
    );

    // The clock arrives at the fixture window's start. `inst-ws-activate` fires on
    // `now >= effectiveFrom`, so the boundary instant itself is due.
    let (year, month, day) = common::COVERAGE_FROM_UTC;
    let boundary = utc_ymd_hms(year, month, day, 0, 0, 0);

    let report = WindowActivationJob::new(h.db.clone(), JobsConfig::default())
        .run(boundary)
        .await
        .expect("the activation sweep runs");
    assert_eq!(
        report.activated, 1,
        "the sweep reaches this window now - see this test's doc for the premise that \
         changed. A 0 here means the interleaving never happened."
    );
    assert_eq!(
        window_repo::find(&h.db.conn().expect("conn"), &h.scope(), h.tenant, window_id,)
            .await
            .expect("read the window back")
            .map(|window| window.state),
        Some(WindowState::Active),
        "the truth row moved, and it is the window under the pin that moved"
    );

    // The reviewer decides, after the flip. `judge` re-assembles the shape and
    // re-derives the digest off the store as it now stands.
    let decided = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "approve"),
            None,
            &[],
        ))
        .await;

    let status = decided.status();
    let body = body_json(decided).await;
    // The refusal this test exists to refute, named so a future reader sees which
    // code the pin's two-token reading keeps off the wire — and read **before**
    // the status, which is what makes it an assertion rather than a restatement:
    // a 200 whose body says `approved` cannot carry a wire code, so the same line
    // under the two assertions below is satisfied by their having passed.
    assert!(
        !format!("{body}").contains("APPROVAL_CONTENT_MISMATCH"),
        "the pin must match after the flip: {body}"
    );
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "a clock-driven flip is not a content change: the two-person review stands. Got {body}"
    );
    assert_eq!(body["state"], "approved");

    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Approved,
        "and the record carries the decision, not merely the response"
    );
}

/// The flag is not decoration: when the subject moves, the document returned is
/// no longer the one the pin was taken over, and the reviewer is told.
#[tokio::test]
async fn a_moved_subject_is_shown_as_not_matching_the_pin() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    h.allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    let approval_id = approval_rows(&h).await[0].approval_id;
    // Approve first: while the unit is `submitted` the TOCTOU void would close
    // it, so the only world in which a decided record's subject can have moved
    // is this one.
    //
    // **And the approve is asserted, because that premise is the whole case.**
    // The decision was sent and dropped until 2026-08-20: a 403 or a 409 here
    // left the unit `submitted`, and both assertions at the foot still held — so
    // the case passed while covering a *pending* record, which is the one world
    // its own comment says cannot produce a moved subject.
    let decided = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(decided.status(), axum::http::StatusCode::OK);
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Approved,
        "the subject may only move under a *decided* record"
    );
    let before = rest_support::price_rows(&h, plan_id).await;
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
        .expect("move the subject");

    let body = body_json(
        h.allowed_as(APPROVER)
            .send(with_headers(
                "GET",
                &format!("/bss-pricing/v1/approvals/{approval_id}"),
                None,
                &[],
            ))
            .await,
    )
    .await;

    assert_eq!(body["content_matches_pin"], false);
    assert_eq!(
        body["pinned_content"]["rows"][0]["content"]["amount_minor"], 1,
        "the document is the subject as it stands, which is exactly why the flag is needed"
    );
}

/// The queue, and its state filter.
#[tokio::test]
async fn the_queue_lists_pending_and_decided_units_and_filters_by_state() {
    let h = Harness::new().await;
    let pending = a_pending_unit(&h).await;
    let decided = {
        // A second plan, so a second unit can exist: one subject holds one
        // pending unit.
        let plan_id = Uuid::now_v7();
        let seeded = seed_publishable_plan(&h, plan_id).await;
        h.allowed_as(SUBMITTER)
            .send(with_headers(
                "POST",
                &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
                None,
                &[("if-match", &seeded.etag())],
            ))
            .await;
        let id = approval_rows(&h)
            .await
            .into_iter()
            .map(|row| row.approval_id)
            .find(|id| *id != pending)
            .expect("the second unit");
        h.allowed_as(APPROVER)
            .send(with_headers(
                "POST",
                &decision_path(id, "approve"),
                None,
                &[],
            ))
            .await;
        id
    };

    let all = body_json(
        h.allowed_as(APPROVER)
            .send(with_headers("GET", "/bss-pricing/v1/approvals", None, &[]))
            .await,
    )
    .await;
    let ids: Vec<String> = all["items"]
        .as_array()
        .expect("a page")
        .iter()
        .map(|item| item["approval_id"].as_str().expect("an id").to_owned())
        .collect();
    assert_eq!(ids.len(), 2, "no filter is every state");
    assert!(ids.contains(&pending.to_string()));
    assert!(ids.contains(&decided.to_string()));
    assert!(
        all["page_info"]["next_cursor"].is_null(),
        "an exhausted page carries no cursor, so a client stops without an extra call"
    );

    let only_pending = body_json(
        h.allowed_as(APPROVER)
            .send(with_headers(
                "GET",
                "/bss-pricing/v1/approvals?$filter=state%20eq%20'submitted'",
                None,
                &[],
            ))
            .await,
    )
    .await;
    let filtered = only_pending["items"].as_array().expect("a page");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["approval_id"], pending.to_string());
}

/// The walk is a keyset walk, and a page that has more says so.
#[tokio::test]
async fn the_queue_pages_and_the_cursor_resumes_after_the_last_row() {
    let h = Harness::new().await;
    let first = a_pending_unit(&h).await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    h.allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(approval_rows(&h).await.len(), 2);

    let page = body_json(
        h.allowed_as(APPROVER)
            .send(with_headers(
                "GET",
                "/bss-pricing/v1/approvals?limit=1",
                None,
                &[],
            ))
            .await,
    )
    .await;
    assert_eq!(page["items"].as_array().expect("a page").len(), 1);
    let cursor = page["page_info"]["next_cursor"]
        .as_str()
        .expect("a page that has more carries a cursor")
        .to_owned();

    let next = body_json(
        h.allowed_as(APPROVER)
            .send(with_headers(
                "GET",
                &format!("/bss-pricing/v1/approvals?limit=1&cursor={cursor}"),
                None,
                &[],
            ))
            .await,
    )
    .await;
    let second = next["items"].as_array().expect("a page");
    assert_eq!(second.len(), 1);
    assert_ne!(
        second[0]["approval_id"], page["items"][0]["approval_id"],
        "the walk resumes strictly after the cursor"
    );
    let _ = first;
}

#[tokio::test]
async fn an_unknown_query_parameter_state_is_refused() {
    let h = Harness::new().await;

    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "GET",
            "/bss-pricing/v1/approvals?state=pending",
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "unrecognized query parameter `state`",
    );
}

#[tokio::test]
async fn an_unknown_state_filter_is_refused_rather_than_ignored() {
    let h = Harness::new().await;

    let response = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "GET",
            "/bss-pricing/v1/approvals?$filter=state%20eq%20'pending'",
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "unknown approval state `pending`",
    );
}

/// Another tenant's queue is empty rather than refused, which is the same
/// posture the record read takes.
#[tokio::test]
async fn the_queue_is_tenant_scoped() {
    let h = Harness::new().await;
    a_pending_unit(&h).await;

    let body = body_json(
        h.other_tenant()
            .send(with_headers("GET", "/bss-pricing/v1/approvals", None, &[]))
            .await,
    )
    .await;

    assert!(body["items"].as_array().expect("a page").is_empty());
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

/// Every route on this surface is gated, and refused callers write nothing.
#[tokio::test]
async fn every_approval_route_is_gated_and_a_refusal_decides_nothing() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;
    let routes = [
        ("GET", "/bss-pricing/v1/approvals".to_owned()),
        ("GET", format!("/bss-pricing/v1/approvals/{approval_id}")),
        ("POST", decision_path(approval_id, "approve")),
        ("POST", decision_path(approval_id, "reject")),
        ("POST", decision_path(approval_id, "withdraw")),
    ];

    for (method, path) in &routes {
        assert_eq!(
            h.denied()
                .send(with_headers(method, path, None, &[]))
                .await
                .status(),
            axum::http::StatusCode::FORBIDDEN,
            "{method} {path} under a denying PDP"
        );
        assert_eq!(
            h.anonymous()
                .send(with_headers(method, path, None, &[]))
                .await
                .status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "{method} {path} with no context"
        );
        assert_eq!(
            h.unavailable()
                .send(with_headers(method, path, None, &[]))
                .await
                .status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path} with the PDP down must fail closed"
        );
        assert_eq!(
            h.unconstrained()
                .send(with_headers(method, path, None, &[]))
                .await
                .status(),
            axum::http::StatusCode::FORBIDDEN,
            "{method} {path} under an unconstrained allow"
        );
    }

    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Submitted,
        "a 403 alone would also be produced by a handler that decided first"
    );
}

/// The gate is asked about the pair the catalog names, not merely about
/// *something*.
///
/// An allow/deny test cannot catch a route gated on the wrong pair; this can.
///
/// **The gate is the `first` question, not the last.** It read `last()` until
/// 2026-08-11, when it was true because every route here asked exactly one — and
/// it broke the moment `withdraw` grew a second, which is the right outcome from
/// the wrong assertion: `last()` would equally have passed a route that gated on
/// the correct pair and then acted, or gated on the wrong pair first and the right
/// one afterwards. The gate is what is asked *before the handler acts*, so this
/// asks for the first.
#[tokio::test]
async fn each_route_asks_the_pdp_for_the_pair_the_catalog_names() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;
    let approval_label = toolkit_gts::gts_id!("cf.bss.pricing.approval.v1~");

    for (method, path, action) in [
        ("GET", "/bss-pricing/v1/approvals".to_owned(), "read"),
        (
            "GET",
            format!("/bss-pricing/v1/approvals/{approval_id}"),
            "read",
        ),
        ("POST", decision_path(approval_id, "approve"), "approve"),
        ("POST", decision_path(approval_id, "reject"), "approve"),
        ("POST", decision_path(approval_id, "withdraw"), "approve"),
    ] {
        let (client, seen) = h.recording();
        let _ = client.send(with_headers(method, &path, None, &[])).await;
        let asked = seen.lock().expect("recorder");
        let request = asked.first().expect("the route asked the PDP");
        assert_eq!(
            request.resource.resource_type, approval_label,
            "{method} {path}"
        );
        assert_eq!(request.action.name, action, "{method} {path}");
    }
}

/// **`withdraw` asks a second question, and it is pinned here rather than left to
/// be discovered.**
///
/// It is the only route on this surface that asks twice, so the census above —
/// which is about the *gate* — cannot say what the second one is, and an unpinned
/// extra PDP call is exactly the kind of thing that drifts into a gate.
///
/// The second question is not a gate: a denial narrows the caller's authority to
/// their own units instead of refusing the request. It exists because
/// `inst-as-void` names the withdrawer as "the submitter (or a `CatalogAdmin`)"
/// and nothing at this layer can answer a question about roles — `SecurityContext`
/// carries a subject, a tenant and token scopes. So `CatalogAdmin` is asked as
/// `plan × publish`, the authority that role actually carries and no
/// `FinanceReviewer` holds.
///
/// It is deliberately **tenant-wide** (`resource_id` absent): the question is
/// whether this principal is a catalog authority at all, and the unit's plan is
/// not known at this route — only its approval id.
#[tokio::test]
async fn the_withdraw_route_asks_a_second_non_gating_question_about_catalog_authority() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let (client, seen) = h.recording();
    let _ = client
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "withdraw"),
            None,
            &[],
        ))
        .await;
    let asked = seen.lock().expect("recorder");

    assert_eq!(
        asked.len(),
        2,
        "the withdraw asks its gate and then the authority question: {:?}",
        asked
            .iter()
            .map(|r| (&r.resource.resource_type, &r.action.name))
            .collect::<Vec<_>>()
    );
    let authority = &asked[1];
    assert_eq!(
        authority.resource.resource_type,
        toolkit_gts::gts_id!("cf.bss.pricing.plan.v1~"),
    );
    assert_eq!(authority.action.name, "publish");
}

/// **A catalog authority held in another tenant is not one here** (review T-1).
///
/// `withdraw_approval` asks a second PDP question to decide
/// `WithdrawAuthority::CatalogAuthority`, and passed `owner_tenant_id =
/// Some(subject_tenant)` together with `require_constraints = false`. The
/// membership assertion in `authz::access_scope` is conjoined with that same
/// flag, so the `Some(tenant)` argument did not constrain the answer: a principal
/// whose `plan x publish` compiled to a *different* tenant was handed
/// `CatalogAuthority` here, and `authorize_decision` lets that authority void a
/// unit its holder did not submit.
///
/// **The assertion is the code and not the status.** A bare 403 is what the
/// existing `scope_mismatch` case already produces, from the *first* gate — so
/// asserting only the status would pass against a build that refuses this route
/// for the wrong reason. `WITHDRAW_FORBIDDEN` is minted only where the authority
/// narrowed to `OwnUnitsOnly` and the withdrawer is not the submitter, which is
/// exactly the question this is about. The unit staying `Submitted` is the second
/// half: a withdraw releases the canonical scope keys the unit held.
///
/// The fixture had to be widened to state this at all: `SelectiveResolver`
/// carried one `allowed_tenant` for every pair, so the divergent shape could not
/// be staged, and every case in the crate answered the same with the check and
/// without it. Its positive control is
/// `a_foreign_principal_with_catalog_authority_may_withdraw_the_unit`, one line
/// up, which grants the same pair in *this* tenant and succeeds.
#[tokio::test]
async fn catalog_authority_compiled_to_another_tenant_does_not_authorize_a_withdraw_here() {
    let h = Harness::new().await;
    let approval_id = a_pending_unit(&h).await;

    let response = h
        .selectively_allowed_as_with_foreign_pair(
            APPROVER,
            &[
                (labels::APPROVAL, actions::APPROVE),
                (labels::PLAN, actions::PUBLISH),
            ],
            (labels::PLAN, actions::PUBLISH),
        )
        .send(with_headers(
            "POST",
            &decision_path(approval_id, "withdraw"),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        problem_code(response).await,
        "WITHDRAW_FORBIDDEN",
        "the authority has to narrow to OwnUnitsOnly; a denial from the approve gate \
         would be the right status for the wrong reason"
    );
    assert_eq!(
        approval_row(&h, approval_id).await.state,
        ApprovalState::Submitted,
        "a refused withdraw leaves the unit open and its scope keys held"
    );
}
