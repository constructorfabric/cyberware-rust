//! The bundle authoring surface — `design/08-bundles.md` §5,
//! `inst-ba-author` / `inst-ba-validate` / `inst-ba-return`.
//!
//! Three routes: create the bundle on its plan, replace an open draft revision's
//! whole composition, and publish it.
//!
//! # D-11 is why the third route's gate is not the first two's
//!
//! Authoring is `bundle × write` (`ProductManager`, `CatalogAdmin`). **Publish
//! requires `plan × publish` only** — D-11: under the
//! conjunction the design set originally stated, only `CatalogAdmin` could publish
//! a bundle while §1.3 promises `FinanceManager` can, and one of the three
//! statements had to be wrong. Publish is a plan-level act; the authoring
//! authority was already exercised, and the composition is protected at publish
//! time by the approval content pin. Component checks are **validations, not
//! caller authz**, which is why nothing here gates on the components' own plans.
//!
//! # The composition is addressed by the bundle and versioned by the revision
//!
//! `If-Match` on the composition route carries the **plan revision's** entity tag
//! (`"<revision>-<version>"`, D-170), not a tag of the composition's own. The
//! composition has none, deliberately: a component-set edit advances the
//! revision's tag, so two authors editing one draft cannot both satisfy their
//! precondition. `BundleRepo`'s module doc carries the argument.
//!
//! **§5 spells the composition route `PATCH /bss-pricing/v1/bundles`** — a
//! collection PATCH with the subject in the body. This mounts
//! `PATCH /bss-pricing/v1/bundles/{bundleId}` instead, because a precondition
//! addresses a resource and a collection that answers `If-Match` is a collection
//! pretending to be one. The divergence is reported in the owed register (B-10)
//! rather than smoothed over.
//!
//! # 422 does not exist on this platform
//!
//! §5 types every composition refusal 422 and that notation is architectural:
//! `CanonicalError` renders `InvalidArgument`, `FailedPrecondition` and
//! `OutOfRange` all as **400**. The **code string** is the discriminator a
//! consumer matches on, and the ten of them travel inside the `ValidationFailed`
//! envelope, one violation per failing rule — which is what makes a composition
//! remediable in one pass. No route here declares a 422 response.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query};
use axum::http::header::{ETAG, LOCATION};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::HeaderMap, http::StatusCode};
use bss_pricing_sdk::odata::BundleFilterField;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::DbTx;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::ApprovalView;
use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::preconditions;
use crate::api::rest::state::AuthoringState;
use crate::domain::bundle::{
    Absorber, InvoiceItemization, Party, PartyShare, PriceBasis, RevShareGroup,
};
use crate::domain::bundle_rules::check_basis_declared;
use crate::domain::error::DomainError;
use crate::domain::materiality::{self, ChangeSet, MaterialityVerdict};
use crate::domain::money::CurrencyCode;
use crate::domain::scope_key::{PlanId, Region};
use crate::infra::idempotent::{self, Guarded, GuardedRequest, TxFuture};
use crate::infra::storage::repo::{BundleComponentDraft, CompositionDraft, NewBundle, bundle_repo};
use time::OffsetDateTime;

const TAG: &str = "BSS Pricing Bundles";

/// The at-most-once operation the bundle create claims under (§9).
///
/// Per-route, `plans.rs`' rule: the key is scoped to the operation, so one client
/// key used on two different verbs does not collide.
const CREATE_BUNDLE_OPERATION: &str = "bss_pricing.create_bundle";

/// The wire tokens for the two arms — `api::rest::publish`'s, so a client's
/// `match` does not depend on which plane it called, and so `"published"` has one
/// home rather than a third spelling beside `publish.rs`'s and `overlays.rs`'s.
const OUTCOME_SUBMITTED: &str = crate::api::rest::publish::OUTCOME_SUBMITTED;
const OUTCOME_PUBLISHED: &str = crate::api::rest::publish::OUTCOME_PUBLISHED;

/// `POST` — create a bundle on its plan.
pub const BUNDLES: &str = "/bss-pricing/v1/bundles";
/// `PATCH` — replace an open draft revision's whole composition.
pub const BUNDLE_BY_ID: &str = "/bss-pricing/v1/bundles/{bundleId}";
/// `POST` — validate and publish.
pub const BUNDLE_PUBLISH: &str = "/bss-pricing/v1/bundles/{bundleId}/publish";

// ---------------------------------------------------------------------------
// Wire types.
// ---------------------------------------------------------------------------

/// `POST /bss-pricing/v1/bundles`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct CreateBundleRequest {
    /// The `bundle`-type plan this bundle is the composition of.
    pub plan_id: Uuid,
    /// `sum_of_parts` or `own_price`. **Optional on the wire and required in
    /// substance**: `inst-bb-declared` says the basis MUST be declared, and
    /// `BASIS_MISSING` is what an absent one is told. Modelling it as
    /// `Option` is what makes that code reachable — a required field would be
    /// refused by the deserializer with a message the design set does not own.
    pub price_basis: Option<String>,
    /// `aggregate` or `itemize`. Defaults to `aggregate`, which is the layout
    /// that adds nothing to an invoice a bundle did not already have.
    pub invoice_itemization: Option<String>,
}

/// `PATCH /bss-pricing/v1/bundles/{bundleId}`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct CompositionRequest {
    /// The plan revision this composition belongs to.
    pub plan_revision: u64,
    /// The referenced components, whole. **Replaced, never merged** — every
    /// Slice-8 rule quantifies over the set.
    pub components: Vec<ComponentRequest>,
    /// The rev-share groups, one per included vendor SKU. Empty is legal and is
    /// what a bundle with no revenue to allocate looks like.
    #[serde(default)]
    pub rev_share: Vec<RevShareGroupRequest>,
}

/// One component of a submitted composition.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct ComponentRequest {
    /// The component's **plan** (B1) — a bare SKU id is ambiguous per
    /// `(currency, region)`.
    pub component_plan_id: Uuid,
    /// The registry SKU it publishes under.
    pub included_sku_id: Uuid,
    /// Selection-time lower bound.
    pub min_qty: Option<i32>,
    /// Selection-time upper bound.
    pub max_qty: Option<i32>,
}

/// One rev-share group of a submitted composition.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RevShareGroupRequest {
    /// The included vendor SKU whose revenue this group allocates.
    pub vendor_sku_id: Uuid,
    /// The group's explicit platform cut, in basis points.
    pub platform_cut_bp: i32,
    /// Who absorbs the publish-time residual — a party of this group, or the
    /// `platform` sentinel. Absent means `platform`, which is D-07's default and
    /// what makes an unnominated state unrepresentable.
    pub residual_absorber_party: Option<String>,
    /// The parties and their **typed** shares.
    pub parties: Vec<PartyShareRequest>,
}

/// One party's typed share.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PartyShareRequest {
    /// The recipient. May not be blank and may not be `platform`.
    pub party: String,
    /// The typed share, in basis points.
    pub share_bp: i32,
}

/// `POST /bss-pricing/v1/bundles/{bundleId}/publish`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PublishBundleRequest {
    /// The plan revision to publish.
    pub plan_revision: u64,
    /// The `(currency, region)` markets the bundle sells in.
    ///
    /// Supplied rather than derived because a `sum_of_parts` bundle carries no
    /// price rows of its own (`inst-bb-rowless`), so there is nothing to read the
    /// market set off. It is the coverage walk's domain and the tax-basis walk's.
    pub markets: Vec<MarketRequest>,
}

/// One sold market.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct MarketRequest {
    /// ISO 4217.
    pub currency: String,
    /// The region axis value.
    pub region: String,
}

/// What a created bundle answers with.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct BundleView {
    /// The bundle's identity.
    pub bundle_id: Uuid,
    /// The plan it rides.
    pub plan_id: Uuid,
    /// Its declared basis.
    pub price_basis: String,
    /// Its invoice layout.
    pub invoice_itemization: String,
}

/// What a composition write answers with.
///
/// A body rather than a bare `204`, because the entity tag it carries is the one
/// the caller needs for its next edit and a response with a header and no
/// document invites a client to ignore the body it does not have.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct CompositionAcceptedView {
    /// The bundle whose composition was replaced.
    pub bundle_id: Uuid,
    /// The revision it now stands at.
    pub plan_revision: u64,
}

/// `GET /bss-pricing/v1/bundles/{bundleId}` — the query half.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ReadBundleQuery {
    /// The revision to read. Absent means the plan's current or draft revision.
    pub plan_revision: Option<String>,
}

/// What the read answers with — the bundle's declaration and its composition.
///
/// The component and rev-share members are the **authoring** shapes
/// ([`ComponentRequest`], [`RevShareGroupRequest`]) rather than views of their
/// own, so what an author reads back is spelled exactly as what they wrote. A
/// second rendering of one composition is a second answer to what it is.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct BundleCompositionView {
    /// The bundle read.
    pub bundle_id: Uuid,
    /// The plan it rides.
    pub plan_id: Uuid,
    /// The revision this composition belongs to.
    pub plan_revision: u64,
    /// `sum_of_parts` or `own_price`.
    pub price_basis: String,
    /// `aggregate` or `itemize`.
    pub invoice_itemization: String,
    /// The referenced components.
    pub components: Vec<ComponentRequest>,
    /// The rev-share groups, one per included vendor SKU.
    pub rev_share: Vec<RevShareGroupRequest>,
}

/// What a publish answers with — which of the two acts it performed, and why.
///
/// `response` only, as `overlays::SubmitAcceptedView` is: it carries an
/// [`ApprovalView`], which is a projection of a stored record and has no
/// `Deserialize`. It was declared `request, response` while its fields were three
/// scalars, and nothing ever sent one.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PublishAcceptedView {
    /// The bundle this call acted on.
    pub bundle_id: Uuid,
    /// The revision it acted at.
    pub plan_revision: u64,
    /// Which act this was: `submitted_for_approval` on the first call,
    /// `published` on the call a second principal's approval authorized.
    ///
    /// D-104 makes a composition change always material, so the first call over
    /// any content **stages** it. `POST …/price-overlays/{id}/submit` answers the
    /// same two-token shape for the same reason.
    pub outcome: String,
    /// Why this change is material — **evaluated, not asserted**.
    ///
    /// It reads `alwaysMaterialTrigger` for every composition change (D-104), and
    /// that constancy is a property of the rule rather than of this field: the
    /// token is produced by [`bundle_publish_materiality`] through the same
    /// evaluator every other unit's is. A hard-coded literal here tells a client a
    /// property of the request that nothing has established, and a false token cannot
    /// be told from a real one downstream.
    pub materiality: String,
    /// The unit this call opened, on the submit arm; the unit that authorized it,
    /// on the publish arm.
    pub approval: Option<ApprovalView>,
}

// ---------------------------------------------------------------------------
// Conversions.
// ---------------------------------------------------------------------------

fn draft_of(request: &CompositionRequest) -> Result<CompositionDraft, DomainError> {
    let components = request
        .components
        .iter()
        .map(|c| BundleComponentDraft {
            component_plan_id: c.component_plan_id,
            included_sku_id: c.included_sku_id,
            min_qty: c.min_qty,
            max_qty: c.max_qty,
        })
        .collect();

    let mut rev_share_groups = Vec::with_capacity(request.rev_share.len());
    for group in &request.rev_share {
        // Absent means the platform, which is the column's default and D-07's
        // rule: an unnominated state cannot exist.
        let absorber = match group.residual_absorber_party.as_deref() {
            None => Absorber::Platform,
            Some(token) => Absorber::parse(token).ok_or_else(|| {
                DomainError::InvalidRequest(format!(
                    "residual_absorber_party `{token}` is neither a party nor the platform sentinel"
                ))
            })?,
        };
        let mut parties = Vec::with_capacity(group.parties.len());
        for party in &group.parties {
            let named = Party::new(&party.party).ok_or_else(|| {
                DomainError::InvalidRequest(format!(
                    "party `{}` is blank or spells the reserved `platform` sentinel",
                    party.party
                ))
            })?;
            parties.push(PartyShare {
                party: named,
                share_bp: party.share_bp,
            });
        }
        rev_share_groups.push(RevShareGroup {
            vendor_sku_id: group.vendor_sku_id,
            platform_cut_bp: group.platform_cut_bp,
            residual_absorber: absorber,
            parties,
        });
    }
    Ok(CompositionDraft {
        components,
        rev_share_groups,
    })
}

fn markets_of(request: &PublishBundleRequest) -> Result<Vec<(CurrencyCode, Region)>, DomainError> {
    request
        .markets
        .iter()
        .map(|m| Ok((CurrencyCode::new(&m.currency)?, Region::new(&m.region)?)))
        .collect()
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

async fn create_bundle(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    // Authoring is `bundle x write`; publish is the route below and is D-11's.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::BUNDLE,
        crate::authz::actions::WRITE,
        Some(crate::authz::OwnerTenant(tenant)),
        None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Parsed after the gate, as `plans.rs` and `prices.rs` order it.
    let body: CreateBundleRequest = preconditions::parse_body(&body)?;
    // **Used, and Z12-7 is why it had to become so.** The key was taken and
    // discarded — required of every caller and read by nothing — so a retry ran the
    // create a second time and was answered by whatever the store made of it: on
    // this route `uq_pricing_bundle_plan` caught the insert, so a client that
    // retried on a timeout was told `BUNDLE_EXISTS_ON_PLAN`, with no bundle id and
    // no way to tell its own first attempt from another operator's bundle. That is
    // the inversion at-most-once exists to prevent, arriving as a plausible-looking
    // conflict rather than as a fault.
    let client_key = preconditions::idempotency_key(&headers)?;
    let request_hash = preconditions::request_digest(&body)?;

    let basis = match body.price_basis.as_deref() {
        None => check_basis_declared(None).map_err(|code| {
            DomainError::InvalidRequest(format!("{code}: a bundle must declare its price basis"))
        })?,
        Some(token) => PriceBasis::parse(token).ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "price_basis `{token}` is neither sum_of_parts nor own_price"
            ))
        })?,
    };
    let itemization = match body.invoice_itemization.as_deref() {
        None => InvoiceItemization::Aggregate,
        Some(token) => InvoiceItemization::parse(token).ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "invoiceItemization `{token}` is neither aggregate nor itemize"
            ))
        })?,
    };

    let plan_id = PlanId::new(body.plan_id);
    let wire_plan_id = body.plan_id;
    let now = OffsetDateTime::now_utc();
    let stamp = audit_stamp(&ctx, now, correlation);
    let mutation_scope = scope.clone();

    let outcome = idempotent::guarded(
        &state.db,
        &state.idempotency,
        &scope,
        GuardedRequest {
            operation: CREATE_BUNDLE_OPERATION,
            client_key,
            request_hash,
            tenant_id: tenant,
            status: StatusCode::CREATED.as_u16().into(),
            now,
        },
        move |txn: &DbTx<'_>| -> TxFuture<'_, BundleView> {
            Box::pin(async move {
                // Minted **inside** the guarded body, `plans::create_plan`'s and
                // `windows::schedule_window`'s rule: a replay does not reach this
                // closure at all, so an id minted above it would be a second one
                // nobody is ever told about.
                let bundle_id = Uuid::now_v7();
                // `create_on` rather than `BundleRepo::create`: the claim and the
                // insert have to be one transaction, which is the whole of
                // `guarded`'s guarantee — a create that failed rolls its claim back
                // with it, so the retry claims afresh instead of being told
                // "already done" forever.
                bundle_repo::create_on(
                    txn,
                    &mutation_scope,
                    NewBundle {
                        bundle_id,
                        tenant_id: tenant,
                        plan_id,
                        price_basis: basis,
                        invoice_itemization: itemization,
                    },
                    stamp,
                )
                .await
                .map(|bundle_id| BundleView {
                    bundle_id,
                    plan_id: wire_plan_id,
                    price_basis: basis.as_str().to_owned(),
                    invoice_itemization: itemization.as_str().to_owned(),
                })
                .map_err(|e| crate::infra::storage::repo_failure(&e))
            })
        },
        |view: &BundleView| {
            serde_json::to_value(view).map_err(|e| {
                DomainError::Internal(format!("cannot render the created bundle: {e}"))
            })
        },
    )
    .await
    .map_err(CanonicalError::from)?;

    Ok(match outcome {
        Guarded::Performed(view) => created_bundle(view.bundle_id, view)?,
        Guarded::Replayed { status, body } => {
            replayed_bundle(CREATE_BUNDLE_OPERATION, status, &body)?
        }
    })
}

/// The `201` a performed create answers, with the `Location` §3 requires.
fn created_bundle(bundle_id: Uuid, view: BundleView) -> Result<Response, CanonicalError> {
    let mut response = (StatusCode::CREATED, Json(view)).into_response();
    response.headers_mut().insert(
        LOCATION,
        format!("{BUNDLES}/{bundle_id}")
            .parse()
            .map_err(|_| DomainError::InvalidRequest("unrenderable location".to_owned()))?,
    );
    Ok(response)
}

/// The stored answer, replayed.
///
/// `plans::replayed`'s shape and its reasoning: the status and body are what the
/// first caller was told, and the `Location` is rebuilt from the id **that body
/// carries** rather than from anything this request computed. No `ETag` — the dedup
/// row stores a status and a body and no headers, and a bundle's identity row
/// carries no version of its own in any case.
///
/// # Errors
/// [`DomainError::Internal`] when the stored status is not one — see
/// [`super::replayed_status`].
fn replayed_bundle(
    operation: &str,
    status: i32,
    body: &serde_json::Value,
) -> Result<Response, DomainError> {
    let status = super::replayed_status(operation, status)?;
    let location = body
        .get("bundle_id")
        .and_then(serde_json::Value::as_str)
        .map(|bundle_id| format!("{BUNDLES}/{bundle_id}"));
    Ok(match location {
        Some(location) => (status, [(LOCATION, location)], Json(body.clone())).into_response(),
        None => (status, Json(body.clone())).into_response(),
    })
}

async fn replace_composition(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(bundle_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::BUNDLE,
        crate::authz::actions::WRITE,
        Some(crate::authz::OwnerTenant(tenant)),
        Some(crate::authz::ResourceRef(bundle_id)),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let body: CompositionRequest = preconditions::parse_body(&body)?;
    // The **plan revision's** tag, not one of the composition's own: see the
    // module doc.
    let tag = preconditions::if_match_revision(&headers)?;
    let draft = draft_of(&body)?;

    let plan_id = state
        .bundles
        .plan_of(&scope, tenant, bundle_id)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "bundle".to_owned(),
            id: bundle_id.to_string(),
        })?;

    let stamp = audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    let revision = state
        .bundles
        .replace_composition(
            &scope,
            tenant,
            plan_id,
            tag.revision,
            tag.version,
            draft,
            stamp,
        )
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?;

    let mut response = Json(CompositionAcceptedView {
        bundle_id,
        plan_revision: revision.revision,
    })
    .into_response();
    response.headers_mut().insert(
        ETAG,
        preconditions::revision_etag(revision.revision, revision.row_version)
            .parse()
            .map_err(|_| DomainError::InvalidRequest("unrenderable entity tag".to_owned()))?,
    );
    Ok(response)
}

async fn publish_bundle(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(bundle_id): Path<Uuid>,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    // **D-11**: `plan x publish` only. Not `bundle x write` as well — under the
    // conjunction the design set first stated, only `CatalogAdmin` could publish a
    // bundle while §1.3 promises `FinanceManager` can.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::PUBLISH,
        Some(crate::authz::OwnerTenant(tenant)),
        // **`None`, and not this object's own id.** The action is on a **plan** —
        // that is what the authz catalog's endpoint map puts it on — and the plan
        // id is not in hand here: it is resolved below, from a row this gate has
        // to authorize before it may be read. Passing the bundle id asked the
        // PDP about an object the `plan` label carries no identity for, so a role
        // definition of the form "allow this action where `resource_id in
        // {{planA}}`" was evaluated against the wrong object and denied — the
        // availability arm of the rule `windows.rs` states at length and
        // restructured three handlers to keep.
        //
        // `None` is the tenant-wide question every batch gate in this gear already
        // asks, and the compiled scope still binds `tenant_id` in SQL, so nothing
        // widens. **What is still owed** is `windows.rs`'s coarse-then-narrow
        // pair — this ask to scope the lookup, then a second anchored ask on the
        // resolved plan — with its `FURTHER_QUESTIONS` row, which is what would
        // make the narrow authority a fact the census asserts rather than an
        // absence.
        /* resource_id */
        None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let body: PublishBundleRequest = preconditions::parse_body(&body)?;
    let markets = markets_of(&body)?;

    let plan_id = state
        .bundles
        .plan_of(&scope, tenant, bundle_id)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "bundle".to_owned(),
            id: bundle_id.to_string(),
        })?;

    // `inst-ba-validate`: the whole rule set, aggregate. A blocking violation
    // blocks the publish — there is no severity below blocking that still
    // publishes (§4.2).
    let report = state
        .bundle_service
        .validate_publish(&scope, tenant, plan_id, body.plan_revision, markets)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?;
    if !report.is_publishable() {
        return Err(DomainError::ValidationFailed(report).into());
    }

    // **D-104, `inst-ba-material`.** A composition change is always material, so a
    // single principal's call stages it and a second principal's approval is what
    // publishes it. A route that committed straight through here — evaluating no
    // verdict, opening no unit, pinning no content, answering a hard-coded
    // `alwaysMaterialTrigger` — would do it on the one surface in this gear where
    // the money being divided belongs to third parties. `composition_change_set` and
    // `rev_share_change_set` are what make `Trigger::BundleComposition` answer
    // `subject_exists_in_this_crate` truthfully; without a caller they leave it
    // claiming a subject nothing can evaluate.
    //
    // **And the verdict has to say which act it was.** D-104 registers two triggers
    // because a rev-share re-split *is* vendor payout and a component swap changes
    // what the customer receives; one token, `alwaysMaterialTrigger`, names neither.
    // That half is settled below.
    //
    // `overlays::submit_overlay` is the precedent, one plane over and the same
    // shape: `priceOverlayMutation` was a mounted surface writing its materiality
    // token as a literal while nothing built its change set, and it was closed the
    // same way.
    let now = OffsetDateTime::now_utc();
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!(
            "bss-pricing: bundle publish subject: {e}"
        )))
    })?;

    // The content this call would freeze, assembled once and used for both arms —
    // it is what the lookup below matches an approval against and what the unit
    // pins.
    //
    // **The plan shape alone is not enough, and the first version of this route
    // assumed it was.** It read "the composition normalizes onto its absorber
    // inside the plan, so the plan shape *is* the composition's content" — true at
    // publish, false at submit, and D-104 exists precisely because a
    // `sum_of_parts` recomposition carries no price-row delta at all. So an
    // approve taken over one component set authorized a different one. The
    // revision's `row_version` is folded in because a composition edit advances it
    // (`PATCH …/bundles/{id}` is taken under the revision's tag), which makes the
    // digest move for every composition edit including the ones that move no row.
    // See `bundle_content_hash`.
    let draft = state
        .plans
        .find_revision(&scope, tenant, plan_id, body.plan_revision)
        .await
        .map_err(|e| CanonicalError::from(crate::infra::storage::repo_failure(&e)))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "plan revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    let shape = crate::infra::publish::assemble(&conn, &scope, tenant, plan_id, now)
        .await
        .map_err(CanonicalError::from)?;
    let pin = crate::domain::approval::content_pin::bundle_content_hash(&shape, draft.row_version);
    let subject_ref =
        crate::infra::approval::bundle_composition_unit_ref(plan_id, body.plan_revision);

    // **Which of D-104's two acts this is** (D-232's remaining half). The request
    // declares neither — `PublishBundleRequest` carries a revision and a market
    // set — so the answer is a diff of the composition being published against the
    // last one that was live, and `BundleService::declared_publish_act` is the one
    // question that resolves both operands. It is asked on **both** arms and
    // answers identically on both, which is the property `load_live_predecessor`'s
    // doc argues: the reviewer must not be shown one act and the publish store
    // another.
    let act = state
        .bundle_service
        .declared_publish_act(&scope, tenant, plan_id, body.plan_revision)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?;

    // One verdict, rendered twice — never built twice. The wire's string and the
    // record's jsonb cannot come from two evaluations.
    let (reason, stored_materiality) =
        crate::api::rest::overlays::rendered_materiality(&bundle_publish_materiality(&act))?;

    // Matched on the **content** and not merely on the subject: an approval whose
    // composition moved after the decision covers content that no longer exists,
    // so answering with it would authorize a component set nobody reviewed.
    let approved = state
        .approvals
        .approved_unit(&scope, tenant, &subject_ref, &pin)
        .await
        .map_err(CanonicalError::from)?;

    if let Some(record) = approved {
        // The publish arm: a second, independent person has seen exactly this
        // composition.
        state
            .bundle_service
            // `draft.row_version` is the version the pin above was computed over
            // and the unit was matched on, handed down so the write can refuse a
            // composition that moved between the two. Not re-read here: this is
            // the surface's own read, and a second one would be a second answer
            // to the question the compare exists to ask.
            .publish_composition(
                &scope,
                tenant,
                plan_id,
                body.plan_revision,
                draft.row_version,
                audit_stamp(&ctx, now, correlation),
            )
            .await
            .map_err(|e| crate::infra::storage::repo_failure(&e))?;

        // 202, per `inst-ba-return`, and on the **event** half of that
        // instruction only: `BundleUpdated` is enqueued in the same transaction
        // as the effective shares, and a consumer reads the composition from it.
        //
        // **The read-model half is unbuilt and this comment used to claim it.**
        // `inst-ba-return` also says "composition frozen into the read model /
        // snapshot", and `publish_composition` records no `PendingVersionRow`:
        // no `CatalogVersion` advances, no subject re-projects, and
        // `PlanSubjectDelta` carries no bundle member to re-project into. So the
        // 202 is asynchrony about the outbox, not about a projector that would
        // catch up later — nothing will. The divergence is reported here and in
        // `domain::sellability`'s `inst-sg-bundle` section, which is the surface
        // it costs: the sellability gate cannot walk a bundle's components
        // because this act never freezes them.
        return Ok((
            StatusCode::ACCEPTED,
            Json(PublishAcceptedView {
                bundle_id,
                plan_revision: body.plan_revision,
                outcome: OUTCOME_PUBLISHED.to_owned(),
                materiality: reason,
                approval: Some(ApprovalView::from(&record)),
            }),
        )
            .into_response());
    }

    // The submit arm: open D-104's always-material unit over this composition.
    let opened = state
        .approvals
        .submit_bundle_publish(
            &scope,
            tenant,
            plan_id,
            body.plan_revision,
            Uuid::now_v7(),
            pin.to_vec(),
            stored_materiality,
            audit_stamp(&ctx, now, correlation),
        )
        .await
        .map_err(CanonicalError::from)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(PublishAcceptedView {
            bundle_id,
            plan_revision: body.plan_revision,
            outcome: OUTCOME_SUBMITTED.to_owned(),
            materiality: reason,
            approval: Some(ApprovalView::from(&opened)),
        }),
    )
        .into_response())
}

/// The materiality verdict a composition publish carries — **evaluated, not
/// asserted**.
///
/// `overlays::overlay_submit_materiality`'s shape and its reason: the token an
/// operator reads is produced by the same evaluator every other unit's is, so two
/// units compared by a reader are two answers from one function rather than one
/// answer and one literal.
///
/// It passes no policy and no baseline, and that is not a shortcut —
/// [`materiality::evaluate`] examines the **act** half before it consults either,
/// so a configured threshold cannot reach this act. That is the whole of what
/// D-104 decided: with a threshold configured, the evaluator saw no price-row
/// delta to trip on and a component swap reached consumers with no approver,
/// while a $1 price-row change above threshold took two people.
///
/// # This call is what makes both of D-104's triggers real
///
/// The act half is `ChangeSet::act()`, reachable through nothing but
/// [`ChangeSet::of_act`], so a trigger no surface constructs can never be answered
/// by the evaluator however many tables its subject has.
/// `infra::bundle::composition_change_set` was exactly such a constructor — a
/// `pub fn` building a declaration, and this is its caller.
///
/// **It takes the act rather than choosing it**. Choosing it here
/// would put the composition diff on the route beside the authz gate and the
/// validation pass; `infra::bundle::declared_act` is where the rule lives and
/// `BundleService::declared_publish_act` is where its two operands are read, so
/// what remains here is the evaluation — which is the same division `infra::window`
/// takes for a shortening (D-176: the surface classifies inside its own
/// transaction, the evaluator turns the classification into a verdict).
fn bundle_publish_materiality(act: &ChangeSet) -> MaterialityVerdict {
    materiality::evaluate(act, /* policy */ None, /* baseline */ None)
}

/// `GET /bundles`.
///
/// Gated `bundle × read`, the pair [`read_bundle`] already asks for, with
/// `resource_id: None` because there is no single resource to name — what the PDP
/// compiles is then the tenant filter the whole walk runs under, and
/// `require_constraints` is `true` so an unconstrained allow fail-closes rather
/// than paging through every tenant's catalogue.
///
/// It answers [`BundleView`] and not [`BundleCompositionView`], which is the same
/// split `list_approvals` and `get_approval` make: a composition is three further
/// queries per bundle, so a page of a hundred would be three hundred round trips
/// to show a basis and an invoice layout.
#[allow(clippy::implicit_hasher)]
async fn list_bundles(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<BundleView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::BUNDLE,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let page = state
        .bundles
        .list_odata(&scope, tenant, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page
            .items
            .iter()
            .map(|record| BundleView {
                bundle_id: record.bundle_id,
                plan_id: record.plan_id.get(),
                price_basis: record.price_basis.as_str().to_owned(),
                invoice_itemization: record.invoice_itemization.as_str().to_owned(),
            })
            .collect(),
        page_info: page.page_info,
    }))
}

/// `GET /bss-pricing/v1/bundles/{bundleId}` — the bundle and its composition.
///
/// **The read D-310 adds, and the gap it closes.** §5's endpoint map had three
/// rows and none of them a `GET`, so a composition was reachable through no
/// surface in the gear: not by its author, not by an operator, and — once D-104's
/// always-material unit existed — not by the approver deciding it. The approval
/// surface was corrected first, because that is where the money decision is made;
/// this is the authoring side, so the composition has a reader that does not
/// require an open unit.
///
/// Gated `bundle × read`, which `FinanceReviewer` already holds — D-104 relies on
/// that grant rather than asking for a new one.
///
/// The revision defaults to the plan's open draft, which is what an author editing
/// a composition means by "the composition"; `?plan_revision=` names an older one.
/// No `If-Match` and no idempotency key: this is a read, and the composition's
/// concurrency story is the plan revision's entity tag, which belongs to the write.
async fn read_bundle(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(bundle_id): Path<Uuid>,
    Query(query): Query<ReadBundleQuery>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::BUNDLE,
        crate::authz::actions::READ,
        // **`None`, because this is a read** — `authz::access_scope`'s stated
        // two-way split: reads let the PDP derive the scope from the subject and
        // its role, never from a caller-supplied tenant, and only a write passes
        // `Some(target_tenant)` so the membership assertion has a target to test.
        // A read passing `Some(tenant)` runs that write-only assertion on a read.
        // Nothing escalates — the value is `ctx.subject_tenant_id()` and never
        // caller-supplied — but it is a live divergence between a module's stated
        // contract and its callers, and the contract is what a later reader trusts.
        /* owner_tenant_id */
        None,
        Some(crate::authz::ResourceRef(bundle_id)),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let plan_id = state
        .bundles
        .plan_of(&scope, tenant, bundle_id)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "bundle".to_owned(),
            id: bundle_id.to_string(),
        })?;
    let bundle = state
        .bundles
        .find_by_plan(&scope, tenant, plan_id)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "bundle".to_owned(),
            id: bundle_id.to_string(),
        })?;

    // The plan's open draft is what an author means by "the composition"; a
    // caller after an older one names it.
    // The open draft first, then the current revision: an author editing a
    // composition means the draft, and a plan with none has only its published
    // revision to show. Absent both, the plan has no revision at all.
    let asked_revision = query
        .plan_revision
        .as_deref()
        .map(|raw| {
            raw.trim().parse::<u64>().map_err(|_| {
                DomainError::InvalidRequest(format!(
                    "plan_revision: `{raw}` is not a revision number"
                ))
            })
        })
        .transpose()?;
    let revision = if let Some(revision) = asked_revision {
        revision
    } else {
        let draft = state
            .plans
            .find_open_draft(&scope, tenant, plan_id)
            .await
            .map_err(|e| crate::infra::storage::repo_failure(&e))?;
        let resolved = if let Some(row) = draft {
            Some(row)
        } else {
            state
                .plans
                .find_current(&scope, tenant, plan_id)
                .await
                .map_err(|e| crate::infra::storage::repo_failure(&e))?
        };
        resolved
            .ok_or_else(|| DomainError::NotFound {
                subject: "plan revision".to_owned(),
                id: plan_id.to_string(),
            })?
            .revision
    };
    let composition = state
        .bundles
        .load_composition(&scope, tenant, plan_id, revision)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?;

    Ok((
        StatusCode::OK,
        Json(BundleCompositionView {
            bundle_id,
            plan_id: plan_id.get(),
            plan_revision: revision,
            price_basis: bundle.price_basis.as_str().to_owned(),
            invoice_itemization: bundle.invoice_itemization.as_str().to_owned(),
            components: composition
                .components
                .iter()
                .map(|c| ComponentRequest {
                    component_plan_id: c.component_plan_id,
                    included_sku_id: c.included_sku_id,
                    min_qty: c.min_qty,
                    max_qty: c.max_qty,
                })
                .collect(),
            rev_share: composition
                .rev_share_groups
                .iter()
                .map(|g| RevShareGroupRequest {
                    vendor_sku_id: g.vendor_sku_id,
                    platform_cut_bp: g.platform_cut_bp,
                    residual_absorber_party: Some(g.residual_absorber.as_str().to_owned()),
                    parties: g
                        .parties
                        .iter()
                        .map(|p| PartyShareRequest {
                            party: p.party.get().to_owned(),
                            share_bp: p.share_bp,
                        })
                        .collect(),
                })
                .collect(),
        }),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// The router.
// ---------------------------------------------------------------------------

/// Mount the bundle routes.
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    // The path is spelled as a **literal** here and as [`BUNDLES`] above, unlike
    // its three neighbours which pass the `const`: DE0801 validates a literal
    // argument and silently passes a `const` one, so the route-shape rule only
    // binds where the literal is. `module_test`'s census is what pins the two
    // spellings together.
    let router = OperationBuilder::get("/bss-pricing/v1/bundles")
        .operation_id("bss_pricing.list_bundles")
        .summary("List the tenant's bundles (cursor-paginated)")
        .description(
            "One page of the tenant's bundles in `bundle_id` order, with an opaque `cursor` and a \
             `limit` whose server default is 100 and whose hard cap is 1,000 (D-125). Narrow \
             with `$filter=plan_id eq <uuid>` to the bundle riding one plan, which a plan \
             carries at most one of. There is no `lifecycle_state` filter: a bundle row is an \
             identity and its composition is revision-scoped, so a bundle's state is its plan \
             revision's state and belongs to the plan surface. The composition is **not** on \
             this page - a bundle's components and revenue shares are three further queries \
             each - so a caller opens `GET /bss-pricing/v1/bundles/{bundleId}` for one bundle's \
             composition. Gates on `bundle` x `read`, which is the pair the by-id read already \
             asks for.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Bundles per page (default 100, hard cap 1,000)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_bundles)
        .with_odata_filter::<BundleFilterField>()
        .with_odata_orderby::<BundleFilterField>()
        .json_response_with_schema::<Page<BundleView>>(
            openapi,
            StatusCode::OK,
            "One page of the tenant's bundles.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::post(BUNDLES)
        .operation_id("bss_pricing.create_bundle")
        .summary("Create a bundle on its plan")
        .description(
            "Declares a `bundle`-type plan's price basis (`sum_of_parts` or `own_price`) and its \
             invoice itemization. The composition itself is authored through `PATCH \
             /bss-pricing/v1/bundles/{bundleId}`, which is where it becomes revision-scoped. A \
             plan carries at most one bundle. An absent `price_basis` is refused `BASIS_MISSING` \
             (`inst-bb-declared`). A `plan_id` naming no plan the caller can read is `404` - a \
             plan in another tenant answers exactly as an absent one does, so the refusal \
             confirms nothing about a catalog the caller cannot see. **`Idempotency-Key` is \
             required and is honoured**: a retry under the same key replays the first call's \
             `201` and its bundle id rather than creating or refusing anything, and the same key \
             carrying a different request is `409` `IDEMPOTENCY_PAYLOAD_MISMATCH`. Gates on \
             `bundle` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::idempotency_key_param())
        .json_request::<CreateBundleRequest>(
            openapi,
            "The bundle's identity and its initial composition.",
        )
        .handler(create_bundle)
        .json_response_with_schema::<BundleView>(
            openapi,
            StatusCode::CREATED,
            "The bundle as created.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        // Declared because `bundle_repo::create_on` resolves the body's `plan_id`
        // in the caller's scope and answers `NotFound` when it does not resolve.
        // This is the only route in the gear that takes a plan id from a **body**
        // and creates something out of it, `POST /plans/{planId}/clone` being the
        // path-parameter one; both answer 404 and both declare it.
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(BUNDLE_BY_ID)
        .operation_id("bss_pricing.read_bundle")
        .summary("Read a bundle and its composition")
        .description(
            "Answers the bundle's declaration - its `price_basis` and \
             `invoice_itemization` - together with the component set and the rev-share \
             groups at a revision. `plan_revision` names one; absent, it is the plan's \
             open draft, or its current revision when there is no draft. \
             \
             The composition was readable through no surface until D-310, which made it \
             unreadable to the approver of the always-material unit D-104 opens over it. \
             Declares no precondition and no idempotency key: this is a read, and the \
             composition's concurrency story is the plan revision's entity tag, which \
             belongs to the write. Gates on `bundle` x `read`.",
        )
        .tag(TAG)
        .path_param("bundleId", "The bundle to read.")
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "plan_revision",
            false,
            "The plan revision whose composition is answered. Absent resolves to the plan's open \
             draft, or to its current revision when no draft is open - so a caller who omits it \
             reads the composition an author is editing, not the one a subscriber is billed on. \
             Declared because the handler reads it: a parameter the description narrates and the \
             document does not name is one no generated client can send, and this read would then \
             answer one revision only (Z13-10's class).",
            "integer",
        )
        .handler(read_bundle)
        .json_response_with_schema::<BundleCompositionView>(
            openapi,
            StatusCode::OK,
            "The bundle and its composition at the resolved revision.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(BUNDLE_BY_ID)
        .operation_id("bss_pricing.replace_bundle_composition")
        .summary("Replace an open draft revision's whole composition")
        .description(
            "Replaces the component set and the rev-share groups **wholesale** - every Slice-8 \
             rule quantifies over the set, so a partial update leaves nothing the validator can \
             evaluate. `If-Match` carries the **plan revision's** entity tag, because the \
             composition rides it: an edit that left the revision's tag alone would let two \
             authors edit one draft and both satisfy their precondition. Only an open draft \
             revision's composition is mutable (D-92). Gates on `bundle` x `write`.",
        )
        .tag(TAG)
        .path_param("bundleId", "The bundle whose composition is replaced.")
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::if_match_param(
            "The tag is the plan revision's, read off `GET /bundles/{bundleId}`.",
        ))
        .json_request::<CompositionRequest>(openapi, "The replacement composition, whole.")
        .handler(replace_composition)
        .json_response_with_schema::<CompositionAcceptedView>(
            openapi,
            StatusCode::OK,
            "The composition was replaced; the entity tag names the revision it now stands at.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::post(BUNDLE_PUBLISH)
        .operation_id("bss_pricing.publish_bundle")
        .summary("Validate and publish a bundle composition")
        .description(
            "Runs the whole composition rule set - basis, component publication, per-market \
             coverage, frequency match, one tax display basis per market (D-119) and the \
             rev-share reconciliation (D-07) - and reports **every** failure in one pass. A \
             single blocking violation blocks the publish. On success the group residuals are \
             normalized onto their absorbers so published effective shares sum to exactly 10000 \
             bp, and `BundleUpdated` is emitted in the same transaction. Composition changes are \
             **always material** (D-104) whatever a threshold policy says. Requires `plan` x \
             `publish` **only** (D-11): the composition was authored under `bundle` x `write` \
             and component checks are validations, not caller authz.",
        )
        .tag(TAG)
        .path_param("bundleId", "The bundle to publish.")
        .authenticated()
        .no_license_required()
        .json_request::<PublishBundleRequest>(
            openapi,
            "The publish request - the composition revision it is composed against.",
        )
        .handler(publish_bundle)
        .json_response_with_schema::<PublishAcceptedView>(
            openapi,
            StatusCode::ACCEPTED,
            "Accepted; the effective rev-share split is committed and `BundleUpdated` is \
             enqueued. The composition is **not** frozen into the read model: a composition \
             publish advances no `CatalogVersion`, so a pinned read does not carry it.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        // C7-1's refusal, declared. `publish_composition` re-reads the revision
        // inside its writing transaction and answers `RepoError::StaleRowVersion`
        // when the composition moved after the reviewer's pin was taken over it;
        // that maps through `DomainError::StaleVersion` to a **409**
        // (`infra::error_mapping_tests` pins the status). It is the central
        // outcome of that fix rather than an edge, so a generated client meeting
        // it must have it modelled — and the two siblings on this same struct,
        // `POST /bundles` and `PATCH /bundles/{bundleId}`, both declare theirs.
        // The remedy differs from a retry, which is why the status has to reach
        // the client at all: the operator re-reviews the composition that moved.
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
        .layer(Extension(state))
        // D-178's edge, carried with the routes rather than at the merge, so a
        // surface reachable without it cannot build an `AuditStamp`.
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}
