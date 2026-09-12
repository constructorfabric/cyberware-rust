//! `POST /bss-pricing/v1/plans/{planId}/publish` — the entrance
//! (`02-plan-definition.md` §5, `01-foundation.md` §4.2,
//! `05-governance.md` §2).
//!
//! # One route, two acts, and the design set is what makes them one
//!
//! §5's Purpose cell reads *"Run fail-closed validation **+ submit for
//! approval/publish**"*, and the authz catalog spells the action
//! `publish (submit for publish)`. §2 step 1 then runs the
//! `MaterialityEvaluator` at *"publish submit"*, opens a unit when the change is
//! material, and step 5 says an approve makes *"the Foundation publish
//! continue"*. So the same call does one of two things depending on what the
//! subject already carries:
//!
//! - **no approved unit over this revision *and this content*** — validate,
//!   evaluate materiality, open a pinned unit, answer **202** carrying it. This
//!   is the submit;
//! - **an approved unit over this revision and this content** — mint
//!   [`PublishAuthorization::approved`] from it and call the commit, answer
//!   **200** carrying the receipt. This is §2 step 5's continuation.
//!
//! The alternative shape — the approve route driving the commit — was rejected:
//! it would make `approval × approve` sufficient to publish, and the matrix is
//! explicit that *"no role carries both `plan × publish` and `approval ×
//! approve`"*. Two authorities, two calls.
//!
//! **"And this content" is what keeps the route out of a dead end**, and it was
//! found by driving it. `inst-ap-pin` voids only `submitted` records, so an
//! approval survives a mutation of its own subject; a route that branched on
//! "does this revision carry an approval" would then take the commit arm every
//! time, be refused by the pin, and refuse **forever** — a price row's
//! `row_version` is inside the digest, so even restoring the old values does not
//! restore the old pin, and the plan's only escape would be abandoning the
//! revision. Matching the content instead means a moved subject simply has no
//! approval, so the route opens a fresh unit and a second person looks at what
//! is actually there. The security property is identical; the difference is that
//! an operator can act on the answer. The stale `approved` record is left
//! standing — `inst-as-immutable`, and it is the true record of a decision that
//! was made over content that never published.
//!
//! # Materiality is evaluated against the tenant's real policy, and the arm it
//! # unlocks is reached
//!
//! Materiality is **not** a constant here. `GET/PUT
//! /bss-pricing/v1/config/approval-threshold-policy` is mounted (`module.rs` merges
//! its router), so the policy is read through `infra::threshold::effective_policy`
//! and a tenant that has configured one sees it in the stored verdict. D-10 is the
//! bootstrap rather than a reason for a constant: the first threshold a tenant ever
//! sets goes through this very workflow.
//!
//! **What this unit may author is not what decides it.** With a configured policy,
//! the route's own authoring door bounds the change set to two shapes, because
//! `price_repo::insert_prepared` refuses a draft row on a key `find_key_occupant`
//! finds occupied — *"this is not the way to reprice an occupied key. That is the
//! D-88 supersession unit"*:
//!
//! * it carries a **draft** row, whose key therefore holds no published row, so
//!   `inst-mat-newrow` answers `rowWithoutBaseline` at step 3a; or
//! * it carries **no** draft row, so every row is published and unchanged, and
//!   D-115's pure-shape clause answers `alwaysMaterialTrigger`.
//!
//! So a plan revision can **add** a price row and change the plan's shape, and can
//! never **edit** a price; the threshold comparison has no authorable subject here.
//!
//! # A third door onto an occupied key is what reaches the arm
//!
//! **Something does stage a moved row for this route to judge**, so the two shapes
//! above do not bound the change set on their own.
//! `infra::supersession` and `api::rest::supersessions` are built and merged, a
//! material supersession stages its successor as a `draft` on an occupied key and
//! leaves it there until a reviewer decides it, and `infra::publish::assemble` loads
//! it — *"`shape.rows` keeps carrying it, so every completeness and uniformity rule
//! still sees the key covered"*. What keeps it out of this route's publish is
//! `validated_draft_rows` excluding it, not the absence of a stager.
//!
//! Excluded from the **publish** and handed to the **evaluator**, that row is a
//! moved row on a plan-revision change set, which is the one shape
//! `triggers::triggered_by_content`'s `moves_no_row` cannot see through. Measured
//! through this router: a plan whose revision authored nothing but a D-319 period
//! floor at €500 published `AutoPublishable`, `approval: null`, `published_price_ids:
//! []` — a per-period minimum reaching consumers approver-free on a revision that
//! published no row at all. `materiality_of` now reads
//! `infra::publish::unit_row_set`, the set `validated_draft_rows` commits, so the
//! evaluator and the commit range over one row set and the two bullets above are
//! exhaustive again.
//!
//! The arm below is therefore written rather than `unreachable!()`: what makes it
//! reachable is a **third** door onto an occupied key, and this crate has now grown
//! two. It is reached from `infra::window`, whose change sets are the published rows
//! unchanged, and there always with a delta of zero.
//!
//! # The approve→commit window is closed **inside the commit**, not here
//!
//! `inst-ap-pin` scopes the TOCTOU void to a `submitted` record, so an approved
//! unit is not voided when its subject moves, and there is a real window between
//! the decision and this call. This handler does not check the pin and re-verify
//! it here: it *carries* the pinned digest on the authorization, and
//! [`PublishService::commit`] re-derives it inside the transaction that freezes
//! the content. A check made here would be a check the world can move behind —
//! and the commit's own compare-and-swap does not cover the gap, because it
//! swaps on the **revision's** row version while a price row edited inside the
//! window moves the **row's**.
//!
//! # What this route does check, and could not delegate
//!
//! `PublishAuthorization::approved` deliberately does not assert
//! `submitter != approver` (`domain::publish`'s module doc argues why the rule
//! keeps one owner). The record it is minted from was written under a store
//! CHECK and a domain rule that both refuse a self-approval, so this check has
//! no reachable failure through this gear's own paths — which is precisely why
//! it is written and unit-tested rather than assumed:
//! [`authorization_of`] is a pure function of a record, and
//! `publish_tests::an_approved_record_naming_one_principal_twice_is_refused_here`
//! constructs the record the store cannot hold.

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::HeaderMap, http::StatusCode};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::{ApprovalView, MaterialityView};
use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::plans::{if_match_param, no_open_draft, require_same_revision};
use crate::api::rest::preconditions;
use crate::api::rest::state::GovernanceState;
use crate::domain::error::DomainError;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::{self, ChangeSet, MaterialityVerdict, PublishedPriceBaseline};
use crate::domain::plan_shape::PlanShape;
use crate::domain::publish::{PlanPublishUnit, PublishAuthorization, PublishReceipt};
use crate::domain::scope_key::PlanId;
use crate::infra::storage::repo::approval_repo::ApprovalRecord;
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the publish operation (DE0205).
const TAG: &str = "BSS Pricing Plans";

/// The publish action, as a sub-resource segment (D-140: never a colon method).
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a **literal** argument and silently passes a `const` one; the two
/// spellings are pinned together by `tests/module_test.rs`'s route census.
pub const PLAN_PUBLISH: &str = "/bss-pricing/v1/plans/{planId}/publish";

/// The wire token for the submit arm.
///
/// `pub(crate)` because the window surface answers the same word for the same act:
/// D-62's two controlled mutations open a pinned unit and change nothing, which is
/// this arm with a different subject. Two spellings of one outcome would make a
/// client's `match` depend on which route it called.
pub(crate) const OUTCOME_SUBMITTED: &str = "submitted_for_approval";

/// The wire token for the commit arm.
pub(crate) const OUTCOME_PUBLISHED: &str = "published";

/// What a publish commit produced (§4.2 step 4's five artifacts, named).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PublishReceiptView {
    /// The plan that was published.
    pub plan_id: Uuid,
    /// The revision that became current.
    pub revision: u64,
    /// The registry's **pending** handle. It is not a `CatalogVersion` and must
    /// not be pinned as one: the commit requests an assignment and
    /// `CatalogVersionPublished` resolves it, so a consumer that treated this as
    /// a version would be resolving against an addressability that does not
    /// exist yet.
    pub pending_version_ref: Option<String>,
    /// The price rows this commit moved into `published`.
    pub published_price_ids: Vec<Uuid>,
    /// The `seq` the plan's audit-chain segment reached.
    pub audit_seq: u64,
}

impl From<&PublishReceipt> for PublishReceiptView {
    fn from(receipt: &PublishReceipt) -> Self {
        Self {
            plan_id: receipt.plan_id().get(),
            revision: receipt.revision(),
            pending_version_ref: receipt.version_ref().pending_ref().map(str::to_owned),
            published_price_ids: receipt.published_price_ids().to_vec(),
            audit_seq: receipt.audit_seq(),
        }
    }
}

/// What the call did: opened a unit, or froze the revision.
///
/// One response type across both statuses rather than two schemas on one
/// operation, so a generated client has one thing to deserialize and reads
/// `outcome` to know which arm it got.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PublishOutcomeView {
    /// `submitted_for_approval` (202) | `published` (200).
    pub outcome: String,
    /// The evaluator's verdict. Present on both arms — a publish that ran under
    /// an approval says what made the change material in the first place.
    pub materiality: Option<MaterialityView>,
    /// The unit this call opened, or the one it published under.
    pub approval: Option<ApprovalView>,
    /// The receipt, on the publish arm.
    pub receipt: Option<PublishReceiptView>,
}

/// Build the Axum router for the publish mount and register its operation.
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-pricing/v1/plans/{planId}/publish")
        .operation_id("bss_pricing.publish_plan")
        .summary("Submit a plan's open draft revision for publish, or publish it")
        .description(
            "Runs the fail-closed validation pipeline over the plan's open draft revision and \
             then does one of two things. With **no approved approval unit** over that exact \
             revision AND that exact content, it evaluates materiality, opens a pinned unit and \
             answers `202` - the submit. With one, it mints the publish authorization from that \
             record and commits, answering `200` with the receipt. An approval whose subject \
             moved after the decision covers content that no longer exists, so it does not \
             answer: the route opens a fresh unit and a second person looks at what is actually \
             there. Materiality is evaluated against the tenant's **configured** \
             approval-threshold policy (`GET/PUT /bss-pricing/v1/config/approval-threshold-policy`), \
             and configuring one is itself an always-material act (D-10), so a tenant that has \
             configured nothing has every publish material by rule rather than by omission. A \
             configured policy changes which rule answers and not yet the outcome: repricing an \
             occupied scope key needs the D-88 supersession unit, which this gear does not carry, \
             so a revision either adds a row with no baseline or moves no row at all - and both \
             are material whatever a threshold says. \
             \
             The `If-Match` precondition names the revision **and** its version. A second submit \
             while a unit is still `submitted` is `PENDING_CHANGE_UNIT_EXISTS` (409) - decide it \
             or withdraw it. The content approved is re-derived inside the commit's own \
             transaction and a mismatch is `APPROVAL_CONTENT_MISMATCH` (409): the approve-to-commit \
             window is real, because the TOCTOU void closes only `submitted` records. A retired \
             plan answers `PLAN_RETIRED_NO_SUCCESSOR`, a plan whose every revision is \
             `abandoned` answers `PLAN_ABANDONED_NO_SUCCESSOR`, and a plan holding no open draft \
             answers `LIFECYCLE_FORBIDDEN`. Idempotency is per plan revision and is structural: \
             a revision has one edge out of `draft` and the commit's compare-and-swap refuses \
             the second attempt.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan whose open draft revision is published.")
        .param(if_match_param(
            "On a plan route the tag names **both** the revision it was read from and that \
             revision's version, joined by a hyphen (for example `\"3-7\"`); here it must name \
             the plan's open draft, and its version is what the commit swaps on.",
        ))
        .handler(publish_plan)
        .json_response_with_schema::<PublishOutcomeView>(
            openapi,
            StatusCode::OK,
            "The revision is published; the receipt names the pending version handle.",
        )
        .json_response_with_schema::<PublishOutcomeView>(
            openapi,
            StatusCode::ACCEPTED,
            "The change is material and an approval unit is open; a second principal decides it.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-178's edge, applied here rather than where the routers are merged so it
    // travels with the routes: a surface reachable without it cannot build an
    // `AuditStamp`, and `correlation::require_correlation` answers 500 rather
    // than minting a second value per record.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// `POST /plans/{planId}/publish`.
async fn publish_plan(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(plan_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Once, at the top, and not `Uuid::now_v7()` at each of the three call sites
    // below — which satisfies "never NULL" and not D-178 clause (2), since the
    // commit arm's audit record and its `pricing_outbox` row are two of the things
    // one call produces and a per-site mint cannot promise they agree.
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let plan_id = PlanId::new(plan_id);
    let scope = publish_scope(&enforcer, &ctx, plan_id.get(), tenant).await?;
    let asserted = preconditions::if_match_revision(&headers)?;
    let now = OffsetDateTime::now_utc();

    // The subject, resolved before anything else acts on it. A plan with no open
    // draft is discriminated the way S2 §5 asks — spent, nothing to publish, or
    // absent — rather than answered the assembler's bare 404.
    let draft = state
        .plans
        .find_open_draft(&scope, tenant, plan_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let Some(draft) = draft else {
        return Err(CanonicalError::from(
            no_open_draft(&state.plans, &scope, tenant, plan_id, "publish").await,
        ));
    };
    require_same_revision(plan_id, asserted, draft.revision).map_err(CanonicalError::from)?;
    let unit = PlanPublishUnit::plan_content(plan_id, draft.revision);
    let subject_ref =
        crate::infra::storage::repo::audit_repo::plan_revision_ref(plan_id, draft.revision);

    // The content this call would freeze, assembled once and used for both arms:
    // it is what the lookup below matches an approval against, what the
    // materiality evaluator reads, and — re-derived inside the commit's own
    // transaction — what the pin is finally checked against.
    let shape = assemble_subject(&state, &scope, tenant, plan_id, now).await?;
    let pin = crate::domain::approval::content_hash(&shape);

    // The decision this **content** already carries, if it carries one. Not
    // "does this revision carry an approval": an approval whose subject moved
    // after the decision covers content that no longer exists, and answering
    // with it would refuse the publish at the commit's pin check for the rest of
    // the plan's life. See `approval_repo::find_approved_for_content`.
    let approved = state
        .approvals
        .approved_unit(&scope, tenant, &subject_ref, &pin)
        .await
        .map_err(CanonicalError::from)?;

    // The commit arm: a second person has seen exactly this content.
    if let Some(record) = approved {
        let authorization = authorization_of(&record).map_err(CanonicalError::from)?;
        let receipt = state
            .publish
            .commit(
                &ctx,
                &scope,
                tenant,
                unit,
                asserted.version,
                authorization,
                ctx.subject_id(),
                correlation,
                now,
            )
            .await
            .map_err(CanonicalError::from)?;
        return Ok(published(&record, &receipt));
    }

    // The submit arm. Fail-closed validation first (§5's Purpose cell, §4.2 step
    // 2): a plan that cannot publish must not be put in front of a reviewer, who
    // would then approve a change set the commit refuses.
    let report = state
        .publish
        .precheck(&scope, tenant, plan_id, now)
        .await
        .map_err(CanonicalError::from)?;
    if !report.is_publishable() {
        return Err(CanonicalError::from(DomainError::ValidationFailed(report)));
    }

    let verdict = materiality_of(&state, &scope, tenant, plan_id, &shape, now).await?;
    if verdict.is_material() {
        let opened = state
            .approvals
            .submit(
                &scope,
                tenant,
                plan_id,
                Uuid::now_v7(),
                serde_json::to_value(MaterialityView::from(&verdict)).map_err(|e| {
                    CanonicalError::from(DomainError::Internal(format!(
                        "cannot render the materiality verdict: {e}"
                    )))
                })?,
                audit_stamp(&ctx, now, correlation),
            )
            .await
            .map_err(CanonicalError::from)?;
        return Ok(submitted(&opened, &verdict));
    }

    // **This arm is reachable.** "Repricing an occupied key needs the D-88 unit" is
    // true and not sufficient: that unit (`inst-su-api`, D-195) stages its successor
    // as a *draft on an occupied key* which sits in the plan's candidate set until a
    // reviewer decides it. Handed that row, the evaluator sees a moved row on a
    // plan-revision change set and answers `AutoPublishable`. The change set is
    // `unit_row_set` — this unit's rows — so the row belongs to the act that staged
    // it;
    // `rest_publish::a_period_bound_published_beside_an_orphaned_successor_is_still_judged`
    // is what holds it. Written rather than panicked, because a third door onto an
    // occupied key would reach here rather than crash.
    let receipt = state
        .publish
        .commit(
            &ctx,
            &scope,
            tenant,
            unit,
            asserted.version,
            PublishAuthorization::auto_publishable(),
            ctx.subject_id(),
            correlation,
            now,
        )
        .await
        .map_err(CanonicalError::from)?;
    Ok(auto_published(&receipt, &verdict))
}

/// The authorization a decided record yields, or the refusal it earns.
///
/// A free function over the record so it is testable without a database: the
/// only failure it can report is one the store's own
/// `chk_pricing_approval_distinct_principals` and
/// `domain::approval::authorize_decision` both already refuse, so no path
/// through this gear can stage it and a test has to construct the record
/// directly.
///
/// **It is written anyway, and the reason is ownership rather than defence in
/// depth.** `PublishAuthorization::approved` does not assert
/// `submitter != approver` — `domain::publish`'s module doc argues that the
/// two-person rule keeps one owner, and that owner is the approval workflow.
/// This function is where the workflow's answer is *read* by the publish path,
/// and reading a stored `approver_principal` without comparing it would mean
/// this path trusts a column rather than the rule. A row written around the
/// store — by a migration, a restore, a later slice's writer — is exactly the
/// case where the rule has to hold and the CHECK did not run.
///
/// # Errors
/// [`DomainError::SelfApprovalForbidden`] when the record names one principal
/// twice; [`DomainError::Internal`] when an `approved` record carries no
/// approver or a `content_hash` that is not 32 bytes — both are invariant
/// breaches (`chk_pricing_approval_approver`, and a digest this crate is the
/// only writer of) rather than anything a caller did.
pub(crate) fn authorization_of(
    record: &ApprovalRecord,
) -> Result<PublishAuthorization, DomainError> {
    let approver = crate::infra::approval::independent_approver(record)?;
    let pinned: [u8; 32] = record.content_hash.as_slice().try_into().map_err(|_| {
        DomainError::Internal(format!(
            "approval {} pins {} bytes; the content pin is a 32-byte SHA-256 digest",
            record.approval_id,
            record.content_hash.len()
        ))
    })?;
    Ok(PublishAuthorization::approved(
        record.approval_id,
        record.submitter_principal,
        approver,
        pinned,
    ))
}

/// The `plan × publish` gate.
async fn publish_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    resource_id: Uuid,
    tenant: Uuid,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::PUBLISH,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ Some(crate::authz::ResourceRef(resource_id)),
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// The publish subject, assembled the way the pin and the rules see it.
async fn assemble_subject(
    state: &GovernanceState,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<PlanShape, CanonicalError> {
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!(
            "bss-pricing: publish subject: {e}"
        )))
    })?;
    crate::infra::publish::assemble(&conn, scope, tenant, plan_id, now)
        .await
        .map_err(CanonicalError::from)
}

/// §3's materiality evaluation, over the real change set, the real baseline **and
/// the tenant's real threshold policy**.
///
/// # The policy was `None`, and the justification for it was a false premise
///
/// The tenant's approval-threshold policy is read here. Its `GET/PUT
/// /bss-pricing/v1/config/approval-threshold-policy` surface **is** mounted —
/// `src/module.rs` merges the router and `tests/rest_threshold_policy.rs` drives
/// the whole round trip — and reading it as unmounted is not cosmetic: it leaves
/// `infra::threshold::effective_policy`, the function whose own doc calls it *"the
/// fail-safe, executable"* and which `domain::materiality` names as "its
/// production caller", with **zero callers**, and every publish of every tenant
/// answering `noConfiguredThreshold` however the tenant configured itself. A rule
/// that cannot be switched off is not a fail-safe; it is an absent feature that
/// reads like one.
///
/// It is read through a connection rather than a transaction, which is the same
/// reading `assemble_subject` takes one line above and for the same reason: the
/// materiality verdict is evaluated **once at submit** (§3 step 3) and the unit it
/// opens carries it as a value, so nothing later re-reads the policy and there is no
/// second read to disagree with. `infra::window`'s path is the one that needs the
/// transaction, because there the evaluation and the write are one act.
///
/// The other two arguments were always real: the change set is the rows this publish
/// would produce, and the baseline is the plan's currently published rows — `None`
/// when the plan has never published, which is `inst-mat-first`.
///
/// Both carry **whole rows** rather than scope keys, because D-115's delta domain
/// takes its operand off a row's content. A plan-revision publish is not itself a
/// registered trigger, so the change set is built with
/// [`ChangeSet::of_records`](crate::domain::materiality::ChangeSet::of_records);
/// what is registered about a revision is decided from its content, by
/// `triggers::triggered_by_content`.
///
/// # The change set is **this unit's** rows, never the plan's
///
/// `infra::publish::unit_row_set` — the set
/// [`validated_draft_rows`](crate::infra::publish) commits — rather than
/// `shape.rows`, and the two differ by exactly one thing: a draft row on a key a
/// `published` row already holds, which since D-195 is a **supersession successor
/// another unit staged**. Handing that row to the evaluator was not a rounding
/// error. It is a row whose content *moved* against the baseline, and it is the
/// only way a plan-revision change set can hold one — `create_draft` refuses an
/// occupied key, so this unit can add rows and change shape and can never edit a
/// price. With one in the set, `triggers::triggered_by_content`'s `moves_no_row`
/// answers `false`, D-115's whole-revision trigger never fires, and a revision
/// whose entire content is plan shape — a D-319 period floor at €500 a period —
/// evaluates `AutoPublishable` and publishes with no second principal. Measured
/// through this route, and held by
/// `rest_publish::a_period_bound_published_beside_an_orphaned_successor_is_still_judged`.
///
/// It is also the right set for a reason that has nothing to do with that hole:
/// the verdict is **stored** and a `thresholdReached` unit names the row that
/// tripped, so a plan-revision unit built from the plan's rows could put a
/// `FinanceReviewer` in front of a `price_id` this commit will not touch.
async fn materiality_of(
    state: &GovernanceState,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    shape: &PlanShape,
    now: OffsetDateTime,
) -> Result<MaterialityVerdict, CanonicalError> {
    let change = ChangeSet::of_records(
        crate::infra::publish::unit_row_set(shape)
            .into_iter()
            .cloned(),
    );
    let baseline = if shape.baseline.is_none() {
        None
    } else {
        let published = state
            .prices
            .list_for_plan(scope, tenant, plan_id, &[LifecycleState::Published])
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
        Some(PublishedPriceBaseline::of_records(published))
    };
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!(
            "bss-pricing: threshold policy: {e}"
        )))
    })?;
    // **`now` is the request's, not a second reading of the clock.** D-188 makes the
    // policy a function of the instant — a version whose `effectiveFrom` is ahead is
    // not in force — and this route already holds one instant that the precheck, the
    // audit stamp and the unit it opens all carry. Two readings would let a publish
    // be evaluated against one policy and stamped in a moment where a different one
    // applied, which is the same "one act, one world" the transaction-taking caller
    // in `infra::window` gets from sharing a transaction.
    let policy = crate::infra::threshold::effective_policy_at(&conn, scope, tenant, now).await?;
    Ok(materiality::evaluate(
        &change,
        policy.as_ref(),
        baseline.as_ref(),
    ))
}

/// The 202 a submit answers with.
fn submitted(record: &ApprovalRecord, verdict: &MaterialityVerdict) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(PublishOutcomeView {
            outcome: OUTCOME_SUBMITTED.to_owned(),
            materiality: Some(MaterialityView::from(verdict)),
            approval: Some(ApprovalView::from(record)),
            receipt: None,
        }),
    )
        .into_response()
}

/// The 200 a commit under an approval answers with.
fn published(record: &ApprovalRecord, receipt: &PublishReceipt) -> Response {
    let view = ApprovalView::from(record);
    (
        StatusCode::OK,
        Json(PublishOutcomeView {
            outcome: OUTCOME_PUBLISHED.to_owned(),
            materiality: view.materiality.clone(),
            approval: Some(view),
            receipt: Some(PublishReceiptView::from(receipt)),
        }),
    )
        .into_response()
}

/// The 200 an auto-publishable commit answers with.
///
/// **Reached**, and the module doc carries the argument in full: `infra::window`'s
/// change sets are the published rows unchanged, so they arrive here with a delta
/// of zero. This is the one publish path that commits with `approval: None` — no
/// second principal — which is what makes the arm worth auditing rather than
/// skipping.
fn auto_published(receipt: &PublishReceipt, verdict: &MaterialityVerdict) -> Response {
    (
        StatusCode::OK,
        Json(PublishOutcomeView {
            outcome: OUTCOME_PUBLISHED.to_owned(),
            materiality: Some(MaterialityView::from(verdict)),
            approval: None,
            receipt: Some(PublishReceiptView::from(receipt)),
        }),
    )
        .into_response()
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod publish_tests;
