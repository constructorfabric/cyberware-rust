//! `GET/PUT /bss-pricing/v1/config/gl-codes` — the general-ledger codes a tenant
//! declares (D-356) — and what declaring them does to a plan's publish.
//!
//! # What is under test, and why the publish trio comes first
//!
//! The vocabulary's own surface is `rounding-policies`' one document over, and
//! its cases below are that suite's. The three publish cases are the reason the
//! surface exists: a plan whose descriptor `glCode` is **in** the declared set
//! publishes, one whose code is **outside a non-empty** set is refused
//! `GL_CODE_UNKNOWN`, and one on a tenant that declared **nothing** publishes any
//! code at all. The refusal is asserted first, because the empty set means
//! "unconstrained" and an unwired `rule_params` looks exactly like a satisfied
//! vocabulary — refusing is what proves the set reached the rule (the D-254
//! defect class `infra::publish` records having paid for once already).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::gl_codes::GL_CODES;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::entity::{gl_code_taxonomy, plan, plan_descriptor_set};
use bss_pricing::infra::storage::repo::NewPriceDraft;
use rest_support::{
    Harness, body_json, etag_of, problem_code, publishable_row, publishable_scope_key,
    seed_publishable_shape, with_headers,
};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{AccessScope, SecureInsertExt, SecureUpdateExt};
use uuid::Uuid;

/// The submitting principal.
const SUBMITTER: Uuid = Uuid::from_u128(0x5_7e);

// ---------------------------------------------------------------------------
// What declaring the vocabulary does to publish (D-356).
// ---------------------------------------------------------------------------

/// A publishable plan with one priced, covered row. `seed_publishable_shape`
/// authors the descriptor `glCode` **`4000`**, which is the code every case here
/// declares, omits or ignores.
async fn seed_priced_plan(harness: &Harness, plan_id: Uuid, case_seq: u128) -> String {
    let shape = seed_publishable_shape(harness, plan_id).await;
    let plan = PlanId::new(plan_id);
    let scope = harness.scope();

    let price_id = Uuid::now_v7();
    harness
        .state
        .prices
        .create_draft(
            &scope,
            harness.tenant,
            NewPriceDraft {
                price_id,
                scope_key: publishable_scope_key(plan, shape.phase, "eu"),
                content: publishable_row(),
                created_by: rest_support::SEED_ACTOR,
                created_at_utc: rest_support::at(10),
                correlation_id: Uuid::from_u128(0x_61_c0_de_00 + case_seq),
            },
        )
        .await
        .expect("author the row");

    // `inst-wc-required`: no row publishes without a window on its canonical key.
    let conn = harness.state.db.conn().expect("conn");
    common::schedule_coverage_window(
        &conn,
        &scope,
        harness.tenant,
        price_id,
        rest_support::seed_stamp(),
    )
    .await;
    shape.etag()
}

/// **The vocabulary, end to end**: an undeclared code is refused at publish, and
/// declaring it lets the same plan through.
///
/// The pair matters more than either half, for `rest_rounding_policy`'s reason:
/// a test that only declared the code and published would pass against a rule
/// whose operand nobody loaded.
#[tokio::test]
async fn an_undeclared_gl_code_is_refused_and_declaring_it_lets_the_plan_publish() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let etag = seed_priced_plan(&harness, plan_id, 1).await;

    // A vocabulary that does not contain the descriptor's `4000`.
    declare_gl_code(&harness, "4010-TAX").await;

    let refused = publish(&harness, plan_id, &etag).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let detail = body_json(refused).await.to_string();
    assert!(
        detail.contains("GL_CODE_UNKNOWN"),
        "the refusal names the vocabulary rule; got {detail}"
    );
    assert!(
        detail.contains("`4000`"),
        "and the code the author typed, so they know what to declare or fix; got {detail}"
    );

    declare_gl_code(&harness, "4000").await;

    let after = publish(&harness, plan_id, &etag).await;
    let status = after.status();
    let body = body_json(after).await;
    // **202 and an opened unit**, not merely "the code is absent": a publish that
    // failed for some other reason would satisfy an absence assertion just as well.
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "the declared code clears the rule; got {body}"
    );
    assert_eq!(body["outcome"], serde_json::json!("submitted_for_approval"));
}

/// An empty vocabulary constrains nothing — the opt-in reading (D-356).
///
/// The negative control for the case above, and the single property that let
/// the table land: without it, every plan whose `glCode` nobody had declared
/// would have failed publish on the day of the migration.
#[tokio::test]
async fn a_tenant_with_no_declared_vocabulary_publishes_any_gl_code() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let etag = seed_priced_plan(&harness, plan_id, 2).await;

    let after = publish(&harness, plan_id, &etag).await;
    assert_eq!(
        after.status(),
        StatusCode::ACCEPTED,
        "declaring nothing is not opting in; got {}",
        body_json(after).await
    );
}

/// Declare one GL code straight at the table.
///
/// Direct because the publish rule's operand is what these cases are about, not
/// the vocabulary surface — `rest_rounding_policy::declare_rounding_value`
/// carries the same reasoning. The surface's own cases are below.
async fn declare_gl_code(harness: &Harness, value: &str) {
    let conn = harness.db.conn().expect("conn");
    let row = gl_code_taxonomy::ActiveModel {
        tenant_id: Set(harness.tenant),
        value: Set(value.to_owned()),
        display_name: Set(format!("fixture {value}")),
        state: Set("active".to_owned()),
    };
    gl_code_taxonomy::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope the value")
        .exec(&conn)
        .await
        .expect("declare the value");
}

async fn publish(
    harness: &Harness,
    plan_id: Uuid,
    tag: &str,
) -> axum::http::Response<axum::body::Body> {
    harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
            None,
            &[("if-match", tag)],
        ))
        .await
}

// ---------------------------------------------------------------------------
// The vocabulary's own surface (D-356) — `rest_rounding_policy`'s cases, one
// document over.
// ---------------------------------------------------------------------------

async fn read_vocabulary(harness: &Harness) -> (StatusCode, Option<String>, serde_json::Value) {
    let response = harness
        .allowed()
        .send(with_headers("GET", GL_CODES, None, &[]))
        .await;
    let status = response.status();
    let tag = etag_of(&response);
    (status, tag, body_json(response).await)
}

async fn write_vocabulary(
    harness: &Harness,
    values: serde_json::Value,
    tag: &str,
) -> axum::http::Response<axum::body::Body> {
    harness
        .allowed()
        .send(with_headers(
            "PUT",
            GL_CODES,
            Some(serde_json::json!({ "values": values })),
            &[("if-match", tag)],
        ))
        .await
}

/// Declare one code through the surface, under the tag the store currently
/// renders, and assert it landed.
async fn put_one(harness: &Harness, value: &str) {
    let (_, tag, _) = read_vocabulary(harness).await;
    let response = write_vocabulary(
        harness,
        serde_json::json!([{ "value": value, "display_name": format!("GL {value}") }]),
        &tag.expect("a tag"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        body_json(response).await
    );
}

/// A tenant that has declared nothing reads an empty set **with a tag** — the
/// state every tenant starts in, and the one the bootstrap `PUT` asserts.
#[tokio::test]
async fn an_undeclared_vocabulary_reads_empty_with_a_tag() {
    let harness = Harness::new().await;

    let (status, tag, body) = read_vocabulary(&harness).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["values"], serde_json::json!([]));
    assert_eq!(body["resource"], serde_json::json!("gl-codes"));
    assert!(tag.is_some(), "the empty set is a state and carries a tag");
}

/// A `PUT` declares the set and the `GET` agrees; state defaults to `active`.
#[tokio::test]
async fn a_declared_set_round_trips_and_defaults_to_active() {
    let harness = Harness::new().await;
    let (_, tag, _) = read_vocabulary(&harness).await;

    let response = write_vocabulary(
        &harness,
        serde_json::json!([
            { "value": "4010-TAX", "display_name": "Sales tax payable" },
            { "value": "4000-REV", "display_name": "Revenue" }
        ]),
        &tag.expect("a tag"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = read_vocabulary(&harness).await;
    assert_eq!(
        body["values"],
        serde_json::json!([
            { "value": "4000-REV", "display_name": "Revenue", "state": "active" },
            { "value": "4010-TAX", "display_name": "Sales tax payable", "state": "active" }
        ]),
        "ordered by value, active unless said otherwise"
    );
}

/// An omitted value is **retired, not deleted**, and the retirement is visible
/// in the next read so an operator can re-activate it.
#[tokio::test]
async fn an_omitted_value_is_retired_and_stays_readable() {
    let harness = Harness::new().await;
    let (_, tag, _) = read_vocabulary(&harness).await;
    write_vocabulary(
        &harness,
        serde_json::json!([
            { "value": "4000-REV", "display_name": "Revenue" },
            { "value": "4010-TAX", "display_name": "Sales tax payable" }
        ]),
        &tag.expect("a tag"),
    )
    .await;

    let (_, tag, _) = read_vocabulary(&harness).await;
    let response = write_vocabulary(
        &harness,
        serde_json::json!([{ "value": "4000-REV", "display_name": "Revenue" }]),
        &tag.expect("a tag"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = read_vocabulary(&harness).await;
    assert_eq!(body["values"][1]["value"], serde_json::json!("4010-TAX"));
    assert_eq!(
        body["values"][1]["state"],
        serde_json::json!("retired"),
        "retired rather than gone: {body}"
    );
}

/// A stale tag on the vocabulary is refused **as a stale tag** and writes nothing.
#[tokio::test]
async fn a_stale_vocabulary_tag_is_refused() {
    let harness = Harness::new().await;
    let (_, first, _) = read_vocabulary(&harness).await;
    let first = first.expect("a tag");
    write_vocabulary(
        &harness,
        serde_json::json!([{ "value": "4000-REV", "display_name": "Revenue" }]),
        &first,
    )
    .await;

    let response = write_vocabulary(
        &harness,
        serde_json::json!([{ "value": "4010-TAX", "display_name": "Sales tax payable" }]),
        &first,
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    // Which 409: this route answers `TAXONOMY_VALUE_IN_USE` on the same status.
    assert_eq!(problem_code(response).await, "STALE_VERSION");
    let (_, _, body) = read_vocabulary(&harness).await;
    assert_eq!(body["values"][0]["value"], serde_json::json!("4000-REV"));
    assert_eq!(body["values"].as_array().map(Vec::len), Some(1));
}

/// A blank value is refused by the surface rather than by a constraint, and a
/// refused write leaves the set exactly where it was.
#[tokio::test]
async fn a_blank_vocabulary_value_is_refused() {
    let harness = Harness::new().await;
    // The refusal needs something to lose: a readback against the empty default
    // is satisfied by a refusal that wiped the set on its way out.
    put_one(&harness, "4000-REV").await;
    let (_, tag, before) = read_vocabulary(&harness).await;
    assert_eq!(before["values"].as_array().map(Vec::len), Some(1));

    let response = write_vocabulary(
        &harness,
        serde_json::json!([{ "value": "   ", "display_name": "nothing" }]),
        &tag.expect("a tag"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (_, _, after) = read_vocabulary(&harness).await;
    assert_eq!(after["values"], before["values"]);
}

/// A value listed twice is refused by the surface rather than swallowed by the
/// key — `rounding_policies`' argument, on the identical document shape.
#[tokio::test]
async fn a_value_listed_twice_in_one_body_is_refused() {
    let harness = Harness::new().await;
    put_one(&harness, "4000-REV").await;
    let (_, tag, before) = read_vocabulary(&harness).await;

    let response = write_vocabulary(
        &harness,
        serde_json::json!([
            { "value": "4010-TAX", "display_name": "T", "state": "active" },
            { "value": "4010-TAX", "display_name": "T", "state": "retired" }
        ]),
        &tag.expect("a tag"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await.to_string();
    assert!(
        problem.contains("appears twice in this body"),
        "the refusal names the repetition: {problem}"
    );
    let (_, _, after) = read_vocabulary(&harness).await;
    assert_eq!(after["values"], before["values"]);
}

/// A code a **published** revision's descriptor set names cannot be retired
/// through the surface, and nothing is written.
///
/// The guard's whole point: retiring under a frozen reference would leave a
/// `CatalogVersion` naming a code no vocabulary declares. The revision is seeded
/// straight at the tables — the publish path is `rest_publish`'s subject — with
/// the descriptor row inserted while the revision is a draft, which is what its
/// append-only trigger admits, and the revision then flipped as `publish_revision`
/// flips it.
#[tokio::test]
async fn a_code_a_published_descriptor_set_names_cannot_be_retired() {
    let harness = Harness::new().await;
    put_one(&harness, "4000-REV").await;
    seed_published_revision_naming(&harness, Uuid::now_v7(), "4000-REV").await;

    let (_, tag, before) = read_vocabulary(&harness).await;
    let refused = write_vocabulary(&harness, serde_json::json!([]), &tag.expect("a tag")).await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(refused).await, "TAXONOMY_VALUE_IN_USE");
    let (_, _, after) = read_vocabulary(&harness).await;
    assert_eq!(after["values"], before["values"]);
    assert_eq!(after["values"][0]["state"], serde_json::json!("active"));
}

/// Another tenant's vocabulary is invisible, and reads as the empty — hence
/// unconstrained — set.
///
/// A GL code is what an ERP posts a tenant's revenue against; one tenant's
/// catalog must neither see nor be judged by another's chart of accounts.
#[tokio::test]
async fn another_tenants_vocabulary_is_invisible() {
    let harness = Harness::new().await;
    put_one(&harness, "4000-REV").await;

    let response = harness
        .other_tenant()
        .send(with_headers("GET", GL_CODES, None, &[]))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(etag_of(&response).is_some());
    assert_eq!(body_json(response).await["values"], serde_json::json!([]));
}

async fn seed_published_revision_naming(harness: &Harness, plan_id: Uuid, gl_code: &str) {
    let conn = harness.db.conn().expect("conn");
    let plan_row = plan::ActiveModel {
        plan_id: Set(plan_id),
        revision: Set(1),
        tenant_id: Set(harness.tenant),
        lifecycle_state: Set(LifecycleState::Draft.as_str().to_owned()),
        created_by: Set(rest_support::SEED_ACTOR),
        created_at_utc: Set(rest_support::at(9)),
        ..Default::default()
    };
    plan::Entity::insert(plan_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &plan_row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the plan revision");

    let descriptors = plan_descriptor_set::ActiveModel {
        plan_id: Set(plan_id),
        plan_revision: Set(1),
        tenant_id: Set(harness.tenant),
        invoice_line_template: Set(Some("{plan}".to_owned())),
        gl_code: Set(Some(gl_code.to_owned())),
        itemization_rule: Set(Some("per_charge".to_owned())),
        additional_fields: Set(serde_json::json!({})),
    };
    plan_descriptor_set::Entity::insert(descriptors.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &descriptors)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the descriptor set");

    let moved = plan::Entity::update_many()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Published.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id))
                .add(plan::Column::Revision.eq(1_i64)),
        )
        .exec(&conn)
        .await
        .expect("publish the revision");
    assert_eq!(moved.rows_affected, 1, "the seed must have moved one row");
}
