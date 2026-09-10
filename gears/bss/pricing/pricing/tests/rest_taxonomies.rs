//! `GET/PUT /config/taxonomies/{class}`, driven through the real router.
//!
//! # The positive control is the whole point of this file
//!
//! Every other suite in this crate can assume a taxonomy value exists, because
//! until now the only way to make one exist was direct SQL. What this surface
//! claims is that an **operator** can declare a value and that Slice 9's overlay
//! scope rule then accepts it — so the first case here drives exactly that, end
//! to end and through HTTP: `PUT` a brand, then author a brand-scoped overlay
//! against it.
//!
//! Without that case the file would be a pile of refusals, and a surface that
//! refuses everything passes every refusal test it has. It is also the specific
//! claim the slice exists to make good: `inst-plv-scope` and `inst-tx-region`
//! both validate against these four tables, and both shipped before any of them
//! had a writer.
//!
//! # Why the `ETag` cases are not ceremony here
//!
//! The `PUT` replaces the **whole** value set, so a lost update is not a
//! last-writer-wins on one field — it is the other author's addition being
//! **retired**, which reads afterwards exactly like a value somebody meant to
//! withdraw. That is why the precondition is asserted on the concurrent path and
//! not merely on a malformed header.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::taxonomies::{TAXONOMY, TAXONOMY_VALUE, TAXONOMY_VALUES};
use bss_pricing::authz::{actions, labels};
use rest_support::{
    Harness, approval_row, approval_rows, audit_rows, body_json, etag_of, location_of,
    problem_code, request, with_headers,
};
use serde_json::json;
use uuid::Uuid;

/// The `CatalogAdmin` who configures the taxonomies.
const ADMIN: uuid::Uuid = uuid::Uuid::from_u128(0xca_d0);
/// The second principal a **referenced** value's edit needs (D-353, D-355): a
/// different actor from `ADMIN`, because `independent_approver` refuses a
/// self-approval.
const REVIEWER: uuid::Uuid = uuid::Uuid::from_u128(0xa_c0);

fn path(class: &str) -> String {
    TAXONOMY.replace("{class}", class)
}

/// Read one taxonomy, answering the body and the tag together.
///
/// The two are taken from **one** response deliberately: a helper that read the
/// body and then re-read for a tag could hand a caller a tag describing a
/// different state than the body it was given, which is the exact failure the
/// precondition exists to catch.
async fn read(harness: &Harness, class: &str) -> (serde_json::Value, String) {
    let response = harness
        .allowed_as(ADMIN)
        .send(request("GET", &path(class), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "the GET must answer 200");
    let tag = etag_of(&response).expect("a taxonomy read must carry its entity tag");
    (body_json(response).await, tag)
}

fn values_path(class: &str) -> String {
    TAXONOMY_VALUES.replace("{class}", class)
}

fn value_path(class: &str, value: &str) -> String {
    TAXONOMY_VALUE
        .replace("{class}", class)
        .replace("{value}", value)
}

/// `POST …/values` with one value.
async fn declare(
    harness: &Harness,
    class: &str,
    body: serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    harness
        .allowed_as(ADMIN)
        .send(with_headers("POST", &values_path(class), Some(body), &[]))
        .await
}

/// A brand declared through this surface is one a brand-scoped overlay can name.
///
/// **The positive control across two surfaces.** The version of this that D-353
/// removed drove the declare half through the whole-set `PUT`, which is gone —
/// but the overlay half went with it, and what replaced it in this file seeds
/// `price_overlay` rows straight through the entity, which never asks
/// `inst-plv-scope` anything. The only non-`global` scopes left in
/// `rest_overlays.rs` both assert refusals, so a declare door that stored the
/// value spelled differently from the column the scope rule queries — trimmed,
/// cased, normalised — would leave every suite green while no brand overlay
/// could ever publish. Both halves go through HTTP here for that reason.
///
/// `CREATED`, not merely "some code other than `SCOPE_VALUE_UNKNOWN`": the weaker
/// assertion is satisfied by a malformed request, which is what this case did on
/// its first run — answering 400 for a missing field while "proving" the scope
/// rule accepted the brand.
#[tokio::test]
async fn a_brand_declared_here_is_one_a_brand_scoped_overlay_can_name() {
    let harness = Harness::new().await;

    let declared = declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme Corp" }),
    )
    .await;
    assert_eq!(
        declared.status(),
        StatusCode::CREATED,
        "the brand is declared: {}",
        body_json(declared).await
    );

    let overlay = harness
        .allowed_as(ADMIN)
        .send(with_headers(
            "POST",
            "/bss-pricing/v1/price-overlays",
            Some(json!({
                "scope_class": "brand",
                "scope_value": "acme",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [{
                    "adjustment_kind": "discount",
                    "magnitude_kind": "percent_bp",
                    "adjustment_value": 500,
                }]
            })),
            &[("idempotency-key", "brand-overlay-1")],
        ))
        .await;
    let status = overlay.status();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a brand declared through this surface must satisfy inst-plv-scope: if this fails, the \
         write surface and the read the scope rule makes of it disagree about what `declared` \
         means. Body: {}",
        body_json(overlay).await
    );
}

/// `GET …/values/{value}`: the body and the value's own tag.
async fn read_value(harness: &Harness, class: &str, value: &str) -> (serde_json::Value, String) {
    let response = harness
        .allowed_as(ADMIN)
        .send(request("GET", &value_path(class, value), None))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the value GET must answer 200"
    );
    let tag = etag_of(&response).expect("a value read must carry its entity tag");
    (body_json(response).await, tag)
}

/// `PATCH …/values/{value}` under the given tag.
async fn patch(
    harness: &Harness,
    class: &str,
    value: &str,
    tag: &str,
    body: serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    harness
        .allowed_as(ADMIN)
        .send(with_headers(
            "PATCH",
            &value_path(class, value),
            Some(body),
            &[("if-match", tag)],
        ))
        .await
}

/// One **published** overlay scoped to `(class, value)`, written through the
/// entity.
///
/// The authoring route cannot produce this state in one call — a submit opens an
/// always-material approval unit (D-50) — and what is under test here is the
/// taxonomy guard, not the overlay lifecycle.
async fn seed_published_overlay(harness: &Harness, class: &str, value: &str) {
    seed_published_overlay_n(harness, class, value, 0).await;
}

/// The same, with an explicit ordinal so one test can seed **several** scopes,
/// each at its own precedence.
///
/// The ids were fixed literals until D-355 put the two reference counts on the
/// wire: with one hardcoded primary key a second call is a `UNIQUE` violation
/// inside the helper rather than a second reference, which capped every
/// assertion about those counts at 1 and left the two-plane refusal
/// `check_retirable` renders untestable end to end.
async fn seed_published_overlay_n(harness: &Harness, class: &str, value: &str, nth: u128) {
    use sea_orm::ActiveValue::Set;
    use sea_orm::EntityTrait;
    use toolkit_db::secure::{AccessScope, SecureInsertExt};

    let conn = harness.db.conn().expect("conn");
    let row = bss_pricing::infra::storage::entity::price_overlay::ActiveModel {
        price_overlay_id: Set(uuid::Uuid::from_u128(0x0e_9a_00 + nth)),
        revision: Set(1),
        tenant_id: Set(harness.tenant),
        lifecycle_state: Set("published".to_owned()),
        scope_class: Set(class.to_owned()),
        scope_value: Set(value.to_owned()),
        // Distinct per call: `uq_pricing_price_overlay_precedence` is unique on
        // `(tenant_id, scope_class, precedence)` among published rows, so the
        // ordinal has to move the precedence and not merely the id. That index
        // is also the real bound on how many published overlays of one class a
        // tenant can hold, hence on `active_overlay_scopes`.
        precedence: Set(20 + i32::try_from(nth).expect("a small ordinal")),
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

/// One **published** price row naming `region` on its axis — the row plane
/// `references_to` counts for `region` alone.
///
/// Written through the entity because the authoring route cannot reach
/// `published` in one call, and what is under test is the governance gate, not
/// the publish lifecycle. Mirrors `sqlite_taxonomy_repo::publish_price_row_in`.
async fn seed_published_price_row(harness: &Harness, region: &str) {
    seed_published_price_row_n(harness, region, 0).await;
}

/// The same, with an explicit ordinal — [`seed_published_overlay_n`]'s reason.
/// The plan revision is shared across calls (one plan, many rows), so only the
/// row id moves.
async fn seed_published_price_row_n(harness: &Harness, region: &str, nth: u128) {
    use sea_orm::ActiveValue::Set;
    use sea_orm::EntityTrait;
    use toolkit_db::secure::{AccessScope, SecureInsertExt};

    let conn = harness.db.conn().expect("conn");
    let stamped = time::OffsetDateTime::now_utc();
    let plan_id = uuid::Uuid::from_u128(0x91a4);
    let plan_row = bss_pricing::infra::storage::entity::plan::ActiveModel {
        plan_id: Set(plan_id),
        revision: Set(1),
        tenant_id: Set(harness.tenant),
        lifecycle_state: Set("published".to_owned()),
        created_by: Set(uuid::Uuid::from_u128(0x4444)),
        created_at_utc: Set(stamped),
        ..Default::default()
    };
    // One plan carries every seeded row, so a second call finds it already
    // there — the row is what must be distinct, not its plan.
    let planted = bss_pricing::infra::storage::entity::plan::Entity::insert(plan_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &plan_row)
        .expect("scope")
        .exec(&conn)
        .await;
    if nth == 0 {
        planted.expect("seed the plan revision");
    }

    let price_row = bss_pricing::infra::storage::entity::price::ActiveModel {
        price_id: Set(uuid::Uuid::from_u128(0xb0_01_00 + nth)),
        tenant_id: Set(harness.tenant),
        plan_id: Set(plan_id),
        currency: Set("EUR".to_owned()),
        region: Set(region.to_owned()),
        price_overlay: Set("base".to_owned()),
        phase: Set(uuid::Uuid::from_u128(0xf1)),
        price_eligibility: Set("all_subscriptions".to_owned()),
        charge_kind: Set("recurring".to_owned()),
        cohort: Set("none".to_owned()),
        // Distinct per call: `uq_pricing_price_scope_key_current` is unique over
        // the whole eleven-column scope key among published rows, and `region`
        // is the one column this seeder must hold fixed — so the ordinal moves
        // `dimension_key`, the axis with no meaning of its own here.
        dimension_key: Set(if nth == 0 {
            String::new()
        } else {
            format!("d{nth}")
        }),
        tax_inclusive: Set(false),
        lifecycle_state: Set("published".to_owned()),
        created_by: Set(uuid::Uuid::from_u128(0x4444)),
        created_at_utc: Set(stamped),
        row_version: Set(0),
        ..Default::default()
    };
    bss_pricing::infra::storage::entity::price::Entity::insert(price_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &price_row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the price row");
}

/// Take every published overlay of this tenant out of the published state.
///
/// The only way an already-referenced value legitimately becomes unreferenced,
/// and the reason it matters: the governance gate is a snapshot, so a test that
/// wants the *second* half of a value's life has to be able to end the first.
async fn supersede_published_overlays(harness: &Harness) {
    use sea_orm::{ColumnTrait, Condition, EntityTrait};
    use toolkit_db::secure::{AccessScope, SecureUpdateExt};

    let conn = harness.db.conn().expect("conn");
    bss_pricing::infra::storage::entity::price_overlay::Entity::update_many()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .col_expr(
            bss_pricing::infra::storage::entity::price_overlay::Column::LifecycleState,
            sea_orm::sea_query::Expr::value("superseded"),
        )
        .filter(Condition::all().add(
            bss_pricing::infra::storage::entity::price_overlay::Column::TenantId.eq(harness.tenant),
        ))
        .exec(&conn)
        .await
        .expect("supersede the seeded overlays");
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

/// Approve one unit as the second principal.
async fn approve(harness: &Harness, approval_id: Uuid) {
    let response = harness
        .selectively_allowed_as(REVIEWER, &[(labels::APPROVAL, actions::APPROVE)])
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "approve must answer 200");
}

/// The approval a `202` names.
async fn unit_of(response: axum::http::Response<axum::body::Body>) -> Uuid {
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(body["outcome"], "submitted_for_approval", "{body}");
    assert!(body["value"].is_null(), "nothing has committed yet: {body}");
    body["approval"]["approval_id"]
        .as_str()
        .expect("approval.approval_id")
        .parse()
        .expect("a UUID")
}

/// The whole governed edit: submit (`202`), approve as the second principal,
/// then GET the committed value (`200`). No second PATCH is needed.
async fn patch_governed(
    harness: &Harness,
    class: &str,
    value: &str,
    tag: &str,
    body: serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    let approval_id = unit_of(patch(harness, class, value, tag, body).await).await;
    approve(harness, approval_id).await;
    let committed = harness
        .allowed_as(ADMIN)
        .send(request("GET", &value_path(class, value), None))
        .await;
    assert_eq!(
        committed.status(),
        StatusCode::OK,
        "approve applied the edit"
    );
    committed
}

// ---------------------------------------------------------------------------
// The set read.
// ---------------------------------------------------------------------------

/// A tenant with no taxonomy is answered `200` with an empty list **and a tag**:
/// a state, not an absent resource. Its tag covers authored content only.
#[tokio::test]
async fn a_tenant_with_no_values_reads_200_with_an_empty_list_and_a_tag() {
    let harness = Harness::new().await;
    let (body, tag) = read(&harness, "brand").await;
    assert_eq!(body["class"], "brand");
    assert_eq!(body["values"].as_array().map(Vec::len), Some(0));
    assert!(!tag.is_empty());

    let conditional = harness
        .allowed_as(ADMIN)
        .send(with_headers(
            "GET",
            &path("brand"),
            None,
            &[("if-none-match", &tag)],
        ))
        .await;
    assert_eq!(conditional.status(), StatusCode::OK);
    assert_eq!(conditional.headers()["cache-control"], "private, no-store");
    assert_eq!(conditional.headers()["etag"], tag);
    assert_eq!(body_json(conditional).await, body);
}

/// `global` and `customerGroup` are not addressable, and the refusal names the four.
#[tokio::test]
async fn an_unaddressable_class_is_refused_naming_the_four() {
    let harness = Harness::new().await;
    for segment in ["global", "customerGroup", "orgTier", "colour"] {
        let response = harness
            .allowed_as(ADMIN)
            .send(request("GET", &path(segment), None))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{segment}");
        let body = body_json(response).await;
        let detail = body.to_string();
        for named in ["region", "brand", "partner", "org_tier"] {
            assert!(
                detail.contains(named),
                "{segment}: the refusal must name `{named}`: {detail}"
            );
        }
    }
}

/// The last class is one token, `org_tier`, on the wire and in the echo (D-241).
#[tokio::test]
async fn the_org_tier_class_is_addressed_and_echoed_as_one_token() {
    let harness = Harness::new().await;
    let created = declare(
        &harness,
        "org_tier",
        json!({ "value": "gold", "display_name": "Gold" }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let (body, _) = read(&harness, "org_tier").await;
    assert_eq!(body["class"], "org_tier");
    assert_eq!(codes(&body), ["gold:active"]);
}

// ---------------------------------------------------------------------------
// D-353: declaring a value — `POST …/values`, committed at once.
// ---------------------------------------------------------------------------

/// A `POST` declares one value: `201`, the value's own tag, a `Location` naming
/// it, and both reads see it.
#[tokio::test]
async fn a_post_declares_one_value_and_both_reads_see_it() {
    let harness = Harness::new().await;
    let (_, set_tag_before) = read(&harness, "brand").await;

    let created = declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        location_of(&created).as_deref(),
        Some(value_path("brand", "acme").as_str())
    );
    let created_tag = etag_of(&created).expect("a create carries the value's tag");
    let body = body_json(created).await;
    assert_eq!(body["value"], "acme");
    assert_eq!(
        body["state"], "active",
        "a declared value is active unless said otherwise"
    );

    let (one, read_tag) = read_value(&harness, "brand", "acme").await;
    assert_eq!(one["display_name"], "Acme");
    assert_eq!(
        read_tag, created_tag,
        "the create and the read render one tag"
    );

    let (set, set_tag_after) = read(&harness, "brand").await;
    assert_eq!(codes(&set), ["acme:active"]);
    assert_ne!(
        set_tag_before, set_tag_after,
        "the set's tag moves when a value joins it"
    );
}

/// The value is its own key: the same body again is the create's replay (200,
/// nothing written); other content for a held value is `409` naming the code.
#[tokio::test]
async fn a_repeated_post_replays_and_a_conflicting_one_is_409() {
    let harness = Harness::new().await;
    let body = json!({ "value": "acme", "display_name": "Acme" });
    assert_eq!(
        declare(&harness, "brand", body.clone()).await.status(),
        StatusCode::CREATED
    );
    let before = audit_rows(&harness).await.len();

    let replay = declare(&harness, "brand", body).await;
    assert_eq!(
        replay.status(),
        StatusCode::OK,
        "same body, same value: a replay"
    );
    assert_eq!(
        audit_rows(&harness).await.len(),
        before,
        "a replay writes nothing"
    );

    let conflicting = declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "ACME Ltd" }),
    )
    .await;
    assert_eq!(conflicting.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(conflicting).await, "TAXONOMY_VALUE_EXISTS");

    let (one, _) = read_value(&harness, "brand", "acme").await;
    assert_eq!(
        one["display_name"], "Acme",
        "a refused declaration writes nothing"
    );
}

/// The region universe alone carries D-01's two markers; they round-trip there
/// and are refused — not dropped — elsewhere.
#[tokio::test]
async fn the_tax_markers_round_trip_on_the_region_taxonomy_and_are_refused_elsewhere() {
    let harness = Harness::new().await;
    let created = declare(
        &harness,
        "region",
        json!({ "value": "apac", "display_name": "APAC", "tax_category": "vat_standard", "tax_rate_present": true }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let (one, _) = read_value(&harness, "region", "apac").await;
    assert_eq!(one["tax_category"], "vat_standard");
    assert_eq!(one["tax_rate_present"], true);

    let refused = declare(
        &harness,
        "brand",
        json!({ "value": "b2", "display_name": "B", "tax_category": "x" }),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let (set, _) = read(&harness, "brand").await;
    assert_eq!(
        codes(&set),
        Vec::<String>::new(),
        "a refused declaration writes nothing"
    );
}

/// A blank value is refused at the edge: the empty string is the store's sentinel
/// for the classless overlay scope.
#[tokio::test]
async fn a_blank_value_is_refused_at_the_edge() {
    let harness = Harness::new().await;
    for blank in ["", "   "] {
        let refused = declare(
            &harness,
            "brand",
            json!({ "value": blank, "display_name": "X" }),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{blank:?}");
    }
    let missing = harness
        .allowed_as(ADMIN)
        .send(request("GET", &value_path("brand", "%20"), None))
        .await;
    assert_eq!(
        missing.status(),
        StatusCode::BAD_REQUEST,
        "a blank segment is not an id"
    );
}

// ---------------------------------------------------------------------------
// D-353: editing a value — `PATCH …/values/{value}`, the governed door.
// ---------------------------------------------------------------------------

/// The first PATCH opens a unit; approve atomically commits the value and audit.
///
/// **This is also D-355's referenced arm.** The overlay seed below is what keeps
/// the door governed at all, so a regression that ungoverned a *referenced*
/// value reddens here — on the case that already asserts the unit count, the
/// pinned subject, the materiality reason and the tag's movement, rather than in
/// a second test that could only fail where this one already fails.
#[tokio::test]
async fn a_patch_opens_a_unit_writes_nothing_and_commits_once_approved() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    // Referenced, so the edit stays the governed door (D-355).
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let before = approval_rows(&harness).await.len();

    let approval_id = unit_of(
        patch(
            &harness,
            "brand",
            "acme",
            &tag,
            json!({ "display_name": "ACME Ltd" }),
        )
        .await,
    )
    .await;
    assert_eq!(
        approval_rows(&harness).await.len(),
        before + 1,
        "one edit, one unit"
    );
    let (one, tag_after_submit) = read_value(&harness, "brand", "acme").await;
    assert_eq!(
        one["display_name"], "Acme",
        "nothing is written before the second principal"
    );
    assert_eq!(tag_after_submit, tag, "and the value's tag has not moved");
    let stored = approval_row(&harness, approval_id).await;
    assert_eq!(
        stored.subject_kind,
        bss_pricing::domain::audit::AuditSubjectKind::TaxonomyValue
    );
    assert_eq!(
        stored.materiality["reason"], "alwaysMaterialTrigger",
        "{:?}",
        stored.materiality
    );
    assert!(
        stored.subject_ref.starts_with("taxonomy-value/"),
        "the proposal rides the ref: {}",
        stored.subject_ref
    );

    approve(&harness, approval_id).await;
    let committed = harness
        .allowed_as(ADMIN)
        .send(request("GET", &value_path("brand", "acme"), None))
        .await;
    assert_eq!(committed.status(), StatusCode::OK);
    let new_tag = etag_of(&committed).expect("the commit answers with the new tag");
    assert_ne!(new_tag, tag);
    let body = body_json(committed).await;
    assert_eq!(body["display_name"], "ACME Ltd");
    assert_eq!(
        body["state"], "active",
        "unnamed fields are left as they were"
    );
    assert_eq!(
        approval_row(&harness, approval_id).await.state,
        bss_pricing::domain::approval::ApprovalState::Approved
    );
    let count = audit_rows(&harness).await.len();
    let retry = harness
        .allowed_as(REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(retry.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(retry).await, "APPROVAL_NOT_PENDING");
    assert_eq!(
        audit_rows(&harness).await.len(),
        count,
        "retry does not reapply or audit"
    );
    let stale_patch = patch(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({"display_name": "ACME Ltd"}),
    )
    .await;
    assert_eq!(stale_patch.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(stale_patch).await, "STALE_VERSION");
}

/// A new published dependency can invalidate a patch without moving its pin.
/// Domain guard failure must roll back the verdict, value and decision audit.
#[tokio::test]
async fn a_taxonomy_guard_failure_rolls_back_approve_and_its_audit() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "region",
        json!({
            "value": "guard-test", "display_name": "Europe", "tax_category": "vat_standard"
        }),
    )
    .await;
    seed_published_overlay(&harness, "region", "guard-test").await;
    let (_, tag) = read_value(&harness, "region", "guard-test").await;
    let approval_id = unit_of(
        patch(
            &harness,
            "region",
            "guard-test",
            &tag,
            json!({"tax_category": null}),
        )
        .await,
    )
    .await;
    seed_published_price_row(&harness, "guard-test").await;
    let before = audit_rows(&harness).await.len();
    let response = harness
        .allowed_as(REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "TAXONOMY_VALUE_IN_USE");
    let unit = approval_row(&harness, approval_id).await;
    assert_eq!(
        unit.state,
        bss_pricing::domain::approval::ApprovalState::Submitted
    );
    assert!(unit.approver_principal.is_none());
    assert!(unit.decided_at.is_none());
    assert_eq!(audit_rows(&harness).await.len(), before);
    let (value, after_tag) = read_value(&harness, "region", "guard-test").await;
    assert_eq!(value["tax_category"], "vat_standard");
    assert_eq!(after_tag, tag);
}

/// A resource-pinned approval scope must reach its own taxonomy effect, but
/// never a different approval. The flat HTTP fixture cannot model this pin.
#[tokio::test]
async fn a_resource_pinned_taxonomy_approval_applies_only_its_own_subject() {
    use bss_pricing::domain::approval::{ApprovalState, DecisionBy, WithdrawAuthority};
    use bss_pricing::domain::audit::AuditStamp;
    use bss_pricing::infra::approval::{ApprovalService, DecideRequest, RegionGrant};
    use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};

    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({"value": "acme", "display_name": "Acme"}),
    )
    .await;
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let approval_id = unit_of(
        patch(
            &harness,
            "brand",
            "acme",
            &tag,
            json!({"display_name": "ACME Ltd"}),
        )
        .await,
    )
    .await;
    let scoped_to = |id| {
        AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![harness.tenant]),
            ScopeFilter::in_uuids(pep_properties::RESOURCE_ID, vec![id]),
        ]))
    };
    let service = ApprovalService::new(harness.db.clone());
    let now = time::OffsetDateTime::now_utc();
    let request = DecideRequest {
        approval_id,
        decision: DecisionBy::Approve(REVIEWER),
        reason: None,
        approver_regions: RegionGrant::Explicit(std::collections::BTreeSet::new()),
        stamp: AuditStamp {
            actor_principal_id: REVIEWER,
            recorded_at: now,
            correlation_id: Uuid::now_v7(),
        },
        withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
    };
    let wrong = scoped_to(Uuid::now_v7());
    assert!(
        service
            .find(&wrong, harness.tenant, approval_id, now)
            .await
            .expect("read")
            .is_none()
    );
    assert!(
        service
            .decide(&wrong, harness.tenant, request.clone())
            .await
            .is_err()
    );
    assert_eq!(
        approval_row(&harness, approval_id).await.state,
        ApprovalState::Submitted
    );
    let right = scoped_to(approval_id);
    assert!(
        service
            .find(&right, harness.tenant, approval_id, now)
            .await
            .expect("read")
            .expect("unit")
            .content_matches_pin
    );
    service
        .decide(&right, harness.tenant, request)
        .await
        .expect("atomic approve under pinned scope");
    assert_eq!(
        read_value(&harness, "brand", "acme").await.0["display_name"],
        "ACME Ltd"
    );
    assert_eq!(
        approval_row(&harness, approval_id).await.state,
        ApprovalState::Approved
    );
    assert_eq!(
        audit_rows(&harness)
            .await
            .iter()
            .filter(|row| row.subject_ref == "taxonomy/brand/acme"
                && row.action == "update"
                && row.approval_ref == Some(approval_id))
            .count(),
        1
    );
}

/// The reviewer reads what they sign for: the value as held and as the edit
/// would leave it, and `content_matches_pin`.
#[tokio::test]
async fn the_unit_shows_the_value_before_and_after() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "region",
        json!({ "value": "apac", "display_name": "APAC" }),
    )
    .await;
    // Referenced, so the edit stays the governed door (D-355).
    seed_published_overlay(&harness, "region", "apac").await;
    let (_, tag) = read_value(&harness, "region", "apac").await;
    let approval_id = unit_of(
        patch(
            &harness,
            "region",
            "apac",
            &tag,
            json!({ "display_name": "Europe", "tax_category": "vat_standard" }),
        )
        .await,
    )
    .await;

    let detail = harness
        .allowed_as(REVIEWER)
        .send(request(
            "GET",
            &format!("/bss-pricing/v1/approvals/{approval_id}"),
            None,
        ))
        .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let body = body_json(detail).await;
    let pinned = &body["pinned_taxonomy_value"];
    assert_eq!(pinned["class"], "region", "{body}");
    assert_eq!(pinned["value"], "apac");
    assert_eq!(pinned["before"]["display_name"], "APAC");
    assert!(pinned["before"]["tax_category"].is_null());
    assert_eq!(pinned["after"]["display_name"], "Europe");
    assert_eq!(pinned["after"]["tax_category"], "vat_standard");
    assert_eq!(body["content_matches_pin"], true);
}

/// A relabel moves the value's tag and the set's tag, and leaves a sibling's
/// alone — the reason the route exists beside a whole-set write.
#[tokio::test]
async fn a_committed_relabel_moves_this_values_tag_and_not_its_siblings() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    declare(
        &harness,
        "brand",
        json!({ "value": "zenith", "display_name": "Zenith" }),
    )
    .await;
    // Referenced, so the edit stays the governed door (D-355).
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, acme_tag) = read_value(&harness, "brand", "acme").await;
    let (_, zenith_before) = read_value(&harness, "brand", "zenith").await;
    let (_, set_before) = read(&harness, "brand").await;

    patch_governed(
        &harness,
        "brand",
        "acme",
        &acme_tag,
        json!({ "display_name": "ACME Ltd" }),
    )
    .await;

    let (_, acme_after) = read_value(&harness, "brand", "acme").await;
    assert_ne!(acme_after, acme_tag);
    let (_, zenith_after) = read_value(&harness, "brand", "zenith").await;
    assert_eq!(zenith_before, zenith_after, "a sibling's tag does not move");
    let (_, set_after) = read(&harness, "brand").await;
    assert_ne!(set_before, set_after, "the set's tag does");
}

/// The `PATCH` asserts the value's **own** tag: the set's tag does not satisfy
/// it, a tag from the same value in another class does not, a consumed one is
/// `409 STALE_VERSION`, an absent one is `400`.
#[tokio::test]
async fn a_patch_asserts_the_values_own_tag() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    declare(
        &harness,
        "partner",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    // Referenced, so the edit stays the governed door (D-355).
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, set_tag) = read(&harness, "brand").await;
    let (_, brand_tag) = read_value(&harness, "brand", "acme").await;
    let (_, partner_tag) = read_value(&harness, "partner", "acme").await;
    assert_ne!(
        brand_tag, partner_tag,
        "one value in two classes is two resources"
    );

    for wrong in [&set_tag, &partner_tag] {
        let refused = patch(
            &harness,
            "brand",
            "acme",
            wrong,
            json!({ "display_name": "X" }),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        assert_eq!(problem_code(refused).await, "STALE_VERSION");
    }
    let without = harness
        .allowed_as(ADMIN)
        .send(with_headers(
            "PATCH",
            &value_path("brand", "acme"),
            Some(json!({ "display_name": "X" })),
            &[],
        ))
        .await;
    assert_eq!(without.status(), StatusCode::BAD_REQUEST);

    patch_governed(
        &harness,
        "brand",
        "acme",
        &brand_tag,
        json!({ "display_name": "Y" }),
    )
    .await;
    let stale = patch(
        &harness,
        "brand",
        "acme",
        &brand_tag,
        json!({ "display_name": "Z" }),
    )
    .await;
    assert_eq!(
        stale.status(),
        StatusCode::CONFLICT,
        "the tag was consumed by the commit"
    );
    assert_eq!(problem_code(stale).await, "STALE_VERSION");
    assert_eq!(
        read_value(&harness, "brand", "acme").await.0["display_name"],
        "Y"
    );
}

/// A **referenced** value's retirement is refused before any unit opens; an
/// **unreferenced** value's retirement (and re-activation) is the operator's own
/// — a direct `200` (D-355).
///
/// The two halves are the whole of what retirement now is: the guard answers
/// `409` with the declared code, opens nothing and writes nothing, and what it
/// admits is ungoverned, so there is no third case in which a retirement waits
/// on a second principal.
#[tokio::test]
async fn a_referenced_retirement_is_refused_and_an_unreferenced_one_commits_directly() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let before = approval_rows(&harness).await.len();

    let refused = patch(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({ "state": "retired" }),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(refused).await, "TAXONOMY_VALUE_IN_USE");
    assert_eq!(
        approval_rows(&harness).await.len(),
        before,
        "a refused edit opens no unit"
    );
    let (one, tag_after) = read_value(&harness, "brand", "acme").await;
    assert_eq!(one["state"], "active");
    assert_eq!(tag_after, tag);

    declare(
        &harness,
        "brand",
        json!({ "value": "zenith", "display_name": "Zenith" }),
    )
    .await;
    let (_, tag) = read_value(&harness, "brand", "zenith").await;
    // D-355: nothing published names `zenith`, so its retirement is the
    // operator's own — a direct 200, no unit.
    let retired = patch(
        &harness,
        "brand",
        "zenith",
        &tag,
        json!({ "state": "retired" }),
    )
    .await;
    assert_eq!(retired.status(), StatusCode::OK);
    let tag = etag_of(&retired).expect("tag");
    assert_eq!(
        body_json(retired).await["state"],
        "retired",
        "retirement is a state, not a deletion"
    );
    let (set, _) = read(&harness, "brand").await;
    assert!(
        codes(&set).contains(&"zenith:retired".to_owned()),
        "still listed: {:?}",
        codes(&set)
    );
    let back = patch(
        &harness,
        "brand",
        "zenith",
        &tag,
        json!({ "state": "active" }),
    )
    .await;
    assert_eq!(back.status(), StatusCode::OK);
    assert_eq!(body_json(back).await["state"], "active");
}

/// On the `region` universe `taxCategory` has three spellings — absent (keep),
/// a string (set), `null` (clear) — and the markers are refused on the other
/// three universes rather than dropped.
#[tokio::test]
async fn a_patch_sets_keeps_and_clears_the_region_tax_category() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "region",
        json!({ "value": "apac", "display_name": "APAC" }),
    )
    .await;
    // Referenced by a published **overlay** and not a price row, so the three
    // spellings stay the governed door (D-355) while D-245's clear guard --
    // which counts published price rows resolving through the region -- still
    // has nothing to count.
    seed_published_overlay(&harness, "region", "apac").await;
    let (_, tag) = read_value(&harness, "region", "apac").await;

    let set = patch_governed(
        &harness,
        "region",
        "apac",
        &tag,
        json!({ "tax_category": "vat_standard", "tax_rate_present": true }),
    )
    .await;
    let tag = etag_of(&set).expect("tag");
    let body = body_json(set).await;
    assert_eq!(body["tax_category"], "vat_standard");
    assert_eq!(body["tax_rate_present"], true);

    let kept = patch_governed(
        &harness,
        "region",
        "apac",
        &tag,
        json!({ "display_name": "Europe" }),
    )
    .await;
    let tag = etag_of(&kept).expect("tag");
    assert_eq!(
        body_json(kept).await["tax_category"],
        "vat_standard",
        "absent keeps"
    );

    let cleared = patch_governed(
        &harness,
        "region",
        "apac",
        &tag,
        json!({ "tax_category": null }),
    )
    .await;
    assert!(
        body_json(cleared).await["tax_category"].is_null(),
        "null clears"
    );

    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let refused = patch(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({ "tax_rate_present": true }),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
}

/// A body that changes nothing is not an act: `200`, no unit, no record.
#[tokio::test]
async fn a_patch_that_changes_nothing_opens_no_unit() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let units = approval_rows(&harness).await.len();
    let records = audit_rows(&harness).await.len();

    let noop = patch(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({ "display_name": "Acme" }),
    )
    .await;
    assert_eq!(noop.status(), StatusCode::OK);
    assert_eq!(
        etag_of(&noop).as_deref(),
        Some(tag.as_str()),
        "the value under its unchanged tag"
    );
    assert_eq!(approval_rows(&harness).await.len(), units);
    assert_eq!(audit_rows(&harness).await.len(), records);
}

/// The pin is a TOCTOU guard: a unit decided over a value that has since moved
/// cannot commit — the reviewer signed a different document.
#[tokio::test]
async fn a_unit_over_a_value_that_moved_underneath_it_cannot_commit() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    // Referenced, so the edit stays the governed door (D-355).
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;

    let relabel = unit_of(
        patch(
            &harness,
            "brand",
            "acme",
            &tag,
            json!({ "display_name": "ACME Ltd" }),
        )
        .await,
    )
    .await;
    // A second, different edit of the same value commits first.
    patch_governed(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({ "display_name": "Acme Corp" }),
    )
    .await;

    let detail = harness
        .allowed_as(REVIEWER)
        .send(request(
            "GET",
            &format!("/bss-pricing/v1/approvals/{relabel}"),
            None,
        ))
        .await;
    let body = body_json(detail).await;
    assert_eq!(
        body["content_matches_pin"], false,
        "the held value is not the one pinned: {body}"
    );

    let decided = harness
        .allowed_as(REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{relabel}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        decided.status(),
        StatusCode::CONFLICT,
        "an approve over a moved document is refused"
    );
    assert_eq!(problem_code(decided).await, "APPROVAL_CONTENT_MISMATCH");
    assert_eq!(
        read_value(&harness, "brand", "acme").await.0["display_name"],
        "Acme Corp"
    );
}

/// A second edit of the same value while the first waits is its own unit; the
/// same edit twice is the pending unit, named.
#[tokio::test]
async fn the_same_edit_resubmitted_names_the_pending_unit() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    // Referenced, so the edit stays the governed door (D-355).
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let first = unit_of(
        patch(
            &harness,
            "brand",
            "acme",
            &tag,
            json!({ "display_name": "ACME Ltd" }),
        )
        .await,
    )
    .await;

    let again = patch(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({ "display_name": "ACME Ltd" }),
    )
    .await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(again).await, "PENDING_CHANGE_UNIT_EXISTS");
    let body_text = {
        let response = harness
            .allowed_as(REVIEWER)
            .send(request(
                "GET",
                &format!("/bss-pricing/v1/approvals/{first}"),
                None,
            ))
            .await;
        body_json(response).await.to_string()
    };
    assert!(body_text.contains(&first.to_string()));
}

// ---------------------------------------------------------------------------
// D-355: which edits are governed at all.
// ---------------------------------------------------------------------------

/// D-355: while nothing published names the value, an edit commits at once —
/// no unit, `200`, and one audited `update` record under no approval.
#[tokio::test]
async fn an_unreferenced_value_edit_commits_without_approval() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let units = approval_rows(&harness).await.len();

    let committed = patch(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({ "display_name": "ACME Ltd" }),
    )
    .await;
    assert_eq!(
        committed.status(),
        StatusCode::OK,
        "an unreferenced edit commits on the first call"
    );
    let new_tag = etag_of(&committed).expect("the commit answers with the new tag");
    assert_ne!(new_tag, tag);
    assert_eq!(body_json(committed).await["display_name"], "ACME Ltd");
    assert_eq!(
        approval_rows(&harness).await.len(),
        units,
        "no approval unit is opened"
    );

    let updated = audit_rows(&harness)
        .await
        .into_iter()
        .rfind(|r| r.subject_ref == "taxonomy/brand/acme")
        .expect("the commit's record");
    assert_eq!(updated.action, "update");
    assert!(
        updated.approval_ref.is_none(),
        "an ungoverned commit names no unit"
    );
}

/// D-355 on the region markers: an unreferenced region's tax markers are set
/// without a second principal; the value plane the operator actually touches.
#[tokio::test]
async fn an_unreferenced_region_marker_edit_commits_without_approval() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "region",
        json!({ "value": "apac", "display_name": "APAC" }),
    )
    .await;
    let (_, tag) = read_value(&harness, "region", "apac").await;
    let units = approval_rows(&harness).await.len();

    let committed = patch(
        &harness,
        "region",
        "apac",
        &tag,
        json!({ "tax_category": "vat_standard", "tax_rate_present": true }),
    )
    .await;
    assert_eq!(
        committed.status(),
        StatusCode::OK,
        "unreferenced: direct commit"
    );
    let body = body_json(committed).await;
    assert_eq!(body["tax_category"], "vat_standard");
    assert_eq!(body["tax_rate_present"], true);
    assert_eq!(approval_rows(&harness).await.len(), units, "no unit");
}

/// D-355: a region named by a published **price row** (not an overlay) is
/// governed too — the row plane `references_to` counts for `region` alone.
#[tokio::test]
async fn a_region_named_by_a_published_price_row_is_governed() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "region",
        json!({ "value": "eu", "display_name": "EU" }),
    )
    .await;
    seed_published_price_row(&harness, "eu").await;
    let (_, tag) = read_value(&harness, "region", "eu").await;

    let opened = patch(
        &harness,
        "region",
        "eu",
        &tag,
        json!({ "display_name": "Europe" }),
    )
    .await;
    assert_eq!(
        opened.status(),
        StatusCode::ACCEPTED,
        "a referenced region's edit opens a unit"
    );
}

/// A unit already pending over this exact edit refuses it, even once the value
/// has become unreferenced.
///
/// The sequence is a real one: the edit opens a unit while a published overlay
/// names the value, the overlay is then superseded, and the operator re-sends.
/// Without the check on the ungoverned arm that re-send commits unilaterally and
/// leaves its own unit `submitted` over a change that already happened — a unit
/// no approver can act on, since the approve fails closed on the re-derived pin,
/// and one that never leaves the queue.
#[tokio::test]
async fn a_pending_unit_refuses_the_same_edit_once_the_reference_goes_away() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;

    // Governed while referenced: this opens the unit and writes nothing.
    let approval_id = unit_of(
        patch(
            &harness,
            "brand",
            "acme",
            &tag,
            json!({ "display_name": "ACME Ltd" }),
        )
        .await,
    )
    .await;

    supersede_published_overlays(&harness).await;
    assert_eq!(
        read_value(&harness, "brand", "acme").await.0["edit_governed"],
        false,
        "the reference is gone, so the value is now editable alone"
    );

    let refused = patch(
        &harness,
        "brand",
        "acme",
        &tag,
        json!({ "display_name": "ACME Ltd" }),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(refused).await, "PENDING_CHANGE_UNIT_EXISTS");
    assert_eq!(
        read_value(&harness, "brand", "acme").await.0["display_name"],
        "Acme",
        "the refusal wrote nothing"
    );
    assert_eq!(
        approval_row(&harness, approval_id).await.state,
        bss_pricing::domain::approval::ApprovalState::Submitted,
        "and the unit the operator must still decide is untouched"
    );
}

/// Both reference planes count past one, and on `region` both are non-zero at
/// once — the shape the retire guard's two-plane refusal actually renders.
///
/// Asserted because the counts are `u64` on the wire and every other case pins
/// them at 0 or 1: a renderer that reported `bool`-as-integer, or that counted
/// one plane twice, would pass all of those.
#[tokio::test]
async fn the_reference_counts_are_counts_and_not_flags() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "region",
        json!({ "value": "eu", "display_name": "EU" }),
    )
    .await;
    seed_published_overlay_n(&harness, "region", "eu", 0).await;
    seed_published_overlay_n(&harness, "region", "eu", 1).await;
    seed_published_price_row_n(&harness, "eu", 0).await;
    seed_published_price_row_n(&harness, "eu", 1).await;
    seed_published_price_row_n(&harness, "eu", 2).await;

    let (body, _) = read_value(&harness, "region", "eu").await;
    assert_eq!(body["edit_governed"], true, "{body}");
    assert_eq!(body["references"]["active_overlay_scopes"], 2, "{body}");
    assert_eq!(body["references"]["published_price_rows"], 3, "{body}");
}

// ---------------------------------------------------------------------------
// D-355 read side: the signal a UI labels its buttons from.
// ---------------------------------------------------------------------------

/// The by-value read reports whether an edit of it is governed, and the counts
/// behind that answer.
///
/// Read twice over the **same** value rather than over two values: what is
/// asserted is that the signal follows the reference appearing, which a pair of
/// separately-seeded values cannot distinguish from a per-value constant.
#[tokio::test]
async fn the_value_read_reports_whether_its_edit_is_governed() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;

    let (before, tag) = read_value(&harness, "brand", "acme").await;
    let (_, set_tag) = read(&harness, "brand").await;
    assert_eq!(before["edit_governed"], false, "{before}");
    assert_eq!(before["references"]["published_price_rows"], 0);
    assert_eq!(before["references"]["active_overlay_scopes"], 0);

    seed_published_overlay(&harness, "brand", "acme").await;
    let (after, _) = read_value(&harness, "brand", "acme").await;
    assert_eq!(after["edit_governed"], true, "{after}");
    assert_eq!(after["references"]["active_overlay_scopes"], 1);
    for (uri, old_tag) in [(path("brand"), set_tag), (value_path("brand", "acme"), tag)] {
        let response = harness
            .allowed_as(ADMIN)
            .send(with_headers(
                "GET",
                &uri,
                None,
                &[("if-none-match", &old_tag)],
            ))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["etag"], old_tag);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let body = body_json(response).await;
        let value = body.get("values").map_or(&body, |values| &values[0]);
        assert_eq!(value["references"]["active_overlay_scopes"], 1);
    }
}

/// A region named by a published **price row** reports its count on the row
/// plane — the plane only `region` has.
#[tokio::test]
async fn the_region_read_counts_the_published_price_row_plane() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "region",
        json!({ "value": "eu", "display_name": "EU" }),
    )
    .await;
    seed_published_price_row(&harness, "eu").await;

    let (body, _) = read_value(&harness, "region", "eu").await;
    assert_eq!(body["edit_governed"], true, "{body}");
    assert_eq!(body["references"]["published_price_rows"], 1);
    let (set, _) = read(&harness, "region").await;
    let listed = set["values"]
        .as_array()
        .expect("values")
        .iter()
        .find(|value| value["value"] == "eu")
        .expect("listed region");
    assert_eq!(listed["references"], body["references"]);
}

/// The list carries the same counts as detail, including explicit zeroes.
///
/// Both arms in one list, because the claim is that the flag discriminates
/// within a single read — a list on which every value answered the same thing
/// would pass an assertion on either value alone.
#[tokio::test]
async fn the_set_read_marks_each_values_governed_flag() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    declare(
        &harness,
        "brand",
        json!({ "value": "zenith", "display_name": "Zenith" }),
    )
    .await;
    seed_published_overlay(&harness, "brand", "acme").await;

    let (set, _) = read(&harness, "brand").await;
    let by_value: std::collections::HashMap<String, &serde_json::Value> = set["values"]
        .as_array()
        .expect("values")
        .iter()
        .map(|v| (v["value"].as_str().expect("value").to_owned(), v))
        .collect();
    assert_eq!(by_value["acme"]["edit_governed"], true, "{set}");
    assert_eq!(by_value["zenith"]["edit_governed"], false, "{set}");
    assert_eq!(
        by_value["acme"]["references"],
        json!({
            "published_price_rows": 0, "active_overlay_scopes": 1
        })
    );
    assert_eq!(
        by_value["zenith"]["references"],
        json!({
            "published_price_rows": 0, "active_overlay_scopes": 0
        })
    );
    let (detail, _) = read_value(&harness, "brand", "acme").await;
    assert_eq!(by_value["acme"]["references"], detail["references"]);
}

/// D-354's seeded `global` reads as ungoverned: the seed exists so a fresh
/// tenant can publish at once, and a ceremony on its first edit would be the
/// cost it was added to remove.
#[tokio::test]
async fn the_seeded_global_reads_as_ungoverned() {
    let harness = Harness::new().await;
    let one = harness
        .other_tenant()
        .send(request("GET", &value_path("region", "global"), None))
        .await;
    assert_eq!(one.status(), StatusCode::OK);
    let body = body_json(one).await;
    assert_eq!(body["edit_governed"], false, "{body}");
    assert_eq!(body["references"]["published_price_rows"], 0, "{body}");
}

// ---------------------------------------------------------------------------
// Audit and tenant isolation.
// ---------------------------------------------------------------------------

/// A declaration is one `create` record naming the value; a committed edit is
/// one `update` record naming the value, its state before and after, and the
/// unit it ran under — the diff a whole-set record could not carry.
#[tokio::test]
async fn per_value_mutations_are_audited_naming_the_value_with_before_and_after() {
    let harness = Harness::new().await;
    let before = audit_rows(&harness).await.len();
    declare(
        &harness,
        "brand",
        json!({ "value": "acme", "display_name": "Acme" }),
    )
    .await;
    let rows = audit_rows(&harness).await;
    assert_eq!(rows.len(), before + 1, "one declaration, one record");
    let created = rows.last().expect("a record");
    assert_eq!(created.subject_ref, "taxonomy/brand/acme");
    assert_eq!(created.subject_kind, "taxonomy_value");
    assert_eq!(created.action, "create");
    assert!(created.before_state.is_none());
    assert_eq!(
        created.after_state.as_ref().map(|s| s["state"].clone()),
        Some(json!("active"))
    );
    assert!(
        created.approval_ref.is_none(),
        "declaring runs under no unit"
    );

    // The record under test is the **governed** commit's, so the value has to
    // be referenced (D-355); the ungoverned twin, whose record names no unit,
    // is `an_unreferenced_value_edit_commits_without_approval`.
    seed_published_overlay(&harness, "brand", "acme").await;
    let (_, tag) = read_value(&harness, "brand", "acme").await;
    let approval_id = unit_of(
        patch(
            &harness,
            "brand",
            "acme",
            &tag,
            json!({ "display_name": "ACME Ltd" }),
        )
        .await,
    )
    .await;
    let taxonomy_records_before_commit = audit_rows(&harness)
        .await
        .iter()
        .filter(|r| r.subject_ref == "taxonomy/brand/acme")
        .count();
    assert_eq!(
        taxonomy_records_before_commit, 1,
        "submit writes no taxonomy mutation"
    );
    approve(&harness, approval_id).await;

    let rows = audit_rows(&harness).await;
    let updated = rows
        .iter()
        .rfind(|r| r.subject_ref == "taxonomy/brand/acme")
        .expect("the commit's record");
    assert_eq!(updated.action, "update");
    assert_eq!(updated.actor_principal_id, REVIEWER);
    assert_eq!(
        updated
            .before_state
            .as_ref()
            .map(|s| s["display_name"].clone()),
        Some(json!("Acme"))
    );
    assert_eq!(
        updated
            .after_state
            .as_ref()
            .map(|s| s["display_name"].clone()),
        Some(json!("ACME Ltd"))
    );
    assert_eq!(
        updated.approval_ref,
        Some(approval_id),
        "the commit names the unit it ran under"
    );
}

/// Another tenant's value reads and patches exactly like an absent one, and the
/// owner's value is untouched — the by-id census's twin for the `PATCH`, whose
/// per-value tag the shared fixture cannot obtain.
#[tokio::test]
async fn a_foreign_tenants_value_reads_and_patches_like_an_absent_one() {
    use sea_orm::ActiveValue::Set;
    use sea_orm::EntityTrait;
    use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};

    let harness = Harness::new().await;
    let foreign_tenant = Uuid::now_v7();
    let conn = harness.db.conn().expect("conn");
    let row = bss_pricing::infra::storage::entity::brand_taxonomy::ActiveModel {
        tenant_id: Set(foreign_tenant),
        value: Set("theirs".to_owned()),
        display_name: Set("Theirs".to_owned()),
        state: Set("active".to_owned()),
    };
    bss_pricing::infra::storage::entity::brand_taxonomy::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the foreign tenant's value");

    let foreign = harness
        .allowed_as(ADMIN)
        .send(request("GET", &value_path("brand", "theirs"), None))
        .await;
    let absent = harness
        .allowed_as(ADMIN)
        .send(request("GET", &value_path("brand", "nobody"), None))
        .await;
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    let (foreign, absent) = (body_json(foreign).await, body_json(absent).await);
    assert_eq!(
        foreign["type"], absent["type"],
        "one problem type for both: {foreign} vs {absent}"
    );

    // A well-formed tag no read handed out: the handler must reach the store's
    // answer (absent), not refuse the header.
    let some_tag = format!("\"{}\"", "0".repeat(64));
    let patched = patch(
        &harness,
        "brand",
        "theirs",
        &some_tag,
        json!({ "display_name": "Mine" }),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::NOT_FOUND);
    let untouched = bss_pricing::infra::storage::entity::brand_taxonomy::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read")
        .into_iter()
        .find(|r| r.tenant_id == foreign_tenant)
        .expect("the foreign row");
    assert_eq!(untouched.display_name, "Theirs");
}

// ---------------------------------------------------------------------------
// D-354: the seeded region, on the wire.
// ---------------------------------------------------------------------------

/// A tenant that has declared no region reads the seeded `global` — on the set
/// and by value, with a tag — while its other universes stay empty and a tenant
/// that declared its own regions is not given one.
#[tokio::test]
async fn a_fresh_tenants_region_universe_is_the_seeded_global() {
    let harness = Harness::new().await;

    let set = harness
        .other_tenant()
        .send(request("GET", &path("region"), None))
        .await;
    assert_eq!(set.status(), StatusCode::OK);
    let body = body_json(set).await;
    assert_eq!(codes(&body), ["global:active"]);
    let seed = &body["values"][0];
    assert_eq!(seed["display_name"], "Global");
    assert!(
        seed["tax_category"].is_null(),
        "no tax fact asserted: {seed}"
    );
    assert_eq!(seed["tax_rate_present"], false);

    let one = harness
        .other_tenant()
        .send(request("GET", &value_path("region", "global"), None))
        .await;
    assert_eq!(one.status(), StatusCode::OK);
    assert!(
        etag_of(&one).is_some(),
        "the virtual value carries a tag like any other"
    );

    let brands = harness
        .other_tenant()
        .send(request("GET", &path("brand"), None))
        .await;
    assert_eq!(
        codes(&body_json(brands).await),
        Vec::<String>::new(),
        "only the region universe is seeded"
    );

    // The harness tenant declared its fixture regions, so it holds rows and is
    // not seeded.
    let (own, _) = read(&harness, "region").await;
    assert!(
        !codes(&own).contains(&"global:active".to_owned()),
        "a tenant with rows is not given the seed: {:?}",
        codes(&own)
    );
}

/// A referenced label with a real pending proposal, prepared through HTTP.
async fn pending_rename(harness: &Harness, class: &str, value: &str, label: &str) -> Uuid {
    let (_, tag) = read_value(harness, class, value).await;
    unit_of(patch(harness, class, value, &tag, json!({"display_name": label})).await).await
}

/// List/detail use the stored proposal, keep the live name, and retain stale
/// sibling units rather than selecting one pending edit or auto-voiding it.
#[tokio::test]
async fn taxonomy_pending_preview_tracks_multiple_proposals_and_atomic_apply() {
    let harness = Harness::new().await;
    let created = declare(
        &harness,
        "brand",
        json!({"value":"acme", "display_name":"Acme"}),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert!(body_json(created).await.get("pending_approvals").is_none());
    let rejected_read_field = declare(
        &harness,
        "brand",
        json!({
            "value":"forged", "display_name":"Forged", "pending_approvals":[],
        }),
    )
    .await;
    assert_eq!(rejected_read_field.status(), StatusCode::BAD_REQUEST);
    let (empty, _) = read_value(&harness, "brand", "acme").await;
    assert_eq!(empty["pending_approvals"], json!([]));
    let (_, original_tag) = read_value(&harness, "brand", "acme").await;
    let rejected_read_field = patch(
        &harness,
        "brand",
        "acme",
        &original_tag,
        json!({"display_name":"Must not save", "pending_approvals":[]}),
    )
    .await;
    assert_eq!(rejected_read_field.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_value(&harness, "brand", "acme").await.0["display_name"],
        "Acme"
    );
    seed_published_overlay(&harness, "brand", "acme").await;
    let first = pending_rename(&harness, "brand", "acme", "ACME Ltd").await;
    let (one, _) = read_value(&harness, "brand", "acme").await;
    assert_eq!(one["pending_approvals"].as_array().unwrap().len(), 1);
    let second = pending_rename(&harness, "brand", "acme", "ACME Corp").await;
    let detail = harness
        .allowed_as(ADMIN)
        .send(request(
            "GET",
            &bss_pricing::api::rest::approvals::APPROVAL
                .replace("{approvalId}", &first.to_string()),
            None,
        ))
        .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let detail = body_json(detail).await;
    for side in ["before", "after"] {
        let pinned = &detail["pinned_taxonomy_value"][side];
        assert!(pinned.is_object(), "pinned authored {side}: {detail}");
        assert!(pinned.get("pending_approvals").is_none());
    }
    let (before, tag) = read_value(&harness, "brand", "acme").await;
    assert_eq!(before["display_name"], "Acme");
    let units = before["pending_approvals"].as_array().unwrap();
    assert_eq!(units.len(), 2);
    for (unit, id, name) in [
        (&units[0], first, "ACME Ltd"),
        (&units[1], second, "ACME Corp"),
    ] {
        assert_eq!(unit["approval_id"], json!(id));
        assert_eq!(unit["subject_kind"], "taxonomy_value");
        assert_eq!(unit["content_access"], "granted");
        assert_eq!(unit["proposed_changes"], json!({"display_name": name}));
        assert_eq!(unit["content_matches_pin"], true);
        assert!(unit.get("subject_ref").is_none());
        assert!(unit.get("submitter_principal").is_none());
    }
    let (list, _) = read(&harness, "brand").await;
    assert_eq!(list["values"][0], before);
    for uri in [path("brand"), value_path("brand", "acme")] {
        let (client, seen) = harness.recording();
        let response = client.send(request("GET", &uri, None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let asked = seen.lock().expect("recorder");
        let questions: Vec<_> = asked
            .iter()
            .map(|request| {
                (
                    request.resource.resource_type.as_str(),
                    request.action.name.as_str(),
                    request.context.require_constraints,
                )
            })
            .collect();
        assert_eq!(
            questions,
            [
                (labels::CONFIG, actions::READ, true),
                (labels::APPROVAL, actions::READ, true),
            ],
            "one preview PDP request for all pending units"
        );
    }
    // Derived metadata does not change the authoring tag, but even a conditional
    // request gets the current pending list, not a misleading 304.
    let conditional = harness
        .allowed_as(ADMIN)
        .send(with_headers(
            "GET",
            &value_path("brand", "acme"),
            None,
            &[("if-none-match", &tag)],
        ))
        .await;
    assert_eq!(conditional.status(), StatusCode::OK);
    assert_eq!(conditional.headers()["cache-control"], "private, no-store");
    assert_eq!(conditional.headers()["etag"], tag);
    assert_eq!(body_json(conditional).await, before);

    approve(&harness, first).await;
    let (after, new_tag) = read_value(&harness, "brand", "acme").await;
    assert_eq!(after["display_name"], "ACME Ltd");
    assert_ne!(new_tag, tag);
    assert_eq!(after["pending_approvals"].as_array().unwrap().len(), 1);
    assert_eq!(after["pending_approvals"][0]["approval_id"], json!(second));
    assert_eq!(after["pending_approvals"][0]["content_matches_pin"], false);
    assert_eq!(
        after["pending_approvals"][0]["proposed_changes"],
        json!({"display_name":"ACME Corp"})
    );
    let approval_path =
        bss_pricing::api::rest::approvals::APPROVAL.replace("{approvalId}", &second.to_string());
    let rejected = harness
        .allowed_as(REVIEWER)
        .send(request(
            "POST",
            &format!("{approval_path}/reject"),
            Some(json!({"reason":"superseded"})),
        ))
        .await;
    assert_eq!(rejected.status(), StatusCode::OK);
    let (done, _) = read_value(&harness, "brand", "acme").await;
    assert_eq!(done["pending_approvals"], json!([]));
}

/// Denied or foreign approval-read scopes keep metadata but expose no content.
#[tokio::test]
async fn config_read_alone_cannot_see_a_pending_taxonomy_patch() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({"value":"acme", "display_name":"Acme"}),
    )
    .await;
    seed_published_overlay(&harness, "brand", "acme").await;
    let id = pending_rename(&harness, "brand", "acme", "Confidential label").await;
    let clients = [
        harness.selectively_allowed_as(ADMIN, &[(labels::CONFIG, actions::READ)]),
        harness.selectively_allowed_as_with_foreign_pair(
            ADMIN,
            &[
                (labels::CONFIG, actions::READ),
                (labels::APPROVAL, actions::READ),
            ],
            (labels::APPROVAL, actions::READ),
        ),
    ];
    for client in clients {
        for uri in [path("brand"), value_path("brand", "acme")] {
            // A tag obtained with full access must not bypass a subsequent
            // restricted/outage PDP decision via an early 304.
            let original = harness
                .allowed_as(ADMIN)
                .send(request("GET", &uri, None))
                .await;
            let tag = etag_of(&original).expect("tag");
            let response = client
                .send(with_headers("GET", &uri, None, &[("if-none-match", &tag)]))
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["cache-control"], "private, no-store");
            let body = body_json(response).await;
            assert!(!body.to_string().contains("Confidential label"));
            let value = body.get("values").map_or(&body, |values| &values[0]);
            assert_eq!(value["display_name"], "Acme");
            let pending = value["pending_approvals"].as_array().unwrap();
            assert_eq!(pending.len(), 1);
            let unit = pending[0].as_object().unwrap();
            assert_eq!(
                unit.len(),
                4,
                "only three metadata fields plus access status"
            );
            assert_eq!(unit["approval_id"], json!(id));
            assert_eq!(unit["content_access"], "restricted");
        }
    }
}

/// The region marker preview preserves keep, clear, set and explicit false.
#[tokio::test]
async fn region_pending_patch_preserves_null_and_false_without_unchanged_fields() {
    let harness = Harness::new().await;
    let created = declare(&harness, "region", json!({
        "value":"preview", "display_name":"Preview", "tax_category":"standard", "tax_rate_present":true,
    })).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    seed_published_overlay(&harness, "region", "preview").await;
    let (_, tag) = read_value(&harness, "region", "preview").await;
    let clear = json!({"tax_category":null, "tax_rate_present":false});
    unit_of(patch(&harness, "region", "preview", &tag, clear.clone()).await).await;
    let set = json!({"tax_category":"reduced"});
    unit_of(patch(&harness, "region", "preview", &tag, set.clone()).await).await;
    let (value, _) = read_value(&harness, "region", "preview").await;
    assert_eq!(value["tax_category"], "standard");
    assert_eq!(value["tax_rate_present"], true);
    assert_eq!(value["pending_approvals"][0]["proposed_changes"], clear);
    assert_eq!(value["pending_approvals"][1]["proposed_changes"], set);
}

/// Returns a resource-pinned config scope and an independent approval scope,
/// or fails only the approval question. Uniform allow doubles cannot test this.
struct PreviewResolver {
    tenant: Uuid,
    approval_ids: Vec<Uuid>,
    approval_unavailable: bool,
}

#[async_trait::async_trait]
impl authz_resolver_sdk::AuthZResolverApi for PreviewResolver {
    async fn evaluate(
        &self,
        _ctx: toolkit_security::PlatformSecurityContext,
        req: authz_resolver_sdk::models::EvaluationRequest,
    ) -> Result<
        authz_resolver_sdk::models::EvaluationResponse,
        toolkit_canonical_errors::CanonicalError,
    > {
        use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
        use authz_resolver_sdk::models::{EvaluationResponse, EvaluationResponseContext};
        use toolkit_security::pep_properties;
        let is_approval = req.resource.resource_type == labels::APPROVAL;
        if is_approval && self.approval_unavailable {
            return Err(toolkit_canonical_errors::CanonicalError::service_unavailable().create());
        }
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![
                        Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            vec![self.tenant],
                        )),
                        Predicate::In(InPredicate::new(
                            pep_properties::RESOURCE_ID,
                            if is_approval {
                                self.approval_ids.clone()
                            } else {
                                vec![self.tenant]
                            },
                        )),
                    ],
                }],
                deny_reason: None,
            },
        })
    }
}

/// Row restrictions, not merely tenant membership, determine preview access.
/// An unavailable PDP cannot look like a legitimate restricted preview.
#[tokio::test]
async fn taxonomy_preview_respects_resource_scopes_and_reports_pdp_outages() {
    let harness = Harness::new().await;
    declare(
        &harness,
        "brand",
        json!({"value":"acme", "display_name":"Acme"}),
    )
    .await;
    seed_published_overlay(&harness, "brand", "acme").await;
    let first = pending_rename(&harness, "brand", "acme", "Allowed label").await;
    let second = pending_rename(&harness, "brand", "acme", "Hidden label").await;
    for unavailable in [false, true] {
        let client = harness.client_as(
            std::sync::Arc::new(PreviewResolver {
                tenant: harness.tenant,
                approval_ids: vec![first],
                approval_unavailable: unavailable,
            }),
            Some((harness.tenant, ADMIN)),
        );
        for uri in [path("brand"), value_path("brand", "acme")] {
            let response = client.send(request("GET", &uri, None)).await;
            if unavailable {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                let body = body_json(response).await;
                assert!(body.get("pending_approvals").is_none());
                continue;
            }
            assert_eq!(response.status(), StatusCode::OK);
            let body = body_json(response).await;
            let value = body.get("values").map_or(&body, |values| &values[0]);
            let units = value["pending_approvals"].as_array().unwrap();
            assert_eq!(units.len(), 2);
            assert_eq!(units[0]["approval_id"], json!(first));
            assert_eq!(units[0]["content_access"], "granted");
            assert_eq!(
                units[0]["proposed_changes"],
                json!({"display_name":"Allowed label"})
            );
            assert_eq!(units[1]["approval_id"], json!(second));
            assert_eq!(units[1]["content_access"], "restricted");
            assert!(!body.to_string().contains("Hidden label"));
        }
    }
}
