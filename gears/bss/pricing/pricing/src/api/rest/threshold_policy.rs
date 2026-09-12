//! `GET/PUT /bss-pricing/v1/config/approval-threshold-policy` — the tenant's
//! per-currency approval-threshold policy (`design/05-governance.md` §5, D-10).
//!
//! # The `PUT` opens a unit; it does not apply a diff
//!
//! D-10 is direction-agnostic: **any** policy diff needs an independent second
//! `FinanceReviewer`, because the two-person rule's own foundation must not be
//! single-person-editable. Loosening a threshold obviously needs review;
//! tightening one does too, since a tightening that nobody reviewed is a denial of
//! service on the authoring plane dressed as prudence. So the `PUT` answers **202**
//! with the pending unit, and the proposed version becomes the tenant's policy when
//! that unit is approved **and its `effectiveFrom` has arrived** (D-188) — both facts
//! `infra::threshold::effective_version` reads, one off the approval store and one off
//! the clock, and neither a column anything flips.
//!
//! The second half is why the `GET` can answer `effective: null` for a tenant whose
//! proposal was approved minutes ago: an approved version whose start is still ahead
//! is not in force, and the tenant stays on the version it had — or on nothing, which
//! makes everything material.
//!
//! **The bootstrap is reachable and is fail-safe, which is the sentence that makes
//! the whole arrangement work.** A tenant with no policy has everything material
//! (`inst-mat-failsafe`), so its *first* policy `PUT` is itself an always-material
//! act that needs a second principal. No tenant can therefore configure a threshold
//! without first completing an approved unit, and a single-reviewer tenant simply
//! stays at "everything material" rather than being locked out of the gear.
//!
//! # `approval_policy`, not `config`, and that is segregation of duties
//!
//! The authz catalog carries `cf.bss.pricing.approval_policy.v1~` as a resource
//! type of its own, deliberately separate from `config`: a config admin must not be
//! able to weaken the thresholds that govern their own changes. The label already
//! existed and no authz vocabulary is minted here. `FinanceReviewer` holds
//! `approval_policy × write/read` in the matrix, which is also what makes D-61
//! satisfiable — the approver of a policy unit can read what they are approving.
//!
//! # §5's Idempotency cell, both halves (D-186)
//!
//! §5 gives this row the cell *"`ETag` + approval unit"*. The approval unit half was
//! always implemented exactly. The `ETag` half now is too, and the argument that used
//! to stand here for skipping it is **withdrawn as false** rather than edited around.
//!
//! **The premise that failed.** It read: *"a tenant's **first** proposal is made
//! against no prior version, so there is no tag the caller could echo … a mandatory
//! `If-Match` would make the bootstrap unreachable"*. The `GET` answers **200** with
//! `effective: null` for a tenant that has never had a version approved — that is this
//! module's own decision, argued two sections down — so the resource always has a
//! representation and therefore always has an entity tag. The bootstrap was never
//! unreachable, and `rest_threshold_policy.rs` pins it: a tenant with no policy reads
//! the tag off the `GET` and its first `PUT` is accepted carrying it.
//!
//! **The second premise was true and beside the point.** Every other `If-Match` in
//! this gear names a [`RowVersion`] read off a mutable row's `row_version` column, and
//! `pricing_approval_threshold` carries no such column — it is append-only history and
//! a version is minted, not bumped. But an entity tag is a statement about a
//! **representation**, not about a column. So the tag is a digest over the two facts
//! this `GET` serves: the effective version's number *or its absence*, and the pending
//! unit's id *or its absence*. Both, because a tag that moved only with `effective`
//! would not change when a proposal opened, and a validator that does not change when
//! the representation changes is broken rather than lenient. Absence is framed
//! distinctly from any number, which matters because a tenant's first version is
//! `0` — see [`PolicyTag`].
//!
//! **What the approval unit covers, and what it left open.**
//! `PENDING_CHANGE_UNIT_EXISTS` (409) refuses a second proposal while one is under
//! review, and every proposal that opens is reviewed by a second principal who is
//! shown its content (D-61). That is a strong control and it is why this was a gap
//! rather than a hole. What it does not reach is the **sequential** case: two
//! operators propose in turn, the first is approved, the second authored their version
//! against a policy they read before that happened, and nothing told them the world
//! moved. The precondition closes exactly that.
//!
//! **Where it is tested is a decision, not an implementation detail.** The header is
//! *read* at the transport ([`preconditions::if_match_policy`]) and *compared* inside
//! `ThresholdService::propose`/`retire`'s transaction, against the same two reads the
//! `GET` composes and before a version number is minted. `preconditions`' own doc
//! draws that line — this module refuses a request it cannot understand, the store
//! refuses one whose premise has moved — and a comparison in the handler would read
//! the world, decide, and then hand the decision to a statement that races it. The
//! refusal is `STALE_VERSION` (409), §3.3's own category for an `ETag` conflict, and
//! deliberately not a 412: the canonical family carries no such status and §3.3
//! forbids minting one, which is the same argument that keeps 422 out of this gear.
//!
//! [`RowVersion`]: crate::domain::concurrency::RowVersion
//! [`PolicyTag`]: crate::domain::concurrency::PolicyTag
//!
//! # No `404`, and no route-level `409` on the `GET`
//!
//! A tenant with no policy is answered `200` with `version: null` and no entries.
//! That is a **state** and not an absent resource: "unset" is the design set's own
//! bootstrap, `inst-mat-failsafe` is named for it, and answering 404 would tell an
//! operator the surface does not exist when what they actually need to know is that
//! everything is material.
//!
//! # The way back to unset is the `PUT`'s `retire` marker, and it deletes nothing
//! # (D-185)
//!
//! §6 makes *"unset ⇒ two-person rule always"* the state every tenant starts in, and
//! until D-185 it was a state no tenant could **return** to: the store is per-currency
//! rows keyed `(tenant, version, currency)`, so "no thresholds" is a version with zero
//! rows — which the `PUT` refuses (`THRESHOLD_INVALID`) and which, admitted, no reader
//! could tell from a version nobody proposed.
//!
//! **What the marker adds is expressiveness, not a capability.** The way back is not
//! "a version of absurdly high bars" that "stops being true the day a currency is
//! added": `reaches_absolute` is `magnitude >= absolute_minor`, so a **high** bar
//! makes *fewer* changes material, not more — the way back by arithmetic is a bar of
//! **zero**, which the CHECK permits and which every delta reaches. And a
//! currency with no entry meets `inst-mat-percurrency`'s fail-safe, so adding one
//! makes changes in it material rather than silently unreviewed. What a zero-bar
//! version cannot do is *say what the operator meant* — an
//! auditor reading it sees a threshold set to zero, not a decision to have none.
//!
//! So the way back is a **tombstone**, and its door is the `PUT`'s `retire` marker
//! rather than a verb of its own. A `DELETE` was built for it and withdrawn on the
//! same day, because a verb promising removal would describe the opposite of what
//! happens: the store is append-only and a retirement is an **appended version**. It
//! is a proposal like every other — a new version minted with the next number, its
//! content pinned, and the same always-material unit D-10 opens over any policy diff,
//! so a tenant cannot revert the two-person rule single-handed, which is the property
//! the whole arrangement would be worthless without. Nothing is deleted; the store
//! stays append-only history and the earlier versions stay exactly as their approvers
//! signed them.
//!
//! The `GET` then answers `effective` with that version and an **empty** entry list,
//! which is deliberately not `effective: null`. Both are unset and both make every
//! change material, but only one of them is a decision somebody made — and an auditor
//! asking when this tenant stopped having thresholds reads the version's number and
//! `effectiveFrom`, neither of which a null carries.

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
use uuid::Uuid;

use crate::api::rest::approvals::{
    ApprovalView, MaterialityView, PinnedThresholdPolicyView, ThresholdEntryView,
};
use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::preconditions;
use crate::api::rest::state::GovernanceState;
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::instant::truncate_millis;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{self, ChangeSet, ThresholdBasis, ThresholdEntry};
use crate::domain::money::CurrencyCode;
use crate::infra::threshold::AssertedPolicy;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to both operations (DE0205).
const TAG: &str = "BSS Pricing Governance";

/// The policy resource, as a `config` sub-resource (D-140: never a colon method).
///
/// The literal is repeated in both `OperationBuilder` calls below because DE0801
/// validates a **literal** argument and silently passes a `const` one; the two
/// spellings are pinned together by `tests/module_test.rs`'s route census.
pub const APPROVAL_THRESHOLD_POLICY: &str = "/bss-pricing/v1/config/approval-threshold-policy";

/// The largest basis-point threshold this surface accepts: 100%.
///
/// §6 says only `percent > 0` and names no ceiling, so this is a **decision** and
/// it is stated as one. A threshold above 100% is a threshold no price change can
/// reach — a doubling is `10_000` bp — so it is the two-person rule switched off by
/// arithmetic, which is exactly what
/// `chk_pricing_approval_threshold_percent_positive` refuses at the other end of
/// the range. It is also what keeps `percent_bp`'s domain `u32` inside the store's
/// signed 32-bit column with four orders of magnitude to spare.
pub const MAX_PERCENT_BP: u32 = 10_000;

/// The `If-Match` header this `PUT` requires, declared so a generated client sends
/// it (D-171).
///
/// **Not `plans::if_match_param`**, though the discipline is that helper's and the
/// argument for declaring at all is quoted from it: without a declaration a
/// generated client omits the header and learns of it from a 400. What does not
/// transfer is the *subject*. That helper's text is about a draft row and D-141 —
/// "every mutating verb on a draft presents its `ETag`" — and this resource is
/// neither a draft nor a row: it is append-only history, its tag is not a row
/// version, and the rule that puts a precondition here is §5's Idempotency cell
/// plus D-186. Reusing the sentence would have declared a true header under a false
/// reason.
fn if_match_param() -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(
            "Mandatory precondition (RFC 9110), and the governance section 5 `ETag` cell for this \
             row. The value is \
             the **opaque** tag the `GET` returns in its `ETag` header - copy it back verbatim. \
             It is not a row version: this store is append-only and has no version column, so \
             the tag is a digest over the representation the `GET` serves, which means it moves \
             when a version takes effect **and** when a proposal opens or closes. A tenant with \
             no policy at all is answered `200` and carries a tag, so the first proposal a \
             tenant ever makes satisfies this like any other. A tag that no longer describes \
             the policy is `409` `STALE_VERSION`; an absent or malformed one is `400`. Weak \
             validators, the wildcard `*` and tag lists are all refused - a wildcard would \
             author a governance policy over whatever happens to be current, which is what the \
             precondition exists to prevent."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// The tenant's policy as it stands: what is in force, and what is under review.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ThresholdPolicyView {
    /// The version currently in force, or `null` for a tenant that has none — which
    /// is a tenant that has never had one approved, **or** one whose only approved
    /// versions all start in the future. Under `null` every change is material
    /// (`inst-mat-failsafe`).
    ///
    /// **Present with an empty `entries` is the third state** and is not the same as
    /// `null`: it is a tombstone (D-185), a version the tenant authored and a second
    /// principal approved, saying they have no thresholds. Both make every change
    /// material; only this distinguishes "never configured" from "configured, then
    /// deliberately retired", which is what an auditor asking *when did this tenant
    /// stop having thresholds* needs — the version number and its `effectiveFrom`
    /// answer it, and a null cannot.
    pub effective: Option<PinnedThresholdPolicyView>,
    /// The unit reviewing a proposal, if one is open. A second `PUT` while this is
    /// present is refused `PENDING_CHANGE_UNIT_EXISTS` (409).
    pub pending_approval: Option<ApprovalView>,
}

/// A proposed policy version.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PutThresholdPolicyRequest {
    /// When the thresholds start applying, once approved. UTC, millisecond
    /// precision (D-144). Required on a threshold proposal; **must be absent on a
    /// retirement**, which is not schedulable — see `retire`.
    #[serde(default, with = "rfc3339::option")]
    pub effective_from: Option<OffsetDateTime>,
    /// The per-currency entries. **The whole policy**, not a patch: a version is a
    /// complete entry set, so a currency left out of this list is a currency with
    /// no threshold — which is material by `inst-mat-percurrency`'s fail-safe
    /// rather than a currency that keeps its old value. Required on a threshold
    /// proposal; **must be absent on a retirement**.
    pub entries: Option<Vec<ThresholdEntryView>>,
    /// Propose the **tombstone** — a version that positively says this tenant has
    /// no thresholds (D-185).
    ///
    /// A positive marker rather than an empty `entries`, because an empty entry set
    /// writes no rows and no reader could tell it from a version nobody proposed —
    /// which is why the authoring door refuses one. And the same `PUT` rather than
    /// a `DELETE`, because nothing is deleted: the store is append-only, and a
    /// retirement is an **appended version** like any other, minted with the next
    /// number, pinned, and in force only once an independent reviewer approves it.
    /// A verb promising removal would describe the opposite of what happens.
    ///
    /// Mutually exclusive with the two fields above: a retirement authors no
    /// entries, and it is not schedulable — it takes the tenant back to *everything
    /// is material*, so a future date would be an operator asking for **less**
    /// review between now and then than they have already decided they want.
    pub retire: Option<bool>,
}

/// What the `PUT` did: opened a unit over the proposed version.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ThresholdProposalView {
    /// The version this proposal minted. It is **not** in force yet.
    pub proposed: PinnedThresholdPolicyView,
    /// The always-material unit reviewing it (D-10).
    pub approval: ApprovalView,
}

/// Build the Axum router for the two policy operations and register them.
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/config/approval-threshold-policy")
        .operation_id("bss_pricing.get_approval_threshold_policy")
        .summary("Read the tenant's approval-threshold policy")
        .description(
            "The per-currency thresholds currently **in force** - the greatest proposed version \
             whose unit an independent `FinanceReviewer` approved, whose content still matches \
             what they signed, and whose `effectiveFrom` has arrived - together with the proposal \
             under review, if there is one. A tenant with no such version is answered `200` with \
             `effective: null`, which is a state and not an absent resource: unset means the \
             two-person rule \
             applies to **every** change (`inst-mat-failsafe`), and a currency with no entry in a \
             configured policy is material for the same reason. **The response carries the \
             `ETag` the `PUT` demands**, and this is the only place to obtain one: the tag is an \
             opaque digest over what this response serves - the effective version's number or \
             its absence, and the pending unit's id or its absence - so it moves when a version \
             takes effect and when a proposal opens or is withdrawn. A tenant with no policy is \
             answered `200` and carries a tag like any other, which is what makes a first \
             proposal satisfiable. Gates on `approval_policy` x \
             `read`, which is deliberately not `config` x `read`: a config admin must not read or \
             move the policy that governs their own changes.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::plans::if_none_match_param())
        .handler(get_threshold_policy)
        .json_response_with_schema::<ThresholdPolicyView>(
            openapi,
            StatusCode::OK,
            "The effective policy and the proposal under review.",
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

    let router = OperationBuilder::put("/bss-pricing/v1/config/approval-threshold-policy")
        .operation_id("bss_pricing.put_approval_threshold_policy")
        .summary("Propose the tenant's approval-threshold policy")
        .description(
            "Writes the proposal as a **new version** and answers `202` with the always-material \
             approval unit reviewing it (D-10). It does **not** apply the diff: the version \
             becomes the tenant's policy when an independent `FinanceReviewer` approves that \
             unit and its `effectiveFrom` has arrived - a version dated in the future is \
             approved and not yet in force, and the tenant stays on the policy it had - which is \
             why the store is versioned and append-only rather than mutated in \
             place - under mutation in place the proposed content would have nowhere to live and \
             the approval's pin nothing to cover. The body is the **whole** policy and not a \
             patch. A tenant's first proposal is itself material under the fail-safe, so no \
             tenant can configure a threshold without completing an approved unit first. Shape \
             rules, all `THRESHOLD_INVALID` (400): keys are ISO 4217 codes, at least one entry, \
             no currency twice, exactly one of `absolute_minor` / `percent_bp` per entry, \
             `absolute_minor` >= 0, and `percent_bp` in `1..=10000`. A second proposal while one is \
             under review is `PENDING_CHANGE_UNIT_EXISTS` (409) - decide it or withdraw it. \
             **`If-Match` is required**: send the opaque `ETag` the `GET` returned, verbatim. It \
             is not a row version - this store has none - but a digest over the policy as the \
             `GET` serves it, so it moves both when a version takes effect and when a proposal \
             opens or closes. The bootstrap is not an exception: a tenant with no policy is \
             answered `200` with `effective: null` and that response carries a tag, so a first \
             proposal asserts it like any other. A tag that no longer describes the policy is \
             refused `STALE_VERSION` (409) and **nothing is written** - no version number is \
             consumed and no unit opens; re-read the `GET` and author against the tag it hands \
             back. An absent or malformed tag is `400`. Gates on `approval_policy` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(if_match_param())
        .json_request::<PutThresholdPolicyRequest>(openapi, "The proposed per-currency entries.")
        .handler(put_threshold_policy)
        .json_response_with_schema::<ThresholdProposalView>(
            openapi,
            StatusCode::ACCEPTED,
            "The proposal is open; the body names the version and the unit reviewing it.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-178's edge, applied at this router's own tail for the reason every other
    // mutating router applies it at its own: a surface reachable without it cannot
    // build an `AuditStamp`.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// `GET /config/approval-threshold-policy`.
/// It answers a [`Response`] rather than a [`Json`] because it carries the `ETag`
/// the `PUT` demands (D-186), and it is the **only** place a caller can obtain one:
/// there is no create on this resource whose response could seed a tag. A tenant
/// with no policy at all is answered 200 and carries a tag like everyone else,
/// which is the sentence the withdrawn premise got wrong.
async fn get_threshold_policy(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx).await?;
    let held = state
        .thresholds
        .state(&scope, ctx.subject_tenant_id())
        .await
        .map_err(CanonicalError::from)?;
    // The tag comes off the state the body is rendered from, not off a second
    // read: `ThresholdState::tag` is the one producer, shared with the comparison
    // inside `propose`.
    let tag = preconditions::policy_etag(&held.tag());
    // The conditional read, and this resource is the one it was most owed: its
    // `If-Match` is mandatory and this `GET` is the **only** place a caller can
    // obtain a tag, so a client holding a fresh precondition had to re-download the
    // whole threshold set on every poll to get one.
    if preconditions::if_none_match(&headers, &tag) {
        return Ok(preconditions::not_modified(&tag));
    }
    Ok((
        [(ETAG, tag)],
        Json(ThresholdPolicyView {
            effective: held.effective.as_ref().map(PinnedThresholdPolicyView::from),
            pending_approval: held.pending.as_ref().map(ApprovalView::from),
        }),
    )
        .into_response())
}

/// `PUT /config/approval-threshold-policy`.
async fn put_threshold_policy(
    Extension(state): Extension<Arc<GovernanceState>>,
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

    // **After the gate, deliberately.** A caller who may not touch this resource is
    // told that, rather than being told their header is malformed — the ordering
    // `rest_authz.rs`'s `every_route_asks_the_catalogued_pair` depends on, and the
    // same ordering `schedule_window` argues for its own parameters. The tag is
    // *asserted* here and *tested* inside the service's transaction (D-186).
    let asserted = AssertedPolicy {
        tag: preconditions::if_match_policy(&headers).map_err(CanonicalError::from)?,
        now,
    };

    // `Bytes` + `parse_body`, never axum's `Json` extractor — the rule
    // `preconditions::parse_body` states and the other body-bearing handlers follow.
    // `Json<PutThresholdPolicyRequest>` would falsify the sentence three lines above:
    // an extractor runs during dispatch, so the body is read *before*
    // `require_authenticated`, and an anonymous caller who omits `Content-Type` is
    // answered 415 and one whose body does not fit the type 422 — two statuses this
    // registration does not declare, one of them the status `01-foundation.md` §3.3
    // forbids by name, both outside the canonical `Problem` envelope, on the route
    // that authors the tenant's two-person-review thresholds.
    let request: PutThresholdPolicyRequest = preconditions::parse_body(&body)?;

    let materiality = policy_diff_materiality()?;
    let stamp = audit_stamp(&ctx, now, correlation);

    // **The two arms of one door.** A retirement and a threshold set are both
    // *appended versions* — same mint, same always-material unit, same pin — so they
    // are authored the same way and differ only in what the version says. The body
    // discriminates, and the exclusivity is checked rather than assumed: a request
    // carrying both would be an operator who cannot be told which of the two they
    // proposed, over a diff a reviewer is about to sign.
    let (version, record) = if request.retire == Some(true) {
        if request.entries.is_some() || request.effective_from.is_some() {
            return Err(CanonicalError::from(DomainError::ThresholdInvalid(
                "a retirement authors no entries and is not schedulable: it takes the tenant \
                 back to `everything is material`, so a future start would be asking for less \
                 review in the meantime than has already been decided. Send `retire` alone"
                    .to_owned(),
            )));
        }
        // Quantized to the millisecond (D-144) and **truncated**, so the instant is
        // never later than the moment asked for.
        let at = truncate_millis(now);
        state
            .thresholds
            .retire(
                &scope,
                tenant,
                Uuid::now_v7(),
                at,
                asserted,
                materiality,
                stamp,
            )
            .await
            .map_err(CanonicalError::from)?
    } else {
        let Some(effective_from) = request.effective_from else {
            return Err(CanonicalError::from(DomainError::ThresholdInvalid(
                "a threshold proposal authors the instant its bars start applying; send \
                 `effectiveFrom`, or `retire` to propose the tombstone"
                    .to_owned(),
            )));
        };
        let Some(entries) = request.entries.as_ref() else {
            return Err(CanonicalError::from(DomainError::ThresholdInvalid(
                "a threshold proposal is a complete entry set; send `entries`, or `retire` to \
                 propose the tombstone. An empty list is not a retirement - it writes no rows \
                 and no reader could tell it from a version nobody proposed"
                    .to_owned(),
            )));
        };
        let entries = parse_entries(entries).map_err(CanonicalError::from)?;
        state
            .thresholds
            .propose(
                &scope,
                tenant,
                Uuid::now_v7(),
                effective_from,
                entries,
                asserted,
                materiality,
                stamp,
            )
            .await
            .map_err(CanonicalError::from)?
    };

    Ok((
        StatusCode::ACCEPTED,
        Json(ThresholdProposalView {
            proposed: PinnedThresholdPolicyView::from(&version),
            approval: ApprovalView::from(&record),
        }),
    )
        .into_response())
}
/// The materiality verdict a policy diff's unit records — **evaluated, not
/// asserted**.
///
/// The verdict is `materiality::evaluate`'s over the registered trigger this act *is*
/// (`Trigger::ThresholdPolicyDiff`, D-10), rather than a `Material` value written by
/// hand — so the stored `materiality` jsonb of a policy unit is produced by the same
/// evaluator as every other unit's, and a reader comparing two units is comparing two
/// answers from one function. It passes no policy, which is right for a reason beyond
/// convenience: the trigger is examined at §3 step 4 before any threshold is
/// consulted, so no configured policy can make this act auto-publishable, which is
/// D-10 exactly.
///
/// **Shared by both arms of the `PUT`** — a threshold set and the `retire` marker —
/// and that is the point rather than deduplication: D-10 is direction-agnostic, so
/// configuring a threshold and retiring one are one act as far as materiality is
/// concerned, and two call sites each building their own verdict would be two places
/// for that to stop being true. (Two branches of one handler, not two routes: the
/// `retire` marker is what replaced a `DELETE` verb.)
///
/// # Errors
/// [`DomainError::Internal`] when the verdict will not serialize, which is
/// unreachable and reported rather than unwrapped.
fn policy_diff_materiality() -> Result<serde_json::Value, CanonicalError> {
    let verdict = materiality::evaluate(
        &ChangeSet::of_act(Trigger::ThresholdPolicyDiff, Vec::new()),
        /* policy */ None,
        /* baseline */ None,
    );
    serde_json::to_value(MaterialityView::from(&verdict)).map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!(
            "cannot render the materiality verdict: {e}"
        )))
    })
}

// ---------------------------------------------------------------------------
// Request parsing — the shape rules §6 states and §5 codes `THRESHOLD_INVALID`.
// ---------------------------------------------------------------------------

/// The authored entries as the domain holds them.
///
/// Every refusal here is [`DomainError::ThresholdInvalid`], including the currency
/// one: [`CurrencyCode::new`] answers `CURRENCY_INVALID`, which is the right code
/// on a price row and the wrong one here — a caller of this route has no price row,
/// and a `currency` precondition violation would name a field they cannot find. The
/// error is therefore **re-coded**, not passed through, and the detail names the
/// offending code so the remedy is still actionable.
///
/// The order is the caller's, sorted by currency before it leaves: the pin is taken
/// over this rendering, so a policy proposed as `USD, EUR` and one proposed as
/// `EUR, USD` must be one version with one digest rather than two.
///
/// # Errors
/// [`DomainError::ThresholdInvalid`] on a code that is not ISO 4217 alpha-3, on an
/// entry setting neither or both bases, on a negative `absolute_minor`, and on a
/// `percent_bp` outside `1..=`[`MAX_PERCENT_BP`]. The empty-set and
/// duplicate-currency rules are [`ThresholdVersion::new`]'s and are refused inside
/// the proposal's transaction, where the version number they would have consumed is
/// still unminted.
fn parse_entries(authored: &[ThresholdEntryView]) -> Result<Vec<ThresholdEntry>, DomainError> {
    let refuse = |why: String| DomainError::ThresholdInvalid(why);
    let mut entries = Vec::with_capacity(authored.len());
    for entry in authored {
        let currency = CurrencyCode::new(&entry.currency).map_err(|_| {
            refuse(format!(
                "entries[{}]: a threshold key is an ISO 4217 alpha-3 code",
                entry.currency
            ))
        })?;
        let basis = match (entry.absolute_minor, entry.percent_bp) {
            (Some(minor), None) => {
                if minor < 0 {
                    return Err(refuse(format!(
                        "entries[{currency}]: absolute_minor is {minor}; a negative threshold is \
                         below every change there is, which is the two-person rule switched off \
                         by arithmetic"
                    )));
                }
                ThresholdBasis::Absolute { minor }
            }
            (None, Some(bp)) => {
                if bp == 0 || bp > MAX_PERCENT_BP {
                    return Err(refuse(format!(
                        "entries[{currency}]: percent_bp is {bp}; it must be in 1..={MAX_PERCENT_BP} \
                         (basis points, 10000 = 100%)"
                    )));
                }
                ThresholdBasis::Percent { bp }
            }
            (Some(_), Some(_)) => {
                return Err(refuse(format!(
                    "entries[{currency}]: absolute_minor and percent_bp are both set; a threshold \
                     has one basis, or the evaluator picks one with nothing saying which"
                )));
            }
            (None, None) => {
                return Err(refuse(format!(
                    "entries[{currency}]: neither absolute_minor nor percent_bp is set; an entry \
                     that thresholds nothing still counts as this currency having one, which is \
                     the fail-safe switched off by an empty row"
                )));
            }
        };
        entries.push(ThresholdEntry { currency, basis });
    }
    // Sorted here and nowhere else. `ThresholdVersion::new` keeps its caller's
    // order deliberately, so that the two producers of a version - this and the
    // store's `ORDER BY currency` - have one place each to be right rather than one
    // shared re-sort that hides a disagreement.
    entries.sort_by(|left, right| left.currency.as_str().cmp(right.currency.as_str()));
    Ok(entries)
}

/// The `approval_policy × read` gate.
///
/// `resource_id` is `None` because the policy is the tenant's and has no id of its
/// own; the tenant axis is the scope's, as it is on the approval list.
async fn read_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::APPROVAL_POLICY,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// The `approval_policy × write` gate.
///
/// `owner_tenant_id = Some(caller's tenant)` because this is a write, for
/// `approvals::decide_scope`'s reason: the membership assertion is what refuses a
/// target outside the compiled scope, the degraded flat-`In` decision not
/// re-checking the property.
async fn write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::APPROVAL_POLICY,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

#[cfg(test)]
#[path = "threshold_policy_tests.rs"]
mod threshold_policy_tests;
