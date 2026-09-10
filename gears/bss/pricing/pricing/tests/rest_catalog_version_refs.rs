//! `GET /bss-pricing/v1/catalog-version/refs/{pendingRef}` — one publish
//! handle's subject rows, through the real router.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::frontier::CATALOG_VERSION_REF;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::read_model::SubjectRef;
use bss_pricing::infra::storage::repo::catalog_version_ref_repo::RefIdentity;
use bss_pricing::infra::storage::repo::{PendingVersionRow, catalog_version_ref_repo};
use bss_pricing_sdk::CatalogVersion;
use rest_support::{Harness, body_json, request};
use uuid::Uuid;

const HANDLE: &str = "dev-local-v9";

fn ref_path(pending_ref: &str) -> String {
    CATALOG_VERSION_REF.replace("{pendingRef}", pending_ref)
}

async fn seed_handle(harness: &Harness, handle: &str, plan_id: Uuid) {
    let conn = harness.db.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &harness.scope(),
        PendingVersionRow::for_subject(
            harness.tenant,
            handle.to_owned(),
            &SubjectRef::Plan(plan_id),
            Some(0),
            Some(LifecycleState::Published),
            rest_support::at(10),
        ),
    )
    .await
    .expect("record the handle");
}

#[tokio::test]
async fn get_returns_the_pending_subjects_of_the_handle() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_handle(&harness, HANDLE, plan_id).await;

    let got = harness
        .allowed()
        .send(request("GET", &ref_path(HANDLE), None))
        .await;
    assert_eq!(got.status(), StatusCode::OK);
    let body = body_json(got).await;
    assert_eq!(body["pending_version_ref"], HANDLE);
    assert_eq!(body["subjects"].as_array().expect("subjects").len(), 1);
    assert_eq!(body["subjects"][0]["subject_kind"], "plan");
    assert_eq!(body["subjects"][0]["subject_ref"], plan_id.to_string());
    assert_eq!(body["subjects"][0]["status"], "pending");
    assert!(body["subjects"][0]["catalog_version"].is_null());
}

#[tokio::test]
async fn get_unknown_handle_is_404() {
    let harness = Harness::new().await;
    let got = harness
        .allowed()
        .send(request("GET", &ref_path("no-such-handle"), None))
        .await;
    assert_eq!(got.status(), StatusCode::NOT_FOUND);
}

/// A handle seeded under one tenant reads as absent to another.
///
/// The handler's own note is that "unknown and not-yours read the same", and
/// that indistinguishability is the only thing between this route and a
/// confirmation of another tenant's publish handle. `get_unknown_handle_is_404`
/// proves the absent case only: a random handle is absent for everybody, so it
/// says nothing about scope. Here the handle demonstrably EXISTS — the case
/// above reads it back as 200 — which is what makes the foreign 404 a scope
/// signal rather than an absence one.
#[tokio::test]
async fn get_is_tenant_scoped_and_a_foreign_handle_reads_as_absent() {
    let harness = Harness::new().await;
    seed_handle(&harness, HANDLE, Uuid::now_v7()).await;

    let got = harness
        .other_tenant()
        .send(request("GET", &ref_path(HANDLE), None))
        .await;
    assert_eq!(
        got.status(),
        StatusCode::NOT_FOUND,
        "a foreign tenant must not learn the handle exists"
    );
}

#[tokio::test]
async fn get_after_observe_is_commit_observed() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_handle(&harness, HANDLE, plan_id).await;
    let conn = harness.db.conn().expect("conn");
    catalog_version_ref_repo::observe_commit(
        &conn,
        &harness.scope(),
        harness.tenant,
        HANDLE,
        rest_support::at(11),
    )
    .await
    .expect("observe");

    let got = harness
        .allowed()
        .send(request("GET", &ref_path(HANDLE), None))
        .await;
    assert_eq!(got.status(), StatusCode::OK);
    let body = body_json(got).await;
    assert_eq!(body["subjects"][0]["status"], "commit_observed");
}

#[tokio::test]
async fn get_after_finalize_is_committed() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_handle(&harness, HANDLE, plan_id).await;
    let conn = harness.db.conn().expect("conn");
    let row = catalog_version_ref_repo::list_for_pending_ref(
        &conn,
        &harness.scope(),
        harness.tenant,
        HANDLE,
    )
    .await
    .expect("read")
    .into_iter()
    .next()
    .expect("one subject");
    catalog_version_ref_repo::finalize(
        &conn,
        &harness.scope(),
        RefIdentity::of(&row),
        CatalogVersion::new(4),
        rest_support::at(12),
    )
    .await
    .expect("finalize");

    let got = harness
        .allowed()
        .send(request("GET", &ref_path(HANDLE), None))
        .await;
    assert_eq!(got.status(), StatusCode::OK);
    let body = body_json(got).await;
    assert_eq!(body["subjects"][0]["status"], "committed");
    assert_eq!(body["subjects"][0]["catalog_version"], 4);
}
