//! The `PriceOverlay` authoring surface — `design/09-price-overlays.md` §5,
//! `inst-pl-author` / `inst-pl-validate` / `inst-pl-return` / `inst-pl-commit`.
//!
//! Four routes: author or edit an overlay draft, read one by id, submit it, and
//! list them.
//!
//! # A save lands a **draft only**, and nothing publishes from it
//!
//! `inst-pl-return` is explicit:
//! the `POST`/`PATCH` answers 201/200 and the overlay is a `draft`. Publishing
//! is the submit's, and the submit is **always material** (D-50) — overlay
//! creation, line add or remove, magnitude, kind, audience, precedence, dating
//! and disclosure changes all route through the Slice 5 approval workflow before
//! anything publishes. That is why this module's `POST` never touches the
//! lifecycle and why the submit answers **202**.
//!
//! # `PATCH` addresses a resource, so it is not the collection `POST`
//!
//! §5 spells both halves as `POST/PATCH /bss-pricing/v1/price-overlays` — one
//! path, two methods, the subject in the body. This mounts the `PATCH` as
//! `/bss-pricing/v1/price-overlays/{overlayId}` instead, for the reason Slice 8
//! reported for its own composition route (owed entry B-10): a precondition
//! addresses a resource, and a collection that answers `If-Match` is a
//! collection pretending to be one. The divergence is in the owed register
//! rather than smoothed over.
//!
//! # What `If-Match` carries here, and why it is not the plan's
//!
//! An overlay has **no plan**: D-92 gives it a revision chain of its own, so the
//! entity tag is the overlay revision's `"<revision>-<version>"` (D-170's shape)
//! and not a plan revision's. `BundleRepo`'s composition borrows the plan's tag
//! because a bundle rides a plan; nothing here does.
//!
//! # 422 does not exist on this platform
//!
//! §5 types seven of the nine overlay codes 422, and that notation is
//! architectural: `CanonicalError` renders `InvalidArgument`,
//! `FailedPrecondition` and `OutOfRange` all as **400**, and the **code string**
//! is the discriminator a consumer matches on. Those seven travel inside the
//! `ValidationFailed` envelope, one violation per failing rule, which is what
//! makes an overlay remediable in one pass.
//!
//! **The other two do not, and that is §5's own typing rather than a choice
//! here.** `PRECEDENCE_DUPLICATE` and `OVERLAY_INTERVAL_OVERLAP` are typed
//! **409**: they are conflicts on state a sibling overlay holds, which a caller
//! resolves by re-reading rather than by editing their own request. So
//! [`conflict_of`](crate::domain::overlay_rules::conflict_of) lifts either of
//! them out of the report and answers the typed conflict, and everything else
//! goes in the envelope. No route here declares a 422.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query};
use axum::http::header::{ETAG, LOCATION};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::HeaderMap, http::StatusCode};
use bss_pricing_sdk::odata::OverlayFilterField;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::DbTx;
use toolkit_odata::PageInfo;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::{ApprovalView, MaterialityView};
use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::preconditions;
use crate::api::rest::state::{AuthoringState, GovernanceState};
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{self, ChangeSet, MaterialityReason, MaterialityVerdict};
use crate::domain::overlay::{
    Disclosure, LineKey, OverlayInterval, OverlayLifecycle, OverlayLine, ScopeClass, ScopeSelector,
    ScopeValue, TargetRef, TargetSku, TaxBasis,
};
use crate::domain::overlay_rules::{
    OverlayCandidate, check_authored_shape, check_tax_basis_declared, conflict_of, validate,
};
use crate::domain::scope_key::PlanId;
use crate::infra::idempotent::{self, Guarded, GuardedRequest, TxFuture};
use crate::infra::storage::repo::{NewOverlay, OverlayRecord, overlay_repo};
use time::OffsetDateTime;

const TAG: &str = "BSS Pricing Overlays";

/// The at-most-once operation the overlay create claims under (§9).
///
/// Per-route, `plans.rs`' rule: the key is scoped to the operation, so one client
/// key used on two different verbs does not collide.
const CREATE_OVERLAY_OPERATION: &str = "bss_pricing.create_price_overlay";

/// The wire token for the submit arm — `api::rest::publish`'s, so a client's
/// `match` does not depend on which plane it called.
const OUTCOME_SUBMITTED: &str = crate::api::rest::publish::OUTCOME_SUBMITTED;

/// The wire token for the publish arm — `publish.rs`'s, like the line above it.
///
/// Re-spelled `"published"` here, three lines below a sibling that imports its
/// token, renaming `publish::OUTCOME_PUBLISHED` leaves
/// `PATCH /price-overlays/{overlayId}`'s commit arm answering the old word with
/// every suite green. `rest_authz`'s scan cannot catch it — it excludes `"published"`
/// deliberately, because the word is also `LifecycleState::Published` in a column
/// and a text scan would drag the lifecycle renderers in — but that is an argument
/// about the *scan*, not about re-spelling the const at the one site that did.
const OUTCOME_PUBLISHED: &str = crate::api::rest::publish::OUTCOME_PUBLISHED;

/// The materiality reason an overlay act always carries (D-50), for the arm that
/// reads it back off a **stored** verdict rather than evaluating one.
///
/// Rendered by the enum that owns the token rather than spelled again here: the
/// stored verdict this falls back for was written by
/// [`MaterialityReason::as_str`], so a second spelling is a fallback free to stop
/// agreeing with the value it substitutes for.
const OVERLAY_ACT_REASON: &str = MaterialityReason::AlwaysMaterialTrigger.as_str();

/// `POST` — author an overlay draft; `GET` — list them.
pub const PRICE_OVERLAYS: &str = "/bss-pricing/v1/price-overlays";
/// `GET` — the authoring revision; `PATCH` — replace an open draft's line set.
pub const PRICE_OVERLAY_BY_ID: &str = "/bss-pricing/v1/price-overlays/{overlayId}";
/// `POST` — submit the draft for the always-material approval unit.
pub const PRICE_OVERLAY_SUBMIT: &str = "/bss-pricing/v1/price-overlays/{overlayId}/submit";

// ---------------------------------------------------------------------------
// Wire types. The wire is `snake_case` — `toolkit_macros::api_dto` does not
// rename — so the design set's `orgTier` is `org_tier` here and everywhere.
// ---------------------------------------------------------------------------

/// `POST /bss-pricing/v1/price-overlays`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct CreateOverlayRequest {
    /// `partner | org_tier | brand | region | customer_group | global`.
    pub scope_class: String,
    /// The taxonomy value. **Absent exactly when the class is `global`** — the
    /// pairing is unrepresentable in the domain, so a mismatch is refused here.
    pub scope_value: Option<String>,
    /// L2's explicit precedence, unique within the class among published
    /// overlays.
    pub precedence: i32,
    /// `inclusive | exclusive | delegated_tariffs`. **Optional on the wire and
    /// required in substance**: L5 says the basis MUST be declared and
    /// `TAX_BASIS_UNDECLARED` is what an absent one is told. Modelling it as
    /// `Option` is what makes that code reachable — a required field would be
    /// refused by the deserializer with a message the design set does not own.
    pub tax_basis: Option<String>,
    /// `restricted | public`. Absent means `restricted`, L6's fail-closed
    /// default.
    pub disclosure: Option<String>,
    /// Inclusive start of the overlay's own interval.
    #[serde(default, with = "rfc3339::option")]
    pub effective_from: Option<OffsetDateTime>,
    /// Exclusive end of it.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
    /// The plans the lines may target.
    pub target_plan_ids: Vec<Uuid>,
    /// The adjustment lines, whole. At least one (D-42).
    pub lines: Vec<OverlayLineRequest>,
}

/// `PATCH /bss-pricing/v1/price-overlays/{overlayId}` — the line set, whole.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct ReplaceLinesRequest {
    /// The revision being edited. Must be the open draft.
    pub revision: u64,
    /// The lines, **replaced and never merged**: every D-42 rule quantifies over
    /// the set, so a partial update leaves nothing the validator can evaluate.
    pub lines: Vec<OverlayLineRequest>,
}

/// One authored adjustment line.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct OverlayLineRequest {
    /// The line's identity. Absent mints one; supplying it is how an edit keeps
    /// a line's identity stable across revisions (D-92).
    pub line_id: Option<Uuid>,
    /// `None` is the **list-default line**, which applies to every target.
    pub plan_id: Option<Uuid>,
    /// Optional narrowing. Requires `plan_id`.
    pub target_sku: Option<String>,
    /// The grandfathered generation this line filters to (D-78). Requires
    /// `plan_id`.
    #[serde(default, with = "rfc3339::option")]
    pub cohort: Option<OffsetDateTime>,
    /// `markup | discount | fixed`.
    pub adjustment_kind: String,
    /// `percent_bp | amount`. **Declared, never inferred** (D-08).
    pub magnitude_kind: String,
    /// The basis-points magnitude, on a `percent_bp` line.
    pub adjustment_value: Option<i64>,
    /// The per-currency values, on an `amount` line.
    #[serde(default)]
    pub amounts: Vec<AmountRequest>,
}

/// One currency's value on an amount-based line.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct AmountRequest {
    /// ISO 4217.
    pub currency: String,
    /// The magnitude, in the currency's ISO 4217 minor unit.
    pub value_minor: i64,
}

/// One overlay revision, as the read surface renders it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct OverlayView {
    /// The overlay's identity.
    pub price_overlay_id: Uuid,
    /// Which revision this is — the **first** half of the `If-Match` tag.
    pub revision: u64,
    /// The row version — the **second** half of the `If-Match` tag
    /// `"<revision>-<version>"` a `PATCH` must assert.
    ///
    /// **Without it the edit door was reachable from nowhere.** The tag was
    /// emitted at exactly two places, the create's `201` and the `PATCH`'s own
    /// `200`; the create's *replay* deliberately omits it and this list emits no
    /// `ETag`. `GET …/price-overlays/{id}` is the reload: it answers the
    /// authoring revision and the tag a subsequent `PATCH` must present. A
    /// caller that lost the one-shot tag re-reads that path rather than paging
    /// the tenant's overlay set.
    ///
    /// The three sibling lists carry their token on the page for this reason and
    /// say so — `windows.rs`'s `mutation_seq` (*"a caller that lists and then
    /// adjusts needs the precondition from the page"*), `prices.rs`'s and
    /// `plans.rs`'s `row_version`. This list carried exactly half of its own tag.
    ///
    /// Distinct from [`crate::domain::overlay::OverlayRevision`]'s deliberate
    /// omission of it, and that distinction is the point: a **pin** must not carry
    /// a concurrency token, because it answers "is this the content the reviewer
    /// saw" and the token moves for reasons the content did not. A **view** is not
    /// a pin — it is what a client reads before it writes.
    pub row_version: u64,
    /// `draft | published | superseded`.
    pub lifecycle_state: String,
    /// Its scope class.
    pub scope_class: String,
    /// Its scope value, absent for `global`.
    pub scope_value: Option<String>,
    /// Its precedence.
    pub precedence: i32,
    /// Its declared basis.
    pub tax_basis: String,
    /// Its exposure flag.
    pub disclosure: String,
    /// Inclusive start of its own interval.
    #[serde(default, with = "rfc3339::option")]
    pub effective_from: Option<OffsetDateTime>,
    /// Exclusive end of it.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
    /// The plans its lines may target.
    pub target_plan_ids: Vec<Uuid>,
    /// Its lines.
    pub lines: Vec<OverlayLineView>,
}

/// One line, as the read surface renders it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct OverlayLineView {
    /// The line's identity, stable across revisions that do not change it.
    pub line_id: Uuid,
    /// Its target plan, absent on the list-default line.
    pub plan_id: Option<Uuid>,
    /// Its SKU narrowing.
    pub target_sku: Option<String>,
    /// The generation it filters to.
    #[serde(default, with = "rfc3339::option")]
    pub cohort: Option<OffsetDateTime>,
    /// `markup | discount | fixed`.
    pub adjustment_kind: String,
    /// `percent_bp | amount`.
    pub magnitude_kind: String,
    /// The bp magnitude, on a percent line.
    pub adjustment_value: Option<i64>,
    /// The per-currency values, on an amount line.
    pub amounts: Vec<AmountRequest>,
}

/// What a list read answers with.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct OverlayListView {
    /// The overlays, ordered by id then revision -- the cursor's own key order.
    ///
    /// **Not precedence order.** See the repository's `list`: a keyset walk has
    /// to be ordered by the key its cursor names, and D-125's cursor is a single
    /// id. Each row still carries its `precedence`, so a caller assembling a
    /// stack reads it from the row rather than from the sequence.
    pub overlays: Vec<OverlayView>,
    /// D-125's page block: the limit in force and the token for the next page,
    /// `null` once the walk is exhausted.
    pub page_info: PageInfo,
}

/// What an edit answers with.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct OverlayAcceptedView {
    /// The overlay that was written.
    pub price_overlay_id: Uuid,
    /// The revision it now stands at.
    pub revision: u64,
}

/// What a submit answers with.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct SubmitAcceptedView {
    /// The overlay submitted.
    pub price_overlay_id: Uuid,
    /// The revision submitted.
    pub revision: u64,
    /// What the call did: `submitted_for_approval` (202) | `published` (200).
    ///
    /// One response type across both statuses rather than two schemas on one
    /// operation — `PublishOutcomeView`'s arrangement one plane over, and its
    /// reason: a generated client has one thing to deserialize and reads this
    /// field to know which arm it got.
    pub outcome: String,
    /// The registry's **pending** handle, on the publish arm only.
    ///
    /// Not a `CatalogVersion`, and it must not be pinned as one: the commit
    /// requests an assignment and `CatalogVersionPublished` resolves it, so a
    /// consumer treating this as a version would be resolving against an
    /// addressability that does not exist yet.
    pub pending_version_ref: Option<String>,
    /// Why this submit is material. **Always** `alwaysMaterialTrigger` (D-50):
    /// an overlay line has no per-currency baseline to threshold, so the G1
    /// no-delta rule applies and no threshold can make it immaterial. Stated in
    /// the response because an operator who expected auto-publish under a
    /// configured threshold needs the reason, not only the outcome.
    ///
    /// **Evaluated, not asserted** — see [`overlay_submit_materiality`]. Holding the
    /// token as a literal here is a second spelling of `MaterialityReason::as_str`'s,
    /// in a crate whose rule is that a token has one home: the two agree, and nothing
    /// but the coincidence keeps them agreeing.
    pub materiality: String,
    /// The Slice 5 approval unit this submit opened (D-50, D-225).
    ///
    /// **`Option` because the wire shape must survive the arm that does not open
    /// one**, and there is exactly one: a submit refused before the unit is opened
    /// answers an error rather than this view. It is `Some` on every success today,
    /// and it is optional rather than required so that a later arm — an already-
    /// approved revision re-submitted, say — has somewhere to say "no new unit"
    /// without changing the field's type under a consumer.
    ///
    /// The 202 promises a two-person workflow, so this route has to open the unit
    /// (D-225) — a route that opens nothing and says so nowhere promises one that
    /// does not run.
    pub approval: Option<ApprovalView>,
    /// The advisory findings the pipeline raised. Warnings never block, and this
    /// is the channel that makes them advisory — the `ValidationFailed` envelope
    /// exists only on the rejecting path, so a warning carried only there would
    /// be computed and discarded (the defect D-197 records for the plan plane).
    pub warnings: Vec<AdvisoryView>,
}

/// One advisory finding.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct AdvisoryView {
    /// The machine-readable code — `FIXED_LINE_DISCARDS_STACK`, `TARGET_RETIRED`.
    pub code: String,
    /// What it is about.
    pub subject: String,
    /// What the author is told.
    pub detail: String,
}

// ---------------------------------------------------------------------------
// Conversions.
// ---------------------------------------------------------------------------

/// The scope selector, refusing every pairing the domain makes unrepresentable.
fn scope_of(class: &str, value: Option<&str>) -> Result<ScopeSelector, DomainError> {
    let class = ScopeClass::parse(class).ok_or_else(|| {
        DomainError::InvalidRequest(format!(
            "scope_class `{class}` is not one of partner, org_tier, brand, region, \
             customer_group, global"
        ))
    })?;
    match (class, value) {
        (ScopeClass::Global, None) => Ok(ScopeSelector::Global),
        (ScopeClass::Global, Some(_)) => Err(DomainError::InvalidRequest(
            "the global scope class carries no scope_value: it selects every payer of the \
             tenant, so there is no value for a taxonomy to declare"
                .to_owned(),
        )),
        (_, None) => Err(DomainError::InvalidRequest(format!(
            "a {class}-scoped overlay must name a scope_value; only the global class has none"
        ))),
        (_, Some(raw)) => {
            let value = ScopeValue::new(raw).ok_or_else(|| {
                DomainError::InvalidRequest(
                    "scope_value may not be blank: the empty string is the store's sentinel \
                     for the classless scope"
                        .to_owned(),
                )
            })?;
            ScopeSelector::scoped(class, value).ok_or_else(|| {
                DomainError::Internal(
                    "scope pairing refused after its class was checked".to_owned(),
                )
            })
        }
    }
}

/// One authored line, with every pairing §6 spends a `CHECK` on refused here by
/// construction.
fn line_of(request: &OverlayLineRequest) -> Result<OverlayLine, DomainError> {
    let key = match (request.plan_id, request.target_sku.as_deref()) {
        (None, None) => LineKey::list_default(),
        (None, Some(_)) => {
            return Err(DomainError::InvalidRequest(
                "a target_sku line must name its plan_id: a bare SKU is ambiguous per \
                 (currency, region)"
                    .to_owned(),
            ));
        }
        (Some(plan), None) => LineKey::for_plan(PlanId::new(plan)),
        (Some(plan), Some(raw)) => {
            let sku = TargetSku::new(raw).ok_or_else(|| {
                DomainError::InvalidRequest("target_sku may not be blank".to_owned())
            })?;
            LineKey::for_sku(PlanId::new(plan), sku)
        }
    };
    let key = match request.cohort {
        None => key,
        Some(cohort) => key.for_cohort(cohort).ok_or_else(|| {
            DomainError::InvalidRequest(
                "a cohort line must name its plan_id: the cohort is validated against the \
                 line's target plan, which the list-default line does not have (D-78)"
                    .to_owned(),
            )
        })?,
    };

    Ok(OverlayLine {
        line_id: request.line_id.unwrap_or_else(Uuid::now_v7),
        key,
        adjustment: crate::domain::overlay::adjustment_of(
            &request.adjustment_kind,
            &request.magnitude_kind,
            request.adjustment_value,
            &amount_pairs(&request.amounts),
        )?,
    })
}

/// Every authored line, with every **world-free** rule checked before the store
/// sees them.
///
/// D-67's magnitude ranges, D-42's line-key uniqueness and §1.7's
/// effective-interval sanity are all decidable from the authored document alone,
/// and each of them is otherwise enforced **only** by a `CHECK` or a unique index
/// — which reaches the caller as a driver error, i.e. a **500**, for a request
/// whose remedy is to correct one field. All three were measured answering 500
/// before they moved here; [`check_authored_shape`] carries the argument.
///
/// The line-key half matters twice over: the store refusing a duplicate at
/// **save** also made `OVERLAY_LINE_DUPLICATE` unreachable at **submit**, so a
/// code §5 declares had no path that could raise it.
fn lines_of(
    interval: OverlayInterval,
    requests: &[OverlayLineRequest],
) -> Result<Vec<OverlayLine>, DomainError> {
    let lines: Vec<OverlayLine> = requests
        .iter()
        .map(line_of)
        .collect::<Result<_, DomainError>>()?;
    let report = check_authored_shape(interval, &lines);
    if report.is_publishable() {
        Ok(lines)
    } else {
        Err(DomainError::ValidationFailed(report))
    }
}

fn view_of(record: &OverlayRecord) -> OverlayView {
    OverlayView {
        price_overlay_id: record.price_overlay_id,
        revision: record.revision,
        // `i64` in the row and `u64` on the wire, as everywhere else in this gear:
        // the column is signed because the store's integer type is, and a negative
        // version is not a state the writer can produce.
        row_version: u64::try_from(record.row_version).unwrap_or_default(),
        lifecycle_state: record.lifecycle_state.as_str().to_owned(),
        scope_class: record.scope.class().as_str().to_owned(),
        scope_value: record.scope.value().map(|v| v.as_str().to_owned()),
        precedence: record.precedence,
        tax_basis: record.tax_basis.as_str().to_owned(),
        disclosure: record.disclosure.as_str().to_owned(),
        effective_from: record.interval.from,
        effective_to: record.interval.to,
        target_plan_ids: record.target_ref.plans.iter().map(|p| p.get()).collect(),
        lines: record.lines.iter().map(line_view_of).collect(),
    }
}

fn line_view_of(line: &OverlayLine) -> OverlayLineView {
    OverlayLineView {
        line_id: line.line_id,
        plan_id: line.key.plan_id().map(PlanId::get),
        target_sku: line.key.target_sku().map(|s| s.as_str().to_owned()),
        cohort: line.key.cohort(),
        adjustment_kind: line.adjustment.kind().as_str().to_owned(),
        magnitude_kind: line.adjustment.magnitude_kind().as_str().to_owned(),
        adjustment_value: line.adjustment.percent_bp(),
        amounts: line.adjustment.amounts().map_or_else(Vec::new, |set| {
            set.iter()
                .map(|(currency, value_minor)| AmountRequest {
                    currency: currency.as_str().to_owned(),
                    value_minor,
                })
                .collect()
        }),
    }
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

async fn create_overlay(
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
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PRICE_OVERLAY,
        crate::authz::actions::WRITE,
        Some(crate::authz::OwnerTenant(tenant)),
        None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Parsed after the gate, as `plans.rs`, `prices.rs` and `bundles.rs` order it.
    let body: CreateOverlayRequest = preconditions::parse_body(&body)?;
    // **Used, and Z12-7's overlay half is why it had to become so.** The key was
    // taken and discarded — required of every caller and read by nothing — and
    // unlike `POST /bundles` no uniqueness index stands behind this create. So a
    // retry was not answered a wrong-but-plausible conflict; it **authored a
    // second draft overlay**, with its own id and its own revision 0, and the
    // caller was told `201` as though that were its first attempt's answer. A
    // route that demands a key gives every caller reason to believe retrying is
    // safe, which is what made discarding it worse than never asking.
    let client_key = preconditions::idempotency_key(&headers)?;
    let request_hash = preconditions::request_digest(&body)?;

    let selector = scope_of(&body.scope_class, body.scope_value.as_deref())?;
    // L5's *silence fails*, on its own entry point: `tax_basis` is NOT NULL in
    // the store and `TaxBasis` is closed, so this is the only place the code is
    // reachable. It is raised as a one-violation report rather than as a generic
    // bad request, so `TAX_BASIS_UNDECLARED` reaches the wire as the
    // discriminator §5 declares rather than buried in a message.
    let tax_basis = match body.tax_basis.as_deref() {
        None => {
            let code = check_tax_basis_declared(None)
                .err()
                .unwrap_or("TAX_BASIS_UNDECLARED");
            let mut report = crate::domain::validation::ValidationReport::default();
            report.violate(
                code,
                "tax_basis",
                "an overlay must declare its tax basis or explicitly delegate it to Tariffs; \
                 silence fails (L5)",
            );
            return Err(DomainError::ValidationFailed(report).into());
        }
        Some(token) => TaxBasis::parse(token).ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "tax_basis `{token}` is none of inclusive, exclusive, delegated_tariffs"
            ))
        })?,
    };
    let disclosure = match body.disclosure.as_deref() {
        None => Disclosure::Restricted,
        Some(token) => Disclosure::parse(token).ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "disclosure `{token}` is neither restricted nor public"
            ))
        })?,
    };
    let interval = OverlayInterval {
        from: body.effective_from,
        to: body.effective_to,
    };
    let lines = lines_of(interval, &body.lines)?;

    let now = OffsetDateTime::now_utc();
    let stamp = audit_stamp(&ctx, now, correlation);
    let target_ref = TargetRef {
        plans: body
            .target_plan_ids
            .iter()
            .map(|p| PlanId::new(*p))
            .collect(),
    };
    let precedence = body.precedence;
    let mutation_scope = scope.clone();

    let outcome = idempotent::guarded(
        &state.db,
        &state.idempotency,
        &scope,
        GuardedRequest {
            operation: CREATE_OVERLAY_OPERATION,
            client_key,
            request_hash,
            tenant_id: tenant,
            status: StatusCode::CREATED.as_u16().into(),
            now,
        },
        move |txn: &DbTx<'_>| -> TxFuture<'_, OverlayAcceptedView> {
            Box::pin(async move {
                // Minted **inside** the guarded body, `plans::create_plan`'s and
                // `bundles::create_bundle`'s rule: a replay does not reach this
                // closure at all, so an id minted above it would be a second
                // overlay id nobody is ever told about.
                let price_overlay_id = Uuid::now_v7();
                // `overlay_repo::create_on` rather than `OverlayRepo::create`: the
                // claim and the three statements have to be one transaction, which
                // is the whole of `guarded`'s guarantee — a create that failed
                // rolls its claim back with it, so the retry claims afresh instead
                // of being told "already done" forever.
                overlay_repo::create_on(
                    txn,
                    &mutation_scope,
                    NewOverlay {
                        price_overlay_id,
                        tenant_id: tenant,
                        scope: selector,
                        precedence,
                        interval,
                        tax_basis,
                        disclosure,
                        target_ref,
                    },
                    lines,
                    stamp,
                )
                .await
                .map(|revision| OverlayAcceptedView {
                    price_overlay_id,
                    revision,
                })
                .map_err(|e| crate::infra::storage::repo_failure(&e))
            })
        },
        |view: &OverlayAcceptedView| {
            serde_json::to_value(view).map_err(|e| {
                DomainError::Internal(format!("cannot render the created overlay: {e}"))
            })
        },
    )
    .await
    .map_err(CanonicalError::from)?;

    Ok(match outcome {
        Guarded::Performed(view) => created_overlay(view)?,
        Guarded::Replayed { status, body } => {
            replayed_overlay(CREATE_OVERLAY_OPERATION, status, &body)?
        }
    })
}

/// The `201` a performed create answers — a **draft**, since nothing publishes
/// from a save (`inst-pl-return`) — with its location and its entity tag.
fn created_overlay(view: OverlayAcceptedView) -> Result<Response, CanonicalError> {
    let price_overlay_id = view.price_overlay_id;
    let revision = view.revision;
    let mut response = (StatusCode::CREATED, Json(view)).into_response();
    response.headers_mut().insert(
        LOCATION,
        format!("{PRICE_OVERLAYS}/{price_overlay_id}")
            .parse()
            .map_err(|_| DomainError::InvalidRequest("unrenderable location".to_owned()))?,
    );
    response.headers_mut().insert(
        ETAG,
        preconditions::revision_etag(revision, crate::domain::concurrency::RowVersion::new(0))
            .parse()
            .map_err(|_| DomainError::InvalidRequest("unrenderable entity tag".to_owned()))?,
    );
    Ok(response)
}

/// The recorded answer a replay is handed back, verbatim.
///
/// `plans::replayed`'s shape and its reasoning. `Location` is rebuilt from the
/// **recorded body**, which is a pure function of an id that body carries and of
/// nothing this request computed.
///
/// **No `ETag`, and here that is load-bearing rather than incidental.** The dedup
/// row stores a status and a body and no headers, and the tag the first caller was
/// given named `(revision 0, version 0)` — a coordinate a `PATCH` between the two
/// calls has already moved. A tag rebuilt from the stored body would be a
/// precondition token that looks current and is not, and this plane's `PATCH`
/// requires one; a replayed caller re-reads the overlay for its tag, which is the
/// read it would have to make before editing in any case.
///
/// # Errors
/// [`DomainError::Internal`] when the stored status is not one — see
/// [`super::replayed_status`].
fn replayed_overlay(
    operation: &str,
    status: i32,
    body: &serde_json::Value,
) -> Result<Response, DomainError> {
    let status = super::replayed_status(operation, status)?;
    let location = body
        .get("price_overlay_id")
        .and_then(serde_json::Value::as_str)
        .map(|price_overlay_id| format!("{PRICE_OVERLAYS}/{price_overlay_id}"));
    Ok(match location {
        Some(location) => (status, [(LOCATION, location)], Json(body.clone())).into_response(),
        None => (status, Json(body.clone())).into_response(),
    })
}

async fn replace_lines(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(price_overlay_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PRICE_OVERLAY,
        crate::authz::actions::WRITE,
        Some(crate::authz::OwnerTenant(tenant)),
        Some(crate::authz::ResourceRef(price_overlay_id)),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let body: ReplaceLinesRequest = preconditions::parse_body(&body)?;
    // The **overlay revision's** tag, not a plan's: an overlay has no plan.
    let tag = preconditions::if_match_revision(&headers)?;
    // **The two must agree.** The store is addressed by the tag, so a body naming
    // a different revision would rewrite one revision and be told about another —
    // and a client that then submitted the revision it was handed would submit a
    // revision it never edited. Refused rather than silently preferring one.
    if body.revision != tag.revision {
        return Err(DomainError::InvalidRequest(format!(
            "the body names revision {} and If-Match names revision {}; a line-set replacement \
             addresses one revision and the two must agree",
            body.revision, tag.revision
        ))
        .into());
    }
    // The lines only — the interval is not editable through this route, so an
    // interval check here would judge a value the request does not carry.
    let lines = lines_of(OverlayInterval::default(), &body.lines)?;

    let stamp = audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    let row_version = state
        .overlays
        .replace_lines(
            &scope,
            tenant,
            price_overlay_id,
            tag.revision,
            i64::try_from(tag.version.get()).unwrap_or(i64::MAX),
            lines,
            stamp,
        )
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?;

    let mut response = Json(OverlayAcceptedView {
        price_overlay_id,
        // The revision that was **written**, which is the tag's.
        revision: tag.revision,
    })
    .into_response();
    response.headers_mut().insert(
        ETAG,
        preconditions::revision_etag(
            tag.revision,
            crate::domain::concurrency::RowVersion::new(
                u64::try_from(row_version).unwrap_or_default(),
            ),
        )
        .parse()
        .map_err(|_| DomainError::InvalidRequest("unrenderable entity tag".to_owned()))?,
    );
    Ok(response)
}

async fn submit_overlay(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(price_overlay_id): Path<Uuid>,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PRICE_OVERLAY,
        crate::authz::actions::WRITE,
        Some(crate::authz::OwnerTenant(tenant)),
        Some(crate::authz::ResourceRef(price_overlay_id)),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let request: SubmitOverlayRequest = preconditions::parse_body(&body)?;
    let record = state
        .overlays
        .load(&scope, tenant, price_overlay_id, request.revision)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "price overlay revision".to_owned(),
            id: format!("{price_overlay_id}/{}", request.revision),
        })?;

    // **Only an open draft is submittable.** §5's row is "Submit **the draft**"
    // and `inst-pl-commit` pins the approval unit to one ("subject stays draft;
    // mutation voids the unit"). Without this, submitting a `published` revision
    // would open a second always-material unit over content that is already live —
    // and `overlay_facts` skips the candidate's own overlay (D-107), so a live
    // revision would validate against a world told to ignore it.
    //
    // The `PATCH` gets this from its compare-and-swap, which carries
    // `lifecycle_state = 'draft'`; the submit has no swap, so it needs the check.
    if record.lifecycle_state != OverlayLifecycle::Draft {
        return Err(DomainError::LifecycleForbidden(format!(
            "price overlay {price_overlay_id} revision {} is {}; only an open draft revision is \
             submittable",
            record.revision,
            record.lifecycle_state.as_str()
        ))
        .into());
    }

    // `inst-pl-validate`: the whole rule set, aggregate, over the world as it
    // stands **now**. §4.2 runs the same set again inside the publish commit,
    // because the world moves between the two -- and that second run is
    // `infra::overlay_publish` step 1b (D-234 residue (1)), so this sentence is
    // a statement about built code rather than about an intended one.
    let content_for_pin = record.content();
    let world = state
        .overlays
        .world_for(&scope, tenant, &record)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?;
    let candidate = OverlayCandidate {
        price_overlay_id: record.price_overlay_id,
        revision: record.revision,
        scope: record.scope.clone(),
        precedence: record.precedence,
        interval: record.interval,
        tax_basis: record.tax_basis,
        disclosure: record.disclosure,
        target_ref: record.target_ref.clone(),
        lines: record.lines.clone(),
        world,
    };
    let report = validate(&candidate);

    // §5 types two of the nine codes **409** and the rest as architectural 422s.
    // A conflict is lifted out of the envelope so a caller reads the status the
    // design set assigns it; see the module doc.
    if let Some(violation) = conflict_of(&report) {
        return Err(match violation.code.as_str() {
            crate::domain::overlay_rules::PRECEDENCE_DUPLICATE => {
                DomainError::PrecedenceDuplicate(violation.detail.clone())
            }
            _ => DomainError::OverlayIntervalOverlap(violation.detail.clone()),
        }
        .into());
    }
    if !report.is_publishable() {
        return Err(DomainError::ValidationFailed(report).into());
    }

    // `inst-pl-commit`'s **second** half (D-06, D-234): a second person has
    // already seen exactly this content, so this call is the publish rather than
    // the submit. `POST …/plans/{planId}/publish`'s shape one plane over, and its
    // reason — S9 §5 spells this route as "Submit the draft … **then the D-06
    // publish unit**", so the two acts are one route by the design set's own
    // arrangement rather than by convenience.
    //
    // Matched on the content and not merely on the subject: an approval whose
    // subject moved after the decision covers content that no longer exists, so
    // answering with it would refuse the publish at the commit's own pin check
    // for the rest of the overlay's life. See `approval_repo::find_approved_for_content`.
    let subject_ref = crate::infra::storage::repo::audit_repo::overlay_revision_ref(
        price_overlay_id,
        record.revision,
    );
    let pin = crate::domain::approval::content_pin::overlay_content_hash(&content_for_pin);
    if let Some(approved) = state
        .approvals
        .approved_unit(&scope, tenant, &subject_ref, &pin)
        .await?
    {
        let authorization =
            crate::api::rest::publish::authorization_of(&approved).map_err(CanonicalError::from)?;
        let receipt = state
            .overlay_publish
            .commit(
                &ctx,
                &scope,
                tenant,
                crate::domain::publish::OverlayPublishUnit::new(price_overlay_id, record.revision),
                authorization,
                audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
            )
            .await?;
        return Ok((
            StatusCode::OK,
            Json(SubmitAcceptedView {
                price_overlay_id,
                revision: record.revision,
                outcome: OUTCOME_PUBLISHED.to_owned(),
                pending_version_ref: Some(receipt.pending_ref),
                materiality: ApprovalView::from(&approved)
                    .materiality
                    .and_then(|view| view.reason)
                    .unwrap_or_else(|| OVERLAY_ACT_REASON.to_owned()),
                approval: Some(ApprovalView::from(&approved)),
                warnings: Vec::new(),
            }),
        )
            .into_response());
    }

    // `inst-pl-commit`'s first half (D-50, D-225): the submit opens the
    // always-material Slice 5 approval unit. It runs **inside a transaction**, which
    // is `approval_repo::open`'s requirement rather than this route's preference —
    // the unit's audit record is appended in the same transaction, so a unit that
    // committed while its trail rolled back would leave `pricing_audit_log` answering
    // "who submitted this" with nothing.
    //
    // This arm is reached when **no** approved unit covers this revision and this
    // content. The publish half `inst-pl-commit` also names (D-06) is the early
    // return above, which closes D-225 with it. *(This comment read
    // "still not wired … nothing publishes" while sitting in the else-branch of code
    // that publishes — a sentence that was true when written and false one commit
    // later. It is the same defect as a register entry describing a pipeline that
    // exists as one that does not, and it is corrected here for the same reason.)*
    let (reason, stored_materiality) = rendered_materiality(&overlay_submit_materiality())?;
    let content = record.content();
    let opened = state
        .approvals
        .submit_overlay(
            &scope,
            tenant,
            &content,
            Uuid::now_v7(),
            stored_materiality,
            audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
        )
        .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(SubmitAcceptedView {
            price_overlay_id,
            revision: record.revision,
            outcome: OUTCOME_SUBMITTED.to_owned(),
            pending_version_ref: None,
            materiality: reason,
            approval: Some(ApprovalView::from(&opened)),
            warnings: report
                .warnings
                .iter()
                .map(|w| AdvisoryView {
                    code: w.code.clone(),
                    subject: w.subject.clone(),
                    detail: w.detail.clone(),
                })
                .collect(),
        }),
    )
        .into_response())
}

/// The materiality verdict an overlay submit carries — **evaluated, not asserted**.
///
/// `api::rest::threshold_policy::policy_diff_materiality`'s shape, for its reason:
/// the token an operator reads is produced by the same evaluator every other unit's
/// is, so two units compared by a reader are two answers from one function rather
/// than one answer and one literal. It passes no policy and no baseline, and that is
/// not a shortcut — [`materiality::evaluate`] examines the **act** half before it
/// consults either, so a configured threshold cannot reach this act. D-50 in the
/// evaluator's own terms.
///
/// # This call is what makes `Trigger::PriceOverlayMutation` real
///
/// `Trigger::subject_exists_in_this_crate` answers `true` for this trigger, and for
/// an **act**-half trigger that predicate is a claim about a *declaration*: the act
/// half is `ChangeSet::act()`, reachable through nothing but [`ChangeSet::of_act`],
/// so a trigger no surface constructs can never be answered by the evaluator however
/// many tables its subject has. `domain::materiality::triggers`' module doc records
/// that at length for `GrandfatheringCutover`, whose store landed three commits
/// before its declaration and which stayed `false` throughout.
///
/// **A declaration is not the same as a `pub fn` that builds one**, which is the
/// sharper half and the reason this sits on a mounted route's path rather than in a
/// constructor beside the repository. `infra::bundle::composition_change_set` and
/// `rev_share_change_set` were exactly such constructors, with **no caller anywhere
/// in the crate**, while `BundleComposition` and `RevenueShareChange` answered
/// `true` on the strength of them (D-232). Both halves have since been settled and
/// neither by leaving the claim alone: `composition_change_set`'s caller is
/// `bundles::bundle_publish_materiality`, which gives the composition publish this
/// function's shape, and `rev_share_change_set`'s is `infra::bundle::declared_act`,
/// which is what picks between the two acts
/// D-104 registers — and it could not have been written earlier for a reason worth
/// carrying here: until the verdict could name the trigger, the two declarations
/// rendered identical bytes, so the call would have been observable to the census
/// that reads for it and to nothing else (D-321).
///
/// # Errors
/// [`DomainError::Internal`] when the verdict carries no reason. The only verdict
/// that does is a threshold-tripped one, and the act half answers above every
/// threshold — so this is unreachable, and it is *reported* rather than unwrapped
/// because the alternative is either a panic on a route or a literal fallback, and a
/// literal fallback is the very duplication this function removes.
fn overlay_submit_materiality() -> MaterialityVerdict {
    materiality::evaluate(
        &ChangeSet::of_act(Trigger::PriceOverlayMutation, Vec::new()),
        /* policy */ None,
        /* baseline */ None,
    )
}

/// The reason token the accepted view carries, and the jsonb the unit stores.
///
/// **One verdict, rendered twice — never built twice.** The pair is returned
/// together so the wire's string and the record's jsonb cannot come from two
/// evaluations, which is the same rule that made the token stop being a literal in
/// the first place: `MaterialityReason::as_str` has one home, and
/// `MaterialityView` is the one renderer of a verdict.
///
/// # Errors
/// [`DomainError::Internal`] when the verdict carries no reason, or will not
/// serialize. The only verdict with no reason is a threshold-tripped one and the act
/// half answers above every threshold, so both arms are unreachable — and *reported*
/// rather than unwrapped, because the alternative is a panic on a route or a literal
/// fallback, and a literal fallback is the duplication this removes.
pub(crate) fn rendered_materiality(
    verdict: &MaterialityVerdict,
) -> Result<(String, serde_json::Value), CanonicalError> {
    let reason = verdict
        .reason()
        .map(|reason| reason.as_str().to_owned())
        .ok_or_else(|| {
            CanonicalError::from(DomainError::Internal(
                "a declared act evaluated to a verdict with no reason".to_owned(),
            ))
        })?;
    let stored = serde_json::to_value(MaterialityView::from(verdict)).map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!(
            "cannot render the materiality verdict: {e}"
        )))
    })?;
    Ok((reason, stored))
}

/// `POST /bss-pricing/v1/price-overlays/{overlayId}/submit`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct SubmitOverlayRequest {
    /// The revision to submit.
    pub revision: u64,
}

#[allow(clippy::implicit_hasher)]
async fn list_overlays(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PRICE_OVERLAY,
        crate::authz::actions::READ,
        None,
        None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let page = state
        .overlays
        .list_odata(&scope, tenant, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(OverlayListView {
        overlays: page.items.iter().map(view_of).collect(),
        page_info: page.page_info,
    })
    .into_response())
}

/// `GET /price-overlays/{overlayId}` — the authoring revision.
///
/// Draft wins when one is open, else the published revision. Same rule as
/// [`super::plans`]' `GET /plans/{planId}`: a caller editing an overlay is
/// working on the draft, and a read that answered the published revision would
/// hand them a body their next `PATCH` would not match. Abandoned-only, missing,
/// or out of scope all read as 404 — one answer, so a 403 cannot confirm that
/// a foreign tenant's overlay exists.
///
/// The gate is `price_overlay × read` with `owner_tenant_id = None`, so the PDP
/// derives the scope from the subject; `resource_id` names this overlay.
async fn get_overlay(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(price_overlay_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PRICE_OVERLAY,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ Some(crate::authz::ResourceRef(price_overlay_id)),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let tenant = ctx.subject_tenant_id();
    let record = state
        .overlays
        .authoring(&scope, tenant, price_overlay_id)
        .await
        .map_err(|e| crate::infra::storage::repo_failure(&e))?
        .ok_or_else(|| CanonicalError::from(not_readable(price_overlay_id)))?;

    let view = view_of(&record);
    let tag = preconditions::revision_etag(
        view.revision,
        crate::domain::concurrency::RowVersion::new(view.row_version),
    );
    if preconditions::if_none_match(&headers, &tag) {
        return Ok(preconditions::not_modified(&tag));
    }
    Ok(([(ETAG, tag)], Json(view)).into_response())
}

/// The "absent, or not yours, or holding nothing an author can act on" refusal.
fn not_readable(price_overlay_id: Uuid) -> DomainError {
    DomainError::NotFound {
        subject: "price overlay".to_owned(),
        id: price_overlay_id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The router.
// ---------------------------------------------------------------------------

/// Mount the overlay authoring routes.
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(PRICE_OVERLAYS)
        .operation_id("bss_pricing.create_price_overlay")
        .summary("Author a PriceOverlay draft")
        .description(
            "Creates an overlay at revision 0 in `draft`, with its whole adjustment line set \
             (D-42: an overlay is a container of one or more lines, never a single adjustment). \
             A save **never publishes** - the submit route is what opens the always-material \
             approval unit. An absent `tax_basis` is refused `TAX_BASIS_UNDECLARED`: L5 says \
             the basis must be declared and silence fails. `disclosure` defaults to \
             `restricted`, which is L6's fail-closed default. **`Idempotency-Key` is required \
             and is honoured**: a retry under the same key replays the first call's `201` and \
             its overlay id rather than authoring a second draft, and the same key carrying a \
             different request is `409` `IDEMPOTENCY_PAYLOAD_MISMATCH`. Gates on \
             `price_overlay` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::idempotency_key_param())
        .json_request::<CreateOverlayRequest>(
            openapi,
            "The overlay's scope, tax basis, disclosure and its whole adjustment line set.",
        )
        .handler(create_overlay)
        .json_response_with_schema::<OverlayAcceptedView>(
            openapi,
            StatusCode::CREATED,
            "The overlay draft as created; the entity tag names the revision it stands at.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::get(PRICE_OVERLAYS)
        .operation_id("bss_pricing.list_price_overlays")
        .summary("List PriceOverlays")
        .description(
            "The admin and Tariffs read. Returns **every** revision, draft included, optionally \
             narrowed with `$filter=scope_class eq 'global'`, and paginated on an opaque cursor \
             per D-125. Default order is `price_overlay_id` then `revision` - the seekset \
             tiebreaker. Every row carries its `precedence`, so a caller assembling a stack \
             reads it from the row rather than from the sequence. It does **not** filter on \
             `disclosure`: L6 governs consumer-facing exposure and section 3 step 7 is explicit \
             that operator and service reads are unaffected, so a `restricted` overlay is still \
             its author's to read. Gates on `price_overlay` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::history::limit_param())
        .param(crate::api::rest::history::cursor_param())
        .handler(list_overlays)
        .with_odata_filter::<OverlayFilterField>()
        .with_odata_orderby::<OverlayFilterField>()
        .json_response_with_schema::<OverlayListView>(
            openapi,
            StatusCode::OK,
            "The tenant's overlays.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(PRICE_OVERLAY_BY_ID)
        .operation_id("bss_pricing.get_price_overlay")
        .summary("Read a PriceOverlay's authoring revision")
        .description(
            "Returns the overlay's **open draft** revision when it has one, and its **published** \
             revision otherwise, with its whole line set. This is the authoring read \
             (`price_overlay` x `read`); `lifecycle_state` and `revision` name which revision \
             was answered, so a caller never infers it. The `ETag` header carries \
             `\"<revision>-<row_version>\"` and is what `PATCH` takes as `If-Match`. An overlay \
             outside the caller's scope reads exactly like an absent one (404, no existence \
             leak). The create's `Location` is this path.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("overlayId", "The overlay to read.")
        .param(crate::api::rest::plans::if_none_match_param())
        .handler(get_overlay)
        .json_response_with_schema::<OverlayView>(
            openapi,
            StatusCode::OK,
            "The overlay's open draft revision, or its published revision.",
        )
        .no_content_response(
            StatusCode::NOT_MODIFIED,
            "The caller's `If-None-Match` matches the current representation, so the body is \
             not re-sent.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(PRICE_OVERLAY_BY_ID)
        .operation_id("bss_pricing.replace_price_overlay_lines")
        .summary("Replace an open draft revision's whole line set")
        .description(
            "Replaces the adjustment lines **wholesale** - every D-42 rule quantifies over the \
             set, so a partial update leaves nothing the validator can evaluate. `If-Match` \
             carries the **overlay revision's** entity tag, not a plan's: an overlay has no plan \
             and D-92 gives it a revision chain of its own. Only an open draft revision's lines \
             are mutable. Supplying a line's `line_id` is how its identity stays stable across \
             revisions. Gates on `price_overlay` x `write`.",
        )
        .tag(TAG)
        .path_param("overlayId", "The overlay whose line set is replaced.")
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::if_match_param(
            "The tag is the overlay revision's own, not a plan's; the submit's receipt and the \
             previous replace both answer one.",
        ))
        .json_request::<ReplaceLinesRequest>(
            openapi,
            "The replacement line set, whole - every D-42 rule quantifies over the set.",
        )
        .handler(replace_lines)
        .json_response_with_schema::<OverlayAcceptedView>(
            openapi,
            StatusCode::OK,
            "The line set was replaced; the entity tag names the version it now stands at.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router
        .layer(Extension(state))
        // D-178's edge, carried with the routes rather than at the merge, so a
        // surface reachable without it cannot build an `AuditStamp`.
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// Mount the overlay **submit** route, on the governance state.
///
/// # Why this one route is mounted apart from its siblings
///
/// It has two acts, and the second requests a `CatalogVersion`.
/// [`GovernanceState`]'s own criterion — which of the two states may request one —
/// is what put the overlay routes on [`AuthoringState`] when the submit had a
/// single act and requested none; gaining the publish arm (D-234) is exactly the
/// condition that criterion names. So the route moved rather than the criterion
/// bending, and it joins the plan plane's identical two-act route
/// (`POST …/plans/{planId}/publish`), which has always been here.
///
/// **The path does not change**, so no §5 table row and no authz catalog row
/// moves: the routers are merged and the URL is what a client sees.
pub fn governance_router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = Router::new();
    OperationBuilder::post(PRICE_OVERLAY_SUBMIT)
        .operation_id("bss_pricing.submit_price_overlay")
        .summary("Submit a PriceOverlay draft revision")
        .description(
            "Runs the whole `PriceOverlayValidator` rule set - scope taxonomy, the line set, the \
             eligibility filter, magnitude ranges, per-currency coverage, precedence uniqueness, \
             interval non-overlap and referential integrity - and reports **every** failure in \
             one pass. A single blocking violation blocks the submit. `PRECEDENCE_DUPLICATE` and \
             `OVERLAY_INTERVAL_OVERLAP` answer **409**, which is what section 5 types them; the \
             other seven travel as an aggregate 400 whose per-violation codes are the \
             discriminators. Overlay mutation is **always material** (D-50) whatever a threshold \
             policy says. Gates on `price_overlay` x `write`.",
        )
        .tag(TAG)
        .path_param("overlayId", "The overlay to submit.")
        .authenticated()
        .no_license_required()
        .json_request::<SubmitOverlayRequest>(
            openapi,
            "The submission's note and the revision it is composed against.",
        )
        .handler(submit_overlay)
        .json_response_with_schema::<SubmitAcceptedView>(
            openapi,
            StatusCode::OK,
            "The submit carried its own authority and published: `outcome` is `published` \
             and `pendingVersionRef` names the version it minted.",
        )
        .json_response_with_schema::<SubmitAcceptedView>(
            openapi,
            StatusCode::ACCEPTED,
            "Accepted; the overlay is material and awaits an independent reviewer.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
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

/// The wire amounts as the pairs [`crate::domain::overlay::adjustment_of`] takes.
///
/// The adapter is here because [`AmountRequest`] is an `api_dto` — a wire type by
/// construction — and the parse it feeds is not. Keeping the DTO on this side of
/// the line is what let the parse move to the domain, which is what stopped
/// `infra::repricing` from calling into `api::rest` to read a stored document.
fn amount_pairs(amounts: &[AmountRequest]) -> Vec<(String, i64)> {
    amounts
        .iter()
        .map(|a| (a.currency.clone(), a.value_minor))
        .collect()
}
