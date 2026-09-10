//! Read-only Product Catalog projections: SKUs and tax-category definitions.
//!
//! Tax categories are definitions, not rates or SKU/region defaults. Unlike the
//! optional SKU suggestions below, an unconfigured category dictionary answers
//! an explicit 501; a configured provider's outage remains a 503.
//!
//! # A pass-through, and it says whose answer it is
//!
//! This gear owns no catalog. The route exists because the surfaces that author
//! a **meter** or bind a plan's **`sku_id`** cannot otherwise offer anything but
//! the values the tenant has already used, and a first row for a new SKU is
//! then typed from memory. So it reads
//! [`ProductCatalogClientV1`](crate::domain::ports::ProductCatalogClientV1) and
//! renders the answer, validating nothing: neither the meter nor the `sku_id`
//! is checked against this list anywhere, and pretending otherwise by filtering
//! here would imply a constraint the publish path does not enforce.
//!
//! # Why it answers 200 with a `source` instead of failing
//!
//! The ordinary state of this deployment is that no registry is wired, and a
//! pick-list with no suggestions is not an error — the operator types the value,
//! exactly as before this route existed. What would be an error is **saying the
//! catalog is empty**, because "the tenant sells nothing" and "nobody could be
//! asked" are opposite facts and only one of them is true here.
//!
//! So the body always carries `source`, and an unconfigured registry is
//! `source: "unconfigured"` with no items rather than a 503. The surface reads
//! the field and says which it is. The same distinction the migration surface
//! draws with `subjectsUnresolved`, and drawn the same way for the same reason.
//!
//! A registry that *is* configured and then fails is a different matter and does
//! answer 503: something was expected to reply and did not, and a caller
//! retrying is right.

use std::sync::Arc;

use axum::extract::Extension;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::error::authz_error_to_canonical;
use crate::domain::error::DomainError;
use crate::domain::ports::{CatalogSku, ProductCatalogClientV1, ProductCatalogError};

const TAG: &str = "BSS Pricing";

/// The one path this surface serves. A constant because three separate route
/// censuses name it, and a literal repeated four times is a literal that drifts.
pub const CATALOG_SKUS: &str = "/bss-pricing/v1/catalog/skus";

/// Read-only dictionary for a price row's `tax_category_ref` picker.
pub const CATALOG_TAX_CATEGORIES: &str = "/bss-pricing/v1/catalog/tax-categories";

/// A definition from Product Catalog; no assignment or rate lives here.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CatalogTaxCategoryView {
    /// Stable provider-owned code.
    pub code: String,
    /// Human-readable label, read from the provider.
    pub display_name: String,
}

/// The dictionary and its configured source. Failure is never an empty success.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CatalogTaxCategoriesView {
    /// `local_dev_static` or `registry`. Demo codes are not production taxonomy.
    pub source: String,
    /// Definitions in provider order; a configured empty dictionary is `[]`.
    pub items: Vec<CatalogTaxCategoryView>,
}

/// One SKU as this gear passes it on.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CatalogSkuView {
    /// What a plan's `sku_id` binds to.
    pub sku_id: Uuid,
    pub sku_code: String,
    pub name: String,
    /// Present on a usage SKU and absent on one priced per period. The presence
    /// **is** the distinction; there is no separate flag.
    pub metering_unit: Option<String>,
    /// The registry's own word, passed through unparsed.
    pub status: String,
    pub plan_tier: Option<String>,
}

/// The catalog, and where it came from.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CatalogSkusView {
    /// `unconfigured` | `local_dev_static` | `registry`.
    ///
    /// **Read this before reading `items`.** An empty list under `unconfigured`
    /// means nobody was asked; an empty list under `registry` means the tenant
    /// sells nothing. A surface that renders them alike is showing an
    /// all-clear it has no basis for.
    pub source: String,
    pub items: Vec<CatalogSkuView>,
}

fn view_of(sku: CatalogSku) -> CatalogSkuView {
    CatalogSkuView {
        sku_id: sku.sku_id,
        sku_code: sku.sku_code,
        name: sku.name,
        metering_unit: sku.metering_unit,
        status: sku.status,
        plan_tier: sku.plan_tier,
    }
}

/// What this surface needs: the port and the name of the thing behind it.
pub struct ApiState {
    pub catalog: Arc<dyn ProductCatalogClientV1>,
    /// The configured source, so the answer can name itself. Held rather than
    /// derived: the port cannot say which implementation it is, and asking it
    /// to would put a deployment concern in a cross-gear contract.
    pub source: &'static str,
}

/// The gate, asked even though this handler reads no table.
///
/// The scope it returns is unused — there is nothing here to scope, the answer
/// comes from another gear — and the call is the point: without it the route is
/// merely *authenticated*, any token reads the catalog, and a PDP outage leaves
/// it answering 200 while every neighbour fails closed. Both were caught by
/// `rest_authz`, which is exactly what that suite is for.
async fn require_config_read(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<(), CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::CONFIG,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map(|_| ())
    .map_err(authz_error_to_canonical)
}

async fn list_skus(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    require_config_read(&enforcer, &ctx).await?;
    match state
        .catalog
        .list_skus(&ctx)
        .await
        .map_err(ProductCatalogError::from)
    {
        Ok(skus) => Ok((
            StatusCode::OK,
            Json(CatalogSkusView {
                source: state.source.to_owned(),
                items: skus.into_iter().map(view_of).collect(),
            }),
        )
            .into_response()),
        // Not a failure: it is this deployment's ordinary state, and the caller
        // is told so rather than handed a 503 it would retry forever.
        Err(ProductCatalogError::Unconfigured) => Ok((
            StatusCode::OK,
            Json(CatalogSkusView {
                source: "unconfigured".to_owned(),
                items: Vec::new(),
            }),
        )
            .into_response()),
        // Something was expected to answer and did not. A retry is right.
        Err(e) => {
            // The 503 this renders carries no detail (`infra::error_mapping`'s own
            // account of why), so this line is the only record of what the catalog
            // said.
            tracing::error!(
                error = %e,
                "bss-pricing: the product catalog could not be read; the pick-list answers 503"
            );
            Err(CanonicalError::from(
                DomainError::CatalogVersionUnavailable(format!("product catalog: {e}")),
            ))
        }
    }
}

/// Read the category dictionary after the same config gate as SKU browsing.
async fn list_tax_categories(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
) -> Result<Json<CatalogTaxCategoriesView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    require_config_read(&enforcer, &ctx).await?;
    let categories = state.catalog.list_tax_categories(&ctx).await?;
    Ok(Json(CatalogTaxCategoriesView {
        source: state.source.to_owned(),
        items: categories
            .into_iter()
            .map(|category| CatalogTaxCategoryView {
                code: category.code,
                display_name: category.display_name,
            })
            .collect(),
    }))
}

/// Build the two read-only projections over the same provider and gate.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get(CATALOG_SKUS)
        .operation_id("bss_pricing.list_catalog_skus")
        .summary("The SKUs this tenant may price")
        .description(
            "A pass-through of the Product & SKU registry's browse list, for the pick-lists \
             that author a price row's meter or bind a plan's `skuId`. This gear validates \
             nothing against it: a meter and a `skuId` are taken as given, and filtering here \
             would imply a constraint publish does not enforce. \
             **Read `source` before `items`**: `unconfigured` with no items means no registry \
             was asked, which is not the same fact as a tenant that sells nothing. A registry \
             that is configured and fails answers 503 instead. \
             Gates on `config` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(list_skus)
        .json_response_with_schema::<CatalogSkusView>(
            openapi,
            StatusCode::OK,
            "The catalog, and the source that answered.",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);
    OperationBuilder::get(CATALOG_TAX_CATEGORIES)
        .operation_id("bss_pricing.list_catalog_tax_categories")
        .summary("Tax-category definitions from Product Catalog")
        .description(
            "Read-only codes and display names for a price row's `tax_category_ref`. \
             Definitions belong to Product Catalog; rates and tax rules do not. \
             `source` distinguishes the explicitly enabled `local_dev_static` demo from \
             `registry`. No CRUD, SKU/region assignment or automatic dev fallback. \
             An unconfigured provider answers 501; an unavailable one answers 503. \
             Gates on `config` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(list_tax_categories)
        .json_response_with_schema::<CatalogTaxCategoriesView>(
            openapi,
            StatusCode::OK,
            "Definitions and their source.",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .problem_response(
            openapi,
            StatusCode::NOT_IMPLEMENTED,
            "No product catalog configured",
        )
        .error_503(openapi)
        .register(router, openapi)
        .layer(Extension(state))
}
