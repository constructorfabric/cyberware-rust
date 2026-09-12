//! `GET/PUT /bss-pricing/v1/customer-groups/taxonomy`, driven through the real
//! router (`design/09-price-overlays.md` §3 `inst-cg-taxonomy`, §5;
//! `design/05-governance.md`'s endpoint map).
//!
//! # The claim this file exists to make good
//!
//! A first attempt at this surface was briefed as a fifth arm of
//! `api::rest::taxonomies`' `{class}` route, gated on `config`. That was wrong:
//! the design set gives this taxonomy its own route and its own resource,
//! `customer_group`, specifically so it is **not** reachable by every holder of
//! `config × write` — per-payer commercial data is more sensitive than plan or
//! config authoring. `every_route_asks_the_catalogued_pair` in `rest_authz.rs`
//! proves this route asks the right pair; what no allow/deny fixture there can
//! prove is that the two resources are actually **segregated** — that a caller
//! holding one does not, by that grant alone, satisfy the other. That is
//! `a_caller_holding_config_write_but_not_customer_group_write_is_refused`
//! below, armed with a `SelectiveResolver` that answers per-pair rather than
//! uniformly, and its mirror control proves the isolation runs both ways.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::approvals::APPROVAL_APPROVE;
use bss_pricing::api::rest::customer_groups::{
    CUSTOMER_GROUP_MEMBER, CUSTOMER_GROUP_MEMBER_MOVE, CUSTOMER_GROUP_MEMBERS,
    CUSTOMER_GROUP_MEMBERS_MOVE, CUSTOMER_GROUP_TAXONOMY,
};
use bss_pricing::authz::{actions, labels};
use bss_pricing::config::JobsConfig;
use bss_pricing::domain::audit::AuditSubjectKind;
use bss_pricing::domain::ports::CatalogVersionRegistryV1;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::jobs::readmodel_warm::ReadModelWarmJob;
use bss_pricing::infra::storage::entity::read_model;
use bss_pricing::infra::storage::repo::audit_repo;
use bss_pricing::infra::storage::repo::group_membership_repo;
use bss_pricing::infra::storage::repo::{NewApproval, approval_repo};
use rest_support::{
    Harness, approval_row, approval_rows, audit_rows, body_json, etag_of, membership_row,
    pending_version_refs, problem_code, refused_by, request, stamp_of, with_headers,
};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use serde_json::json;
use std::sync::Arc;
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use uuid::Uuid;

/// The `CatalogAdmin` who configures the taxonomies.
const ADMIN: uuid::Uuid = uuid::Uuid::from_u128(0xca_d1);

/// Read the taxonomy, answering the body and the tag together — the same
/// one-response discipline `rest_taxonomies.rs::read` uses, and for the same
/// reason: a helper that re-read for a tag could hand back one describing a
/// different state than the body.
async fn read(harness: &Harness) -> (serde_json::Value, String) {
    let response = harness
        .allowed_as(ADMIN)
        .send(request("GET", CUSTOMER_GROUP_TAXONOMY, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "the GET must answer 200");
    let tag = etag_of(&response).expect("a taxonomy read must carry its entity tag");
    (body_json(response).await, tag)
}

/// `PUT` a value set under a tag.
async fn put(
    harness: &Harness,
    tag: &str,
    values: serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    harness
        .allowed_as(ADMIN)
        .send(with_headers(
            "PUT",
            CUSTOMER_GROUP_TAXONOMY,
            Some(json!({ "values": values })),
            &[("if-match", tag)],
        ))
        .await
}

/// One **published** overlay scoped to `(customerGroup, value)`, written
/// through the entity — `rest_taxonomies.rs::seed_published_overlay`'s sibling.
/// The authoring route cannot produce this state in one call (a submit opens
/// an always-material approval unit, D-50), and what is under test here is the
/// taxonomy guard, not the overlay lifecycle.
async fn seed_published_overlay(harness: &Harness, value: &str) {
    use sea_orm::ActiveValue::Set;
    use sea_orm::EntityTrait;
    use toolkit_db::secure::{AccessScope, SecureInsertExt};

    let conn = harness.db.conn().expect("conn");
    let row = bss_pricing::infra::storage::entity::price_overlay::ActiveModel {
        price_overlay_id: Set(uuid::Uuid::from_u128(0x0c_9a)),
        revision: Set(1),
        tenant_id: Set(harness.tenant),
        lifecycle_state: Set("published".to_owned()),
        scope_class: Set("customer_group".to_owned()),
        scope_value: Set(value.to_owned()),
        precedence: Set(20),
        effective_from: Set(None),
        effective_to: Set(None),
        tax_basis: Set("delegated_tariffs".to_owned()),
        disclosure: Set("restricted".to_owned()),
        target_ref: Set(json!({"plans": []})),
        row_version: Set(0),
    };
    bss_pricing::infra::storage::entity::price_overlay::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed a published overlay");
}

fn codes(body: &serde_json::Value) -> Vec<String> {
    body["values"]
        .as_array()
        .expect("values is an array")
        .iter()
        .map(|v| {
            format!(
                "{}:{}",
                v["value"].as_str().expect("value"),
                v["state"].as_str().expect("state")
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The round trip.
// ---------------------------------------------------------------------------

/// A tenant with no values is answered `200` with an empty list **and a tag**
/// — the bootstrap reads its precondition off this response like every other
/// caller.
#[tokio::test]
async fn a_tenant_with_no_values_reads_200_with_an_empty_list_and_a_tag() {
    let harness = Harness::new().await;

    let (body, tag) = read(&harness).await;

    assert_eq!(codes(&body), Vec::<String>::new());
    assert!(
        !tag.is_empty(),
        "the bootstrap carries a tag like any other"
    );
}

/// The round trip: declare a value, read it back, and see it in the list.
#[tokio::test]
async fn a_put_declares_a_value_and_the_get_reads_it_back() {
    let harness = Harness::new().await;
    let (_, tag) = read(&harness).await;

    let response = put(
        &harness,
        &tag,
        json!([{ "value": "gold", "display_name": "Gold" }]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (body, _) = read(&harness).await;
    assert_eq!(codes(&body), ["gold:active"]);
    assert_eq!(body["values"][0]["display_name"], "Gold");
}

/// A value the body omits is retired, not deleted, and stays readable.
#[tokio::test]
async fn a_value_the_body_omits_is_retired_and_still_listed() {
    let harness = Harness::new().await;
    let (_, tag) = read(&harness).await;
    put(
        &harness,
        &tag,
        json!([
            { "value": "gold", "display_name": "Gold" },
            { "value": "silver", "display_name": "Silver" }
        ]),
    )
    .await;

    let (_, tag) = read(&harness).await;
    let response = put(
        &harness,
        &tag,
        json!([{ "value": "gold", "display_name": "Gold" }]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (body, _) = read(&harness).await;
    assert_eq!(codes(&body), ["gold:active", "silver:retired"]);
}

// ---------------------------------------------------------------------------
// The precondition.
// ---------------------------------------------------------------------------

/// A `PUT` with no `If-Match` is refused before anything is written.
#[tokio::test]
async fn a_put_without_if_match_is_refused() {
    let harness = Harness::new().await;

    let response = harness
        .allowed_as(ADMIN)
        .send(request(
            "PUT",
            CUSTOMER_GROUP_TAXONOMY,
            Some(json!({ "values": [{ "value": "gold", "display_name": "Gold" }] })),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // **Which** 400, the reason `re_enrolling_a_payer_immediately_is_refused_by_the_audit_only_door`
    // states for its own refusal later in this file: this route renders several
    // — an unparsable body, a value outside the vocabulary — and the status
    // separates none of them. The precondition mints no code, so the guard's own
    // sentence is the discriminator.
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "If-Match is required on this verb",
    );
    let (body, _) = read(&harness).await;
    assert_eq!(codes(&body), Vec::<String>::new(), "nothing was written");
}

/// A stale tag is refused as a lost-update guard, exactly as the four-class
/// taxonomy's `PUT` is.
#[tokio::test]
async fn a_concurrent_whole_set_put_is_refused_rather_than_retiring_the_other_authors_value() {
    let harness = Harness::new().await;
    let (_, shared_tag) = read(&harness).await;

    let first = put(
        &harness,
        &shared_tag,
        json!([{ "value": "gold", "display_name": "Gold" }]),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);

    let second = put(
        &harness,
        &shared_tag,
        json!([{ "value": "silver", "display_name": "Silver" }]),
    )
    .await;

    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(second).await, "STALE_VERSION");

    let (body, _) = read(&harness).await;
    assert_eq!(
        codes(&body),
        ["gold:active"],
        "the first author's value survives and the second's was not applied"
    );
}

// ---------------------------------------------------------------------------
// The retire guard — `inst-cg-taxonomy`'s referential check.
// ---------------------------------------------------------------------------

/// **The `409` this surface's guard exists to produce.** A published
/// `customerGroup`-scoped overlay blocks its value's retirement.
#[tokio::test]
async fn retiring_a_referenced_value_answers_409_with_the_declared_code() {
    let harness = Harness::new().await;
    let (_, tag) = read(&harness).await;
    put(
        &harness,
        &tag,
        json!([{ "value": "gold", "display_name": "Gold" }]),
    )
    .await;

    seed_published_overlay(&harness, "gold").await;

    let (_, tag) = read(&harness).await;
    let refused = put(&harness, &tag, json!([])).await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(refused).await, "TAXONOMY_VALUE_IN_USE");

    let (body, _) = read(&harness).await;
    assert_eq!(
        codes(&body),
        ["gold:active"],
        "a refused PUT writes nothing at all"
    );
}

// ---------------------------------------------------------------------------
// The audit half.
// ---------------------------------------------------------------------------

/// One audit record per `PUT`, naming this taxonomy.
#[tokio::test]
async fn a_put_is_audited_once_naming_the_taxonomy() {
    let harness = Harness::new().await;
    let before = audit_rows(&harness).await.len();
    let (_, tag) = read(&harness).await;

    put(
        &harness,
        &tag,
        json!([
            { "value": "gold", "display_name": "Gold" },
            { "value": "silver", "display_name": "Silver" }
        ]),
    )
    .await;

    let rows = audit_rows(&harness).await;
    assert_eq!(
        rows.len(),
        before + 1,
        "one PUT is one audited act, however many values it moved"
    );
    assert_eq!(
        rows.last().expect("a record").subject_ref,
        "taxonomy/customer_group"
    );
}

// ---------------------------------------------------------------------------
// The separate-route claim: `customer_group` and `config` are segregated.
// ---------------------------------------------------------------------------

/// **The whole point of the separate route.** A caller holding `config ×
/// write` — the grant that declares regions, brands, partners and org tiers —
/// does NOT, by that grant alone, satisfy `customer_group × write`.
///
/// Driven with [`rest_support::Harness::selectively_allowed_as`] rather than
/// `allowed_as`: the ordinary fixture grants every pair uniformly, which
/// cannot tell "gated on the right label and this caller lacks it" from
/// "gated on the wrong label and every fixture happens to satisfy it" — the
/// exact blind spot `rest_authz.rs`'s module doc names for the census it
/// carries. This case is the reason that census exists, armed against the
/// specific defect a shared `{class}` route would have reintroduced: a
/// `config`-only caller reaching a resource `05-governance.md` says is more
/// sensitive.
#[tokio::test]
async fn a_caller_holding_config_write_but_not_customer_group_write_is_refused() {
    let harness = Harness::new().await;
    let (_, tag) = read(&harness).await;

    let response = harness
        .selectively_allowed_as(ADMIN, &[(labels::CONFIG, actions::WRITE)])
        .send(with_headers(
            "PUT",
            CUSTOMER_GROUP_TAXONOMY,
            Some(json!({ "values": [{ "value": "gold", "display_name": "Gold" }] })),
            &[("if-match", tag.as_str())],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "config x write must not satisfy customer_group x write"
    );
    let (body, _) = read(&harness).await;
    assert_eq!(codes(&body), Vec::<String>::new(), "nothing was written");
}

/// The mirror control: a caller holding `customer_group × write` **and
/// nothing else** — not even `config` — succeeds. Without this, the case above
/// could pass merely because `SelectiveResolver` denies everything by
/// default; this is the positive half that proves the pair this route asks
/// for is the one that actually authorizes it.
#[tokio::test]
async fn a_caller_holding_only_customer_group_write_succeeds() {
    let harness = Harness::new().await;
    // The tag comes from the ordinary admin client: what this case is about is
    // the `PUT`'s gate, not the `GET`'s, so the read is not driven through the
    // restricted client.
    let (_, tag) = read(&harness).await;

    let response = harness
        .selectively_allowed_as(ADMIN, &[(labels::CUSTOMER_GROUP, actions::WRITE)])
        .send(with_headers(
            "PUT",
            CUSTOMER_GROUP_TAXONOMY,
            Some(json!({ "values": [{ "value": "gold", "display_name": "Gold" }] })),
            &[("if-match", tag.as_str())],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "customer_group x write alone, with no config grant at all, must authorize the write"
    );
}

// ---------------------------------------------------------------------------
// Task 6: the membership routes, and the publish unit `dod-customer-group`'s
// MUST requires (`design/09-price-overlays.md:472-473`) — audit-only path
// only (`inst-mm-renewal`).
// ---------------------------------------------------------------------------

/// The admin who enrolls payers.
const MEMBERSHIP_ADMIN: uuid::Uuid = uuid::Uuid::from_u128(0xca_d2);

/// Enroll a payer through the real route, returning the parsed body.
///
/// Declares `gold` active first — `GROUP_UNKNOWN` refuses a group the
/// taxonomy never declared, and this suite's own subject is the membership
/// mutation, not the taxonomy gate (that is `retiring_a_referenced_value_
/// answers_409_with_the_declared_code` and its siblings above).
async fn enroll(harness: &Harness, payer_tenant_id: Uuid) -> (StatusCode, serde_json::Value) {
    rest_support::declare_customer_group(harness, "gold").await;
    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold"),
            Some(json!({
                "payer_tenant_id": payer_tenant_id,
                "effective_from": "2026-01-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "enroll-1")],
        ))
        .await;
    let status = response.status();
    (status, body_json(response).await)
}

/// Run the read-model warm sweep the way `sqlite_read_model.rs`'s own
/// `sweep` helper does — a fresh [`ReadModelWarmJob`] over the harness's own
/// provider and the **same** registry double the route's publish unit
/// requested its handle from, so a ref this test recorded and a ref the
/// sweep resolves are the same act's two halves.
async fn sweep(harness: &Harness, now: OffsetDateTime) -> u64 {
    let job = ReadModelWarmJob::new(
        harness.db.clone(),
        Arc::clone(&harness.registry) as Arc<dyn CatalogVersionRegistryV1>,
        JobsConfig::default(),
    );
    let report = job.run(now).await.expect("the sweep pass runs");
    report.subjects_projected
}

/// Every warm delta of one membership subject, as `(catalog_version, payload)`.
///
/// Read through the entity with `AccessScope::allow_all()`, `rest_support`'s own
/// readbacks' reason: the assertion is about what landed in the store, not about
/// what the calling principal may see.
async fn membership_deltas(
    harness: &Harness,
    membership_id: &str,
) -> Vec<(i64, serde_json::Value)> {
    let conn = harness.db.conn().expect("conn");
    read_model::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(read_model::Column::TenantId.eq(harness.tenant))
                .add(read_model::Column::SubjectKind.eq("group_membership"))
                .add(read_model::Column::SubjectRef.eq(membership_id)),
        )
        .order_by(read_model::Column::CatalogVersion, Order::Asc)
        .all(&conn)
        .await
        .expect("read the membership deltas")
        .into_iter()
        .map(|row| (row.catalog_version, row.payload))
        .collect()
}

/// **The claim this section exists to prove.** A route-level enrollment is a
/// real publish unit: a pending ref recorded against the membership subject,
/// the audit record written, and the projector able to run on it and land a
/// warm delta — not merely a response that says so.
///
/// To redden this: comment out `record_ref`'s call inside
/// `membership_publish::enroll_in` (or drop the call to it entirely). The row
/// still lands and the `201` still answers — the response body proves
/// nothing here, which is exactly why every assertion below reads the
/// **store** instead. With no ref recorded, `pending_version_refs` returns
/// empty and the sweep has nothing to resolve, so `subjects_projected` reads
/// `0` and the third assertion catches what the first two, read alone, could
/// each still pass past (a ref recorded for the wrong subject id would fail
/// only the first).
#[tokio::test]
async fn a_route_level_enrollment_is_a_real_publish_unit() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();

    let (status, body) = enroll(&harness, payer_tenant_id).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let membership_id = body["membership"]["membership_id"]
        .as_str()
        .expect("membership_id")
        .to_owned();
    let pending_ref = body["pending_version_ref"]
        .as_str()
        .expect("pending_version_ref")
        .to_owned();
    assert!(!pending_ref.is_empty());

    // 1. A pending ref, recorded against exactly this membership subject.
    let refs = pending_version_refs(&harness).await;
    let matched: Vec<_> = refs
        .iter()
        .filter(|r| r.subject_kind == "group_membership" && r.subject_ref == membership_id)
        .collect();
    assert_eq!(
        matched.len(),
        1,
        "expected exactly one pending ref for membership {membership_id}, got {refs:?}"
    );
    assert_eq!(matched[0].pending_ref, pending_ref);

    // 2. The audit record `group_membership_repo::enroll` writes.
    let audits = audit_rows(&harness).await;
    assert!(
        audits
            .iter()
            .any(|a| a.subject_ref == membership_id && a.action == "create"),
        "no audit record named membership {membership_id}: {audits:?}"
    );

    // 3. The projector can run on the ref this route recorded: commit the
    // handle on the registry double and sweep.
    harness.registry.commit(&pending_ref, 9);
    let projected = sweep(&harness, OffsetDateTime::now_utc()).await;
    assert_eq!(
        projected, 1,
        "the projector must land exactly one warm delta off this route's own pending ref"
    );
}

/// **The audit-only property.** `inst-mm-renewal`'s default commits directly
/// and opens **no** approval unit.
///
/// Paired with [`a_pending_approval_unit_is_visible_through_the_same_readback`]
/// as the positive control the module doc's TESTS section requires: without
/// it, a broken `approval_rows` that always answered empty would make this
/// test pass for the wrong reason.
///
/// To redden this: give `membership_publish::enroll_in` a call that opens an
/// approval unit (any `approval_repo::open` call over the enrolled
/// membership's subject) before it returns. The route still answers `201`
/// and the publish-unit assertions in the sibling test still hold, which is
/// why this is its own test rather than a fourth assertion bolted onto that
/// one.
#[tokio::test]
async fn enrolling_a_payer_opens_no_approval_unit() {
    let harness = Harness::new().await;
    let before = approval_rows(&harness).await.len();

    let (status, _body) = enroll(&harness, Uuid::now_v7()).await;
    assert_eq!(status, StatusCode::CREATED);

    assert_eq!(
        approval_rows(&harness).await.len(),
        before,
        "the audit-only path must open no approval unit"
    );
}

/// The positive control for the test above: `approval_rows` really does see
/// a unit when one is opened, seeded independently of the membership route
/// through `approval_repo::open` directly (no plan needed — the approval
/// store's own writer takes a bare subject).
///
/// To redden this: remove the `approval_repo::open` call below. Then this
/// test asserts `1 == 0` and fails loudly, which is what proves it was
/// checking something rather than passing by construction.
#[tokio::test]
async fn a_pending_approval_unit_is_visible_through_the_same_readback() {
    let harness = Harness::new().await;
    let conn = harness.db.conn().expect("conn");
    assert_eq!(approval_rows(&harness).await.len(), 0, "clean start");

    approval_repo::open(
        &conn,
        &harness.scope(),
        NewApproval {
            approval_id: Uuid::now_v7(),
            tenant_id: harness.tenant,
            // The approval store's own shape (`<plan_id>/<revision>`), unrelated
            // to any membership route — a bare label tripped `CorruptRow` here
            // during this test's own development, which is `subject_of`'s own
            // fail-closed reading of a subject no writer in this crate produces.
            subject_ref: audit_repo::plan_revision_ref(PlanId::new(Uuid::now_v7()), 0),
            subject_kind: AuditSubjectKind::PlanRevision,
            content_hash: vec![0u8; 32],
            materiality: json!({ "material": true, "reason": "positive-control" }),
            held_keys: std::collections::BTreeSet::new(),
        },
        stamp_of(MEMBERSHIP_ADMIN, OffsetDateTime::now_utc()),
    )
    .await
    .expect("seed a pending unit unrelated to any membership route");

    assert_eq!(
        approval_rows(&harness).await.len(),
        1,
        "the readback must see a unit that genuinely exists"
    );
}

/// The move route composes an end and an enroll on one publish unit — both
/// subjects recorded against the **same** pending ref.
#[tokio::test]
async fn a_move_records_both_membership_subjects_against_one_pending_ref() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    let (status, body) = enroll(&harness, payer_tenant_id).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let old_membership_id = body["membership"]["membership_id"]
        .as_str()
        .expect("membership_id")
        .to_owned();
    rest_support::declare_customer_group(&harness, "silver").await;

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "silver")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            // Future-dated, so this case stays on the renewal-aligned arm it is
            // about: D-350 routes a move landing now or in the past to the
            // material arm, which writes no membership row for this to read.
            Some(json!({ "effective_from": "2099-06-01T00:00:00.000000Z" })),
            &[("idempotency-key", "move-1")],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // **Under `moved`, on this arm too, since 2026-08-20** (review D4-3's
    // residual): the renewal-aligned arm answered `MembershipMoveView` bare while
    // the commit arm answered `MembershipMoveMaterialView`, so the operation
    // declared two schemas on one status and the declared one was missing two
    // required members of the body a D-350 move actually receives. One type, one
    // status, `outcome` the discriminator.
    assert_eq!(body["outcome"], "committed", "{body}");
    let ended_id = body["moved"]["ended"]["membership_id"]
        .as_str()
        .expect("moved.ended.membership_id");
    assert_eq!(
        ended_id, old_membership_id,
        "the move ended the payer's prior membership"
    );
    let enrolled_id = body["moved"]["enrolled"]["membership_id"]
        .as_str()
        .expect("moved.enrolled.membership_id")
        .to_owned();
    let pending_ref = body["moved"]["pending_version_ref"]
        .as_str()
        .expect("moved.pending_version_ref")
        .to_owned();

    let refs = pending_version_refs(&harness).await;
    let for_pending: Vec<_> = refs
        .iter()
        .filter(|r| r.pending_ref == pending_ref)
        .collect();
    let subjects: std::collections::BTreeSet<&str> =
        for_pending.iter().map(|r| r.subject_ref.as_str()).collect();
    assert!(
        subjects.contains(old_membership_id.as_str()) && subjects.contains(enrolled_id.as_str()),
        "the one handle must carry both membership subjects: {refs:?}"
    );
}

/// The `PATCH` ends a membership under its own `If-Match`, and the same
/// publish unit and audit-only properties hold for it.
#[tokio::test]
async fn adjusting_a_membership_is_also_its_own_publish_unit() {
    let harness = Harness::new().await;
    let (status, body) = enroll(&harness, Uuid::now_v7()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let membership_id = body["membership"]["membership_id"]
        .as_str()
        .expect("membership_id")
        .to_owned();
    // There is no `GET` on a membership (§5), so the tag a `PATCH` asserts is
    // the one the create's own response carried — read off the body rather
    // than a second request.
    let row_version = body["membership"]["row_version"]
        .as_u64()
        .expect("row_version");
    let expected_tag = format!("\"{row_version}\"");

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "PATCH",
            &CUSTOMER_GROUP_MEMBER
                .replace("{group}", "gold")
                .replace("{id}", &membership_id),
            Some(json!({ "effective_to": "2026-03-01T00:00:00.000000Z" })),
            &[("if-match", &expected_tag)],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let pending_ref = body["pending_version_ref"]
        .as_str()
        .expect("pending_version_ref");

    let refs = pending_version_refs(&harness).await;
    assert!(
        refs.iter()
            .any(|r| r.subject_ref == membership_id && r.pending_ref == pending_ref),
        "the PATCH must record its own pending ref: {refs:?}"
    );
}

/// **Two publish units over one membership row, through the two routes that
/// mint them, and each version freezes the state its own commit judged.**
///
/// `MembershipSubjectDelta`'s doc named this sequence as the premise the
/// projector's live read of the row was safe under — *"nothing in this gear yet
/// mints more than one publish unit per membership row"* — and asked whoever
/// wired `enroll` and `end_membership` into the registry request/pending-ref
/// path to look again. `POST …/members` and `PATCH …/members/{id}` are that
/// wiring, so the order below is the whole test: enroll, **commit** its version,
/// end the membership, commit that version, and only then sweep. Read live at
/// that point the projector answers "ended" for both, so the enrollment's
/// version reports a membership already over at an instant when it was not —
/// permanently, on an INSERT-only delta a consumer resolves a pin against.
///
/// The sibling in `tests/sqlite_read_model.rs`
/// (`each_of_two_publish_units_over_one_membership_freezes_the_state_it_judged`)
/// proves the same property one layer down. This one is at the routes because
/// what has to hold is that **the producers** pin — a route reaching the
/// repository around `membership_publish` would record a ref with no pin and
/// fail here rather than silently freeze the wrong interval.
///
/// To redden this: have `membership_publish::record_ref` pass `None` for the
/// interval end, or make `read_model::project_membership_subject` read
/// `record.effective_to` again. Both make version 9 carry `2026-03-01`.
#[tokio::test]
async fn two_route_level_mutations_of_one_membership_freeze_two_different_intervals() {
    let harness = Harness::new().await;
    let (status, body) = enroll(&harness, Uuid::now_v7()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let membership_id = body["membership"]["membership_id"]
        .as_str()
        .expect("membership_id")
        .to_owned();
    assert!(
        body["membership"]["effective_to"].is_null(),
        "the enrollment is open-ended, which is the state its version must freeze: {body}"
    );
    let enrolled_ref = body["pending_version_ref"]
        .as_str()
        .expect("pending_version_ref")
        .to_owned();
    let expected_tag = format!(
        "\"{}\"",
        body["membership"]["row_version"]
            .as_u64()
            .expect("row_version")
    );

    // The first unit's version commits, and the second mutation lands before
    // the sweep — the window D-47 makes up to five minutes wide.
    harness.registry.commit(&enrolled_ref, 9);

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "PATCH",
            &CUSTOMER_GROUP_MEMBER
                .replace("{group}", "gold")
                .replace("{id}", &membership_id),
            Some(json!({ "effective_to": "2026-03-01T00:00:00.000000Z" })),
            &[("if-match", &expected_tag)],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let ended_ref = body["pending_version_ref"]
        .as_str()
        .expect("pending_version_ref")
        .to_owned();
    assert_ne!(
        ended_ref, enrolled_ref,
        "the two mutations are two publish units, which is the premise this test drives"
    );
    harness.registry.commit(&ended_ref, 10);

    assert_eq!(
        sweep(&harness, OffsetDateTime::now_utc()).await,
        2,
        "one warm delta per publish unit"
    );

    let deltas = membership_deltas(&harness, &membership_id).await;
    let at_9 = deltas
        .iter()
        .find(|(version, _)| *version == 9)
        .map(|(_, payload)| payload)
        .expect("the enrollment's version is warm");
    let at_10 = deltas
        .iter()
        .find(|(version, _)| *version == 10)
        .map(|(_, payload)| payload)
        .expect("the end's version is warm");
    assert_eq!(
        at_9.get("effectiveTo"),
        Some(&serde_json::Value::Null),
        "version 9 froze the enrollment, which was open-ended when its publish judged it: {at_9}"
    );
    assert_eq!(
        at_10.get("effectiveTo"),
        Some(&json!("2026-03-01T00:00:00.000000Z")),
        "and version 10 froze the end, which is the state its own publish judged: {at_10}"
    );
}

// ---------------------------------------------------------------------------
// `GROUP_UNKNOWN` (§5:257): `{group}` must be declared and active.
// ---------------------------------------------------------------------------

/// A `{group}` the taxonomy has never declared is refused `GROUP_UNKNOWN`, and
/// nothing lands: no membership row, no pending ref, no audit record.
///
/// **The positive control** is `a_route_level_enrollment_is_a_real_publish_unit`
/// above: it already proves an enrollment into a group `declare_customer_group`
/// made active succeeds end to end, so this refusal is provably about the
/// undeclared group and not about the route being broken in general.
///
/// To redden this: remove the `require_active_group` call from
/// `membership_publish::enroll_in`. The route still answers `201` and the
/// positive control above still passes, which is why this needs its own test
/// rather than a shared one.
#[tokio::test]
async fn enrolling_into_an_undeclared_group_is_refused_group_unknown_and_writes_nothing() {
    let harness = Harness::new().await;
    // No `declare_customer_group` call: the tenant's taxonomy is empty.

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBERS.replace("{group}", "nonexistent"),
            Some(json!({
                "payer_tenant_id": Uuid::now_v7(),
                "effective_from": "2026-01-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "undeclared-1")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "GROUP_UNKNOWN");

    assert_eq!(pending_version_refs(&harness).await, Vec::new());
    assert!(audit_rows(&harness).await.is_empty());
}

/// **The retired case (§5's "declared and then retired" reading).** A
/// `{group}` that was declared and then retired is refused `GROUP_UNKNOWN`
/// exactly as an undeclared one is — retirement guards existing references,
/// it does not bless a new one.
///
/// To redden this: in `membership_publish::require_active_group`, change the
/// `Some(entry) if entry.state == TaxonomyState::Active => Ok(())` arm's
/// negative case to answer `Ok(())` regardless of state (i.e. only refuse
/// `None`). The sibling test above (an **undeclared** group) still catches
/// nothing wrong — its group answers `None`, not `Some(Retired)` — which is
/// exactly why the retired case needs a test of its own rather than trusting
/// the undeclared one to cover it.
#[tokio::test]
async fn enrolling_into_a_retired_group_is_refused_group_unknown_and_writes_nothing() {
    let harness = Harness::new().await;
    rest_support::declare_customer_group(&harness, "silver-tier").await;
    rest_support::retire_customer_group(&harness, "silver-tier").await;

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBERS.replace("{group}", "silver-tier"),
            Some(json!({
                "payer_tenant_id": Uuid::now_v7(),
                "effective_from": "2026-01-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "retired-1")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "GROUP_UNKNOWN");

    assert_eq!(pending_version_refs(&harness).await, Vec::new());
    assert!(audit_rows(&harness).await.is_empty());
}

/// The move route's target group is checked the same way — a payer cannot be
/// moved into a group the taxonomy does not currently declare.
#[tokio::test]
async fn moving_a_payer_into_an_undeclared_group_is_refused_group_unknown_and_writes_nothing() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    let (status, body) = enroll(&harness, payer_tenant_id).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let refs_before = pending_version_refs(&harness).await;
    let audits_before = audit_rows(&harness).await.len();

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "nonexistent")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            Some(json!({ "effective_from": "2026-06-01T00:00:00.000000Z" })),
            &[("idempotency-key", "move-undeclared-1")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "GROUP_UNKNOWN");

    // Nothing moved: the payer's one prior membership is still exactly what
    // it was, and no second pending ref or audit record appeared.
    assert_eq!(pending_version_refs(&harness).await, refs_before);
    assert_eq!(audit_rows(&harness).await.len(), audits_before);
}

// ---------------------------------------------------------------------------
// The material path: `inst-mm-immediate` / `inst-mm-renewal`'s control pair,
// and the approve -> commit lane (Task 7 of the customer-group plane).
// ---------------------------------------------------------------------------

/// The independent second principal every material-path test below decides
/// under — distinct from [`MEMBERSHIP_ADMIN`], which is what
/// `chk_pricing_approval_distinct_principals` and `inst-tp-distinct` both
/// require of an approve.
const MOVE_APPROVER: uuid::Uuid = uuid::Uuid::from_u128(0xca_d3);

/// `POST .../move` with no `immediate` field at all.
///
/// **The negative half of the control pair.** Paired with
/// [`moving_a_payer_immediately_is_material_and_writes_no_membership_row_until_approved`]:
/// one alone proves nothing, because a route that opened a unit on *every*
/// move would still pass a test that only checked the immediate case, and a
/// route that opened one on *no* move would still pass a test that only
/// checked the default case. Together they prove the field is what decides
/// it.
///
/// To redden this: give `move_membership`'s renewal-aligned branch a call
/// that opens an approval unit (any `ApprovalService::submit_membership_move_on`
/// call before it returns). The route still answers `200` and the sibling
/// pending-ref test still holds, which is why this is checked on its own.
#[tokio::test]
async fn moving_a_payer_without_immediate_still_opens_no_approval_unit() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    let (status, body) = enroll(&harness, payer_tenant_id).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    rest_support::declare_customer_group(&harness, "silver").await;
    let before = approval_rows(&harness).await.len();

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "silver")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            // **A future instant, and that is now load-bearing** (review O-1,
            // D-350). This case read `2026-06-01`, which is in the past, so what
            // it pinned was that a *backdated* move — an immediate re-resolution
            // by any reading — committed with one principal. The renewal-aligned
            // arm is the one where the effect lands at a future renewal, and that
            // is what this fixture has to express.
            Some(json!({ "effective_from": "2099-06-01T00:00:00.000000Z" })),
            &[("idempotency-key", "move-renewal-1")],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    assert_eq!(
        approval_rows(&harness).await.len(),
        before,
        "a renewal-aligned move must open no approval unit (inst-mm-renewal)"
    );

    // **The body this arm answers is the one the 200 declares** (review D4-3's
    // residual, 2026-08-20). It answered `MembershipMoveView` bare — `{ended,
    // enrolled, pendingVersionRef}` — while `move_membership_immediate`'s commit
    // arm answered `MembershipMoveMaterialView` on the same status, so the
    // operation had two schemas on `200` and the declared one was missing both of
    // the other's required members. Asserted here rather than only on the commit
    // arm because this is the arm whose body *changed*: a caller of the
    // renewal-aligned move now reads the move under `moved`.
    assert_eq!(body["outcome"], "committed", "{body}");
    assert_eq!(body["moved"]["enrolled"]["group_value"], "silver", "{body}");
    assert!(
        !body["moved"]["pending_version_ref"]
            .as_str()
            .expect("moved.pending_version_ref")
            .is_empty(),
        "the committing arm names the publish unit it opened: {body}"
    );
    // The two members that separate this arm from the material one: nothing was
    // evaluated and nothing was opened, so rendering either would be reporting a
    // record nobody wrote.
    assert!(body["materiality"].is_null(), "{body}");
    assert!(body["approval"].is_null(), "{body}");
}

/// A **replay** of a renewal-aligned move answers the recorded body, and the
/// recorded body is the one the declaration names.
///
/// `idempotent::guarded` stores the rendered response and hands it back verbatim,
/// so the recording call is a second site with the same choice of shape — and one
/// no assertion covered. Recording `MembershipMoveView` while the live arm answered
/// the material view would make one `Idempotency-Key` answer two shapes on one
/// status, which is exactly the defect review D4-3's residual names, arriving
/// through the replay instead of through the arm.
#[tokio::test]
async fn a_replayed_renewal_aligned_move_answers_the_same_shape_the_first_call_did() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    let (status, body) = enroll(&harness, payer_tenant_id).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    rest_support::declare_customer_group(&harness, "silver").await;

    let move_request = || {
        with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "silver")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            Some(json!({ "effective_from": "2099-06-01T00:00:00.000000Z" })),
            &[("idempotency-key", "move-replay-1")],
        )
    };

    let first = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(move_request())
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = body_json(first).await;

    let replayed = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(move_request())
        .await;
    assert_eq!(replayed.status(), StatusCode::OK);
    let replayed_body = body_json(replayed).await;

    assert_eq!(
        replayed_body, first_body,
        "a replay is the recorded answer verbatim"
    );
    assert_eq!(replayed_body["outcome"], "committed", "{replayed_body}");
    assert!(
        replayed_body["moved"]["enrolled"]["membership_id"].is_string(),
        "and the recorded shape is the material view, not the bare move: {replayed_body}"
    );
}

/// A move that lands **now or in the past** is material whatever the body says
/// (review O-1, 2026-08-19, D-350).
///
/// `immediate` was the sole discriminator, so `inst-mm-immediate`'s two-person
/// rule was elective: omit the member and the identical rows committed under one
/// principal, with no outbox event and nothing in the store telling an approved
/// move from an unapproved one. The instant is the one fact the server owns.
#[tokio::test]
async fn a_backdated_move_is_material_even_though_the_body_does_not_say_immediate() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    let (status, body) = enroll(&harness, payer_tenant_id).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    rest_support::declare_customer_group(&harness, "silver").await;
    let before = approval_rows(&harness).await.len();

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "silver")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            Some(json!({ "effective_from": "2026-06-01T00:00:00.000000Z" })),
            &[("idempotency-key", "move-backdated-1")],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "a move landing in the past is an immediate re-resolution and opens the unit"
    );
    assert_eq!(
        approval_rows(&harness).await.len(),
        before + 1,
        "and the unit is in the store, not merely in the response"
    );
}

/// The side door, closed: an enrollment that **re-resolves** a payer who already
/// has membership history is refused (review O-1, D-350).
///
/// Half-open intervals mean an end at `T` plus an enrollment at `T` compose
/// exactly the row pair a move writes, with no overlap for `refuse_overlap` to
/// see and no approval anywhere. The refusal names the move route, which has the
/// unit.
#[tokio::test]
async fn re_enrolling_a_payer_immediately_is_refused_by_the_audit_only_door() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    let (status, body) = enroll(&harness, payer_tenant_id).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    rest_support::declare_customer_group(&harness, "silver").await;

    // The second half of the composition: the same payer, another group, landing
    // now. Its first half (ending the current interval) is legitimate on its own.
    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBERS.replace("{group}", "silver"),
            Some(json!({
                "payer_tenant_id": payer_tenant_id,
                "effective_from": "2026-06-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "side-door-1")],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an immediate re-resolution by another door is refused"
    );
    // **Which** 400, because this route renders several and the status alone was
    // the whole of this case until 2026-08-20: an undeclared group answers the
    // same status with `GROUP_UNKNOWN` (line 850), an empty interval and an
    // overlap answer it with theirs, so the door D-350 closed could reopen with
    // this test green. `refuse_a_move_by_the_side_door` raises a bare
    // `InvalidRequest` and therefore renders **no** wire code — `problem_code`
    // panics on it — so the discriminator is its detail, which is also the only
    // thing that sends the operator to the route that has the unit.
    let problem = body_json(response).await;
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("inst-mm-immediate"),
        "the refusal names the rule that owns it, not another of this route's 400s: {problem}"
    );
    assert!(
        detail.contains("/move"),
        "and it names the route that opens the unit, which is the whole remediation: {problem}"
    );
}

/// The **positive control**, and the narrowness of the rule in one case: a payer
/// with no membership history is being onboarded, not moved, so the identical
/// request lands.
///
/// Without it the refusal above would pass against a door that refused every
/// enrollment — which would refuse the ordinary act this route exists for.
#[tokio::test]
async fn a_first_enrollment_landing_now_is_onboarding_and_still_lands() {
    let harness = Harness::new().await;
    rest_support::declare_customer_group(&harness, "silver").await;

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBERS.replace("{group}", "silver"),
            Some(json!({
                "payer_tenant_id": Uuid::now_v7(),
                "effective_from": "2026-06-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "onboarding-1")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
}

/// One enrolment request, twice — the shape a client that timed out retries.
///
/// Not [`enroll`], which declares `gold` on every call and would collide with its
/// own seed the second time, and not [`enroll_from`], which asserts `201` before
/// the caller can look at the answer.
fn enrollment_request(payer_tenant_id: Uuid) -> axum::http::Request<axum::body::Body> {
    with_headers(
        "POST",
        &CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold"),
        Some(json!({
            "payer_tenant_id": payer_tenant_id,
            "effective_from": "2026-01-01T00:00:00.000000Z"
        })),
        &[("idempotency-key", "enroll-retry-1")],
    )
}

/// **A retry of an already-committed enrolment replays its stored answer** (review
/// H2, 2026-08-20).
///
/// The side-door guard above is a read of the payer's membership history, and on a
/// retry the history it finds is **the retry's own first call**. Evaluated ahead of
/// the idempotency gate it therefore answered `400` — "use the move route" — for an
/// enrolment that had already landed, which is the exact inversion the mandatory
/// `Idempotency-Key` exists to prevent: the client is told its request was invalid
/// when the server holds the `201` it should be replaying.
///
/// So the guard now runs **inside** the guarded body, where a replayed key returns
/// the recorded answer before any rule of the route is consulted. To redden this:
/// move `refuse_a_move_by_the_side_door` back above `idempotent::guarded` in
/// `create_membership`.
///
/// The membership id is read back from **both** answers rather than from the store:
/// a replay that minted a second id would be a different defect with the same
/// status code, and only the id tells them apart. The store is asserted too — one
/// interval, not two.
#[tokio::test]
async fn a_retried_enrollment_replays_the_recorded_201_rather_than_refusing_as_a_side_door_move() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    rest_support::declare_customer_group(&harness, "gold").await;

    let first = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(enrollment_request(payer_tenant_id))
        .await;
    let first_status = first.status();
    let first_body = body_json(first).await;
    assert_eq!(
        first_status,
        StatusCode::CREATED,
        "the enrolment of a payer with no history lands: {first_body}"
    );

    let retry = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(enrollment_request(payer_tenant_id))
        .await;
    let retry_status = retry.status();
    let retry_body = body_json(retry).await;
    assert_eq!(
        retry_status,
        StatusCode::CREATED,
        "the same key and the same body must replay the recorded answer, not be refused as a \
         re-resolution of the payer its own first call enrolled: {retry_body}"
    );
    assert_eq!(
        retry_body["membership"]["membership_id"], first_body["membership"]["membership_id"],
        "a replay reports the membership the first call created, never a second one"
    );

    let conn = harness.db.conn().expect("conn");
    let held = group_membership_repo::intervals_for_payer(
        &conn,
        &harness.scope(),
        harness.tenant,
        payer_tenant_id,
    )
    .await
    .expect("read the payer's intervals");
    assert_eq!(
        held.len(),
        1,
        "and nothing ran twice: one enrolment, one interval: {held:?}"
    );
}

/// **The positive half of the control pair.** `immediate: true` makes the
/// move material: the route answers `202`, an approval unit opens carrying
/// `alwaysMaterialTrigger` — read back from the **store**, never from the
/// response body (F-1: a route that echoes its own literal proves nothing
/// about what the store holds) — and, `inst-mm-pending`'s own promise, no
/// `pricing_group_membership` row exists yet.
///
/// To redden this: delete the `if request.immediate == Some(true)` branch
/// from `move_membership`, folding the immediate call into the ordinary
/// audit-only path. The response then answers `200` with a committed move
/// instead of `202`, and `approval_rows` stays at `before`.
#[tokio::test]
async fn moving_a_payer_immediately_is_material_and_writes_no_membership_row_until_approved() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    rest_support::declare_customer_group(&harness, "gold").await;
    let before = approval_rows(&harness).await.len();

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "gold")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            Some(json!({
                "effective_from": "2026-06-01T00:00:00.000000Z",
                "immediate": true
            })),
            &[("idempotency-key", "move-immediate-1")],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");
    assert_eq!(body["outcome"], "submitted_for_approval");
    assert!(body["moved"].is_null(), "nothing has committed yet");

    assert_eq!(
        approval_rows(&harness).await.len(),
        before + 1,
        "an immediate re-resolution must open exactly one approval unit"
    );
    let approval_id: Uuid = body["approval"]["approval_id"]
        .as_str()
        .expect("approval.approval_id")
        .parse()
        .expect("a UUID");
    let stored = approval_row(&harness, approval_id).await;
    assert_eq!(stored.subject_kind, AuditSubjectKind::Membership);
    assert_eq!(
        stored.materiality["reason"], "alwaysMaterialTrigger",
        "an immediate re-resolution must be stored as an always-material trigger: {:?}",
        stored.materiality
    );

    // `inst-mm-pending`: nothing is written to the membership plane before
    // approval.
    let conn = harness.db.conn().expect("conn");
    let intervals = group_membership_repo::intervals_for_payer(
        &conn,
        &harness.scope(),
        harness.tenant,
        payer_tenant_id,
    )
    .await
    .expect("read the payer's intervals");
    assert!(
        intervals.is_empty(),
        "no membership row may exist before the unit is approved"
    );
}

/// **The approve -> commit path works end to end, at three calls and not
/// two.** Once [`MOVE_APPROVER`] — independent of the submitter — approves
/// the unit the first call opened, retrying the identical move (a fresh
/// `Idempotency-Key`, since this is a new attempt rather than a replay of
/// the first) finds the approved unit and commits: `200`, a real membership
/// row, and the D-06 publish unit's pending handle. A **third** call —
/// `ApprovalState::Approved` is terminal, so the unit this call finds is the
/// same one the second call already applied — must answer the same way
/// rather than trying to re-apply the move: this is the replay guard's own
/// test, and every earlier version of this suite stopped at two calls, which
/// is exactly why the guard's absence was invisible.
///
/// To redden this (the commit half): in `move_membership_immediate`,
/// short-circuit the `approved_unit` branch to always fall through to "open
/// a new unit" instead of committing. The second call then answers `202`
/// again (a second, distinct approval unit) instead of `200` with a
/// committed move, and the membership-row assertion after it fails.
///
/// To redden this (the replay guard): remove the `already_applied` check
/// from `ApprovalService::commit_membership_move_in`. The third call then
/// re-enters `move_payer_in` for a proposal already applied, which tries to
/// end the membership row the second call just created at the exact instant
/// it starts — `end_membership` refuses that `MembershipIntervalEmpty`, and
/// the third call answers a 4xx interval error instead of replaying `200`.
#[tokio::test]
async fn an_immediately_moved_payer_commits_once_a_second_principal_approves() {
    let harness = Harness::new().await;
    let payer_tenant_id = Uuid::now_v7();
    rest_support::declare_customer_group(&harness, "gold").await;

    let submit_response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "gold")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            Some(json!({
                "effective_from": "2026-06-01T00:00:00.000000Z",
                "immediate": true
            })),
            &[("idempotency-key", "move-immediate-commit-1")],
        ))
        .await;
    assert_eq!(submit_response.status(), StatusCode::ACCEPTED);
    let submit_body = body_json(submit_response).await;
    let approval_id: Uuid = submit_body["approval"]["approval_id"]
        .as_str()
        .expect("approval.approval_id")
        .parse()
        .expect("a UUID");

    let approve_response = harness
        .allowed_as(MOVE_APPROVER)
        .send(request(
            "POST",
            &APPROVAL_APPROVE.replace("{approvalId}", &approval_id.to_string()),
            None,
        ))
        .await;
    assert_eq!(
        approve_response.status(),
        StatusCode::OK,
        "body: {:?}",
        body_json(approve_response).await
    );

    let commit_response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "gold")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            Some(json!({
                "effective_from": "2026-06-01T00:00:00.000000Z",
                "immediate": true
            })),
            &[("idempotency-key", "move-immediate-commit-2")],
        ))
        .await;
    let status = commit_response.status();
    let body = body_json(commit_response).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["outcome"], "committed");
    assert_eq!(body["moved"]["enrolled"]["group_value"], "gold");
    assert_eq!(
        body["moved"]["enrolled"]["payer_tenant_id"],
        payer_tenant_id.to_string()
    );
    assert!(
        !body["moved"]["pending_version_ref"]
            .as_str()
            .expect("pending_version_ref")
            .is_empty()
    );
    let committed_membership_id = body["moved"]["enrolled"]["membership_id"].clone();

    // **The third call.** `ApprovalState::Approved` is terminal, so this
    // finds the same approved unit the second call already applied. It must
    // answer idempotently — the same membership, no new row, no interval
    // refusal — rather than trying to re-apply a proposal that already
    // landed.
    let replay_response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBER_MOVE
                .replace("{group}", "gold")
                .replace("{payerId}", &payer_tenant_id.to_string()),
            Some(json!({
                "effective_from": "2026-06-01T00:00:00.000000Z",
                "immediate": true
            })),
            &[("idempotency-key", "move-immediate-commit-3")],
        ))
        .await;
    let replay_status = replay_response.status();
    let replay_body = body_json(replay_response).await;
    assert_eq!(
        replay_status,
        StatusCode::OK,
        "a third call over an already-committed unit must answer idempotently, not refuse an \
         interval error: body: {replay_body}"
    );
    assert_eq!(replay_body["outcome"], "committed");
    assert_eq!(
        replay_body["moved"]["enrolled"]["membership_id"], committed_membership_id,
        "a replay must report the membership the second call already created, not a new one"
    );

    let conn = harness.db.conn().expect("conn");
    let intervals = group_membership_repo::intervals_for_payer(
        &conn,
        &harness.scope(),
        harness.tenant,
        payer_tenant_id,
    )
    .await
    .expect("read the payer's intervals");
    assert_eq!(
        intervals.len(),
        1,
        "the approved move must actually write the membership row, and the replay must not \
         write a second one"
    );
}

// ---------------------------------------------------------------------------
// `PATCH .../members/{id}`: `{group}` is checked, not decorative
// (`api::rest::prices::row_of_plan`'s shape).
// ---------------------------------------------------------------------------

/// A `PATCH` whose `{group}` disagrees with the addressed membership's own
/// stored group is refused exactly like an absent membership, and the row is
/// untouched — no new pending ref, no row version moved.
///
/// Paired with `adjusting_a_membership_is_also_its_own_publish_unit` as the
/// positive control: that test `PATCH`es the **same** membership through its
/// **correct** group and succeeds, which is what proves this refusal is about
/// the mismatch and not about the route being broken in general.
///
/// To redden this: remove the `membership_of_group` call from
/// `adjust_membership`. The positive-control PATCH still succeeds (it never
/// exercises a mismatch), which is why the pairing is necessary rather than
/// decorative — a broken removal would silently pass the paired test too if
/// the paired test were the only one changed to catch it.
#[tokio::test]
async fn patching_a_membership_through_the_wrong_group_is_refused_and_writes_nothing() {
    let harness = Harness::new().await;
    let (status, body) = enroll(&harness, Uuid::now_v7()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let membership_id: Uuid = body["membership"]["membership_id"]
        .as_str()
        .expect("membership_id")
        .parse()
        .expect("a UUID");
    rest_support::declare_customer_group(&harness, "wrong-group").await;
    let refs_before = pending_version_refs(&harness).await;
    let before = membership_row(&harness, membership_id)
        .await
        .expect("the enrolled row exists");

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "PATCH",
            &CUSTOMER_GROUP_MEMBER
                .replace("{group}", "wrong-group")
                .replace("{id}", &membership_id.to_string()),
            Some(json!({ "effective_to": "2026-03-01T00:00:00.000000Z" })),
            &[("if-match", "\"0\"")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let after = membership_row(&harness, membership_id)
        .await
        .expect("the row still exists, untouched");
    assert_eq!(
        after.effective_to, before.effective_to,
        "effective_to moved"
    );
    assert_eq!(after.row_version, before.row_version, "row_version moved");
    assert_eq!(
        pending_version_refs(&harness).await,
        refs_before,
        "a refused PATCH must record no pending ref"
    );
}

// ---------------------------------------------------------------------------
// Reading a group's memberships (D-322)
// ---------------------------------------------------------------------------

async fn list_members(harness: &Harness, group: &str) -> (StatusCode, serde_json::Value) {
    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "GET",
            &CUSTOMER_GROUP_MEMBERS.replace("{group}", group),
            None,
            &[],
        ))
        .await;
    let status = response.status();
    (status, body_json(response).await)
}

/// An enrolment is **visible** afterwards — the whole reason the read exists.
///
/// Before D-322 the only evidence an enrolment landed was the 202 the caller had
/// already seen; nothing in the contract could show the membership again.
#[tokio::test]
async fn an_enrolled_payer_is_readable_in_the_group() {
    let harness = Harness::new().await;
    let payer = Uuid::from_u128(0x_d3_22_01);
    let (status, _) = enroll(&harness, payer).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = list_members(&harness, "gold").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["group_value"], json!("gold"));
    assert_eq!(body["memberships"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["memberships"][0]["payer_tenant_id"], json!(payer));
}

/// A group nobody has been enrolled into reads as an **empty list**, not a 404.
///
/// A declared group with no members is a state, and answering an absence would
/// make "is this group empty or does it not exist" unanswerable from the read.
#[tokio::test]
async fn a_group_with_no_memberships_reads_empty() {
    let harness = Harness::new().await;
    rest_support::declare_customer_group(&harness, "gold").await;

    let (status, body) = list_members(&harness, "gold").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["memberships"], json!([]));
}

/// An **ended** membership stays in the list.
///
/// The slice names an auditor who reads membership history, and membership is
/// effective-dated: a list of only the live intervals answers "who is in this
/// group" and silently loses "who has been", which is the other half of the
/// same question.
#[tokio::test]
async fn an_ended_membership_is_still_listed() {
    let harness = Harness::new().await;
    let payer = Uuid::from_u128(0x_d3_22_02);
    let (_, created) = enroll(&harness, payer).await;
    let membership_id = created["membership"]["membership_id"]
        .as_str()
        .expect("the id")
        .to_owned();
    let version = created["membership"]["row_version"]
        .as_u64()
        .expect("the create response carries the membership's row_version");

    let ended = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "PATCH",
            &CUSTOMER_GROUP_MEMBER
                .replace("{group}", "gold")
                .replace("{id}", &membership_id),
            Some(json!({ "effective_to": "2026-06-01T00:00:00.000000Z" })),
            &[("if-match", &format!("\"{version}\""))],
        ))
        .await;
    assert_eq!(ended.status(), StatusCode::OK, "the interval move landed");

    let (_, body) = list_members(&harness, "gold").await;
    assert_eq!(body["memberships"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        body["memberships"][0]["effective_to"],
        json!("2026-06-01T00:00:00.000000Z"),
        "the ended interval is shown rather than filtered away"
    );
}

// ---------------------------------------------------------------------------
// D4-4: the list read is bounded and filterable.
// ---------------------------------------------------------------------------

/// **The membership list pages, and the page is the whole answer to "is there
/// more".**
///
/// This read was unpaginated and unbounded until 2026-08-18 — no `limit`, no
/// `cursor`, no filter, a bare `Vec` response and `.all(runner)` with no `LIMIT`
/// behind it — against `api/rest.rs`'s own opening sentence that every collection
/// surface paginates on an opaque cursor (D-125). The exposure is a property of the
/// table rather than of its traffic: memberships are effective-dated and ended rows
/// are deliberately kept, so a group's row count grows monotonically over a ≥7-year
/// retention and is never pruned. One response was every membership ever recorded.
///
/// The walk is asserted rather than the page size, because a `limit` that truncated
/// would satisfy a size assertion while losing rows: the two pages are concatenated
/// and compared against the whole set, which fails if the walk skips, repeats or
/// stops early.
#[tokio::test]
async fn the_membership_list_pages_and_the_walk_loses_no_row() {
    let h = Harness::new().await;
    rest_support::declare_customer_group(&h, "gold").await;

    let payers: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    for (n, payer) in payers.iter().enumerate() {
        let response = h
            .allowed_as(MEMBERSHIP_ADMIN)
            .send(with_headers(
                "POST",
                &CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold"),
                Some(json!({
                    "payer_tenant_id": payer,
                    "effective_from": "2026-01-01T00:00:00.000000Z"
                })),
                &[("idempotency-key", &format!("page-enroll-{n}"))],
            ))
            .await;
        let status = response.status();
        let seed_body = body_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "seed {n}: {seed_body}");
    }

    let members_of = async |query: String| -> serde_json::Value {
        let response = h
            .allowed_as(MEMBERSHIP_ADMIN)
            .send(request(
                "GET",
                &format!(
                    "{}{query}",
                    CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold")
                ),
                None,
            ))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    };

    let whole = members_of(String::new()).await;
    assert_eq!(
        whole["memberships"].as_array().map(Vec::len),
        Some(3),
        "the fixture seeds three, or the walk below tests nothing: {whole}"
    );
    assert!(
        whole["page_info"]["next_cursor"].is_null(),
        "one page holds them all at the default limit: {whole}"
    );

    // Two rows at a time: the first page must hand back a cursor, the second must
    // not, and the two together must be the whole set in order.
    let first = members_of("?limit=2".to_owned()).await;
    assert_eq!(first["memberships"].as_array().map(Vec::len), Some(2));
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("a page that stopped short names where to resume: {first}"))
        .to_owned();

    let second = members_of(format!("?limit=2&cursor={cursor}")).await;
    assert_eq!(second["memberships"].as_array().map(Vec::len), Some(1));
    assert!(
        second["page_info"]["next_cursor"].is_null(),
        "and the last page says so rather than pointing at an empty one: {second}"
    );

    let walked: Vec<&serde_json::Value> = first["memberships"]
        .as_array()
        .expect("an array")
        .iter()
        .chain(second["memberships"].as_array().expect("an array"))
        .collect();
    let expected: Vec<&serde_json::Value> = whole["memberships"]
        .as_array()
        .expect("an array")
        .iter()
        .collect();
    assert_eq!(
        walked, expected,
        "the walk loses no row and repeats none: {first} then {second}"
    );
}

/// **The `payer_tenant_id` filter is the by-id read this family owes.**
///
/// `api/rest.rs` holds every read-shape deviation to a stated mitigation, and this
/// family's was the only one with none: there is no `GET …/members/{id}`, and
/// before D4-4 the list had no filter either, so reaching one payer's history meant
/// paging the whole group.
///
/// The negative is the assertion — a payer who is *not* in the filter must be
/// absent — because a filter that was ignored would pass a positive-only check.
#[tokio::test]
async fn the_membership_list_narrows_to_one_payer() {
    let h = Harness::new().await;
    rest_support::declare_customer_group(&h, "gold").await;

    let wanted = Uuid::now_v7();
    let other = Uuid::now_v7();
    for (n, payer) in [wanted, other].into_iter().enumerate() {
        let response = h
            .allowed_as(MEMBERSHIP_ADMIN)
            .send(with_headers(
                "POST",
                &CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold"),
                Some(json!({
                    "payer_tenant_id": payer,
                    "effective_from": "2026-01-01T00:00:00.000000Z"
                })),
                &[("idempotency-key", &format!("filter-enroll-{n}"))],
            ))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let response = h
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(request(
            "GET",
            &format!(
                "{}?$filter=payer_tenant_id%20eq%20{wanted}",
                CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold")
            ),
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let filtered = body_json(response).await;

    let payers: Vec<&str> = filtered["memberships"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["payer_tenant_id"].as_str())
        .collect();
    assert_eq!(
        payers,
        vec![wanted.to_string().as_str()],
        "the filter narrows to the payer asked for and excludes the other: {filtered}"
    );

    // The filter key is `payer_tenant_id`, the name the rows above read back and
    // the name of the column. `payer_id` was the key until this surface shipped,
    // and it is refused rather than accepted as an alias: one value answering to
    // two filter names is what let a caller filter on a name it never read.
    let stale = h
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(request(
            "GET",
            &format!(
                "{}?$filter=payer_id%20eq%20{wanted}",
                CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold")
            ),
            None,
        ))
        .await;
    assert_eq!(
        stale.status(),
        StatusCode::BAD_REQUEST,
        "`payer_id` is not a filter field on this collection"
    );

    // A retired named key is 400, not a silent ignore and not an OData parse.
    let refused = h
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(request(
            "GET",
            &format!(
                "{}?payer_id=not-a-uuid",
                CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold")
            ),
            None,
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(refused).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| { detail.contains("payer_id") && detail.contains("$filter") }),
        "the refusal names the retired key and points at `$filter`: {problem}"
    );
}

// ---------------------------------------------------------------------------
// D-322 clause 4: the order is the effective date, not the write time.
// ---------------------------------------------------------------------------

/// Enrol one payer at a stated `effective_from`, through the real route.
///
/// The suite's [`enroll`] hardcodes one instant and one idempotency key, which is
/// exactly what an ordering test cannot use: the two rows below have to differ on
/// the effective date and agree on nothing else.
async fn enroll_from(harness: &Harness, payer: Uuid, effective_from: &str, key: &str) -> Uuid {
    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold"),
            Some(json!({
                "payer_tenant_id": payer,
                "effective_from": effective_from
            })),
            &[("idempotency-key", key)],
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "seeding `{key}` at {effective_from} must land"
    );
    let body = body_json(response).await;
    body["membership"]["membership_id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("the enrolment answers its membership id: {body}"))
}

/// **A backdated enrolment sorts by when it takes effect, not by when it was
/// typed** (D-322 clause 4).
///
/// The clause is *"keyed by group, ordered by `(effective_from, membership_id)`"*
/// and the reader it names is an auditor answering "who has been in this group",
/// which is a question in effective-date order. Until 2026-08-18 the read ordered
/// by `membership_id` alone (review Z16-1) — and that is not a near-miss for the
/// decision, because `membership_id` is a `Uuid::now_v7()` minted at the request:
/// it is time-ordered by **write** time. An operator enrolling a payer today with
/// last month's `effectiveFrom` produced a row that sorted *after* one taking
/// effect later.
///
/// The fixture is built in the order that makes the two disagree — the later
/// interval is written first — so a read that answers write order fails here and a
/// read that answers the decision's order passes.
#[tokio::test]
async fn the_membership_list_is_ordered_by_effective_date_and_not_by_write_time() {
    let h = Harness::new().await;
    rest_support::declare_customer_group(&h, "gold").await;

    let later = Uuid::now_v7();
    let backdated = Uuid::now_v7();
    // Written first, effective second.
    enroll_from(&h, later, "2026-06-01T00:00:00.000000Z", "order-later").await;
    // Written second, effective first — the backdating an operator does routinely.
    enroll_from(
        &h,
        backdated,
        "2026-01-01T00:00:00.000000Z",
        "order-backdated",
    )
    .await;

    let (status, body) = list_members(&h, "gold").await;
    assert_eq!(status, StatusCode::OK);

    let dates: Vec<&str> = body["memberships"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["effective_from"].as_str())
        .collect();
    assert_eq!(
        dates,
        vec!["2026-01-01T00:00:00.000000Z", "2026-06-01T00:00:00.000000Z"],
        "the list is ordered by the effective date the decision names: {body}"
    );
}

/// **And the walk keeps that order across pages**, which is the half a composite
/// order can silently break.
///
/// The cursor was a single `membership_id` while the order was that same column,
/// so the walk's sort key and its resume key were one thing. Ordering by
/// `(effective_from, membership_id)` and resuming from a lone id gives a keyset
/// walk whose two keys disagree: pages then skip or repeat rows. The suite's own
/// `the_membership_list_pages_and_the_walk_loses_no_row` cannot see it — it
/// compares the concatenated walk against the whole set and deliberately asserts
/// no order, so it is satisfied by any order the two agree on.
///
/// So this walks one row at a time, over a fixture whose effective order is the
/// reverse of its write order, and asserts **both** properties at once: every row
/// arrives exactly once, and they arrive in the decision's order.
#[tokio::test]
async fn the_membership_walk_holds_the_effective_date_order_across_pages() {
    let h = Harness::new().await;
    rest_support::declare_customer_group(&h, "gold").await;

    // Written newest-effective-first, so write order and effective order are
    // reverses of one another and a walk resuming on the wrong key loses a row.
    enroll_from(&h, Uuid::now_v7(), "2026-09-01T00:00:00.000000Z", "walk-3").await;
    enroll_from(&h, Uuid::now_v7(), "2026-05-01T00:00:00.000000Z", "walk-2").await;
    enroll_from(&h, Uuid::now_v7(), "2026-02-01T00:00:00.000000Z", "walk-1").await;

    let mut dates: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..4 {
        let query = match cursor.as_deref() {
            Some(token) => format!("?limit=1&cursor={token}"),
            None => "?limit=1".to_owned(),
        };
        let response = h
            .allowed_as(MEMBERSHIP_ADMIN)
            .send(request(
                "GET",
                &format!(
                    "{}{query}",
                    CUSTOMER_GROUP_MEMBERS.replace("{group}", "gold")
                ),
                None,
            ))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "page {page}");
        let body = body_json(response).await;
        for row in body["memberships"].as_array().expect("an array") {
            dates.push(
                row["effective_from"]
                    .as_str()
                    .expect("an effective date")
                    .to_owned(),
            );
        }
        cursor = body["page_info"]["next_cursor"]
            .as_str()
            .map(std::borrow::ToOwned::to_owned);
        if cursor.is_none() {
            break;
        }
    }

    assert!(
        cursor.is_none(),
        "the walk must end rather than page forever"
    );
    assert_eq!(
        dates,
        vec![
            "2026-02-01T00:00:00.000000Z".to_owned(),
            "2026-05-01T00:00:00.000000Z".to_owned(),
            "2026-09-01T00:00:00.000000Z".to_owned()
        ],
        "one row per page, every row once, in the decision's order"
    );
}

/// **A retired group still shows the memberships it accumulated** (D-322
/// clause 3).
///
/// The clause exists as a *refusal to validate*: the read does not check the group
/// against the taxonomy, because *"a retired group still holds the memberships it
/// accumulated, and refusing to show them would hide precisely what
/// `inst-cg-taxonomy`'s retire guard exists to protect"*. The handler honours it
/// and, until 2026-08-18, **nothing asserted it** (review Z16-2) — so a later
/// author adding the "missing" validation to the read, by symmetry with the write
/// door that does refuse a retired group, would have reversed a decided clause with
/// the whole suite green.
///
/// The membership is ended before the group is retired, because a **live** one is a
/// reference that `check_customer_group_retirable` refuses to retire over
/// (`TAXONOMY_VALUE_IN_USE`) — so the state the clause is about is reachable only
/// through an accumulated, ended interval, which is also the state clause 1 keeps.
#[tokio::test]
async fn the_members_of_a_retired_group_are_still_listed() {
    let h = Harness::new().await;
    let payer = Uuid::from_u128(0x_d3_22_03);
    let (_, created) = enroll(&h, payer).await;
    let membership_id = created["membership"]["membership_id"]
        .as_str()
        .expect("the id")
        .to_owned();
    let version = created["membership"]["row_version"]
        .as_u64()
        .expect("the create response carries the membership's row_version");

    let ended = h
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "PATCH",
            &CUSTOMER_GROUP_MEMBER
                .replace("{group}", "gold")
                .replace("{id}", &membership_id),
            Some(json!({ "effective_to": "2026-06-01T00:00:00.000000Z" })),
            &[("if-match", &format!("\"{version}\""))],
        ))
        .await;
    assert_eq!(ended.status(), StatusCode::OK, "the interval is closed");

    // Retire `gold` by omitting it from the whole declared set.
    let (_, tag) = read(&h).await;
    let retired = put(&h, &tag, json!([])).await;
    assert_eq!(
        retired.status(),
        StatusCode::OK,
        "an ended membership is not a live reference, so the retire is accepted"
    );
    let (taxonomy, _) = read(&h).await;
    assert_eq!(
        codes(&taxonomy),
        ["gold:retired"],
        "the group really is retired, or this test proves nothing: {taxonomy}"
    );

    let (status, body) = list_members(&h, "gold").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a retired group's members read 200, not GROUP_UNKNOWN: {body}"
    );
    assert_eq!(
        body["memberships"].as_array().map(Vec::len),
        Some(1),
        "and the accumulated membership is still there: {body}"
    );
    assert_eq!(body["memberships"][0]["payer_tenant_id"], json!(payer));
}

/// **The SQL tenant predicate on the membership adjust.**
///
/// `rest_authz.rs`'s census cannot reach it: `absent_ids` varies none of that
/// route's segments, so its `foreign` and `absent` requests would be the same
/// bytes, and the route is listed in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE`.
/// Here the membership id comes off the enrollment's own response and can be
/// varied, so the two arms differ in exactly the id whose tenant is in question.
///
/// The sibling verbs on this surface have no equivalent case and cannot: `{group}`
/// is a taxonomy **selector** with no absent state, and `POST …/members/{payerId}
/// /move` onboards a payer it finds no membership for rather than refusing one — so
/// their cross-tenant claim is that a foreign caller's write lands in the foreign
/// caller's own tenant, a property about emptiness rather than about a refusal.
///
/// `adjust_membership` resolves the row through `membership_of_group` under the
/// compiled write scope and checks nothing else about the group, so that predicate
/// is the whole of this door's object-level authority: a handler resolving `{id}`
/// before narrowing would end another tenant's membership.
#[tokio::test]
async fn a_foreign_tenant_cannot_adjust_this_tenants_membership() {
    let harness = Harness::new().await;
    let (status, body) = enroll(&harness, Uuid::now_v7()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let membership_id: Uuid = body["membership"]["membership_id"]
        .as_str()
        .expect("membership_id")
        .parse()
        .expect("a UUID");

    let adjust = |id: Uuid| {
        with_headers(
            "PATCH",
            &CUSTOMER_GROUP_MEMBER
                .replace("{group}", "gold")
                .replace("{id}", &id.to_string()),
            Some(json!({ "effective_to": "2026-03-01T00:00:00.000000Z" })),
            &[("if-match", "\"0\"")],
        )
    };
    rest_support::foreign_is_indistinguishable(
        &harness,
        adjust(membership_id),
        adjust(Uuid::now_v7()),
    )
    .await;

    // The control, and it is what makes the two refusals mean anything. Last,
    // because it ends the membership and mints a publish unit.
    let owner = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(adjust(membership_id))
        .await;
    let status = owner.status();
    let body = body_json(owner).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner's identical adjust must be accepted, or the refusals above are about the \
         request rather than about the tenant: {body}"
    );
    assert!(
        membership_row(&harness, membership_id)
            .await
            .expect("the row is there")
            .effective_to
            .is_some(),
        "and it is the owner's call that moved the end the two refused ones did not"
    );
}

// ---------------------------------------------------------------------------
// Bulk move: `POST …/members/move` (`inst-mm-bulk`).
// ---------------------------------------------------------------------------

fn bulk_move_path(group: &str) -> String {
    CUSTOMER_GROUP_MEMBERS_MOVE.replace("{group}", group)
}

#[tokio::test]
async fn a_bulk_move_with_no_payers_is_400() {
    let harness = Harness::new().await;
    rest_support::declare_customer_group(&harness, "silver").await;
    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &bulk_move_path("silver"),
            Some(json!({
                "payer_ids": [],
                "effective_from": "2026-06-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "bulk-empty-1")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_bulk_move_opens_one_unit_and_writes_no_membership_row_until_approved() {
    let harness = Harness::new().await;
    rest_support::declare_customer_group(&harness, "silver").await;
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let before = approval_rows(&harness).await.len();

    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &bulk_move_path("silver"),
            Some(json!({
                "payer_ids": [first, second],
                "effective_from": "2026-06-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "bulk-submit-1")],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");
    assert_eq!(body["outcome"], "submitted_for_approval");
    assert!(body["moved"].is_null(), "nothing has committed yet: {body}");
    assert_eq!(body["materiality"]["trigger"], "bulkGroupMove", "{body}");
    assert_eq!(approval_rows(&harness).await.len(), before + 1);

    let approval_id: Uuid = body["approval"]["approval_id"]
        .as_str()
        .expect("approval.approval_id")
        .parse()
        .expect("a UUID");
    let stored = approval_row(&harness, approval_id).await;
    assert_eq!(stored.subject_kind, AuditSubjectKind::Membership);
    assert_eq!(
        stored.materiality["trigger"], "bulkGroupMove",
        "the store must carry the act the route declared: {:?}",
        stored.materiality
    );

    let conn = harness.db.conn().expect("conn");
    for payer in [first, second] {
        let intervals = group_membership_repo::intervals_for_payer(
            &conn,
            &harness.scope(),
            harness.tenant,
            payer,
        )
        .await
        .expect("read");
        assert!(
            intervals.is_empty(),
            "no membership row may exist before the unit is approved: {payer}"
        );
    }
}

#[tokio::test]
async fn one_payer_on_the_bulk_door_is_still_always_material() {
    let harness = Harness::new().await;
    rest_support::declare_customer_group(&harness, "silver").await;
    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &bulk_move_path("silver"),
            Some(json!({
                "payer_ids": [Uuid::now_v7()],
                "effective_from": "2099-06-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "bulk-one-future-1")],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "the bulk door does not take the renewal-aligned arm: {body}"
    );
    assert_eq!(
        body["materiality"]["trigger"], "bulkGroupMove",
        "one payer on the bulk door is still the bulk act: {body}"
    );
}

#[tokio::test]
async fn a_bulk_move_commits_every_payer_once_a_second_principal_approves() {
    let harness = Harness::new().await;
    rest_support::declare_customer_group(&harness, "silver").await;
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let body = json!({
        "payer_ids": [first, second],
        "effective_from": "2026-06-01T00:00:00.000000Z"
    });

    let submit = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &bulk_move_path("silver"),
            Some(body.clone()),
            &[("idempotency-key", "bulk-commit-1")],
        ))
        .await;
    assert_eq!(submit.status(), StatusCode::ACCEPTED);
    let submit_body = body_json(submit).await;
    let approval_id: Uuid = submit_body["approval"]["approval_id"]
        .as_str()
        .expect("approval.approval_id")
        .parse()
        .expect("a UUID");

    let approve = harness
        .allowed_as(MOVE_APPROVER)
        .send(request(
            "POST",
            &APPROVAL_APPROVE.replace("{approvalId}", &approval_id.to_string()),
            None,
        ))
        .await;
    assert_eq!(approve.status(), StatusCode::OK);

    let commit = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &bulk_move_path("silver"),
            Some(body),
            &[("idempotency-key", "bulk-commit-2")],
        ))
        .await;
    let status = commit.status();
    let body = body_json(commit).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["outcome"], "committed");
    let moved = body["moved"].as_array().expect("moved[]");
    assert_eq!(moved.len(), 2, "{body}");
    let payers: Vec<_> = moved
        .iter()
        .map(|row| row["enrolled"]["payer_tenant_id"].as_str().expect("payer"))
        .collect();
    assert!(payers.contains(&first.to_string().as_str()), "{body}");
    assert!(payers.contains(&second.to_string().as_str()), "{body}");
    assert!(
        moved
            .iter()
            .all(|row| row["enrolled"]["group_value"] == "silver"),
        "{body}"
    );
}

#[tokio::test]
async fn a_bulk_move_into_an_undeclared_group_is_refused_group_unknown() {
    let harness = Harness::new().await;
    let response = harness
        .allowed_as(MEMBERSHIP_ADMIN)
        .send(with_headers(
            "POST",
            &bulk_move_path("nonexistent"),
            Some(json!({
                "payer_ids": [Uuid::now_v7()],
                "effective_from": "2026-06-01T00:00:00.000000Z"
            })),
            &[("idempotency-key", "bulk-undeclared-1")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "GROUP_UNKNOWN");
}
