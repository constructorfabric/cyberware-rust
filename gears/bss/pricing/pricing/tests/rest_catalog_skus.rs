//! `GET /catalog/skus`, driven with a catalog that actually answers.
//!
//! # Why this binary exists rather than a case in the shared harness
//!
//! All three places that mount this router — `rest_support/mod.rs`,
//! `module_test.rs` and `rest_authz.rs` — wire
//! `UnconfiguredProductCatalogClientV1`, whose `list_skus` returns
//! `Err(unconfigured_catalog())` unconditionally. The handler has
//! three arms and only the middle one was reachable in the entire suite: 200
//! with `source: "unconfigured"` and no items. `view_of`, `state.source` as a
//! *real* source name, and the 503 arm had zero coverage, and no test asserted
//! anything at all about a response from this route — the path constant appeared
//! in the two route censuses and nowhere else.
//!
//! Proven rather than assumed at the time it was found: replacing the `Ok(skus)`
//! arm with `unreachable!()` left `module_test` at 14 passed / 0 failed and
//! `rest_authz` at 21 passed / 0 failed.
//!
//! Those three mounts are left exactly as they are. The unconfigured client is
//! the right double for a census and for the authz properties, which range over
//! every mounted route and have no business depending on a registry. This binary
//! mounts the same router with the state a configured deployment has, which is
//! the only thing the shared harness could not supply.
//!
//! # What the route's own contract says, and what is therefore asserted
//!
//! "**Read `source` before `items`**: `unconfigured` with no items means no
//! registry was asked, which is not the same fact as a tenant that sells
//! nothing." That sentence is unfalsifiable while only one arm runs, so the
//! three cases here are the three distinguishable answers — a real source with
//! items, the unconfigured non-answer, and the configured-but-failing 503 that
//! must NOT be flattened into an empty 200.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use bss_pricing_sdk::product_catalog::catalog_unreachable;
use std::sync::Arc;
use toolkit_canonical_errors::CanonicalError;

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use bss_pricing::api::rest::catalog_skus::{ApiState, CATALOG_SKUS, CATALOG_TAX_CATEGORIES};
use bss_pricing::domain::ports::{
    CatalogSku, CatalogTaxCategory, ProductCatalogClientV1, UnconfiguredProductCatalogClientV1,
};
use bss_pricing::infra::local_dev_catalog::{
    DEV_LOCAL_CODE_PREFIX, DEV_LOCAL_SKU_PREFIX, LocalDevStaticProductCatalog,
};
use rest_support::{FlatInResolver, body_json, request, security_context};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use authz_resolver_sdk::PolicyEnforcer;

/// A configured registry that cannot be reached — the 503 arm's operand.
///
/// `Unreachable` rather than `Internal` because it is the one an operator will
/// actually meet, and the two take the same arm; a double that could only
/// produce the arm's *other* input would prove less.
struct UnreachableCatalog;

/// A reachable real provider with no definitions, distinct from unconfigured.
struct EmptyCatalog;

#[async_trait]
impl ProductCatalogClientV1 for EmptyCatalog {
    async fn list_skus(&self, _ctx: &SecurityContext) -> Result<Vec<CatalogSku>, CanonicalError> {
        Ok(Vec::new())
    }

    async fn list_tax_categories(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<Vec<CatalogTaxCategory>, CanonicalError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn tax_categories_are_named_marked_stable_and_read_only() {
    let client = client(Arc::new(LocalDevStaticProductCatalog), "local_dev_static");
    let response = client
        .send(request("GET", CATALOG_TAX_CATEGORIES, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body,
        serde_json::json!({
            "source": "local_dev_static",
            "items": [
                {"code": "DEV-TAX-SUBSCRIPTION", "display_name": "Subscription (demo)"},
                {"code": "DEV-TAX-USAGE", "display_name": "Usage (demo)"},
                {"code": "DEV-TAX-SUPPORT", "display_name": "Support (demo)"}
            ]
        })
    );
    assert_eq!(
        body_json(
            client
                .send(request("GET", CATALOG_TAX_CATEGORIES, None))
                .await
        )
        .await,
        body
    );
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        assert_eq!(
            client
                .send(request(method, CATALOG_TAX_CATEGORIES, None))
                .await
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}

#[tokio::test]
async fn tax_dictionary_distinguishes_empty_unconfigured_and_unreachable() {
    let empty = client(Arc::new(EmptyCatalog), "registry")
        .send(request("GET", CATALOG_TAX_CATEGORIES, None))
        .await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(
        body_json(empty).await,
        serde_json::json!({"source": "registry", "items": []})
    );
    let unconfigured = client(Arc::new(UnconfiguredProductCatalogClientV1), "unconfigured")
        .send(request("GET", CATALOG_TAX_CATEGORIES, None))
        .await;
    assert_eq!(unconfigured.status(), StatusCode::NOT_IMPLEMENTED);
    let unreachable = client(Arc::new(UnreachableCatalog), "registry")
        .send(request("GET", CATALOG_TAX_CATEGORIES, None))
        .await;
    assert_eq!(unreachable.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[async_trait]
impl ProductCatalogClientV1 for UnreachableCatalog {
    async fn list_tax_categories(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<Vec<CatalogTaxCategory>, CanonicalError> {
        Err(catalog_unreachable("connection refused"))
    }

    async fn list_skus(&self, _ctx: &SecurityContext) -> Result<Vec<CatalogSku>, CanonicalError> {
        Err(catalog_unreachable("connection refused".to_owned()))
    }
}

/// The real router over the given state, behind an allowing PDP and a context.
///
/// The gate is real, not stubbed out: `require_config_read` runs on every one of
/// these calls, so a change that dropped it would redden `rest_authz` and a
/// change that made it refuse would redden every case here.
fn client(catalog: Arc<dyn ProductCatalogClientV1>, source: &'static str) -> rest_support::Client {
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let openapi = OpenApiRegistryImpl::new();
    let router: Router = bss_pricing::api::rest::catalog_skus::router(
        Arc::new(ApiState { catalog, source }),
        &openapi,
    )
    .layer(axum::Extension(PolicyEnforcer::new(Arc::new(
        FlatInResolver {
            allowed: vec![tenant],
        },
    ))))
    .layer(axum::middleware::from_fn(
        toolkit::api::canonical_error_middleware,
    ))
    .layer(axum::Extension(security_context(principal, tenant)));

    rest_support::Client::over(router)
}

#[tokio::test]
async fn a_configured_catalog_returns_its_skus_under_its_own_source_name() {
    // The arm no test in the crate could reach. It exercises `view_of` — the
    // whole snapshot-to-wire mapping — and `state.source` carrying a real name
    // rather than the string the error arm hardcodes.
    let response = client(Arc::new(LocalDevStaticProductCatalog), "local_dev_static")
        .send(request("GET", CATALOG_SKUS, None))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    assert_eq!(
        body["source"], "local_dev_static",
        "the answer must name the source it actually came from; `unconfigured` is the \
         error arm's literal and must not appear on a configured deployment"
    );

    let items = body["items"].as_array().expect("items is a list");
    assert_eq!(
        items.len(),
        LocalDevStaticProductCatalog::skus().len(),
        "every SKU the port returned must reach the wire; a filter here would imply a \
         constraint publish does not enforce"
    );

    // Field-by-field on one entry rather than a length check: `view_of` maps six
    // members and a transposition between two `Option<String>` neighbours
    // (`metering_unit` and `plan_tier`) would survive any count.
    //
    // Snake case on the wire because `toolkit_macros::api_dto` emits
    // `#[serde(rename_all = "snake_case")]` — the platform's DTO convention,
    // already recorded at `approvals.rs:673` and `threshold_policy_tests.rs:91`.
    // Written camelCase first and every field read `null`, which is the reason
    // to compare members rather than to count them.
    let first = &items[0];
    let source = &LocalDevStaticProductCatalog::skus()[0];
    assert_eq!(first["sku_id"], source.sku_id.to_string());
    assert_eq!(first["sku_code"], source.sku_code);
    assert_eq!(first["name"], source.name);
    assert_eq!(first["status"], source.status);
    assert_eq!(
        first["metering_unit"],
        source
            .metering_unit
            .clone()
            .map_or(serde_json::Value::Null, serde_json::Value::String)
    );
    assert_eq!(
        first["plan_tier"],
        source
            .plan_tier
            .clone()
            .map_or(serde_json::Value::Null, serde_json::Value::String)
    );

    // The mode's own visibility promises, asserted where an operator meets them
    // rather than only in the unit tests of the double: every fabricated id sits
    // in the reserved namespace and every code says so in the pick-list itself.
    for item in items {
        assert!(
            item["sku_id"]
                .as_str()
                .expect("sku_id is a string")
                .starts_with(DEV_LOCAL_SKU_PREFIX),
            "a fabricated SKU must stay sweepable by id prefix: {item}"
        );
        assert!(
            item["sku_code"]
                .as_str()
                .expect("sku_code is a string")
                .starts_with(DEV_LOCAL_CODE_PREFIX),
            "and must be visible as fabricated in the list itself: {item}"
        );
    }

    // A status the catalog declares non-publishable is passed through unparsed —
    // the registry's own word. A view that dropped or normalised it would teach
    // an operator that status does not matter.
    let statuses: Vec<&str> = items
        .iter()
        .map(|i| i["status"].as_str().expect("status is a string"))
        .collect();
    assert!(
        statuses.iter().any(|s| *s != "active"),
        "the static set deliberately carries non-active entries, or this assertion is \
         checking nothing: {statuses:?}"
    );
}

#[tokio::test]
async fn an_unconfigured_catalog_answers_200_naming_itself_rather_than_failing() {
    // The one arm the shared harness could reach, asserted here for the first
    // time: it appeared in the two route censuses and nothing ever read its body.
    let response = client(Arc::new(UnconfiguredProductCatalogClientV1), "unconfigured")
        .send(request("GET", CATALOG_SKUS, None))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an unwired registry is this deployment's ordinary state, not a fault the caller \
         should retry forever"
    );
    let body = body_json(response).await;
    assert_eq!(body["source"], "unconfigured");
    assert_eq!(body["items"].as_array().expect("items is a list").len(), 0);
}

#[tokio::test]
async fn a_configured_catalog_that_fails_is_a_503_and_never_an_empty_two_hundred() {
    // The distinction the route's own description tells consumers to key on, and
    // the only one that can silently mislead: an empty 200 here would tell an
    // operator the tenant sells nothing when in fact nobody could be asked.
    //
    // Note the state: `source` is a real name, so this case cannot pass by
    // falling into the unconfigured arm.
    let response = client(Arc::new(UnreachableCatalog), "registry")
        .send(request("GET", CATALOG_SKUS, None))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a registry that was expected to answer and did not is a retry, not an all-clear"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["status"], 503,
        "the envelope's own status must agree with the header: {body}"
    );
    assert!(
        body["type"]
            .as_str()
            .unwrap_or_default()
            .contains("service_unavailable"),
        "and it must classify as unavailable rather than as a client fault: {body}"
    );

    // Asserted as it actually is, not as it reads at the raise site. The handler
    // builds `CatalogVersionUnavailable("product catalog: ...")`, and the
    // canonical ladder replaces the detail with "Service temporarily
    // unavailable" on the way out — deliberately, since a 5xx detail is an
    // internal string. So the operator gets the class and the log gets the
    // cause; pinning that here stops a future reader assuming the wire carries
    // the reason.
    assert_eq!(body["detail"], "Service temporarily unavailable");
}
