//! `POST /bss-pricing/v1/plans/{planId}/retire` — the plan's end
//! (`inst-rt-api`, `inst-rt-return`, `inst-re-warn`, D-109, D-128).
//!
//! `api::rest::cutovers`' shape, and every argument that module makes about this
//! layer holds here verbatim and is not restated: why the body is parsed **after**
//! the gate, why the answer is `202`, and why the verdict rides the response for
//! the record rather than as an explanation.
//!
//! # What differs, and it is the whole of this module's own content
//!
//! **One route, two acts, discriminated by the body.** `inst-rt-api` puts the
//! dry-run *first* — "returns the cancelled-window preview" — and the confirm
//! behind it. They are one path rather than two because they must answer from the
//! same judgement: a preview computed by a different endpoint from the commit is
//! a preview an operator can read and then watch do something else.
//! `infra::retirement` enforces that by sharing `compose_preview`; this module's
//! part is not to offer a second way in.
//!
//! **The gate is `plan × retire`, an action of its own.** Not `write`, which the
//! cutover takes: retirement is a publish unit with no way back (D-128), so an
//! operator holding the authority to *change* a plan does not thereby hold the
//! authority to *end* it. The action already exists in the catalog
//! (`authz::actions::RETIRE`) and this is its first route.
//!
//! **Both arms of the confirm answer `202`, and neither can honestly answer
//! `200`.** A retirement is always material (D-109), so the ordinary arm is
//! `submitted_for_approval`; the committed arm still leaves a **pending**
//! `CatalogVersion` behind, so the fact the caller most cares about — that the
//! plan reads not-sellable from a pin — is not true yet either.
//!
//! **The dry-run answers `200`.** It is a read: it opens no unit, requests no
//! version and writes nothing, which is what makes it legible to the approver
//! *before* the decision (D-61's reviewability invariant).

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::ApprovalView;
use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::preconditions;
use crate::api::rest::state::GovernanceState;
use crate::api::rest::windows::verdict_json;
use crate::domain::retirement::{KeptReason, WindowDisposition};
use crate::domain::scope_key::PlanId;
use crate::infra::retirement::{RetirementOutcome, RetirementPreview};
use time::OffsetDateTime;

/// The route's registered path template.
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a literal argument and silently passes a `const` one;
/// `tests/module_test.rs` binds the two spellings together.
pub const PLAN_RETIRE: &str = "/bss-pricing/v1/plans/{planId}/retire";

/// The `OpenAPI` tag this surface is filed under — the plan plane's, because the
/// subject is the plan rather than a price row or a window.
const TAG: &str = "BSS Pricing Plans";

/// The wire token for the submit arm — `api::rest::publish`'s, imported rather
/// than re-spelled.
///
/// `OUTCOME_SUBMITTED` is `pub(crate)` precisely so this surface does not carry a
/// second spelling: a client's `match` must not depend on which route answered it,
/// and a rename of the const there has to reach here. It is the **outcome**
/// vocabulary and not a lifecycle one — no column in this gear stores this word,
/// which is why `windows`, `overlays`, `bundles` and `customer_groups` all import
/// this same const while each keeps its own stored-state renderer.
const OUTCOME_SUBMITTED: &str = crate::api::rest::publish::OUTCOME_SUBMITTED;

/// The body of a retirement request.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RetireBody {
    /// Preview only: compute the verdict, write nothing, open no unit.
    ///
    /// **Defaults to the safe arm.** A body that omits it is read as a dry-run,
    /// because the failure modes are not symmetric: a caller who meant to confirm
    /// and got a preview reads it and calls again, while a caller who meant to
    /// preview and got a confirm has opened an approval unit over an irreversible
    /// act.
    pub dry_run: Option<bool>,
    // **`reason_code` was here and is removed**. It was a
    // non-`Option` member of a request DTO, so the generated schema marked it
    // required and `parse_body` refused a body without one -- and nothing ever
    // read it. `retire_plan` passed it nowhere, and `AuditStamp` carries only
    // `actor_principal_id`, `recorded_at` and `correlation_id`, so there was no
    // slot for it to land in. Its own doc claimed it was "recorded on the unit".
    //
    // Removed rather than wired, because the design set asks for a reason on this
    // door in no instruction: `reason_code` appears in `docs/` only as
    // `pricing_price_window.reason_code`, which is the window plane's own column
    // and does reach its store. Giving the retirement a recorded reason is a
    // design decision with a column behind it, not a repair -- and a required
    // parameter that is silently discarded is worse than an absent one, because
    // the operator believes the reason was kept.
}

/// One window and what the retirement will do to it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct WindowVerdictView {
    /// The window row.
    pub window_id: Uuid,
    /// The price row it schedules.
    pub price_id: Uuid,
    /// `cancelled` | `kept`.
    pub disposition: String,
    /// Why it is kept: `in_flight_subscribers` | `presence_unresolved` |
    /// `grandfathered_coverage_bound`. `null` on a cancelled window.
    ///
    /// **The field `inst-re-cancelflow` is about.** The confirm screen has to
    /// label kept windows distinctly from cancelled ones, and an operator shown a
    /// bare count cannot tell a plan whose coverage is being preserved from one
    /// whose coverage is being torn down.
    ///
    /// The third token is D-04's bound rather than D-51's predicate, and it is a
    /// **distinct** token for the same reason the first two are distinct from each
    /// other: "kept because somebody is on it", "kept because nobody could be
    /// asked" and "kept because this generation is grandfathered until a date the
    /// cancellation would not cover through" are three different facts, and an
    /// operator deciding whether to chase the difference needs the one that
    /// applies. It is not an error code — no refusal is rendered on this path —
    /// so D-204 clause (2)'s bar on this crate declaring wire facts the design set
    /// does not is untouched: §5 declares codes, and this is a reason label on a
    /// kept window, the vocabulary of which `inst-re-cancelflow` leaves open.
    pub kept_reason: Option<String>,
}

/// A reference to the plan, rendered for the confirm screen.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReferenceView {
    /// What kind of reference it is.
    pub kind: String,
    /// The referrer's id.
    pub referrer_id: Uuid,
    /// A name an operator can act on.
    pub referrer_label: String,
}

/// The dry-run's answer (`inst-rt-api`, `inst-re-warn`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RetirementPreviewView {
    /// The plan this would retire.
    pub plan_id: Uuid,
    /// The revision the flip would target.
    pub revision: u64,
    /// Every not-yet-active window, and what would happen to it.
    pub windows: Vec<WindowVerdictView>,
    /// References that **block** the retirement. Non-empty means the confirm
    /// would be refused `RETIRE_PLAN_REFERENCED`.
    pub blocking_references: Vec<ReferenceView>,
    /// Whether the in-flight-subscriber lane could be asked at all.
    ///
    /// `true` says every window above is kept because nobody could be asked
    /// rather than because a subscriber was found — D-131's fail-closed clause,
    /// which D-182 makes this system's only case.
    pub presence_unresolved: bool,
}

impl RetirementPreviewView {
    fn of(preview: &RetirementPreview) -> Self {
        Self {
            plan_id: preview.plan_id.get(),
            revision: preview.revision,
            windows: preview
                .windows
                .iter()
                .map(|verdict| WindowVerdictView {
                    window_id: verdict.window_id,
                    price_id: verdict.price_id,
                    disposition: match verdict.disposition {
                        WindowDisposition::Cancelled => "cancelled".to_owned(),
                        WindowDisposition::Kept(_) => "kept".to_owned(),
                    },
                    kept_reason: match verdict.disposition {
                        WindowDisposition::Cancelled => None,
                        WindowDisposition::Kept(KeptReason::InFlightSubscribers) => {
                            Some("in_flight_subscribers".to_owned())
                        }
                        WindowDisposition::Kept(KeptReason::PresenceUnresolved) => {
                            Some("presence_unresolved".to_owned())
                        }
                        WindowDisposition::Kept(KeptReason::GrandfatheredCoverageBound) => {
                            Some("grandfathered_coverage_bound".to_owned())
                        }
                    },
                })
                .collect(),
            blocking_references: preview
                .references
                .blocking
                .iter()
                .map(|(kind, reference)| ReferenceView {
                    kind: kind.as_str().to_owned(),
                    referrer_id: reference.referrer_id,
                    referrer_label: reference.referrer_label.clone(),
                })
                .collect(),
            presence_unresolved: preview.presence_unresolved,
        }
    }
}

/// What a confirmed retirement answers.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RetirementOutcomeView {
    /// `retired` | `submitted_for_approval`.
    pub outcome: String,
    /// What was decided about, on both arms — the approver reads it here.
    pub preview: RetirementPreviewView,
    /// The registry's pending handle. `null` on the controlled arm: no version is
    /// requested for an act that did not commit (D-156).
    pub pending_version_ref: Option<String>,
    /// The windows the cancellation flow was invoked on. Empty on the controlled
    /// arm, and empty on the committed one too whenever the presence lane could
    /// not answer.
    pub cancelled_window_ids: Vec<Uuid>,
    /// The unit a second principal has to decide. `null` on the committed arm.
    pub approval: Option<ApprovalView>,
}

async fn retire_plan(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    correlation: Option<Extension<CorrelationId>>,
    Path(plan_id): Path<Uuid>,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(correlation)?;
    let plan_id = PlanId::new(plan_id);
    let tenant = ctx.subject_tenant_id();

    // `plan × retire`, and not `write`: ending a plan is not a change to it.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::RETIRE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ Some(crate::authz::ResourceRef(plan_id.get())),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Parsed after the gate — `api::rest::supersessions`' house rule.
    let request: RetireBody = preconditions::parse_body(&body)?;

    if request.dry_run.unwrap_or(true) {
        let preview = state.retirements.preview(&scope, tenant, plan_id).await?;
        return Ok((StatusCode::OK, Json(RetirementPreviewView::of(&preview))).into_response());
    }

    let stamp =
        crate::api::rest::auth_context::audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    let outcome = state
        .retirements
        .retire(&ctx, &scope, tenant, plan_id, verdict_json, stamp)
        .await?;

    let view = match &outcome {
        RetirementOutcome::Retired(receipt) => RetirementOutcomeView {
            outcome: "retired".to_owned(),
            preview: RetirementPreviewView {
                plan_id: receipt.plan_id.get(),
                revision: receipt.revision,
                // The committed arm answers with what it **did**, and the
                // cancelled set below is the whole of that. Re-composing the
                // preview here would re-read a world the flip has already moved.
                windows: Vec::new(),
                blocking_references: Vec::new(),
                // **A literal with no operand, correct today and armed to lie**
                // (RUST-DATA-001, adjudicated LOW/latent).
                // [`RetirementReceipt`] carries no presence flag, so this is not a
                // fact about what the retirement found; it is `true` because
                // `RetirementService::compose_preview` can hand the composer only
                // `PresenceMap::fail_closed()` (D-131's clause, which D-182 makes
                // this system's only case) and under fail-closed every key reads
                // occupied, every window is `Kept(PresenceUnresolved)` and
                // `cancelled_window_ids` below is provably always empty.
                //
                // The day the D-79 presence lane hands a **resolved** map this
                // asserts the opposite of what happened, and a non-empty
                // `cancelled_window_ids` beside a `true` here is the
                // self-contradiction the DTO's own doc rules out. It is not
                // repaired here because the repair is a widened receipt, which is
                // that lane's own act — the shape `infra::retirement` records for
                // the missing `PriceWindowCancelled` event. What guards the
                // interval is
                // `rest_retirement::the_committed_arm_reports_no_presence_answer_and_cancels_nothing`,
                // which reddens the moment the pair can disagree.
                presence_unresolved: true,
            },
            pending_version_ref: Some(receipt.pending_version_ref.clone()),
            cancelled_window_ids: receipt.cancelled_window_ids.clone(),
            approval: None,
        },
        RetirementOutcome::SubmittedForApproval(pending) => RetirementOutcomeView {
            outcome: OUTCOME_SUBMITTED.to_owned(),
            preview: RetirementPreviewView::of(&pending.preview),
            pending_version_ref: None,
            cancelled_window_ids: Vec::new(),
            approval: Some(ApprovalView::from(&pending.approval)),
        },
    };
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

/// Build the Axum router for the retirement surface and register its operation.
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    OperationBuilder::post("/bss-pricing/v1/plans/{planId}/retire")
        .operation_id("bss_pricing.retire_plan")
        .summary("End a plan: block new subscriptions, preserve in-flight coverage")
        .description(
            "Retires a published plan (D-128). The flip is `published` -> `retired` on the plan's \
             **current** revision, which stays current - so the projector keeps sourcing it and an \
             in-flight subscriber's arrears charge still resolves. Retirement stops **selling**, \
             never **rating**. \
             \
             `dryRun` (**default `true`**) returns the preview an operator confirms against: every \
             not-yet-active window with what would happen to it, kept windows labelled distinctly \
             from cancelled ones, and the references that would block the act. A window is \
             cancelled only on a scope key with **no** in-flight subscribers (D-51); a window that \
             is a key's continuing coverage is kept, because cancelling it opens a trailing void no \
             gap check can see. The presence question is asked of the D-79 lane in **one** call \
             (D-131); while that lane has no client, the fail-closed posture reads every key as \
             occupied and every window is kept (D-182), which `presenceUnresolved` reports. \
             \
             **A retirement is always material** (D-109), so no configured threshold makes it \
             commit on one principal: the confirm answers `submitted_for_approval` until an \
             independent reviewer decides, then `retired`. Both answer `202` - the committed arm \
             leaves a **pending** `CatalogVersion`, so the plan does not read not-sellable from a \
             pin until that version completes. \
             \
             Refused with `RETIRE_PLAN_REFERENCED` (409) while a bundle composes the plan or an \
             add-on overrides one of its price rows; the refusal enumerates the referrers, because \
             the remedy is to re-compose or retire a *named* one first. Refused with \
             `RETIRE_OVER_LIVE_CUTOVER` (409) while a decided cutover of the plan is still ahead \
             of the clock, naming the unit: the remedy is to wait for its instant or withdraw the \
             unit while it is still submitted. A plan that is not at a published revision is \
             refused with its own state. \
             \
             Gates on `plan` x `retire` - an action of its own, because holding the authority to \
             change a plan is not holding the authority to end it.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan being retired.")
        // Describes what the DTO **holds**: `reason_code` was removed from
        // `RetireBody` by the same commit that left this sentence naming it, and no DTO in this
        // gear sets `deny_unknown_fields`, so an integrator following the description sent a
        // `reasonCode` that was accepted and silently discarded — the outcome the removal's own
        // comment calls worse than an absent field.
        .json_request::<RetireBody>(openapi, "Whether this is a dry-run.")
        .handler(retire_plan)
        .json_response_with_schema::<RetirementPreviewView>(
            openapi,
            StatusCode::OK,
            "The dry run's preview. This is the **modal** answer: `dryRun` defaults to `true`, \
             so a request that omits it is previewed and nothing is retired.",
        )
        .json_response_with_schema::<RetirementOutcomeView>(
            openapi,
            StatusCode::ACCEPTED,
            "The retirement committed, or a unit was opened over it; `outcome` says which.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi)
        .layer(Extension(state))
        // D-178's correlation edge, per-router as every mutating surface applies it.
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}
