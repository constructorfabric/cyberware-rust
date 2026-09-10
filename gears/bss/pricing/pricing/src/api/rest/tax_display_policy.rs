//! `GET/PUT /bss-pricing/v1/config/tax-display-policy` — C4's enforcement mode
//! (`design/04-currency-tax.md` §5, §6, `inst-td-policy`).
//!
//! # What this policy governs, and the half it deliberately does not
//!
//! C4 reads as one switch over two incomplete-basis cases, and **D-154 splits
//! it**. This surface therefore configures exactly one of them:
//!
//! * the **rate** arm — a `taxInclusive` row in a region declaring no tax rate —
//!   is what `warn` relaxes. The missing fact is a rate nobody in this gear owns
//!   and Tax Engine is pre-GA, so a tenant may legitimately accept it;
//! * the **category** arm blocks whatever this says. `taxCategory` is a pinned
//!   D-48 v1 descriptor element, and *"a per-tenant display policy may not
//!   publish past a pinned contract element"*.
//!
//! The `PUT`'s own description says so, because an operator selecting `warn` and
//! expecting both arms to relax would read a publish refusal as a bug.
//!
//! # No approval unit, and that is the contrast worth stating
//!
//! `api::rest::threshold_policy`'s `PUT` opens an always-material D-10 approval
//! unit; this one does not. The difference is what each policy governs: the
//! threshold policy decides *whether a change needs a second principal*, so
//! letting one person edit it single-handedly would be the two-person rule
//! disabling itself. This one decides how strictly one publish rule reports one
//! missing external fact, it can only ever make publishing **harder or softer on
//! a single arm**, and §10 assigns it to `CatalogAdmin` config. Gating it behind an
//! approval would be inventing governance the design set does not ask for.
//!
//! It is audited for the same reason every config mutation is.
//!
//! # `If-Match`, for the taxonomy pair's reason
//!
//! §5's Idempotency cell is `ETag`. A tenant with no policy row is answered
//! **200** with the fail-closed default — C4 governs them identically — so the
//! resource always has a representation and the bootstrap asserts a tag like any
//! other caller.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::HeaderMap;
use axum::http::header::ETAG;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::operation_builder::{ParamLocation, ParamSpec};
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;

use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::preconditions;
use crate::api::rest::state::AuthoringState;
use crate::domain::concurrency::PolicyTag;
use crate::domain::error::DomainError;
use crate::domain::tax_display::TaxDisplayPolicy;
use crate::infra::storage::repo::policy_repo;
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// `OpenAPI` tag (DE0205).
const TAG: &str = "BSS Pricing Configuration";

/// The policy resource.
///
/// The literal is repeated in both `OperationBuilder` calls because DE0801
/// validates a **literal** argument and silently passes a `const` one.
pub const TAX_DISPLAY_POLICY: &str = "/bss-pricing/v1/config/tax-display-policy";

/// The tenant's tax-display policy.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct TaxDisplayPolicyView {
    /// `fail_closed` (the ratified default) or `warn`.
    pub mode: String,
}

fn if_match_param() -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(
            "Mandatory precondition (RFC 9110), and section 5's `ETag` cell for this row. Send \
             the opaque tag the `GET` returned, verbatim. A tenant that has never configured \
             anything is answered `200` with the fail-closed default and carries a tag, so a \
             first `PUT` asserts it like any other. A tag that no longer describes the policy is \
             `409` `STALE_VERSION`; an absent or malformed one is `400`."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// Build the Axum router for the two policy operations and register them.
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/config/tax-display-policy")
        .operation_id("bss_pricing.get_tax_display_policy")
        .summary("Read the tenant's tax-display policy")
        .description(
            "The tenant's enforcement mode over the tax display basis check. `fail_closed` is \
             the ratified default and applies to every tenant that has never configured \
             anything - a tenant with no policy row is answered `200` with it rather than `404`, \
             because unset and fail-closed are one state. Gates on `config` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::if_none_match_param())
        .handler(get_policy)
        .json_response_with_schema::<TaxDisplayPolicyView>(
            openapi,
            StatusCode::OK,
            "The tenant's tax-display policy.",
        )
        // The conditional read's answer (RFC 9110 section 15.4.5). Declared
        // because it is reachable: this route emits an `ETag` and honours the
        // `If-None-Match` a caller sends it back in. A read that emits a validator
        // and ignores the conditional is the half-implementation to avoid.
        .no_content_response(
            StatusCode::NOT_MODIFIED,
            "The caller's `If-None-Match` matches the current representation, so the body is \
             not re-sent.",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::put("/bss-pricing/v1/config/tax-display-policy")
        .operation_id("bss_pricing.put_tax_display_policy")
        .summary("Set the tenant's tax-display policy")
        .description(
            "Sets the enforcement mode. **`warn` relaxes exactly one of the two \
             incomplete-basis cases**, and an operator selecting it should know which: a \
             tax-inclusive row in a region that declares no tax rate becomes an advisory instead \
             of a refusal, because the missing fact is a rate nobody in this catalog owns and \
             the Tax Engine is pre-GA. It does **not** relax the other case - a row whose \
             effective tax category resolves to nothing still blocks publish under any policy, \
             because `taxCategory` is a pinned billing-descriptor element and a per-tenant \
             display setting may not publish past a pinned contract element. Selecting `warn` \
             and expecting both to relax is the one way to misread this switch. \
             Unlike the approval-threshold policy this opens **no approval unit**: that one \
             decides whether changes need a second principal and so must not be \
             single-person-editable, while this one only softens how a single publish rule \
             reports a single missing external fact. It is audited. **`If-Match` is required.** \
             Gates on `config` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(if_match_param())
        .json_request::<TaxDisplayPolicyView>(openapi, "The mode to set.")
        .handler(put_policy)
        .json_response_with_schema::<TaxDisplayPolicyView>(
            openapi,
            StatusCode::OK,
            "The policy as it now stands.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

async fn get_policy(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx).await?;
    let mode = held_mode(&state, &scope, ctx.subject_tenant_id()).await?;
    Ok(render(mode, Some(&headers)))
}

async fn put_policy(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = write_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();

    let asserted = preconditions::if_match_policy(&headers).map_err(CanonicalError::from)?;
    let request: TaxDisplayPolicyView = preconditions::parse_body(&body)?;
    let mode = TaxDisplayPolicy::parse(&request.mode).ok_or_else(|| {
        CanonicalError::from(DomainError::InvalidRequest(format!(
            "tax-display policy mode `{}` is not one of `fail_closed` or `warn`; the default is \
             fail_closed and it is the ratified rule for every tenant (C4)",
            request.mode
        )))
    })?;

    // **The tag is resolved to a mode here and enforced in the `WHERE` there.**
    // This module refuses a request it cannot *understand* (`if_match_policy`
    // above); the store refuses one whose premise has *moved*. Comparing here and
    // updating unconditionally is the T-7 defect, and it was rebuilt on this
    // surface one commit after being removed from the taxonomy one — found by
    // review, twice, independently.
    //
    // The tag over a two-valued scalar resolves to exactly one mode, so the
    // asserted premise **is** `expected`, and the write is a compare-and-swap on
    // it.
    let expected = TaxDisplayPolicy::ALL
        .iter()
        .copied()
        .find(|candidate| tag_of(*candidate) == asserted)
        .ok_or_else(|| {
            CanonicalError::from(DomainError::StaleVersion(
                "the If-Match tag does not describe any tax-display policy this tenant could \
                 hold. Re-read the GET and author against the tag it hands back"
                    .to_owned(),
            ))
        })?;

    let conn = state
        .db
        .conn()
        .map_err(|e| CanonicalError::from(DomainError::Internal(format!("policy conn: {e}"))))?;
    let applied = policy_repo::set_tax_display_policy(
        &conn,
        &scope,
        tenant,
        mode,
        expected,
        &audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    if !applied {
        return Err(CanonicalError::from(DomainError::StaleVersion(
            "the If-Match tag no longer describes this tenant's tax-display policy: it changed \
             after you read it, and nothing was written. Re-read the GET and author against the \
             tag it hands back"
                .to_owned(),
        )));
    }

    Ok(render(mode, None))
}

/// The tenant's mode as it stands, defaulting to C4's fail-closed rule.
async fn held_mode(
    state: &AuthoringState,
    scope: &AccessScope,
    tenant: uuid::Uuid,
) -> Result<TaxDisplayPolicy, CanonicalError> {
    let conn = state
        .db
        .conn()
        .map_err(|e| CanonicalError::from(DomainError::Internal(format!("policy conn: {e}"))))?;
    // `LimitsConfig::default()` for `rounding_policy::held_ref`'s reason, and here the
    // argument is even shorter: C4 is fail-closed "for **all** tenants", so
    // `AuthoringPolicy::from_deployment_defaults` hard-codes the mode and offers no
    // deployment knob to get wrong. What a `LimitsConfig` reaches is the four caps,
    // none of which this path reads — the resolved policy is dropped on the next line.
    let policy = policy_repo::PolicyObjectRepo::new(
        state.db.clone(),
        &crate::config::LimitsConfig::default(),
    )
    .authoring_policy_on(&conn, scope, tenant)
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    policy
        .tax_display_policy()
        .map_err(|e| CanonicalError::from(repo_failure(&e)))
}

/// One rendering for both verbs, so the `GET`'s tag and the `PUT`'s response tag
/// cannot come from two readings of one policy.
/// `conditional` is `Some` on the `GET` and `None` on the `PUT`: the tag has one
/// producer and a conditional read must compare against *that* value, so the
/// comparison lives beside the rendering rather than in a second reading of the
/// same resource. See [`preconditions::if_none_match`].
fn render(mode: TaxDisplayPolicy, conditional: Option<&HeaderMap>) -> Response {
    let tag = preconditions::policy_etag(&tag_of(mode));
    if conditional.is_some_and(|headers| preconditions::if_none_match(headers, &tag)) {
        return preconditions::not_modified(&tag);
    }
    (
        [(ETAG, tag)],
        Json(TaxDisplayPolicyView {
            mode: mode.as_str().to_owned(),
        }),
    )
        .into_response()
}

/// The tag of this resource's representation — one scalar, so one field.
fn tag_of(mode: TaxDisplayPolicy) -> PolicyTag {
    PolicyTag::of_taxonomy(
        "tax-display-policy",
        std::iter::once(crate::domain::concurrency::TaxonomyTagEntry {
            value: mode.as_str(),
            state: mode.as_str(),
            display_name: "",
            tax_category: None,
            tax_rate_present: false,
        }),
    )
}

async fn read_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::CONFIG,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

async fn write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::CONFIG,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}
