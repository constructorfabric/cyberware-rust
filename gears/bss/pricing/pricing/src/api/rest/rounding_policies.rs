//! `GET/PUT /bss-pricing/v1/config/rounding-policies` — the rounding references
//! a tenant declares (D-334).
//!
//! # Why this is not `/config/taxonomies/{class}`
//!
//! It is the same document shape, and it is deliberately not addressed through
//! the same route. [`TaxonomyClass::scope_class`] is a **total** function into
//! [`ScopeClass`], so a fifth class would declare that an overlay may be scoped
//! by rounding policy — a claim that is false and that would reach the
//! `pricing_price_overlay.scope_class` `CHECK`. `customer_group` is excluded
//! from that route for the same family of reason and has its own parallel
//! methods; this follows it.
//!
//! # What declaring a vocabulary does
//!
//! With **no** values declared, nothing changes: a price row may name any
//! reference, which is where every tenant is today (D-334 clause 3). Declare the
//! first value and `inst-tx-rounding` starts refusing references outside the
//! set at publish, under `ROUNDING_POLICY_UNKNOWN`.
//!
//! That opt-in is the whole reason the taxonomy could land at all. The
//! alternative — an empty set refusing every row — would have broken every
//! catalog on the day of the migration, including the seven rows on the stand
//! that carry `half_up` while the tenant default carries `half_even`. Those
//! two spellings of one intention are the hazard this surface exists to make
//! visible.
//!
//! # A `PUT` replaces the whole set, and an omitted value is retired
//!
//! The taxonomies' rule, and the reason is theirs: something published may still
//! resolve against a value, so it is retired rather than deleted — a retirement
//! stops it being authorable and leaves what already resolves alone. A retirement
//! is **refused** while a published row or the tenant default still names it
//! (`TAXONOMY_VALUE_IN_USE`, 409), and nothing is written.
//!
//! # No approval unit
//!
//! `api::rest::rounding_policy`'s reasoning, one document over: this narrows what
//! may be authored and can only make publishing harder, so it is `config`
//! configuration under `CatalogAdmin` rather than a D-10 unit. It is audited.

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
use crate::domain::error::DomainError;
use crate::domain::overlay::ScopeValue;
use crate::domain::taxonomy::{TAXONOMY_VALUE_IN_USE, TaxonomyEntry, TaxonomyState};
use crate::infra::storage::repo::taxonomy_repo;
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// `OpenAPI` tag (DE0205).
const TAG: &str = "BSS Pricing Configuration";

/// The vocabulary resource.
///
/// The literal is repeated in both `OperationBuilder` calls because DE0801
/// validates a **literal** argument and silently passes a `const` one.
pub const ROUNDING_POLICIES: &str = "/bss-pricing/v1/config/rounding-policies";

/// One declared rounding reference.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RoundingPolicyValueView {
    /// The reference a price row's `roundingPolicyRef` must match. Never blank.
    pub value: String,
    /// The operator's label for it.
    pub display_name: String,
    /// `active` or `retired`. Absent reads as `active`, because a body listing a
    /// value is a body declaring it.
    pub state: Option<String>,
}

/// The vocabulary, whole.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RoundingPoliciesView {
    /// What this document is a representation **of**.
    ///
    /// A constant, and it exists for a client rather than for a reader: a
    /// response carries no URL, so a client that must pair this body with the
    /// `ETag` header — the only source of the `PUT`'s precondition — has nothing
    /// else to attribute it by. `taxonomies::TaxonomyView` already discriminates
    /// on `class`; these two documents were structurally identical (`values`
    /// alone) and a client reading several config documents concurrently could
    /// pair either one's tag with the other's body, which is a wrong
    /// precondition rather than a failed one.
    pub resource: String,
    /// Every declared value, `active` and `retired` alike, ordered by value.
    ///
    /// Retirements are **included** so the round trip is honest: an operator who
    /// reads, edits and writes back can see the value they are about to
    /// re-activate.
    pub values: Vec<RoundingPolicyValueView>,
}

/// The body of a `PUT`: the complete value set, not a patch.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PutRoundingPoliciesRequest {
    /// The whole vocabulary. A value declared today and absent here is
    /// **retired**, and the retirement is refused while anything names it.
    pub values: Vec<RoundingPolicyValueView>,
}

fn if_match_param() -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(
            "Mandatory precondition (RFC 9110). Send the opaque tag the `GET` returned, \
             verbatim. A tenant that has declared nothing is answered `200` with an empty set \
             and carries a tag, so a first `PUT` asserts it like any other. A tag that no longer \
             describes the stored set is `409` `STALE_VERSION` and nothing is written - a `PUT` \
             replaces the whole set, so applying yours over a moved one would retire whatever \
             the other author added."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// Build the Axum router for the two operations and register them.
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/config/rounding-policies")
        .operation_id("bss_pricing.get_rounding_policies")
        .summary("Read the tenant's declared rounding references")
        .description(
            "The vocabulary a price row's `roundingPolicyRef` and the tenant default are \
             validated against. **An empty set constrains nothing** - that is where every tenant \
             starts, and declaring the first value is how a tenant opts in. Retired values are \
             included, because an operator editing the set has to see what they may re-activate. \
             Gates on `config` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::if_none_match_param())
        .handler(get_values)
        .json_response_with_schema::<RoundingPoliciesView>(
            openapi,
            StatusCode::OK,
            "The declared vocabulary.",
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

    let router = OperationBuilder::put("/bss-pricing/v1/config/rounding-policies")
        .operation_id("bss_pricing.put_rounding_policies")
        .summary("Replace the tenant's declared rounding references")
        .description(
            "Replaces the **whole** set. A value declared today and absent from the body is \
             retired, not deleted: something published may still resolve against it, and a \
             deletion would make that reference dangle rather than merely stop being \
             authorable. A retirement is refused with `TAXONOMY_VALUE_IN_USE` (409) while a \
             published price row or the tenant default still names the value, and **nothing at \
             all is written** - re-point them first. \
             Declaring the first value turns the publish check on: from then on a row naming a \
             reference outside the active set fails publish with `ROUNDING_POLICY_UNKNOWN`. \
             Clearing the set turns it off again. It is audited, and opens no approval unit. \
             **`If-Match` is required.** Gates on `config` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(if_match_param())
        .json_request::<PutRoundingPoliciesRequest>(openapi, "The complete value set.")
        .handler(put_values)
        .json_response_with_schema::<RoundingPoliciesView>(
            openapi,
            StatusCode::OK,
            "The vocabulary as it now stands.",
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

async fn get_values(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx).await?;
    let held = state
        .taxonomies
        .list_rounding_policies(&scope, ctx.subject_tenant_id())
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    Ok(render(&held, Some(&headers)))
}

async fn put_values(
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
    let request: PutRoundingPoliciesRequest = preconditions::parse_body(&body)?;
    let entries = authored_entries(request.values)?;

    // **The premise goes to the store, not tested here** — `taxonomies`' own
    // reasoning: a whole-set `PUT` compared against a read on a plain connection
    // lets two callers whose reads both precede either commit each pass, and the
    // second one's write retires whatever the first added.
    let result = state
        .taxonomies
        .replace_rounding_policies(
            &scope,
            tenant,
            entries,
            &asserted,
            audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    if result.stale {
        return Err(CanonicalError::from(DomainError::StaleVersion(
            "the If-Match tag no longer describes this tenant's rounding vocabulary: it changed \
             after you read it. Re-read the GET and author against the tag it hands back - a PUT \
             replaces the whole set, so applying yours over a moved one would retire whatever \
             the other author added"
                .to_owned(),
        )));
    }

    if let Some(violation) = result.report.violations.first() {
        // The first only, for `taxonomies`' reason: a pinned value is one fact
        // about the world and an operator acts on it and re-reads.
        debug_assert_eq!(violation.code, TAXONOMY_VALUE_IN_USE);
        return Err(CanonicalError::from(DomainError::TaxonomyValueInUse(
            violation.detail.clone(),
        )));
    }

    Ok(render(&result.entries, None))
}

/// The representation with the tag that covers it — one function for both verbs,
/// so the `GET`'s tag and the `PUT`'s cannot come from two readings.
/// `conditional` is `Some` on the `GET` and `None` on the `PUT`: the tag has one
/// producer and a conditional read must compare against *that* value, so the
/// comparison lives beside the rendering rather than in a second reading of the
/// same resource. See [`preconditions::if_none_match`].
fn render(entries: &[TaxonomyEntry], conditional: Option<&HeaderMap>) -> Response {
    let tag = preconditions::policy_etag(&taxonomy_repo::rounding_policy_tag_of(entries));
    if conditional.is_some_and(|headers| preconditions::if_none_match(headers, &tag)) {
        return preconditions::not_modified(&tag);
    }
    (
        [(ETAG, tag)],
        Json(RoundingPoliciesView {
            resource: "rounding-policies".to_owned(),
            values: entries
                .iter()
                .map(|entry| RoundingPolicyValueView {
                    value: entry.value.as_str().to_owned(),
                    display_name: entry.display_name.clone(),
                    state: Some(entry.state.as_str().to_owned()),
                })
                .collect(),
        }),
    )
        .into_response()
}

/// Parse the body's values, refusing what the store's `CHECK`s would.
///
/// Refused here rather than at the driver for `plans`' reason: a constraint
/// violation arrives as an internal fault naming a constraint, while an author
/// needs a message naming the field.
fn authored_entries(
    values: Vec<RoundingPolicyValueView>,
) -> Result<Vec<TaxonomyEntry>, CanonicalError> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut entries = Vec::with_capacity(values.len());
    for view in values {
        let value = ScopeValue::new(&view.value).ok_or_else(|| {
            CanonicalError::from(DomainError::InvalidRequest(
                "a rounding policy value is blank; a vocabulary entry names a reference and \
                 the empty string is not one"
                    .to_owned(),
            ))
        })?;
        if !seen.insert(value.as_str().to_owned()) {
            // `taxonomies::authored_entries`' refusal on the identical document
            // shape. A body naming one value twice has two states for it and no
            // rule says which wins — and the second spelling never reaches the
            // key that would refuse it, because `apply_replace_rounding_policy`
            // collects the submitted set into a `BTreeMap` keyed by value. So
            // de-duplicating here would answer 200 for a set the author did not
            // send, with whichever line came last.

            return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
                "value `{value}` appears twice in this body; a rounding policy vocabulary \
                 declares each value once, and a repeated one leaves its state undecided"
            ))));
        }
        let state = match view.state.as_deref() {
            None => TaxonomyState::Active,
            Some(token) => TaxonomyState::parse(token).ok_or_else(|| {
                CanonicalError::from(DomainError::InvalidRequest(format!(
                    "rounding policy state `{token}` is not one of `active` or `retired`"
                )))
            })?,
        };
        entries.push(TaxonomyEntry {
            value,
            display_name: view.display_name,
            state,
            tax: None,
        });
    }
    Ok(entries)
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
