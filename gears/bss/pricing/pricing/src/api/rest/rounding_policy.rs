//! `GET/PUT /bss-pricing/v1/config/rounding-policy` — the tenant's default
//! rounding policy (PRD §17.4, D-320).
//!
//! # Why this surface exists
//!
//! Rounding decides the last minor unit of every charge, and the design set has
//! always described it as a **tenant** setting with a per-row override: the
//! publish rule `foundation.rounding_policy_resolved` checks nothing at all when
//! the tenant has a default, and falls back to demanding one on every row when
//! it does not. But `default_rounding_policy_ref` had **no writer** — the only
//! writer of `pricing_policy_object` set `tax_display_policy_mode` — so the
//! default was permanently `None`, the fail-closed arm was the only reachable
//! path, and `roundingPolicyRef` on the price row looked like a mandatory
//! per-row field rather than the override it is. An operator asking "shouldn't
//! rounding be one setting?" was reading the symptom exactly right.
//!
//! # It is a `ref`, and the tenant's own vocabulary is what bounds it
//!
//! The stored value names a policy this gear does not own, so the gear cannot
//! invent a vocabulary for it — but the tenant declares one, through
//! `GET/PUT /bss-pricing/v1/config/rounding-policies`, and D-348 makes that
//! declaration binding here: `put_policy` reads the active set and refuses a
//! reference outside it with `DomainError::RoundingPolicyUnknown`. An empty set
//! constrains nothing, which is `prices::require_declared_region`'s reading and
//! is what keeps this from being a migration every existing tenant has to run
//! before their next config write.
//!
//! The empty string is refused separately, for the reason `plan_name` does
//! (D-318) — a cleared default is spelled `null`, and two spellings of one state
//! is a defect this gear has paid for.
//!
//! # No approval unit, for `tax_display_policy`'s reason
//!
//! `api::rest::threshold_policy`'s `PUT` opens an always-material D-10 unit
//! because that policy decides *whether a change needs a second principal*, so a
//! single person editing it would be the two-person rule disabling itself. This
//! one supplies a default that publish would otherwise demand row by row — it
//! changes no price and can only make publishing **easier**, never softer on any
//! money check — and §10 assigns config to `CatalogAdmin`. It is audited, as
//! every config mutation is.
//!
//! # `If-Match`, resolved against the held value rather than an enumeration
//!
//! The tax-display policy is two-valued, so its `PUT` resolves the asserted tag
//! by trying both modes. A rounding ref is free text and cannot be enumerated,
//! so the tag is resolved against what the tenant currently **holds**, and that
//! value is the premise the store's `WHERE` matches on. The compare-and-swap is
//! not weakened: a writer who moved the ref between the read and the write
//! affects zero rows and the caller is told, rather than silently overwriting.
//!
//! A tenant with no policy row is answered **200** with `null` — unset is a
//! state, not an absence — so the resource always has a representation and a
//! first `PUT` asserts a tag like any other caller.

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
use crate::domain::concurrency::{PolicyTag, TaxonomyTagEntry};
use crate::domain::error::DomainError;
use crate::infra::storage::repo::policy_repo;
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// `OpenAPI` tag (DE0205).
const TAG: &str = "BSS Pricing Configuration";

/// The policy resource.
///
/// The literal is repeated in both `OperationBuilder` calls because DE0801
/// validates a **literal** argument and silently passes a `const` one.
pub const ROUNDING_POLICY: &str = "/bss-pricing/v1/config/rounding-policy";

/// The tenant's default rounding policy.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RoundingPolicyView {
    /// The reference, or `null` when the tenant has set no default.
    pub default_rounding_policy_ref: Option<String>,
}

fn if_match_param() -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(
            "Mandatory precondition (RFC 9110). Send the opaque tag the `GET` returned, \
             verbatim. A tenant that has never set a default is answered `200` with `null` and \
             carries a tag, so a first `PUT` asserts it like any other. A tag that no longer \
             describes the stored default is `409` `STALE_VERSION`; an absent or malformed one \
             is `400`."
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
    let router = OperationBuilder::get("/bss-pricing/v1/config/rounding-policy")
        .operation_id("bss_pricing.get_rounding_policy")
        .summary("Read the tenant's default rounding policy")
        .description(
            "The reference every price row falls back to when it declares no `roundingPolicyRef` \
             of its own. `null` means no default is set, which is a state rather than an \
             absence - a tenant in it must give every published row its own reference or the \
             plan fails publish with `ROUNDING_POLICY_UNRESOLVED`. Gates on `config` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::if_none_match_param())
        .handler(get_policy)
        .json_response_with_schema::<RoundingPolicyView>(
            openapi,
            StatusCode::OK,
            "The tenant's default rounding policy.",
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

    let router = OperationBuilder::put("/bss-pricing/v1/config/rounding-policy")
        .operation_id("bss_pricing.put_rounding_policy")
        .summary("Set the tenant's default rounding policy")
        .description(
            "Sets the reference every price row without its own falls back to. Setting it makes \
             `ROUNDING_POLICY_UNRESOLVED` unreachable for this tenant: the publish rule checks \
             the rows only when there is no default. Send `null` to clear it, which puts the \
             tenant back to needing a reference on every published row - the empty string is \
             **refused** rather than stored, so that unset has one spelling. \
             A value outside the active set of `GET /bss-pricing/v1/config/rounding-policies` \
             is refused `ROUNDING_POLICY_UNKNOWN` (D-348); a tenant who has declared no \
             vocabulary constrains nothing, so an empty set accepts any reference. \
             Unlike the approval-threshold policy this \
             opens **no approval unit**: that one decides whether changes need a second \
             principal, while this one supplies a default publish would otherwise demand row by \
             row, changes no price, and can only make publishing easier. It is audited. \
             **`If-Match` is required.** Gates on `config` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(if_match_param())
        .json_request::<RoundingPolicyView>(openapi, "The default to set, or `null` to clear it.")
        .handler(put_policy)
        .json_response_with_schema::<RoundingPolicyView>(
            openapi,
            StatusCode::OK,
            "The default as it now stands.",
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
    let held = held_ref(&state, &scope, ctx.subject_tenant_id()).await?;
    Ok(render(held.as_deref(), Some(&headers)))
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
    let request: RoundingPolicyView = preconditions::parse_body(&body)?;

    // Blank is refused rather than normalised to `null`, for D-318's reason on
    // `planName`: a state with two spellings is one every reader has to
    // special-case, and the first that forgets shows a default that is there and
    // means nothing.
    if request
        .default_rounding_policy_ref
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(CanonicalError::from(DomainError::InvalidRequest(
            "defaultRoundingPolicyRef is blank: clear the default by sending `null`, which is \
             the one spelling of unset"
                .to_owned(),
        )));
    }
    let requested = request.default_rounding_policy_ref.as_deref();

    // **The default is a reference, so it is judged like one.**
    // `RoundingPolicyDeclared` carries `tenant_default` precisely
    // because the publish walk skips every `None` row — the set the default
    // stands in for — but that rule runs on the publish path alone, and
    // `infra::supersession`, `infra::cutover` and mass repricing freeze the
    // default onto a row with no rule over it at all. Judging it at its own write
    // door is what makes the three of them safe without a fourth hand-enumerated
    // site, and it is the door `RoundingPolicyDeclared::violation_for`'s doc says
    // is owed ("giving this rule the same door... is the fix that would make the
    // pair symmetric again... left for a decision") — D-348 takes it.
    //
    // Modelled on `prices::require_declared_region`, including its empty-set
    // reading: a tenant who has declared no vocabulary constrains nothing, which
    // is `violation_for`'s own first clause and keeps this from becoming a
    // migration every existing tenant has to run before their next config write.
    if let Some(reference) = requested {
        let conn = state.db.conn().map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!("taxonomy conn: {e}")))
        })?;
        let declared = crate::infra::storage::repo::taxonomy_repo::active_rounding_policies(
            &conn, &scope, tenant,
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
        let rule = crate::domain::taxonomy::RoundingPolicyDeclared {
            declared,
            tenant_default: None,
        };
        if let Some(violation) = rule.violation_for("defaultRoundingPolicyRef", reference) {
            return Err(CanonicalError::from(DomainError::RoundingPolicyUnknown(
                violation.detail,
            )));
        }
    }

    // **The premise is what the tenant holds, resolved here and matched in the
    // store's `WHERE`.** The tax-display surface enumerates its two modes to
    // resolve the tag; a free-text ref cannot be enumerated, so the held value is
    // read and its tag compared. If the ref moves between this read and the
    // write, the store's compare-and-swap matches nothing and the caller gets a
    // 409 rather than a lost update — comparing here and writing unconditionally
    // is the T-7 defect this table has already suffered once.
    let held = held_ref(&state, &scope, tenant).await?;
    if tag_of(held.as_deref()) != asserted {
        return Err(CanonicalError::from(DomainError::StaleVersion(
            "the If-Match tag does not describe this tenant's stored default rounding policy. \
             Re-read the GET and author against the tag it hands back"
                .to_owned(),
        )));
    }

    let conn = state
        .db
        .conn()
        .map_err(|e| CanonicalError::from(DomainError::Internal(format!("policy conn: {e}"))))?;
    let applied = policy_repo::set_default_rounding_policy(
        &conn,
        &scope,
        tenant,
        requested,
        held.as_deref(),
        &audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    if !applied {
        return Err(CanonicalError::from(DomainError::StaleVersion(
            "the If-Match tag no longer describes this tenant's default rounding policy: it \
             changed after you read it, and nothing was written. Re-read the GET and author \
             against the tag it hands back"
                .to_owned(),
        )));
    }

    Ok(render(requested, None))
}

/// The tenant's stored default, or `None` when they have set none.
async fn held_ref(
    state: &AuthoringState,
    scope: &AccessScope,
    tenant: uuid::Uuid,
) -> Result<Option<String>, CanonicalError> {
    let conn = state
        .db
        .conn()
        .map_err(|e| CanonicalError::from(DomainError::Internal(format!("policy conn: {e}"))))?;
    // **`LimitsConfig::default()` here is not a substitution for the deployment's
    // `limits` section**, and the reason is in `AuthoringPolicy::from_deployment_defaults`:
    // the only fields a `LimitsConfig` reaches are the four caps, and the rounding
    // default is explicitly not one of them — it is `None` for every deployment,
    // because a deployment-wide rounding default would decide the last minor unit of
    // every charge of every tenant that never asked for one. So this path cannot read
    // a configured value wrongly; it reads exactly one field no deployment configures,
    // and the resolved policy is dropped on the next line. A cap read added here would
    // change that, and would then need the deployment's `limits` carried on
    // `AuthoringState` — `repricing_runs::policies` is what that looks like.
    let policy = policy_repo::PolicyObjectRepo::new(
        state.db.clone(),
        &crate::config::LimitsConfig::default(),
    )
    .authoring_policy_on(&conn, scope, tenant)
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    Ok(policy.default_rounding_policy_ref().map(ToOwned::to_owned))
}

/// One rendering for both verbs, so the `GET`'s tag and the `PUT`'s response tag
/// cannot come from two readings of one policy.
/// `conditional` is `Some` on the `GET` and `None` on the `PUT`: the tag has one
/// producer and a conditional read must compare against *that* value, so the
/// comparison lives beside the rendering rather than in a second reading of the
/// same resource. See [`preconditions::if_none_match`].
fn render(value: Option<&str>, conditional: Option<&HeaderMap>) -> Response {
    let tag = preconditions::policy_etag(&tag_of(value));
    if conditional.is_some_and(|headers| preconditions::if_none_match(headers, &tag)) {
        return preconditions::not_modified(&tag);
    }
    (
        [(ETAG, tag)],
        Json(RoundingPolicyView {
            default_rounding_policy_ref: value.map(ToOwned::to_owned),
        }),
    )
        .into_response()
}

/// The tag of this resource's representation — one nullable scalar.
///
/// Unset tags as the literal `"(unset)"` rather than as an empty entry set: an
/// empty set and a set holding the empty string must not collide, and a tenant
/// who has cleared their default is asserting a real state whose tag has to be
/// distinct from every ref anyone could author.
fn tag_of(value: Option<&str>) -> PolicyTag {
    PolicyTag::of_taxonomy(
        "rounding-policy",
        std::iter::once(TaxonomyTagEntry {
            value: value.unwrap_or("(unset)"),
            state: if value.is_some() { "set" } else { "unset" },
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
