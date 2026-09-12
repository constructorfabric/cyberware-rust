//! Slice 7's interactive repricing surface: `POST /plans/{planId}/supersessions`
//! (`inst-su-api`, D-88).
//!
//! One route, and it is the only way to reprice a **published** canonical scope key
//! interactively. What it composes is a single unit —
//! [`crate::infra::supersession::SupersessionService`] — so that the successor row, the
//! predecessor window's shorten and the successor window's schedule take **one**
//! approval rather than three: composing it from `POST …/prices` plus
//! `PATCH /price-windows/{id}` plus `POST …/windows` is three publish units with three
//! materiality evaluations, which is precisely what D-88 exists to collapse.
//!
//! # A module of its own, and that is the rule rather than a preference
//!
//! `api::rest::windows`' own doc states it: the route census keys on `pub fn router`
//! under `src/api/rest/**`, so **one module, one router, one census entry**. This route
//! is not a window surface — it authors a price row — and folding it into that module
//! would put a second subject under a doc whose whole first paragraph is about windows.
//! It shares `GovernanceState` because it holds the `CatalogVersion` registry handle,
//! which is the criterion [`crate::api::rest::state`] splits the two states on. This
//! gear now has **three** requesters of that one registry; `state`'s module doc carries
//! why three requesters is still one incrementer.
//!
//! # It answers **202** on both arms, and never 200 or 201
//!
//! `inst-su-return` says 202, and the reason is the window surface's: a publish unit is
//! not consumer-visible until `CatalogVersionPublished` + warm, so a 200 would claim the
//! price changed *for readers* when what changed is the truth side. A **201** would be
//! wrong for a second reason — the successor draft is created on the controlled arm too,
//! and a `Location` naming it would invite a caller to `PATCH` the row a reviewer is
//! looking at.
//!
//! `outcome` is the discriminator: `submitted_for_approval` when a second principal is
//! required, `superseded` when the unit committed. Two arms of one field rather than
//! two status codes, which is `WindowMutationOutcomeView`'s decision and the same
//! argument — neither arm can honestly answer 200.
//!
//! # The caller names the **predecessor row**, not the scope key
//!
//! `inst-su-api` offers both — *"the target scope key (or the current `priceId`)"* — and
//! the row id is taken. Three reasons, in order of weight: the key is ten axes and
//! re-spelling it on the wire is ten chances to name a key the caller did not mean,
//! while `PriceRowView` already hands the caller the id it is looking at; the row is
//! what the operator is actually superseding, so a row that has since left the published
//! plane is refused by name rather than resolving to a key whose occupant is a different
//! row; and the path's `{planId}` is then **checkable** against the row's own plan
//! rather than being decorative.
//!
//! # There is no `Idempotency-Key`, and that is §5's column rather than an omission
//!
//! §5 gives this endpoint an idempotency of **`(planId, scope key, changeover
//! instant)`** — the act's own identity — where the window `POST`'s column says *client
//! idempotency key*. So the key is natural and is paid inside the service: a second
//! request naming the same act is answered with the **same** pending unit rather than
//! `PENDING_CHANGE_UNIT_EXISTS`, and a request naming the same act with different
//! content is refused `DUPLICATE_SCOPE_KEY`.
//!
//! **Wiring `infra::idempotent` here keyed on the act would break the surface**, and the
//! reason is worth recording because the gate is right next door on the window `POST`.
//! The gate records the response body and replays it for every later arrival under one
//! key. The first call answers `submitted_for_approval`; the call made *after* an
//! independent approve is supposed to **commit**. Under a gate keyed on the act, that
//! second call would be handed the first call's recorded 202 and the unit could never be
//! committed at all. The window `POST` escapes this because its key is the *client's*,
//! and its own description says so in as many words: "completing an act a reviewer has
//! since approved is a new attempt and needs a new key."
//!
//! **DIVERGENCE, reported rather than fixed: the idempotency is natural up to the
//! commit and not past it.** A request replayed *after* the unit committed finds the
//! predecessor no longer the key's current row, and is refused —
//! `SUPERSESSION_WOULD_EMPTY_WINDOW` from `compose_windows`, because the successor's own
//! window now begins exactly at the changeover. That is a legible refusal and a safe direction (nothing is written), but
//! it is not the replay §5's column reads as. Making it one needs a stored response, and
//! a stored response needs a decision about what a replay of an act that was 202-then-
//! committed should say — which is the design set's, not this module's.

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::{ApprovalView, MaterialityView};
use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::preconditions;
use crate::api::rest::prices::{PriceContentView, content_of};
use crate::api::rest::state::GovernanceState;
use crate::api::rest::windows::verdict_json;
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::scope_key::PlanId;
use crate::infra::storage::repo::price_repo;
use crate::infra::supersession::{
    SupersessionOutcome, SupersessionPending, SupersessionReceipt, SupersessionRequest,
};
use time::OffsetDateTime;

/// The route's registered path template.
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a literal argument and silently passes a `const` one; `tests/module_test.rs`
/// binds the two spellings together.
pub const PLAN_SUPERSESSIONS: &str = "/bss-pricing/v1/plans/{planId}/supersessions";

/// The `OpenAPI` tag this surface is filed under — Slice 7's, as the window routes are.
const TAG: &str = "Price Windows";

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

/// The body of `POST /plans/{planId}/supersessions`.
///
/// The three things `inst-su-api` names, plus the change reason the scheduled window is
/// recorded under. The successor's content is [`PriceContentView`] — the **same** wire
/// shape `POST …/prices` and `PATCH …/prices/{priceId}` carry — so a row authored
/// through a supersession and a row authored through the ordinary door are one
/// vocabulary rather than two.
///
/// `supersedesPriceId` inside that content is **ignored**, and deliberately not rejected:
/// the door stamps it from the read that validated the key, so a body echoing a row it
/// just read back would otherwise have to strip a field to be accepted. The commit
/// checks the stored link rather than the submitted one
/// (`price_repo::commit_supersession_rows`), which is what makes ignoring it safe.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct SupersedeRequest {
    /// The row being superseded — the key's current published row.
    ///
    /// A row id rather than the ten axes; the module doc has the three reasons.
    pub predecessor_price_id: Uuid,
    /// When coverage hands over. Strictly future at submit, and at least D-47's max
    /// batching delay ahead at the approval commit (`inst-su-instant`).
    #[serde(with = "rfc3339")]
    pub changeover: OffsetDateTime,
    /// The successor row's whole content, in the authoring vocabulary.
    pub successor: PriceContentView,
    /// The operator-supplied change reason the **scheduled** window is recorded under.
    ///
    /// The shorten carries none and cannot be given one: `adjust_effective_to` takes no
    /// reason and the column is frozen by the append-only trigger. That asymmetry is a
    /// design-set gap recorded at `infra::supersession::SupersessionCommit`, not a
    /// choice here.
    pub reason_code: String,
}

/// What a supersession answers with.
///
/// `outcome` is the only discriminator, for [`crate::api::rest::windows`]'s stated
/// reason: neither arm can honestly answer 200, so the status is 202 on both and the
/// token says which happened. Every other field is nullable on exactly one arm, and
/// which one is written at the field.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct SupersessionOutcomeView {
    /// `superseded` | `submitted_for_approval`.
    pub outcome: String,
    /// The plan whose subject re-projects.
    pub plan_id: Uuid,
    /// The plan revision this act freezes — the plan's **current** one. A supersession
    /// opens no revision: price rows hang off the plan, not off a revision.
    pub revision: u64,
    /// The row that left, or would leave, the published plane.
    pub predecessor_price_id: Uuid,
    /// The successor draft. On **both** arms, because the controlled arm stages it —
    /// that draft is the reviewer's subject, so there is nothing to review until it
    /// exists. It is the id the store holds, never one this request minted and
    /// discarded.
    pub successor_price_id: Uuid,
    /// The changeover, echoed so a retry can be built from the answer.
    #[serde(with = "rfc3339")]
    pub changeover: OffsetDateTime,
    /// The window whose end moves, or would move, to the changeover.
    pub shortened_window_id: Uuid,
    /// The successor's open-ended window. `null` on the controlled arm, where no window
    /// was scheduled and an id here would name a row that does not exist.
    pub successor_window_id: Option<Uuid>,
    /// The registry's pending handle. `null` on the controlled arm: no version was
    /// requested for an act that did not commit, and D-156 is why.
    pub pending_version_ref: Option<String>,
    /// The act sequence the shortened window now stands at — the tag a caller's next act
    /// on that window must assert (D-190/D-191). `null` on the controlled arm, where the
    /// window did not move.
    ///
    /// # Why these two tags are body fields and this route emits no `ETag`
    ///
    /// `api::rest::windows`' `if_match_param` tells a caller the tag comes from *"the
    /// answer to the act that scheduled or last moved this window"*, and every committed
    /// window act there emits one header. That rule was written when one surface moved
    /// windows. A supersession moves **two** in one act — it shortens the predecessor's
    /// and schedules the successor's — and one `ETag` header cannot carry two tags, so
    /// both travel as fields and neither is dropped. Emitting the shortened window's as
    /// the header would have been the worse choice: a caller reading it per the declared
    /// contract would have no way to tell which of the two windows it named.
    pub shortened_mutation_seq: Option<u64>,
    /// The act sequence the **successor's** window stands at, which no other response
    /// carries at all: the window is created by this act, so a caller that has to adjust
    /// it later has no other producer of its tag. `null` on the controlled arm, where no
    /// window was scheduled.
    pub successor_mutation_seq: Option<u64>,
    /// Why a second principal was required, or was not — **this call's** evaluation.
    ///
    /// Present exactly when this call evaluated one: on the arm that opened a unit, and
    /// on the auto-publishable commit. `null` on the two arms that evaluate nothing —
    /// the idempotent replay and the commit under an approval — where the authority is
    /// the verdict the **unit stored**, and it rides `approval`. One verdict per answer,
    /// which is the whole of it: carrying a fresh one beside a stored one let a single
    /// 202 assert two materiality reasons for one act.
    pub materiality: Option<MaterialityView>,
    /// The unit reviewing the act, **or the one it committed under**. `null` exactly when
    /// no second principal was involved — which makes it the discriminator between an
    /// auto-publishable commit and an approved one, the role it plays on the publish
    /// mount.
    pub approval: Option<ApprovalView>,
}

impl SupersessionOutcomeView {
    fn of_receipt(receipt: &SupersessionReceipt) -> Self {
        Self {
            outcome: "superseded".to_owned(),
            plan_id: receipt.plan_id.get(),
            revision: receipt.revision,
            predecessor_price_id: receipt.predecessor_price_id,
            successor_price_id: receipt.successor_price_id,
            changeover: receipt.changeover,
            shortened_window_id: receipt.shortened_window_id,
            successor_window_id: Some(receipt.successor_window_id),
            pending_version_ref: Some(receipt.pending_version_ref.clone()),
            shortened_mutation_seq: Some(receipt.shortened_mutation_seq),
            successor_mutation_seq: Some(receipt.successor_mutation_seq),
            // **Both carried on this arm too**, which is `api::rest::publish`'s decision
            // and was this module's mistake: `PublishOutcomeView` renders the verdict on
            // both arms and uses `approval` as the discriminator between "two principals
            // agreed" and "no second principal was required". Nulling both here made a
            // committed supersession under an approval byte-identical to an
            // auto-publishable one, so a client could not record which approval
            // authorized the price change it had just made.
            materiality: receipt.verdict.as_ref().map(MaterialityView::from),
            approval: receipt.authorization.as_ref().map(ApprovalView::from),
        }
    }

    fn of_pending(pending: &SupersessionPending) -> Self {
        Self {
            outcome: OUTCOME_SUBMITTED.to_owned(),
            plan_id: pending.plan_id.get(),
            revision: pending.revision,
            predecessor_price_id: pending.predecessor_price_id,
            successor_price_id: pending.successor_price_id,
            changeover: pending.changeover,
            shortened_window_id: pending.shortened_window_id,
            successor_window_id: None,
            pending_version_ref: None,
            shortened_mutation_seq: None,
            successor_mutation_seq: None,
            // `None` on the idempotent replay, where the unit's **stored** verdict is
            // the authority and rides `approval` — see `SupersessionPending::verdict`.
            // Two documents here is how one 202 came to assert two reasons for one act.
            materiality: pending.verdict.as_ref().map(MaterialityView::from),
            approval: Some(ApprovalView::from(&pending.approval)),
        }
    }
}

/// `POST /plans/{planId}/supersessions`: compose the unit, and commit it if it may.
async fn supersede_price(
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

    // `plan × write` — §5's own map for this endpoint, and the same pair the window
    // mutations ask for. **Not `plan × publish`**: the entrance is what `publish`
    // guards, and a supersession that commits does so under an approval this route
    // does not grant itself. `resource_id` names the plan so the gate is answerable
    // per plan, and `require_constraints` is set so an unconstrained allow fail-closes.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ Some(crate::authz::ResourceRef(plan_id.get())),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // **Parsed after the gate**, which is `api::rest::plans`' house rule stated outright:
    // a module doc asserting "the gate before the repository" reads as two disciplines if
    // two modules differ on where the body is read. `windows.rs` parses first only because
    // `preconditions::idempotency_key` must be read first (D-171) — a reason this route
    // does not have, since the gate needs nothing but the path. Before this order a
    // caller holding no grant at all was told their body was malformed, and a PDP outage
    // answered 400 rather than the fail-closed 503.
    let request: SupersedeRequest = preconditions::parse_body(&body)?;

    // The key comes from the **row**, resolved under the scope the gate compiled. This
    // read is outside the service's transaction on purpose and is not a precondition:
    // what it establishes is *which act the caller means*, and the service re-reads the
    // key's whole world inside the transaction that writes (D-176). A row that leaves
    // the published plane between here and there is refused there, by name.
    let key = price_repo::load_scope_key(
        &state.db.conn().map_err(|e| {
            DomainError::Internal(format!("bss-pricing: supersession connection: {e}"))
        })?,
        &scope,
        tenant,
        request.predecessor_price_id,
    )
    .await
    .map_err(|e| crate::infra::storage::repo_failure(&e))?
    .ok_or_else(|| DomainError::NotFound {
        subject: "price row".to_owned(),
        id: request.predecessor_price_id.to_string(),
    })?;
    // A row of another plan answers **404** rather than 400, which is `patch_price`'s
    // decision for the same shape: the path names a resource that does not exist under
    // that plan, and a 400 would confirm the row exists somewhere else.
    if key.plan_id() != plan_id {
        return Err(DomainError::NotFound {
            subject: "price row on this plan".to_owned(),
            id: request.predecessor_price_id.to_string(),
        }
        .into());
    }

    let stamp =
        crate::api::rest::auth_context::audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    // Ahead of the call, for the reason `cutovers` gives: the column is frozen by
    // the table's append-only trigger.
    crate::api::rest::require_reason_code(&request.reason_code)?;
    let outcome = state
        .supersessions
        .supersede(
            &ctx,
            &scope,
            tenant,
            SupersessionRequest {
                key,
                changeover: request.changeover,
                successor: content_of(&request.successor)?,
                // Minted here, server-side, exactly as a price row's id and a window's
                // are: the surface has to be able to name both in its answer before
                // either is durable. On the call that follows an approve the price id is
                // discarded and the staged row's is used — see `SupersessionRequest`.
                successor_price_id: Uuid::now_v7(),
                successor_window_id: Uuid::now_v7(),
                reason_code: request.reason_code,
            },
            verdict_json,
            stamp,
        )
        .await?;

    let view = match &outcome {
        SupersessionOutcome::Committed(receipt) => SupersessionOutcomeView::of_receipt(receipt),
        SupersessionOutcome::SubmittedForApproval(pending) => {
            SupersessionOutcomeView::of_pending(pending)
        }
    };
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

/// Build the Axum router for the supersession surface and register its operation.
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    OperationBuilder::post("/bss-pricing/v1/plans/{planId}/supersessions")
        .operation_id("bss_pricing.supersede_price")
        .summary("Reprice a published canonical scope key as one atomic unit")
        .description(
            "Composes the **supersession unit** (D-88) over one canonical scope key: a successor \
             draft row beside the published row it replaces, the predecessor window's \
             `effectiveTo` shortened to the changeover, and the successor's window scheduled \
             open-ended from it - one approval unit and one local ACID transaction, or nothing. \
             Answers `202` on both arms, `outcome` saying which: `submitted_for_approval` when the \
             standard per-currency price-delta evaluation makes the change material and no \
             independent approve covers it yet, `superseded` when the unit committed. \
             \
             The body names the **current `priceId`** rather than the ten scope-key axes, and \
             the `{planId}` segment is checked against the row's own plan - a row of another plan \
             answers `404`. The changeover must be strictly future at submit and at least D-47's \
             max batching delay (5 min) ahead at the approval commit \
             (`SUPERSESSION_INSTANT_PASSED`); the key must hold a scheduled or active window \
             covering it, a dormant key being a revival rather than a supersession \
             (`SUPERSESSION_KEY_DORMANT`); an `existing_grandfathered` generation may never be \
             superseded at all; and a usage row's successor must not move what its continued tier \
             counter is denominated in, derived from or priced by (`SUPERSESSION_UNIT_MISMATCH`). \
             \
             **No `Idempotency-Key` header, which is S5's own idempotency column for this \
             surface**: the act is keyed by `(planId, scope key, changeover instant)`, so a second \
             identical request is answered with the *same* pending unit and a request naming the \
             same act with different content is `DUPLICATE_SCOPE_KEY` (409). A replay after the \
             unit has committed is refused rather than replayed; the module doc reports that as a \
             divergence. \
             \
             The successor draft **is** written on the controlled arm - it is the reviewer's \
             subject - while the publish, the flip and both window operations are not. Gates on \
             `plan` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan whose key is being repriced.")
        .json_request::<SupersedeRequest>(
            openapi,
            "The predecessor row, the changeover instant and the successor's content.",
        )
        .handler(supersede_price)
        .json_response_with_schema::<SupersessionOutcomeView>(
            openapi,
            StatusCode::ACCEPTED,
            "The unit committed, or a unit was opened over the staged successor; `outcome` says \
             which.",
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
        // D-178's correlation edge, per-router as every mutating surface applies it: a
        // handler without it cannot build an `AuditStamp` and answers 500 to a caller
        // who should have got 403, `require_correlation` running above the authz gate.
        // `rest_authz.rs::every_mutating_router_applies_the_correlation_edge` scans the
        // source for this rather than a maintained list.
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}
