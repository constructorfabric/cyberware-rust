//! `GET/PUT /bss-pricing/v1/customer-groups/taxonomy` — the BSS customer-group
//! value universe, on its **own** route (`design/09-price-overlays.md` §3
//! `inst-cg-taxonomy`, §5; `design/05-governance.md`'s endpoint map).
//!
//! # Why this is not a fifth arm of `api::rest::taxonomies`
//!
//! `api::rest::taxonomies` mounts `GET/PUT /config/taxonomies/{class}` over
//! `TaxonomyClass`'s four members and gates on `config × read/write`. A first
//! attempt at this surface was briefed as a fifth arm of that enum on that same
//! route, and it was the wrong shape: `design/09-price-overlays.md` §5 gives the
//! customer-group taxonomy its **own** route, and `05-governance.md`'s
//! endpoint-mapping table says so explicitly — *"the customer-group taxonomy is
//! **not** here: it lives at `/bss-pricing/v1/customer-groups/taxonomy` under
//! `customer_group` (more sensitive)"*. `authz.rs`'s own module doc gives the
//! reason: *"`customer_group` is its own resource, not part of `plan` —
//! per-payer membership is payer-level commercial data, more sensitive than
//! plan authoring"* — and the taxonomy of legal values for that data is the same
//! sensitivity class as the membership records it will gate. Filing it under
//! `config × write` would have handed every `CatalogAdmin` who declares a brand
//! or a region the same authority over the customer-group vocabulary, which is
//! exactly the widening `05-governance.md` segregates against.
//!
//! `TaxonomyClass` (`crate::domain::taxonomy`) therefore gains **no** arm for
//! this class — that enum is the shared `{class}` route's own vocabulary, and an
//! arm there is precisely what would make this value set addressable through
//! the route it must not use. The two authz permissions this route consumes,
//! `customer_group × read` and `customer_group × write`, are already declared
//! in `gts/permissions.rs` and this route is their
//! **first** consumer — no new authz vocabulary is minted here.
//!
//! # The shape otherwise
//!
//! Same resource discipline as `api::rest::taxonomies`, restated because the
//! two are siblings rather than the same handler: the `PUT` carries the
//! **whole** value set (a value the body omits is retired, never deleted,
//! because a value a published overlay scope still names has to keep
//! existing), `If-Match` is required (D-171; a tenant with no values at all is
//! answered `200` with an empty list and a tag, so the bootstrap reads its
//! precondition off the `GET` like every other caller), and a retirement the
//! `inst-cg-taxonomy` guard refuses fails the whole `PUT` with
//! `TAXONOMY_VALUE_IN_USE` (409), writing nothing.
//!
//! This taxonomy carries no `tax_*` markers — those are D-01's, declared on the
//! region taxonomy alone — so its value view is narrower than
//! `TaxonomyValueView`'s.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::HeaderMap;
use axum::http::header::ETAG;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};
use bss_pricing_sdk::odata::MembershipFilterField;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::{OperationBuilderODataExt, ParamLocation, ParamSpec};
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::{ApprovalView, MaterialityView};
use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::plans::idempotency_key_param;
use crate::api::rest::preconditions;
use crate::api::rest::state::AuthoringState;
use crate::domain::concurrency::RowVersion;
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{self, ChangeSet, MaterialityVerdict};
use crate::domain::membership_change::{MembershipMoveProposal, MembershipMoveSet};
use crate::domain::overlay::ScopeValue;
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::taxonomy::{TAXONOMY_VALUE_IN_USE, TaxonomyEntry, TaxonomyState};
use crate::infra::approval::ApprovalService;
use crate::infra::idempotent::{self, Guarded, GuardedRequest};
use crate::infra::membership_publish;
use crate::infra::storage::repo::approval_repo;
use crate::infra::storage::repo::taxonomy_repo::customer_group_tag_of;
use crate::infra::storage::repo::{IdempotencyGate, NewMembership, group_membership_repo};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to both operations (DE0205).
const TAG: &str = "BSS Pricing Customer Groups";

/// The customer-group taxonomy resource.
///
/// The literal is repeated in both `OperationBuilder` calls below because
/// DE0801 validates a **literal** argument and silently passes a `const` one;
/// the two spellings are pinned together by `tests/module_test.rs`'s route
/// census, exactly as `taxonomies::TAXONOMY`'s is.
pub const CUSTOMER_GROUP_TAXONOMY: &str = "/bss-pricing/v1/customer-groups/taxonomy";

/// The membership collection of one group (`design/09-price-overlays.md` §5,
/// `POST`).
///
/// The literal is repeated in the `OperationBuilder` call below for
/// [`CUSTOMER_GROUP_TAXONOMY`]'s reason: DE0801 validates a literal argument
/// and silently passes a `const` one.
pub const CUSTOMER_GROUP_MEMBERS: &str = "/bss-pricing/v1/customer-groups/{group}/members";

/// One membership, addressed by its own id (§5, `PATCH`).
pub const CUSTOMER_GROUP_MEMBER: &str = "/bss-pricing/v1/customer-groups/{group}/members/{id}";

/// The atomic move into `{group}` (the **target** group), addressed by the
/// payer (§5, `POST.../move`, `inst-ms-move`, D-09).
pub const CUSTOMER_GROUP_MEMBER_MOVE: &str =
    "/bss-pricing/v1/customer-groups/{group}/members/{payerId}/move";

/// Many payers into `{group}` in one approval unit (`inst-mm-bulk`).
pub const CUSTOMER_GROUP_MEMBERS_MOVE: &str =
    "/bss-pricing/v1/customer-groups/{group}/members/move";

// ---------------------------------------------------------------------------
// Views.
// ---------------------------------------------------------------------------

/// One declared customer-group value, as an operator reads and writes it.
///
/// Narrower than `taxonomies::TaxonomyValueView`: this table carries no `tax_*`
/// columns (D-01's markers are the region taxonomy's alone), so there is
/// nothing here for them to occupy.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct CustomerGroupValueView {
    /// The declared code — the string a `customerGroup`-scoped overlay's
    /// `scopeValue` must match. Never blank.
    pub value: String,
    /// The operator's label for it.
    pub display_name: String,
    /// `active` or `retired`. Optional on the way in and defaulting to
    /// `active`, because a body listing a value is a body declaring it;
    /// sending `"retired"` is the explicit spelling of the same act as leaving
    /// it out, and both are guarded identically.
    pub state: Option<String>,
}

/// The customer-group taxonomy, whole.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CustomerGroupTaxonomyView {
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
    /// Retirements are **included**, for `taxonomies::TaxonomyView`'s reason: an
    /// operator who reads, edits and writes back has to be able to see the
    /// value they are about to re-activate.
    pub values: Vec<CustomerGroupValueView>,
}

/// One page of a group's memberships (D-322, paginated by D4-4).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MembershipListView {
    /// The group these belong to — echoed, so a client reading several groups
    /// can attribute a response that carries no URL.
    pub group_value: String,
    /// This page's memberships, **ended ones included**.
    ///
    /// Ordered by `(effective_from, membership_id)` — the pair the walk's cursor
    /// names, not the id alone. D-322 clause 4 reads this list in effective-date
    /// order and [`group_membership_repo::memberships_in_group`] says why the id
    /// cannot serve it: a `membership_id` is minted at the request, so ordering by
    /// it is ordering by *write* time, and an operator enrolling a payer today with
    /// last month's `effectiveFrom` would sort after one taking effect later. The
    /// id is the tie-break, which is what keeps the order total across a page
    /// boundary falling inside one instant.
    ///
    pub memberships: Vec<MembershipView>,
    /// D-125's envelope: `next_cursor`, `prev_cursor` (always `null`) and the
    /// `limit` this page was served at.
    ///
    /// Spelled `page_info` on `Page<T>`, the shape every other paginated read in
    /// this gear answers. This family keeps its own view because the response also
    /// echoes `group_value`, which `Page<T>` has nowhere to carry.
    pub page_info: toolkit_odata::PageInfo,
}

/// The body of a `PUT`: the complete value set.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PutCustomerGroupTaxonomyRequest {
    /// The whole taxonomy, not a patch. A value declared today and absent here
    /// is **retired**.
    pub values: Vec<CustomerGroupValueView>,
}

// ---------------------------------------------------------------------------
// Router.
// ---------------------------------------------------------------------------

/// The `If-Match` header this `PUT` requires (D-171) —
/// `taxonomies::if_match_param`'s wording, restated for this resource.
fn if_match_param() -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(
            "Mandatory precondition (RFC 9110). The value is the **opaque** tag the `GET` \
             returns in its `ETag` header - copy it back verbatim. It is not a row version: this \
             taxonomy is a value set with no version column, so the tag is a digest over the \
             representation the `GET` serves - every value's code, state and label. A tenant with \
             no values at all is answered `200` with an empty list and carries a tag, so a first \
             `PUT` asserts it like any other. It matters here more than the row count suggests, \
             because this `PUT` replaces the **whole** set: without it, two admins who each add \
             one value would leave a taxonomy carrying one addition and silently retiring the \
             other's. A tag that no longer describes the taxonomy is `409` `STALE_VERSION`; an \
             absent or malformed one is `400`."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// Build the Axum router for the two customer-group taxonomy operations and
/// register them.
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/customer-groups/taxonomy")
        .operation_id("bss_pricing.get_customer_group_taxonomy")
        .summary("Read the tenant's BSS customer-group taxonomy")
        .description(
            "Every customer-group value the tenant has declared, `active` and `retired` alike, \
             ordered by value. Retirements are included deliberately: retirement is guarded \
             rather than cascading and `retired -> active` is a legal audited move, so an \
             operator editing this list has to be able to see the value they are about to \
             re-activate. A tenant that has declared nothing is answered `200` with an empty list \
             - that is a state, not an absent resource. **The response carries the `ETag` the \
             `PUT` demands**, and this is the only place to obtain one. This is a **separate** \
             resource from `GET /config/taxonomies/{class}` and gates on `customer_group` x \
             `read`, never `config` x `read` - per-payer commercial data is more sensitive than \
             plan/config authoring (`05-governance.md`).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::if_none_match_param())
        .handler(get_customer_group_taxonomy)
        .json_response_with_schema::<CustomerGroupTaxonomyView>(
            openapi,
            StatusCode::OK,
            "The declared customer-group values.",
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
        // No `.error_400`, and the absence is the measurement rather than an
        // oversight: this handler binds no path parameter, no `Query` and no body,
        // so its whole error surface is `require_authenticated` (401), `read_scope`
        // (403/503) and `repo_failure` over a plain `SELECT`, whose 400-producing
        // arms are all state-machine edges a list read cannot reach. Its four config
        // `GET` peers declare none either. The one config `GET` that legitimately
        // declares a 400 is `GET /config/taxonomies/{class}`, which earns it by
        // parsing a path segment.
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::put("/bss-pricing/v1/customer-groups/taxonomy")
        .operation_id("bss_pricing.put_customer_group_taxonomy")
        .summary("Replace the tenant's BSS customer-group taxonomy")
        .description(
            "The body is the **whole** taxonomy, not a patch. A value listed here is declared \
             (and re-activated if it was retired); a value the tenant holds today and this body \
             omits is **retired** - never deleted, because a value a published `customerGroup`-\
             scoped overlay still names has to keep existing. Sending a value with `state: \
             \"retired\"` is the explicit spelling of the same act, and the two are guarded \
             identically. **Retirement is guarded** (`inst-cg-taxonomy`): a value still named by \
             a published `PriceOverlay` scoped to `customerGroup` and that value cannot retire - \
             the response is `409` `TAXONOMY_VALUE_IN_USE` naming the count, and **nothing at all \
             is written**, so the taxonomy an operator re-authors against is the one they had. \
             One transaction, one verdict: there is no partial application. A customer group is \
             never a price-row axis, so unlike `config`'s four taxonomies this guard has only one \
             plane to check. **`If-Match` is required**: send the opaque `ETag` the `GET` \
             returned. Gates on `customer_group` x `write` - a caller holding `config` x `write` \
             alone (declares regions, brands, partners, org tiers) does NOT satisfy this gate, by \
             design: per-payer commercial data is segregated from plan/config authoring.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(if_match_param())
        .json_request::<PutCustomerGroupTaxonomyRequest>(
            openapi,
            "The complete customer-group value set.",
        )
        .handler(put_customer_group_taxonomy)
        .json_response_with_schema::<CustomerGroupTaxonomyView>(
            openapi,
            StatusCode::OK,
            "The taxonomy as it now stands.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-178's edge, at this router's own tail — `taxonomies::router`'s reason: a
    // surface reachable without it cannot build an `AuditStamp`.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `GET /customer-groups/taxonomy`.
async fn get_customer_group_taxonomy(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx).await?;

    let held = state
        .taxonomies
        .list_customer_groups(&scope, ctx.subject_tenant_id())
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    Ok(render(&held, Some(&headers)))
}

/// `PUT /customer-groups/taxonomy`.
async fn put_customer_group_taxonomy(
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
    let now = OffsetDateTime::now_utc();

    let asserted = preconditions::if_match_policy(&headers).map_err(CanonicalError::from)?;
    let request: PutCustomerGroupTaxonomyRequest = preconditions::parse_body(&body)?;
    let entries = authored_entries(request.values)?;

    // The premise is handed to the store, not tested here — `taxonomies::
    // put_taxonomy`'s reason (D-186): this handler refuses a request it cannot
    // *understand*, and the store refuses one whose premise has *moved*, inside
    // the transaction that writes.
    let result = state
        .taxonomies
        .replace_customer_groups(
            &scope,
            tenant,
            entries,
            &asserted,
            audit_stamp(&ctx, now, correlation),
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    if result.stale {
        return Err(CanonicalError::from(DomainError::StaleVersion(
            "the If-Match tag no longer describes the customer-group taxonomy: it changed after \
             you read it. Re-read the GET and author against the tag it hands back - a PUT \
             replaces the whole value set, so applying yours over a moved one would retire \
             whatever the other author added"
                .to_owned(),
        )));
    }

    if let Some(violation) = result.report.violations.first() {
        debug_assert_eq!(violation.code, TAXONOMY_VALUE_IN_USE);
        return Err(CanonicalError::from(DomainError::TaxonomyValueInUse(
            violation.detail.clone(),
        )));
    }

    Ok(render(&result.entries, None))
}

// ---------------------------------------------------------------------------
// Rendering and parsing.
// ---------------------------------------------------------------------------

/// The representation, with the tag that covers it — `taxonomies::render`'s
/// reason: one function for both verbs, so the `GET`'s tag and the `PUT`'s
/// response tag cannot come from two renderings of one taxonomy.
/// `conditional` is `Some` on the `GET` and `None` on the `PUT`: the tag has one
/// producer and a conditional read must compare against *that* value, so the
/// comparison lives beside the rendering rather than in a second reading of the
/// same resource. See [`preconditions::if_none_match`].
fn render(entries: &[TaxonomyEntry], conditional: Option<&HeaderMap>) -> Response {
    let tag = preconditions::policy_etag(&customer_group_tag_of(entries));
    if conditional.is_some_and(|headers| preconditions::if_none_match(headers, &tag)) {
        return preconditions::not_modified(&tag);
    }
    (
        [(ETAG, tag)],
        Json(CustomerGroupTaxonomyView {
            resource: "customer-groups".to_owned(),
            values: entries.iter().map(view_of).collect(),
        }),
    )
        .into_response()
}

fn view_of(entry: &TaxonomyEntry) -> CustomerGroupValueView {
    CustomerGroupValueView {
        value: entry.value.as_str().to_owned(),
        display_name: entry.display_name.clone(),
        state: Some(entry.state.as_str().to_owned()),
    }
}

/// Turn the authored body into domain entries, refusing what is malformed.
fn authored_entries(
    values: Vec<CustomerGroupValueView>,
) -> Result<Vec<TaxonomyEntry>, CanonicalError> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut entries = Vec::with_capacity(values.len());
    for value in values {
        let declared = ScopeValue::new(&value.value).ok_or_else(|| {
            CanonicalError::from(DomainError::InvalidRequest(
                "a customer-group value must not be blank or whitespace: the empty string is \
                 the store's sentinel for the classless overlay scope, so a blank value here \
                 would make that sentinel forgeable"
                    .to_owned(),
            ))
        })?;
        if !seen.insert(declared.as_str().to_owned()) {
            // Refused rather than de-duplicated: a body naming one value twice
            // has two states for it and no rule says which wins.
            return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
                "value `{declared}` appears twice in this body; a taxonomy declares each value \
                 once, and a repeated one leaves its state undecided"
            ))));
        }
        let state = match value.state.as_deref() {
            None => TaxonomyState::Active,
            Some(token) => TaxonomyState::parse(token).ok_or_else(|| {
                CanonicalError::from(DomainError::InvalidRequest(format!(
                    "value `{declared}` carries state `{token}`; a taxonomy value is `active` or \
                     `retired`, and nothing else"
                )))
            })?,
        };
        entries.push(TaxonomyEntry {
            value: declared,
            display_name: value.display_name,
            state,
            tax: None,
        });
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// The gates.
// ---------------------------------------------------------------------------

/// The `customer_group × read` gate.
///
/// **Deliberately not `config × read`.** See the module doc: filing this
/// surface under `config` would hand every `CatalogAdmin` who declares a
/// region or a brand the same read over the customer-group vocabulary, which
/// is exactly the segregation `05-governance.md` draws.
async fn read_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::CUSTOMER_GROUP,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// The `customer_group × write` gate.
///
/// `owner_tenant_id = Some(caller's tenant)` because this is a write, for
/// `taxonomies::write_scope`'s reason: the membership assertion is what
/// refuses a target outside the compiled scope, the degraded flat-`In`
/// decision not re-checking the property.
async fn write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::CUSTOMER_GROUP,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

// ---------------------------------------------------------------------------
// Membership: `POST.../members`, `PATCH.../members/{id}`,
// `POST.../members/{payerId}/move` (`design/09-price-overlays.md` §3
// `inst-cg-record`/`inst-ms-move`, §5).
//
// Every one of these three routes gates on the **same** `customer_group ×
// write` pair the taxonomy `PUT` above does — [`write_scope`] is reused
// verbatim, so this section mints no new authz vocabulary either. What is new
// is the state: these routes request a `CatalogVersion`
// (`dod-customer-group`'s MUST that every committed membership mutation is
// its own publish unit through the Foundation engine, D-06), which is
// `api::rest::state`'s own criterion for keeping a route off
// [`AuthoringState`]. Rather than widen the crate-wide `GovernanceState` —
// touched by every other authoring/governance route and constructed by hand
// at four call sites — [`MembershipState`] is a small state of its own,
// `api::rest::repricing_runs::ApiState`'s precedent: that state also pairs an
// authoring-shaped read with its own `registry: Arc<dyn
// CatalogVersionRegistryV1>` field, for the same reason stated there — "a
// repricing run's apply is not an authoring act; it commits, exactly as a
// publish does".
// ---------------------------------------------------------------------------

/// The membership routes' dependencies.
///
/// A dedicated state rather than a field on `GovernanceState` — see the
/// section banner above. The same registry `Arc` every other requester in
/// this gear holds: two requesters of one registry is one incrementer, never
/// two — `api::rest::state`'s module doc carries the argument.
#[derive(Clone)]
pub struct MembershipState {
    /// The provider the publish transaction opens its one transaction on.
    pub db: DBProvider<DbError>,
    /// The at-most-once gate `POST.../members` and `POST.../move` claim
    /// under — §5's `Idempotency-Key` column for both. `PATCH.../members/{id}`
    /// needs none of its own: its precondition is `If-Match`, and the
    /// compare-and-swap that answers it is already at-most-once by
    /// construction (a second identical `PATCH` either finds nothing left to
    /// move, or is told its tag is stale).
    pub idempotency: IdempotencyGate,
    /// The sole incrementer of `CatalogVersion` this plane requests from.
    pub registry: Arc<dyn CatalogVersionRegistryV1>,
}

/// One membership, as a caller reads it back.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MembershipView {
    /// The membership's durable name.
    pub membership_id: Uuid,
    /// The payer's commercial-profile key.
    pub payer_tenant_id: Uuid,
    /// The taxonomy value the payer is enrolled in over this interval.
    pub group_value: String,
    /// Inclusive start, UTC.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `null` is open-ended.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
    /// The concurrency token a later `PATCH` asserts as its `If-Match`.
    pub row_version: u64,
}

impl From<&group_membership_repo::MembershipRow> for MembershipView {
    fn from(row: &group_membership_repo::MembershipRow) -> Self {
        Self {
            membership_id: row.membership_id,
            payer_tenant_id: row.payer_tenant_id,
            group_value: row.group_value.clone(),
            effective_from: row.effective_from,
            effective_to: row.effective_to,
            row_version: row.row_version,
        }
    }
}

/// What `POST.../members` and `PATCH.../members/{id}` answer with.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MembershipMutationView {
    /// The membership as committed.
    pub membership: MembershipView,
    /// The registry's **pending** handle — the D-06 publish unit this
    /// mutation opened. The row is durable now; a caller resolving this
    /// payer's group through the read model sees it only once
    /// `CatalogVersionPublished` warms.
    pub pending_version_ref: String,
}

impl From<&membership_publish::MembershipPublishReceipt> for MembershipMutationView {
    fn from(receipt: &membership_publish::MembershipPublishReceipt) -> Self {
        Self {
            membership: MembershipView::from(&receipt.membership),
            pending_version_ref: receipt.pending_ref.clone(),
        }
    }
}

/// The move itself: what `POST.../move` carries under
/// [`MembershipMoveMaterialView::moved`] once it has committed.
///
/// Not a response body of its own: both
/// of the route's committing arms answer the material view, so this type is the
/// payload one of its members holds rather than a second schema on `200`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MembershipMoveView {
    /// The membership that was ended, `null` when the payer held none active
    /// at the move instant — the atomic move degrades to a plain enrollment
    /// rather than refusing.
    pub ended: Option<MembershipView>,
    /// The membership created in the target group.
    pub enrolled: MembershipView,
    /// The one registry handle both rows' publish unit was recorded against.
    pub pending_version_ref: String,
}

impl From<&membership_publish::MembershipMoveReceipt> for MembershipMoveView {
    fn from(receipt: &membership_publish::MembershipMoveReceipt) -> Self {
        Self {
            ended: receipt.ended.as_ref().map(MembershipView::from),
            enrolled: MembershipView::from(&receipt.enrolled),
            pending_version_ref: receipt.pending_ref.clone(),
        }
    }
}

/// The body of `POST /customer-groups/{group}/members`.
// `(request, response)` and not `(request)` alone: the guarded `POST` digests the
// **parsed** request, and that needs the type to serialize —
// `windows::ScheduleWindowRequest`'s own note, verbatim.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct EnrollMembershipRequest {
    /// The payer's commercial-profile key (`inst-cg-record`). AMS supplies
    /// this identity; tenant topology is never modified.
    pub payer_tenant_id: Uuid,
    /// Inclusive start of the half-open interval, UTC.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; absent is open-ended.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
}

/// The body of `PATCH /customer-groups/{group}/members/{id}`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EndMembershipRequest {
    /// The new exclusive end, UTC — `inst-ms-time`'s "ending early = setting
    /// `to`".
    #[serde(with = "rfc3339")]
    pub effective_to: OffsetDateTime,
}

/// The body of `POST /customer-groups/{group}/members/{payerId}/move`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct MoveMembershipRequest {
    /// The instant the move pivots on — the ended membership's new
    /// `effectiveTo` and the new one's `effectiveFrom`, both at once (D-09:
    /// "no gap, no overlap").
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// Request an **immediate** re-resolution rather than the renewal-aligned
    /// default (`inst-mm-immediate`, `inst-mm-renewal`). Absent or `false` is
    /// the default: the move commits directly, audit-only, exactly as it did
    /// before this field existed. `true` makes the move a **material** change
    /// — it takes the Slice 5 two-person rule and commits nothing until a
    /// second principal approves.
    pub immediate: Option<bool>,
}

/// The body of `POST /customer-groups/{group}/members/move`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct BulkMoveMembershipRequest {
    /// Every payer moving into `{group}`. One entry per payer; duplicates
    /// are refused (`MembershipMoveSet::new`).
    pub payer_ids: Vec<Uuid>,
    /// The instant every listed payer pivots on.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
}

/// What `POST …/members/move` answers, on both arms.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct BulkMembershipMoveMaterialView {
    /// `"submitted_for_approval"` or `"committed"`.
    pub outcome: String,
    /// One [`MembershipMoveView`] per payer, canonical payer order, once
    /// committed. `None` while the unit is still `submitted`.
    pub moved: Option<Vec<MembershipMoveView>>,
    /// The `BulkGroupMove` verdict this act was opened under. `None` once
    /// committed.
    pub materiality: Option<MaterialityView>,
    /// The one approval unit covering the whole set.
    pub approval: Option<ApprovalView>,
}

/// What `POST.../move` answers, on **every** arm.
///
/// [`crate::api::rest::windows::WindowMutationOutcomeView`]'s shape, for the
/// same reason: this route has three possible acts — commit directly, open a
/// unit, or apply one an earlier call already opened and a second principal
/// has since approved — and a reader must be able to tell which one this
/// response documents rather than inferring it from which fields happen to be
/// populated.
///
/// **It is every arm's**. The
/// renewal-aligned default answered [`MembershipMoveView`] bare, so the
/// operation declared two schemas on `200` — one of which an `OpenAPI` document
/// could not carry — and D-350 had made the undeclared one the arm every
/// backdated or now-dated move takes. One type on one status is the fix the
/// registration's own note named.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MembershipMoveMaterialView {
    /// `"submitted_for_approval"` or `"committed"` — see
    /// [`crate::api::rest::publish::OUTCOME_SUBMITTED`] and
    /// [`MEMBERSHIP_OUTCOME_COMMITTED`].
    pub outcome: String,
    /// The move as committed, when this call is the one that applied it.
    /// `None` while the unit is still `submitted`.
    pub moved: Option<MembershipMoveView>,
    /// The materiality verdict this act was opened under. `None` once
    /// committed — that verdict is `approval`'s own, and repeating it here
    /// would be a second reading of one record — and `None` on the
    /// renewal-aligned arm, where no verdict was ever evaluated.
    pub materiality: Option<MaterialityView>,
    /// The approval unit, in either state: `submitted` right after this call
    /// opened it, or `approved` when this call is the one reading it back to
    /// commit.
    pub approval: Option<ApprovalView>,
}

/// The outcome token [`MembershipMoveMaterialView::outcome`] carries once the
/// move has landed — the renewal-aligned arm committing directly, or the call
/// that applied an approved unit.
///
/// Not `crate::api::rest::publish::OUTCOME_PUBLISHED`: that token means a plan
/// became addressable, which is not what committing a membership move is —
/// `windows::OUTCOME_MUTATED`'s reading, applied to this subject.
const MEMBERSHIP_OUTCOME_COMMITTED: &str = "committed";

/// The operation `POST.../members` claims idempotency keys under.
const CREATE_MEMBER_OPERATION: &str = "bss_pricing.create_customer_group_member";

/// The operation `POST.../move` claims idempotency keys under — distinct
/// from [`CREATE_MEMBER_OPERATION`] so one client key used on both verbs does
/// not collide (`pricing_idempotency_dedup`'s key is `(tenant, operation,
/// client_key)`).
const MOVE_MEMBER_OPERATION: &str = "bss_pricing.move_customer_group_member";

/// Build the Axum router for the membership mutations and register them.
///
/// On [`MembershipState`], not [`AuthoringState`] — see the section banner.
#[allow(clippy::too_many_lines)] // five OperationBuilder chains, one per route; flat is clearer than helpers
pub fn governance_router(state: Arc<MembershipState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/customer-groups/{group}/members")
        .operation_id("bss_pricing.list_customer_group_members")
        .summary("Read a customer group's memberships")
        .description(
            "Every membership recorded in the group, oldest interval first. **Ended memberships \
             are included**, and that is the point rather than an oversight: this slice names an \
             auditor who reads membership history, membership is effective-dated, and a list of \
             only the currently-active intervals answers neither \"who is in this group\" nor \
             \"who has been\". Narrow to one payer with `$filter=payer_tenant_id eq '<uuid>'`. \
             **D-322, 2026-08-16**: the slice's endpoint table specified the three write verbs and \
             no read at all, so a membership could be created, ended and moved and never looked \
             at - an operator's only evidence that an enrolment landed was the 202 they had \
             already seen. Gates on `customer_group` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("group", "The group whose memberships are read.")
        // D-125's pair plus the family's own filter. With **no query parameter at
        // all** the response is every membership ever recorded in the group, over a
        // table whose ended rows are deliberately kept for a >=7-year retention,
        // against this layer's own opening sentence.
        .param(crate::api::rest::history::limit_param())
        .param(crate::api::rest::history::cursor_param())
        .handler(list_memberships)
        .with_odata_filter::<MembershipFilterField>()
        .with_odata_orderby::<MembershipFilterField>()
        .json_response_with_schema::<MembershipListView>(
            openapi,
            StatusCode::OK,
            "The group's memberships, ended ones included.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::post("/bss-pricing/v1/customer-groups/{group}/members")
        .operation_id("bss_pricing.create_customer_group_member")
        .summary("Enroll a payer into a customer group")
        .description(
            "Creates an effective-dated membership (`inst-cg-record`) and commits it directly: \
             the default membership change is renewal-aligned and audit-only (`inst-mm-renewal`) \
             - no approval unit opens. The committed mutation is its **own publish unit** through \
             the Foundation engine (D-06, `dod-customer-group`'s MUST): the response carries the \
             registry's **pending** handle, and a caller resolving this payer's group through the \
             read model sees the new membership only once `CatalogVersionPublished` warms it. \
             `{group}` MUST be declared and **active** in the tenant's customer-group taxonomy \
             (`GET/PUT .../customer-groups/taxonomy`) - absent or **retired** both answer \
             `GROUP_UNKNOWN` (400 carrying the code - the design set types it 422 and the \
             canonical family has no such category, so the code is the discriminator; \
             `api::rest::bundles`' \"422 does not exist on this platform\"), the retired case \
             deliberately not merged into a generic \"not found\": `inst-cg-taxonomy`'s retire \
             guard protects references a value already \
             carries, and a route that enrolled a new one into a retired group would leave that \
             guard's point unenforced for the one act that most needs it. Interval validation is \
             D-09's: an interval overlapping the payer's existing membership in the **same** group \
             is `MEMBERSHIP_OVERLAP` (409); one overlapping a membership in **another** group is \
             `MEMBERSHIP_CONFLICT` (409 - a payer holds at most one active membership across all \
             groups) - use the move operation instead. **An `Idempotency-Key` header is required \
             and is honoured**: the same key with the same body returns the first answer verbatim \
             and mints no second membership. Gates on `customer_group` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("group", "The group the payer is enrolled into.")
        .param(idempotency_key_param())
        .json_request::<EnrollMembershipRequest>(openapi, "The payer and the effective interval.")
        .handler(create_membership)
        .json_response_with_schema::<MembershipMutationView>(
            openapi,
            StatusCode::CREATED,
            "The membership as committed, carrying the publish unit's pending handle.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch("/bss-pricing/v1/customer-groups/{group}/members/{id}")
        .operation_id("bss_pricing.adjust_customer_group_member")
        .summary("End or adjust a membership's interval")
        .description(
            "Moves a membership's `effectiveTo` under the `If-Match` precondition - \
             `inst-ms-time`'s \"ending early = setting `to`\" (audited); records are never \
             mutated in place otherwise, history is retained. Audit-only and commits directly, \
             exactly as the create does, and is its own publish unit (D-06). **`{group}` is \
             checked, not decorative**: `{id}` is what the store keys on, and `{group}` MUST name \
             the membership's own `groupValue` or the response is `404` exactly as an absent \
             membership's would be (`api::rest::prices`' `row_of_plan` shape, applied here so a \
             caller cannot act on `{id}` through a `{group}` segment that disagrees with it). The \
             overlap checks run again against the narrowed interval: `MEMBERSHIP_OVERLAP` / \
             `MEMBERSHIP_CONFLICT` as the create's. A stale `If-Match` is `409` \
             `STALE_VERSION`. Gates on `customer_group` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "group",
            "The membership's own group. Checked against the stored row, not merely addressing - \
             a mismatch answers 404.",
        )
        .path_param("id", "The membership to end or adjust.")
        .param(crate::api::rest::plans::if_match_param(
            "The membership's own `rowVersion`, as an opaque tag - there is no `GET` on a \
             membership, so the tag a caller asserts is the one the create or a previous adjust \
             answered.",
        ))
        .json_request::<EndMembershipRequest>(openapi, "The new exclusive end.")
        .handler(adjust_membership)
        .json_response_with_schema::<MembershipMutationView>(
            openapi,
            StatusCode::OK,
            "The membership as it now stands, carrying the publish unit's pending handle.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post("/bss-pricing/v1/customer-groups/{group}/members/move")
        .operation_id("bss_pricing.move_customer_group_members")
        .summary("Atomically transfer many payers into {group}")
        .description(
            "The bulk group move (`inst-mm-bulk`): every `payer_ids` entry ends its active \
             membership, if any, and enrolls in `{group}` at `effective_from`, as one approval \
             unit. The route declares the act - always material, no `immediate` flag. The first \
             call commits nothing and answers `202` with `outcome: \"submitted_for_approval\"`. \
             After a second principal approves, the same POST answers `200` with \
             `outcome: \"committed\"` and `moved` for every payer. One payer on this door is \
             still bulk. `{group}` MUST be declared and active (`GROUP_UNKNOWN` otherwise). An \
             `Idempotency-Key` is required and is not claimed (the material arm's contract). \
             Gates on `customer_group` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("group", "The target group.")
        .param(idempotency_key_param())
        .json_request::<BulkMoveMembershipRequest>(
            openapi,
            "The payers and the instant they pivot on.",
        )
        .handler(move_memberships)
        .json_response_with_schema::<BulkMembershipMoveMaterialView>(
            openapi,
            StatusCode::OK,
            "The set committed: `outcome` is `\"committed\"` and `moved` is one entry per payer.",
        )
        .json_response_with_schema::<BulkMembershipMoveMaterialView>(
            openapi,
            StatusCode::ACCEPTED,
            "The unit is open: nothing moved, and `approval` names what a second principal must \
             decide.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router =
        OperationBuilder::post("/bss-pricing/v1/customer-groups/{group}/members/{payerId}/move")
            .operation_id("bss_pricing.move_customer_group_member")
            .summary("Atomically transfer a payer into {group}")
            .description(
                "The atomic move (`inst-ms-move`, D-09): ends the payer's active membership, if \
                 any, and enrolls a new one in `{group}` - the **target** group - both at the \
                 same instant, with no gap and no overlap. Audit-only and commits directly \
                 (`inst-mm-renewal`'s default), on **one** publish unit covering both rows: the \
                 response's `pendingVersionRef` is the one handle both memberships were recorded \
                 against, so the registry's D-47 batching resolves them into the same \
                 `CatalogVersion` together. A payer with no active membership at the move instant \
                 is simply enrolled - `moved.ended` answers `null`. `{group}` MUST be declared \
                 and **active** in the tenant's customer-group taxonomy - absent or retired both \
                 answer `GROUP_UNKNOWN` (400 carrying the code, `create_customer_group_member`'s \
                 own reason and the same status note). **An `Idempotency-Key` header is required \
                 and is honoured.** \
                 **Every arm answers `MembershipMoveMaterialView`**, and `outcome` is the \
                 discriminator: the status alone does not say which arm ran, exactly as on every \
                 other two-arm mutation in this gear. A move the server reads as **immediate** - \
                 `immediate: true`, or an `effectiveFrom` landing at or before now (D-350) - \
                 takes the Slice 5 two-person rule (`inst-mm-immediate`): the first such call \
                 commits nothing and answers `202` with `outcome: \"submitted_for_approval\"` \
                 and the opened unit, and the call made after a second principal approves it \
                 answers `200` with `outcome: \"committed\"` and `moved`. The renewal-aligned \
                 default answers `200` with `outcome: \"committed\"` on the first call: it \
                 commits directly, so `materiality` and `approval` are both `null` there. Gates \
                 on `customer_group` x `write`.",
            )
            .tag(TAG)
            .authenticated()
            .no_license_required()
            .path_param("group", "The target group.")
            .path_param("payerId", "The payer being moved.")
            .param(idempotency_key_param())
            .json_request::<MoveMembershipRequest>(openapi, "The instant the move pivots on.")
            .handler(move_membership)
            // **One type on one status, which is what an OpenAPI document can
            // express**. This
            // registration declared `MembershipMoveView` and the operation had two
            // schemas on `200`: `move_membership_immediate`'s commit arm has always
            // answered `MembershipMoveMaterialView` there, and D-350 made that arm
            // the one every backdated or now-dated move takes — so the declared 200
            // was missing `enrolled` and `pending_version_ref`, both required, for a
            // caller validating against the document.
            //
            // Closed the way the note that stood here named, and the way
            // `windows::WindowMutationOutcomeView` already resolves the same
            // problem: the renewal-aligned arm answers the material view too, with
            // `outcome: "committed"` and the move under `moved`. It changes the body
            // of the arm every current caller uses, which is why it was a contract
            // decision rather than a declaration fix — the alternative was to leave
            // one of the two arms undeclarable.
            .json_response_with_schema::<MembershipMoveMaterialView>(
                openapi,
                StatusCode::OK,
                "The move committed: `outcome` is `\"committed\"` and `moved` carries the ended \
                 membership (if any), the new one, and the publish unit's pending handle. Either \
                 the renewal-aligned default committing directly, or the call that applied a \
                 material move a second principal had approved.",
            )
            .json_response_with_schema::<MembershipMoveMaterialView>(
                openapi,
                StatusCode::ACCEPTED,
                "`immediate: true` and the unit is open: nothing moved, and `approval` names what \
                 a second principal must decide.",
            )
            .error_400(openapi)
            .error_401(openapi)
            .error_403(openapi)
            .error_409(openapi)
            .error_500(openapi)
            .error_503(openapi)
            .register(router, openapi);

    // D-178's edge, `taxonomies::router`'s reason: a mutation reachable
    // without it cannot build an `AuditStamp`.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// The group path segment, non-blank — [`authored_entries`]'s validation,
/// applied to one value instead of a whole set.
fn required_group(group: &str) -> Result<String, CanonicalError> {
    ScopeValue::new(group)
        .map(|value| value.as_str().to_owned())
        .ok_or_else(|| {
            CanonicalError::from(DomainError::InvalidRequest(
                "the group segment must not be blank or whitespace".to_owned(),
            ))
        })
}

/// The membership named by `{id}`, **confirmed to belong to `{group}`**.
///
/// `PATCH.../members/{id}` names two identifiers in one path — `{group}` and
/// `{id}` — while [`group_membership_repo`] keys on `membership_id` alone, so
/// nothing else confirms they agree. `prices::row_of_plan`'s shape and its own
/// reason, applied here on the coordinator's explicit direction: a membership
/// under the wrong group's URL is answered exactly like an absent one, rather
/// than silently acted on through `{id}` while `{group}` is ignored — the
/// silent version is what lets a caller move a membership it did not mean to
/// touch.
///
/// # Errors
/// [`DomainError::NotFound`] when no membership in the caller's scope answers
/// to `id`, **or** one does and its own `group_value` is not `group` — the
/// same answer either way, so the response does not confirm which group a
/// membership the caller cannot reach through this path actually belongs to.
async fn membership_of_group(
    state: &MembershipState,
    scope: &AccessScope,
    tenant: Uuid,
    group: &str,
    membership_id: Uuid,
) -> Result<(), CanonicalError> {
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: membership lookup: {e}")).create()
    })?;
    let found = group_membership_repo::find(&conn, scope, tenant, membership_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    match found {
        Some(row) if row.group_value == group => Ok(()),
        _ => Err(CanonicalError::from(DomainError::NotFound {
            subject: "membership".to_owned(),
            id: membership_id.to_string(),
        })),
    }
}

/// `GET /customer-groups/{group}/members` (D-322).
///
/// Reads through the same repo the resolution walk uses, so an operator and
/// Tariffs are looking at one set of intervals rather than two projections of
/// it. The group is **not** validated against the taxonomy here: a group that
/// has been retired still has the memberships it accumulated, and refusing to
/// show them would hide exactly what the retire guard is protecting.
#[allow(clippy::implicit_hasher)]
async fn list_memberships(
    Extension(state): Extension<Arc<MembershipState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(group): Path<String>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let scope = read_scope(&enforcer, &ctx).await?;
    let group_value = required_group(&group)?;

    let conn = state.db.conn().map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!("membership conn: {e}")))
    })?;
    let page = group_membership_repo::list_odata(
        &conn,
        &scope,
        ctx.subject_tenant_id(),
        &group_value,
        &odata,
    )
    .await
    .map_err(map_odata_page_err)?;

    Ok(Json(MembershipListView {
        group_value,
        memberships: page.items.iter().map(MembershipView::from).collect(),
        page_info: page.page_info,
    })
    .into_response())
}

/// `POST /customer-groups/{group}/members`.
async fn create_membership(
    Extension(state): Extension<Arc<MembershipState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(group): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = write_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();
    let key = preconditions::idempotency_key(&headers)?;
    let request: EnrollMembershipRequest = preconditions::parse_body(&body)?;
    let digest = preconditions::request_digest(&request)?;
    let group_value = required_group(&group)?;

    let now = OffsetDateTime::now_utc();
    let stamp = audit_stamp(&ctx, now, correlation);
    let registry = Arc::clone(&state.registry);
    let mutation_ctx = ctx.clone();
    let mutation_scope = scope.clone();
    let payer_tenant_id = request.payer_tenant_id;
    let effective_from = request.effective_from;
    let effective_to = request.effective_to;

    let guarded = idempotent::guarded(
        &state.db,
        &state.idempotency,
        &scope,
        GuardedRequest {
            operation: CREATE_MEMBER_OPERATION,
            client_key: key,
            request_hash: digest,
            tenant_id: tenant,
            status: StatusCode::CREATED.as_u16().into(),
            now,
        },
        move |txn| {
            Box::pin(async move {
                // **Inside the guarded body, and the id below is not the only thing
                // that has to be**. This refusal is a read of
                // the payer's membership history, so on a retry the history it finds
                // is the retry's *own first call*: evaluated ahead of the gate it
                // answered `400` "use the move route" for an enrolment that had
                // already landed, which is the inversion the mandatory
                // `Idempotency-Key` exists to prevent. A replay does not reach this
                // closure at all, so here it cannot see the row it created itself.
                //
                // The claim is already written by the time this runs, and that is
                // sound rather than a leak: a refusal rolls the whole transaction
                // back with it, which is `infra::idempotent`'s central argument —
                // "a retry of a key whose request never happened claims afresh".
                refuse_a_move_by_the_side_door(
                    txn,
                    &mutation_scope,
                    tenant,
                    payer_tenant_id,
                    effective_from,
                    now,
                )
                .await?;
                // Minted **inside** the guarded body, `windows::schedule_window`'s rule: an
                // id minted outside would be a second one nobody is ever told about, since
                // a replay does not reach this closure at all.
                let membership_id = Uuid::now_v7();
                membership_publish::enroll_in(
                    txn,
                    registry.as_ref(),
                    &mutation_ctx,
                    &mutation_scope,
                    tenant,
                    NewMembership {
                        membership_id,
                        tenant_id: tenant,
                        payer_tenant_id,
                        group_value,
                        effective_from,
                        effective_to,
                    },
                    stamp,
                )
                .await
            })
        },
        |receipt| body_of(&MembershipMutationView::from(receipt)),
    )
    .await?;

    Ok(match guarded {
        Guarded::Performed(receipt) => (
            StatusCode::CREATED,
            [(
                ETAG,
                preconditions::etag(RowVersion::new(receipt.membership.row_version)),
            )],
            Json(MembershipMutationView::from(&receipt)),
        )
            .into_response(),
        Guarded::Replayed { status, body } => replayed(CREATE_MEMBER_OPERATION, status, &body)?,
    })
}

/// `PATCH /customer-groups/{group}/members/{id}`.
///
/// No idempotency gate — the precondition is `If-Match`, and the
/// compare-and-swap [`group_membership_repo::end_membership`] runs is
/// already at-most-once by construction (`windows::adjust_window`'s own
/// shape: parsed and asked for before anything is resolved, compared inside
/// the writing transaction rather than out here).
async fn adjust_membership(
    Extension(state): Extension<Arc<MembershipState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path((group, id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = write_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();
    let expected = preconditions::if_match(&headers)?;
    let request: EndMembershipRequest = preconditions::parse_body(&body)?;
    let group_value = required_group(&group)?;
    membership_of_group(&state, &scope, tenant, &group_value, id).await?;

    let now = OffsetDateTime::now_utc();
    let stamp = audit_stamp(&ctx, now, correlation);
    let registry = Arc::clone(&state.registry);
    let db = state.db.clone();
    let mutation_ctx = ctx.clone();
    let mutation_scope = scope.clone();
    let effective_to = request.effective_to;
    let expected_version = expected.get();

    let (_, outcome) = db
        .db()
        .in_transaction::<membership_publish::MembershipPublishReceipt, DomainError, _>(
            move |txn| {
                Box::pin(async move {
                    membership_publish::end_in(
                        txn,
                        registry.as_ref(),
                        &mutation_ctx,
                        &mutation_scope,
                        tenant,
                        id,
                        effective_to,
                        expected_version,
                        stamp,
                    )
                    .await
                })
            },
        )
        .await;
    let receipt = outcome.map_err(|err| {
        err.into_domain(|infra| {
            DomainError::Internal(format!("bss-pricing: membership end transaction: {infra}"))
        })
    })?;

    Ok((
        StatusCode::OK,
        [(
            ETAG,
            preconditions::etag(RowVersion::new(receipt.membership.row_version)),
        )],
        Json(MembershipMutationView::from(&receipt)),
    )
        .into_response())
}

/// `POST /customer-groups/{group}/members/{payerId}/move`.
async fn move_membership(
    Extension(state): Extension<Arc<MembershipState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path((group, payer_id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = write_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();
    let key = preconditions::idempotency_key(&headers)?;
    let request: MoveMembershipRequest = preconditions::parse_body(&body)?;
    let digest = preconditions::request_digest(&request)?;
    let group_value = required_group(&group)?;

    // **The material fork (`inst-mm-immediate`), ahead of the guard.** An
    // immediate re-resolution is a different act from the renewal-aligned
    // default below, not a variation on it: it opens no idempotency claim at
    // all (`idempotent::guarded`'s replay would answer the *first* call's 202
    // forever, which is exactly wrong for a caller retrying after a second
    // principal has approved — the same "a fresh Idempotency-Key marks a
    // fresh attempt" contract `windows::schedule_window`'s material arm
    // relies on) and it may commit nothing on this call at all.
    // **The act decides, not the caller** (D-350).
    //
    // `immediate` was the sole discriminator, so the two-person rule
    // `inst-mm-immediate` states was *elective*: one principal holding
    // `customer_group x write` omitted the member and `move_payer_in` committed
    // the identical rows — byte for byte, no outbox event, nothing in the store
    // distinguishing an approved move from an unapproved one. The flag stays,
    // because a caller declaring intent up front is worth answering precisely, but
    // it can only ever *widen*: a move that lands at or before now **is** an
    // immediate re-resolution whatever the body says. `inst-ws-future-start`'s
    // strictness is the precedent for reading an instant against the clock rather
    // than against a claim.
    // **The recorded answer is consulted before the fork, not inside one arm of it**. The fork
    // below reads the wall clock, and the guard is only reached down the renewal-aligned branch
    // — so a caller whose move landed through that branch, and whose retry under the same key
    // arrives after `effective_from` has passed, would take the immediate arm, consult nothing,
    // and open an approval unit for a move that already committed. Approving it runs
    // `move_payer_in` a second time on the same payer.
    //
    // This read opens no claim, so it does not disturb the immediate arm's contract
    // that a fresh key marks a fresh attempt: it answers only where an answer was
    // already recorded, which can only have happened down the guarded branch.
    let now_for_replay = OffsetDateTime::now_utc();
    let (_, replay) = state
        .db
        .db()
        .in_transaction::<Option<(i32, serde_json::Value)>, DomainError, _>({
            let gate = state.idempotency.clone();
            let scope = scope.clone();
            let key = key.clone();
            let digest = digest.clone();
            move |txn| {
                Box::pin(async move {
                    gate.recorded_response(
                        txn,
                        &scope,
                        tenant,
                        MOVE_MEMBER_OPERATION,
                        &key,
                        &digest,
                        now_for_replay,
                    )
                    .await
                    .map_err(|e| crate::infra::storage::repo_failure(&e))
                })
            }
        })
        .await;
    let replay = replay.map_err(crate::infra::idempotent::tx_failure)?;
    if let Some((status, body)) = replay {
        return replayed(MOVE_MEMBER_OPERATION, status, &body).map_err(CanonicalError::from);
    }

    let lands_now = request.effective_from <= OffsetDateTime::now_utc();
    if request.immediate == Some(true) || lands_now {
        return move_membership_immediate(
            &state,
            &ctx,
            &scope,
            tenant,
            group_value,
            payer_id,
            request.effective_from,
            correlation,
        )
        .await;
    }

    let now = OffsetDateTime::now_utc();
    let stamp = audit_stamp(&ctx, now, correlation);
    let registry = Arc::clone(&state.registry);
    let mutation_ctx = ctx.clone();
    let mutation_scope = scope.clone();
    let effective_from = request.effective_from;

    let guarded = idempotent::guarded(
        &state.db,
        &state.idempotency,
        &scope,
        GuardedRequest {
            operation: MOVE_MEMBER_OPERATION,
            client_key: key,
            request_hash: digest,
            tenant_id: tenant,
            status: StatusCode::OK.as_u16().into(),
            now,
        },
        move |txn| {
            Box::pin(async move {
                let new_membership_id = Uuid::now_v7();
                membership_publish::move_payer_in(
                    txn,
                    registry.as_ref(),
                    &mutation_ctx,
                    &mutation_scope,
                    tenant,
                    payer_id,
                    new_membership_id,
                    group_value,
                    effective_from,
                    stamp,
                )
                .await
            })
        },
        // The **recorded** body is the material view too, and it has to be: a
        // replay is handed back verbatim, so recording the bare
        // `MembershipMoveView` here would make one key answer a shape the
        // declaration no longer names on the first call's own status.
        |receipt| body_of(&committed_move(receipt)),
    )
    .await?;

    Ok(match guarded {
        Guarded::Performed(receipt) => {
            (StatusCode::OK, Json(committed_move(&receipt))).into_response()
        }
        Guarded::Replayed { status, body } => replayed(MOVE_MEMBER_OPERATION, status, &body)?,
    })
}

/// The `200` body of a move that **committed**, on either arm.
///
/// One shape for one status: the renewal-aligned arm and
/// `move_membership_immediate`'s commit arm answer the same type, discriminated by
/// `outcome`, which is [`crate::api::rest::windows::WindowMutationOutcomeView`]'s
/// arrangement and the one an `OpenAPI` document can declare.
///
/// `materiality` and `approval` are `None` here rather than empty: nothing about a
/// renewal-aligned move was evaluated, and a verdict rendered where none was taken
/// would be a verdict nobody made — `WindowMutationOutcomeView::materiality`'s own
/// reason for the same absence.
fn committed_move(
    receipt: &membership_publish::MembershipMoveReceipt,
) -> MembershipMoveMaterialView {
    MembershipMoveMaterialView {
        outcome: MEMBERSHIP_OUTCOME_COMMITTED.to_owned(),
        moved: Some(MembershipMoveView::from(receipt)),
        materiality: None,
        approval: None,
    }
}

/// Refuse an enrollment that **composes a group move** out of an audit-only door
/// (D-350).
///
/// `inst-mm-immediate` makes an immediate re-resolution a material change needing
/// a second principal, and the move route opens that unit. Half-open intervals
/// mean `PATCH …/members/{id}` closing an interval at `T` and this route opening
/// one at `T` compose **exactly** the row pair `move_payer_in` writes — no
/// overlap for `refuse_overlap` to catch, no approval anywhere, two calls one
/// principal can make.
///
/// **Narrow on purpose, and the narrowness is the decision.** A first enrollment
/// is onboarding, not a move: the payer was in no group, so nothing re-resolves
/// and refusing it would refuse the ordinary act this route exists for. What
/// cannot be audit-only is an enrollment landing **now or in the past** for a
/// payer who **already has membership history** — that is a re-resolution of a
/// payer who was somewhere else, which is the act `inst-mm-immediate` names. A
/// future-dated enrollment is renewal-aligned by construction and stays open.
///
/// One extra read on the create path, and it is the read `move_payer_in` already
/// makes for the same payer.
///
/// # Where it runs, which is load-bearing
///
/// **Inside [`create_membership`]'s guarded body, never in front of the gate.**
/// The operand is the payer's *whole* membership history —
/// [`group_membership_repo::intervals_for_payer`] filters on tenant and payer
/// only, so every interval the payer ever held answers, ended or not, in any
/// group — and a retry's history therefore includes the row its own first call
/// wrote. Ahead of `idempotent::guarded` this guard pre-empted the replay and
/// answered `400` for an enrolment that had already landed; behind it, a replayed
/// key returns the recorded `201` without reaching this function at all. That is
/// also why it takes a `runner` rather than the state: the read joins the
/// transaction the claim was made in, and the toolkit refuses `Db::conn()` inside
/// an open one.
///
/// `now` is the handler's own instant, the one the claim and the audit stamp are
/// both dated by (`GuardedRequest::now`'s reason: a request is never judged
/// against a clock other than its own).
///
/// # Errors
/// [`DomainError::InvalidRequest`] when the enrollment re-resolves a payer who
/// already holds membership history; [`RepoError`]'s ladder on a storage failure.
async fn refuse_a_move_by_the_side_door(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant: Uuid,
    payer_tenant_id: Uuid,
    effective_from: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if effective_from > now {
        return Ok(());
    }
    let held = group_membership_repo::intervals_for_payer(runner, scope, tenant, payer_tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    if held.is_empty() {
        return Ok(());
    }
    Err(DomainError::InvalidRequest(
        "`effectiveFrom` takes effect now or in the past for a payer who already holds \
         membership history, which re-resolves them into this group immediately \
         (`inst-mm-immediate`) - a material change needing a second principal. This route is \
         audit-only and opens no approval unit, and paired with an end on the payer's current \
         membership it composes the same interval pair a move writes. Use POST \
         /bss-pricing/v1/customer-groups/{group}/members/{payerId}/move, which opens the unit - \
         or author this enrollment to take effect at a future instant"
            .to_owned(),
    ))
}

/// What the material half of a move door answered, before the door renders it
/// in its own view.
enum MembershipMoveOutcome {
    /// An approved unit over this exact set existed and committed.
    Committed(Vec<membership_publish::MembershipMoveReceipt>),
    /// No such unit yet: one was opened, and this is its verdict and record.
    Submitted(Box<OpenedMove>),
}

/// The unit a first arrival opened, with the verdict it recorded.
struct OpenedMove {
    verdict: MaterialityVerdict,
    opened: approval_repo::ApprovalRecord,
}

/// The material half both move doors share — `inst-mm-immediate`'s two acts on
/// one route (`overlays::submit_overlay`'s D-234 shape, applied to a subject with
/// no draft table), which `inst-mm-bulk` takes over a set.
///
/// **First arrival** — no approved unit over this exact `(payers, group,
/// instant)` yet: record the door's materiality `verdict`, open the Slice 5 unit
/// and answer [`MembershipMoveOutcome::Submitted`]. **Nothing is written to the
/// membership plane on this call** — `inst-mm-pending`'s own rule, restated
/// from the route's side: no `pricing_group_membership` row exists until a
/// second principal approves.
///
/// **A later arrival, once approved** — [`approval_repo::find_approved_for_content`]
/// finds a unit over this exact subject and content, [`ApprovalService::commit_membership_move_in`]
/// re-verifies the two-person rule and applies the move atomically, and this
/// call answers [`MembershipMoveOutcome::Committed`] carrying the receipts —
/// from the payer's read model onward a material move and a renewal-aligned one
/// converge on one publish unit.
///
/// `door` names the door in a failure detail (`membership move`, `bulk membership
/// move`); the door renders the outcome because the two answer different views.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the state, the security context \
              and scope the gate compiled, the tenant, the target group the path names, the \
              set the door assembled, the correlation the HTTP edge established, and the \
              door's own verdict and name. `move_membership`'s own reason, one call up."
)]
async fn move_membership_set(
    state: &MembershipState,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant: Uuid,
    group_value: &str,
    set: MembershipMoveSet,
    correlation: Uuid,
    verdict: fn() -> MaterialityVerdict,
    door: &str,
) -> Result<MembershipMoveOutcome, CanonicalError> {
    let subject_ref = approval_repo::membership_move_subject_ref(&set)
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let pin = crate::domain::approval::content_pin::membership_content_hash(&set);

    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: {door} lookup: {e}")).create()
    })?;
    // **The target group is judged on this arm too** (D-350). The refusal lives
    // inside `move_payer_in`, which this arm does not reach until an approval
    // exists — so without the judgement here an undeclared or retired group is
    // refused on the committing arm and answered `202` on this one, opening a unit
    // over a move that can never commit. Invisible while `immediate` is the only
    // way in; D-350 routes a backdated move here, and this is the door it arrives
    // at.
    membership_publish::require_active_group(&conn, scope, tenant, group_value)
        .await
        .map_err(CanonicalError::from)?;

    let approved =
        approval_repo::find_approved_for_content(&conn, scope, tenant, &subject_ref, &pin)
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    if let Some(approved) = approved {
        let stamp = audit_stamp(ctx, OffsetDateTime::now_utc(), correlation);
        let registry = Arc::clone(&state.registry);
        let commit_ctx = ctx.clone();
        let commit_scope = scope.clone();
        let (_, outcome) = state
            .db
            .db()
            .in_transaction::<Vec<membership_publish::MembershipMoveReceipt>, DomainError, _>(
                move |txn| {
                    Box::pin(async move {
                        ApprovalService::commit_membership_move_in(
                            txn,
                            registry.as_ref(),
                            &commit_ctx,
                            &commit_scope,
                            tenant,
                            &approved,
                            stamp,
                        )
                        .await
                    })
                },
            )
            .await;
        let receipts = outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: {door} commit: {infra}"))
            })
        })?;
        return Ok(MembershipMoveOutcome::Committed(receipts));
    }

    let verdict = verdict();
    let (_reason, stored_materiality) = crate::api::rest::overlays::rendered_materiality(&verdict)?;
    let stamp = audit_stamp(ctx, OffsetDateTime::now_utc(), correlation);
    let submit_scope = scope.clone();
    let set_for_submit = set.clone();
    let (_, outcome) = state
        .db
        .db()
        .in_transaction::<approval_repo::ApprovalRecord, DomainError, _>(move |txn| {
            Box::pin(async move {
                ApprovalService::submit_membership_move_on(
                    txn,
                    &submit_scope,
                    tenant,
                    &set_for_submit,
                    Uuid::now_v7(),
                    stored_materiality,
                    stamp,
                )
                .await
            })
        })
        .await;
    let opened = outcome.map_err(|err| {
        err.into_domain(|infra| {
            DomainError::Internal(format!("bss-pricing: {door} submit: {infra}"))
        })
    })?;
    Ok(MembershipMoveOutcome::Submitted(Box::new(OpenedMove {
        verdict,
        opened,
    })))
}

/// The material half of `POST .../members/{payerId}/move`: one payer through
/// [`move_membership_set`], answered in the single-payer view. `200` carries the
/// committed membership and the D-06 publish unit's pending handle — the same
/// shape the audit-only path answers; `202` carries the unit it opened.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the state, the security context \
              and scope the gate compiled, the tenant, the target group and payer the path \
              names, the instant the move pivots on, and the correlation the HTTP edge \
              established. `move_membership`'s own reason, one call up."
)]
async fn move_membership_immediate(
    state: &MembershipState,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant: Uuid,
    group_value: String,
    payer_id: Uuid,
    effective_from: OffsetDateTime,
    correlation: Uuid,
) -> Result<Response, CanonicalError> {
    let set = MembershipMoveSet::new(vec![MembershipMoveProposal {
        payer_tenant_id: payer_id,
        group_value: group_value.clone(),
        effective_from,
    }])
    .map_err(CanonicalError::from)?;
    let outcome = move_membership_set(
        state,
        ctx,
        scope,
        tenant,
        &group_value,
        set,
        correlation,
        immediate_membership_materiality,
        "membership move",
    )
    .await?;
    match outcome {
        MembershipMoveOutcome::Committed(receipts) => {
            let receipt = receipts.into_iter().next().ok_or_else(|| {
                CanonicalError::from(DomainError::Internal(
                    "an approved single-payer membership move committed no receipt".to_owned(),
                ))
            })?;
            Ok((StatusCode::OK, Json(committed_move(&receipt))).into_response())
        }
        MembershipMoveOutcome::Submitted(submitted) => Ok((
            StatusCode::ACCEPTED,
            Json(MembershipMoveMaterialView {
                outcome: crate::api::rest::publish::OUTCOME_SUBMITTED.to_owned(),
                moved: None,
                materiality: Some(MaterialityView::from(&submitted.verdict)),
                approval: Some(ApprovalView::from(&submitted.opened)),
            }),
        )
            .into_response()),
    }
}

/// The materiality verdict `inst-mm-immediate`'s unit records — **evaluated,
/// not asserted** (`threshold_policy::policy_diff_materiality`'s shape and
/// reason): the stored `materiality` jsonb is produced by the same evaluator
/// every other unit's is, and this call is what makes
/// [`Trigger::ImmediateMembershipReresolution`] real — see that trigger's own
/// note in `domain::materiality::triggers` on a declaration being distinct
/// from a `pub fn` that builds one.
fn immediate_membership_materiality() -> MaterialityVerdict {
    materiality::evaluate(
        &ChangeSet::of_act(Trigger::ImmediateMembershipReresolution, Vec::new()),
        /* policy */ None,
        /* baseline */ None,
    )
}

/// `POST /customer-groups/{group}/members/move` — `inst-mm-bulk`.
///
/// Always the material arm. The route is the declaration; the count of
/// `payer_ids` is not.
async fn move_memberships(
    Extension(state): Extension<Arc<MembershipState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(group): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = write_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();
    let _key = preconditions::idempotency_key(&headers)?;
    let request: BulkMoveMembershipRequest = preconditions::parse_body(&body)?;
    let group_value = required_group(&group)?;
    let set = MembershipMoveSet::new(
        request
            .payer_ids
            .iter()
            .map(|payer_tenant_id| MembershipMoveProposal {
                payer_tenant_id: *payer_tenant_id,
                group_value: group_value.clone(),
                effective_from: request.effective_from,
            })
            .collect(),
    )
    .map_err(CanonicalError::from)?;
    move_memberships_bulk(&state, &ctx, &scope, tenant, group_value, set, correlation).await
}

/// The material half of the bulk door: the set through [`move_membership_set`],
/// answered in the set view — one unit, then one commit of the set.
async fn move_memberships_bulk(
    state: &MembershipState,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant: Uuid,
    group_value: String,
    set: MembershipMoveSet,
    correlation: Uuid,
) -> Result<Response, CanonicalError> {
    let outcome = move_membership_set(
        state,
        ctx,
        scope,
        tenant,
        &group_value,
        set,
        correlation,
        bulk_membership_materiality,
        "bulk membership move",
    )
    .await?;
    match outcome {
        MembershipMoveOutcome::Committed(receipts) => Ok((
            StatusCode::OK,
            Json(BulkMembershipMoveMaterialView {
                outcome: MEMBERSHIP_OUTCOME_COMMITTED.to_owned(),
                moved: Some(receipts.iter().map(MembershipMoveView::from).collect()),
                materiality: None,
                approval: None,
            }),
        )
            .into_response()),
        MembershipMoveOutcome::Submitted(submitted) => Ok((
            StatusCode::ACCEPTED,
            Json(BulkMembershipMoveMaterialView {
                outcome: crate::api::rest::publish::OUTCOME_SUBMITTED.to_owned(),
                moved: None,
                materiality: Some(MaterialityView::from(&submitted.verdict)),
                approval: Some(ApprovalView::from(&submitted.opened)),
            }),
        )
            .into_response()),
    }
}

/// The materiality verdict `inst-mm-bulk`'s unit records.
fn bulk_membership_materiality() -> MaterialityVerdict {
    materiality::evaluate(
        &ChangeSet::of_act(Trigger::BulkGroupMove, Vec::new()),
        /* policy */ None,
        /* baseline */ None,
    )
}

/// The recorded body of a guarded membership mutation: the view, as JSON.
///
/// # Errors
/// [`DomainError::Internal`] when the view will not serialize — unreachable,
/// and reported rather than unwrapped for `windows::body_of`'s reason: it
/// would otherwise abort a transaction that has already written.
fn body_of<T: serde::Serialize>(view: &T) -> Result<serde_json::Value, DomainError> {
    serde_json::to_value(view)
        .map_err(|e| DomainError::Internal(format!("cannot render the membership mutation: {e}")))
}

/// The answer a replay is handed back — the stored status and body, verbatim.
///
/// # Errors
/// [`DomainError::Internal`] when the stored status is not one — see
/// [`super::replayed_status`].
fn replayed(
    operation: &str,
    status: i32,
    body: &serde_json::Value,
) -> Result<Response, DomainError> {
    let status = super::replayed_status(operation, status)?;
    Ok((status, Json(body.clone())).into_response())
}
